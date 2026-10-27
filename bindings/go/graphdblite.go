// SPDX-License-Identifier: MIT
// Copyright (c) 2026 ds7n

// Package graphdblite provides Go bindings for the graphdblite embedded graph database.
//
// graphdblite is an embedded graph database with Cypher query support,
// backed by SQLite for storage. It supports multi-process concurrent access.
//
// Basic usage:
//
//	db, err := graphdblite.Open("my.db")
//	if err != nil {
//	    log.Fatal(err)
//	}
//	defer db.Close()
//
//	// Write data
//	_, err = db.Execute("CREATE (n:Person {name: 'Alice', age: 30})")
//
//	// Read data
//	result, err := db.Query("MATCH (n:Person) RETURN n.name, n.age")
//	for i := range result.RowCount() {
//	    name := result.ValueStr(i, 0)
//	    age := result.ValueI64(i, 1)
//	    fmt.Printf("%s is %d years old\n", name, age)
//	}
//	result.Free()
package graphdblite

/*
#cgo linux,amd64   LDFLAGS: -L${SRCDIR}/lib/linux_amd64   -lgraphdblite_ffi -lm -ldl -lpthread
#cgo linux,arm64   LDFLAGS: -L${SRCDIR}/lib/linux_arm64   -lgraphdblite_ffi -lm -ldl -lpthread
#cgo darwin,arm64  LDFLAGS: -L${SRCDIR}/lib/darwin_arm64  -lgraphdblite_ffi -lm
#cgo windows,amd64 LDFLAGS: -L${SRCDIR}/lib/windows_amd64 -lgraphdblite_ffi -lws2_32 -luserenv -lntdll -ladvapi32 -lbcrypt
#include "../ffi/graphdblite.h"
#include <stdlib.h>
*/
import "C"

import (
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"reflect"
	"runtime"
	"sort"
	"strconv"
	"strings"
	"unsafe"
)

// ErrClosed is returned by calls on a Database after Close.
var ErrClosed = errors.New("database is closed")

// ErrTxFinished is returned by calls on a transaction after its Commit or
// Rollback succeeded (or after its Database was closed).
var ErrTxFinished = errors.New("transaction is finished")

// Database represents an open graphdblite database.
type Database struct {
	ptr *C.GraphDB
}

// Result holds the result of a Cypher query.
type Result struct {
	ptr *C.GraphResult
}

// lastError returns the most recent error message from the C library.
//
// The message lives in thread-local storage on the C side, so this must run
// on the same OS thread as the failing call: only call it through cCall.
func lastError() error {
	msg := C.graphdb_last_error()
	if msg == nil {
		return errors.New("unknown graphdblite error")
	}
	return errors.New(C.GoString(msg))
}

// cCall runs one C call that reports failure through graphdb_last_error.
//
// The goroutine is locked to its OS thread for the call and the error
// read, so it cannot migrate between them and read another thread's (or
// no) error message.
func cCall(op string, call func() C.int32_t) error {
	runtime.LockOSThread()
	defer runtime.UnlockOSThread()
	if rc := call(); rc != 0 {
		return fmt.Errorf("graphdblite %s: %w", op, lastError())
	}
	return nil
}

func newDatabase(ptr *C.GraphDB) *Database {
	db := &Database{ptr: ptr}
	runtime.SetFinalizer(db, (*Database).Close)
	return db
}

// Open opens a database at the given file path. The path ":memory:" opens
// a private in-memory database (no file is created).
func Open(path string) (*Database, error) {
	cpath := C.CString(path)
	defer C.free(unsafe.Pointer(cpath))

	var ptr *C.GraphDB
	if err := cCall("open", func() C.int32_t { return C.graphdb_open(cpath, &ptr) }); err != nil {
		return nil, err
	}
	return newDatabase(ptr), nil
}

