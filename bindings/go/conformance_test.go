// Binding conformance tests — see ../../docs/BINDING_CONFORMANCE.md.
//
// Each test is named TestBC_NN_* so a failure points at a specific
// checklist row. Every first-party binding must pass an equivalent set.

package graphdblite

import (
	"path/filepath"
	"testing"
)

func openTempDB(t *testing.T) (*Database, string) {
	t.Helper()
	path := filepath.Join(t.TempDir(), "db.sqlite")
	db, err := Open(path)
	if err != nil {
		t.Fatalf("Open(%s): %v", path, err)
	}
	return db, path
}

func personCount(t *testing.T, runner interface {
	Query(string) (*Result, error)
}) int64 {
	t.Helper()
	res, err := runner.Query("MATCH (n:Person) RETURN count(n) AS c")
	if err != nil {
		t.Fatalf("count query: %v", err)
	}
	defer res.Free()
	return res.ValueI64(0, 0)
}

// --------------------------------------------------------------------------
// BC-01 — explicit rollback discards writes
// --------------------------------------------------------------------------
func TestBC_01_ExplicitRollback(t *testing.T) {
	db, _ := openTempDB(t)
	defer db.Close()

	tx, err := db.BeginWrite()
	if err != nil {
		t.Fatalf("BeginWrite: %v", err)
	}
	res, err := tx.Execute("CREATE (n:Person {name: 'Alice'})")
	if err != nil {
		t.Fatalf("Execute: %v", err)
	}
	res.Free()
	if err := tx.Rollback(); err != nil {
		t.Fatalf("Rollback: %v", err)
	}

	if got := personCount(t, db); got != 0 {
		t.Fatalf("after rollback: count = %d, want 0", got)
	}
}

// --------------------------------------------------------------------------
// BC-02 — commit persists writes
// --------------------------------------------------------------------------
func TestBC_02_CommitPersists(t *testing.T) {
	db, _ := openTempDB(t)
	defer db.Close()

	tx, err := db.BeginWrite()
	if err != nil {
		t.Fatalf("BeginWrite: %v", err)
	}
	res, err := tx.Execute("CREATE (n:Person {name: 'Alice'})")
	if err != nil {
		t.Fatalf("Execute: %v", err)
	}
	res.Free()
	if err := tx.Commit(); err != nil {
		t.Fatalf("Commit: %v", err)
	}

	if got := personCount(t, db); got != 1 {
		t.Fatalf("after commit: count = %d, want 1", got)
	}
}

// --------------------------------------------------------------------------
// BC-04 — nested begin returns an error; first tx remains usable
// --------------------------------------------------------------------------
func TestBC_04_NestedBeginRejected(t *testing.T) {
	db, _ := openTempDB(t)
	defer db.Close()

	tx, err := db.BeginWrite()
	if err != nil {
		t.Fatalf("BeginWrite: %v", err)
	}

	if _, err := db.BeginWrite(); err == nil {
		t.Fatalf("nested BeginWrite: want error, got nil")
	}
	if _, err := db.BeginRead(); err == nil {
		t.Fatalf("nested BeginRead while write tx open: want error, got nil")
	}

	// First tx must still work.
	res, err := tx.Execute("CREATE (n:Person {name: 'Alice'})")
	if err != nil {
		t.Fatalf("first tx still works: %v", err)
	}
	res.Free()
	if err := tx.Commit(); err != nil {
		t.Fatalf("Commit: %v", err)
	}
	if got := personCount(t, db); got != 1 {
		t.Fatalf("count after commit = %d, want 1", got)
	}
}

// --------------------------------------------------------------------------
// BC-05 — write inside a read transaction returns an error
// --------------------------------------------------------------------------
func TestBC_05_WriteInReadTx(t *testing.T) {
	db, _ := openTempDB(t)
	defer db.Close()

	tx, err := db.BeginRead()
	if err != nil {
		t.Fatalf("BeginRead: %v", err)
	}
	defer tx.Commit()

	if _, err := tx.Query("CREATE (n:Person {name: 'Alice'})"); err == nil {
		t.Fatalf("write inside read tx: want error, got nil")
	}
}

// --------------------------------------------------------------------------
// BC-06 — commit/rollback without an active tx returns an error
// --------------------------------------------------------------------------
func TestBC_06_CommitWithoutTx(t *testing.T) {
	db, _ := openTempDB(t)
	defer db.Close()

	tx, err := db.BeginWrite()
	if err != nil {
		t.Fatalf("BeginWrite: %v", err)
	}
	if err := tx.Commit(); err != nil {
		t.Fatalf("first Commit: %v", err)
	}
	if err := tx.Commit(); err == nil {
		t.Fatalf("double Commit: want error, got nil")
	}
}

