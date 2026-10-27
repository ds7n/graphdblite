// Tests for the 2026-10 review fixes: statement atomicity inside
// transactions, parameters, value lifetimes, JSON shape, error handling
// across goroutines, and GC safety.

package graphdblite

import (
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"os"
	"runtime"
	"strings"
	"sync"
	"testing"
)

// wantErr fails the test unless err is non-nil and its message contains
// want (the specific failure, not just "some error").
func wantErr(t *testing.T, err error, want string) {
	t.Helper()
	if err == nil {
		t.Fatalf("got nil error, want %q", want)
	}
	if !strings.Contains(err.Error(), want) {
		t.Fatalf("error = %q, want it to contain %q", err, want)
	}
}

func mustOpenMemory(t *testing.T) *Database {
	t.Helper()
	db, err := OpenMemory()
	if err != nil {
		t.Fatalf("OpenMemory: %v", err)
	}
	return db
}

func mustExec(t *testing.T, db *Database, q string) {
	t.Helper()
	res, err := db.Execute(q)
	if err != nil {
		t.Fatalf("Execute(%q): %v", q, err)
	}
	res.Free()
}

func countLabel(t *testing.T, db *Database, label string) int64 {
	t.Helper()
	res, err := db.Query(fmt.Sprintf("MATCH (n:%s) RETURN count(n) AS c", label))
	if err != nil {
		t.Fatalf("count %s: %v", label, err)
	}
	defer res.Free()
	return res.ValueI64(0, 0)
}

// BC-13: a failed statement inside a transaction is rolled back on its own;
// the transaction stays usable and commits the other statements.
func TestBC_13_StatementAtomicity(t *testing.T) {
	db := mustOpenMemory(t)
	defer db.Close()
	tx, err := db.BeginWrite()
	if err != nil {
		t.Fatalf("BeginWrite: %v", err)
	}
	res, err := tx.Execute("CREATE (:K)")
	if err != nil {
		t.Fatalf("first statement: %v", err)
	}
	res.Free()
	_, err = tx.Execute("UNWIND [1, 'a'] AS i CREATE (:M) RETURN i - 1 AS x")
	want := "graphdblite tx query: TypeError(InvalidArgumentType) at runtime: Type mismatch: cannot apply `-` to String and Integer"
	if err == nil || err.Error() != want {
		t.Fatalf("failing statement: err = %v, want %q", err, want)
	}
	res, err = tx.Execute("CREATE (:K2)")
	if err != nil {
		t.Fatalf("statement after failure: %v", err)
	}
	res.Free()
	if err := tx.Commit(); err != nil {
		t.Fatalf("Commit: %v", err)
	}
	for label, want := range map[string]int64{"K": 1, "K2": 1, "M": 0} {
		if got := countLabel(t, db, label); got != want {
			t.Errorf("count(%s) = %d, want %d", label, got, want)
		}
	}
}

// D7: parameters, including nested lists/maps; floats stay floats even
// when integral.
func TestQueryParamsTypes(t *testing.T) {
	db := mustOpenMemory(t)
	defer db.Close()
	res, err := db.QueryParams(
		"RETURN $i AS i, $f AS f, $s AS s, $b AS b, $n AS n, $l AS l, $m.k[1] AS mk, $u AS u",
		map[string]any{
			"i": 7,
			"f": 1.0,
			"s": "x",
			"b": true,
			"n": nil,
			"l": []any{1, "two", []float64{3}},
			"m": map[string]any{"k": []int{10, 20}},
			"u": uint8(5),
		},
	)
	if err != nil {
		t.Fatalf("QueryParams: %v", err)
	}
	defer res.Free()
	checks := []struct {
		col      int64
		wantType int32
	}{{0, TypeI64}, {1, TypeF64}, {2, TypeString}, {3, TypeBool}, {4, TypeNull}, {5, TypeList}, {6, TypeI64}, {7, TypeI64}}
	for _, c := range checks {
		if got := res.ValueType(0, c.col); got != c.wantType {
			t.Errorf("column %d type = %d, want %d", c.col, got, c.wantType)
		}
	}
	if got := res.ValueI64(0, 0); got != 7 {
		t.Errorf("i = %d, want 7", got)
	}
	if got := res.ValueF64(0, 1); got != 1.0 {
		t.Errorf("f = %v, want 1.0", got)
	}
	if got := res.ValueStr(0, 5); got != "[1, two, [3.0]]" {
		t.Errorf("l = %q", got)
	}
	if got := res.ValueI64(0, 6); got != 20 {
		t.Errorf("m.k[1] = %d, want 20", got)
	}
	if got := res.ValueI64(0, 7); got != 5 {
		t.Errorf("u = %d, want 5", got)
	}
}

