//! Node.js bindings for graphdblite via napi-rs.

use napi::bindgen_prelude::*;
use napi::{NapiRaw, NapiValue};
use napi_derive::napi;

use graphdblite::cypher::{executor, parser, planner, record::Record};
use graphdblite::{Config, Database as RustDatabase, GraphError, Value};

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
    inner: Option<RustDatabase>,
}

#[napi]
impl Database {
    /// Open a database at the given file path.
    #[napi(constructor)]
    pub fn new(path: String) -> Result<Self> {
        let db = RustDatabase::open(&path).map_err(to_napi_err)?;
        Ok(Self { inner: Some(db) })
    }

    /// Open a database with a custom busy timeout (milliseconds).
    #[napi(factory)]
    pub fn open_with_timeout(path: String, busy_timeout_ms: u32) -> Result<Self> {
        let config = Config {
            busy_timeout_ms,
            ..Config::default()
        };
        let db = RustDatabase::open_with_config(&path, config).map_err(to_napi_err)?;
        Ok(Self { inner: Some(db) })
    }

    /// Open an in-memory database (for testing).
    #[napi(factory)]
    pub fn open_memory() -> Result<Self> {
        let db = RustDatabase::open_memory().map_err(to_napi_err)?;
        Ok(Self { inner: Some(db) })
    }

    /// Execute a read-only Cypher query. Returns an array of objects.
    #[napi]
    pub fn query(&mut self, env: Env, cypher: String) -> Result<Vec<napi::JsObject>> {
        let db = self
            .inner
            .as_mut()
            .ok_or_else(|| napi::Error::from_reason("database is closed"))?;
        let tx = db.begin_read().map_err(to_napi_err)?;
        let records = tx.query(&cypher).map_err(to_napi_err)?;
        tx.commit().map_err(to_napi_err)?;
        records_to_napi(&env, &records)
    }

    /// Execute a write Cypher query (CREATE, DELETE, SET, MERGE). Returns an array of objects.
    #[napi]
    pub fn execute(&mut self, env: Env, cypher: String) -> Result<Vec<napi::JsObject>> {
        let db = self
            .inner
            .as_mut()
            .ok_or_else(|| napi::Error::from_reason("database is closed"))?;
        let tx = db.begin_write().map_err(to_napi_err)?;
        let records = tx.query(&cypher).map_err(to_napi_err)?;
        tx.commit().map_err(to_napi_err)?;
        records_to_napi(&env, &records)
    }

    /// Close the database connection.
    #[napi]
    pub fn close(&mut self) {
        self.inner.take();
    }
}

/// A read-write transaction. Created via Database.beginWrite().
#[napi]
pub struct WriteTransaction {
    db: Option<RustDatabase>,
}

#[napi]
impl WriteTransaction {
    /// Execute a Cypher query within this transaction.
    #[napi]
    pub fn execute(&self, env: Env, cypher: String) -> Result<Vec<napi::JsObject>> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| napi::Error::from_reason("transaction is finished"))?;
        let conn = db.connection();
        let stmt = parser::parse(&cypher).map_err(to_napi_err)?;
        let plan = planner::plan(conn, &stmt).map_err(to_napi_err)?;
        let ctx = executor::ExecContext {
            max_result_rows: db.max_result_rows,
            ..Default::default()
        };
        let records = executor::execute_with_ctx(conn, &plan, &ctx).map_err(to_napi_err)?;
        records_to_napi(&env, &records)
    }

    /// Execute a read-only Cypher query within this transaction.
    #[napi]
    pub fn query(&self, env: Env, cypher: String) -> Result<Vec<napi::JsObject>> {
        self.execute(env, cypher)
    }

    /// Commit the transaction.
    #[napi]
    pub fn commit(&mut self) -> Result<()> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| napi::Error::from_reason("transaction is finished"))?;
        db.connection()
            .execute_batch("COMMIT")
            .map_err(|e| napi::Error::from_reason(format!("commit failed: {e}")))?;
        self.db = None;
        Ok(())
    }

    /// Rollback the transaction.
    #[napi]
    pub fn rollback(&mut self) -> Result<()> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| napi::Error::from_reason("transaction is finished"))?;
        let _ = db.connection().execute_batch("ROLLBACK");
        self.db = None;
        Ok(())
    }
}

/// A read-only transaction. Created via Database.beginRead().
#[napi]
pub struct ReadTransaction {
    db: Option<RustDatabase>,
}

#[napi]
impl ReadTransaction {
    /// Execute a read-only Cypher query within this transaction.
    #[napi]
    pub fn query(&self, env: Env, cypher: String) -> Result<Vec<napi::JsObject>> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| napi::Error::from_reason("transaction is finished"))?;
        let conn = db.connection();
        let stmt = parser::parse(&cypher).map_err(to_napi_err)?;
        let plan = planner::plan(conn, &stmt).map_err(to_napi_err)?;
        let ctx = executor::ExecContext {
            max_result_rows: db.max_result_rows,
            ..Default::default()
        };
        let records = executor::execute_with_ctx(conn, &plan, &ctx).map_err(to_napi_err)?;
        records_to_napi(&env, &records)
    }

    /// Commit (release) the read transaction.
    #[napi]
    pub fn commit(&mut self) -> Result<()> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| napi::Error::from_reason("transaction is finished"))?;
        db.connection()
            .execute_batch("COMMIT")
            .map_err(|e| napi::Error::from_reason(format!("commit failed: {e}")))?;
        self.db = None;
        Ok(())
    }
}
