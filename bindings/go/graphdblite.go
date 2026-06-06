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
	"errors"
	"fmt"
	"runtime"
	"unsafe"
)

// Database represents an open graphdblite database.
type Database struct {
	ptr *C.GraphDB
}

// Result holds the result of a Cypher query.
type Result struct {
	ptr *C.GraphResult
}

// lastError returns the most recent error message from the C library.
func lastError() error {
	msg := C.graphdb_last_error()
	if msg == nil {
		return errors.New("unknown graphdblite error")
	}
	return errors.New(C.GoString(msg))
}

// Open opens a database at the given file path.
func Open(path string) (*Database, error) {
	cpath := C.CString(path)
	defer C.free(unsafe.Pointer(cpath))

	var ptr *C.GraphDB
	rc := C.graphdb_open(cpath, &ptr)
	if rc != 0 {
		return nil, fmt.Errorf("graphdblite open: %w", lastError())
	}
	db := &Database{ptr: ptr}
	runtime.SetFinalizer(db, (*Database).Close)
	return db, nil
}

// OpenWithTimeout opens a database with a custom busy timeout in milliseconds.
func OpenWithTimeout(path string, busyTimeoutMs uint32) (*Database, error) {
	cpath := C.CString(path)
	defer C.free(unsafe.Pointer(cpath))

	var ptr *C.GraphDB
	rc := C.graphdb_open_with_timeout(cpath, C.uint32_t(busyTimeoutMs), &ptr)
	if rc != 0 {
		return nil, fmt.Errorf("graphdblite open: %w", lastError())
	}
	db := &Database{ptr: ptr}
	runtime.SetFinalizer(db, (*Database).Close)
	return db, nil
}

// OpenMemory opens an in-memory database (for testing).
func OpenMemory() (*Database, error) {
	var ptr *C.GraphDB
	rc := C.graphdb_open_memory(&ptr)
	if rc != 0 {
		return nil, fmt.Errorf("graphdblite open memory: %w", lastError())
	}
	db := &Database{ptr: ptr}
	runtime.SetFinalizer(db, (*Database).Close)
	return db, nil
}

// SnapshotTo writes a consistent single-file snapshot of this database to path.
//
// Uses SQLite's VACUUM INTO under the hood: produces a self-contained file
// (no -wal / -shm sidecars), defragmented and compacted. Returns an error
// when a transaction is active on this handle or when path already exists.
func (db *Database) SnapshotTo(path string) error {
	if db.ptr == nil {
		return errors.New("database is closed")
	}
	cpath := C.CString(path)
	defer C.free(unsafe.Pointer(cpath))
	if rc := C.graphdb_snapshot_to(db.ptr, cpath); rc != 0 {
		return fmt.Errorf("graphdblite snapshot_to: %w", lastError())
	}
	return nil
}

// Close closes the database and frees its resources.
// It is safe to call Close multiple times.
func (db *Database) Close() {
	if db.ptr != nil {
		C.graphdb_close(db.ptr)
		db.ptr = nil
		runtime.SetFinalizer(db, nil)
	}
}

// Query executes a read-only Cypher query and returns the result.
// The caller must call result.Free() when done.
func (db *Database) Query(cypher string) (*Result, error) {
	if db.ptr == nil {
		return nil, errors.New("database is closed")
	}
	ccypher := C.CString(cypher)
	defer C.free(unsafe.Pointer(ccypher))

	var ptr *C.GraphResult
	rc := C.graphdb_query(db.ptr, ccypher, &ptr)
	if rc != 0 {
		return nil, fmt.Errorf("graphdblite query: %w", lastError())
	}
	return &Result{ptr: ptr}, nil
}

// Execute runs a write Cypher query (CREATE, DELETE, SET, MERGE) and returns the result.
// The caller must call result.Free() when done.
func (db *Database) Execute(cypher string) (*Result, error) {
	if db.ptr == nil {
		return nil, errors.New("database is closed")
	}
	ccypher := C.CString(cypher)
	defer C.free(unsafe.Pointer(ccypher))

	var ptr *C.GraphResult
	rc := C.graphdb_execute(db.ptr, ccypher, &ptr)
	if rc != 0 {
		return nil, fmt.Errorf("graphdblite execute: %w", lastError())
	}
	return &Result{ptr: ptr}, nil
}

// Transaction types.

// WriteTransaction represents an active write transaction.
type WriteTransaction struct {
	db *Database
}

// ReadTransaction represents an active read transaction.
type ReadTransaction struct {
	db *Database
}

// BeginWrite starts a write transaction.
// The caller must call Commit or Rollback when done.
func (db *Database) BeginWrite() (*WriteTransaction, error) {
	if db.ptr == nil {
		return nil, errors.New("database is closed")
	}
	rc := C.graphdb_tx_begin_write(db.ptr)
	if rc != 0 {
		return nil, fmt.Errorf("graphdblite begin write: %w", lastError())
	}
	return &WriteTransaction{db: db}, nil
}

