package graphdblite

import (
	"testing"
)

func TestOpenMemory(t *testing.T) {
	db, err := OpenMemory()
	if err != nil {
		t.Fatalf("OpenMemory: %v", err)
	}
	defer db.Close()
}

func TestCreateAndQuery(t *testing.T) {
	db, err := OpenMemory()
	if err != nil {
		t.Fatalf("OpenMemory: %v", err)
	}
	defer db.Close()

	// Create nodes.
	res, err := db.Execute("CREATE (n:Person {name: 'Alice', age: 30})")
	if err != nil {
		t.Fatalf("Execute CREATE: %v", err)
	}
	res.Free()

	res, err = db.Execute("CREATE (n:Person {name: 'Bob', age: 25})")
	if err != nil {
		t.Fatalf("Execute CREATE: %v", err)
	}
	res.Free()

	// Query nodes.
	res, err = db.Query("MATCH (n:Person) RETURN n.name, n.age ORDER BY n.name")
	if err != nil {
		t.Fatalf("Query: %v", err)
	}
	defer res.Free()

	if res.RowCount() != 2 {
		t.Fatalf("expected 2 rows, got %d", res.RowCount())
	}
	if res.ColumnCount() != 2 {
		t.Fatalf("expected 2 columns, got %d", res.ColumnCount())
	}

	// Check column names.
	if name := res.ColumnName(0); name != "n.name" {
		t.Errorf("column 0 = %q, want %q", name, "n.name")
	}
	if name := res.ColumnName(1); name != "n.age" {
		t.Errorf("column 1 = %q, want %q", name, "n.age")
	}

	// Check values.
	if v := res.ValueStr(0, 0); v != "Alice" {
		t.Errorf("row 0, col 0 = %q, want %q", v, "Alice")
	}
	if v := res.ValueI64(0, 1); v != 30 {
		t.Errorf("row 0, col 1 = %d, want %d", v, 30)
	}
	if v := res.ValueStr(1, 0); v != "Bob" {
		t.Errorf("row 1, col 0 = %q, want %q", v, "Bob")
	}
	if v := res.ValueI64(1, 1); v != 25 {
		t.Errorf("row 1, col 1 = %d, want %d", v, 25)
	}
}

func TestWriteTransaction(t *testing.T) {
	db, err := OpenMemory()
	if err != nil {
		t.Fatalf("OpenMemory: %v", err)
	}
	defer db.Close()

	// Begin write transaction, create two nodes, commit.
	tx, err := db.BeginWrite()
	if err != nil {
		t.Fatalf("BeginWrite: %v", err)
	}

	res, err := tx.Execute("CREATE (n:Animal {species: 'Cat'})")
	if err != nil {
		t.Fatalf("tx Execute: %v", err)
	}
	res.Free()

	res, err = tx.Execute("CREATE (n:Animal {species: 'Dog'})")
	if err != nil {
		t.Fatalf("tx Execute: %v", err)
	}
	res.Free()

	if err := tx.Commit(); err != nil {
		t.Fatalf("Commit: %v", err)
	}

	// Verify data is there.
	res, err = db.Query("MATCH (n:Animal) RETURN n.species ORDER BY n.species")
	if err != nil {
		t.Fatalf("Query: %v", err)
	}
	defer res.Free()

	if res.RowCount() != 2 {
		t.Fatalf("expected 2 rows, got %d", res.RowCount())
	}
	if v := res.ValueStr(0, 0); v != "Cat" {
		t.Errorf("row 0 = %q, want %q", v, "Cat")
	}
}

func TestRollback(t *testing.T) {
	db, err := OpenMemory()
	if err != nil {
		t.Fatalf("OpenMemory: %v", err)
	}
	defer db.Close()

	// Create a node, then rollback.
	tx, err := db.BeginWrite()
	if err != nil {
		t.Fatalf("BeginWrite: %v", err)
	}

	res, err := tx.Execute("CREATE (n:Temp {val: 1})")
	if err != nil {
		t.Fatalf("tx Execute: %v", err)
	}
	res.Free()

	if err := tx.Rollback(); err != nil {
		t.Fatalf("Rollback: %v", err)
	}

	// Verify nothing is there.
	res, err = db.Query("MATCH (n:Temp) RETURN n.val")
	if err != nil {
		t.Fatalf("Query: %v", err)
	}
	defer res.Free()

	if res.RowCount() != 0 {
		t.Fatalf("expected 0 rows after rollback, got %d", res.RowCount())
	}
}

func TestJSON(t *testing.T) {
	db, err := OpenMemory()
	if err != nil {
		t.Fatalf("OpenMemory: %v", err)
	}
	defer db.Close()

	res, err := db.Execute("CREATE (n:X {v: 42})")
	if err != nil {
		t.Fatalf("Execute: %v", err)
	}
	res.Free()

	res, err = db.Query("MATCH (n:X) RETURN n.v")
	if err != nil {
		t.Fatalf("Query: %v", err)
	}
	defer res.Free()

	json := res.JSON()
	if json == "" || json == "[]" {
		t.Errorf("expected non-empty JSON, got %q", json)
	}
}

