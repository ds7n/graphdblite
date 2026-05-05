//! Node.js bindings for graphdblite via napi-rs.

use napi::bindgen_prelude::*;
use napi::{NapiRaw, NapiValue};
use napi_derive::napi;
use std::sync::{Arc, Mutex};

use graphdblite::{Config, Database as RustDatabase, GraphError, Record, Value};

/// Shared inner state — `None` once `Database.close()` runs. Wrapped in
/// `Arc<Mutex<>>` so the `Database` JS object and any active
/// `WriteTransaction` / `ReadTransaction` operate on the same underlying
/// `RustDatabase` without ownership transfer.
type SharedDb = Arc<Mutex<Option<RustDatabase>>>;

fn closed_err() -> napi::Error {
    napi::Error::from_reason("database is closed")
}

fn finished_err() -> napi::Error {
    napi::Error::from_reason("transaction is finished")
}

fn to_napi_err(e: GraphError) -> napi::Error {
    napi::Error::from_reason(e.to_string())
}

/// Convert an Array to JsUnknown via raw napi pointer.
fn array_to_unknown(env: &Env, arr: &napi::JsObject) -> Result<napi::JsUnknown> {
    unsafe { napi::JsUnknown::from_raw(env.raw(), arr.raw()) }
}

fn value_to_napi(env: &Env, val: &Value) -> Result<napi::JsUnknown> {
    match val {
        Value::Null => env.get_null().map(|v| v.into_unknown()),
        Value::Bool(b) => env.get_boolean(*b).map(|v| v.into_unknown()),
        Value::I64(n) => env.create_int64(*n).map(|v| v.into_unknown()),
        Value::F64(n) => env.create_double(*n).map(|v| v.into_unknown()),
        Value::String(s) => env.create_string(s).map(|v| v.into_unknown()),
        Value::List(items) => {
            let mut arr = env.create_array_with_length(items.len())?;
            for (i, item) in items.iter().enumerate() {
                arr.set_element(i as u32, value_to_napi(env, item)?)?;
            }
            array_to_unknown(env, &arr)
        }
        Value::Path(p) => {
            let mut arr = env.create_array_with_length(p.nodes.len())?;
            for (i, n) in p.nodes.iter().enumerate() {
                arr.set_element(i as u32, env.create_int64(n.id.0 as i64)?)?;
            }
            array_to_unknown(env, &arr)
        }
        Value::Node(n) => {
            let mut obj = env.create_object()?;
            obj.set("__id", env.create_int64(n.id.0 as i64)?)?;
            let mut labels_arr = env.create_array_with_length(n.labels.len())?;
            for (i, label) in n.labels.iter().enumerate() {
                labels_arr.set_element(i as u32, env.create_string(label)?)?;
            }
            obj.set("__labels", labels_arr)?;
            for (k, v) in &n.properties {
                obj.set(k.as_str(), value_to_napi(env, v)?)?;
            }
            Ok(obj.into_unknown())
        }
        Value::Edge(e) => {
            let mut obj = env.create_object()?;
            obj.set("__src", env.create_int64(e.src.0 as i64)?)?;
            obj.set("__dst", env.create_int64(e.dst.0 as i64)?)?;
            obj.set("__label", env.create_string(&e.label)?)?;
            for (k, v) in &e.properties {
                obj.set(k.as_str(), value_to_napi(env, v)?)?;
            }
            Ok(obj.into_unknown())
        }
        Value::Map(map) => {
            let mut obj = env.create_object()?;
            for (k, v) in map {
                obj.set(k.as_str(), value_to_napi(env, v)?)?;
            }
            Ok(obj.into_unknown())
        }
        // Temporal types — expose as ISO string.
        other => env
            .create_string(&format!("{other}"))
            .map(|v| v.into_unknown()),
    }
}

fn records_to_napi(env: &Env, records: &[Record]) -> Result<Vec<napi::JsObject>> {
    let mut result = Vec::with_capacity(records.len());
    for rec in records {
        let mut obj = env.create_object()?;
        for (key, val) in &rec.fields {
            obj.set(key.as_str(), value_to_napi(env, val)?)?;
        }
        result.push(obj);
    }
    Ok(result)
}

/// An embedded graph database with Cypher query support.
#[napi]
pub struct Database {
    inner: SharedDb,
}

impl Database {
    fn new_inner(db: RustDatabase) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Some(db))),
        }
    }
}

#[napi]
impl Database {
    /// Open a database at the given file path.
    #[napi(constructor)]
    pub fn new(path: String) -> Result<Self> {
        let db = RustDatabase::open(&path).map_err(to_napi_err)?;
        Ok(Self::new_inner(db))
    }

    /// Open a database with a custom busy timeout (milliseconds).
    #[napi(factory)]
    pub fn open_with_timeout(path: String, busy_timeout_ms: u32) -> Result<Self> {
        let config = Config {
            busy_timeout_ms,
            ..Config::default()
        };
        let db = RustDatabase::open_with_config(&path, config).map_err(to_napi_err)?;
        Ok(Self::new_inner(db))
    }

    /// Open an in-memory database (for testing).
    #[napi(factory)]
    pub fn open_memory() -> Result<Self> {
        let db = RustDatabase::open_memory().map_err(to_napi_err)?;
        Ok(Self::new_inner(db))
    }