// OpenWithTimeout opens a database with a custom busy timeout in milliseconds.
func OpenWithTimeout(path string, busyTimeoutMs uint32) (*Database, error) {
	cpath := C.CString(path)
	defer C.free(unsafe.Pointer(cpath))

	var ptr *C.GraphDB
	err := cCall("open", func() C.int32_t {
		return C.graphdb_open_with_timeout(cpath, C.uint32_t(busyTimeoutMs), &ptr)
	})
	if err != nil {
		return nil, err
	}
	return newDatabase(ptr), nil
}

// OpenMemory opens an in-memory database (for testing).
func OpenMemory() (*Database, error) {
	var ptr *C.GraphDB
	if err := cCall("open memory", func() C.int32_t { return C.graphdb_open_memory(&ptr) }); err != nil {
		return nil, err
	}
	return newDatabase(ptr), nil
}

// SnapshotTo writes a consistent single-file snapshot of this database to path.
//
// Uses SQLite's VACUUM INTO under the hood: produces a self-contained file
// (no -wal / -shm sidecars), defragmented and compacted. Returns an error
// when a transaction is active on this handle or when path already exists.
func (db *Database) SnapshotTo(path string) error {
	defer runtime.KeepAlive(db)
	if db.ptr == nil {
		return ErrClosed
	}
	cpath := C.CString(path)
	defer C.free(unsafe.Pointer(cpath))
	return cCall("snapshot_to", func() C.int32_t { return C.graphdb_snapshot_to(db.ptr, cpath) })
}

// Close closes the database and frees its resources. An open transaction
// is rolled back. It is safe to call Close multiple times.
func (db *Database) Close() {
	if db.ptr != nil {
		C.graphdb_close(db.ptr)
		db.ptr = nil
		runtime.SetFinalizer(db, nil)
	}
}

// runMode selects which C entry point runs a statement.
type runMode int

const (
	modeRead  runMode = iota // own read transaction
	modeWrite                // own write transaction
	modeTx                   // inside the handle's open transaction
)

// run is the shared body of every query method. params may be nil.
func (db *Database) run(op, cypher string, params map[string]any, mode runMode) (*Result, error) {
	// Keep db (and so its finalizer) alive until the C call returns.
	defer runtime.KeepAlive(db)
	if db.ptr == nil {
		return nil, ErrClosed
	}
	var cparams *C.char
	if params != nil {
		js, err := encodeParams(params)
		if err != nil {
			return nil, err
		}
		cparams = C.CString(js)
		defer C.free(unsafe.Pointer(cparams))
	}
	ccypher := C.CString(cypher)
	defer C.free(unsafe.Pointer(ccypher))

	var ptr *C.GraphResult
	err := cCall(op, func() C.int32_t {
		switch mode {
		case modeRead:
			return C.graphdb_query_params(db.ptr, ccypher, cparams, &ptr)
		case modeWrite:
			return C.graphdb_execute_params(db.ptr, ccypher, cparams, &ptr)
		default:
			return C.graphdb_tx_execute_params(db.ptr, ccypher, cparams, &ptr)
		}
	})
	if err != nil {
		return nil, err
	}
	return &Result{ptr: ptr}, nil
}

// Query executes a read-only Cypher query and returns the result.
// The caller must call result.Free() when done.
func (db *Database) Query(cypher string) (*Result, error) {
	return db.run("query", cypher, nil, modeRead)
}

// QueryParams is Query with $name parameters. See encodeParams for the
// supported Go types.
func (db *Database) QueryParams(cypher string, params map[string]any) (*Result, error) {
	return db.run("query", cypher, params, modeRead)
}

// Execute runs a write Cypher query (CREATE, DELETE, SET, MERGE) and returns the result.
// The caller must call result.Free() when done.
func (db *Database) Execute(cypher string) (*Result, error) {
	return db.run("execute", cypher, nil, modeWrite)
}

// ExecuteParams is Execute with $name parameters.
func (db *Database) ExecuteParams(cypher string, params map[string]any) (*Result, error) {
	return db.run("execute", cypher, params, modeWrite)
}

// Transaction types.

// WriteTransaction represents an active write transaction.
//
// A statement that fails inside the transaction is rolled back on its own;
// the transaction stays open and its other statements are unaffected.
type WriteTransaction struct {
	db *Database
}

