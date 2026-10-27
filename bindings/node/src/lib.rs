// SPDX-License-Identifier: MIT
// Copyright (c) 2026 ds7n

//! Node.js bindings for graphdblite via napi-rs.

use napi::bindgen_prelude::*;
use napi::{JsBigInt, JsBoolean, JsNumber, JsString, NapiRaw, NapiValue, ValueType};
use napi_derive::napi;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use graphdblite::{Config, Database as RustDatabase, GraphError, Record, Value};

/// State shared by a `Database` JS object and its transactions.
struct State {
    /// `None` once `Database.close()` runs.
    db: Option<RustDatabase>,
    /// Id of the transaction object that owns the open transaction, if any.
    /// A transaction object whose id is not this one is finished: it can
    /// never commit, roll back, or run statements in someone else's
    /// transaction.
    active_tx: Option<u64>,
    /// Source of transaction ids.
    next_tx_id: u64,
}

/// Shared inner state, wrapped in `Arc<Mutex<>>` so the `Database` JS
/// object and any active `WriteTransaction` / `ReadTransaction` operate on
/// the same underlying `RustDatabase` without ownership transfer.
type SharedDb = Arc<Mutex<State>>;

/// Lock the shared state.
///
/// Every exported method runs under `#[napi(catch_unwind)]`, which turns a
/// Rust panic into a JS exception instead of aborting the process. A panic
/// that unwinds while this lock is held poisons the mutex; recover the
/// guard, clear the poison and roll back the transaction the panic
/// interrupted, so the handle keeps working instead of failing forever.
fn lock_db(inner: &SharedDb) -> std::sync::MutexGuard<'_, State> {
    inner.lock().unwrap_or_else(|poisoned| {
        inner.clear_poison();
        let mut guard = poisoned.into_inner();
        if let Some(db) = guard.db.as_mut() {
            // No transaction open is fine: there is nothing to undo.
            let _ = db.rollback();
        }
        guard.active_tx = None;
        guard
    })
}

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

/// Largest integer a JS number holds exactly (`Number.MAX_SAFE_INTEGER`).
const MAX_SAFE_INTEGER: i64 = (1 << 53) - 1;

/// An integer as a JS number when it is exactly representable, else as a
/// BigInt (so no precision is lost).
fn int_to_napi(env: &Env, n: i64) -> Result<napi::JsUnknown> {
    if (-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&n) {
        env.create_int64(n).map(|v| v.into_unknown())
    } else {
        env.create_bigint_from_i64(n)?.into_unknown()
    }
}

/// Metadata keys of a node object. A user property with one of these names
/// is left out: metadata wins.
const NODE_META_KEYS: [&str; 2] = ["__id", "__labels"];

/// Metadata keys of a relationship object. `__label` is a deprecated alias
/// of `__type`, kept for 0.1.x callers.
const EDGE_META_KEYS: [&str; 5] = ["__src", "__dst", "__type", "__label", "__edge_seq"];

fn set_props(
    env: &Env,
    obj: &mut napi::JsObject,
    props: &graphdblite::Properties,
    meta: &[&str],
) -> Result<()> {
    for (k, v) in props {
        if meta.contains(&k.as_str()) {
            continue;
        }
        obj.set(k.as_str(), value_to_napi(env, v)?)?;
    }
    Ok(())
}

fn node_to_napi(env: &Env, n: &graphdblite::Node) -> Result<napi::JsObject> {
    let mut obj = env.create_object()?;
    obj.set("__id", int_to_napi(env, n.id.0 as i64)?)?;
    let mut labels_arr = env.create_array_with_length(n.labels.len())?;
    for (i, label) in n.labels.iter().enumerate() {
        labels_arr.set_element(i as u32, env.create_string(label)?)?;
    }
    obj.set("__labels", labels_arr)?;
    set_props(env, &mut obj, &n.properties, &NODE_META_KEYS)?;
    Ok(obj)
}

