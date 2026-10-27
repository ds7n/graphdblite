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
//! - Strings returned for a result (`graphdb_result_value_str`,
//!   `graphdb_result_column_name`, `graphdb_result_json`) are owned by that
//!   result and stay valid until `graphdb_result_free` on it.
//! - The `graphdb_last_error` string is thread-local and valid until the next
//!   API call on the same thread.
//!
//! # Threads
//!
//! A handle may be shared across threads (calls serialize on an internal
//! lock), but a transaction begun with `graphdb_tx_begin_write` /
//! `graphdb_tx_begin_read` belongs to the thread that began it: other
//! threads' transaction calls are rejected until it ends. The `_shared`
//! begin variants skip that check, for bindings that track transaction
//! ownership themselves.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::ptr;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread::ThreadId;

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

/// The `GraphError` reported for a panic caught at the FFI boundary.
fn panic_error(payload: &Box<dyn std::any::Any + Send>) -> GraphError {
    GraphError::transaction(format!("internal panic: {}", panic_message(payload)))
}

/// Contain a panic at an `extern "C"` boundary: run `op`, and if it panics
/// record the panic as the last error and return `fallback` instead of
/// unwinding into C (undefined behavior; in practice a process abort).
/// Every export's body runs inside this.
fn catch_or<T>(fallback: T, op: impl FnOnce() -> T) -> T {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(op)).unwrap_or_else(|payload| {
        set_error(&panic_error(&payload).to_string());
        fallback
    })
}

/// Run `op` on the handle's database with panic containment.
///
/// The lock is taken outside the `catch_unwind`, so a panic inside `op`
/// unwinds only `op`'s frames and never poisons the mutex. A panic can
/// still leave a transaction open (Cypher runs inside an implicit or
/// explicit transaction), so it is rolled back before returning the error:
/// the handle stays usable for the next call.
fn with_db<T>(
    handle: &GraphDB,
    op: impl FnOnce(&mut Database) -> Result<T, GraphError>,
) -> Result<T, GraphError> {
    let mut guard = lock_db(handle);
    let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| op(&mut guard))) {
        Ok(result) => result,
        Err(payload) => {
            // No transaction open is fine: there is nothing to undo.
            let _ = guard.rollback();
            Err(panic_error(&payload))
        }
    };
    // However the call ended, a transaction that is no longer open has no
    // owner any more.
    if !guard.in_transaction() {
        *lock_owner(handle) = None;
    }
    result
}

/// [`with_db`] for calls that act on the handle's active transaction:
/// rejects them when that transaction belongs to another thread.
fn with_tx_db<T>(
    handle: &GraphDB,
    op: impl FnOnce(&mut Database) -> Result<T, GraphError>,
) -> Result<T, GraphError> {
    with_db(handle, |d| {
        check_tx_owner(handle, d)?;
        op(d)
    })
}

/// Error unless the active transaction (if any) may be used by this thread.
fn check_tx_owner(handle: &GraphDB, d: &Database) -> Result<(), GraphError> {
    if !d.in_transaction() {
        return Ok(());
    }
    match *lock_owner(handle) {
        Some(TxOwner::Thread(t)) if t != std::thread::current().id() => {
            Err(GraphError::Transaction {
                message: "the active transaction belongs to another thread".to_string(),
                hint: Some(
                    "use a transaction only from the thread that began it, or begin it with \
                     graphdb_tx_begin_write_shared / graphdb_tx_begin_read_shared"
                        .to_string(),
                ),
            })
        }
        _ => Ok(()),
    }
}

/// Run a fallible operation, converting any panic into a recoverable
/// `GraphError` instead of letting it unwind across the `extern "C"` boundary
/// (which is undefined behavior — in practice a process abort). Query/execute
/// paths run untrusted Cypher that can panic on malformed input, so every such
/// path funnels through here to guarantee the C caller gets `-1` +
/// `graphdb_last_error` rather than a crashed host process.
#[cfg(test)]
fn catch<T>(op: impl FnOnce() -> Result<T, GraphError>) -> Result<T, GraphError> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(op)) {
        Ok(res) => res,
        Err(payload) => Err(panic_error(&payload)),
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
    /// Who may use the active transaction. Only touched while `db` is
    /// locked (lock order: `db`, then `tx_owner`).
    tx_owner: Mutex<Option<TxOwner>>,
}

impl GraphDB {
    fn boxed(db: Database) -> *mut GraphDB {
        Box::into_raw(Box::new(GraphDB {
            db: Mutex::new(db),
            tx_owner: Mutex::new(None),
        }))
    }
}

/// Which threads may use a handle's active transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TxOwner {
    /// Only the thread that began it (`graphdb_tx_begin_write` / `_read`).
    Thread(ThreadId),
    /// Any thread (`graphdb_tx_begin_write_shared` / `_read_shared`).
    Any,
}

/// Lock the transaction-owner slot. Nothing panics while it is held, so a
/// poisoned lock still holds a consistent value.
fn lock_owner(handle: &GraphDB) -> MutexGuard<'_, Option<TxOwner>> {
    handle
        .tx_owner
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Lock the inner Database. [`with_db`] never lets a panic unwind while the
/// lock is held, so poisoning should not happen; if it does, recover the
/// guard, clear the poison and roll back any interrupted transaction rather
/// than failing every later call on the handle.
fn lock_db(handle: &GraphDB) -> std::sync::MutexGuard<'_, Database> {
    handle.db.lock().unwrap_or_else(|poisoned| {
        handle.db.clear_poison();
        let mut guard = poisoned.into_inner();
        let _ = guard.rollback();
        guard
    })
}

/// Opaque query result handle.
pub struct GraphResult {
    records: Vec<Record>,
    /// Serialized JSON string, lazily built.
    json: Option<CString>,
    /// Column names from the first record.
    columns: Vec<CString>,
    /// `graphdb_result_value_str` strings, one per `(row, col)` asked for.
    /// Kept until `graphdb_result_free` so every returned pointer stays
    /// valid (a `CString`'s buffer does not move when the map grows). A
    /// mutex because the getter takes a `*const` and may be called from
    /// several threads.
    value_strs: Mutex<HashMap<(usize, usize), CString>>,
}

