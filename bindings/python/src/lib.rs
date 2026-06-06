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

use graphdblite::{Config, Database as RustDatabase, GraphError, Record, Value};

// --- Exception hierarchy ---

create_exception!(_graphdblite, GraphDBError, pyo3::exceptions::PyException);
create_exception!(_graphdblite, ParseError, GraphDBError);
create_exception!(_graphdblite, StorageError, GraphDBError);
create_exception!(_graphdblite, NodeNotFoundError, GraphDBError);

/// Map a GraphError to the appropriate Python exception.
fn to_py_err(e: GraphError) -> PyErr {
    match &e {
        // Syntax/semantic/etc. query errors all flow to ParseError for now —
        // preserving the existing coarse surface area. Future refinement could
        // introduce per-kind exception classes.
        GraphError::Query(_) => ParseError::new_err(e.to_string()),
        GraphError::Storage { .. } => StorageError::new_err(e.to_string()),
        GraphError::NodeNotFound { .. } => NodeNotFoundError::new_err(e.to_string()),
        _ => GraphDBError::new_err(e.to_string()),
    }
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
            let ids: Vec<PyObject> = p
                .nodes
                .iter()
                .map(|n| Ok(n.id.0.into_pyobject(py)?.into_any().unbind()))
                .collect::<PyResult<_>>()?;
            ids.into_pyobject(py)?.into_any().unbind()
        }
        Value::Node(n) => {
            let dict = PyDict::new(py);
            dict.set_item("__id", n.id.0)?;
            dict.set_item("__labels", &n.labels)?;
            for (k, v) in &n.properties {
                dict.set_item(k, value_to_py(py, v)?)?;
            }
            dict.into_any().unbind()
        }
        Value::Edge(e) => {
            let dict = PyDict::new(py);
            dict.set_item("__src", e.src.0)?;
            dict.set_item("__dst", e.dst.0)?;
            dict.set_item("__label", &e.label)?;
            for (k, v) in &e.properties {
                dict.set_item(k, value_to_py(py, v)?)?;
            }
            dict.into_any().unbind()
        }
        Value::Map(map) => {
            let dict = PyDict::new(py);
            for (k, v) in map {
                dict.set_item(k, value_to_py(py, v)?)?;
            }
            dict.into_any().unbind()
        }
        // Temporal types — expose as ISO string.
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