fn edge_to_napi(env: &Env, e: &graphdblite::Edge) -> Result<napi::JsObject> {
    let mut obj = env.create_object()?;
    obj.set("__src", int_to_napi(env, e.src.0 as i64)?)?;
    obj.set("__dst", int_to_napi(env, e.dst.0 as i64)?)?;
    obj.set("__type", env.create_string(&e.label)?)?;
    obj.set("__label", env.create_string(&e.label)?)?;
    obj.set("__edge_seq", int_to_napi(env, e.seq as i64)?)?;
    set_props(env, &mut obj, &e.properties, &EDGE_META_KEYS)?;
    Ok(obj)
}

fn value_to_napi(env: &Env, val: &Value) -> Result<napi::JsUnknown> {
    match val {
        Value::Null => env.get_null().map(|v| v.into_unknown()),
        Value::Bool(b) => env.get_boolean(*b).map(|v| v.into_unknown()),
        Value::I64(n) => int_to_napi(env, *n),
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
            let mut nodes = env.create_array_with_length(p.nodes.len())?;
            for (i, n) in p.nodes.iter().enumerate() {
                nodes.set_element(i as u32, node_to_napi(env, n)?)?;
            }
            let mut rels = env.create_array_with_length(p.edges.len())?;
            for (i, e) in p.edges.iter().enumerate() {
                rels.set_element(i as u32, edge_to_napi(env, e)?)?;
            }
            let mut obj = env.create_object()?;
            obj.set("nodes", nodes)?;
            obj.set("relationships", rels)?;
            Ok(obj.into_unknown())
        }
        Value::Node(n) => Ok(node_to_napi(env, n)?.into_unknown()),
        Value::Edge(e) => Ok(edge_to_napi(env, e)?.into_unknown()),
        Value::Map(map) => {
            let mut obj = env.create_object()?;
            for (k, v) in map {
                obj.set(k.as_str(), value_to_napi(env, v)?)?;
            }
            Ok(obj.into_unknown())
        }
        // Temporal types: exposed as ISO strings.
        other => env
            .create_string(&format!("{other}"))
            .map(|v| v.into_unknown()),
    }
}

fn records_to_napi(env: &Env, records: &[Record]) -> Result<Vec<napi::JsObject>> {
    let mut result = Vec::with_capacity(records.len());
    for rec in records {
        let mut obj = env.create_object()?;
        for (key, val) in rec.iter() {
            obj.set(key.as_str(), value_to_napi(env, val)?)?;
        }
        result.push(obj);
    }
    Ok(result)
}

/// A query-parameter error, naming the parameter.
fn param_err(name: &str, what: &str) -> napi::Error {
    napi::Error::new(Status::InvalidArg, format!("parameter `{name}`: {what}"))
}

/// Convert a JS value to a Cypher value for a query parameter.
///
/// null / undefined -> null, boolean, number (an integer-valued number
/// within the safe-integer range becomes an integer, any other number a
/// float), bigint (must fit a signed 64-bit integer), string, Array ->
/// list, other objects -> map of their enumerable properties. Functions,
/// symbols and Date objects are rejected.
fn napi_to_value(name: &str, val: napi::JsUnknown) -> Result<Value> {
    napi_to_value_at(name, val, 0)
}

/// Deepest array/object nesting accepted in a query parameter. A deeper (or
/// self-referencing) structure would overflow the stack in the recursive
/// conversion, which no `catch_unwind` can contain.
const MAX_VALUE_DEPTH: usize = 128;

