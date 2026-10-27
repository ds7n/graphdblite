"""shortestPath through the Python binding (GitHub issue #2)."""
from __future__ import annotations

import graphdblite


def _mixed_type_db(tmp_path) -> graphdblite.Database:
    db = graphdblite.Database(str(tmp_path / "test.db"))
    db.execute(
        "CREATE (a:P {name: 'A'})-[:KNOWS]->(b:P {name: 'B'})"
        "-[:WORKS_WITH]->(c:P {name: 'C'}), "
        "(d:P {name: 'D'}), (a)-[:LIKES]->(e:P {name: 'E'})"
    )
    return db


def test_untyped_shortest_path_unbound_endpoints(tmp_path) -> None:
    db = _mixed_type_db(tmp_path)
    rows = db.execute(
        "MATCH p = shortestPath((a)-[*]->(b)) WHERE p IS NOT NULL "
        "RETURN a.name AS src, b.name AS dst, length(p) AS len, "
        "[r IN relationships(p) | type(r)] AS types"
    )
    got = sorted((r["src"], r["dst"], r["len"], r["types"]) for r in rows)
    assert got == [
        ("A", "B", 1, ["KNOWS"]),
        ("A", "C", 2, ["KNOWS", "WORKS_WITH"]),
        ("A", "E", 1, ["LIKES"]),
        ("B", "C", 1, ["WORKS_WITH"]),
    ]


def test_unreachable_pair_has_no_row(tmp_path) -> None:
    db = _mixed_type_db(tmp_path)
    rows = db.execute(
        "MATCH (c:P {name: 'C'}), (a:P {name: 'A'}), "
        "p = shortestPath((c)-[*]->(a)) RETURN p"
    )
    assert rows == []
