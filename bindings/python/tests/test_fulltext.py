"""Smoke test: fulltext index via Python binding."""

from __future__ import annotations

import graphdblite


def test_fulltext_index_smoke(tmp_path):
    db = graphdblite.Database(str(tmp_path / "test.db"))

    with db.begin_write() as tx:
        tx.execute("CREATE (:Doc {body: 'hello world'})")
        tx.execute("CREATE (:Doc {body: 'goodbye now'})")
        tx.create_fulltext_index("Doc", "body")
        tx.commit()

    rows = db.query("MATCH (n:Doc) WHERE n.body CONTAINS 'hello' RETURN n.body AS b")
    assert len(rows) == 1
    assert rows[0]["b"] == "hello world"

    with db.begin_write() as tx:
        tx.drop_fulltext_index("Doc", "body")
        tx.commit()

    db.close()


def test_fulltext_index_starts_with(tmp_path):
    db = graphdblite.Database(str(tmp_path / "test.db"))

    with db.begin_write() as tx:
        tx.execute("CREATE (:Item {name: 'apple pie'})")
        tx.execute("CREATE (:Item {name: 'banana split'})")
        tx.create_fulltext_index("Item", "name")
        tx.commit()

    rows = db.query("MATCH (n:Item) WHERE n.name STARTS WITH 'apple' RETURN n.name AS n")
    assert len(rows) == 1
    assert rows[0]["n"] == "apple pie"

    db.close()


def test_fulltext_index_ends_with(tmp_path):
    db = graphdblite.Database(str(tmp_path / "test.db"))

    with db.begin_write() as tx:
        tx.execute("CREATE (:Item {name: 'apple pie'})")
        tx.execute("CREATE (:Item {name: 'banana split'})")
        tx.create_fulltext_index("Item", "name")
        tx.commit()

    rows = db.query("MATCH (n:Item) WHERE n.name ENDS WITH 'split' RETURN n.name AS n")
    assert len(rows) == 1
    assert rows[0]["n"] == "banana split"

    db.close()
