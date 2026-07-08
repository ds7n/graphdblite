// SPDX-License-Identifier: MIT
// Copyright (c) 2026 ds7n

//! C FFI bindings for graphdblite.
//!
//! All functions return 0 on success, non-zero on error.
//! Call `graphdb_last_error` to retrieve the error message after a failure.
//!
//! # Ownership rules
//!
//! - `graphdb_open` returns a `*mut GraphDB` that must be freed with `graphdb_close`.
//! - `graphdb_query` returns a `*mut GraphResult` that must be freed with `graphdb_result_free`.
//! - All `*const c_char` strings returned by this library are owned by the library
//!   and valid until the next API call on the same handle.

use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::ptr;
use std::sync::Mutex;

use graphdblite::{Config, Database, GraphError, Record, Value};

// ---------------------------------------------------------------------------
// Thread-local error storage
// ---------------------------------------------------------------------------

thread_local! {
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

fn set_error(msg: &str) {
    LAST_ERROR.with(|e| {
        *e.borrow_mut() = CString::new(msg).ok();
    });
}

fn clear_error() {
    LAST_ERROR.with(|e| {
        *e.borrow_mut() = None;
    });
}

fn wrap_result<T>(result: Result<T, GraphError>, out: impl FnOnce(T)) -> i32 {
    match result {
        Ok(val) => {
            clear_error();
            out(val);
            0
        }
        Err(e) => {
            set_error(&e.to_string());
            -1
        }
    }
}

/// Run a fallible operation, converting any panic into a recoverable
/// `GraphError` instead of letting it unwind across the `extern "C"` boundary
/// (which is undefined behavior — in practice a process abort). Query/execute
/// paths run untrusted Cypher that can panic on malformed input, so every such
/// path funnels through here to guarantee the C caller gets `-1` +
/// `graphdb_last_error` rather than a crashed host process.
fn catch<T>(op: impl FnOnce() -> Result<T, GraphError>) -> Result<T, GraphError> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(op)) {
        Ok(res) => res,
        Err(payload) => {
            let msg = panic_message(&payload);
            Err(GraphError::transaction(format!("internal panic: {msg}")))
        }
    }
}

/// Best-effort extraction of a human-readable message from a panic payload.
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

// ---------------------------------------------------------------------------
// Opaque handle types
// ---------------------------------------------------------------------------

/// Opaque database handle.
///
/// The inner `Database` is wrapped in a `Mutex` so concurrent FFI calls on the
/// same handle from multiple threads serialize safely instead of producing
/// aliased `&mut` references (UB). Callers may still share a `*mut GraphDB`
/// across threads; per-call locking enforces exclusion internally.
pub struct GraphDB {
    db: Mutex<Database>,
}

/// Lock the inner Database, mapping poisoning to a recoverable error.
fn lock_db(handle: &GraphDB) -> Result<std::sync::MutexGuard<'_, Database>, GraphError> {
    handle.db.lock().map_err(|_| GraphError::Transaction {
        message: "database mutex poisoned".into(),
        hint: None,
    })
}

/// Opaque query result handle.
pub struct GraphResult {
    records: Vec<Record>,
    /// Serialized JSON string, lazily built.
    json: Option<CString>,
    /// Column names from the first record.
    columns: Vec<CString>,
}

// ---------------------------------------------------------------------------
// Error API
// ---------------------------------------------------------------------------

/// Get the last error message, or NULL if no error occurred.
///
/// The returned pointer is valid until the next API call on the current thread.
#[no_mangle]
pub extern "C" fn graphdb_last_error() -> *const c_char {
    LAST_ERROR.with(|e| e.borrow().as_ref().map_or(ptr::null(), |s| s.as_ptr()))
}

// ---------------------------------------------------------------------------
// Database lifecycle
// ---------------------------------------------------------------------------