/// Convert a Python object to a graphdblite Value.
fn py_to_value(obj: &Bound<'_, pyo3::types::PyAny>) -> PyResult<Value> {
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
        let items: PyResult<Vec<Value>> = list.iter().map(|item| py_to_value(&item)).collect();
        Ok(Value::List(items?))
    } else if let Ok(dict) = obj.cast::<PyDict>() {
        let mut map = std::collections::BTreeMap::new();
        for (key, value) in dict.iter() {
            let k: String = key.extract()?;
            let v = py_to_value(&value)?;
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

/// Python wrapper for the graphdblite Database.
///
/// `unsendable` is required in pyo3 0.28+: `RustDatabase` wraps a
/// `rusqlite::Connection` (`RefCell` internally), which is `!Send`/`!Sync`.
/// The binding already serializes access through `&mut self`; this attribute
/// also enforces Python-side single-threaded use.
#[pyclass(name = "Database", unsendable)]
pub struct PyDatabase {
    inner: Option<RustDatabase>,
    path: String,
}

#[pymethods]
impl PyDatabase {
    /// Open a database at the given path.
    ///
    /// The path is canonicalized to prevent directory traversal.
    #[new]
    #[pyo3(signature = (path, busy_timeout_ms=5000))]
    fn new(path: &str, busy_timeout_ms: u32) -> PyResult<Self> {
        let resolved = PathBuf::from(path);
        let resolved = if resolved.exists() {
            resolved
                .canonicalize()
                .map_err(|e| PyValueError::new_err(format!("invalid path '{path}': {e}")))?
        } else {
            let parent = resolved.parent().unwrap_or(std::path::Path::new("."));
            let parent = parent
                .canonicalize()
                .map_err(|e| PyValueError::new_err(format!("invalid path '{path}': {e}")))?;
            parent.join(resolved.file_name().ok_or_else(|| {
                PyValueError::new_err(format!("invalid path '{path}': no filename"))
            })?)
        };
        let config = Config {
            busy_timeout_ms,
            ..Config::default()
        };
        let display_path = resolved.display().to_string();
        let db = RustDatabase::open_with_config(&resolved, config).map_err(to_py_err)?;
        Ok(Self {
            inner: Some(db),
            path: display_path,
        })
    }

    /// Open an in-memory database (for testing).
    #[staticmethod]
    fn open_memory() -> PyResult<Self> {
        let db = RustDatabase::open_memory().map_err(to_py_err)?;
        Ok(Self {
            inner: Some(db),
            path: ":memory:".to_string(),
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
        let db = self
            .inner
            .as_mut()
            .ok_or_else(|| PyRuntimeError::new_err("database is closed"))?;
        let param_map = params.map(py_dict_to_value_map).transpose()?;
        let cypher = cypher.to_string();
        let records = py
            .detach(|| {
                let tx = db.read_tx()?;
                let r = tx.query_with_params(&cypher, param_map.as_ref())?;
                tx.commit()?;
                Ok::<_, GraphError>(r)
            })
            .map_err(to_py_err)?;
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
        let db = self
            .inner
            .as_mut()
            .ok_or_else(|| PyRuntimeError::new_err("database is closed"))?;
        let param_map = params.map(py_dict_to_value_map).transpose()?;
        let cypher = cypher.to_string();
        let records = py
            .detach(|| {
                let tx = db.write_tx()?;
                let r = tx.query_with_params(&cypher, param_map.as_ref())?;
                tx.commit()?;
                Ok::<_, GraphError>(r)
            })
            .map_err(to_py_err)?;
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
        let mut this = slf.borrow_mut();
        let db = this.inner.take().ok_or_else(|| {
            PyRuntimeError::new_err("database is closed or already in a transaction")
        })?;
        PyWriteTransaction::start(db, slf.clone().unbind())
    }

    /// Begin a read-only transaction. Use as a context manager:
    ///
    /// ```python
    /// with db.begin_read() as tx:
    ///     results = tx.query("MATCH (n) RETURN n")
    /// ```
    fn begin_read(slf: &Bound<'_, Self>) -> PyResult<PyReadTransaction> {
        let mut this = slf.borrow_mut();
        let db = this.inner.take().ok_or_else(|| {
            PyRuntimeError::new_err("database is closed or already in a transaction")
        })?;
        PyReadTransaction::start(db, slf.clone().unbind())
    }

    /// Write a consistent single-file snapshot of this database to ``path``.
    ///
    /// Uses SQLite's ``VACUUM INTO`` under the hood: produces a self-contained
    /// file (no ``-wal`` / ``-shm`` sidecars), defragmented and compacted.
    /// Raises if a transaction is active on this handle or if ``path`` already
    /// exists.
    fn snapshot_to(&mut self, py: Python, path: &str) -> PyResult<()> {
        let db = self
            .inner
            .as_mut()
            .ok_or_else(|| PyRuntimeError::new_err("database is closed"))?;
        let path_owned = path.to_string();
        py.detach(|| db.snapshot_to(&path_owned)).map_err(to_py_err)
    }

    /// Close the database connection.
    fn close(&mut self) {
        self.inner.take();
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

/// Python wrapper for a read-write transaction.
///
/// Holds the `RustDatabase` for the duration of the transaction. The database
/// is returned to `PyDatabase` on commit, rollback, or context manager exit.
#[pyclass(name = "WriteTransaction", unsendable)]
pub struct PyWriteTransaction {
    db: Option<RustDatabase>,
    parent: Py<PyDatabase>,
    finished: bool,
}

impl PyWriteTransaction {
    fn start(mut db: RustDatabase, parent: Py<PyDatabase>) -> PyResult<Self> {
        db.begin_write().map_err(to_py_err)?;
        Ok(Self {
            db: Some(db),
            parent,
            finished: false,
        })
    }

    fn get_db_mut(&mut self) -> PyResult<&mut RustDatabase> {
        self.db
            .as_mut()
            .ok_or_else(|| PyRuntimeError::new_err("transaction is already finished"))
    }

    /// Return the database to the parent PyDatabase.
    fn return_db(&mut self, py: Python) {
        if let Some(db) = self.db.take() {
            let mut parent = self.parent.borrow_mut(py);
            parent.inner = Some(db);
        }
    }
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
        let param_map = params.map(py_dict_to_value_map).transpose()?;
        let db = self.get_db_mut()?;
        let records = db
            .execute_with_params(cypher, param_map.as_ref())
            .map_err(to_py_err)?;
        records_to_py(py, &records)
    }

    /// Execute a read-only Cypher query within this transaction.
    #[pyo3(signature = (cypher, params=None))]
    fn query(
        &mut self,
        py: Python,
        cypher: &str,
        params: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Vec<PyObject>> {
        self.execute(py, cypher, params)
    }

    /// Create a secondary index on (label, property) for faster lookups.
    fn create_index(&mut self, label: &str, property: &str) -> PyResult<()> {
        let db = self.get_db_mut()?;
        db.create_index(label, property).map_err(to_py_err)
    }

    /// Drop a secondary index on (label, property).
    fn drop_index(&mut self, label: &str, property: &str) -> PyResult<()> {
        let db = self.get_db_mut()?;
        db.drop_index(label, property).map_err(to_py_err)
    }

    /// Create a fulltext index on (label, property). Accelerates
    /// `CONTAINS` / `STARTS WITH` / `ENDS WITH` Cypher predicates via
    /// SQLite FTS5 (trigram tokenizer, case-sensitive).
    fn create_fulltext_index(&mut self, label: &str, property: &str) -> PyResult<()> {
        let db = self.get_db_mut()?;
        db.create_fulltext_index(label, property).map_err(to_py_err)
    }

    /// Create a case-insensitive fulltext index on (label, property).
    ///
    /// Backed by SQLite FTS5's `trigram case_sensitive 0` tokenizer — plain
    /// `CONTAINS` / `STARTS WITH` / `ENDS WITH` against this property
    /// becomes case-insensitive and FTS-accelerated.
    fn create_fulltext_index_ci(&mut self, label: &str, property: &str) -> PyResult<()> {
        let db = self.get_db_mut()?;
        db.create_fulltext_index_ci(label, property)
            .map_err(to_py_err)
    }

    /// Create a word-tokenized fulltext index on (label, property).
    ///
    /// Backed by SQLite FTS5's `unicode61` tokenizer — enables the
    /// `fts.search` procedure for BM25-ranked full-text search over
    /// natural-language text.
    fn create_fulltext_index_word(&mut self, label: &str, property: &str) -> PyResult<()> {
        let db = self.get_db_mut()?;
        db.create_fulltext_index_word(label, property)
            .map_err(to_py_err)
    }

    /// Create a multi-property word-tokenized fulltext index on `label`.
    ///
    /// One FTS5 virtual table covers all listed properties. Use with
    /// `CALL fts.search(label, '*', query)` to search across all
    /// columns, or `CALL fts.search(label, property, query)` to scope
    /// to one column.
    fn create_fulltext_index_word_multi(
        &mut self,
        label: &str,
        properties: Vec<String>,
    ) -> PyResult<()> {
        let db = self.get_db_mut()?;
        db.create_fulltext_index_word_multi(label, &properties)
            .map_err(to_py_err)
    }

    /// Drop a fulltext index on (label, property).
    fn drop_fulltext_index(&mut self, label: &str, property: &str) -> PyResult<()> {
        let db = self.get_db_mut()?;
        db.drop_fulltext_index(label, property).map_err(to_py_err)
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
        let db = self.get_db_mut()?;
        let records = db
            .execute_with_params(&cypher, Some(&params))
            .map_err(to_py_err)?;
        let _py = py;
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
        let db = self.get_db_mut()?;
        db.execute_with_params(&cypher, Some(&params))
            .map_err(to_py_err)?;
        Ok(())
    }

    /// Commit the transaction.
    fn commit(&mut self, py: Python) -> PyResult<()> {
        let db = self.get_db_mut()?;
        db.commit().map_err(to_py_err)?;
        self.finished = true;
        self.return_db(py);
        Ok(())
    }

    /// Rollback the transaction.
    fn rollback(&mut self, py: Python) -> PyResult<()> {
        let db = self.get_db_mut()?;
        let _ = db.rollback();
        self.finished = true;
        self.return_db(py);
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
        if !self.finished {
            if exc_type.is_some() {
                self.rollback(py)?;
            } else {
                self.commit(py)?;
            }
        }
        Ok(false) // don't suppress exceptions
    }
}

/// Python wrapper for a read-only transaction.
#[pyclass(name = "ReadTransaction", unsendable)]
pub struct PyReadTransaction {
    db: Option<RustDatabase>,
    parent: Py<PyDatabase>,
    finished: bool,
}

impl PyReadTransaction {
    fn start(mut db: RustDatabase, parent: Py<PyDatabase>) -> PyResult<Self> {
        db.begin_read().map_err(to_py_err)?;
        Ok(Self {
            db: Some(db),
            parent,
            finished: false,
        })
    }

    fn get_db_mut(&mut self) -> PyResult<&mut RustDatabase> {
        self.db
            .as_mut()
            .ok_or_else(|| PyRuntimeError::new_err("transaction is already finished"))
    }

    fn return_db(&mut self, py: Python) {
        if let Some(db) = self.db.take() {
            let mut parent = self.parent.borrow_mut(py);
            parent.inner = Some(db);
        }
    }
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
        let param_map = params.map(py_dict_to_value_map).transpose()?;
        let db = self.get_db_mut()?;
        let records = db
            .execute_with_params(cypher, param_map.as_ref())
            .map_err(to_py_err)?;
        records_to_py(py, &records)
    }

    /// Commit (release) the read transaction.
    fn commit(&mut self, py: Python) -> PyResult<()> {
        let db = self.get_db_mut()?;
        db.commit().map_err(to_py_err)?;
        self.finished = true;
        self.return_db(py);
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
        if !self.finished {
            self.commit(py)?;
        }
        Ok(false)
    }
}

/// Python module definition.
#[pymodule]
pub fn _graphdblite(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyDatabase>()?;
    m.add_class::<PyWriteTransaction>()?;
    m.add_class::<PyReadTransaction>()?;
    m.add("GraphDBError", m.py().get_type::<GraphDBError>())?;
    m.add("ParseError", m.py().get_type::<ParseError>())?;
    m.add("StorageError", m.py().get_type::<StorageError>())?;
    m.add("NodeNotFoundError", m.py().get_type::<NodeNotFoundError>())?;
    Ok(())
}
