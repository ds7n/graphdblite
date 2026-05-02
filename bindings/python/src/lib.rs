#![allow(unexpected_cfgs)]

use std::path::PathBuf;

use std::collections::HashMap;

use pyo3::create_exception;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyFloat, PyList, PyString, PyTuple};

use graphdblite::{Config, Database as RustDatabase, GraphError, NodeId, Properties, Value};

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
fn value_to_py(py: Python, val: &Value) -> PyObject {
    match val {
        Value::Null => py.None(),
        Value::Bool(b) => b.to_object(py),
        Value::I64(n) => n.to_object(py),
        Value::F64(n) => n.to_object(py),
        Value::String(s) => s.to_object(py),
        Value::List(items) => {
            let py_items: Vec<PyObject> = items.iter().map(|v| value_to_py(py, v)).collect();
            py_items.to_object(py)
        }
        Value::Path(p) => {
            let ids: Vec<PyObject> = p.nodes.iter().map(|n| n.id.0.to_object(py)).collect();
            ids.to_object(py)
        }
        Value::Node(n) => {
            let dict = PyDict::new_bound(py);
            dict.set_item("__id", n.id.0).unwrap();
            dict.set_item("__labels", &n.labels).unwrap();
            for (k, v) in &n.properties {
                dict.set_item(k, value_to_py(py, v)).unwrap();
            }
            dict.to_object(py)
        }
        Value::Edge(e) => {
            let dict = PyDict::new_bound(py);
            dict.set_item("__src", e.src.0).unwrap();
            dict.set_item("__dst", e.dst.0).unwrap();
            dict.set_item("__label", &e.label).unwrap();
            for (k, v) in &e.properties {
                dict.set_item(k, value_to_py(py, v)).unwrap();
            }
            dict.to_object(py)
        }
        Value::Map(map) => {
            let dict = PyDict::new_bound(py);
            for (k, v) in map {
                dict.set_item(k, value_to_py(py, v)).unwrap();
            }
            dict.to_object(py)
        }
        // Temporal types — expose as ISO string.
        other => format!("{other}").to_object(py),
    }
}

fn records_to_py(
    py: Python,
    records: &[graphdblite::cypher::record::Record],
) -> PyResult<Vec<PyObject>> {
    let mut result = Vec::new();
    for rec in records {
        let dict = PyDict::new_bound(py);
        for (key, val) in &rec.fields {
            dict.set_item(key, value_to_py(py, val))?;
        }
        result.push(dict.to_object(py));
    }
    Ok(result)
}