func TestValueTypes(t *testing.T) {
	db, err := OpenMemory()
	if err != nil {
		t.Fatalf("OpenMemory: %v", err)
	}
	defer db.Close()

	res, err := db.Execute("CREATE (n:T {b: true, i: 42, f: 3.14, s: 'hello'})")
	if err != nil {
		t.Fatalf("Execute: %v", err)
	}
	res.Free()

	res, err = db.Query("MATCH (n:T) RETURN n.b, n.i, n.f, n.s")
	if err != nil {
		t.Fatalf("Query: %v", err)
	}
	defer res.Free()

	if res.ValueType(0, 0) != TypeBool {
		t.Errorf("expected bool type for col 0, got %d", res.ValueType(0, 0))
	}
	if !res.ValueBool(0, 0) {
		t.Error("expected true for col 0")
	}
	if res.ValueType(0, 1) != TypeI64 {
		t.Errorf("expected i64 type for col 1, got %d", res.ValueType(0, 1))
	}
	if res.ValueI64(0, 1) != 42 {
		t.Errorf("expected 42 for col 1, got %d", res.ValueI64(0, 1))
	}
	if res.ValueType(0, 2) != TypeF64 {
		t.Errorf("expected f64 type for col 2, got %d", res.ValueType(0, 2))
	}
	if v := res.ValueF64(0, 2); v < 3.13 || v > 3.15 {
		t.Errorf("expected ~3.14 for col 2, got %f", v)
	}
	if res.ValueType(0, 3) != TypeString {
		t.Errorf("expected string type for col 3, got %d", res.ValueType(0, 3))
	}
	if v := res.ValueStr(0, 3); v != "hello" {
		t.Errorf("expected 'hello' for col 3, got %q", v)
	}
}

func TestClosedDatabaseError(t *testing.T) {
	db, err := OpenMemory()
	if err != nil {
		t.Fatalf("OpenMemory: %v", err)
	}
	db.Close()

	_, err = db.Query("MATCH (n) RETURN n")
	if err == nil {
		t.Error("expected error querying closed database")
	}
}

func TestCreateAndDropIndex(t *testing.T) {
	db, err := OpenMemory()
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()

	tx, err := db.BeginWrite()
	if err != nil {
		t.Fatal(err)
	}
	if err := tx.CreateIndex("Person", "name"); err != nil {
		_ = tx.Rollback()
		t.Fatalf("CreateIndex: %v", err)
	}
	if err := tx.CreateIndex("Person", "name"); err == nil {
		_ = tx.Rollback()
		t.Fatal("expected duplicate create_index to error")
	}
	if err := tx.DropIndex("Person", "name"); err != nil {
		_ = tx.Rollback()
		t.Fatalf("DropIndex: %v", err)
	}
	if err := tx.Commit(); err != nil {
		t.Fatal(err)
	}
}

func TestCreateAndDropFulltextIndex(t *testing.T) {
	db, err := OpenMemory()
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()

	tx, err := db.BeginWrite()
	if err != nil {
		t.Fatal(err)
	}
	if err := tx.CreateFulltextIndex("Doc", "body"); err != nil {
		_ = tx.Rollback()
		t.Fatalf("CreateFulltextIndex: %v", err)
	}
	res, err := tx.Execute("CREATE (:Doc {body: 'hello world'})")
	if err != nil {
		_ = tx.Rollback()
		t.Fatalf("insert: %v", err)
	}
	res.Free()
	if err := tx.Commit(); err != nil {
		t.Fatal(err)
	}

	// CONTAINS query should hit the FTS rewrite path.
	res, err = db.Query("MATCH (n:Doc) WHERE n.body CONTAINS 'hello' RETURN n.body")
	if err != nil {
		t.Fatalf("contains query: %v", err)
	}
	if got := res.RowCount(); got != 1 {
		res.Free()
		t.Fatalf("expected 1 row, got %d", got)
	}
	res.Free()

	tx, err = db.BeginWrite()
	if err != nil {
		t.Fatal(err)
	}
	if err := tx.DropFulltextIndex("Doc", "body"); err != nil {
		_ = tx.Rollback()
		t.Fatalf("DropFulltextIndex: %v", err)
	}
	if err := tx.DropFulltextIndex("Doc", "body"); err == nil {
		_ = tx.Rollback()
		t.Fatal("expected drop-of-missing to error")
	}
	if err := tx.Rollback(); err != nil {
		t.Fatal(err)
	}
}

func TestCreateFulltextIndexCI(t *testing.T) {
	db, err := OpenMemory()
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()

	tx, err := db.BeginWrite()
	if err != nil {
		t.Fatal(err)
	}
	if err := tx.CreateFulltextIndexCI("Doc", "body"); err != nil {
		_ = tx.Rollback()
		t.Fatalf("CreateFulltextIndexCI: %v", err)
	}
	res, err := tx.Execute("CREATE (:Doc {body: 'Hello World'})")
	if err != nil {
		_ = tx.Rollback()
		t.Fatalf("insert: %v", err)
	}
	res.Free()
	if err := tx.Commit(); err != nil {
		t.Fatal(err)
	}

	// Case-mismatched CONTAINS must hit the CI FTS rewrite path.
	res, err = db.Query("MATCH (n:Doc) WHERE n.body CONTAINS 'hello' RETURN n.body")
	if err != nil {
		t.Fatalf("contains query: %v", err)
	}
	if got := res.RowCount(); got != 1 {
		res.Free()
		t.Fatalf("expected 1 row from CI CONTAINS, got %d", got)
	}
	res.Free()
}