impl GraphResult {
    fn boxed(records: Vec<Record>) -> *mut GraphResult {
        Box::into_raw(Box::new(GraphResult {
            records,
            json: None,
            columns: Vec::new(),
            value_strs: Mutex::new(HashMap::new()),
        }))
    }
}

// ---------------------------------------------------------------------------
// Error API
// ---------------------------------------------------------------------------

/// Get the last error message, or NULL if no error occurred.
///
/// The returned pointer is valid until the next API call on the current thread.
#[no_mangle]
pub extern "C" fn graphdb_last_error() -> *const c_char {
    catch_or(ptr::null(), || {
        LAST_ERROR.with(|e| e.borrow().as_ref().map_or(ptr::null(), |s| s.as_ptr()))
    })
}

// ---------------------------------------------------------------------------
// Database lifecycle
// ---------------------------------------------------------------------------

/// Open a database at the given file path. The path `:memory:` opens a
/// private in-memory database (no file is created).
///
/// Returns 0 on success and writes the handle to `*out`.
/// Returns non-zero on error; call `graphdb_last_error()` for details.
#[no_mangle]
pub unsafe extern "C" fn graphdb_open(path: *const c_char, out: *mut *mut GraphDB) -> i32 {
    catch_or(-1, || {
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
            *out = GraphDB::boxed(db);
        })
    })
}

/// Open a database with a custom busy timeout (milliseconds).
#[no_mangle]
pub unsafe extern "C" fn graphdb_open_with_timeout(
    path: *const c_char,
    busy_timeout_ms: u32,
    out: *mut *mut GraphDB,
) -> i32 {
    catch_or(-1, || {
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
            *out = GraphDB::boxed(db);
        })
    })
}