/// [`napi_to_value`] with the current array/object nesting depth.
fn napi_to_value_at(name: &str, val: napi::JsUnknown, depth: usize) -> Result<Value> {
    if depth > MAX_VALUE_DEPTH {
        return Err(param_err(
            name,
            &format!("nesting exceeds the maximum depth of {MAX_VALUE_DEPTH}"),
        ));
    }
    Ok(match val.get_type()? {
        ValueType::Null | ValueType::Undefined => Value::Null,
        ValueType::Boolean => {
            let b: JsBoolean = val.try_into()?;
            Value::Bool(b.get_value()?)
        }
        ValueType::Number => {
            let n: JsNumber = val.try_into()?;
            let f = n.get_double()?;
            if f.fract() == 0.0 && f.abs() <= MAX_SAFE_INTEGER as f64 {
                Value::I64(f as i64)
            } else {
                Value::F64(f)
            }
        }
        ValueType::BigInt => {
            let b: JsBigInt = unsafe { val.cast() };
            let (n, lossless) = b.get_i64()?;
            if !lossless {
                return Err(param_err(name, "BigInt outside the signed 64-bit range"));
            }
            Value::I64(n)
        }
        ValueType::String => {
            let s: JsString = val.try_into()?;
            Value::String(s.into_utf8()?.into_owned()?)
        }
        ValueType::Object => {
            let obj: napi::JsObject = val.try_into()?;
            if obj.is_date()? {
                return Err(param_err(
                    name,
                    "Date is not supported; pass an ISO string and convert in Cypher",
                ));
            }
            if obj.is_array()? {
                let len = obj.get_array_length()?;
                let mut items = Vec::with_capacity(len as usize);
                for i in 0..len {
                    items.push(napi_to_value_at(
                        name,
                        obj.get_element::<napi::JsUnknown>(i)?,
                        depth + 1,
                    )?);
                }
                Value::List(items)
            } else {
                let keys = obj.get_property_names()?;
                let len = keys.get_array_length()?;
                let mut map = BTreeMap::new();
                for i in 0..len {
                    let key: JsString = keys.get_element(i)?;
                    let key = key.into_utf8()?.into_owned()?;
                    let v = obj.get_named_property::<napi::JsUnknown>(&key)?;
                    map.insert(key, napi_to_value_at(name, v, depth + 1)?);
                }
                Value::Map(map)
            }
        }
        other => {
            return Err(param_err(name, &format!("unsupported type {other}")));
        }
    })
}

/// Convert an optional JS params object to named Cypher values.
fn params_to_map(params: Option<napi::JsObject>) -> Result<Option<HashMap<String, Value>>> {
    let Some(obj) = params else {
        return Ok(None);
    };
    let keys = obj.get_property_names()?;
    let len = keys.get_array_length()?;
    let mut map = HashMap::with_capacity(len as usize);
    for i in 0..len {
        let key: JsString = keys.get_element(i)?;
        let key = key.into_utf8()?.into_owned()?;
        let v = obj.get_named_property::<napi::JsUnknown>(&key)?;
        let value = napi_to_value(&key, v)?;
        map.insert(key, value);
    }
    Ok(Some(map))
}

/// An embedded graph database with Cypher query support.
#[napi]
pub struct Database {
    inner: SharedDb,
}

impl Database {
    fn new_inner(db: RustDatabase) -> Self {
        Self {
            inner: Arc::new(Mutex::new(State {
                db: Some(db),
                active_tx: None,
                next_tx_id: 0,
            })),
        }
    }

    /// Begin a transaction and register a new transaction object as its
    /// owner.
    fn begin(&self, write: bool) -> Result<TxCore> {
        let mut guard = lock_db(&self.inner);
        let state = &mut *guard;
        let db = state.db.as_mut().ok_or_else(closed_err)?;
        if write {
            db.begin_write().map_err(to_napi_err)?;
        } else {
            db.begin_read().map_err(to_napi_err)?;
        }
        state.next_tx_id += 1;
        state.active_tx = Some(state.next_tx_id);
        Ok(TxCore {
            inner: Arc::clone(&self.inner),
            id: state.next_tx_id,
            finished: false,
        })
    }
}

#[napi]
impl Database {
    /// Open a database at the given file path. The path `:memory:` opens a
    /// private in-memory database.
    #[napi(constructor, catch_unwind)]
    pub fn new(path: String) -> Result<Self> {
        let db = RustDatabase::open(&path).map_err(to_napi_err)?;
        Ok(Self::new_inner(db))
    }

