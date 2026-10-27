"""Regression: opening a database by bare file name (no directory part)."""

from __future__ import annotations

import os

import graphdblite


def test_open_bare_relative_filename(tmp_path):
    old = os.getcwd()
    os.chdir(tmp_path)
    try:
        db = graphdblite.Database("bare.db")
        db.execute("CREATE (:X {v: 1})")
        assert db.query("MATCH (x:X) RETURN x.v AS v") == [{"v": 1}]
        db.close()
        assert (tmp_path / "bare.db").exists()
    finally:
        os.chdir(old)
