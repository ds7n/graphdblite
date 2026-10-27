// SPDX-License-Identifier: MIT
// Copyright (c) 2026 ds7n

#![allow(unexpected_cfgs)]

use std::path::PathBuf;

use std::collections::HashMap;

use pyo3::create_exception;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyFloat, PyList, PyString, PyTuple};
use pyo3::{Py, PyAny};

/// `PyObject` was removed as a public alias in pyo3 0.28; restore locally
/// to keep the binding's call-site signatures unchanged.
type PyObject = Py<PyAny>;

use graphdblite::{Config, Database as RustDatabase, GraphError, QueryError, Record, Value};

// --- Exception hierarchy ---

create_exception!(_graphdblite, GraphDBError, pyo3::exceptions::PyException);
// Every Cypher query error derives from ParseError: 0.1.x raised ParseError
// for all of them, so `except ParseError` keeps working. The subclasses name
// the actual kind (openCypher error categories).
create_exception!(_graphdblite, ParseError, GraphDBError);
create_exception!(_graphdblite, CypherSyntaxError, ParseError);
create_exception!(_graphdblite, CypherSemanticError, ParseError);
create_exception!(_graphdblite, CypherTypeError, ParseError);
create_exception!(_graphdblite, CypherArgumentError, ParseError);
create_exception!(_graphdblite, CypherArithmeticError, ParseError);
create_exception!(_graphdblite, EntityNotFoundError, ParseError);
create_exception!(_graphdblite, ConstraintViolationError, ParseError);
create_exception!(_graphdblite, ProcedureError, ParseError);
create_exception!(_graphdblite, StorageError, GraphDBError);
create_exception!(_graphdblite, NodeNotFoundError, GraphDBError);
create_exception!(_graphdblite, TransactionError, GraphDBError);

/// Map a GraphError to the appropriate Python exception.
fn to_py_err(e: GraphError) -> PyErr {
    let msg = e.to_string();
    match &e {
        GraphError::Query(q) => match q {
            QueryError::SyntaxError { .. } => CypherSyntaxError::new_err(msg),
            QueryError::SemanticError { .. } => CypherSemanticError::new_err(msg),
            QueryError::TypeError { .. } => CypherTypeError::new_err(msg),
            QueryError::ArgumentError { .. } => CypherArgumentError::new_err(msg),
            QueryError::ArithmeticError { .. } => CypherArithmeticError::new_err(msg),
            QueryError::EntityNotFound { .. } => EntityNotFoundError::new_err(msg),
            QueryError::ConstraintViolation { .. } => ConstraintViolationError::new_err(msg),
            QueryError::ProcedureError { .. } => ProcedureError::new_err(msg),
            // `QueryError` is non-exhaustive: a future kind is still a
            // query error.
            _ => ParseError::new_err(msg),
        },
        GraphError::Storage { .. } => StorageError::new_err(msg),
        GraphError::NodeNotFound { .. } => NodeNotFoundError::new_err(msg),
        GraphError::Transaction { .. } => TransactionError::new_err(msg),
        _ => GraphDBError::new_err(msg),
    }
}

fn closed_err() -> PyErr {
    PyRuntimeError::new_err("database is closed")
}

fn finished_err() -> PyErr {
    PyRuntimeError::new_err("transaction is already finished")
}

/// Run a core call, turning a Rust panic into `GraphDBError`.
///
/// pyo3 would otherwise raise `pyo3_runtime.PanicException`, which derives
/// from `BaseException` and so escapes `except Exception` handlers.
fn catch_panic<T>(op: impl FnOnce() -> Result<T, GraphError>) -> PyResult<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(op)) {
        Ok(result) => result.map_err(to_py_err),
        Err(payload) => {
            let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic".to_string()
            };
            Err(GraphDBError::new_err(format!("internal panic: {msg}")))
        }
    }
}

/// [`catch_panic`] for a call on an open database. A panic can leave the
/// transaction it interrupted open, so it is rolled back before the error
/// is raised: the database stays usable for the next call.
fn call_core<T>(
    db: &mut RustDatabase,
    op: impl FnOnce(&mut RustDatabase) -> Result<T, GraphError>,
) -> PyResult<T> {
    let mut panicked = true;
    let result = catch_panic(|| {
        let r = op(db);
        panicked = false;
        r
    });
    if panicked {
        // No transaction open is fine: there is nothing to undo.
        let _ = db.rollback();
    }
    result
}