// --------------------------------------------------------------------------
// BC-07 — operations on a finished transaction return an error
// --------------------------------------------------------------------------
func TestBC_07_UseAfterCommit(t *testing.T) {
	db, _ := openTempDB(t)
	defer db.Close()

	tx, err := db.BeginWrite()
	if err != nil {
		t.Fatalf("BeginWrite: %v", err)
	}
	res, err := tx.Execute("CREATE (n:Person {name: 'Alice'})")
	if err != nil {
		t.Fatalf("Execute: %v", err)
	}
	res.Free()
	if err := tx.Commit(); err != nil {
		t.Fatalf("Commit: %v", err)
	}

	if _, err := tx.Execute("CREATE (n:Person {name: 'Bob'})"); err == nil {
		t.Fatalf("Execute after commit: want error, got nil")
	}
	if err := tx.Rollback(); err == nil {
		t.Fatalf("Rollback after commit: want error, got nil")
	}
}

func TestBC_07_UseAfterRollback(t *testing.T) {
	db, _ := openTempDB(t)
	defer db.Close()

	tx, err := db.BeginWrite()
	if err != nil {
		t.Fatalf("BeginWrite: %v", err)
	}
	res, err := tx.Execute("CREATE (n:Person {name: 'Alice'})")
	if err != nil {
		t.Fatalf("Execute: %v", err)
	}
	res.Free()
	if err := tx.Rollback(); err != nil {
		t.Fatalf("Rollback: %v", err)
	}
	if _, err := tx.Execute("CREATE (n:Person {name: 'Bob'})"); err == nil {
		t.Fatalf("Execute after rollback: want error, got nil")
	}
}

// --------------------------------------------------------------------------
// BC-09 — closing the database with an open tx does not corrupt the file
// --------------------------------------------------------------------------
func TestBC_09_CloseWithOpenTx(t *testing.T) {
	path := filepath.Join(t.TempDir(), "drop.sqlite")
	db, err := Open(path)
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	tx, err := db.BeginWrite()
	if err != nil {
		t.Fatalf("BeginWrite: %v", err)
	}
	res, err := tx.Execute("CREATE (n:Person {name: 'Lost'})")
	if err != nil {
		t.Fatalf("Execute: %v", err)
	}
	res.Free()
	// Close without commit/rollback. The C ABI / core must auto-rollback.
	db.Close()

	db2, err := Open(path)
	if err != nil {
		t.Fatalf("reopen: %v", err)
	}
	defer db2.Close()
	if got := personCount(t, db2); got != 0 {
		t.Fatalf("after close-without-commit: count = %d, want 0", got)
	}
	// File must still be writeable after recovery.
	tx2, err := db2.BeginWrite()
	if err != nil {
		t.Fatalf("BeginWrite after recovery: %v", err)
	}
	res2, err := tx2.Execute("CREATE (n:Person {name: 'Recovered'})")
	if err != nil {
		t.Fatalf("post-recovery Execute: %v", err)
	}
	res2.Free()
	if err := tx2.Commit(); err != nil {
		t.Fatalf("post-recovery Commit: %v", err)
	}
	if got := personCount(t, db2); got != 1 {
		t.Fatalf("post-recovery count = %d, want 1", got)
	}
}

// --------------------------------------------------------------------------
// BC-10 — result handles freed independently of the producing transaction
// --------------------------------------------------------------------------
func TestBC_10_ResultFreeAfterCommit(t *testing.T) {
	db, _ := openTempDB(t)
	defer db.Close()

	tx, err := db.BeginWrite()
	if err != nil {
		t.Fatalf("BeginWrite: %v", err)
	}
	res, err := tx.Execute("CREATE (n:Person {name: 'Alice'}) RETURN n.name AS name")
	if err != nil {
		t.Fatalf("Execute: %v", err)
	}
	if err := tx.Commit(); err != nil {
		t.Fatalf("Commit: %v", err)
	}
	// Result must still be readable after the producing tx commits.
	if got := res.ValueStr(0, 0); got != "Alice" {
		t.Fatalf("post-commit result read = %q, want %q", got, "Alice")
	}
	res.Free()
	// Free again — must not panic.
	res.Free()
}