    /// Open a database with a custom busy timeout (milliseconds).
    #[napi(factory, catch_unwind)]
    pub fn open_with_timeout(path: String, busy_timeout_ms: u32) -> Result<Self> {
        let config = Config {
            busy_timeout_ms,
            ..Config::default()
        };
        let db = RustDatabase::open_with_config(&path, config).map_err(to_napi_err)?;
        Ok(Self::new_inner(db))
    }

    /// Open an in-memory database (for testing).
    #[napi(factory, catch_unwind)]
    pub fn open_memory() -> Result<Self> {
        let db = RustDatabase::open_memory().map_err(to_napi_err)?;
        Ok(Self::new_inner(db))
    }

    /// Execute a read-only Cypher query, with optional `$name` parameters.
    /// Returns an array of objects.
    #[napi(
        catch_unwind,
        ts_args_type = "cypher: string, params?: Record<string, unknown>"
    )]
    pub fn query(
        &self,
        env: Env,
        cypher: String,
        params: Option<napi::JsObject>,
    ) -> Result<Vec<napi::JsObject>> {
        let params = params_to_map(params)?;
        let mut guard = lock_db(&self.inner);
        let db = guard.db.as_mut().ok_or_else(closed_err)?;
        let tx = db.read_tx().map_err(to_napi_err)?;
        let records = tx
            .query_with_params(&cypher, params.as_ref())
            .map_err(to_napi_err)?;
        tx.commit().map_err(to_napi_err)?;
        records_to_napi(&env, &records)
    }

    /// Execute a write Cypher query (CREATE, DELETE, SET, MERGE), with
    /// optional `$name` parameters. Returns an array of objects.
    #[napi(
        catch_unwind,
        ts_args_type = "cypher: string, params?: Record<string, unknown>"
    )]
    pub fn execute(
        &self,
        env: Env,
        cypher: String,
        params: Option<napi::JsObject>,
    ) -> Result<Vec<napi::JsObject>> {
        let params = params_to_map(params)?;
        let mut guard = lock_db(&self.inner);
        let db = guard.db.as_mut().ok_or_else(closed_err)?;
        let tx = db.write_tx().map_err(to_napi_err)?;
        let records = tx
            .query_with_params(&cypher, params.as_ref())
            .map_err(to_napi_err)?;
        tx.commit().map_err(to_napi_err)?;
        records_to_napi(&env, &records)
    }

    /// Begin a read-write transaction. Pair with `commit()` or `rollback()`.
    /// On Node 22+ the returned object is also `Symbol.dispose`-friendly via
    /// the `using` syntax (rolls back on scope exit).
    #[napi(catch_unwind)]
    pub fn begin_write(&self) -> Result<WriteTransaction> {
        Ok(WriteTransaction {
            core: self.begin(true)?,
        })
    }

    /// Begin a read transaction. Pair with `commit()` or `rollback()`.
    #[napi(catch_unwind)]
    pub fn begin_read(&self) -> Result<ReadTransaction> {
        Ok(ReadTransaction {
            core: self.begin(false)?,
        })
    }

    /// Write a consistent single-file snapshot of this database to `path`.
    ///
    /// Uses SQLite's `VACUUM INTO`: produces a self-contained file (no
    /// `-wal` / `-shm` sidecars), defragmented and compacted. Throws when a
    /// transaction is active on this handle or when `path` already exists.
    #[napi(catch_unwind)]
    pub fn snapshot_to(&self, path: String) -> Result<()> {
        let mut guard = lock_db(&self.inner);
        let db = guard.db.as_mut().ok_or_else(closed_err)?;
        db.snapshot_to(&path).map_err(to_napi_err)
    }

    /// Close the database connection. An open transaction is rolled back.
    #[napi(catch_unwind)]
    pub fn close(&self) {
        let mut guard = lock_db(&self.inner);
        if let Some(mut db) = guard.db.take() {
            if guard.active_tx.take().is_some() {
                // The connection closes right after, which discards the
                // transaction even if this rollback fails.
                let _ = db.rollback();
            }
        }
    }
}

/// State shared by the two transaction classes: which database, and which
/// of its transactions this object owns.
struct TxCore {
    inner: SharedDb,
    id: u64,
    finished: bool,
}