/// Metadata keys of a node dict. A user property with one of these names
/// is left out: metadata wins.
const NODE_META_KEYS: [&str; 2] = ["__id", "__labels"];

/// Metadata keys of a relationship dict. `__label` is a deprecated alias of
/// `__type`, kept for 0.1.x callers.
const EDGE_META_KEYS: [&str; 5] = ["__src", "__dst", "__type", "__label", "__edge_seq"];

/// Copy user properties into `dict`, skipping metadata keys.
fn set_props(
    py: Python,
    dict: &Bound<'_, PyDict>,
    props: &graphdblite::Properties,
    meta: &[&str],
) -> PyResult<()> {
    for (k, v) in props {
        if meta.contains(&k.as_str()) {
            continue;
        }
        dict.set_item(k, value_to_py(py, v)?)?;
    }
    Ok(())
}

fn node_to_py(py: Python, n: &graphdblite::Node) -> PyResult<PyObject> {
    let dict = PyDict::new(py);
    dict.set_item("__id", n.id.0)?;
    dict.set_item("__labels", &n.labels)?;
    set_props(py, &dict, &n.properties, &NODE_META_KEYS)?;
    Ok(dict.into_any().unbind())
}

fn edge_to_py(py: Python, e: &graphdblite::Edge) -> PyResult<PyObject> {
    let dict = PyDict::new(py);
    dict.set_item("__src", e.src.0)?;
    dict.set_item("__dst", e.dst.0)?;
    dict.set_item("__type", &e.label)?;
    dict.set_item("__label", &e.label)?;
    dict.set_item("__edge_seq", e.seq)?;
    set_props(py, &dict, &e.properties, &EDGE_META_KEYS)?;
    Ok(dict.into_any().unbind())
}

/// Convert a graphdblite Value to a Python object.
fn value_to_py(py: Python, val: &Value) -> PyResult<PyObject> {
    Ok(match val {
        Value::Null => py.None(),
        Value::Bool(b) => b.into_pyobject(py)?.to_owned().into_any().unbind(),
        Value::I64(n) => n.into_pyobject(py)?.into_any().unbind(),
        Value::F64(n) => n.into_pyobject(py)?.into_any().unbind(),
        Value::String(s) => s.as_str().into_pyobject(py)?.into_any().unbind(),
        Value::List(items) => {
            let py_items: Vec<PyObject> = items
                .iter()
                .map(|v| value_to_py(py, v))
                .collect::<PyResult<_>>()?;
            py_items.into_pyobject(py)?.into_any().unbind()
        }
        Value::Path(p) => {
            let nodes: Vec<PyObject> = p
                .nodes
                .iter()
                .map(|n| node_to_py(py, n))
                .collect::<PyResult<_>>()?;
            let rels: Vec<PyObject> = p
                .edges
                .iter()
                .map(|e| edge_to_py(py, e))
                .collect::<PyResult<_>>()?;
            let dict = PyDict::new(py);
            dict.set_item("nodes", nodes)?;
            dict.set_item("relationships", rels)?;
            dict.into_any().unbind()
        }
        Value::Node(n) => node_to_py(py, n)?,
        Value::Edge(e) => edge_to_py(py, e)?,
        Value::Map(map) => {
            let dict = PyDict::new(py);
            for (k, v) in map {
                dict.set_item(k, value_to_py(py, v)?)?;
            }
            dict.into_any().unbind()
        }
        // Temporal types: exposed as ISO strings.
        other => format!("{other}").into_pyobject(py)?.into_any().unbind(),
    })
}

fn records_to_py(py: Python, records: &[Record]) -> PyResult<Vec<PyObject>> {
    let mut result = Vec::new();
    for rec in records {
        let dict = PyDict::new(py);
        for (key, val) in rec.iter() {
            dict.set_item(key, value_to_py(py, val)?)?;
        }
        result.push(dict.into_any().unbind());
    }
    Ok(result)
}

/// Deepest list/dict nesting accepted in a query parameter or property
/// value. A deeper (or self-referencing) structure would overflow the stack
/// in the recursive conversion, which no `catch_unwind` can contain.
const MAX_VALUE_DEPTH: usize = 128;

