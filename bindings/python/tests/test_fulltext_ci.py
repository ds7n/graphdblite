"""Smoke test: case-insensitive fulltext index via Python binding."""

from __future__ import annotations

import graphdblite


def test_fulltext_ci_index_matches_across_case(tmp_path):
    db = graphdblite.Database(str(tmp_path / "test.db"))

    with db.begin_write() as tx:
        tx.execute("CREATE (:Person {name: 'Alice Smith'})")
        tx.execute("CREATE (:Person {name: 'Bob Jones'})")
        tx.create_fulltext_index_ci("Person", "name")
        tx.commit()

    rows = db.query(
        "MATCH (n:Person) WHERE n.name CONTAINS 'alice' RETURN n.name AS name"
    )
    assert [r["name"] for r in rows] == ["Alice Smith"]

    db.close()


def test_fulltext_ci_index_tolower_idiom(tmp_path):
    db = graphdblite.Database(str(tmp_path / "test.db"))

    with db.begin_write() as tx:
        tx.execute("CREATE (:Person {name: 'Alice Smith'})")
        tx.create_fulltext_index_ci("Person", "name")
        tx.commit()

    rows = db.query(
        "MATCH (n:Person) WHERE toLower(n.name) CONTAINS toLower('ALICE') "
        "RETURN n.name AS name"
    )
    assert [r["name"] for r in rows] == ["Alice Smith"]

    db.close()
