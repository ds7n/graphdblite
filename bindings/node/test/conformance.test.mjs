// Binding conformance tests — see ../../docs/BINDING_CONFORMANCE.md.
//
// Each test is named "BC-NN — ..." so a failure points at a specific
// checklist row. Run with: node --test test/conformance.test.mjs

import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import gdb from '../index.js';

function freshDb(name = 'db.sqlite') {
  const dir = mkdtempSync(join(tmpdir(), 'gdb-bc-'));
  const path = join(dir, name);
  const db = new gdb.Database(path);
  return { db, path, cleanup: () => rmSync(dir, { recursive: true, force: true }) };
}

function personCount(qrunner) {
  const rows = qrunner.query('MATCH (n:Person) RETURN count(n) AS c');
  // count returns BigInt-safe int64; coerce.
  return Number(rows[0].c);
}

test('BC-01 — explicit rollback discards writes', () => {
  const { db, cleanup } = freshDb();
  try {
    const tx = db.beginWrite();
    tx.execute("CREATE (n:Person {name: 'Alice'})");
    tx.rollback();
    assert.equal(personCount(db), 0);
  } finally { db.close(); cleanup(); }
});

test('BC-02 — commit persists writes', () => {
  const { db, cleanup } = freshDb();
  try {
    const tx = db.beginWrite();
    tx.execute("CREATE (n:Person {name: 'Alice'})");
    tx.commit();
    assert.equal(personCount(db), 1);
  } finally { db.close(); cleanup(); }
});

test('BC-03 — exception mid-transaction rolls back; next op succeeds', () => {
  const { db, cleanup } = freshDb();
  try {
    const tx = db.beginWrite();
    try {
      tx.execute("CREATE (n:Person {name: 'Alice'})");
      throw new Error('boom');
    } catch (e) {
      assert.equal(e.message, 'boom');
      tx.rollback();
    }
    assert.equal(personCount(db), 0);
    // Database is still usable
    const tx2 = db.beginWrite();
    tx2.execute("CREATE (n:Person {name: 'Bob'})");
    tx2.commit();
    assert.equal(personCount(db), 1);
  } finally { db.close(); cleanup(); }
});

test('BC-04 — nested begin returns an error; first tx still usable', () => {
  const { db, cleanup } = freshDb();
  try {
    const tx = db.beginWrite();
    assert.throws(() => db.beginWrite());
    assert.throws(() => db.beginRead());
    tx.execute("CREATE (n:Person {name: 'Alice'})");
    tx.commit();
    assert.equal(personCount(db), 1);
  } finally { db.close(); cleanup(); }
});

test('BC-05 — write inside a read transaction returns an error', () => {
  const { db, cleanup } = freshDb();
  try {
    const tx = db.beginRead();
    try {
      assert.throws(() => tx.query("CREATE (n:Person {name: 'Alice'})"));
    } finally {
      tx.commit();
    }
    assert.equal(personCount(db), 0);
  } finally { db.close(); cleanup(); }
});

test('BC-06 — double commit returns an error', () => {
  const { db, cleanup } = freshDb();
  try {
    const tx = db.beginWrite();
    tx.execute("CREATE (n:Person {name: 'Alice'})");
    tx.commit();
    assert.throws(() => tx.commit());
  } finally { db.close(); cleanup(); }
});

test('BC-07 — operations on a finished transaction return an error', () => {
  const { db, cleanup } = freshDb();
  try {
    const txC = db.beginWrite();
    txC.execute("CREATE (n:Person {name: 'Alice'})");
    txC.commit();
    assert.throws(() => txC.execute("CREATE (n:Person {name: 'Bob'})"));
    assert.throws(() => txC.rollback());

    const txR = db.beginWrite();
    txR.execute("CREATE (n:Person {name: 'Carol'})");
    txR.rollback();
    assert.throws(() => txR.execute("CREATE (n:Person {name: 'Dan'})"));
  } finally { db.close(); cleanup(); }
});

test('BC-08 — concurrent reader sees pre-txn snapshot until writer commits', () => {
  const dir = mkdtempSync(join(tmpdir(), 'gdb-bc-'));
  const path = join(dir, 'shared.sqlite');
  const writer = new gdb.Database(path);
  try {
    writer.execute("CREATE (n:Person {name: 'Seed'})");
    const reader = new gdb.Database(path);
    try {
      assert.equal(personCount(reader), 1);
      const tx = writer.beginWrite();
      tx.execute("CREATE (n:Person {name: 'Alice'})");
      assert.equal(personCount(reader), 1);
      tx.commit();
      assert.equal(personCount(reader), 2);
    } finally { reader.close(); }
  } finally { writer.close(); rmSync(dir, { recursive: true, force: true }); }
});

test('BC-09 — closing the DB with an open tx does not corrupt the file', () => {
  const dir = mkdtempSync(join(tmpdir(), 'gdb-bc-'));
  const path = join(dir, 'drop.sqlite');
  try {
    const db = new gdb.Database(path);
    const tx = db.beginWrite();
    tx.execute("CREATE (n:Person {name: 'Lost'})");
    // Close without commit — Drop on the tx rolls back; close drops the
    // RustDatabase, whose own Drop is a defensive backstop.
    db.close();
    // Reopen and verify
    const db2 = new gdb.Database(path);
    try {
      assert.equal(personCount(db2), 0);
      const tx2 = db2.beginWrite();
      tx2.execute("CREATE (n:Person {name: 'Recovered'})");
      tx2.commit();
      assert.equal(personCount(db2), 1);
    } finally { db2.close(); }
  } finally { rmSync(dir, { recursive: true, force: true }); }
});
