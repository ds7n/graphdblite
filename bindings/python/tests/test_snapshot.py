"""Tests for ``Database.snapshot_to``."""

from __future__ import annotations

import pytest

import graphdblite


def test_snapshot_to_produces_single_file_with_data(tmp_path):
    src = tmp_path / "src.db"
    db = graphdblite.Database(str(src))
    db.execute("CREATE (:Person {name: 'Alice'}), (:Person {name: 'Bob'})")

    dst = tmp_path / "snap.db"
    db.snapshot_to(str(dst))
    db.close()

    assert dst.exists()
    assert not dst.with_name(dst.name + "-wal").exists()
    assert not dst.with_name(dst.name + "-shm").exists()

    snap = graphdblite.Database(str(dst))
    rows = snap.query("MATCH (n:Person) RETURN n.name AS name")
    assert len(rows) == 2


def test_snapshot_to_rejects_existing_target(tmp_path):
    db = graphdblite.Database.open_memory()
    existing = tmp_path / "exists.db"
    existing.write_bytes(b"")
    with pytest.raises(Exception):
        db.snapshot_to(str(existing))


def test_snapshot_to_rejects_when_tx_active(tmp_path):
    db = graphdblite.Database(str(tmp_path / "src.db"))
    dst = tmp_path / "snap.db"
    with db.begin_write() as tx:
        tx.execute("CREATE (:X)")
        with pytest.raises(Exception):
            db.snapshot_to(str(dst))
        tx.rollback()