/// Convert a Python object to a graphdblite Value.
fn py_to_value(obj: &Bound<'_, pyo3::types::PyAny>) -> PyResult<Value> {
    py_to_value_at(obj, 0)
}

/// [`py_to_value`] with the current list/dict nesting depth.
fn py_to_value_at(obj: &Bound<'_, pyo3::types::PyAny>, depth: usize) -> PyResult<Value> {
    if depth > MAX_VALUE_DEPTH {
        return Err(PyValueError::new_err(format!(
            "value nesting exceeds the maximum depth of {MAX_VALUE_DEPTH}"
        )));
    }
    if obj.is_none() {
        Ok(Value::Null)
    } else if obj.cast::<PyBool>().is_ok() {
        // Check bool before i64 — Python bool is a subclass of int.
        Ok(Value::Bool(obj.extract::<bool>()?))
    } else if let Ok(n) = obj.extract::<i64>() {
        Ok(Value::I64(n))
    } else if obj.cast::<PyFloat>().is_ok() {
        Ok(Value::F64(obj.extract::<f64>()?))
    } else if obj.cast::<PyString>().is_ok() {
        Ok(Value::String(obj.extract::<String>()?))
    } else if let Ok(list) = obj.cast::<PyList>() {
        let items: PyResult<Vec<Value>> = list
            .iter()
            .map(|item| py_to_value_at(&item, depth + 1))
            .collect();
        Ok(Value::List(items?))
    } else if let Ok(dict) = obj.cast::<PyDict>() {
        let mut map = std::collections::BTreeMap::new();
        for (key, value) in dict.iter() {
            let k: String = key.extract()?;
            let v = py_to_value_at(&value, depth + 1)?;
            map.insert(k, v);
        }
        Ok(Value::Map(map))
    } else {
        Err(PyValueError::new_err(format!(
            "unsupported property type: {}",
            obj.get_type().name()?
        )))
    }
}

/// Reject labels/edge types that are not valid Cypher `symbolic_name`s.
/// Used by the batch helpers since they inject the name directly into a
/// generated Cypher string and cannot rely on the parser's identifier rules.
fn validate_symbolic_name(name: &str, kind: &str) -> PyResult<()> {
    let mut chars = name.chars();
    let first_ok = chars
        .next()
        .map(|c| c == '_' || c.is_ascii_alphabetic())
        .unwrap_or(false);
    let rest_ok = chars.all(|c| c == '_' || c.is_ascii_alphanumeric());
    if !first_ok || !rest_ok {
        return Err(PyValueError::new_err(format!(
            "{kind} {name:?} must match [A-Za-z_][A-Za-z0-9_]* for batch operations"
        )));
    }
    Ok(())
}

/// Convert a Python dict to a HashMap<String, Value> for query parameters.
fn py_dict_to_value_map(dict: &Bound<'_, PyDict>) -> PyResult<HashMap<String, Value>> {
    let mut map = HashMap::new();
    for (key, value) in dict.iter() {
        let key: String = key.extract()?;
        let val = py_to_value(&value)?;
        map.insert(key, val);
    }
    Ok(map)
}

/// Resolve a user-supplied path: `:memory:` stays as is (an in-memory
/// database); anything else is canonicalized to prevent directory
/// traversal surprises.
fn resolve_path(path: &std::path::Path) -> PyResult<PathBuf> {
    if path.as_os_str() == ":memory:" {
        return Ok(path.to_path_buf());
    }
    let shown = path.display();
    if path.exists() {
        return path
            .canonicalize()
            .map_err(|e| PyValueError::new_err(format!("invalid path '{shown}': {e}")));
    }
    // A bare file name ("my.db") has an empty parent; treat it as the
    // current directory (canonicalizing "" fails).
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => std::path::Path::new("."),
    };
    let parent = parent
        .canonicalize()
        .map_err(|e| PyValueError::new_err(format!("invalid path '{shown}': {e}")))?;
    let name = path
        .file_name()
        .ok_or_else(|| PyValueError::new_err(format!("invalid path '{shown}': no filename")))?;
    Ok(parent.join(name))
}

