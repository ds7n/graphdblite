#![allow(unexpected_cfgs)]

use std::path::PathBuf;

use pyo3::create_exception;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyDict;

use graphdblite::{Config, Database as RustDatabase, GraphError, Value};

// --- Exception hierarchy ---

create_exception!(_graphdblite, GraphDBError, pyo3::exceptions::PyException);
create_exception!(_graphdblite, ParseError, GraphDBError);
create_exception!(_graphdblite, StorageError, GraphDBError);
create_exception!(_graphdblite, NodeNotFoundError, GraphDBError);

/// Map a GraphError to the appropriate Python exception.
fn to_py_err(e: GraphError) -> PyErr {
    match &e {
        GraphError::ParseError(_) => ParseError::new_err(e.to_string()),
        GraphError::Storage(_) => StorageError::new_err(e.to_string()),
        GraphError::NodeNotFound(_) => NodeNotFoundError::new_err(e.to_string()),
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
        Value::Path(nodes) => {
            let ids: Vec<PyObject> = nodes.iter().map(|id| id.0.to_object(py)).collect();
            ids.to_object(py)
        }
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
    fn query(&mut self, py: Python, cypher: &str) -> PyResult<Vec<PyObject>> {
        let db = self
            .inner
            .as_mut()
            .ok_or_else(|| PyRuntimeError::new_err("database is closed"))?;
        let cypher = cypher.to_string();
        let records = py
            .allow_threads(|| {
                let tx = db.begin_read()?;
                let r = tx.query(&cypher)?;
                tx.commit()?;
                Ok::<_, GraphError>(r)
            })
            .map_err(to_py_err)?;
        records_to_py(py, &records)
    }

    /// Execute a write Cypher query (CREATE, DELETE, SET, MERGE). Returns a list of dicts.
    ///
    /// Note: holds an exclusive borrow (`&mut self`) — see `query()` docstring.
    fn execute(&mut self, py: Python, cypher: &str) -> PyResult<Vec<PyObject>> {
        let db = self
            .inner
            .as_mut()
            .ok_or_else(|| PyRuntimeError::new_err("database is closed"))?;
        let cypher = cypher.to_string();
        let records = py
            .allow_threads(|| {
                let tx = db.begin_write()?;
                let r = tx.query(&cypher)?;
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
    fn execute(&self, py: Python, cypher: &str) -> PyResult<Vec<PyObject>> {
        let db = self.get_db()?;
        let conn = db.connection();
        let stmt = graphdblite::cypher::parser::parse(cypher).map_err(to_py_err)?;
        let plan = graphdblite::cypher::planner::plan(conn, &stmt).map_err(to_py_err)?;
        let ctx = graphdblite::cypher::executor::ExecContext {
            max_result_rows: db.max_result_rows,
        };
        let records = graphdblite::cypher::executor::execute_with_ctx(conn, &plan, &ctx)
            .map_err(to_py_err)?;
        records_to_py(py, &records)
    }

    /// Execute a read-only Cypher query within this transaction.
    fn query(&self, py: Python, cypher: &str) -> PyResult<Vec<PyObject>> {
        self.execute(py, cypher)
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
    fn query(&self, py: Python, cypher: &str) -> PyResult<Vec<PyObject>> {
        let db = self.get_db()?;
        let conn = db.connection();
        let stmt = graphdblite::cypher::parser::parse(cypher).map_err(to_py_err)?;
        let plan = graphdblite::cypher::planner::plan(conn, &stmt).map_err(to_py_err)?;
        let ctx = graphdblite::cypher::executor::ExecContext {
            max_result_rows: db.max_result_rows,
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