/// Convert a Python object to a graphdblite Value.
fn py_to_value(obj: &Bound<'_, pyo3::types::PyAny>) -> PyResult<Value> {
    if obj.is_none() {
        Ok(Value::Null)
    } else if obj.downcast::<PyBool>().is_ok() {
        // Check bool before i64 — Python bool is a subclass of int.
        Ok(Value::Bool(obj.extract::<bool>()?))
    } else if let Ok(n) = obj.extract::<i64>() {
        Ok(Value::I64(n))
    } else if obj.downcast::<PyFloat>().is_ok() {
        Ok(Value::F64(obj.extract::<f64>()?))
    } else if obj.downcast::<PyString>().is_ok() {
        Ok(Value::String(obj.extract::<String>()?))
    } else if let Ok(list) = obj.downcast::<PyList>() {
        let items: PyResult<Vec<Value>> = list.iter().map(|item| py_to_value(&item)).collect();
        Ok(Value::List(items?))
    } else if let Ok(dict) = obj.downcast::<PyDict>() {
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

/// Convert a Python dict to a Properties map.
fn py_dict_to_properties(dict: &Bound<'_, PyDict>) -> PyResult<Properties> {
    let mut props = Properties::new();
    for (key, value) in dict.iter() {
        let key: String = key.extract()?;
        let val = py_to_value(&value)?;
        props.insert(key, val);
    }
    Ok(props)
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
#[pyclass(name = "Database")]
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
            .allow_threads(|| {
                let tx = db.begin_read()?;
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
            .allow_threads(|| {
                let tx = db.begin_write()?;
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
#[pyclass(name = "WriteTransaction")]
pub struct PyWriteTransaction {
    db: Option<RustDatabase>,
    parent: Py<PyDatabase>,
    finished: bool,
}

impl PyWriteTransaction {
    fn start(db: RustDatabase, parent: Py<PyDatabase>) -> PyResult<Self> {
        db.connection()
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(|e| PyRuntimeError::new_err(format!("failed to begin transaction: {e}")))?;
        Ok(Self {
            db: Some(db),
            parent,
            finished: false,
        })
    }

    fn get_db(&self) -> PyResult<&RustDatabase> {
        self.db
            .as_ref()
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
        &self,
        py: Python,
        cypher: &str,
        params: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Vec<PyObject>> {
        let db = self.get_db()?;
        let conn = db.connection();
        let mut stmt = graphdblite::cypher::parser::parse(cypher).map_err(to_py_err)?;
        if let Some(p) = params {
            let map = py_dict_to_value_map(p)?;
            stmt = graphdblite::cypher::parser::resolve_params(&stmt, &map).map_err(to_py_err)?;
        }
        let plan = graphdblite::cypher::planner::plan(conn, &stmt).map_err(to_py_err)?;
        let ctx = graphdblite::cypher::executor::ExecContext {
            max_result_rows: db.max_result_rows,
            ..Default::default()
        };
        let records = graphdblite::cypher::executor::execute_with_ctx(conn, &plan, &ctx)
            .map_err(to_py_err)?;
        records_to_py(py, &records)
    }

    /// Execute a read-only Cypher query within this transaction.
    #[pyo3(signature = (cypher, params=None))]
    fn query(
        &self,
        py: Python,
        cypher: &str,
        params: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Vec<PyObject>> {
        self.execute(py, cypher, params)
    }

    /// Create a secondary index on (label, property) for faster lookups.
    fn create_index(&self, label: &str, property: &str) -> PyResult<()> {
        let db = self.get_db()?;
        graphdblite::index::create_index(db.connection(), label, property).map_err(to_py_err)
    }

    /// Drop a secondary index on (label, property).
    fn drop_index(&self, label: &str, property: &str) -> PyResult<()> {
        let db = self.get_db()?;
        graphdblite::index::drop_index(db.connection(), label, property).map_err(to_py_err)
    }

    /// Create multiple nodes with the same label in a single batch.
    ///
    /// Returns a list of node IDs (as integers).
    fn batch_create_nodes(&self, label: &str, nodes: Vec<Bound<'_, PyDict>>) -> PyResult<Vec<u64>> {
        let db = self.get_db()?;
        let conn = db.connection();
        let mut ids = Vec::with_capacity(nodes.len());
        for dict in &nodes {
            let props = py_dict_to_properties(dict)?;
            let id = graphdblite::node::create_node(conn, &[label.to_string()], props.clone())
                .map_err(to_py_err)?;
            graphdblite::index::update_indexes_for_node(conn, id, label, None, &props)
                .map_err(to_py_err)?;
            ids.push(id.0);
        }
        Ok(ids)
    }

    /// Create multiple edges of the same type in a single batch with adjacency coalescing.
    ///
    /// Each edge is a tuple of (src_id, dst_id) or (src_id, dst_id, {props}).
    fn batch_create_edges(&self, edge_type: &str, edges: Vec<Bound<'_, PyTuple>>) -> PyResult<()> {
        let db = self.get_db()?;
        let conn = db.connection();
        let mut edge_data: Vec<(NodeId, NodeId, Properties)> = Vec::with_capacity(edges.len());
        for tup in &edges {
            let src: u64 = tup.get_item(0)?.extract()?;
            let dst: u64 = tup.get_item(1)?.extract()?;
            let props = if tup.len() > 2 {
                let dict: Bound<'_, PyDict> = tup.get_item(2)?.downcast_into().map_err(|_| {
                    PyValueError::new_err("edge tuple third element must be a dict")
                })?;
                py_dict_to_properties(&dict)?
            } else {
                Properties::new()
            };
            edge_data.push((NodeId(src), NodeId(dst), props));
        }
        graphdblite::edge::batch_create_edges(conn, edge_type, &edge_data).map_err(to_py_err)
    }

    /// Commit the transaction.
    fn commit(&mut self, py: Python) -> PyResult<()> {
        let db = self.get_db()?;
        db.connection()
            .execute_batch("COMMIT")
            .map_err(|e| PyRuntimeError::new_err(format!("commit failed: {e}")))?;
        self.finished = true;
        self.return_db(py);
        Ok(())
    }

    /// Rollback the transaction.
    fn rollback(&mut self, py: Python) -> PyResult<()> {
        let db = self.get_db()?;
        let _ = db.connection().execute_batch("ROLLBACK");
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
#[pyclass(name = "ReadTransaction")]
pub struct PyReadTransaction {
    db: Option<RustDatabase>,
    parent: Py<PyDatabase>,
    finished: bool,
}

impl PyReadTransaction {
    fn start(db: RustDatabase, parent: Py<PyDatabase>) -> PyResult<Self> {
        db.connection()
            .execute_batch("BEGIN DEFERRED")
            .map_err(|e| PyRuntimeError::new_err(format!("failed to begin transaction: {e}")))?;
        Ok(Self {
            db: Some(db),
            parent,
            finished: false,
        })
    }

    fn get_db(&self) -> PyResult<&RustDatabase> {
        self.db
            .as_ref()
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
        &self,
        py: Python,
        cypher: &str,
        params: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Vec<PyObject>> {
        let db = self.get_db()?;
        let conn = db.connection();
        let mut stmt = graphdblite::cypher::parser::parse(cypher).map_err(to_py_err)?;
        if let Some(p) = params {
            let map = py_dict_to_value_map(p)?;
            stmt = graphdblite::cypher::parser::resolve_params(&stmt, &map).map_err(to_py_err)?;
        }
        let plan = graphdblite::cypher::planner::plan(conn, &stmt).map_err(to_py_err)?;
        let ctx = graphdblite::cypher::executor::ExecContext {
            max_result_rows: db.max_result_rows,
            ..Default::default()
        };
        let records = graphdblite::cypher::executor::execute_with_ctx(conn, &plan, &ctx)
            .map_err(to_py_err)?;
        records_to_py(py, &records)
    }

    /// Commit (release) the read transaction.
    fn commit(&mut self, py: Python) -> PyResult<()> {
        let db = self.get_db()?;
        db.connection()
            .execute_batch("COMMIT")
            .map_err(|e| PyRuntimeError::new_err(format!("commit failed: {e}")))?;
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
    m.add("GraphDBError", m.py().get_type_bound::<GraphDBError>())?;
    m.add("ParseError", m.py().get_type_bound::<ParseError>())?;
    m.add("StorageError", m.py().get_type_bound::<StorageError>())?;
    m.add(
        "NodeNotFoundError",
        m.py().get_type_bound::<NodeNotFoundError>(),
    )?;
    Ok(())
}