// --------------------------------------------------------------------------
// WithWriteTx / WithReadTx callback wrappers — parity with Node's
// db.withWriteTx(fn) / db.withReadTx(fn). Not numbered as BC because the
// conformance checklist doesn't require them today.
// --------------------------------------------------------------------------

func TestWithWriteTx_CommitsOnSuccess(t *testing.T) {
	db, _ := openTempDB(t)
	defer db.Close()

	if err := db.WithWriteTx(func(tx *WriteTransaction) error {
		res, err := tx.Execute("CREATE (n:Person {name: 'Alice'})")
		if err != nil {
			return err
		}
		res.Free()
		return nil
	}); err != nil {
		t.Fatalf("WithWriteTx: %v", err)
	}
	if got := personCount(t, db); got != 1 {
		t.Fatalf("after WithWriteTx: count = %d, want 1", got)
	}
}

func TestWithWriteTx_RollsBackOnError(t *testing.T) {
	db, _ := openTempDB(t)
	defer db.Close()

	wantErr := "boom"
	err := db.WithWriteTx(func(tx *WriteTransaction) error {
		res, err := tx.Execute("CREATE (n:Person {name: 'Alice'})")
		if err != nil {
			return err
		}
		res.Free()
		return errFromString(wantErr)
	})
	if err == nil || err.Error() != wantErr {
		t.Fatalf("WithWriteTx err = %v, want %q", err, wantErr)
	}
	if got := personCount(t, db); got != 0 {
		t.Fatalf("after WithWriteTx error: count = %d, want 0", got)
	}
	// Database must still accept new transactions.
	if err := db.WithWriteTx(func(tx *WriteTransaction) error {
		res, err := tx.Execute("CREATE (n:Person {name: 'Bob'})")
		if err != nil {
			return err
		}
		res.Free()
		return nil
	}); err != nil {
		t.Fatalf("post-error WithWriteTx: %v", err)
	}
	if got := personCount(t, db); got != 1 {
		t.Fatalf("after recovery: count = %d, want 1", got)
	}
}

func TestWithWriteTx_RollsBackOnPanic(t *testing.T) {
	db, _ := openTempDB(t)
	defer db.Close()

	defer func() {
		if r := recover(); r == nil {
			t.Fatalf("expected panic to propagate, got none")
		}
		if got := personCount(t, db); got != 0 {
			t.Fatalf("after panic: count = %d, want 0", got)
		}
		// Subsequent transactions must work.
		if err := db.WithWriteTx(func(tx *WriteTransaction) error {
			res, err := tx.Execute("CREATE (n:Person {name: 'Carol'})")
			if err != nil {
				return err
			}
			res.Free()
			return nil
		}); err != nil {
			t.Fatalf("post-panic WithWriteTx: %v", err)
		}
		if got := personCount(t, db); got != 1 {
			t.Fatalf("post-panic count = %d, want 1", got)
		}
	}()

	_ = db.WithWriteTx(func(tx *WriteTransaction) error {
		res, _ := tx.Execute("CREATE (n:Person {name: 'Alice'})")
		if res != nil {
			res.Free()
		}
		panic("oops")
	})
}

func TestWithReadTx_ReleasesOnSuccess(t *testing.T) {
	db, _ := openTempDB(t)
	defer db.Close()

	// Seed one row outside the helper.
	if err := db.WithWriteTx(func(tx *WriteTransaction) error {
		res, err := tx.Execute("CREATE (n:Person {name: 'Alice'})")
		if err != nil {
			return err
		}
		res.Free()
		return nil
	}); err != nil {
		t.Fatalf("seed: %v", err)
	}

	var got int64
	if err := db.WithReadTx(func(tx *ReadTransaction) error {
		res, err := tx.Query("MATCH (n:Person) RETURN count(n) AS c")
		if err != nil {
			return err
		}
		got = res.ValueI64(0, 0)
		res.Free()
		return nil
	}); err != nil {
		t.Fatalf("WithReadTx: %v", err)
	}
	if got != 1 {
		t.Fatalf("WithReadTx count = %d, want 1", got)
	}
	// Read tx must have been released — opening a write tx must succeed.
	if _, err := db.BeginWrite(); err != nil {
		t.Fatalf("BeginWrite after WithReadTx: %v", err)
	}
}

// errFromString is a tiny helper that returns an error with a fixed message;
// keeps the test free of an extra package-level dependency.
type stringErr string

func (s stringErr) Error() string { return string(s) }

func errFromString(s string) error { return stringErr(s) }
