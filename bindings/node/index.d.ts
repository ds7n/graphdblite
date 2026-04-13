/** Embedded graph database with Cypher query support. */
export class Database {
  /** Open a database at the given file path. */
  constructor(path: string);

  /** Open a database with a custom busy timeout (milliseconds). */
  static openWithTimeout(path: string, busyTimeoutMs: number): Database;

  /** Open an in-memory database (for testing). */
  static openMemory(): Database;

  /** Execute a read-only Cypher query. Returns an array of objects. */
  query(cypher: string): Record<string, unknown>[];

  /** Execute a write Cypher query (CREATE, DELETE, SET, MERGE). */
  execute(cypher: string): Record<string, unknown>[];

  /** Close the database connection. */
  close(): void;
}

/** A read-write transaction. */
export class WriteTransaction {
  /** Execute a Cypher query within this transaction. */
  execute(cypher: string): Record<string, unknown>[];

  /** Execute a read-only Cypher query within this transaction. */
  query(cypher: string): Record<string, unknown>[];

  /** Commit the transaction. */
  commit(): void;

  /** Rollback the transaction. */
  rollback(): void;
}

/** A read-only transaction. */
export class ReadTransaction {
  /** Execute a read-only Cypher query within this transaction. */
  query(cypher: string): Record<string, unknown>[];

  /** Commit (release) the read transaction. */
  commit(): void;
}