// ReadTransaction represents an active read transaction.
type ReadTransaction struct {
	db *Database
}

// begin starts a transaction. It uses the C "_shared" variants: a Go
// transaction object may be used from any goroutine, and goroutines move
// between OS threads, so the C layer's owning-thread check does not apply.
func (db *Database) begin(op string, write bool) error {
	defer runtime.KeepAlive(db)
	if db.ptr == nil {
		return ErrClosed
	}
	return cCall(op, func() C.int32_t {
		if write {
			return C.graphdb_tx_begin_write_shared(db.ptr)
		}
		return C.graphdb_tx_begin_read_shared(db.ptr)
	})
}

// BeginWrite starts a write transaction.
// The caller must call Commit or Rollback when done.
func (db *Database) BeginWrite() (*WriteTransaction, error) {
	if err := db.begin("begin write", true); err != nil {
		return nil, err
	}
	return &WriteTransaction{db: db}, nil
}

// BeginRead starts a read transaction.
// The caller must call Commit (or Rollback) when done.
func (db *Database) BeginRead() (*ReadTransaction, error) {
	if err := db.begin("begin read", false); err != nil {
		return nil, err
	}
	return &ReadTransaction{db: db}, nil
}

// live returns the transaction's database, or ErrTxFinished.
func live(db *Database) (*Database, error) {
	if db == nil || db.ptr == nil {
		return nil, ErrTxFinished
	}
	return db, nil
}

// Query executes a Cypher query within the write transaction.
func (tx *WriteTransaction) Query(cypher string) (*Result, error) {
	return tx.QueryParams(cypher, nil)
}

// QueryParams executes a Cypher query with $name parameters within the
// write transaction.
func (tx *WriteTransaction) QueryParams(cypher string, params map[string]any) (*Result, error) {
	db, err := live(tx.db)
	if err != nil {
		return nil, err
	}
	return db.run("tx query", cypher, params, modeTx)
}

// Execute is an alias for Query within a write transaction.
func (tx *WriteTransaction) Execute(cypher string) (*Result, error) {
	return tx.Query(cypher)
}

// ExecuteParams is an alias for QueryParams within a write transaction.
func (tx *WriteTransaction) ExecuteParams(cypher string, params map[string]any) (*Result, error) {
	return tx.QueryParams(cypher, params)
}

// CreateIndex creates a secondary index on (label, property) for faster
// equality and STARTS WITH lookups. Must be called inside an active
// write transaction.
func (tx *WriteTransaction) CreateIndex(label, property string) error {
	return tx.ddl("create_index", label, property, func(ptr *C.GraphDB, clabel, cprop *C.char) C.int32_t {
		return C.graphdb_create_index(ptr, clabel, cprop)
	})
}

// DropIndex drops a secondary index on (label, property).
func (tx *WriteTransaction) DropIndex(label, property string) error {
	return tx.ddl("drop_index", label, property, func(ptr *C.GraphDB, clabel, cprop *C.char) C.int32_t {
		return C.graphdb_drop_index(ptr, clabel, cprop)
	})
}

// CreateFulltextIndex creates a fulltext index on (label, property).
// Accelerates CONTAINS / STARTS WITH / ENDS WITH via SQLite FTS5
// (trigram tokenizer, case-sensitive).
func (tx *WriteTransaction) CreateFulltextIndex(label, property string) error {
	return tx.ddl("create_fulltext_index", label, property, func(ptr *C.GraphDB, clabel, cprop *C.char) C.int32_t {
		return C.graphdb_create_fulltext_index(ptr, clabel, cprop)
	})
}

// CreateFulltextIndexCI creates a case-insensitive fulltext index on
// (label, property). The underlying FTS5 trigram tokenizer is built
// with case_sensitive=0. It accelerates the case-insensitive idiom
// toLower(n.prop) CONTAINS toLower($q) (and STARTS WITH / ENDS WITH);
// plain predicates keep Cypher's case-sensitive semantics.
func (tx *WriteTransaction) CreateFulltextIndexCI(label, property string) error {
	return tx.ddl("create_fulltext_index_ci", label, property, func(ptr *C.GraphDB, clabel, cprop *C.char) C.int32_t {
		return C.graphdb_create_fulltext_index_ci(ptr, clabel, cprop)
	})
}