// BeginRead starts a read transaction.
// The caller must call Commit when done.
func (db *Database) BeginRead() (*ReadTransaction, error) {
	if db.ptr == nil {
		return nil, errors.New("database is closed")
	}
	rc := C.graphdb_tx_begin_read(db.ptr)
	if rc != 0 {
		return nil, fmt.Errorf("graphdblite begin read: %w", lastError())
	}
	return &ReadTransaction{db: db}, nil
}

// Query executes a Cypher query within the write transaction.
func (tx *WriteTransaction) Query(cypher string) (*Result, error) {
	if tx.db == nil || tx.db.ptr == nil {
		return nil, errors.New("transaction is finished")
	}
	ccypher := C.CString(cypher)
	defer C.free(unsafe.Pointer(ccypher))

	var ptr *C.GraphResult
	rc := C.graphdb_tx_execute(tx.db.ptr, ccypher, &ptr)
	if rc != 0 {
		return nil, fmt.Errorf("graphdblite tx query: %w", lastError())
	}
	return &Result{ptr: ptr}, nil
}

// Execute is an alias for Query within a write transaction.
func (tx *WriteTransaction) Execute(cypher string) (*Result, error) {
	return tx.Query(cypher)
}

// CreateIndex creates a secondary index on (label, property) for faster
// equality and STARTS WITH lookups. Must be called inside an active
// write transaction.
func (tx *WriteTransaction) CreateIndex(label, property string) error {
	return tx.ddl("create_index", label, property, func(clabel, cprop *C.char) C.int32_t {
		return C.graphdb_create_index(tx.db.ptr, clabel, cprop)
	})
}

// DropIndex drops a secondary index on (label, property).
func (tx *WriteTransaction) DropIndex(label, property string) error {
	return tx.ddl("drop_index", label, property, func(clabel, cprop *C.char) C.int32_t {
		return C.graphdb_drop_index(tx.db.ptr, clabel, cprop)
	})
}

// CreateFulltextIndex creates a fulltext index on (label, property).
// Accelerates CONTAINS / STARTS WITH / ENDS WITH via SQLite FTS5
// (trigram tokenizer, case-sensitive).
func (tx *WriteTransaction) CreateFulltextIndex(label, property string) error {
	return tx.ddl("create_fulltext_index", label, property, func(clabel, cprop *C.char) C.int32_t {
		return C.graphdb_create_fulltext_index(tx.db.ptr, clabel, cprop)
	})
}

// CreateFulltextIndexCI creates a case-insensitive fulltext index on
// (label, property). The underlying FTS5 trigram tokenizer is built
// with case_sensitive=0, so CONTAINS / STARTS WITH / ENDS WITH against
// this property is case-insensitive.
func (tx *WriteTransaction) CreateFulltextIndexCI(label, property string) error {
	return tx.ddl("create_fulltext_index_ci", label, property, func(clabel, cprop *C.char) C.int32_t {
		return C.graphdb_create_fulltext_index_ci(tx.db.ptr, clabel, cprop)
	})
}