/// Open a database at the given file path.
///
/// Returns 0 on success and writes the handle to `*out`.
/// Returns non-zero on error; call `graphdb_last_error()` for details.
#[no_mangle]
pub unsafe extern "C" fn graphdb_open(path: *const c_char, out: *mut *mut GraphDB) -> i32 {
    if path.is_null() || out.is_null() {
        set_error("null pointer argument");
        return -1;
    }
    let path_str = match unsafe { CStr::from_ptr(path) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            set_error(&format!("invalid UTF-8 path: {e}"));
            return -1;
        }
    };
    wrap_result(Database::open(path_str), |db| unsafe {
        *out = Box::into_raw(Box::new(GraphDB { db: Mutex::new(db) }));
    })
}

/// Open a database with a custom busy timeout (milliseconds).
#[no_mangle]
pub unsafe extern "C" fn graphdb_open_with_timeout(
    path: *const c_char,
    busy_timeout_ms: u32,
    out: *mut *mut GraphDB,
) -> i32 {
    if path.is_null() || out.is_null() {
        set_error("null pointer argument");
        return -1;
    }
    let path_str = match unsafe { CStr::from_ptr(path) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            set_error(&format!("invalid UTF-8 path: {e}"));
            return -1;
        }
    };
    let config = Config {
        busy_timeout_ms,
        ..Config::default()
    };
    wrap_result(Database::open_with_config(path_str, config), |db| unsafe {
        *out = Box::into_raw(Box::new(GraphDB { db: Mutex::new(db) }));
    })
}

/// Open an in-memory database.
#[no_mangle]
pub unsafe extern "C" fn graphdb_open_memory(out: *mut *mut GraphDB) -> i32 {
    if out.is_null() {
        set_error("null pointer argument");
        return -1;
    }
    wrap_result(Database::open_memory(), |db| unsafe {
        *out = Box::into_raw(Box::new(GraphDB { db: Mutex::new(db) }));
    })
}

/// Write a consistent single-file snapshot of the database to `path`.
///
/// Uses SQLite's `VACUUM INTO`: produces a self-contained file (no
/// `-wal` / `-shm` sidecars), defragmented and compacted. Returns non-zero
/// when a transaction is active on this handle or when `path` already exists;
/// call `graphdb_last_error` for details.
#[no_mangle]
pub unsafe extern "C" fn graphdb_snapshot_to(db: *mut GraphDB, path: *const c_char) -> i32 {
    if db.is_null() || path.is_null() {
        set_error("null pointer argument");
        return -1;
    }
    let handle = unsafe { &*db };
    let path_str = match unsafe { CStr::from_ptr(path) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            set_error(&format!("invalid UTF-8 path: {e}"));
            return -1;
        }
    };
    let mut guard = match lock_db(handle) {
        Ok(g) => g,
        Err(e) => {
            set_error(&e.to_string());
            return -1;
        }
    };
    wrap_result(guard.snapshot_to(path_str), |_| {})
}

/// Create a secondary index on `(label, property)` for faster lookups.
///
/// If a write transaction is open on this handle, the index DDL runs
/// inside it. Otherwise the call uses the stateful auto-tx path.
/// Returns non-zero on error; call `graphdb_last_error` for details.
#[no_mangle]
pub unsafe extern "C" fn graphdb_create_index(
    db: *mut GraphDB,
    label: *const c_char,
    property: *const c_char,
) -> i32 {
    ddl_call(db, label, property, |d, l, p| d.create_index(l, p))
}

/// Drop a secondary index on `(label, property)`.
#[no_mangle]
pub unsafe extern "C" fn graphdb_drop_index(
    db: *mut GraphDB,
    label: *const c_char,
    property: *const c_char,
) -> i32 {
    ddl_call(db, label, property, |d, l, p| d.drop_index(l, p))
}

/// Create a fulltext index on `(label, property)`. Accelerates
/// `CONTAINS` / `STARTS WITH` / `ENDS WITH` via SQLite FTS5 trigram.
#[no_mangle]
pub unsafe extern "C" fn graphdb_create_fulltext_index(
    db: *mut GraphDB,
    label: *const c_char,
    property: *const c_char,
) -> i32 {
    ddl_call(db, label, property, |d, l, p| d.create_fulltext_index(l, p))
}