// CreateFulltextIndexWord creates a word-tokenized (unicode61) fulltext
// index on (label, property). Suitable for the fts.search procedure.
// Does not accelerate CONTAINS / STARTS WITH / ENDS WITH — use
// CreateFulltextIndex for those.
func (tx *WriteTransaction) CreateFulltextIndexWord(label, property string) error {
	return tx.ddl("create_fulltext_index_word", label, property, func(ptr *C.GraphDB, clabel, cprop *C.char) C.int32_t {
		return C.graphdb_create_fulltext_index_word(ptr, clabel, cprop)
	})
}

// CreateFulltextIndexWordMulti creates a word-tokenized (unicode61)
// fulltext index covering multiple properties on a label. Backed by a
// single SQLite FTS5 multi-column virtual table.
//
// Use CALL fts.search(label, '*', query) to search across every covered
// property, or CALL fts.search(label, property, query) to scope to one
// covered property.
func (tx *WriteTransaction) CreateFulltextIndexWordMulti(label string, properties []string) error {
	return tx.multiDDL("create_fulltext_index_word_multi", label, properties,
		func(ptr *C.GraphDB, clabel *C.char, cprops **C.char, n int) C.int32_t {
			return C.graphdb_create_fulltext_index_word_multi(ptr, clabel, cprops, C.size_t(n))
		})
}

// DropFulltextIndex drops a fulltext index on (label, property).
func (tx *WriteTransaction) DropFulltextIndex(label, property string) error {
	return tx.ddl("drop_fulltext_index", label, property, func(ptr *C.GraphDB, clabel, cprop *C.char) C.int32_t {
		return C.graphdb_drop_fulltext_index(ptr, clabel, cprop)
	})
}

// CreateCompositeIndex creates a multi-column secondary index on
// (label, properties...). Properties must have at least two entries.
// Must be called inside an active write transaction.
func (tx *WriteTransaction) CreateCompositeIndex(label string, properties []string) error {
	return tx.multiDDL("create_composite_index", label, properties,
		func(ptr *C.GraphDB, clabel *C.char, cprops **C.char, n int) C.int32_t {
			return C.graphdb_create_composite_index(ptr, clabel, cprops, C.uintptr_t(n))
		})
}

// DropCompositeIndex drops a multi-column secondary index on
// (label, properties...). Property ordering must match the original
// CreateCompositeIndex call exactly.
func (tx *WriteTransaction) DropCompositeIndex(label string, properties []string) error {
	return tx.multiDDL("drop_composite_index", label, properties,
		func(ptr *C.GraphDB, clabel *C.char, cprops **C.char, n int) C.int32_t {
			return C.graphdb_drop_composite_index(ptr, clabel, cprops, C.uintptr_t(n))
		})
}

// ddl is the shared scaffold for the single-property index DDL methods.
func (tx *WriteTransaction) ddl(op, label, property string, call func(ptr *C.GraphDB, clabel, cprop *C.char) C.int32_t) error {
	db, err := live(tx.db)
	if err != nil {
		return err
	}
	defer runtime.KeepAlive(db)
	clabel := C.CString(label)
	defer C.free(unsafe.Pointer(clabel))
	cprop := C.CString(property)
	defer C.free(unsafe.Pointer(cprop))
	return cCall(op, func() C.int32_t { return call(db.ptr, clabel, cprop) })
}