// D7: execute and both transaction kinds take parameters.
func TestExecuteAndTxParams(t *testing.T) {
	db := mustOpenMemory(t)
	defer db.Close()
	res, err := db.ExecuteParams("CREATE (:P {name: $n})", map[string]any{"n": "Ann"})
	if err != nil {
		t.Fatalf("ExecuteParams: %v", err)
	}
	res.Free()
	tx, err := db.BeginWrite()
	if err != nil {
		t.Fatal(err)
	}
	res, err = tx.ExecuteParams("CREATE (:P {name: $n})", map[string]any{"n": "Bob"})
	if err != nil {
		t.Fatalf("tx.ExecuteParams: %v", err)
	}
	res.Free()
	if err := tx.Commit(); err != nil {
		t.Fatal(err)
	}
	rtx, err := db.BeginRead()
	if err != nil {
		t.Fatal(err)
	}
	res, err = rtx.QueryParams("MATCH (p:P) WHERE p.name IN $names RETURN count(p) AS c",
		map[string]any{"names": []string{"Ann", "Bob", "Cy"}})
	if err != nil {
		t.Fatalf("read tx QueryParams: %v", err)
	}
	if got := res.ValueI64(0, 0); got != 2 {
		t.Errorf("count = %d, want 2", got)
	}
	res.Free()
	if err := rtx.Commit(); err != nil {
		t.Fatal(err)
	}
}

// D7: unsupported or unrepresentable parameter values are rejected with a
// specific error before reaching the database.
func TestQueryParamsRejectsBadValues(t *testing.T) {
	db := mustOpenMemory(t)
	defer db.Close()
	cases := []struct {
		name   string
		params map[string]any
		want   string
	}{
		{"nan", map[string]any{"x": math.NaN()}, "graphdblite params: parameter \"x\": non-finite float NaN"},
		{"inf", map[string]any{"x": math.Inf(1)}, "graphdblite params: parameter \"x\": non-finite float +Inf"},
		{"uint overflow", map[string]any{"x": uint64(math.MaxInt64) + 1}, "graphdblite params: parameter \"x\": integer 9223372036854775808 overflows int64"},
		{"struct", map[string]any{"x": struct{}{}}, "graphdblite params: parameter \"x\": unsupported type struct {}"},
		{"int map key", map[string]any{"x": map[int]int{1: 1}}, "graphdblite params: parameter \"x\": unsupported type map[int]int"},
	}
	for _, c := range cases {
		_, err := db.QueryParams("RETURN $x AS x", c.params)
		if err == nil || err.Error() != c.want {
			t.Errorf("%s: err = %v, want %q", c.name, err, c.want)
		}
	}
	// Boundary: MaxInt64 itself is accepted.
	res, err := db.QueryParams("RETURN $x AS x", map[string]any{"x": uint64(math.MaxInt64)})
	if err != nil {
		t.Fatalf("MaxInt64: %v", err)
	}
	if got := res.ValueI64(0, 0); got != math.MaxInt64 {
		t.Errorf("x = %d", got)
	}
	res.Free()
	// A missing parameter is the core's error.
	_, err = db.QueryParams("RETURN $nope AS x", map[string]any{})
	want := "graphdblite query: ArgumentError(Other) at semantic analysis: missing parameter: $nope"
	if err == nil || err.Error() != want {
		t.Errorf("missing param: err = %v, want %q", err, want)
	}
}

