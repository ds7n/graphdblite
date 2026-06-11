"""Tests for composite index Python binding."""
from __future__ import annotations

import graphdblite


def test_create_composite_index(tmp_path) -> None:
    db = graphdblite.Database(str(tmp_path / "test.db"))
    with db.begin_write() as tx:
        tx.create_composite_index("Person", ["tenant_id", "ext_id"])
    rows = db.execute("CALL db.indexes() YIELD label, property, kind")
    assert len(rows) == 2
    assert {r["property"] for r in rows} == {"tenant_id", "ext_id"}
    assert all(r["kind"] == "btree" for r in rows)
    assert all(r["label"] == "Person" for r in rows)


def test_drop_composite_index(tmp_path) -> None:
    db = graphdblite.Database(str(tmp_path / "test.db"))
    with db.begin_write() as tx:
        tx.create_composite_index("Person", ["a", "b"])
        tx.drop_composite_index("Person", ["a", "b"])
    rows = db.execute("CALL db.indexes() YIELD label, property, kind")
    assert rows == []


def test_composite_index_accelerates_query(tmp_path) -> None:
    db = graphdblite.Database(str(tmp_path / "test.db"))
    db.execute("CREATE (:Person {tenant_id: 1, ext_id: 42, name: 'Alice'})")
    db.execute("CREATE (:Person {tenant_id: 1, ext_id: 99, name: 'Bob'})")
    with db.begin_write() as tx:
        tx.create_composite_index("Person", ["tenant_id", "ext_id"])
    rows = db.execute(
        "MATCH (n:Person) WHERE n.tenant_id = 1 AND n.ext_id = 42 RETURN n.name AS name"
    )
    assert len(rows) == 1
    assert rows[0]["name"] == "Alice"