/// Create a case-insensitive fulltext index on `(label, property)`.
/// Backed by SQLite FTS5 trigram tokenizer with `case_sensitive 0`,
/// so plain `CONTAINS` / `STARTS WITH` / `ENDS WITH` against this
/// property is case-insensitive.
#[no_mangle]
pub unsafe extern "C" fn graphdb_create_fulltext_index_ci(
    db: *mut GraphDB,
    label: *const c_char,
    property: *const c_char,
) -> i32 {
    ddl_call(db, label, property, |d, l, p| {
        d.create_fulltext_index_ci(l, p)
    })
}

/// Create a word-tokenized fulltext index on `(label, property)`.
/// Backed by SQLite FTS5 with the `unicode61` tokenizer. Suitable for
/// the `fts.search` procedure. Does not accelerate
/// `CONTAINS` / `STARTS WITH` / `ENDS WITH` — use
/// `graphdb_create_fulltext_index` for those.
#[no_mangle]
pub unsafe extern "C" fn graphdb_create_fulltext_index_word(
    db: *mut GraphDB,
    label: *const c_char,
    property: *const c_char,
) -> i32 {
    ddl_call(db, label, property, |d, l, p| {
        d.create_fulltext_index_word(l, p)
    })
}

/// Create a multi-property word-tokenized fulltext index on `(label, properties)`.
/// Backed by a single SQLite FTS5 `unicode61` multi-column virtual table that
/// covers every listed property. Use with `CALL fts.search(label, '*', query)`
/// to search across all covered columns, or `CALL fts.search(label, prop, query)`
/// to scope to one covered column.
///
/// `properties` must point to an array of `properties_count` non-null,
/// null-terminated UTF-8 strings. `properties_count` must be > 0.
#[no_mangle]
pub unsafe extern "C" fn graphdb_create_fulltext_index_word_multi(
    db: *mut GraphDB,
    label: *const c_char,
    properties: *const *const c_char,
    properties_count: usize,
) -> i32 {
    if db.is_null() || label.is_null() || properties.is_null() {
        set_error("null pointer argument");
        return -1;
    }
    let handle = unsafe { &*db };
    let label_str = match unsafe { CStr::from_ptr(label) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            set_error(&format!("invalid UTF-8 label: {e}"));
            return -1;
        }
    };
    let mut props: Vec<String> = Vec::with_capacity(properties_count);
    for i in 0..properties_count {
        let p_ptr = unsafe { *properties.add(i) };
        if p_ptr.is_null() {
            set_error("null property pointer in properties array");
            return -1;
        }
        match unsafe { CStr::from_ptr(p_ptr) }.to_str() {
            Ok(s) => props.push(s.to_string()),
            Err(e) => {
                set_error(&format!("invalid UTF-8 property: {e}"));
                return -1;
            }
        }
    }
    let mut guard = match lock_db(handle) {
        Ok(g) => g,
        Err(e) => {
            set_error(&e.to_string());
            return -1;
        }
    };
    wrap_result(
        guard.create_fulltext_index_word_multi(label_str, &props),
        |_| {},
    )
}

/// Drop a fulltext index on `(label, property)`.
#[no_mangle]
pub unsafe extern "C" fn graphdb_drop_fulltext_index(
    db: *mut GraphDB,
    label: *const c_char,
    property: *const c_char,
) -> i32 {
    ddl_call(db, label, property, |d, l, p| d.drop_fulltext_index(l, p))
}

/// Create a composite secondary index on `(label, properties[0..count])`.
///
/// `properties` must point to an array of `count` non-null, null-terminated
/// UTF-8 strings listing the property names in column order. `count` must be
/// ≥ 2. Returns non-zero on error; call `graphdb_last_error` for details.
#[no_mangle]
pub unsafe extern "C" fn graphdb_create_composite_index(
    db: *mut GraphDB,
    label: *const c_char,
    properties: *const *const c_char,
    count: usize,
) -> i32 {
    composite_ddl_call(db, label, properties, count, |d, l, p| {
        d.create_composite_index(l, p)
    })
}