// F5: ValueStr results do not depend on later calls.
func TestValueStrStableAcrossResults(t *testing.T) {
	db := mustOpenMemory(t)
	defer db.Close()
	r1, err := db.Query("RETURN 'aaaa' AS x, 'bbbb' AS y")
	if err != nil {
		t.Fatal(err)
	}
	defer r1.Free()
	r2, err := db.Query("RETURN 'cccc' AS x")
	if err != nil {
		t.Fatal(err)
	}
	defer r2.Free()
	a, b, c := r1.ValueStr(0, 0), r1.ValueStr(0, 1), r2.ValueStr(0, 0)
	if a != "aaaa" || b != "bbbb" || c != "cccc" {
		t.Fatalf("got %q %q %q", a, b, c)
	}
}

// D1 / F11 / F14: JSON shape of paths, floats and relationships.
func TestJSONShape(t *testing.T) {
	db := mustOpenMemory(t)
	defer db.Close()
	mustExec(t, db, "CREATE (:A {k: 1})-[:R {w: 2.0}]->(:B)")
	res, err := db.Query("MATCH p = (:A)-[r]->(:B) RETURN p, r, 1.0 AS f, toFloat('NaN') AS nan")
	if err != nil {
		t.Fatal(err)
	}
	defer res.Free()
	if got := res.ValueType(0, 0); got != TypePath {
		t.Errorf("path type = %d", got)
	}
	if got := res.ValueType(0, 1); got != TypeRelationship {
		t.Errorf("relationship type = %d", got)
	}
	var rows []map[string]any
	if err := json.Unmarshal([]byte(res.JSON()), &rows); err != nil {
		t.Fatalf("JSON() is not valid JSON: %v\n%s", err, res.JSON())
	}
	p := rows[0]["p"].(map[string]any)
	nodes := p["nodes"].([]any)
	rels := p["relationships"].([]any)
	if len(nodes) != 2 || len(rels) != 1 {
		t.Fatalf("path = %v", p)
	}
	if got := nodes[0].(map[string]any)["k"]; got != 1.0 {
		t.Errorf("first node k = %v", got)
	}
	r := rows[0]["r"].(map[string]any)
	if r["__type"] != "R" || r["__label"] != "R" || r["w"] != 2.0 {
		t.Errorf("relationship = %v", r)
	}
	if r["__src"] != nodes[0].(map[string]any)["__id"] {
		t.Errorf("relationship __src does not match the start node: %v", r)
	}
	if rows[0]["nan"] != nil {
		t.Errorf("NaN = %v, want null", rows[0]["nan"])
	}
	if !strings.Contains(res.JSON(), `"f": 1.0`) {
		t.Errorf("float lost its fraction: %s", res.JSON())
	}
}

// F11: every value type code has a constant.
func TestValueTypeConstants(t *testing.T) {
	db := mustOpenMemory(t)
	defer db.Close()
	mustExec(t, db, "CREATE (:A)-[:R]->(:B)")
	res, err := db.Query("MATCH (a:A)-[r]->() RETURN {k: 1} AS m, a, r, date('2020-01-02') AS d, " +
		"localtime('12:00') AS lt, time('12:00Z') AS t, localdatetime('2020-01-02T03:04') AS ldt, " +
		"datetime('2020-01-02T03:04Z') AS dt, duration('P1D') AS du")
	if err != nil {
		t.Fatal(err)
	}
	defer res.Free()
	want := []int32{TypeMap, TypeNode, TypeRelationship, TypeDate, TypeLocalTime, TypeTime, TypeLocalDateTime, TypeDateTime, TypeDuration}
	for col, w := range want {
		if got := res.ValueType(0, int64(col)); got != w {
			t.Errorf("column %d type = %d, want %d", col, got, w)
		}
	}
	if TypeMap != 7 || TypeDuration != 15 {
		t.Errorf("constants do not match the C codes")
	}
}

