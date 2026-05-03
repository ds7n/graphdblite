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

use graphdblite::cypher::{executor, parser, planner, record::Record};
use graphdblite::{Config, Database, GraphError, Value};

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

    let result = (|| -> Result<Vec<Record>, GraphError> {
        let mut guard = lock_db(handle)?;
        let tx = guard.begin_read()?;
        let records = tx.query(cypher_str)?;
        tx.commit()?;
        Ok(records)
    })();

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

    let result = (|| -> Result<Vec<Record>, GraphError> {
        let mut guard = lock_db(handle)?;
        let tx = guard.begin_write()?;
        let records = tx.query(cypher_str)?;
        tx.commit()?;
        Ok(records)
    })();

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
    let guard = match lock_db(handle) {
        Ok(g) => g,
        Err(e) => {
            set_error(&e.to_string());
            return -1;
        }
    };
    match guard.connection().execute_batch("BEGIN IMMEDIATE") {
        Ok(()) => {
            clear_error();
            0
        }
        Err(e) => {
            set_error(&format!("failed to begin write transaction: {e}"));
            -1
        }
    }
}

/// Begin a read transaction.
#[no_mangle]
pub unsafe extern "C" fn graphdb_tx_begin_read(db: *mut GraphDB) -> i32 {
    if db.is_null() {
        set_error("null pointer argument");
        return -1;
    }
    let handle = unsafe { &*db };
    let guard = match lock_db(handle) {
        Ok(g) => g,
        Err(e) => {
            set_error(&e.to_string());
            return -1;
        }
    };
    match guard.connection().execute_batch("BEGIN DEFERRED") {
        Ok(()) => {
            clear_error();
            0
        }
        Err(e) => {
            set_error(&format!("failed to begin read transaction: {e}"));
            -1
        }
    }
}

/// Execute a Cypher query within the current transaction.
#[no_mangle]
pub unsafe extern "C" fn graphdb_tx_query(
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

    let result = (|| -> Result<Vec<Record>, GraphError> {
        let guard = lock_db(handle)?;
        let conn = guard.connection();
        let stmt = parser::parse(cypher_str)?;
        let plan = planner::plan(conn, &stmt)?;
        let ctx = executor::ExecContext {
            max_result_rows: guard.max_result_rows,
            max_traversal_depth: guard.max_traversal_depth,
            max_traversal_work: guard.max_traversal_work,
            ..Default::default()
        };
        executor::execute_with_ctx(conn, &plan, &ctx)
    })();

    wrap_result(result, |records| unsafe {
        *out = Box::into_raw(Box::new(GraphResult {
            records,
            json: None,
            columns: Vec::new(),
        }));
    })
}

/// Commit the current transaction.
#[no_mangle]
pub unsafe extern "C" fn graphdb_tx_commit(db: *mut GraphDB) -> i32 {
    if db.is_null() {
        set_error("null pointer argument");
        return -1;
    }
    let handle = unsafe { &*db };
    let guard = match lock_db(handle) {
        Ok(g) => g,
        Err(e) => {
            set_error(&e.to_string());
            return -1;
        }
    };
    match guard.connection().execute_batch("COMMIT") {
        Ok(()) => {
            clear_error();
            0
        }
        Err(e) => {
            set_error(&format!("commit failed: {e}"));
            -1
        }
    }
}

/// Rollback the current transaction.
#[no_mangle]
pub unsafe extern "C" fn graphdb_tx_rollback(db: *mut GraphDB) -> i32 {
    if db.is_null() {
        set_error("null pointer argument");
        return -1;
    }
    let handle = unsafe { &*db };
    let guard = match lock_db(handle) {
        Ok(g) => g,
        Err(e) => {
            set_error(&e.to_string());
            return -1;
        }
    };
    match guard.connection().execute_batch("ROLLBACK") {
        Ok(()) => {
            clear_error();
            0
        }
        Err(e) => {
            set_error(&format!("rollback failed: {e}"));
            -1
        }
    }
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
    r.records.first().map_or(0, |rec| rec.fields.len()) as i64
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
                .fields
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

    let val = match rec.fields.values().nth(col_idx) {
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
    match rec.fields.values().nth(col as usize) {
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
    match rec.fields.values().nth(col as usize) {
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
    match rec.fields.values().nth(col as usize) {
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
    match rec.fields.values().nth(col as usize) {
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
        for (j, (key, val)) in rec.fields.iter().enumerate() {
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
