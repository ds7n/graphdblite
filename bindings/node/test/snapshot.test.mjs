// Tests for `Database.snapshotTo`.

import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, existsSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import gdb from '../wrapper.js';

function tempDir() {
  return mkdtempSync(join(tmpdir(), 'gdb-snap-'));
}

test('snapshotTo produces a single-file copy with the same data', () => {
  const dir = tempDir();
  try {
    const src = join(dir, 'src.db');
    const db = new gdb.Database(src);
    db.execute("CREATE (:Person {name: 'Alice'}), (:Person {name: 'Bob'})");
    const dst = join(dir, 'snap.db');
    db.snapshotTo(dst);
    db.close();

    assert.ok(existsSync(dst));
    assert.ok(!existsSync(dst + '-wal'));
    assert.ok(!existsSync(dst + '-shm'));

    const snap = new gdb.Database(dst);
    const rows = snap.query('MATCH (n:Person) RETURN n.name AS name');
    assert.equal(rows.length, 2);
    snap.close();
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('snapshotTo rejects an existing target', () => {
  const dir = tempDir();
  try {
    const db = gdb.Database.openMemory();
    const existing = join(dir, 'exists.db');
    writeFileSync(existing, '');
    assert.throws(() => db.snapshotTo(existing));
    db.close();
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('snapshotTo rejects while a transaction is active', () => {
  const dir = tempDir();
  try {
    const db = new gdb.Database(join(dir, 'src.db'));
    const tx = db.beginWrite();
    try {
      assert.throws(() => db.snapshotTo(join(dir, 'snap.db')));
    } finally { tx.rollback(); db.close(); }
  } finally { rmSync(dir, { recursive: true, force: true }); }
});
