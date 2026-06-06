"""Smoke test for multi-property word fulltext indexes."""
from __future__ import annotations

import graphdblite


def test_multi_prop_search_wildcard_and_column_scope(tmp_path) -> None:
    db = graphdblite.Database(str(tmp_path / "multi.db"))
    db.execute(
        "CREATE (:Article {title:'rust systems', body:'memory safe'}), "
        "(:Article {title:'python notes', body:'data science'})"
    )
    with db.begin_write() as tx:
        tx.create_fulltext_index_word_multi("Article", ["title", "body"])

    # Wildcard finds match in either column.
    rows = db.execute(
        "CALL fts.search('Article', '*', 'memory') YIELD node "
        "RETURN node.title AS title"
    )
    assert [r["title"] for r in rows] == ["rust systems"]

    # Column-scoped isolates.
    rows = db.execute(
        "CALL fts.search('Article', 'title', 'memory') YIELD node "
        "RETURN node.title AS title"
    )
    assert rows == []
