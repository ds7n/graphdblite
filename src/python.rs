use std::path::PathBuf;

use pyo3::create_exception;
use pyo3::exceptions::{PyKeyError, PyRuntimeError, PySyntaxError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyDict;

use crate::db::{Config, Database as RustDatabase};
use crate::types::{GraphError, Value};

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
    records: &[crate::cypher::record::Record],
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
        Ok(Self { inner: Some(db), path: display_path })
    }

    /// Open an in-memory database (for testing).
    #[staticmethod]
    fn open_memory() -> PyResult<Self> {
        let db = RustDatabase::open_memory().map_err(to_py_err)?;
        Ok(Self { inner: Some(db), path: ":memory:".to_string() })
    }

    /// Execute a read-only Cypher query. Returns a list of dicts.
    fn query(&mut self, py: Python, cypher: &str) -> PyResult<Vec<PyObject>> {
        let db = self.inner.as_mut().ok_or_else(|| PyRuntimeError::new_err("database is closed"))?;
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
    fn execute(&mut self, py: Python, cypher: &str) -> PyResult<Vec<PyObject>> {
        let db = self.inner.as_mut().ok_or_else(|| PyRuntimeError::new_err("database is closed"))?;
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

/// Python module definition.
#[pymodule]
pub fn _graphdblite(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyDatabase>()?;
    m.add("GraphDBError", m.py().get_type_bound::<GraphDBError>())?;
    m.add("ParseError", m.py().get_type_bound::<ParseError>())?;
    m.add("StorageError", m.py().get_type_bound::<StorageError>())?;
    m.add("NodeNotFoundError", m.py().get_type_bound::<NodeNotFoundError>())?;
    Ok(())
}
