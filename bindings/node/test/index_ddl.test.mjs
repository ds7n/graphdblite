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

test('createFulltextIndexCi enables case-insensitive CONTAINS via FTS', () => {
  const db = gdb.Database.openMemory();
  try {
    const tx = db.beginWrite();
    tx.createFulltextIndexCi('Doc', 'body');
    tx.execute("CREATE (:Doc {body: 'Hello World'})");
    tx.commit();

    const rows = db.query("MATCH (n:Doc) WHERE n.body CONTAINS 'hello' RETURN n.body AS body");
    assert.equal(rows.length, 1);
    assert.equal(rows[0].body, 'Hello World');

    const tx2 = db.beginWrite();
    tx2.dropFulltextIndex('Doc', 'body');
    tx2.commit();
  } finally {
    db.close();
  }
});

test('createFulltextIndexWord enables fts.search', () => {
  const db = gdb.Database.openMemory();
  try {
    const tx = db.beginWrite();
    tx.execute("CREATE (:Doc {body:'rust systems programming'}), (:Doc {body:'python data science'})");
    tx.createFulltextIndexWord('Doc', 'body');
    tx.commit();

    const rows = db.query(
      "CALL fts.search('Doc', 'body', 'rust') YIELD node, score RETURN node.body AS body, score"
    );
    assert.equal(rows.length, 1);
    assert.equal(rows[0].body, 'rust systems programming');
    assert.ok(rows[0].score > 0);

    const tx2 = db.beginWrite();
    tx2.dropFulltextIndex('Doc', 'body');
    tx2.commit();
  } finally {
    db.close();
  }
});

test('createFulltextIndexWordMulti enables fts.search wildcard', () => {
  const db = gdb.Database.openMemory();
  try {
    const tx = db.beginWrite();
    tx.createFulltextIndexWordMulti('Article', ['title', 'body']);
    tx.execute("CREATE (:Article {title:'rust', body:'memory safe'})");
    tx.commit();

    const rows = db.query(
      "CALL fts.search('Article', '*', 'memory') YIELD node, score RETURN node.title AS title"
    );
    assert.equal(rows.length, 1);
    assert.equal(rows[0].title, 'rust');
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

test('createCompositeIndex / dropCompositeIndex round-trip', () => {
  const db = gdb.Database.openMemory();
  try {
    const tx = db.beginWrite();
    tx.createCompositeIndex('Person', ['tenant_id', 'ext_id']);
    assert.throws(
      () => tx.createCompositeIndex('Person', ['tenant_id', 'ext_id']),
      /already exists|index/i,
      'duplicate createCompositeIndex should error'
    );
    tx.commit();

    const rows = db.query("CALL db.indexes() YIELD label, property, kind RETURN label, property, kind");
    const props = rows.map(r => r.property).sort();
    assert.deepEqual(props, ['ext_id', 'tenant_id']);

    const tx2 = db.beginWrite();
    tx2.dropCompositeIndex('Person', ['tenant_id', 'ext_id']);
    tx2.commit();

    const rows2 = db.query("CALL db.indexes() YIELD label, property, kind RETURN label, property, kind");
    assert.equal(rows2.length, 0);
  } finally {
    db.close();
  }
});
