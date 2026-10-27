"""Tests for the 2026-10 review fixes in the Python binding.

Transactions sharing the database (dropped tx, close with an open tx),
statement atomicity, error classes, path / relationship shapes, PathLike
paths, `:memory:`, `__version__`, GIL release, and panic containment.
"""

from __future__ import annotations

import gc
import importlib.metadata
import os
import pathlib
import re
import threading
import time

import pytest

import graphdblite

FAILS_AFTER_WRITING = "UNWIND [1, 'a'] AS i CREATE (:M {v: 1}) RETURN i - 1 AS x"


def _count(db, label: str) -> int:
    return db.query(f"MATCH (n:{label}) RETURN count(n) AS c")[0]["c"]


# --------------------------------------------------------------------------
# F1: a transaction dropped without commit/rollback rolls back and releases
# the database.
# --------------------------------------------------------------------------
def test_dropped_write_tx_rolls_back_and_releases_db(tmp_path):
    db = graphdblite.Database(tmp_path / "a.db")
    tx = db.begin_write()
    tx.execute("CREATE (:Z)")
    del tx
    gc.collect()
    assert _count(db, "Z") == 0
    # A new transaction can begin.
    with db.begin_write() as tx2:
        tx2.execute("CREATE (:Z)")
    assert _count(db, "Z") == 1


def test_dropped_read_tx_releases_db(tmp_path):
    db = graphdblite.Database(tmp_path / "a.db")
    tx = db.begin_read()
    tx.query("RETURN 1 AS x")
    del tx
    gc.collect()
    db.execute("CREATE (:Z)")
    assert _count(db, "Z") == 1


# --------------------------------------------------------------------------
# F2 / BC-09: close() with an open transaction rolls it back; the
# transaction object then raises the closed error and nothing persists.
# --------------------------------------------------------------------------
def test_close_with_open_tx_rolls_back(tmp_path):
    path = tmp_path / "b.db"
    db = graphdblite.Database(path)
    tx = db.begin_write()
    tx.execute("CREATE (:Q)")
    db.close()
    with pytest.raises(RuntimeError, match=r"^database is closed$"):
        tx.commit()
    with pytest.raises(RuntimeError, match=r"^transaction is already finished$"):
        tx.execute("CREATE (:Q)")
    assert repr(db) == "Database(<closed>)"
    with pytest.raises(RuntimeError, match=r"^database is closed$"):
        db.query("RETURN 1 AS x")
    reopened = graphdblite.Database(path)
    assert _count(reopened, "Q") == 0


# --------------------------------------------------------------------------
# D3 / BC-13: a failed statement is rolled back alone; the transaction
# stays usable.
# --------------------------------------------------------------------------
def test_failed_statement_in_tx_is_rolled_back_alone():
    db = graphdblite.Database.open_memory()
    with db.begin_write() as tx:
        tx.execute("CREATE (:K)")
        with pytest.raises(graphdblite.CypherTypeError, match=r"^TypeError\(InvalidArgumentType\)"):
            tx.execute(FAILS_AFTER_WRITING)
        tx.execute("CREATE (:K2)")
    assert (_count(db, "K"), _count(db, "K2"), _count(db, "M")) == (1, 1, 0)


# --------------------------------------------------------------------------
# F13: error classes name the Cypher error kind; ParseError still catches
# every query error.
# --------------------------------------------------------------------------
@pytest.mark.parametrize(
    ("query", "cls", "prefix"),
    [
        ("RETURN 1 +", graphdblite.CypherSyntaxError, "SyntaxError("),
        ("RETURN missing AS x", graphdblite.CypherSyntaxError, "SyntaxError(UndefinedVariable)"),
        ("RETURN 1 - 'a' AS x", graphdblite.CypherTypeError, "TypeError(InvalidArgumentType)"),
        ("RETURN $p AS x", graphdblite.CypherArgumentError, "ArgumentError("),
        ("CALL no.such.proc()", graphdblite.ProcedureError, "ProcedureError("),
    ],
)
def test_query_error_classes(query, cls, prefix):
    db = graphdblite.Database.open_memory()
    with pytest.raises(cls) as info:
        db.query(query, {} if "$" in query else None)
    assert str(info.value).startswith(prefix)
    assert isinstance(info.value, graphdblite.ParseError)
    assert graphdblite.QueryError is graphdblite.ParseError


def test_transaction_errors_are_transaction_error():
    db = graphdblite.Database.open_memory()
    with db.begin_read() as tx:
        with pytest.raises(
            graphdblite.TransactionError,
            match=r"^transaction error: write operations are not permitted inside a read transaction",
        ):
            tx.query("CREATE (:A)")
    assert not issubclass(graphdblite.TransactionError, graphdblite.ParseError)


# --------------------------------------------------------------------------
# D1 / F14: path and relationship shapes.
# --------------------------------------------------------------------------
def test_path_is_nodes_and_relationships():
    db = graphdblite.Database.open_memory()
    db.execute("CREATE (:A {k: 1})-[:R {w: 2}]->(:B)")
    (row,) = db.query("MATCH p = (a:A)-[r]->(b:B) RETURN p, id(a) AS a, id(b) AS b")
    p = row["p"]
    assert set(p) == {"nodes", "relationships"}
    assert [n["__labels"] for n in p["nodes"]] == [["A"], ["B"]]
    assert [n["__id"] for n in p["nodes"]] == [row["a"], row["b"]]
    assert p["nodes"][0]["k"] == 1
    assert p["relationships"] == [
        {
            "__src": row["a"],
            "__dst": row["b"],
            "__type": "R",
            "__label": "R",
            "__edge_seq": 0,
            "w": 2,
        }
    ]


