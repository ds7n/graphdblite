package graphdblite

import (
	"os"
	"path/filepath"
	"testing"
)

func TestSnapshotToProducesSingleFile(t *testing.T) {
	dir := t.TempDir()
	src := filepath.Join(dir, "src.db")
	db, err := Open(src)
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	defer db.Close()

	res, err := db.Execute("CREATE (:Person {name: 'Alice'}), (:Person {name: 'Bob'})")
	if err != nil {
		t.Fatalf("seed Execute: %v", err)
	}
	res.Free()

	dst := filepath.Join(dir, "snap.db")
	if err := db.SnapshotTo(dst); err != nil {
		t.Fatalf("SnapshotTo: %v", err)
	}

	if _, err := os.Stat(dst); err != nil {
		t.Fatalf("snapshot file missing: %v", err)
	}
	for _, sidecar := range []string{dst + "-wal", dst + "-shm"} {
		if _, err := os.Stat(sidecar); err == nil {
			t.Fatalf("unexpected sidecar present: %s", sidecar)
		}
	}

	snap, err := Open(dst)
	if err != nil {
		t.Fatalf("Open snapshot: %v", err)
	}
	defer snap.Close()
	rows, err := snap.Query("MATCH (n:Person) RETURN count(n) AS c")
	if err != nil {
		t.Fatalf("Query: %v", err)
	}
	defer rows.Free()
	if got := rows.ValueI64(0, 0); got != 2 {
		t.Fatalf("count = %d, want 2", got)
	}
}

func TestSnapshotToRejectsExistingTarget(t *testing.T) {
	dir := t.TempDir()
	db, err := OpenMemory()
	if err != nil {
		t.Fatalf("OpenMemory: %v", err)
	}
	defer db.Close()

	dst := filepath.Join(dir, "exists.db")
	if err := os.WriteFile(dst, []byte{}, 0o600); err != nil {
		t.Fatalf("WriteFile: %v", err)
	}
	if err := db.SnapshotTo(dst); err == nil {
		t.Fatalf("SnapshotTo should fail when target exists")
	}
}

func TestSnapshotToRejectsWhenTxActive(t *testing.T) {
	dir := t.TempDir()
	db, err := Open(filepath.Join(dir, "src.db"))
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	defer db.Close()

	tx, err := db.BeginWrite()
	if err != nil {
		t.Fatalf("BeginWrite: %v", err)
	}
	defer tx.Rollback()
	if err := db.SnapshotTo(filepath.Join(dir, "snap.db")); err == nil {
		t.Fatalf("SnapshotTo should fail when tx active")
	}
}
