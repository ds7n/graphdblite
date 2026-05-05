/**
 * JS-side ergonomics layered on top of the NAPI-RS-generated bindings:
 *
 *   - `db.withWriteTx(fn)` / `db.withReadTx(fn)` — callback wrappers that
 *     commit on success and roll back on thrown errors. `fn` may be sync
 *     or async; returns whatever `fn` returns.
 *
 *   - `Symbol.dispose` on `WriteTransaction` / `ReadTransaction` — lets
 *     Node 22+ callers use `using tx = db.beginWrite();` to roll back at
 *     scope exit if the transaction wasn't already committed/rolled back.
 */

const native = require('./index.js')

const { Database, WriteTransaction, ReadTransaction } = native

async function _runInTx(tx, fn) {
  let committed = false
  try {
    const result = await fn(tx)
    tx.commit()
    committed = true
    return result
  } finally {
    if (!committed) {
      try {
        tx.rollback()
      } catch {
        // already finalized — best-effort rollback
      }
    }
  }
}

Database.prototype.withWriteTx = function withWriteTx(fn) {
  return _runInTx(this.beginWrite(), fn)
}

Database.prototype.withReadTx = function withReadTx(fn) {
  return _runInTx(this.beginRead(), fn)
}

// `using` / `await using` support — Node 22+ recognizes these as
// disposable resources. Rolls back if the txn wasn't already finalized.
function _installDispose(cls) {
  const sym = typeof Symbol.dispose === 'symbol' ? Symbol.dispose : null
  if (!sym || cls.prototype[sym]) return
  cls.prototype[sym] = function dispose() {
    try {
      this.rollback()
    } catch {
      // already committed or rolled back
    }
  }
}

_installDispose(WriteTransaction)
_installDispose(ReadTransaction)

module.exports = native