/// Python wrapper for the graphdblite Database.
///
/// `unsendable` is required in pyo3 0.28+: `RustDatabase` wraps a
/// `rusqlite::Connection` (`RefCell` internally), which is `!Send`/`!Sync`.
/// The binding already serializes access through `&mut self`; this attribute
/// also enforces Python-side single-threaded use.
///
/// Transactions share this object's database (they hold a reference to it)
/// instead of taking it over, so a transaction that is dropped, or a
/// `close()` while one is open, cannot strand the database.
#[pyclass(name = "Database", unsendable)]
pub struct PyDatabase {
    inner: Option<RustDatabase>,
    path: String,
    /// Id of the transaction object that owns the open transaction, if any.
    active_tx: Option<u64>,
    /// Source of transaction ids.
    next_tx_id: u64,
}

impl PyDatabase {
    fn db_mut(&mut self) -> PyResult<&mut RustDatabase> {
        self.inner.as_mut().ok_or_else(closed_err)
    }

    /// Begin a transaction and register `id`'s owner.
    fn begin(slf: &Bound<'_, Self>, write: bool) -> PyResult<TxHandle> {
        let mut this = slf.borrow_mut();
        let db = this.db_mut()?;
        call_core(db, |db| {
            if write {
                db.begin_write()
            } else {
                db.begin_read()
            }
        })?;
        this.next_tx_id += 1;
        let id = this.next_tx_id;
        this.active_tx = Some(id);
        Ok(TxHandle {
            parent: slf.clone().unbind(),
            id,
            finished: false,
        })
    }
}

#[pymethods]
impl PyDatabase {
    /// Open a database at the given path (`str` or `os.PathLike`).
    ///
    /// The path is canonicalized to prevent directory traversal. The path
    /// `":memory:"` opens a private in-memory database.
    #[new]
    #[pyo3(signature = (path, busy_timeout_ms=5000))]
    fn new(path: PathBuf, busy_timeout_ms: u32) -> PyResult<Self> {
        let resolved = resolve_path(&path)?;
        let config = Config {
            busy_timeout_ms,
            ..Config::default()
        };
        let display_path = resolved.display().to_string();
        let db = catch_panic(|| RustDatabase::open_with_config(&resolved, config))?;
        Ok(Self {
            inner: Some(db),
            path: display_path,
            active_tx: None,
            next_tx_id: 0,
        })
    }

    /// Open an in-memory database (for testing).
    #[staticmethod]
    fn open_memory() -> PyResult<Self> {
        let db = catch_panic(RustDatabase::open_memory)?;
        Ok(Self {
            inner: Some(db),
            path: ":memory:".to_string(),
            active_tx: None,
            next_tx_id: 0,
        })
    }

