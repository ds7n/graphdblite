# SPDX-License-Identifier: MIT
# Copyright (c) 2026 ds7n

"""graphdblite — Embedded graph database with Cypher support."""

from importlib.metadata import PackageNotFoundError, version

from graphdblite._graphdblite import (
    ConstraintViolationError,
    CypherArgumentError,
    CypherArithmeticError,
    CypherSemanticError,
    CypherSyntaxError,
    CypherTypeError,
    Database,
    EntityNotFoundError,
    GraphDBError,
    NodeNotFoundError,
    ParseError,
    ProcedureError,
    ReadTransaction,
    StorageError,
    TransactionError,
    WriteTransaction,
)

# Base class of every Cypher query error. `ParseError` is the historical
# name (0.1.x raised it for all query errors) and stays the same class.
QueryError = ParseError

__all__ = [
    "ConstraintViolationError",
    "CypherArgumentError",
    "CypherArithmeticError",
    "CypherSemanticError",
    "CypherSyntaxError",
    "CypherTypeError",
    "Database",
    "EntityNotFoundError",
    "GraphDBError",
    "NodeNotFoundError",
    "ParseError",
    "ProcedureError",
    "QueryError",
    "ReadTransaction",
    "StorageError",
    "TransactionError",
    "WriteTransaction",
]

try:
    __version__ = version("graphdblite")
except PackageNotFoundError:  # pragma: no cover - running from a source tree
    __version__ = "0+unknown"