def test_reserved_property_names_rejected():
    db = graphdblite.Database.open_memory()
    with pytest.raises(
        graphdblite.CypherSemanticError,
        match=r"property names starting with `__` are reserved for graphdblite metadata",
    ):
        db.execute("CREATE (:A {__id: 99})")


# --------------------------------------------------------------------------
# F13: os.PathLike paths; F12: ":memory:" is in-memory.
# --------------------------------------------------------------------------
def test_pathlike_paths(tmp_path):
    db = graphdblite.Database(pathlib.Path(tmp_path) / "pl.db")
    db.execute("CREATE (:A)")
    db.snapshot_to(pathlib.Path(tmp_path) / "snap.db")
    assert _count(graphdblite.Database(tmp_path / "snap.db"), "A") == 1
    assert repr(db) == f"Database('{os.path.realpath(tmp_path / 'pl.db')}')"


def test_memory_path_is_in_memory(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    a = graphdblite.Database(":memory:")
    a.execute("CREATE (:X)")
    b = graphdblite.Database(":memory:")
    assert os.listdir(tmp_path) == []
    assert (_count(a, "X"), _count(b, "X")) == (1, 0)
    assert repr(a) == "Database(':memory:')"


# --------------------------------------------------------------------------
# F13: __version__ comes from the package metadata.
# --------------------------------------------------------------------------
def test_version_matches_package_metadata():
    cargo = pathlib.Path(__file__).resolve().parents[3] / "Cargo.toml"
    m = re.search(r'^\[workspace\.package\]\s*\nversion = "([^"]+)"', cargo.read_text(), re.M)
    assert m is not None
    assert graphdblite.__version__ == m.group(1)
    assert graphdblite.__version__ == importlib.metadata.version("graphdblite")


# --------------------------------------------------------------------------
# F13: transaction queries release the GIL, so other Python threads run.
# --------------------------------------------------------------------------
def test_tx_query_releases_gil():
    db = graphdblite.Database.open_memory()
    ticks = 0
    stop = threading.Event()

    def spin():
        nonlocal ticks
        while not stop.is_set():
            ticks += 1
            time.sleep(0)

    t = threading.Thread(target=spin)
    t.start()
    try:
        with db.begin_read() as tx:
            before = ticks
            tx.query("UNWIND range(1, 3000000) AS i RETURN count(i) AS c")
            during = ticks - before
    finally:
        stop.set()
        t.join()
    assert during > 100, f"background thread advanced only {during} ticks during the query"


# --------------------------------------------------------------------------
# F16: a Rust panic surfaces as GraphDBError and the handle stays usable.
# Needs the core's test hook: build with `--features test-hooks`.
# --------------------------------------------------------------------------
PANIC_QUERY = "CALL graphdblite.testPanic()"


def _hooks_enabled() -> bool:
    try:
        graphdblite.Database.open_memory().query(PANIC_QUERY)
    except graphdblite.GraphDBError as e:
        return str(e).startswith("internal panic")
    return False


needs_hooks = pytest.mark.skipif(
    not _hooks_enabled(), reason="built without the test-hooks feature"
)


@needs_hooks
def test_panic_surfaces_as_graphdb_error():
    db = graphdblite.Database.open_memory()
    with pytest.raises(graphdblite.GraphDBError) as info:
        db.query(PANIC_QUERY)
    assert type(info.value) is graphdblite.GraphDBError
    assert str(info.value) == "internal panic: graphdblite test hook: deliberate panic"
    db.execute("CREATE (:A)")
    assert _count(db, "A") == 1


@needs_hooks
def test_panic_inside_tx_rolls_back_and_ends_tx():
    db = graphdblite.Database.open_memory()
    tx = db.begin_write()
    tx.execute("CREATE (:A)")
    with pytest.raises(graphdblite.GraphDBError, match=r"^internal panic: "):
        tx.execute(PANIC_QUERY)
    with pytest.raises(RuntimeError, match=r"^transaction is already finished$"):
        tx.execute("CREATE (:B)")
    assert _count(db, "A") == 0
    with db.begin_write() as tx2:
        tx2.execute("CREATE (:B)")
    assert _count(db, "B") == 1


# --------------------------------------------------------------------------
# Parameter nesting is bounded (a deeper structure overflowed the native
# stack in the recursive conversion, which is a crash, not an exception).
# --------------------------------------------------------------------------


def _nested_list(depth: int):
    v = 1
    for _ in range(depth):
        v = [v]
    return v


def test_deeply_nested_parameter_is_rejected_not_a_crash():
    db = graphdblite.Database.open_memory()
    msg = r"^value nesting exceeds the maximum depth of 128$"
    with pytest.raises(ValueError, match=msg):
        db.query("RETURN 1 AS x", {"p": _nested_list(10000)})
    with pytest.raises(ValueError, match=msg):
        db.query("RETURN 1 AS x", {"p": _nested_list(129)})
    # Boundary: 128 levels still convert.
    assert len(db.query("RETURN $p AS x", {"p": _nested_list(128)})) == 1
    # A self-referencing list is the same class of input.
    cyclic: list = []
    cyclic.append(cyclic)
    with pytest.raises(ValueError, match=msg):
        db.query("RETURN 1 AS x", {"p": cyclic})
    d: dict = {}
    d["a"] = d
    with pytest.raises(ValueError, match=msg):
        db.query("RETURN 1 AS x", {"p": d})
