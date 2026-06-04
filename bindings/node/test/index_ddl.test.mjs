// Tests for WriteTransaction index DDL methods (BC-11).
import test from 'node:test';
import assert from 'node:assert/strict';

import gdb from '../wrapper.js';

test('createIndex / dropIndex round-trip', () => {
  const db = gdb.Database.openMemory();
  try {
    const tx = db.beginWrite();
    tx.createIndex('Person', 'name');
    assert.throws(() => tx.createIndex('Person', 'name'),
      /already exists|index/i,
      'duplicate create_index should error');
    tx.dropIndex('Person', 'name');
    tx.commit();
  } finally {
    db.close();
  }
});

test('createFulltextIndex enables CONTAINS via FTS', () => {
  const db = gdb.Database.openMemory();
  try {
    const tx = db.beginWrite();
    tx.createFulltextIndex('Doc', 'body');
    tx.execute("CREATE (:Doc {body: 'hello world'})");
    tx.commit();

    const rows = db.query("MATCH (n:Doc) WHERE n.body CONTAINS 'hello' RETURN n.body AS body");
    assert.equal(rows.length, 1);
    assert.equal(rows[0].body, 'hello world');

    const tx2 = db.beginWrite();
    tx2.dropFulltextIndex('Doc', 'body');
    assert.throws(() => tx2.dropFulltextIndex('Doc', 'body'),
      /not found|index/i,
      'drop of missing index should error');
    tx2.rollback();
  } finally {
    db.close();
  }
});
