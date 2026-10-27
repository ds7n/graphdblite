// Tests for the 2026-10 review fixes in the Node binding: read-transaction
// rollback (wrapper paths), BigInt for large integers, parameters, path /
// relationship shapes, stale transaction objects, `:memory:`, and panic
// containment.
//
// Run with: node --test test/review.test.mjs

import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, readdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import gdb from '../wrapper.js';

const NESTED = /^transaction error: begin_write: a Read transaction is already active; nested transactions are not supported/;

function count(db, label) {
  return db.query(`MATCH (n:${label}) RETURN count(n) AS c`)[0].c;
}

// F3: withReadTx rolls the read transaction back when the callback throws,
// so the handle is free for the next transaction.
test('withReadTx releases the read tx when the callback throws', async () => {
  const db = gdb.Database.openMemory();
  try {
    await assert.rejects(
      db.withReadTx(() => { throw new Error('boom'); }),
      { message: 'boom' },
    );
    const tx = db.beginWrite();
    tx.execute('CREATE (:A)');
    tx.commit();
    assert.equal(count(db, 'A'), 1);
  } finally { db.close(); }
});

// F3: Symbol.dispose on a read transaction releases it.
test('Symbol.dispose releases a read transaction', () => {
  const db = gdb.Database.openMemory();
  try {
    const rtx = db.beginRead();
    assert.throws(() => db.beginWrite(), { message: NESTED });
    rtx[Symbol.dispose]();
    const tx = db.beginWrite();
    tx.commit();
    // A second dispose is a no-op, rollback() after it reports finished.
    rtx[Symbol.dispose]();
    assert.throws(() => rtx.rollback(), { message: 'transaction is finished' });
  } finally { db.close(); }
});

// F3: the `using` declaration (where the runtime supports it).
test('using releases a read transaction at scope exit', (t) => {
  let run;
  try {
    // eslint-disable-next-line no-new-func
    run = new Function('db', '{ using rtx = db.beginRead(); rtx.query("RETURN 1 AS x"); }');
  } catch (e) {
    if (e instanceof SyntaxError) { t.skip('runtime has no `using` declarations'); return; }
    throw e;
  }
  const db = gdb.Database.openMemory();
  try {
    run(db);
    const tx = db.beginWrite();
    tx.commit();
  } finally { db.close(); }
});

test('ReadTransaction.rollback ends the transaction', () => {
  const db = gdb.Database.openMemory();
  try {
    const rtx = db.beginRead();
    rtx.rollback();
    assert.throws(() => rtx.query('RETURN 1 AS x'), { message: 'transaction is finished' });
    assert.throws(() => rtx.commit(), { message: 'transaction is finished' });
    db.beginWrite().commit();
  } finally { db.close(); }
});

// D2: integers outside +-(2^53 - 1) come back as BigInt; others as numbers.
test('large integers are BigInt', () => {
  const db = gdb.Database.openMemory();
  try {
    const [row] = db.query(
      'RETURN 9007199254740991 AS a, 9007199254740992 AS b, -9007199254740992 AS c, ' +
      '-9007199254740991 AS d, 9223372036854775807 AS e',
    );
    assert.equal(row.a, 9007199254740991);
    assert.equal(row.b, 9007199254740992n);
    assert.equal(row.c, -9007199254740992n);
    assert.equal(row.d, -9007199254740991);
    assert.equal(row.e, 9223372036854775807n);
  } finally { db.close(); }
});

// D7: parameters on every query method.
test('parameters map JS values to Cypher values', () => {
  const db = gdb.Database.openMemory();
  try {
    const [row] = db.query(
      'RETURN $i + 1 AS i, $f * 2 AS f, $s AS s, $b AS b, $n AS n, $u AS u, ' +
      '$l[2][0] AS l, $m.k[1] AS mk, $big AS big, $i / 2 AS idiv',
      {
        i: 7, f: 1.25, s: 'x', b: true, n: null, u: undefined,
        l: [1, 'two', [3.5]], m: { k: [10, 20] }, big: 2n ** 62n,
      },
    );
    assert.deepEqual(row, {
      i: 8, f: 2.5, s: 'x', b: true, n: null, u: null,
      l: 3.5, mk: 20, big: 4611686018427387904n, idiv: 3,
    });
    db.execute('CREATE (:P {name: $n})', { n: 'Ann' });
    const tx = db.beginWrite();
    tx.execute('CREATE (:P {name: $n})', { n: 'Bob' });
    tx.query('CREATE (:P {name: $n})', { n: 'Cy' });
    tx.commit();
    const rtx = db.beginRead();
    const rows = rtx.query('MATCH (p:P) WHERE p.name IN $names RETURN count(p) AS c', { names: ['Ann', 'Bob'] });
    rtx.commit();
    assert.equal(rows[0].c, 2);
  } finally { db.close(); }
});

test('bad parameters are rejected with a specific error', () => {
  const db = gdb.Database.openMemory();
  try {
    assert.throws(() => db.query('RETURN $x AS x', { x: 2n ** 63n }),
      { message: 'parameter `x`: BigInt outside the signed 64-bit range' });
    assert.throws(() => db.query('RETURN $x AS x', { x: new Date(0) }),
      { message: 'parameter `x`: Date is not supported; pass an ISO string and convert in Cypher' });
    assert.throws(() => db.query('RETURN $x AS x', { x: () => 1 }),
      { message: 'parameter `x`: unsupported type Function' });
    assert.throws(() => db.query('RETURN $nope AS x', {}),
      { message: 'ArgumentError(Other) at semantic analysis: missing parameter: $nope' });
    // Boundary: the largest signed 64-bit BigInt is accepted.
    assert.equal(db.query('RETURN $x AS x', { x: 2n ** 63n - 1n })[0].x, 9223372036854775807n);
  } finally { db.close(); }
});