/// Drop a composite secondary index on `(label, properties[0..count])`.
///
/// `properties` must point to an array of `count` non-null, null-terminated
/// UTF-8 strings listing the property names in the same order used at creation.
#[no_mangle]
pub unsafe extern "C" fn graphdb_drop_composite_index(
    db: *mut GraphDB,
    label: *const c_char,
    properties: *const *const c_char,
    count: usize,
) -> i32 {
    composite_ddl_call(db, label, properties, count, |d, l, p| {
        d.drop_composite_index(l, p)
    })
}

/// Shared scaffold for composite DDL exports.
fn composite_ddl_call(
    db: *mut GraphDB,
    label: *const c_char,
    properties: *const *const c_char,
    count: usize,
    op: impl FnOnce(&mut Database, &str, &[&str]) -> Result<(), GraphError>,
) -> i32 {
    if db.is_null() || label.is_null() || properties.is_null() {
        set_error("null pointer argument");
        return -1;
    }
    if count == 0 {
        set_error("count must be > 0");
        return -1;
    }
    let handle = unsafe { &*db };
    let label_str = match unsafe { CStr::from_ptr(label) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            set_error(&format!("invalid UTF-8 label: {e}"));
            return -1;
        }
    };
    let mut prop_strs: Vec<&str> = Vec::with_capacity(count);
    for i in 0..count {
        let p_ptr = unsafe { *properties.add(i) };
        if p_ptr.is_null() {
            set_error("null property pointer in properties array");
            return -1;
        }
        match unsafe { CStr::from_ptr(p_ptr) }.to_str() {
            Ok(s) => prop_strs.push(s),
            Err(e) => {
                set_error(&format!("invalid UTF-8 property: {e}"));
                return -1;
            }
        }
    }
    let mut guard = match lock_db(handle) {
        Ok(g) => g,
        Err(e) => {
            set_error(&e.to_string());
            return -1;
        }
    };
    wrap_result(op(&mut *guard, label_str, &prop_strs), |_| {})
}

/// Shared scaffold for single-property DDL exports.
fn ddl_call(
    db: *mut GraphDB,
    label: *const c_char,
    property: *const c_char,
    op: impl FnOnce(&mut Database, &str, &str) -> Result<(), GraphError>,
) -> i32 {
    if db.is_null() || label.is_null() || property.is_null() {
        set_error("null pointer argument");
        return -1;
    }
    let handle = unsafe { &*db };
    let label_str = match unsafe { CStr::from_ptr(label) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            set_error(&format!("invalid UTF-8 label: {e}"));
            return -1;
        }
    };
    let property_str = match unsafe { CStr::from_ptr(property) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            set_error(&format!("invalid UTF-8 property: {e}"));
            return -1;
        }
    };
    let mut guard = match lock_db(handle) {
        Ok(g) => g,
        Err(e) => {
            set_error(&e.to_string());
            return -1;
        }
    };
    wrap_result(op(&mut *guard, label_str, property_str), |_| {})
}

/// Close a database and free its resources.
///
/// After this call, the pointer is invalid. Passing NULL is a no-op.
#[no_mangle]
pub unsafe extern "C" fn graphdb_close(db: *mut GraphDB) {
    if !db.is_null() {
        unsafe { drop(Box::from_raw(db)) };
    }
}

// ---------------------------------------------------------------------------
// Query / Execute
// ---------------------------------------------------------------------------

/// Execute a read-only Cypher query.
///
/// On success, writes a `GraphResult` handle to `*out`. The caller must free it
/// with `graphdb_result_free`.
#[no_mangle]
pub unsafe extern "C" fn graphdb_query(
    db: *mut GraphDB,
    cypher: *const c_char,
    out: *mut *mut GraphResult,
) -> i32 {
    if db.is_null() || cypher.is_null() || out.is_null() {
        set_error("null pointer argument");
        return -1;
    }
    let handle = unsafe { &*db };
    let cypher_str = match unsafe { CStr::from_ptr(cypher) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            set_error(&format!("invalid UTF-8 query: {e}"));
            return -1;
        }
    };

    let result = catch(|| {
        let mut guard = lock_db(handle)?;
        let tx = guard.read_tx()?;
        let records = tx.query(cypher_str)?;
        tx.commit()?;
        Ok(records)
    });

    wrap_result(result, |records| unsafe {
        *out = Box::into_raw(Box::new(GraphResult {
            records,
            json: None,
            columns: Vec::new(),
        }));
    })
}