impl TxCore {
    /// Run `op` on the database inside this transaction.
    ///
    /// Errors when this transaction is finished, its database is closed, or
    /// its transaction is no longer the open one. Afterwards, if no
    /// transaction is open any more (committed, rolled back, ended by
    /// SQLite), this object is marked finished.
    fn run<T>(
        &mut self,
        op: impl FnOnce(&mut RustDatabase) -> std::result::Result<T, GraphError>,
    ) -> Result<T> {
        if self.finished {
            return Err(finished_err());
        }
        let mut guard = lock_db(&self.inner);
        let state = &mut *guard;
        let Some(db) = state.db.as_mut() else {
            self.finished = true;
            return Err(closed_err());
        };
        if state.active_tx != Some(self.id) {
            self.finished = true;
            return Err(finished_err());
        }
        let result = op(db).map_err(to_napi_err);
        if !db.in_transaction() {
            state.active_tx = None;
            self.finished = true;
        }
        result
    }

    fn execute(
        &mut self,
        env: Env,
        cypher: String,
        params: Option<napi::JsObject>,
    ) -> Result<Vec<napi::JsObject>> {
        let params = params_to_map(params)?;
        let records = self.run(|db| db.execute_with_params(&cypher, params.as_ref()))?;
        records_to_napi(&env, &records)
    }
}

impl Drop for TxCore {
    /// A transaction object garbage-collected without commit/rollback rolls
    /// back, but only if it still owns the open transaction.
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let mut guard = lock_db(&self.inner);
        let state = &mut *guard;
        if state.active_tx == Some(self.id) {
            if let Some(db) = state.db.as_mut() {
                let _ = db.rollback();
            }
            state.active_tx = None;
        }
    }
}

/// A read-write transaction. Created via `Database.beginWrite()`.
///
/// A statement that fails is rolled back on its own; the transaction stays
/// open.
#[napi]
pub struct WriteTransaction {
    core: TxCore,
}

