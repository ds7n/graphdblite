"""Binding conformance tests — see ``docs/BINDING_CONFORMANCE.md``.

Each test is named ``test_BC_NN_*`` so a failure points at a specific
checklist row. Every first-party binding must pass an equivalent set.
"""

from __future__ import annotations

import pytest

import graphdblite


def _open(tmp_path, name: str = "db.sqlite") -> graphdblite.Database:
    return graphdblite.Database(str(tmp_path / name))


def _person_count(tx_or_db) -> int:
    rows = tx_or_db.query("MATCH (n:Person) RETURN count(n) AS c")
    return int(rows[0]["c"])


# --------------------------------------------------------------------------
# BC-01 — implicit rollback on scope exit without commit
# --------------------------------------------------------------------------
def test_BC_01_explicit_rollback(tmp_path):
    db = _open(tmp_path)
    with db.begin_write() as tx:
        tx.execute("CREATE (n:Person {name: 'Alice'})")
        tx.rollback()
    assert _person_count(db) == 0


# --------------------------------------------------------------------------
# BC-02 — commit persists writes
# --------------------------------------------------------------------------
def test_BC_02_commit_persists(tmp_path):
    db = _open(tmp_path)
    with db.begin_write() as tx:
        tx.execute("CREATE (n:Person {name: 'Alice'})")
        # context manager auto-commits on clean exit
    assert _person_count(db) == 1


# --------------------------------------------------------------------------
# BC-03 — exception mid-transaction discards writes; next op still works
# --------------------------------------------------------------------------
def test_BC_03_exception_rolls_back(tmp_path):
    db = _open(tmp_path)
    with pytest.raises(RuntimeError, match="boom"):
        with db.begin_write() as tx:
            tx.execute("CREATE (n:Person {name: 'Alice'})")
            raise RuntimeError("boom")
    assert _person_count(db) == 0
    # Database is still usable after the exception
    with db.begin_write() as tx:
        tx.execute("CREATE (n:Person {name: 'Bob'})")
    assert _person_count(db) == 1


# --------------------------------------------------------------------------
# BC-04 — nested begin returns an error
# --------------------------------------------------------------------------
def test_BC_04_nested_begin_rejected(tmp_path):
    db = _open(tmp_path)
    with db.begin_write() as tx:
        with pytest.raises(Exception):
            db.begin_write()
        # First tx remains usable
        tx.execute("CREATE (n:Person {name: 'Alice'})")
    assert _person_count(db) == 1


# --------------------------------------------------------------------------
# BC-05 — write inside a read transaction returns an error
# --------------------------------------------------------------------------
def test_BC_05_write_in_read_tx_rejected(tmp_path):
    db = _open(tmp_path)
    with db.begin_read() as tx:
        with pytest.raises(Exception):
            tx.query("CREATE (n:Person {name: 'Alice'})")
    assert _person_count(db) == 0


# --------------------------------------------------------------------------
# BC-06 — commit/rollback without an active transaction returns an error
# --------------------------------------------------------------------------
def test_BC_06_commit_without_tx(tmp_path):
    """The binding's PyDatabase has no top-level commit/rollback — the only
    exposed surface is via the transaction context manager. Test the
    transactional analogue: double-commit on the same tx errors."""
    db = _open(tmp_path)
    with db.begin_write() as tx:
        tx.execute("CREATE (n:Person {name: 'Alice'})")
        tx.commit()
        with pytest.raises(Exception):
            tx.commit()


# --------------------------------------------------------------------------
# BC-07 — operations on a finished transaction return an error
# --------------------------------------------------------------------------
def test_BC_07_use_after_commit(tmp_path):
    db = _open(tmp_path)
    tx = db.begin_write()
    tx.execute("CREATE (n:Person {name: 'Alice'})")
    tx.commit()
    with pytest.raises(Exception):
        tx.execute("CREATE (n:Person {name: 'Bob'})")
    with pytest.raises(Exception):
        tx.rollback()


def test_BC_07_use_after_rollback(tmp_path):
    db = _open(tmp_path)
    tx = db.begin_write()
    tx.execute("CREATE (n:Person {name: 'Alice'})")
    tx.rollback()
    with pytest.raises(Exception):
        tx.execute("CREATE (n:Person {name: 'Bob'})")


# --------------------------------------------------------------------------
# BC-08 — concurrent reader sees pre-txn snapshot until writer commits
# --------------------------------------------------------------------------
def test_BC_08_reader_snapshot(tmp_path):
    path = str(tmp_path / "shared.sqlite")
    writer = graphdblite.Database(path)
    # Seed one row outside any explicit txn so the file exists
    writer.execute("CREATE (n:Person {name: 'Seed'})")

    reader = graphdblite.Database(path)
    assert _person_count(reader) == 1

    w_tx = writer.begin_write()
    w_tx.execute("CREATE (n:Person {name: 'Alice'})")

    # Reader is on a separate handle — must still see only the seed row.
    assert _person_count(reader) == 1

    w_tx.commit()
    assert _person_count(reader) == 2


# --------------------------------------------------------------------------
# BC-09 — closing the database with an open txn does not corrupt the file
# --------------------------------------------------------------------------
def test_BC_09_close_with_open_tx(tmp_path):
    path = str(tmp_path / "drop.sqlite")
    db = graphdblite.Database(path)
    tx = db.begin_write()
    tx.execute("CREATE (n:Person {name: 'Lost'})")
    # Drop the database without committing. PyDatabase.close drops the inner.
    del tx
    db.close()
    del db

    # Reopen and confirm uncommitted write was rolled back.
    db2 = graphdblite.Database(path)
    assert _person_count(db2) == 0
    # And the file is still healthy enough to write into.
    with db2.begin_write() as tx2:
        tx2.execute("CREATE (n:Person {name: 'Recovered'})")
    assert _person_count(db2) == 1