/// Execute a write Cypher query (CREATE, DELETE, SET, MERGE).
///
/// On success, writes a `GraphResult` handle to `*out`. The caller must free it
/// with `graphdb_result_free`.
#[no_mangle]
pub unsafe extern "C" fn graphdb_execute(
    db: *mut GraphDB,
    cypher: *const c_char,
    out: *mut *mut GraphResult,
) -> i32 {
    if db.is_null() || cypher.is_null() || out.is_null() {
        set_error("null pointer argument");
        return -1;
    }
    let handle = unsafe { &*db };
    let cypher_str = match unsafe { CStr::from_ptr(cypher) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            set_error(&format!("invalid UTF-8 query: {e}"));
            return -1;
        }
    };

    let result = catch(|| {
        let mut guard = lock_db(handle)?;
        let tx = guard.write_tx()?;
        let records = tx.query(cypher_str)?;
        tx.commit()?;
        Ok(records)
    });

    wrap_result(result, |records| unsafe {
        *out = Box::into_raw(Box::new(GraphResult {
            records,
            json: None,
            columns: Vec::new(),
        }));
    })
}

// ---------------------------------------------------------------------------
// Transaction API
// ---------------------------------------------------------------------------

/// Begin a write transaction. The database handle must not be used for other
/// operations until the transaction is committed or rolled back.
#[no_mangle]
pub unsafe extern "C" fn graphdb_tx_begin_write(db: *mut GraphDB) -> i32 {
    if db.is_null() {
        set_error("null pointer argument");
        return -1;
    }
    let handle = unsafe { &*db };
    let result = (|| -> Result<(), GraphError> { lock_db(handle)?.begin_write() })();
    wrap_result(result, |()| {})
}

/// Begin a read transaction.
#[no_mangle]
pub unsafe extern "C" fn graphdb_tx_begin_read(db: *mut GraphDB) -> i32 {
    if db.is_null() {
        set_error("null pointer argument");
        return -1;
    }
    let handle = unsafe { &*db };
    let result = (|| -> Result<(), GraphError> { lock_db(handle)?.begin_read() })();
    wrap_result(result, |()| {})
}

/// Execute a Cypher query within the current transaction.
#[no_mangle]
pub unsafe extern "C" fn graphdb_tx_execute(
    db: *mut GraphDB,
    cypher: *const c_char,
    out: *mut *mut GraphResult,
) -> i32 {
    if db.is_null() || cypher.is_null() || out.is_null() {
        set_error("null pointer argument");
        return -1;
    }
    let handle = unsafe { &*db };
    let cypher_str = match unsafe { CStr::from_ptr(cypher) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            set_error(&format!("invalid UTF-8 query: {e}"));
            return -1;
        }
    };

    let result = catch(|| lock_db(handle)?.execute(cypher_str));

    wrap_result(result, |records| unsafe {
        *out = Box::into_raw(Box::new(GraphResult {
            records,
            json: None,
            columns: Vec::new(),
        }));
    })
}

/// Deprecated alias for `graphdb_tx_execute`. Will be removed in a future
/// release. Kept temporarily for the Go binding's transition.
#[no_mangle]
pub unsafe extern "C" fn graphdb_tx_query(
    db: *mut GraphDB,
    cypher: *const c_char,
    out: *mut *mut GraphResult,
) -> i32 {
    unsafe { graphdb_tx_execute(db, cypher, out) }
}

/// Commit the current transaction.
#[no_mangle]
pub unsafe extern "C" fn graphdb_tx_commit(db: *mut GraphDB) -> i32 {
    if db.is_null() {
        set_error("null pointer argument");
        return -1;
    }
    let handle = unsafe { &*db };
    let result = (|| -> Result<(), GraphError> { lock_db(handle)?.commit() })();
    wrap_result(result, |()| {})
}