// multiDDL is the shared scaffold for the multi-property index DDL methods.
func (tx *WriteTransaction) multiDDL(op, label string, properties []string, call func(ptr *C.GraphDB, clabel *C.char, cprops **C.char, n int) C.int32_t) error {
	db, err := live(tx.db)
	if err != nil {
		return err
	}
	defer runtime.KeepAlive(db)
	if len(properties) == 0 {
		return fmt.Errorf("graphdblite %s: properties cannot be empty", op)
	}
	clabel := C.CString(label)
	defer C.free(unsafe.Pointer(clabel))

	// Marshal []string into an array of C strings. The slice is Go memory
	// holding only C pointers, which cgo allows passing to C.
	cprops := make([]*C.char, len(properties))
	for i, p := range properties {
		cprops[i] = C.CString(p)
		defer C.free(unsafe.Pointer(cprops[i]))
	}
	return cCall(op, func() C.int32_t { return call(db.ptr, clabel, &cprops[0], len(properties)) })
}

// finish runs the commit or rollback C call for a transaction.
func finish(op string, db *Database, commit bool) error {
	defer runtime.KeepAlive(db)
	return cCall(op, func() C.int32_t {
		if commit {
			return C.graphdb_tx_commit(db.ptr)
		}
		return C.graphdb_tx_rollback(db.ptr)
	})
}

// Commit commits the write transaction. On failure the transaction stays
// open, so the caller can still Rollback.
func (tx *WriteTransaction) Commit() error {
	db, err := live(tx.db)
	if err != nil {
		return err
	}
	if err := finish("commit", db, true); err != nil {
		return err
	}
	tx.db = nil
	return nil
}

// Rollback rolls back the write transaction. The transaction object is
// finished afterwards even if the rollback reports an error.
func (tx *WriteTransaction) Rollback() error {
	db, err := live(tx.db)
	if err != nil {
		return err
	}
	tx.db = nil
	return finish("rollback", db, false)
}

// Query executes a Cypher query within the read transaction.
func (tx *ReadTransaction) Query(cypher string) (*Result, error) {
	return tx.QueryParams(cypher, nil)
}

// QueryParams executes a Cypher query with $name parameters within the
// read transaction.
func (tx *ReadTransaction) QueryParams(cypher string, params map[string]any) (*Result, error) {
	db, err := live(tx.db)
	if err != nil {
		return nil, err
	}
	return db.run("tx query", cypher, params, modeTx)
}

// Commit releases the read transaction. On failure the transaction stays
// open, so the caller can still Rollback.
func (tx *ReadTransaction) Commit() error {
	db, err := live(tx.db)
	if err != nil {
		return err
	}
	if err := finish("commit", db, true); err != nil {
		return err
	}
	tx.db = nil
	return nil
}

// Rollback releases the read transaction (equivalent to Commit, since a
// read transaction has nothing to undo).
func (tx *ReadTransaction) Rollback() error {
	db, err := live(tx.db)
	if err != nil {
		return err
	}
	tx.db = nil
	return finish("rollback", db, false)
}

// WithWriteTx runs fn inside a write transaction, committing on success and
// rolling back on a returned error or a panic. Mirrors the Node binding's
// db.withWriteTx(...) and Python's `with db.begin_write() as tx:` semantics.
//
// If fn returns nil, Commit is called and its error (if any) is returned.
// If fn returns an error, Rollback is attempted and fn's error is returned
// (the rollback error is intentionally discarded — fn's error is the cause).
// If fn panics, Rollback is attempted and the panic is re-raised.
func (db *Database) WithWriteTx(fn func(*WriteTransaction) error) (err error) {
	tx, err := db.BeginWrite()
	if err != nil {
		return err
	}
	committed := false
	defer func() {
		// Re-panic after rollback so callers see the original stack.
		if r := recover(); r != nil {
			if !committed {
				_ = tx.Rollback()
			}
			panic(r)
		}
		if !committed {
			_ = tx.Rollback()
		}
	}()
	if err = fn(tx); err != nil {
		return err
	}
	if err = tx.Commit(); err != nil {
		return err
	}
	committed = true
	return nil
}