    /// Execute a read-only Cypher query. Returns a list of dicts.
    ///
    /// Note: holds an exclusive borrow (`&mut self`) — cannot be called
    /// concurrently from multiple Python threads. PyO3 will raise
    /// `RuntimeError` if a second thread attempts to call while one is active.
    #[pyo3(signature = (cypher, params=None))]
    fn query(
        &mut self,
        py: Python,
        cypher: &str,
        params: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Vec<PyObject>> {
        let db = self.db_mut()?;
        let param_map = params.map(py_dict_to_value_map).transpose()?;
        let cypher = cypher.to_string();
        let records = py.detach(|| {
            call_core(db, |db| {
                let tx = db.read_tx()?;
                let r = tx.query_with_params(&cypher, param_map.as_ref())?;
                tx.commit()?;
                Ok(r)
            })
        })?;
        records_to_py(py, &records)
    }

    /// Execute a write Cypher query (CREATE, DELETE, SET, MERGE). Returns a list of dicts.
    ///
    /// Note: holds an exclusive borrow (`&mut self`) — see `query()` docstring.
    #[pyo3(signature = (cypher, params=None))]
    fn execute(
        &mut self,
        py: Python,
        cypher: &str,
        params: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Vec<PyObject>> {
        let db = self.db_mut()?;
        let param_map = params.map(py_dict_to_value_map).transpose()?;
        let cypher = cypher.to_string();
        let records = py.detach(|| {
            call_core(db, |db| {
                let tx = db.write_tx()?;
                let r = tx.query_with_params(&cypher, param_map.as_ref())?;
                tx.commit()?;
                Ok(r)
            })
        })?;
        records_to_py(py, &records)
    }

    /// Begin a read-write transaction. Use as a context manager:
    ///
    /// ```python
    /// with db.begin_write() as tx:
    ///     tx.execute("CREATE (n:Person {name: 'Alice'})")
    ///     tx.execute("CREATE (n:Person {name: 'Bob'})")
    /// # auto-commits on success, rolls back on exception
    /// ```
    fn begin_write(slf: &Bound<'_, Self>) -> PyResult<PyWriteTransaction> {
        Ok(PyWriteTransaction {
            tx: Self::begin(slf, true)?,
        })
    }

    /// Begin a read-only transaction. Use as a context manager:
    ///
    /// ```python
    /// with db.begin_read() as tx:
    ///     results = tx.query("MATCH (n) RETURN n")
    /// ```
    fn begin_read(slf: &Bound<'_, Self>) -> PyResult<PyReadTransaction> {
        Ok(PyReadTransaction {
            tx: Self::begin(slf, false)?,
        })
    }

    /// Write a consistent single-file snapshot of this database to ``path``
    /// (``str`` or ``os.PathLike``).
    ///
    /// Uses SQLite's ``VACUUM INTO`` under the hood: produces a self-contained
    /// file (no ``-wal`` / ``-shm`` sidecars), defragmented and compacted.
    /// Raises if a transaction is active on this handle or if ``path`` already
    /// exists.
    fn snapshot_to(&mut self, py: Python, path: PathBuf) -> PyResult<()> {
        let db = self.db_mut()?;
        py.detach(|| call_core(db, |db| db.snapshot_to(&path)))
    }

    /// Close the database connection. An open transaction is rolled back;
    /// its transaction object then raises on every call.
    fn close(&mut self) {
        if let Some(mut db) = self.inner.take() {
            if self.active_tx.take().is_some() {
                // Nothing useful to do if the rollback fails: the
                // connection is closed right after, which discards the
                // transaction anyway.
                let _ = call_core(&mut db, |db| db.rollback());
            }
        }
    }

    fn __repr__(&self) -> String {
        match &self.inner {
            Some(_) => format!("Database('{}')", self.path),
            None => "Database(<closed>)".to_string(),
        }
    }

    fn __enter__(slf: PyRef<Self>) -> PyRef<Self> {
        slf
    }

    #[pyo3(signature = (_exc_type=None, _exc_val=None, _exc_tb=None))]
    fn __exit__(
        &mut self,
        _exc_type: Option<&Bound<'_, pyo3::types::PyAny>>,
        _exc_val: Option<&Bound<'_, pyo3::types::PyAny>>,
        _exc_tb: Option<&Bound<'_, pyo3::types::PyAny>>,
    ) -> bool {
        self.close();
        false // don't suppress exceptions
    }
}

/// State shared by the two transaction classes: which database, and which
/// of its transactions this object owns.
struct TxHandle {
    parent: Py<PyDatabase>,
    id: u64,
    finished: bool,
}

impl TxHandle {
    /// Run `op` on the parent's database inside this transaction, with the
    /// GIL released.
    ///
    /// Errors when this transaction is finished, its database is closed, or
    /// its transaction is no longer the open one. Afterwards, if the
    /// transaction is no longer open (committed, rolled back, or ended by a
    /// panic or by SQLite), this object is marked finished.
    fn run<T: Send>(
        &mut self,
        py: Python,
        op: impl FnOnce(&mut RustDatabase) -> Result<T, GraphError> + Send,
    ) -> PyResult<T> {
        if self.finished {
            return Err(finished_err());
        }
        let mut parent = self
            .parent
            .try_borrow_mut(py)
            .map_err(|_| PyRuntimeError::new_err("database is in use by another call"))?;
        let parent = &mut *parent;
        let Some(db) = parent.inner.as_mut() else {
            self.finished = true;
            return Err(closed_err());
        };
        if parent.active_tx != Some(self.id) {
            self.finished = true;
            return Err(finished_err());
        }
        let result = py.detach(|| call_core(db, op));
        if !db.in_transaction() {
            parent.active_tx = None;
            self.finished = true;
        }
        result
    }

    /// Roll back unless finished; used by `rollback()` and on drop.
    /// Errors from the rollback itself are not reported: the transaction is
    /// over either way.
    fn rollback_quietly(&mut self, py: Python) {
        if self.finished {
            return;
        }
        let _ = self.run(py, |db| db.rollback());
        self.finished = true;
    }

    fn execute(
        &mut self,
        py: Python,
        cypher: &str,
        params: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Vec<PyObject>> {
        let param_map = params.map(py_dict_to_value_map).transpose()?;
        let records = self.run(py, |db| db.execute_with_params(cypher, param_map.as_ref()))?;
        records_to_py(py, &records)
    }

    fn commit(&mut self, py: Python) -> PyResult<()> {
        self.run(py, |db| db.commit())
    }
}

impl Drop for TxHandle {
    /// A transaction object dropped without commit/rollback rolls back and
    /// releases the database for the next transaction.
    fn drop(&mut self) {
        if !self.finished {
            Python::attach(|py| self.rollback_quietly(py));
        }
    }
}

/// Python wrapper for a read-write transaction.
///
/// Statements run on the parent `Database`. A statement that fails is rolled
/// back on its own; the transaction stays open. The transaction ends on
/// commit, rollback, context-manager exit, or when the object is dropped
/// (rolled back).
#[pyclass(name = "WriteTransaction", unsendable)]
pub struct PyWriteTransaction {
    tx: TxHandle,
}

#[pymethods]
impl PyWriteTransaction {
    /// Execute a Cypher query within this transaction.
    ///
    /// Optional `params` dict substitutes `$name` parameters in the query.
    #[pyo3(signature = (cypher, params=None))]
    fn execute(
        &mut self,
        py: Python,
        cypher: &str,
        params: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Vec<PyObject>> {
        self.tx.execute(py, cypher, params)
    }

    /// Execute a read-only Cypher query within this transaction.
    #[pyo3(signature = (cypher, params=None))]
    fn query(
        &mut self,
        py: Python,
        cypher: &str,
        params: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Vec<PyObject>> {
        self.tx.execute(py, cypher, params)
    }

    /// Create a secondary index on (label, property) for faster lookups.
    fn create_index(&mut self, py: Python, label: &str, property: &str) -> PyResult<()> {
        self.tx.run(py, |db| db.create_index(label, property))
    }

    /// Drop a secondary index on (label, property).
    fn drop_index(&mut self, py: Python, label: &str, property: &str) -> PyResult<()> {
        self.tx.run(py, |db| db.drop_index(label, property))
    }

    /// Create a composite index on (label, [properties]) for faster multi-property lookups.
    fn create_composite_index(
        &mut self,
        py: Python,
        label: &str,
        properties: Vec<String>,
    ) -> PyResult<()> {
        self.tx.run(py, |db| {
            let refs: Vec<&str> = properties.iter().map(String::as_str).collect();
            db.create_composite_index(label, &refs)
        })
    }

    /// Drop a composite index on (label, [properties]).
    fn drop_composite_index(
        &mut self,
        py: Python,
        label: &str,
        properties: Vec<String>,
    ) -> PyResult<()> {
        self.tx.run(py, |db| {
            let refs: Vec<&str> = properties.iter().map(String::as_str).collect();
            db.drop_composite_index(label, &refs)
        })
    }

    /// Create a fulltext index on (label, property). Accelerates
    /// `CONTAINS` / `STARTS WITH` / `ENDS WITH` Cypher predicates via
    /// SQLite FTS5 (trigram tokenizer, case-sensitive).
    fn create_fulltext_index(&mut self, py: Python, label: &str, property: &str) -> PyResult<()> {
        self.tx
            .run(py, |db| db.create_fulltext_index(label, property))
    }

    /// Create a case-insensitive fulltext index on (label, property).
    ///
    /// Backed by SQLite FTS5's `trigram case_sensitive 0` tokenizer. Accelerates
    /// the case-insensitive idiom `toLower(n.prop) CONTAINS toLower($q)` (and
    /// `STARTS WITH` / `ENDS WITH`); plain predicates keep Cypher's
    /// case-sensitive semantics.
    fn create_fulltext_index_ci(
        &mut self,
        py: Python,
        label: &str,
        property: &str,
    ) -> PyResult<()> {
        self.tx
            .run(py, |db| db.create_fulltext_index_ci(label, property))
    }

    /// Create a word-tokenized fulltext index on (label, property).
    ///
    /// Backed by SQLite FTS5's `unicode61` tokenizer — enables the
    /// `fts.search` procedure for BM25-ranked full-text search over
    /// natural-language text.
    fn create_fulltext_index_word(
        &mut self,
        py: Python,
        label: &str,
        property: &str,
    ) -> PyResult<()> {
        self.tx
            .run(py, |db| db.create_fulltext_index_word(label, property))
    }

    /// Create a multi-property word-tokenized fulltext index on `label`.
    ///
    /// One FTS5 virtual table covers all listed properties. Use with
    /// `CALL fts.search(label, '*', query)` to search across all
    /// columns, or `CALL fts.search(label, property, query)` to scope
    /// to one column.
    fn create_fulltext_index_word_multi(
        &mut self,
        py: Python,
        label: &str,
        properties: Vec<String>,
    ) -> PyResult<()> {
        self.tx.run(py, |db| {
            db.create_fulltext_index_word_multi(label, &properties)
        })
    }

    /// Drop a fulltext index on (label, property).
    fn drop_fulltext_index(&mut self, py: Python, label: &str, property: &str) -> PyResult<()> {
        self.tx
            .run(py, |db| db.drop_fulltext_index(label, property))
    }

    /// Create multiple nodes with the same label in a single batch.
    ///
    /// Returns a list of node IDs (as integers). Implemented as one Cypher
    /// `UNWIND $rows AS p CREATE (n:Label) SET n = p RETURN id(n)` query.
    fn batch_create_nodes(
        &mut self,
        py: Python,
        label: &str,
        nodes: Vec<Bound<'_, PyDict>>,
    ) -> PyResult<Vec<u64>> {
        let rows: Vec<Value> = nodes
            .iter()
            .map(|d| {
                let mut map = std::collections::BTreeMap::new();
                for (k, v) in d.iter() {
                    map.insert(k.extract::<String>()?, py_to_value(&v)?);
                }
                Ok::<_, PyErr>(Value::Map(map))
            })
            .collect::<PyResult<_>>()?;
        validate_symbolic_name(label, "label")?;
        let cypher = format!("UNWIND $rows AS p CREATE (n:{label}) SET n = p RETURN id(n) AS id");
        let mut params = HashMap::new();
        params.insert("rows".to_string(), Value::List(rows));
        let records = self
            .tx
            .run(py, |db| db.execute_with_params(&cypher, Some(&params)))?;
        records
            .into_iter()
            .map(|r| match r.get("id") {
                Some(Value::I64(n)) => Ok(*n as u64),
                _ => Err(PyRuntimeError::new_err(
                    "batch_create_nodes: expected id column",
                )),
            })
            .collect()
    }

    /// Create multiple edges of the same type. Each edge is a tuple of
    /// `(src_id, dst_id)` or `(src_id, dst_id, {props})`.
    ///
    /// Implemented as one Cypher
    /// `UNWIND $rows AS row MATCH (a),(b) WHERE id(a)=row.s AND id(b)=row.d
    ///  CREATE (a)-[r:T]->(b) SET r = row.p` query.
    fn batch_create_edges(
        &mut self,
        py: Python,
        edge_type: &str,
        edges: Vec<Bound<'_, PyTuple>>,
    ) -> PyResult<()> {
        validate_symbolic_name(edge_type, "edge_type")?;
        let mut rows: Vec<Value> = Vec::with_capacity(edges.len());
        for tup in &edges {
            let src: u64 = tup.get_item(0)?.extract()?;
            let dst: u64 = tup.get_item(1)?.extract()?;
            let mut props = std::collections::BTreeMap::new();
            if tup.len() > 2 {
                let dict: Bound<'_, PyDict> = tup.get_item(2)?.cast_into().map_err(|_| {
                    PyValueError::new_err("edge tuple third element must be a dict")
                })?;
                for (k, v) in dict.iter() {
                    props.insert(k.extract::<String>()?, py_to_value(&v)?);
                }
            }
            let mut row = std::collections::BTreeMap::new();
            row.insert("s".to_string(), Value::I64(src as i64));
            row.insert("d".to_string(), Value::I64(dst as i64));
            row.insert("p".to_string(), Value::Map(props));
            rows.push(Value::Map(row));
        }
        let cypher = format!(
            "UNWIND $rows AS row \
             MATCH (a) WHERE id(a) = row.s \
             MATCH (b) WHERE id(b) = row.d \
             CREATE (a)-[r:{edge_type}]->(b) SET r = row.p"
        );
        let mut params = HashMap::new();
        params.insert("rows".to_string(), Value::List(rows));
        self.tx
            .run(py, |db| db.execute_with_params(&cypher, Some(&params)))?;
        Ok(())
    }

    /// Commit the transaction. If the commit fails the transaction stays
    /// open, so it can still be rolled back.
    fn commit(&mut self, py: Python) -> PyResult<()> {
        self.tx.commit(py)
    }

    /// Rollback the transaction.
    fn rollback(&mut self, py: Python) -> PyResult<()> {
        if self.tx.finished {
            return Err(finished_err());
        }
        self.tx.rollback_quietly(py);
        Ok(())
    }

    fn __enter__(slf: PyRef<Self>) -> PyRef<Self> {
        slf
    }

    #[pyo3(signature = (exc_type=None, _exc_val=None, _exc_tb=None))]
    fn __exit__(
        &mut self,
        py: Python,
        exc_type: Option<&Bound<'_, pyo3::types::PyAny>>,
        _exc_val: Option<&Bound<'_, pyo3::types::PyAny>>,
        _exc_tb: Option<&Bound<'_, pyo3::types::PyAny>>,
    ) -> PyResult<bool> {
        if !self.tx.finished {
            if exc_type.is_some() {
                self.tx.rollback_quietly(py);
            } else {
                self.tx.commit(py)?;
            }
        }
        Ok(false) // don't suppress exceptions
    }
}

/// Python wrapper for a read-only transaction.
#[pyclass(name = "ReadTransaction", unsendable)]
pub struct PyReadTransaction {
    tx: TxHandle,
}

#[pymethods]
impl PyReadTransaction {
    /// Execute a read-only Cypher query within this transaction.
    #[pyo3(signature = (cypher, params=None))]
    fn query(
        &mut self,
        py: Python,
        cypher: &str,
        params: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Vec<PyObject>> {
        self.tx.execute(py, cypher, params)
    }

    /// Commit (release) the read transaction.
    fn commit(&mut self, py: Python) -> PyResult<()> {
        self.tx.commit(py)
    }

    /// Roll back (release) the read transaction. Same effect as `commit()`:
    /// a read transaction has nothing to undo.
    fn rollback(&mut self, py: Python) -> PyResult<()> {
        if self.tx.finished {
            return Err(finished_err());
        }
        self.tx.rollback_quietly(py);
        Ok(())
    }

    fn __enter__(slf: PyRef<Self>) -> PyRef<Self> {
        slf
    }

    #[pyo3(signature = (_exc_type=None, _exc_val=None, _exc_tb=None))]
    fn __exit__(
        &mut self,
        py: Python,
        _exc_type: Option<&Bound<'_, pyo3::types::PyAny>>,
        _exc_val: Option<&Bound<'_, pyo3::types::PyAny>>,
        _exc_tb: Option<&Bound<'_, pyo3::types::PyAny>>,
    ) -> PyResult<bool> {
        if !self.tx.finished {
            self.tx.commit(py)?;
        }
        Ok(false)
    }
}

/// Python module definition.
#[pymodule]
pub fn _graphdblite(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    m.add_class::<PyDatabase>()?;
    m.add_class::<PyWriteTransaction>()?;
    m.add_class::<PyReadTransaction>()?;
    m.add("GraphDBError", py.get_type::<GraphDBError>())?;
    m.add("ParseError", py.get_type::<ParseError>())?;
    m.add("CypherSyntaxError", py.get_type::<CypherSyntaxError>())?;
    m.add("CypherSemanticError", py.get_type::<CypherSemanticError>())?;
    m.add("CypherTypeError", py.get_type::<CypherTypeError>())?;
    m.add("CypherArgumentError", py.get_type::<CypherArgumentError>())?;
    m.add(
        "CypherArithmeticError",
        py.get_type::<CypherArithmeticError>(),
    )?;
    m.add("EntityNotFoundError", py.get_type::<EntityNotFoundError>())?;
    m.add(
        "ConstraintViolationError",
        py.get_type::<ConstraintViolationError>(),
    )?;
    m.add("ProcedureError", py.get_type::<ProcedureError>())?;
    m.add("StorageError", py.get_type::<StorageError>())?;
    m.add("NodeNotFoundError", py.get_type::<NodeNotFoundError>())?;
    m.add("TransactionError", py.get_type::<TransactionError>())?;
    Ok(())
}