/// Rollback the current transaction.
#[no_mangle]
pub unsafe extern "C" fn graphdb_tx_rollback(db: *mut GraphDB) -> i32 {
    if db.is_null() {
        set_error("null pointer argument");
        return -1;
    }
    let handle = unsafe { &*db };
    let result = (|| -> Result<(), GraphError> { lock_db(handle)?.rollback() })();
    wrap_result(result, |()| {})
}

// ---------------------------------------------------------------------------
// Result API
// ---------------------------------------------------------------------------

/// Get the number of rows in a query result.
#[no_mangle]
pub unsafe extern "C" fn graphdb_result_row_count(result: *const GraphResult) -> i64 {
    if result.is_null() {
        return 0;
    }
    unsafe { &*result }.records.len() as i64
}

/// Get the number of columns in a query result.
#[no_mangle]
pub unsafe extern "C" fn graphdb_result_column_count(result: *const GraphResult) -> i64 {
    if result.is_null() {
        return 0;
    }
    let r = unsafe { &*result };
    r.records.first().map_or(0, |rec| rec.len()) as i64
}

/// Get a column name by index. Returns NULL if out of bounds.
///
/// The returned pointer is valid until `graphdb_result_free` is called.
#[no_mangle]
pub unsafe extern "C" fn graphdb_result_column_name(
    result: *mut GraphResult,
    col: i64,
) -> *const c_char {
    if result.is_null() || col < 0 {
        return ptr::null();
    }
    let r = unsafe { &mut *result };

    // Lazily build column name cache.
    if r.columns.is_empty() {
        if let Some(rec) = r.records.first() {
            r.columns = rec
                .keys()
                .filter_map(|k| CString::new(k.as_str()).ok())
                .collect();
        }
    }

    r.columns
        .get(col as usize)
        .map_or(ptr::null(), |s| s.as_ptr())
}

/// Get a value as a string representation. Returns NULL if out of bounds.
///
/// The returned pointer is valid until the next call to `graphdb_result_value_str`
/// on the same result, or until `graphdb_result_free`.
#[no_mangle]
pub unsafe extern "C" fn graphdb_result_value_str(
    result: *const GraphResult,
    row: i64,
    col: i64,
) -> *const c_char {
    if result.is_null() || row < 0 || col < 0 {
        return ptr::null();
    }
    let r = unsafe { &*result };
    let row_idx = row as usize;
    let col_idx = col as usize;

    let rec = match r.records.get(row_idx) {
        Some(rec) => rec,
        None => return ptr::null(),
    };

    let val = match rec.value_at(col_idx) {
        Some(val) => val,
        None => return ptr::null(),
    };

    // Store in thread-local to keep the CString alive.
    thread_local! {
        static VALUE_BUF: RefCell<Option<CString>> = const { RefCell::new(None) };
    }
    let s = format_value(val);
    // Interior NULs would silently truncate via `unwrap_or_default()`, leaving
    // the caller with an empty string and no signal. Surface as set_error and
    // return NULL so the failure is observable.
    let cstr = match CString::new(s) {
        Ok(c) => c,
        Err(e) => {
            set_error(&format!("value contains interior NUL byte: {e}"));
            return ptr::null();
        }
    };
    VALUE_BUF.with(|buf| {
        let ptr = cstr.as_ptr();
        *buf.borrow_mut() = Some(cstr);
        ptr
    })
}