// D1 / F14: path and relationship shapes.
test('paths are {nodes, relationships}', () => {
  const db = gdb.Database.openMemory();
  try {
    db.execute('CREATE (:A {k: 1})-[:R {w: 2}]->(:B)');
    const [row] = db.query('MATCH p = (a:A)-[r]->(b:B) RETURN p, id(a) AS a, id(b) AS b');
    assert.deepEqual(Object.keys(row.p), ['nodes', 'relationships']);
    assert.deepEqual(row.p.nodes.map(n => n.__labels), [['A'], ['B']]);
    assert.deepEqual(row.p.nodes.map(n => n.__id), [row.a, row.b]);
    assert.deepEqual(row.p.relationships, [
      { __src: row.a, __dst: row.b, __type: 'R', __label: 'R', __edge_seq: 0, w: 2 },
    ]);
  } finally { db.close(); }
});

// D3 / BC-13: a failed statement is rolled back alone.
test('BC-13 failed statement inside a transaction is rolled back alone', () => {
  const db = gdb.Database.openMemory();
  try {
    const tx = db.beginWrite();
    tx.execute('CREATE (:K)');
    assert.throws(() => tx.execute("UNWIND [1, 'a'] AS i CREATE (:M) RETURN i - 1 AS x"),
      { message: /^TypeError\(InvalidArgumentType\) at runtime: / });
    tx.execute('CREATE (:K2)');
    tx.commit();
    assert.deepEqual([count(db, 'K'), count(db, 'K2'), count(db, 'M')], [1, 1, 0]);
  } finally { db.close(); }
});

// close() with an open transaction rolls it back; the tx object then
// reports the closed database.
test('close rolls back the open transaction', () => {
  const dir = mkdtempSync(join(tmpdir(), 'gdb-rev-'));
  try {
    const path = join(dir, 'c.db');
    const db = new gdb.Database(path);
    const tx = db.beginWrite();
    tx.execute('CREATE (:Q)');
    db.close();
    assert.throws(() => tx.commit(), { message: 'database is closed' });
    assert.throws(() => tx.execute('CREATE (:Q)'), { message: 'transaction is finished' });
    const db2 = new gdb.Database(path);
    assert.equal(count(db2, 'Q'), 0);
    db2.close();
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

// F12: ':memory:' opens a private in-memory database, not a file.
test(':memory: path is in-memory', () => {
  const dir = mkdtempSync(join(tmpdir(), 'gdb-mem-'));
  const cwd = process.cwd();
  try {
    process.chdir(dir);
    const a = new gdb.Database(':memory:');
    a.execute('CREATE (:X)');
    const b = new gdb.Database(':memory:');
    assert.deepEqual(readdirSync(dir), []);
    assert.deepEqual([count(a, 'X'), count(b, 'X')], [1, 0]);
    a.close(); b.close();
  } finally { process.chdir(cwd); rmSync(dir, { recursive: true, force: true }); }
});

// F16: a Rust panic surfaces as a JS error and the handle stays usable.
// Needs the core's test hook: `npm run build -- --features test-hooks`.
const PANIC_QUERY = 'CALL graphdblite.testPanic()';
const PANIC_MESSAGE = 'graphdblite test hook: deliberate panic';

function hooksEnabled() {
  const db = gdb.Database.openMemory();
  try {
    db.query(PANIC_QUERY);
    return false;
  } catch (e) {
    return e.message === PANIC_MESSAGE;
  } finally { db.close(); }
}

test('panic surfaces as an error and the handle stays usable', (t) => {
  if (!hooksEnabled()) { t.skip('built without the test-hooks feature'); return; }
  const db = gdb.Database.openMemory();
  try {
    assert.throws(() => db.query(PANIC_QUERY), { message: PANIC_MESSAGE });
    db.execute('CREATE (:A)');
    assert.equal(count(db, 'A'), 1);
    const tx = db.beginWrite();
    tx.execute('CREATE (:B)');
    assert.throws(() => tx.execute(PANIC_QUERY), { message: PANIC_MESSAGE });
    // The panic rolled the transaction back and finished this object.
    assert.throws(() => tx.execute('CREATE (:B)'), { message: 'transaction is finished' });
    assert.equal(count(db, 'B'), 0);
    db.beginWrite().commit();
  } finally { db.close(); }
});

// Parameter nesting is bounded: deeper structures overflowed the native
// stack in the recursive conversion (a crash, not an exception).
function nestedArray(depth) {
  let v = 1;
  for (let i = 0; i < depth; i++) v = [v];
  return v;
}

test('deeply nested parameters are rejected, not a crash', () => {
  const db = gdb.Database.openMemory();
  try {
    const msg = 'parameter `p`: nesting exceeds the maximum depth of 128';
    assert.throws(() => db.query('RETURN 1 AS x', { p: nestedArray(10000) }), { message: msg });
    assert.throws(() => db.query('RETURN 1 AS x', { p: nestedArray(129) }), { message: msg });
    // Boundary: 128 levels still convert.
    assert.equal(db.query('RETURN $p AS x', { p: nestedArray(128) }).length, 1);
    // Objects count the same way.
    let o = 1;
    for (let i = 0; i < 10000; i++) o = { a: o };
    assert.throws(() => db.query('RETURN 1 AS x', { p: o }), { message: msg });
  } finally { db.close(); }
});
