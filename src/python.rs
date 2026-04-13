use std::path::PathBuf;

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyDict;

use crate::db::{Config, Database as RustDatabase};
use crate::types::Value;

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

/// Python wrapper for the graphdblite Database.
#[pyclass(name = "Database")]
pub struct PyDatabase {
    inner: RustDatabase,
}

#[pymethods]
impl PyDatabase {
    /// Open a database at the given path.
    ///
    /// The path is canonicalized to prevent directory traversal. Paths
    /// containing `..` components that resolve outside the working
    /// directory are allowed — canonicalization simply normalizes them.
    #[new]
    #[pyo3(signature = (path, busy_timeout_ms=5000))]
    fn new(path: &str, busy_timeout_ms: u32) -> PyResult<Self> {
        // Resolve to an absolute path so callers can't accidentally open
        // special files via relative traversal (e.g. "../../proc/self/mem").
        let resolved = PathBuf::from(path);
        let resolved = if resolved.exists() {
            resolved
                .canonicalize()
                .map_err(|e| PyValueError::new_err(format!("invalid path '{path}': {e}")))?
        } else {
            // File doesn't exist yet — canonicalize the parent directory.
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
        let db = RustDatabase::open_with_config(&resolved, config)
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        Ok(Self { inner: db })
    }

    /// Open an in-memory database (for testing).
    #[staticmethod]
    fn open_memory() -> PyResult<Self> {
        let db = RustDatabase::open_memory()
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        Ok(Self { inner: db })
    }

    /// Execute a read-only Cypher query. Returns a list of dicts.
    fn query(&mut self, py: Python, cypher: &str) -> PyResult<Vec<PyObject>> {
        let cypher = cypher.to_string();
        let inner = &mut self.inner;
        let records = py
            .allow_threads(|| {
                let tx = inner.begin_read()?;
                let r = tx.query(&cypher)?;
                tx.commit()?;
                Ok::<_, crate::types::GraphError>(r)
            })
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        records_to_py(py, &records)
    }

    /// Execute a write Cypher query (CREATE, DELETE, SET, MERGE). Returns a list of dicts.
    fn execute(&mut self, py: Python, cypher: &str) -> PyResult<Vec<PyObject>> {
        let cypher = cypher.to_string();
        let inner = &mut self.inner;
        let records = py
            .allow_threads(|| {
                let tx = inner.begin_write()?;
                let r = tx.query(&cypher)?;
                tx.commit()?;
                Ok::<_, crate::types::GraphError>(r)
            })
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        records_to_py(py, &records)
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

/// Python module definition.
#[pymodule]
pub fn _graphdblite(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyDatabase>()?;
    Ok(())
}