/// Get the type of a value. Returns one of:
/// 0 = null, 1 = bool, 2 = i64, 3 = f64, 4 = string, 5 = list, 6 = path
#[no_mangle]
pub unsafe extern "C" fn graphdb_result_value_type(
    result: *const GraphResult,
    row: i64,
    col: i64,
) -> i32 {
    if result.is_null() || row < 0 || col < 0 {
        return 0;
    }
    let r = unsafe { &*result };
    let rec = match r.records.get(row as usize) {
        Some(rec) => rec,
        None => return 0,
    };
    match rec.value_at(col as usize) {
        Some(Value::Null) | None => 0,
        Some(Value::Bool(_)) => 1,
        Some(Value::I64(_)) => 2,
        Some(Value::F64(_)) => 3,
        Some(Value::String(_)) => 4,
        Some(Value::List(_)) => 5,
        Some(Value::Path(_)) => 6,
        Some(Value::Map(_)) => 7,
        Some(Value::Node(_)) => 8,
        Some(Value::Edge(_)) => 9,
        Some(Value::Date(_)) => 10,
        Some(Value::LocalTime(_)) => 11,
        Some(Value::Time(_)) => 12,
        Some(Value::LocalDateTime(_)) => 13,
        Some(Value::DateTime(_)) => 14,
        Some(Value::Duration(_)) => 15,
    }
}

/// Get an integer value. Returns 0 if the value is not an integer.
#[no_mangle]
pub unsafe extern "C" fn graphdb_result_value_i64(
    result: *const GraphResult,
    row: i64,
    col: i64,
) -> i64 {
    if result.is_null() || row < 0 || col < 0 {
        return 0;
    }
    let r = unsafe { &*result };
    let rec = match r.records.get(row as usize) {
        Some(rec) => rec,
        None => return 0,
    };
    match rec.value_at(col as usize) {
        Some(Value::I64(n)) => *n,
        _ => 0,
    }
}

/// Get a float value. Returns 0.0 if the value is not a float.
#[no_mangle]
pub unsafe extern "C" fn graphdb_result_value_f64(
    result: *const GraphResult,
    row: i64,
    col: i64,
) -> f64 {
    if result.is_null() || row < 0 || col < 0 {
        return 0.0;
    }
    let r = unsafe { &*result };
    let rec = match r.records.get(row as usize) {
        Some(rec) => rec,
        None => return 0.0,
    };
    match rec.value_at(col as usize) {
        Some(Value::F64(n)) => *n,
        _ => 0.0,
    }
}

/// Get a boolean value. Returns 0 (false) if the value is not a boolean.
#[no_mangle]
pub unsafe extern "C" fn graphdb_result_value_bool(
    result: *const GraphResult,
    row: i64,
    col: i64,
) -> i32 {
    if result.is_null() || row < 0 || col < 0 {
        return 0;
    }
    let r = unsafe { &*result };
    let rec = match r.records.get(row as usize) {
        Some(rec) => rec,
        None => return 0,
    };
    match rec.value_at(col as usize) {
        Some(Value::Bool(b)) => *b as i32,
        _ => 0,
    }
}

/// Get the full result as a JSON string.
///
/// The returned pointer is valid until `graphdb_result_free` is called.
#[no_mangle]
pub unsafe extern "C" fn graphdb_result_json(result: *mut GraphResult) -> *const c_char {
    if result.is_null() {
        return ptr::null();
    }
    let r = unsafe { &mut *result };

    if r.json.is_none() {
        let json = records_to_json(&r.records);
        r.json = CString::new(json).ok();
    }

    r.json.as_ref().map_or(ptr::null(), |s| s.as_ptr())
}