// CreateFulltextIndexWord creates a word-tokenized (unicode61) fulltext
// index on (label, property). Suitable for the fts.search procedure.
// Does not accelerate CONTAINS / STARTS WITH / ENDS WITH — use
// CreateFulltextIndex for those.
func (tx *WriteTransaction) CreateFulltextIndexWord(label, property string) error {
	return tx.ddl("create_fulltext_index_word", label, property, func(clabel, cprop *C.char) C.int32_t {
		return C.graphdb_create_fulltext_index_word(tx.db.ptr, clabel, cprop)
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
	if tx.db == nil || tx.db.ptr == nil {
		return errors.New("transaction is finished")
	}
	if len(properties) == 0 {
		return errors.New("graphdblite create_fulltext_index_word_multi: properties cannot be empty")
	}
	clabel := C.CString(label)
	defer C.free(unsafe.Pointer(clabel))

	// Marshal []string into a C array of *C.char. Each C.CString allocates
	// in the C heap; defer C.free for each entry so they release in LIFO
	// order when the function returns (whether via success or error path).
	cprops := make([]*C.char, len(properties))
	for i, p := range properties {
		cprops[i] = C.CString(p)
		defer C.free(unsafe.Pointer(cprops[i]))
	}
	cpropsPtr := (**C.char)(unsafe.Pointer(&cprops[0]))

	rc := C.graphdb_create_fulltext_index_word_multi(
		tx.db.ptr,
		clabel,
		cpropsPtr,
		C.size_t(len(properties)),
	)
	if rc != 0 {
		return fmt.Errorf("graphdblite create_fulltext_index_word_multi: %w", lastError())
	}
	return nil
}

// DropFulltextIndex drops a fulltext index on (label, property).
func (tx *WriteTransaction) DropFulltextIndex(label, property string) error {
	return tx.ddl("drop_fulltext_index", label, property, func(clabel, cprop *C.char) C.int32_t {
		return C.graphdb_drop_fulltext_index(tx.db.ptr, clabel, cprop)
	})
}

// ddl is the shared scaffold for the four index-DDL methods above.
func (tx *WriteTransaction) ddl(op, label, property string, call func(clabel, cprop *C.char) C.int32_t) error {
	if tx.db == nil || tx.db.ptr == nil {
		return errors.New("transaction is finished")
	}
	clabel := C.CString(label)
	defer C.free(unsafe.Pointer(clabel))
	cprop := C.CString(property)
	defer C.free(unsafe.Pointer(cprop))
	if rc := call(clabel, cprop); rc != 0 {
		return fmt.Errorf("graphdblite %s: %w", op, lastError())
	}
	return nil
}

// Commit commits the write transaction.
func (tx *WriteTransaction) Commit() error {
	if tx.db == nil || tx.db.ptr == nil {
		return errors.New("transaction is finished")
	}
	rc := C.graphdb_tx_commit(tx.db.ptr)
	tx.db = nil
	if rc != 0 {
		return fmt.Errorf("graphdblite commit: %w", lastError())
	}
	return nil
}

// Rollback rolls back the write transaction.
func (tx *WriteTransaction) Rollback() error {
	if tx.db == nil || tx.db.ptr == nil {
		return errors.New("transaction is finished")
	}
	rc := C.graphdb_tx_rollback(tx.db.ptr)
	tx.db = nil
	if rc != 0 {
		return fmt.Errorf("graphdblite rollback: %w", lastError())
	}
	return nil
}

// Query executes a Cypher query within the read transaction.
func (tx *ReadTransaction) Query(cypher string) (*Result, error) {
	if tx.db == nil || tx.db.ptr == nil {
		return nil, errors.New("transaction is finished")
	}
	ccypher := C.CString(cypher)
	defer C.free(unsafe.Pointer(ccypher))

	var ptr *C.GraphResult
	rc := C.graphdb_tx_execute(tx.db.ptr, ccypher, &ptr)
	if rc != 0 {
		return nil, fmt.Errorf("graphdblite tx query: %w", lastError())
	}
	return &Result{ptr: ptr}, nil
}

// Commit releases the read transaction.
func (tx *ReadTransaction) Commit() error {
	if tx.db == nil || tx.db.ptr == nil {
		return errors.New("transaction is finished")
	}
	rc := C.graphdb_tx_commit(tx.db.ptr)
	tx.db = nil
	if rc != 0 {
		return fmt.Errorf("graphdblite commit: %w", lastError())
	}
	return nil
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

// Result methods.

// RowCount returns the number of rows in the result.
func (r *Result) RowCount() int64 {
	if r.ptr == nil {
		return 0
	}
	return int64(C.graphdb_result_row_count(r.ptr))
}

// ColumnCount returns the number of columns in the result.
func (r *Result) ColumnCount() int64 {
	if r.ptr == nil {
		return 0
	}
	return int64(C.graphdb_result_column_count(r.ptr))
}

// ColumnName returns the name of the column at the given index.
func (r *Result) ColumnName(col int64) string {
	if r.ptr == nil {
		return ""
	}
	cname := C.graphdb_result_column_name(r.ptr, C.int64_t(col))
	if cname == nil {
		return ""
	}
	return C.GoString(cname)
}

// ValueType constants.
const (
	TypeNull   = 0
	TypeBool   = 1
	TypeI64    = 2
	TypeF64    = 3
	TypeString = 4
	TypeList   = 5
	TypePath   = 6
)

// ValueType returns the type of the value at (row, col).
func (r *Result) ValueType(row, col int64) int32 {
	if r.ptr == nil {
		return TypeNull
	}
	return int32(C.graphdb_result_value_type(r.ptr, C.int64_t(row), C.int64_t(col)))
}

// ValueStr returns the string representation of the value at (row, col).
func (r *Result) ValueStr(row, col int64) string {
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
	if r.ptr == nil {
		return 0
	}
	return int64(C.graphdb_result_value_i64(r.ptr, C.int64_t(row), C.int64_t(col)))
}

// ValueF64 returns the float value at (row, col). Returns 0.0 if not a float.
func (r *Result) ValueF64(row, col int64) float64 {
	if r.ptr == nil {
		return 0.0
	}
	return float64(C.graphdb_result_value_f64(r.ptr, C.int64_t(row), C.int64_t(col)))
}

// ValueBool returns the boolean value at (row, col). Returns false if not a boolean.
func (r *Result) ValueBool(row, col int64) bool {
	if r.ptr == nil {
		return false
	}
	return C.graphdb_result_value_bool(r.ptr, C.int64_t(row), C.int64_t(col)) != 0
}

// JSON returns the full result as a JSON string.
func (r *Result) JSON() string {
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