// F7: each goroutine sees its own error message even when many goroutines
// make failing and succeeding calls at the same time.
func TestErrorsAcrossGoroutines(t *testing.T) {
	const workers = 16
	const rounds = 50
	var wg sync.WaitGroup
	errs := make(chan string, workers*rounds)
	for w := 0; w < workers; w++ {
		wg.Add(1)
		go func(w int) {
			defer wg.Done()
			db, err := OpenMemory()
			if err != nil {
				errs <- err.Error()
				return
			}
			defer db.Close()
			want := fmt.Sprintf("graphdblite query: SyntaxError(UndefinedVariable) at line 1:8: missing_%d", w)
			for i := 0; i < rounds; i++ {
				if res, err := db.Query("RETURN 1 AS x"); err == nil {
					res.Free()
				} else {
					errs <- err.Error()
				}
				runtime.Gosched()
				_, err := db.Query(fmt.Sprintf("RETURN missing_%d AS x", w))
				if err == nil || err.Error() != want {
					errs <- fmt.Sprintf("worker %d: got %v, want %q", w, err, want)
				}
			}
		}(w)
	}
	wg.Wait()
	close(errs)
	for e := range errs {
		t.Error(e)
	}
}

// F6: the finalizer of a Database that becomes unreachable during a call
// must not close it mid-call.
func TestGCDuringCall(t *testing.T) {
	stop := make(chan struct{})
	done := make(chan struct{})
	go func() {
		defer close(done)
		for {
			select {
			case <-stop:
				return
			default:
				runtime.GC()
			}
		}
	}()
	for i := 0; i < 200; i++ {
		db, err := OpenMemory()
		if err != nil {
			t.Fatal(err)
		}
		// `db` is unreachable after this call starts.
		res, err := db.Execute("UNWIND range(1, 200) AS i CREATE (:N {i: i}) RETURN count(*) AS c")
		if err != nil {
			t.Fatalf("Execute: %v", err)
		}
		if got := res.ValueI64(0, 0); got != 200 {
			t.Fatalf("count = %d", got)
		}
		res.Free()
	}
	close(stop)
	<-done
}

// F9: a failed Commit keeps the transaction object usable, so the caller
// can still roll back.
func TestCommitFailureKeepsTx(t *testing.T) {
	db := mustOpenMemory(t)
	defer db.Close()
	// A transaction object whose core transaction is not open: Commit
	// reaches the core and fails there.
	tx := &WriteTransaction{db: db}
	err := tx.Commit()
	want := "graphdblite commit: transaction error: no active transaction to commit"
	if err == nil || err.Error() != want {
		t.Fatalf("Commit: err = %v, want %q", err, want)
	}
	if tx.db == nil {
		t.Fatalf("failed Commit marked the transaction finished")
	}
	err = tx.Rollback()
	want = "graphdblite rollback: transaction error: no active transaction to roll back"
	if err == nil || err.Error() != want {
		t.Fatalf("Rollback: err = %v, want %q", err, want)
	}
	if err := tx.Rollback(); !errors.Is(err, ErrTxFinished) {
		t.Fatalf("second Rollback: err = %v, want ErrTxFinished", err)
	}
}

// F12: ":memory:" opens a private in-memory database, not a file.
func TestOpenMemoryPath(t *testing.T) {
	a, err := Open(":memory:")
	if err != nil {
		t.Fatal(err)
	}
	defer a.Close()
	mustExec(t, a, "CREATE (:X)")
	if _, err := os.Stat(":memory:"); !errors.Is(err, os.ErrNotExist) {
		os.Remove(":memory:")
		t.Fatalf("Open(\":memory:\") created a file: stat err = %v", err)
	}
	b, err := Open(":memory:")
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	if got := countLabel(t, a, "X"); got != 1 {
		t.Errorf("a count = %d", got)
	}
	if got := countLabel(t, b, "X"); got != 0 {
		t.Errorf("b count = %d, in-memory databases must be private", got)
	}
}

// Closed handles report ErrClosed.
func TestClosedErrorsAreTyped(t *testing.T) {
	db := mustOpenMemory(t)
	db.Close()
	if _, err := db.Query("RETURN 1"); !errors.Is(err, ErrClosed) {
		t.Errorf("Query: err = %v, want ErrClosed", err)
	}
	if _, err := db.BeginWrite(); !errors.Is(err, ErrClosed) {
		t.Errorf("BeginWrite: err = %v, want ErrClosed", err)
	}
}