/// Open an in-memory database.
#[no_mangle]
pub unsafe extern "C" fn graphdb_open_memory(out: *mut *mut GraphDB) -> i32 {
    catch_or(-1, || {
        if out.is_null() {
            set_error("null pointer argument");
            return -1;
        }
        wrap_result(Database::open_memory(), |db| unsafe {
            *out = GraphDB::boxed(db);
        })
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
    catch_or(-1, || {
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
        wrap_result(with_db(handle, |d| d.snapshot_to(path_str)), |_| {})
    })
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
    catch_or(-1, || {
        ddl_call(db, label, property, |d, l, p| d.create_index(l, p))
    })
}

/// Drop a secondary index on `(label, property)`.
#[no_mangle]
pub unsafe extern "C" fn graphdb_drop_index(
    db: *mut GraphDB,
    label: *const c_char,
    property: *const c_char,
) -> i32 {
    catch_or(-1, || {
        ddl_call(db, label, property, |d, l, p| d.drop_index(l, p))
    })
}

/// Create a fulltext index on `(label, property)`. Accelerates
/// `CONTAINS` / `STARTS WITH` / `ENDS WITH` via SQLite FTS5 trigram.
#[no_mangle]
pub unsafe extern "C" fn graphdb_create_fulltext_index(
    db: *mut GraphDB,
    label: *const c_char,
    property: *const c_char,
) -> i32 {
    catch_or(-1, || {
        ddl_call(db, label, property, |d, l, p| d.create_fulltext_index(l, p))
    })
}

/// Create a case-insensitive fulltext index on `(label, property)`.
/// Backed by SQLite FTS5 trigram tokenizer with `case_sensitive 0`.
/// Accelerates the case-insensitive idiom
/// `toLower(n.prop) CONTAINS toLower($q)` (and `STARTS WITH` / `ENDS WITH`);
/// plain predicates keep Cypher's case-sensitive semantics.
#[no_mangle]
pub unsafe extern "C" fn graphdb_create_fulltext_index_ci(
    db: *mut GraphDB,
    label: *const c_char,
    property: *const c_char,
) -> i32 {
    catch_or(-1, || {
        ddl_call(db, label, property, |d, l, p| {
            d.create_fulltext_index_ci(l, p)
        })
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
    catch_or(-1, || {
        ddl_call(db, label, property, |d, l, p| {
            d.create_fulltext_index_word(l, p)
        })
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
    catch_or(-1, || {
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
        wrap_result(
            with_tx_db(handle, |d| {
                d.create_fulltext_index_word_multi(label_str, &props)
            }),
            |_| {},
        )
    })
}

/// Drop a fulltext index on `(label, property)`.
#[no_mangle]
pub unsafe extern "C" fn graphdb_drop_fulltext_index(
    db: *mut GraphDB,
    label: *const c_char,
    property: *const c_char,
) -> i32 {
    catch_or(-1, || {
        ddl_call(db, label, property, |d, l, p| d.drop_fulltext_index(l, p))
    })
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
    catch_or(-1, || {
        composite_ddl_call(db, label, properties, count, |d, l, p| {
            d.create_composite_index(l, p)
        })
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
    catch_or(-1, || {
        composite_ddl_call(db, label, properties, count, |d, l, p| {
            d.drop_composite_index(l, p)
        })
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
    wrap_result(with_tx_db(handle, |d| op(d, label_str, &prop_strs)), |_| {})
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
    wrap_result(
        with_tx_db(handle, |d| op(d, label_str, property_str)),
        |_| {},
    )
}

/// Close a database and free its resources.
///
/// After this call, the pointer is invalid. Passing NULL is a no-op.
#[no_mangle]
pub unsafe extern "C" fn graphdb_close(db: *mut GraphDB) {
    catch_or((), || {
        if !db.is_null() {
            unsafe { drop(Box::from_raw(db)) };
        }
    })
}

// ---------------------------------------------------------------------------
// Query / Execute
// ---------------------------------------------------------------------------

/// How a query entry point runs its statement.
#[derive(Clone, Copy)]
enum RunMode {
    /// Own read transaction (`graphdb_query*`).
    Read,
    /// Own write transaction (`graphdb_execute*`).
    Write,
    /// Inside the handle's active transaction, or auto-committed when none
    /// is open (`graphdb_tx_execute*`).
    InTx,
}

/// Shared body of every query/execute export: validate the arguments, parse
/// the optional JSON params, run, and hand back a result handle.
unsafe fn run_query(
    db: *mut GraphDB,
    cypher: *const c_char,
    params_json: *const c_char,
    out: *mut *mut GraphResult,
    mode: RunMode,
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
    let params = if params_json.is_null() {
        None
    } else {
        let text = match unsafe { CStr::from_ptr(params_json) }.to_str() {
            Ok(s) => s,
            Err(e) => {
                set_error(&format!("invalid UTF-8 params: {e}"));
                return -1;
            }
        };
        match parse_params(text) {
            Ok(p) => Some(p),
            Err(msg) => {
                set_error(&msg);
                return -1;
            }
        }
    };
    let params = params.as_ref();

    let result = match mode {
        RunMode::Read => with_db(handle, |d| {
            let tx = d.read_tx()?;
            let records = tx.query_with_params(cypher_str, params)?;
            tx.commit()?;
            Ok(records)
        }),
        RunMode::Write => with_db(handle, |d| {
            let tx = d.write_tx()?;
            let records = tx.query_with_params(cypher_str, params)?;
            tx.commit()?;
            Ok(records)
        }),
        RunMode::InTx => with_tx_db(handle, |d| d.execute_with_params(cypher_str, params)),
    };

    wrap_result(result, |records| unsafe {
        *out = GraphResult::boxed(records);
    })
}

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
    catch_or(-1, || unsafe {
        run_query(db, cypher, ptr::null(), out, RunMode::Read)
    })
}

/// Execute a read-only Cypher query with parameters.
///
/// `params_json` is a JSON object mapping parameter names (without `$`) to
/// values, or NULL for no parameters. JSON values map to Cypher values as:
/// `null` -> null, `true`/`false` -> boolean, a number without fraction or
/// exponent that fits a signed 64-bit integer -> integer, any other number
/// -> float, string -> string, array -> list, object -> map. Integers that
/// do not fit a signed 64-bit integer are rejected. There are no temporal
/// literals: pass ISO strings and convert in Cypher (`date($d)`).
///
/// On success, writes a `GraphResult` handle to `*out`. The caller must free it
/// with `graphdb_result_free`.
#[no_mangle]
pub unsafe extern "C" fn graphdb_query_params(
    db: *mut GraphDB,
    cypher: *const c_char,
    params_json: *const c_char,
    out: *mut *mut GraphResult,
) -> i32 {
    catch_or(-1, || unsafe {
        run_query(db, cypher, params_json, out, RunMode::Read)
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
    catch_or(-1, || unsafe {
        run_query(db, cypher, ptr::null(), out, RunMode::Write)
    })
}

/// Execute a write Cypher query with parameters. `params_json` is a JSON
/// object or NULL; see `graphdb_query_params` for the type mapping.
#[no_mangle]
pub unsafe extern "C" fn graphdb_execute_params(
    db: *mut GraphDB,
    cypher: *const c_char,
    params_json: *const c_char,
    out: *mut *mut GraphResult,
) -> i32 {
    catch_or(-1, || unsafe {
        run_query(db, cypher, params_json, out, RunMode::Write)
    })
}

// ---------------------------------------------------------------------------
// Transaction API
// ---------------------------------------------------------------------------

/// Shared body of the four begin exports.
unsafe fn begin_tx(db: *mut GraphDB, write: bool, owner: Option<TxOwner>) -> i32 {
    if db.is_null() {
        set_error("null pointer argument");
        return -1;
    }
    let handle = unsafe { &*db };
    let owner = owner.unwrap_or(TxOwner::Thread(std::thread::current().id()));
    let result = with_db(handle, |d| {
        if write {
            d.begin_write()?;
        } else {
            d.begin_read()?;
        }
        *lock_owner(handle) = Some(owner);
        Ok(())
    });
    wrap_result(result, |()| {})
}

/// Begin a write transaction. The database handle must not be used for other
/// operations until the transaction is committed or rolled back.
///
/// The transaction belongs to the calling thread: transaction calls
/// (`graphdb_tx_execute*`, commit, rollback, index DDL) from other threads
/// fail until it ends.
#[no_mangle]
pub unsafe extern "C" fn graphdb_tx_begin_write(db: *mut GraphDB) -> i32 {
    catch_or(-1, || unsafe { begin_tx(db, true, None) })
}

/// Begin a read transaction. Owned by the calling thread, like
/// `graphdb_tx_begin_write`.
#[no_mangle]
pub unsafe extern "C" fn graphdb_tx_begin_read(db: *mut GraphDB) -> i32 {
    catch_or(-1, || unsafe { begin_tx(db, false, None) })
}

/// Begin a write transaction that any thread may use. For language
/// bindings whose own transaction objects decide who may use them (Go,
/// where goroutines move between OS threads). The caller is responsible
/// for not interleaving unrelated work into the transaction.
#[no_mangle]
pub unsafe extern "C" fn graphdb_tx_begin_write_shared(db: *mut GraphDB) -> i32 {
    catch_or(-1, || unsafe { begin_tx(db, true, Some(TxOwner::Any)) })
}

/// Begin a read transaction that any thread may use. See
/// `graphdb_tx_begin_write_shared`.
#[no_mangle]
pub unsafe extern "C" fn graphdb_tx_begin_read_shared(db: *mut GraphDB) -> i32 {
    catch_or(-1, || unsafe { begin_tx(db, false, Some(TxOwner::Any)) })
}

/// Execute a Cypher query within the current transaction.
///
/// A failed statement is rolled back on its own; the transaction stays
/// open and usable. With no transaction open, the statement runs in its
/// own auto-committed transaction.
#[no_mangle]
pub unsafe extern "C" fn graphdb_tx_execute(
    db: *mut GraphDB,
    cypher: *const c_char,
    out: *mut *mut GraphResult,
) -> i32 {
    catch_or(-1, || unsafe {
        run_query(db, cypher, ptr::null(), out, RunMode::InTx)
    })
}

/// Execute a Cypher query with parameters within the current transaction.
/// `params_json` is a JSON object or NULL; see `graphdb_query_params` for
/// the type mapping.
#[no_mangle]
pub unsafe extern "C" fn graphdb_tx_execute_params(
    db: *mut GraphDB,
    cypher: *const c_char,
    params_json: *const c_char,
    out: *mut *mut GraphResult,
) -> i32 {
    catch_or(-1, || unsafe {
        run_query(db, cypher, params_json, out, RunMode::InTx)
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
    catch_or(-1, || unsafe { graphdb_tx_execute(db, cypher, out) })
}

/// Commit the current transaction.
#[no_mangle]
pub unsafe extern "C" fn graphdb_tx_commit(db: *mut GraphDB) -> i32 {
    catch_or(-1, || {
        if db.is_null() {
            set_error("null pointer argument");
            return -1;
        }
        let handle = unsafe { &*db };
        let result = with_tx_db(handle, |d| d.commit());
        wrap_result(result, |()| {})
    })
}

/// Rollback the current transaction.
#[no_mangle]
pub unsafe extern "C" fn graphdb_tx_rollback(db: *mut GraphDB) -> i32 {
    catch_or(-1, || {
        if db.is_null() {
            set_error("null pointer argument");
            return -1;
        }
        let handle = unsafe { &*db };
        let result = with_tx_db(handle, |d| d.rollback());
        wrap_result(result, |()| {})
    })
}

// ---------------------------------------------------------------------------
// Result API
// ---------------------------------------------------------------------------

/// Get the number of rows in a query result.
#[no_mangle]
pub unsafe extern "C" fn graphdb_result_row_count(result: *const GraphResult) -> i64 {
    catch_or(0, || {
        if result.is_null() {
            return 0;
        }
        unsafe { &*result }.records.len() as i64
    })
}

/// Get the number of columns in a query result.
#[no_mangle]
pub unsafe extern "C" fn graphdb_result_column_count(result: *const GraphResult) -> i64 {
    catch_or(0, || {
        if result.is_null() {
            return 0;
        }
        let r = unsafe { &*result };
        r.records.first().map_or(0, |rec| rec.len()) as i64
    })
}

/// Get a column name by index. Returns NULL if out of bounds.
///
/// The returned pointer is valid until `graphdb_result_free` is called.
#[no_mangle]
pub unsafe extern "C" fn graphdb_result_column_name(
    result: *mut GraphResult,
    col: i64,
) -> *const c_char {
    catch_or(ptr::null(), || {
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
    })
}

/// Get a value as a string representation. Returns NULL if out of bounds.
///
/// The returned pointer is owned by the result and stays valid until
/// `graphdb_result_free` is called on it, independent of other calls (asking
/// for the same cell again returns the same pointer). Floats keep a
/// fractional part (`1.0`); paths render as
/// `{nodes: [...], relationships: [...]}`.
#[no_mangle]
pub unsafe extern "C" fn graphdb_result_value_str(
    result: *const GraphResult,
    row: i64,
    col: i64,
) -> *const c_char {
    catch_or(ptr::null(), || {
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

        let key = (row_idx, col_idx);
        let mut strs = r.value_strs.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(s) = strs.get(&key) {
            return s.as_ptr();
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
        // The CString's heap buffer does not move when it is stored, so the
        // pointer stays valid for the result's lifetime.
        let ptr = cstr.as_ptr();
        strs.insert(key, cstr);
        ptr
    })
}

/// Get the type of a value. Returns one of:
/// 0 = null, 1 = bool, 2 = i64, 3 = f64, 4 = string, 5 = list, 6 = path,
/// 7 = map, 8 = node, 9 = relationship, 10 = date, 11 = local time,
/// 12 = time, 13 = local datetime, 14 = datetime, 15 = duration.
/// Out-of-range cells report 0 (null).
#[no_mangle]
pub unsafe extern "C" fn graphdb_result_value_type(
    result: *const GraphResult,
    row: i64,
    col: i64,
) -> i32 {
    catch_or(-1, || {
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
    })
}

/// Get an integer value. Returns 0 if the value is not an integer.
#[no_mangle]
pub unsafe extern "C" fn graphdb_result_value_i64(
    result: *const GraphResult,
    row: i64,
    col: i64,
) -> i64 {
    catch_or(0, || {
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
    })
}

/// Get a float value. Returns 0.0 if the value is not a float.
#[no_mangle]
pub unsafe extern "C" fn graphdb_result_value_f64(
    result: *const GraphResult,
    row: i64,
    col: i64,
) -> f64 {
    catch_or(0.0, || {
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
    })
}

/// Get a boolean value. Returns 0 (false) if the value is not a boolean.
#[no_mangle]
pub unsafe extern "C" fn graphdb_result_value_bool(
    result: *const GraphResult,
    row: i64,
    col: i64,
) -> i32 {
    catch_or(-1, || {
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
    })
}

/// Get the full result as a JSON string.
///
/// The returned pointer is valid until `graphdb_result_free` is called.
#[no_mangle]
pub unsafe extern "C" fn graphdb_result_json(result: *mut GraphResult) -> *const c_char {
    catch_or(ptr::null(), || {
        if result.is_null() {
            return ptr::null();
        }
        let r = unsafe { &mut *result };

        if r.json.is_none() {
            let json = records_to_json(&r.records);
            r.json = CString::new(json).ok();
        }

        r.json.as_ref().map_or(ptr::null(), |s| s.as_ptr())
    })
}

/// Free a query result.
///
/// After this call, the pointer is invalid. Passing NULL is a no-op.
#[no_mangle]
pub unsafe extern "C" fn graphdb_result_free(result: *mut GraphResult) {
    catch_or((), || {
        if !result.is_null() {
            unsafe { drop(Box::from_raw(result)) };
        }
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Metadata keys of a node object (`__id`, `__labels`). A user property
/// with one of these names is left out of the object: metadata wins.
const NODE_META_KEYS: [&str; 2] = ["__id", "__labels"];

/// Metadata keys of a relationship object. `__label` is a deprecated alias
/// of `__type`, kept for 0.1.x callers.
const EDGE_META_KEYS: [&str; 5] = ["__src", "__dst", "__type", "__label", "__edge_seq"];

/// Parse a JSON params object into named Cypher values. See
/// `graphdb_query_params` for the type mapping.
fn parse_params(text: &str) -> Result<HashMap<String, Value>, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("invalid params JSON: {e}"))?;
    let serde_json::Value::Object(map) = parsed else {
        return Err("invalid params JSON: expected an object of name -> value".to_string());
    };
    map.into_iter()
        .map(|(k, v)| Ok((k.clone(), json_to_value(v, &k)?)))
        .collect()
}

/// Convert one JSON value to a Cypher value; `name` labels errors.
fn json_to_value(v: serde_json::Value, name: &str) -> Result<Value, String> {
    use serde_json::Value as J;
    Ok(match v {
        J::Null => Value::Null,
        J::Bool(b) => Value::Bool(b),
        J::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::I64(i)
            } else if n.is_u64() {
                return Err(format!(
                    "invalid params JSON: parameter `{name}` is an integer outside the \
                     signed 64-bit range"
                ));
            } else {
                // Not an integer: a float (serde_json parses every other
                // number as f64).
                Value::F64(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        J::String(s) => Value::String(s),
        J::Array(items) => Value::List(
            items
                .into_iter()
                .map(|item| json_to_value(item, name))
                .collect::<Result<_, _>>()?,
        ),
        J::Object(map) => Value::Map(
            map.into_iter()
                .map(|(k, v)| Ok((k, json_to_value(v, name)?)))
                .collect::<Result<_, String>>()?,
        ),
    })
}

/// Render a float so it never reads as an integer (`1.0`, not `1`).
fn format_f64(n: f64) -> String {
    // `{:?}` keeps a fractional part and uses exponent notation for very
    // large / small magnitudes, both valid JSON for finite values.
    format!("{n:?}")
}

fn format_value(val: &Value) -> String {
    match val {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::I64(n) => n.to_string(),
        Value::F64(n) => format_f64(*n),
        Value::String(s) => s.clone(),
        Value::List(items) => {
            let parts: Vec<String> = items.iter().map(format_value).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Path(p) => {
            let nodes: Vec<String> = p
                .nodes
                .iter()
                .map(|n| format_value(&Value::Node(n.clone())))
                .collect();
            let rels: Vec<String> = p
                .edges
                .iter()
                .map(|e| format_value(&Value::Edge(e.clone())))
                .collect();
            format!(
                "{{nodes: [{}], relationships: [{}]}}",
                nodes.join(", "),
                rels.join(", ")
            )
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

/// Append `, "key": value` for every user property whose name is not a
/// metadata key.
fn props_to_json(out: &mut String, props: &graphdblite::Properties, meta: &[&str]) {
    for (k, v) in props {
        if meta.contains(&k.as_str()) {
            continue;
        }
        out.push_str(", \"");
        json_escape_into(out, k);
        out.push_str("\": ");
        value_to_json(out, v);
    }
}

fn node_to_json(out: &mut String, n: &graphdblite::Node) {
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
    props_to_json(out, &n.properties, &NODE_META_KEYS);
    out.push('}');
}

fn edge_to_json(out: &mut String, e: &graphdblite::Edge) {
    out.push('{');
    out.push_str("\"__src\": ");
    out.push_str(&e.src.0.to_string());
    out.push_str(", \"__dst\": ");
    out.push_str(&e.dst.0.to_string());
    for key in ["__type", "__label"] {
        out.push_str(", \"");
        out.push_str(key);
        out.push_str("\": \"");
        json_escape_into(out, &e.label);
        out.push('"');
    }
    out.push_str(", \"__edge_seq\": ");
    out.push_str(&e.seq.to_string());
    props_to_json(out, &e.properties, &EDGE_META_KEYS);
    out.push('}');
}

fn value_to_json(out: &mut String, val: &Value) {
    match val {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::I64(n) => out.push_str(&n.to_string()),
        // NaN and infinities have no JSON representation.
        Value::F64(n) if !n.is_finite() => out.push_str("null"),
        Value::F64(n) => out.push_str(&format_f64(*n)),
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
            out.push_str("{\"nodes\": [");
            for (i, n) in p.nodes.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                node_to_json(out, n);
            }
            out.push_str("], \"relationships\": [");
            for (i, e) in p.edges.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                edge_to_json(out, e);
            }
            out.push_str("]}");
        }
        Value::Node(n) => node_to_json(out, n),
        Value::Edge(e) => edge_to_json(out, e),
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
        // Temporal types: serialize as quoted ISO strings in JSON.
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
    fn edge_json_carries_parallel_edge_seq() {
        let e = graphdblite::Edge {
            src: graphdblite::NodeId(1),
            dst: graphdblite::NodeId(2),
            label: "R".to_string(),
            seq: 3,
            properties: Default::default(),
        };
        let mut out = String::new();
        value_to_json(&mut out, &Value::Edge(e));
        assert_eq!(
            out,
            r#"{"__src": 1, "__dst": 2, "__type": "R", "__label": "R", "__edge_seq": 3}"#
        );
    }

    #[test]
    fn catch_passes_through_ok() {
        let r: Result<i32, GraphError> = catch(|| Ok(42));
        assert_eq!(r.unwrap(), 42);
    }

    // `catch_or` returns the fallback and records the panic as last error.
    #[test]
    fn catch_or_returns_fallback_and_sets_last_error() {
        let rc = catch_or(-1, || -> i32 { panic!("kaboom") });
        assert_eq!(rc, -1);
        let msg = unsafe { CStr::from_ptr(graphdb_last_error()) }
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(msg, "transaction error: internal panic: kaboom");
        assert_eq!(catch_or(-1, || 7), 7);
    }

    fn count_nodes(db: *mut GraphDB) -> i64 {
        unsafe {
            let q = CString::new("MATCH (n) RETURN count(n) AS c").unwrap();
            let mut result: *mut GraphResult = ptr::null_mut();
            assert_eq!(graphdb_query(db, q.as_ptr(), &mut result), 0);
            let c = graphdb_result_value_i64(result, 0, 0);
            graphdb_result_free(result);
            c
        }
    }

    // A panic in the middle of a call rolls back the transaction it
    // interrupted, does not poison the handle's mutex, and leaves the
    // handle usable.
    #[test]
    fn panic_inside_a_call_leaves_the_handle_usable() {
        unsafe {
            let mut db: *mut GraphDB = ptr::null_mut();
            assert_eq!(graphdb_open_memory(&mut db), 0);
            let handle = &*db;

            assert_eq!(graphdb_tx_begin_write(db), 0);
            let create = CString::new("CREATE (:A)").unwrap();
            let mut result: *mut GraphResult = ptr::null_mut();
            assert_eq!(graphdb_tx_execute(db, create.as_ptr(), &mut result), 0);
            graphdb_result_free(result);

            let r: Result<(), GraphError> = with_db(handle, |d| {
                d.execute("CREATE (:B)")?;
                panic!("boom")
            });
            let err = r.expect_err("the panic must surface as an error");
            assert_eq!(err.to_string(), "transaction error: internal panic: boom");
            assert!(!handle.db.is_poisoned());

            // The interrupted transaction (A and B) was rolled back...
            assert_eq!(count_nodes(db), 0);
            // ...so no transaction is left open: a new one can begin, and
            // its writes commit normally.
            assert_eq!(graphdb_tx_begin_write(db), 0);
            assert_eq!(graphdb_tx_execute(db, create.as_ptr(), &mut result), 0);
            graphdb_result_free(result);
            assert_eq!(graphdb_tx_commit(db), 0);
            assert_eq!(count_nodes(db), 1);

            graphdb_close(db);
        }
    }

    // A mutex poisoned some other way (a panic while the lock was held) is
    // recovered on the next call instead of failing it.
    #[test]
    fn poisoned_mutex_is_recovered() {
        unsafe {
            let mut db: *mut GraphDB = ptr::null_mut();
            assert_eq!(graphdb_open_memory(&mut db), 0);
            let handle = &*db;
            assert_eq!(graphdb_tx_begin_write(db), 0);
            let poison = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _guard = handle.db.lock().unwrap();
                panic!("poison");
            }));
            assert!(poison.is_err());
            assert!(handle.db.is_poisoned());

            assert_eq!(count_nodes(db), 0);
            assert!(!handle.db.is_poisoned());
            // The transaction open at the time was rolled back on recovery.
            assert_eq!(graphdb_tx_begin_write(db), 0);
            assert_eq!(graphdb_tx_rollback(db), 0);
            graphdb_close(db);
        }
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

#[cfg(test)]
mod ws_f_tests {
    use super::*;

    fn open_mem() -> *mut GraphDB {
        let mut db: *mut GraphDB = ptr::null_mut();
        assert_eq!(unsafe { graphdb_open_memory(&mut db) }, 0);
        db
    }

    fn query(db: *mut GraphDB, q: &str) -> *mut GraphResult {
        let q = CString::new(q).unwrap();
        let mut res: *mut GraphResult = ptr::null_mut();
        let rc = unsafe { graphdb_query(db, q.as_ptr(), &mut res) };
        assert_eq!(rc, 0, "query failed: {}", last_error_string());
        res
    }

    fn last_error_string() -> String {
        let p = graphdb_last_error();
        if p.is_null() {
            return String::new();
        }
        unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_string()
    }

    fn json_of(v: &Value) -> String {
        let mut out = String::new();
        value_to_json(&mut out, v);
        out
    }

    // F5 / D7: every `graphdb_result_value_str` pointer stays valid until
    // its result is freed, across cells and across results.
    #[test]
    fn value_str_pointers_live_until_result_free() {
        let db = open_mem();
        unsafe {
            let r1 = query(db, "RETURN 'aaaa' AS x, 'bbbb' AS y");
            let r2 = query(db, "RETURN 'cccc' AS x");
            let a = graphdb_result_value_str(r1, 0, 0);
            let b = graphdb_result_value_str(r1, 0, 1);
            let c = graphdb_result_value_str(r2, 0, 0);
            // Asking again for a cell returns the same live string.
            let a2 = graphdb_result_value_str(r1, 0, 0);
            assert_eq!(CStr::from_ptr(a).to_str().unwrap(), "aaaa");
            assert_eq!(CStr::from_ptr(b).to_str().unwrap(), "bbbb");
            assert_eq!(CStr::from_ptr(c).to_str().unwrap(), "cccc");
            assert_eq!(a, a2);
            graphdb_result_free(r2);
            assert_eq!(CStr::from_ptr(a).to_str().unwrap(), "aaaa");
            graphdb_result_free(r1);
            graphdb_close(db);
        }
    }

    // F11: floats keep their fractional part; non-finite floats become
    // JSON null (NaN / inf are not valid JSON).
    #[test]
    fn json_floats_are_valid_json() {
        assert_eq!(json_of(&Value::F64(1.0)), "1.0");
        assert_eq!(json_of(&Value::F64(-0.5)), "-0.5");
        assert_eq!(json_of(&Value::F64(1e300)), "1e300");
        assert_eq!(json_of(&Value::F64(f64::NAN)), "null");
        assert_eq!(json_of(&Value::F64(f64::INFINITY)), "null");
        assert_eq!(json_of(&Value::F64(f64::NEG_INFINITY)), "null");
        assert_eq!(json_of(&Value::I64(1)), "1");
    }

    // F11: the string form keeps floats distinguishable from integers.
    #[test]
    fn value_str_keeps_float_fraction() {
        assert_eq!(format_value(&Value::F64(1.0)), "1.0");
        assert_eq!(format_value(&Value::F64(2.5)), "2.5");
        assert_eq!(format_value(&Value::I64(1)), "1");
    }

    // F11: a whole result with floats round-trips through a strict parser.
    #[test]
    fn result_json_parses_with_non_finite_floats() {
        let db = open_mem();
        unsafe {
            let r = query(db, "RETURN 1.0 AS f, toFloat('NaN') AS n, [2.0, 3] AS l");
            let json = CStr::from_ptr(graphdb_result_json(r)).to_str().unwrap();
            let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
            assert_eq!(
                parsed,
                serde_json::json!([{"f": 1.0, "n": null, "l": [2.0, 3]}])
            );
            assert!(parsed[0]["f"].is_f64());
            assert!(parsed[0]["l"][1].is_i64());
            graphdb_result_free(r);
            graphdb_close(db);
        }
    }

    fn node(id: u64, label: &str) -> graphdblite::Node {
        graphdblite::Node {
            id: graphdblite::NodeId(id),
            labels: vec![label.to_string()],
            properties: Default::default(),
        }
    }

    // D1: a path serializes as {nodes, relationships}, not a node-id list.
    #[test]
    fn path_json_has_nodes_and_relationships() {
        let path = graphdblite::PathValue {
            nodes: vec![node(1, "A"), node(2, "B")],
            edges: vec![graphdblite::Edge {
                src: graphdblite::NodeId(1),
                dst: graphdblite::NodeId(2),
                label: "R".to_string(),
                seq: 0,
                properties: Default::default(),
            }],
        };
        let v = Value::Path(path);
        assert_eq!(
            json_of(&v),
            r#"{"nodes": [{"__id": 1, "__labels": ["A"]}, {"__id": 2, "__labels": ["B"]}], "relationships": [{"__src": 1, "__dst": 2, "__type": "R", "__label": "R", "__edge_seq": 0}]}"#
        );
        assert_eq!(
            format_value(&v),
            "{nodes: [(:A #1), (:B #2)], relationships: [[:R 1->2]]}"
        );
    }

    // D1 end to end: a path returned by a query.
    #[test]
    fn path_query_json() {
        let db = open_mem();
        unsafe {
            let c = CString::new("CREATE (:A)-[:R {w: 1}]->(:B)").unwrap();
            let mut res: *mut GraphResult = ptr::null_mut();
            assert_eq!(graphdb_execute(db, c.as_ptr(), &mut res), 0);
            graphdb_result_free(res);
            let r = query(db, "MATCH p = (:A)-->(:B) RETURN p");
            assert_eq!(graphdb_result_value_type(r, 0, 0), 6);
            let json = CStr::from_ptr(graphdb_result_json(r)).to_str().unwrap();
            let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
            let p = &parsed[0]["p"];
            assert_eq!(p["nodes"][0]["__labels"], serde_json::json!(["A"]));
            assert_eq!(p["nodes"][1]["__labels"], serde_json::json!(["B"]));
            assert_eq!(p["relationships"][0]["__type"], "R");
            assert_eq!(p["relationships"][0]["w"], 1);
            assert_eq!(p["relationships"].as_array().unwrap().len(), 1);
            graphdb_result_free(r);
            graphdb_close(db);
        }
    }

    // F14: user properties never overwrite or duplicate metadata keys.
    #[test]
    fn metadata_keys_win_over_user_properties() {
        let mut props = graphdblite::Properties::new();
        props.insert("__src".to_string(), Value::I64(99));
        props.insert("w".to_string(), Value::I64(1));
        let e = graphdblite::Edge {
            src: graphdblite::NodeId(1),
            dst: graphdblite::NodeId(2),
            label: "R".to_string(),
            seq: 0,
            properties: props.clone(),
        };
        assert_eq!(
            json_of(&Value::Edge(e)),
            r#"{"__src": 1, "__dst": 2, "__type": "R", "__label": "R", "__edge_seq": 0, "w": 1}"#
        );
        let mut nprops = graphdblite::Properties::new();
        nprops.insert("__id".to_string(), Value::I64(99));
        nprops.insert("__labels".to_string(), Value::I64(7));
        nprops.insert("k".to_string(), Value::Bool(true));
        let n = graphdblite::Node {
            id: graphdblite::NodeId(5),
            labels: vec!["L".to_string()],
            properties: nprops,
        };
        assert_eq!(
            json_of(&Value::Node(n)),
            r#"{"__id": 5, "__labels": ["L"], "k": true}"#
        );
    }

    /// Raw handle pointer that may be moved to another thread in a test.
    #[derive(Clone, Copy)]
    struct SendPtr(*mut GraphDB);
    unsafe impl Send for SendPtr {}

    fn tx_execute_on_other_thread(db: *mut GraphDB, q: &'static str) -> (i32, String) {
        let p = SendPtr(db);
        std::thread::spawn(move || {
            let p = p;
            let c = CString::new(q).unwrap();
            let mut res: *mut GraphResult = ptr::null_mut();
            let rc = unsafe { graphdb_tx_execute(p.0, c.as_ptr(), &mut res) };
            if !res.is_null() {
                unsafe { graphdb_result_free(res) };
            }
            (rc, last_error_string())
        })
        .join()
        .unwrap()
    }

    fn count(db: *mut GraphDB, label: &str) -> i64 {
        let r = query(db, &format!("MATCH (n:{label}) RETURN count(n) AS c"));
        let n = unsafe { graphdb_result_value_i64(r, 0, 0) };
        unsafe { graphdb_result_free(r) };
        n
    }

    // F8: a transaction begun on one thread cannot be used by another.
    #[test]
    fn tx_is_owned_by_the_beginning_thread() {
        let db = open_mem();
        unsafe {
            assert_eq!(graphdb_tx_begin_write(db), 0);
            let (rc, err) = tx_execute_on_other_thread(db, "CREATE (:B)");
            assert_eq!(rc, -1);
            assert_eq!(
                err,
                "transaction error: the active transaction belongs to another thread\n  \
                 hint: use a transaction only from the thread that began it, or begin it \
                 with graphdb_tx_begin_write_shared / graphdb_tx_begin_read_shared"
            );
            let other = SendPtr(db);
            let rc = std::thread::spawn(move || {
                let p = other;
                graphdb_tx_commit(p.0)
            })
            .join()
            .unwrap();
            assert_eq!(rc, -1, "commit from another thread must be rejected");
            // The owner can still use and roll back its transaction.
            let c = CString::new("CREATE (:A)").unwrap();
            let mut res: *mut GraphResult = ptr::null_mut();
            assert_eq!(graphdb_tx_execute(db, c.as_ptr(), &mut res), 0);
            graphdb_result_free(res);
            assert_eq!(graphdb_tx_rollback(db), 0);
            assert_eq!(count(db, "A"), 0);
            assert_eq!(count(db, "B"), 0);
            // Once the transaction ended, other threads may use the handle.
            let (rc, err) = tx_execute_on_other_thread(db, "CREATE (:B)");
            assert_eq!(rc, 0, "{err}");
            assert_eq!(count(db, "B"), 1);
            graphdb_close(db);
        }
    }

    fn query_params(db: *mut GraphDB, q: &str, params: &str) -> (i32, *mut GraphResult) {
        let q = CString::new(q).unwrap();
        let p = CString::new(params).unwrap();
        let mut res: *mut GraphResult = ptr::null_mut();
        let rc = unsafe { graphdb_query_params(db, q.as_ptr(), p.as_ptr(), &mut res) };
        (rc, res)
    }

    // D7: JSON params map to Cypher values, including nested lists/maps.
    #[test]
    fn query_params_maps_json_types() {
        let db = open_mem();
        unsafe {
            let (rc, r) = query_params(
                db,
                "RETURN $i AS i, $f AS f, $s AS s, $b AS b, $n AS n, $l AS l, $m.k[1] AS mk",
                r#"{"i": 7, "f": 1.5, "s": "x", "b": true, "n": null,
                    "l": [1, "two", [3.0]], "m": {"k": [10, 20]}}"#,
            );
            assert_eq!(rc, 0, "{}", last_error_string());
            assert_eq!(graphdb_result_value_type(r, 0, 0), 2);
            assert_eq!(graphdb_result_value_i64(r, 0, 0), 7);
            assert_eq!(graphdb_result_value_type(r, 0, 1), 3);
            assert_eq!(graphdb_result_value_f64(r, 0, 1), 1.5);
            assert_eq!(
                CStr::from_ptr(graphdb_result_value_str(r, 0, 2))
                    .to_str()
                    .unwrap(),
                "x"
            );
            assert_eq!(graphdb_result_value_bool(r, 0, 3), 1);
            assert_eq!(graphdb_result_value_type(r, 0, 4), 0);
            assert_eq!(
                CStr::from_ptr(graphdb_result_value_str(r, 0, 5))
                    .to_str()
                    .unwrap(),
                "[1, two, [3.0]]"
            );
            assert_eq!(graphdb_result_value_i64(r, 0, 6), 20);
            graphdb_result_free(r);
            graphdb_close(db);
        }
    }

    // D7: execute and tx variants take params too; NULL params means none.
    #[test]
    fn execute_and_tx_params() {
        let db = open_mem();
        unsafe {
            let q = CString::new("CREATE (:P {name: $name})").unwrap();
            let p = CString::new(r#"{"name": "Ann"}"#).unwrap();
            let mut res: *mut GraphResult = ptr::null_mut();
            assert_eq!(
                graphdb_execute_params(db, q.as_ptr(), p.as_ptr(), &mut res),
                0
            );
            graphdb_result_free(res);
            assert_eq!(graphdb_tx_begin_write(db), 0);
            let p2 = CString::new(r#"{"name": "Bob"}"#).unwrap();
            assert_eq!(
                graphdb_tx_execute_params(db, q.as_ptr(), p2.as_ptr(), &mut res),
                0
            );
            graphdb_result_free(res);
            assert_eq!(graphdb_tx_commit(db), 0);
            let q = CString::new("MATCH (p:P) RETURN p.name AS n ORDER BY n").unwrap();
            assert_eq!(
                graphdb_query_params(db, q.as_ptr(), ptr::null(), &mut res),
                0
            );
            assert_eq!(graphdb_result_row_count(res), 2);
            assert_eq!(
                CStr::from_ptr(graphdb_result_value_str(res, 0, 0))
                    .to_str()
                    .unwrap(),
                "Ann"
            );
            assert_eq!(
                CStr::from_ptr(graphdb_result_value_str(res, 1, 0))
                    .to_str()
                    .unwrap(),
                "Bob"
            );
            graphdb_result_free(res);
            graphdb_close(db);
        }
    }

    // D7: malformed params are rejected with a specific message.
    #[test]
    fn bad_params_are_rejected() {
        let db = open_mem();
        let cases = [
            ("{", "invalid params JSON: EOF while parsing an object at line 1 column 1"),
            ("[1]", "invalid params JSON: expected an object of name -> value"),
            (
                r#"{"big": 9223372036854775808}"#,
                "invalid params JSON: parameter `big` is an integer outside the signed 64-bit range",
            ),
        ];
        for (params, want) in cases {
            let (rc, r) = query_params(db, "RETURN 1 AS x", params);
            assert_eq!(rc, -1, "params {params} accepted");
            assert!(r.is_null());
            assert_eq!(last_error_string(), want);
        }
        // Boundary: i64::MAX and i64::MIN are accepted as integers.
        let (rc, r) = query_params(
            db,
            "RETURN $a AS a, $b AS b",
            r#"{"a": 9223372036854775807, "b": -9223372036854775808}"#,
        );
        assert_eq!(rc, 0, "{}", last_error_string());
        unsafe {
            assert_eq!(graphdb_result_value_i64(r, 0, 0), i64::MAX);
            assert_eq!(graphdb_result_value_i64(r, 0, 1), i64::MIN);
            graphdb_result_free(r);
        }
        // A missing parameter is the core's error.
        let (rc, _) = query_params(db, "RETURN $nope AS x", "{}");
        assert_eq!(rc, -1);
        assert_eq!(
            last_error_string(),
            "ArgumentError(Other) at semantic analysis: missing parameter: $nope"
        );
        unsafe { graphdb_close(db) };
    }

    // D3 through the FFI: a failed statement inside a transaction is undone
    // alone and the transaction stays usable.
    #[test]
    fn failed_statement_in_tx_is_rolled_back_alone() {
        let db = open_mem();
        unsafe {
            assert_eq!(graphdb_tx_begin_write(db), 0);
            let mut res: *mut GraphResult = ptr::null_mut();
            let ok = CString::new("CREATE (:K)").unwrap();
            assert_eq!(graphdb_tx_execute(db, ok.as_ptr(), &mut res), 0);
            graphdb_result_free(res);
            let bad =
                CString::new("UNWIND [1, 'a'] AS i CREATE (:M {v: 1}) RETURN i - 1 AS x").unwrap();
            res = ptr::null_mut();
            assert_eq!(graphdb_tx_execute(db, bad.as_ptr(), &mut res), -1);
            assert!(
                last_error_string().starts_with("TypeError("),
                "{}",
                last_error_string()
            );
            assert_eq!(graphdb_tx_commit(db), 0);
            assert_eq!(count(db, "K"), 1);
            assert_eq!(count(db, "M"), 0);
            graphdb_close(db);
        }
    }

    // F8: a shared transaction may be used from any thread.
    #[test]
    fn shared_tx_accepts_other_threads() {
        let db = open_mem();
        unsafe {
            assert_eq!(graphdb_tx_begin_write_shared(db), 0);
            let (rc, err) = tx_execute_on_other_thread(db, "CREATE (:B)");
            assert_eq!(rc, 0, "{err}");
            assert_eq!(graphdb_tx_commit(db), 0);
            assert_eq!(count(db, "B"), 1);
            assert_eq!(graphdb_tx_begin_read_shared(db), 0);
            let (rc, err) = tx_execute_on_other_thread(db, "MATCH (b:B) RETURN b");
            assert_eq!(rc, 0, "{err}");
            assert_eq!(graphdb_tx_commit(db), 0);
            graphdb_close(db);
        }
    }
}
