package graphdblite

import "testing"

// shortestPath through the Go binding (GitHub issue #2).

func mixedTypeDB(t *testing.T) *Database {
	t.Helper()
	db, err := OpenMemory()
	if err != nil {
		t.Fatalf("OpenMemory: %v", err)
	}
	res, err := db.Execute("CREATE (a:P {name: 'A'})-[:KNOWS]->(b:P {name: 'B'})-[:WORKS_WITH]->(c:P {name: 'C'}), " +
		"(d:P {name: 'D'}), (a)-[:LIKES]->(e:P {name: 'E'})")
	if err != nil {
		db.Close()
		t.Fatalf("Execute CREATE: %v", err)
	}
	res.Free()
	return db
}

func TestShortestPathUntypedUnboundEndpoints(t *testing.T) {
	db := mixedTypeDB(t)
	defer db.Close()

	res, err := db.Query("MATCH p = shortestPath((a)-[*]->(b)) " +
		"RETURN a.name AS src, b.name AS dst, length(p) AS len ORDER BY src, dst")
	if err != nil {
		t.Fatalf("Query: %v", err)
	}
	defer res.Free()

	want := []struct {
		src, dst string
		len      int64
	}{{"A", "B", 1}, {"A", "C", 2}, {"A", "E", 1}, {"B", "C", 1}}
	if res.RowCount() != int64(len(want)) {
		t.Fatalf("expected %d rows, got %d", len(want), res.RowCount())
	}
	for i, w := range want {
		row := int64(i)
		src, dst, l := res.ValueStr(row, 0), res.ValueStr(row, 1), res.ValueI64(row, 2)
		if src != w.src || dst != w.dst || l != w.len {
			t.Errorf("row %d = (%q, %q, %d), want (%q, %q, %d)", i, src, dst, l, w.src, w.dst, w.len)
		}
	}
}

func TestShortestPathUnreachablePairHasNoRow(t *testing.T) {
	db := mixedTypeDB(t)
	defer db.Close()

	res, err := db.Query("MATCH (c:P {name: 'C'}), (a:P {name: 'A'}), " +
		"p = shortestPath((c)-[*]->(a)) RETURN p")
	if err != nil {
		t.Fatalf("Query: %v", err)
	}
	defer res.Free()
	if res.RowCount() != 0 {
		t.Fatalf("expected 0 rows, got %d", res.RowCount())
	}
}