// WithReadTx runs fn inside a read transaction, releasing it on return.
// Read transactions have nothing to roll back, so the tx is always finalized
// via Commit on a clean exit; on a returned error or a panic, Commit is
// still called (best-effort) so the database handle is not left holding the
// read lock.
func (db *Database) WithReadTx(fn func(*ReadTransaction) error) (err error) {
	tx, err := db.BeginRead()
	if err != nil {
		return err
	}
	finalized := false
	defer func() {
		if r := recover(); r != nil {
			if !finalized {
				_ = tx.Commit()
			}
			panic(r)
		}
		if !finalized {
			_ = tx.Commit()
		}
	}()
	if err = fn(tx); err != nil {
		return err
	}
	if err = tx.Commit(); err != nil {
		return err
	}
	finalized = true
	return nil
}

// Parameters.

// encodeParams serializes query parameters to the JSON object the C API
// takes. Supported Go values: nil, bool, every int / uint kind (uints above
// math.MaxInt64 are rejected), float32 / float64 (always sent as floats, so
// 1.0 stays a Cypher float; NaN and infinities are rejected), string,
// slices and arrays (Cypher lists), maps with string keys (Cypher maps),
// and pointers / interfaces to those.
func encodeParams(params map[string]any) (string, error) {
	var b strings.Builder
	b.WriteByte('{')
	keys := make([]string, 0, len(params))
	for k := range params {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	for i, k := range keys {
		if i > 0 {
			b.WriteString(", ")
		}
		writeJSONString(&b, k)
		b.WriteString(": ")
		if err := encodeValue(&b, reflect.ValueOf(params[k])); err != nil {
			return "", fmt.Errorf("graphdblite params: parameter %q: %w", k, err)
		}
	}
	b.WriteByte('}')
	return b.String(), nil
}

func writeJSONString(b *strings.Builder, s string) {
	// Marshalling a string cannot fail.
	js, _ := json.Marshal(s)
	b.Write(js)
}

func encodeValue(b *strings.Builder, v reflect.Value) error {
	if !v.IsValid() {
		b.WriteString("null")
		return nil
	}
	switch v.Kind() {
	case reflect.Interface, reflect.Pointer:
		if v.IsNil() {
			b.WriteString("null")
			return nil
		}
		return encodeValue(b, v.Elem())
	case reflect.Bool:
		b.WriteString(strconv.FormatBool(v.Bool()))
	case reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64:
		b.WriteString(strconv.FormatInt(v.Int(), 10))
	case reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64, reflect.Uintptr:
		u := v.Uint()
		if u > math.MaxInt64 {
			return fmt.Errorf("integer %d overflows int64", u)
		}
		b.WriteString(strconv.FormatUint(u, 10))
	case reflect.Float32, reflect.Float64:
		f := v.Float()
		if math.IsNaN(f) || math.IsInf(f, 0) {
			return fmt.Errorf("non-finite float %v", f)
		}
		bits := 64
		if v.Kind() == reflect.Float32 {
			bits = 32
		}
		s := strconv.FormatFloat(f, 'g', -1, bits)
		if !strings.ContainsAny(s, ".eE") {
			s += ".0" // keep integral floats floats
		}
		b.WriteString(s)
	case reflect.String:
		writeJSONString(b, v.String())
	case reflect.Slice, reflect.Array:
		if v.Kind() == reflect.Slice && v.IsNil() {
			b.WriteString("null")
			return nil
		}
		b.WriteByte('[')
		for i := 0; i < v.Len(); i++ {
			if i > 0 {
				b.WriteString(", ")
			}
			if err := encodeValue(b, v.Index(i)); err != nil {
				return err
			}
		}
		b.WriteByte(']')
	case reflect.Map:
		if v.Type().Key().Kind() != reflect.String {
			return fmt.Errorf("unsupported type %s", v.Type())
		}
		if v.IsNil() {
			b.WriteString("null")
			return nil
		}
		keys := v.MapKeys()
		sort.Slice(keys, func(i, j int) bool { return keys[i].String() < keys[j].String() })
		b.WriteByte('{')
		for i, k := range keys {
			if i > 0 {
				b.WriteString(", ")
			}
			writeJSONString(b, k.String())
			b.WriteString(": ")
			if err := encodeValue(b, v.MapIndex(k)); err != nil {
				return err
			}
		}
		b.WriteByte('}')
	default:
		return fmt.Errorf("unsupported type %s", v.Type())
	}
	return nil
}

// Result methods.

// RowCount returns the number of rows in the result.
func (r *Result) RowCount() int64 {
	defer runtime.KeepAlive(r)
	if r.ptr == nil {
		return 0
	}
	return int64(C.graphdb_result_row_count(r.ptr))
}

// ColumnCount returns the number of columns in the result.
func (r *Result) ColumnCount() int64 {
	defer runtime.KeepAlive(r)
	if r.ptr == nil {
		return 0
	}
	return int64(C.graphdb_result_column_count(r.ptr))
}

// ColumnName returns the name of the column at the given index.
func (r *Result) ColumnName(col int64) string {
	defer runtime.KeepAlive(r)
	if r.ptr == nil {
		return ""
	}
	cname := C.graphdb_result_column_name(r.ptr, C.int64_t(col))
	if cname == nil {
		return ""
	}
	return C.GoString(cname)
}

// ValueType constants, matching graphdb_result_value_type.
const (
	TypeNull          = 0
	TypeBool          = 1
	TypeI64           = 2
	TypeF64           = 3
	TypeString        = 4
	TypeList          = 5
	TypePath          = 6
	TypeMap           = 7
	TypeNode          = 8
	TypeRelationship  = 9
	TypeDate          = 10
	TypeLocalTime     = 11
	TypeTime          = 12
	TypeLocalDateTime = 13
	TypeDateTime      = 14
	TypeDuration      = 15
)

// ValueType returns the type of the value at (row, col), one of the Type*
// constants.
func (r *Result) ValueType(row, col int64) int32 {
	defer runtime.KeepAlive(r)
	if r.ptr == nil {
		return TypeNull
	}
	return int32(C.graphdb_result_value_type(r.ptr, C.int64_t(row), C.int64_t(col)))
}

// ValueStr returns the string representation of the value at (row, col).
// The C string is owned by the result, so it is valid for the copy here
// regardless of other calls.
func (r *Result) ValueStr(row, col int64) string {
	defer runtime.KeepAlive(r)
	if r.ptr == nil {
		return ""
	}
	cval := C.graphdb_result_value_str(r.ptr, C.int64_t(row), C.int64_t(col))
	if cval == nil {
		return ""
	}
	return C.GoString(cval)
}

// ValueI64 returns the integer value at (row, col). Returns 0 if not an integer.
func (r *Result) ValueI64(row, col int64) int64 {
	defer runtime.KeepAlive(r)
	if r.ptr == nil {
		return 0
	}
	return int64(C.graphdb_result_value_i64(r.ptr, C.int64_t(row), C.int64_t(col)))
}

// ValueF64 returns the float value at (row, col). Returns 0.0 if not a float.
func (r *Result) ValueF64(row, col int64) float64 {
	defer runtime.KeepAlive(r)
	if r.ptr == nil {
		return 0.0
	}
	return float64(C.graphdb_result_value_f64(r.ptr, C.int64_t(row), C.int64_t(col)))
}

// ValueBool returns the boolean value at (row, col). Returns false if not a boolean.
func (r *Result) ValueBool(row, col int64) bool {
	defer runtime.KeepAlive(r)
	if r.ptr == nil {
		return false
	}
	return C.graphdb_result_value_bool(r.ptr, C.int64_t(row), C.int64_t(col)) != 0
}

// JSON returns the full result as a JSON string. Paths are
// {"nodes": [...], "relationships": [...]}; non-finite floats are null.
func (r *Result) JSON() string {
	defer runtime.KeepAlive(r)
	if r.ptr == nil {
		return "[]"
	}
	cjson := C.graphdb_result_json(r.ptr)
	if cjson == nil {
		return "[]"
	}
	return C.GoString(cjson)
}

// Free releases the result resources.
// It is safe to call Free multiple times.
func (r *Result) Free() {
	if r.ptr != nil {
		C.graphdb_result_free(r.ptr)
		r.ptr = nil
	}
}
