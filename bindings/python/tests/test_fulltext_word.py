"""Smoke test for word-tokenized fulltext indexes + fts.search procedure."""

from __future__ import annotations

import graphdblite


def test_fts_search_returns_node_and_score(tmp_path) -> None:
    db = graphdblite.Database(str(tmp_path / "word.db"))
    db.execute(
        "CREATE (:Person {name:'Alice',bio:'rust systems programming'}), "
        "(:Person {name:'Bob',bio:'python data science'})"
    )
    with db.begin_write() as tx:
        tx.create_fulltext_index_word("Person", "bio")

    rows = db.execute(
        "CALL fts.search('Person', 'bio', 'rust') YIELD node, score "
        "RETURN node.name AS name, score ORDER BY score DESC"
    )
    assert [r["name"] for r in rows] == ["Alice"]
    assert rows[0]["score"] > 0.0