/// Free a query result.
///
/// After this call, the pointer is invalid. Passing NULL is a no-op.
#[no_mangle]
pub unsafe extern "C" fn graphdb_result_free(result: *mut GraphResult) {
    if !result.is_null() {
        unsafe { drop(Box::from_raw(result)) };
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn format_value(val: &Value) -> String {
    match val {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::I64(n) => n.to_string(),
        Value::F64(n) => n.to_string(),
        Value::String(s) => s.clone(),
        Value::List(items) => {
            let parts: Vec<String> = items.iter().map(format_value).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Path(p) => {
            let parts: Vec<String> = p.nodes.iter().map(|n| n.id.0.to_string()).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Node(n) => format!("(:{} #{})", n.labels.join(":"), n.id.0),
        Value::Edge(e) => format!("[:{} {}->{}]", e.label, e.src.0, e.dst.0),
        Value::Map(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{k}: {}", format_value(v)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
        other => format!("{other}"),
    }
}

fn records_to_json(records: &[Record]) -> String {
    let mut out = String::from("[");
    for (i, rec) in records.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push('{');
        for (j, (key, val)) in rec.iter().enumerate() {
            if j > 0 {
                out.push_str(", ");
            }
            out.push('"');
            json_escape_into(&mut out, key);
            out.push_str("\": ");
            value_to_json(&mut out, val);
        }
        out.push('}');
    }
    out.push(']');
    out
}

fn value_to_json(out: &mut String, val: &Value) {
    match val {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::I64(n) => out.push_str(&n.to_string()),
        Value::F64(n) => out.push_str(&n.to_string()),
        Value::String(s) => {
            out.push('"');
            json_escape_into(out, s);
            out.push('"');
        }
        Value::List(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                value_to_json(out, item);
            }
            out.push(']');
        }
        Value::Path(p) => {
            out.push('[');
            for (i, n) in p.nodes.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&n.id.0.to_string());
            }
            out.push(']');
        }
        Value::Node(n) => {
            out.push('{');
            out.push_str("\"__id\": ");
            out.push_str(&n.id.0.to_string());
            out.push_str(", \"__labels\": [");
            for (i, label) in n.labels.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push('"');
                json_escape_into(out, label);
                out.push('"');
            }
            out.push(']');
            for (k, v) in &n.properties {
                out.push_str(", \"");
                json_escape_into(out, k);
                out.push_str("\": ");
                value_to_json(out, v);
            }
            out.push('}');
        }
        Value::Edge(e) => {
            out.push('{');
            out.push_str("\"__src\": ");
            out.push_str(&e.src.0.to_string());
            out.push_str(", \"__dst\": ");
            out.push_str(&e.dst.0.to_string());
            out.push_str(", \"__label\": \"");
            json_escape_into(out, &e.label);
            out.push('"');
            for (k, v) in &e.properties {
                out.push_str(", \"");
                json_escape_into(out, k);
                out.push_str("\": ");
                value_to_json(out, v);
            }
            out.push('}');
        }
        Value::Map(map) => {
            out.push('{');
            for (i, (k, v)) in map.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push('"');
                json_escape_into(out, k);
                out.push_str("\": ");
                value_to_json(out, v);
            }
            out.push('}');
        }
        // Temporal types — serialize as quoted ISO strings in JSON.
        other => {
            out.push('"');
            json_escape_into(out, &format!("{other}"));
            out.push('"');
        }
    }
}

fn json_escape_into(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < '\x20' => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod panic_safety_tests {
    use super::*;

    // `catch` must convert a panic into a recoverable GraphError, never unwind.
    #[test]
    fn catch_converts_panic_to_error() {
        let r: Result<(), GraphError> = catch(|| panic!("boom"));
        let err = r.expect_err("catch should surface the panic as an Err");
        assert!(
            err.to_string().contains("internal panic"),
            "unexpected error message: {err}"
        );
        assert!(
            err.to_string().contains("boom"),
            "panic message lost: {err}"
        );
    }

    // `catch` passes through a normal Ok result unchanged.
    #[test]
    fn catch_passes_through_ok() {
        let r: Result<i32, GraphError> = catch(|| Ok(42));
        assert_eq!(r.unwrap(), 42);
    }

    // End-to-end: a query that panicked in eval before the substring fix must
    // now return a clean result through the FFI boundary (return code 0), and
    // the process must not abort. Guards the whole graphdb_query path.
    #[test]
    fn ffi_query_with_formerly_panicking_input_does_not_abort() {
        unsafe {
            let mut db: *mut GraphDB = ptr::null_mut();
            assert_eq!(graphdb_open_memory(&mut db), 0);
            assert!(!db.is_null());

            let cypher = CString::new("RETURN substring('é', 1) AS s").unwrap();
            let mut result: *mut GraphResult = ptr::null_mut();
            let rc = graphdb_query(db, cypher.as_ptr(), &mut result);

            assert_eq!(rc, 0, "query returned error code {rc}");
            assert!(!result.is_null());
            assert_eq!(graphdb_result_row_count(result), 1);

            graphdb_result_free(result);
            graphdb_close(db);
        }
    }
}