    /// Execute a read-only Cypher query. Returns an array of objects.
    #[napi]
    pub fn query(&self, env: Env, cypher: String) -> Result<Vec<napi::JsObject>> {
        let mut guard = self.inner.lock().expect("Database mutex poisoned");
        let db = guard.as_mut().ok_or_else(closed_err)?;
        let tx = db.read_tx().map_err(to_napi_err)?;
        let records = tx.query(&cypher).map_err(to_napi_err)?;
        tx.commit().map_err(to_napi_err)?;
        records_to_napi(&env, &records)
    }

    /// Execute a write Cypher query (CREATE, DELETE, SET, MERGE). Returns an array of objects.
    #[napi]
    pub fn execute(&self, env: Env, cypher: String) -> Result<Vec<napi::JsObject>> {
        let mut guard = self.inner.lock().expect("Database mutex poisoned");
        let db = guard.as_mut().ok_or_else(closed_err)?;
        let tx = db.write_tx().map_err(to_napi_err)?;
        let records = tx.query(&cypher).map_err(to_napi_err)?;
        tx.commit().map_err(to_napi_err)?;
        records_to_napi(&env, &records)
    }

    /// Begin a read-write transaction. Pair with `commit()` or `rollback()`.
    /// On Node 22+ the returned object is also `Symbol.dispose`-friendly via
    /// the `using` syntax (rolls back on scope exit).
    #[napi]
    pub fn begin_write(&self) -> Result<WriteTransaction> {
        let mut guard = self.inner.lock().expect("Database mutex poisoned");
        let db = guard.as_mut().ok_or_else(closed_err)?;
        db.begin_write().map_err(to_napi_err)?;
        Ok(WriteTransaction {
            inner: Arc::clone(&self.inner),
            finished: false,
        })
    }

    /// Begin a read transaction. Pair with `commit()`.
    #[napi]
    pub fn begin_read(&self) -> Result<ReadTransaction> {
        let mut guard = self.inner.lock().expect("Database mutex poisoned");
        let db = guard.as_mut().ok_or_else(closed_err)?;
        db.begin_read().map_err(to_napi_err)?;
        Ok(ReadTransaction {
            inner: Arc::clone(&self.inner),
            finished: false,
        })
    }

    /// Close the database connection.
    #[napi]
    pub fn close(&self) {
        if let Ok(mut guard) = self.inner.lock() {
            *guard = None;
        }
    }
}

/// A read-write transaction. Created via `Database.beginWrite()`.
#[napi]
pub struct WriteTransaction {
    inner: SharedDb,
    finished: bool,
}

#[napi]
impl WriteTransaction {
    /// Execute a Cypher query within this transaction.
    #[napi]
    pub fn execute(&mut self, env: Env, cypher: String) -> Result<Vec<napi::JsObject>> {
        if self.finished {
            return Err(finished_err());
        }
        let mut guard = self.inner.lock().expect("Database mutex poisoned");
        let db = guard.as_mut().ok_or_else(closed_err)?;
        let records = db.execute(&cypher).map_err(to_napi_err)?;
        records_to_napi(&env, &records)
    }

    /// Execute a Cypher query within this transaction (alias for `execute`).
    #[napi]
    pub fn query(&mut self, env: Env, cypher: String) -> Result<Vec<napi::JsObject>> {
        self.execute(env, cypher)
    }

    /// Commit the transaction.
    #[napi]
    pub fn commit(&mut self) -> Result<()> {
        if self.finished {
            return Err(finished_err());
        }
        let mut guard = self.inner.lock().expect("Database mutex poisoned");
        let db = guard.as_mut().ok_or_else(closed_err)?;
        db.commit().map_err(to_napi_err)?;
        self.finished = true;
        Ok(())
    }

    /// Rollback the transaction.
    #[napi]
    pub fn rollback(&mut self) -> Result<()> {
        if self.finished {
            return Err(finished_err());
        }
        let mut guard = self.inner.lock().expect("Database mutex poisoned");
        let db = guard.as_mut().ok_or_else(closed_err)?;
        db.rollback().map_err(to_napi_err)?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for WriteTransaction {
    fn drop(&mut self) {
        if !self.finished {
            if let Ok(mut guard) = self.inner.lock() {
                if let Some(db) = guard.as_mut() {
                    let _ = db.rollback();
                }
            }
        }
    }
}

/// A read-only transaction. Created via `Database.beginRead()`.
#[napi]
pub struct ReadTransaction {
    inner: SharedDb,
    finished: bool,
}

#[napi]
impl ReadTransaction {
    /// Execute a read-only Cypher query within this transaction.
    #[napi]
    pub fn query(&mut self, env: Env, cypher: String) -> Result<Vec<napi::JsObject>> {
        if self.finished {
            return Err(finished_err());
        }
        let mut guard = self.inner.lock().expect("Database mutex poisoned");
        let db = guard.as_mut().ok_or_else(closed_err)?;
        let records = db.execute(&cypher).map_err(to_napi_err)?;
        records_to_napi(&env, &records)
    }

    /// Commit (release) the read transaction.
    #[napi]
    pub fn commit(&mut self) -> Result<()> {
        if self.finished {
            return Err(finished_err());
        }
        let mut guard = self.inner.lock().expect("Database mutex poisoned");
        let db = guard.as_mut().ok_or_else(closed_err)?;
        db.commit().map_err(to_napi_err)?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for ReadTransaction {
    fn drop(&mut self) {
        if !self.finished {
            if let Ok(mut guard) = self.inner.lock() {
                if let Some(db) = guard.as_mut() {
                    let _ = db.rollback();
                }
            }
        }
    }
}
