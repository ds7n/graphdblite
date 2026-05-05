// Tests for the JS-side wrapper layer (`withWriteTx`, `withReadTx`,
// `Symbol.dispose`). The native binding is verified by conformance.test.mjs.
//
// Run with: node --test test/wrapper.test.mjs

import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import gdb from '../wrapper.js';

function freshDb() {
  const dir = mkdtempSync(join(tmpdir(), 'gdb-wrap-'));
  const path = join(dir, 'db.sqlite');
  const db = new gdb.Database(path);
  return { db, path, cleanup: () => rmSync(dir, { recursive: true, force: true }) };
}

function personCount(qrunner) {
  const rows = qrunner.query('MATCH (n:Person) RETURN count(n) AS c');
  return Number(rows[0].c);
}

test('withWriteTx commits on success and returns the callback result', async () => {
  const { db, cleanup } = freshDb();
  try {
    const id = await db.withWriteTx(tx => {
      const rows = tx.execute("CREATE (n:Person {name: 'Alice'}) RETURN id(n) AS id");
      return Number(rows[0].id);
    });
    assert.equal(typeof id, 'number');
    assert.equal(personCount(db), 1);
  } finally { db.close(); cleanup(); }
});

test('withWriteTx rolls back when the callback throws', async () => {
  const { db, cleanup } = freshDb();
  try {
    await assert.rejects(
      db.withWriteTx(async tx => {
        tx.execute("CREATE (n:Person {name: 'Alice'})");
        throw new Error('boom');
      }),
      /boom/,
    );
    assert.equal(personCount(db), 0);
  } finally { db.close(); cleanup(); }
});

test('withReadTx returns the callback result and releases the txn', async () => {
  const { db, cleanup } = freshDb();
  try {
    db.execute("CREATE (n:Person {name: 'Alice'})");
    const count = await db.withReadTx(tx => personCount(tx));
    assert.equal(count, 1);
  } finally { db.close(); cleanup(); }
});

test('Symbol.dispose rolls back if the txn is still active', () => {
  const { db, cleanup } = freshDb();
  try {
    {
      const tx = db.beginWrite();
      tx.execute("CREATE (n:Person {name: 'Lost'})");
      tx[Symbol.dispose]();
    }
    assert.equal(personCount(db), 0);
  } finally { db.close(); cleanup(); }
});

test('Symbol.dispose is a no-op if the txn was already committed', () => {
  const { db, cleanup } = freshDb();
  try {
    const tx = db.beginWrite();
    tx.execute("CREATE (n:Person {name: 'Alice'})");
    tx.commit();
    // Should not throw.
    tx[Symbol.dispose]();
    assert.equal(personCount(db), 1);
  } finally { db.close(); cleanup(); }
});