#[napi]
impl WriteTransaction {
    /// Execute a Cypher query within this transaction, with optional
    /// `$name` parameters.
    #[napi(
        catch_unwind,
        ts_args_type = "cypher: string, params?: Record<string, unknown>"
    )]
    pub fn execute(
        &mut self,
        env: Env,
        cypher: String,
        params: Option<napi::JsObject>,
    ) -> Result<Vec<napi::JsObject>> {
        self.core.execute(env, cypher, params)
    }

    /// Execute a Cypher query within this transaction (alias for `execute`).
    #[napi(
        catch_unwind,
        ts_args_type = "cypher: string, params?: Record<string, unknown>"
    )]
    pub fn query(
        &mut self,
        env: Env,
        cypher: String,
        params: Option<napi::JsObject>,
    ) -> Result<Vec<napi::JsObject>> {
        self.core.execute(env, cypher, params)
    }

    /// Create a secondary index on (label, property) for faster lookups.
    #[napi(catch_unwind)]
    pub fn create_index(&mut self, label: String, property: String) -> Result<()> {
        self.core.run(|db| db.create_index(&label, &property))
    }

    /// Drop a secondary index on (label, property).
    #[napi(catch_unwind)]
    pub fn drop_index(&mut self, label: String, property: String) -> Result<()> {
        self.core.run(|db| db.drop_index(&label, &property))
    }

    /// Create a fulltext index on (label, property). Accelerates
    /// CONTAINS / STARTS WITH / ENDS WITH via SQLite FTS5 (trigram, case-sensitive).
    #[napi(catch_unwind)]
    pub fn create_fulltext_index(&mut self, label: String, property: String) -> Result<()> {
        self.core
            .run(|db| db.create_fulltext_index(&label, &property))
    }

    /// Create a case-insensitive fulltext index on (label, property).
    /// Uses SQLite FTS5 trigram tokenizer with case_sensitive=0. It
    /// accelerates the case-insensitive idiom
    /// `toLower(n.prop) CONTAINS toLower($q)` (and STARTS WITH / ENDS WITH);
    /// plain predicates keep Cypher's case-sensitive semantics.
    #[napi(catch_unwind)]
    pub fn create_fulltext_index_ci(&mut self, label: String, property: String) -> Result<()> {
        self.core
            .run(|db| db.create_fulltext_index_ci(&label, &property))
    }

    /// Create a word-tokenized fulltext index on (label, property).
    /// Uses SQLite FTS5 `unicode61` tokenizer, intended for use with
    /// the `fts.search` CALL procedure (bm25 ranking on word matches).
    /// Does NOT accelerate `CONTAINS` / `STARTS WITH` / `ENDS WITH`.
    #[napi(catch_unwind)]
    pub fn create_fulltext_index_word(&mut self, label: String, property: String) -> Result<()> {
        self.core
            .run(|db| db.create_fulltext_index_word(&label, &property))
    }

    /// Create a word-tokenized fulltext index covering multiple
    /// properties on a label. Use with `CALL fts.search(label, '*', query)`
    /// to search across all covered properties, or
    /// `CALL fts.search(label, property, query)` to scope to one.
    #[napi(catch_unwind)]
    pub fn create_fulltext_index_word_multi(
        &mut self,
        label: String,
        properties: Vec<String>,
    ) -> Result<()> {
        self.core
            .run(|db| db.create_fulltext_index_word_multi(&label, &properties))
    }

    /// Drop a fulltext index on (label, property).
    #[napi(catch_unwind)]
    pub fn drop_fulltext_index(&mut self, label: String, property: String) -> Result<()> {
        self.core
            .run(|db| db.drop_fulltext_index(&label, &property))
    }

    /// Create a composite (multi-property) secondary index on (label, properties).
    /// Accelerates equality lookups on all listed properties simultaneously.
    #[napi(catch_unwind)]
    pub fn create_composite_index(&mut self, label: String, properties: Vec<String>) -> Result<()> {
        let refs: Vec<&str> = properties.iter().map(String::as_str).collect();
        self.core.run(|db| db.create_composite_index(&label, &refs))
    }

    /// Drop a composite (multi-property) secondary index on (label, properties).
    #[napi(catch_unwind)]
    pub fn drop_composite_index(&mut self, label: String, properties: Vec<String>) -> Result<()> {
        let refs: Vec<&str> = properties.iter().map(String::as_str).collect();
        self.core.run(|db| db.drop_composite_index(&label, &refs))
    }

    /// Commit the transaction. If the commit fails the transaction stays
    /// open, so it can still be rolled back.
    #[napi(catch_unwind)]
    pub fn commit(&mut self) -> Result<()> {
        self.core.run(|db| db.commit())
    }

    /// Rollback the transaction.
    #[napi(catch_unwind)]
    pub fn rollback(&mut self) -> Result<()> {
        let result = self.core.run(|db| db.rollback());
        // Finished either way: a failed rollback leaves nothing to retry.
        self.core.finished = true;
        result
    }
}

/// A read-only transaction. Created via `Database.beginRead()`.
#[napi]
pub struct ReadTransaction {
    core: TxCore,
}

#[napi]
impl ReadTransaction {
    /// Execute a read-only Cypher query within this transaction, with
    /// optional `$name` parameters.
    #[napi(
        catch_unwind,
        ts_args_type = "cypher: string, params?: Record<string, unknown>"
    )]
    pub fn query(
        &mut self,
        env: Env,
        cypher: String,
        params: Option<napi::JsObject>,
    ) -> Result<Vec<napi::JsObject>> {
        self.core.execute(env, cypher, params)
    }

    /// Commit (release) the read transaction.
    #[napi(catch_unwind)]
    pub fn commit(&mut self) -> Result<()> {
        self.core.run(|db| db.commit())
    }

    /// Roll back (release) the read transaction. Same effect as `commit()`:
    /// a read transaction has nothing to undo.
    #[napi(catch_unwind)]
    pub fn rollback(&mut self) -> Result<()> {
        let result = self.core.run(|db| db.rollback());
        self.core.finished = true;
        result
    }
}
