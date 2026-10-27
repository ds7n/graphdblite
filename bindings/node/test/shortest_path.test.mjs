// shortestPath through the Node binding (GitHub issue #2).
import test from 'node:test';
import assert from 'node:assert/strict';

import gdb from '../wrapper.js';

function mixedTypeDb() {
  const db = gdb.Database.openMemory();
  db.execute(
    "CREATE (a:P {name: 'A'})-[:KNOWS]->(b:P {name: 'B'})-[:WORKS_WITH]->(c:P {name: 'C'}), " +
      "(d:P {name: 'D'}), (a)-[:LIKES]->(e:P {name: 'E'})",
  );
  return db;
}

test('untyped shortestPath with unbound endpoints returns every reachable pair', () => {
  const db = mixedTypeDb();
  try {
    const rows = db.query(
      'MATCH p = shortestPath((a)-[*]->(b)) ' +
        'RETURN a.name AS src, b.name AS dst, length(p) AS len ORDER BY src, dst',
    );
    assert.deepEqual(
      rows.map((r) => [r.src, r.dst, Number(r.len)]),
      [
        ['A', 'B', 1],
        ['A', 'C', 2],
        ['A', 'E', 1],
        ['B', 'C', 1],
      ],
    );
  } finally {
    db.close();
  }
});

test('unreachable pair produces no row', () => {
  const db = mixedTypeDb();
  try {
    const rows = db.query(
      "MATCH (c:P {name: 'C'}), (a:P {name: 'A'}), p = shortestPath((c)-[*]->(a)) RETURN p",
    );
    assert.deepEqual(rows, []);
  } finally {
    db.close();
  }
});
