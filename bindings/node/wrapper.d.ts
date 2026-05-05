/* tslint:disable */
/* eslint-disable */

export * from './index'

import { Database, WriteTransaction, ReadTransaction } from './index'

declare module './index' {
  interface Database {
    /**
     * Run `fn` inside a write transaction. Commits on success,
     * rolls back if `fn` throws. `fn` may be async.
     */
    withWriteTx<T>(fn: (tx: WriteTransaction) => T | Promise<T>): Promise<T>

    /**
     * Run `fn` inside a read transaction. Commits (releases) on success,
     * rolls back if `fn` throws. `fn` may be async.
     */
    withReadTx<T>(fn: (tx: ReadTransaction) => T | Promise<T>): Promise<T>
  }

  interface WriteTransaction {
    /** `using` / `await using` support — rolls back if not finalized. */
    [Symbol.dispose](): void
  }

  interface ReadTransaction {
    /** `using` / `await using` support — rolls back if not finalized. */
    [Symbol.dispose](): void
  }
}
