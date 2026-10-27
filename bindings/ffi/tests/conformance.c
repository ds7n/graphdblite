/* Binding conformance harness — see ../../docs/BINDING_CONFORMANCE.md.
 *
 * Each scenario lives in its own static function named bc_NN_*. Failures
 * print the [BC-NN] tag so the failing checklist row is obvious. The
 * program exits 0 only when every scenario passes.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/stat.h>

#include "../graphdblite.h"

/* Each bc_* function returns 0 on pass, 1 on fail. main() aggregates. */
typedef int (*scenario_fn)(const char *tmpdir);

static int fail(const char *tag, const char *msg) {
    fprintf(stderr, "[%s] FAIL: %s", tag, msg);
    const char *err = graphdb_last_error();
    if (err) fprintf(stderr, " (last error: %s)", err);
    fprintf(stderr, "\n");
    return 1;
}

static int pass(const char *tag) {
    fprintf(stdout, "[%s] PASS\n", tag);
    return 0;
}

/* Negative check: `rc` must be an error code AND graphdb_last_error() must
 * contain `want`, so a test cannot pass on an unrelated failure.
 * Returns 0 when both hold, else prints the mismatch and returns 1. */
static int expect_error(const char *tag, int rc, const char *want, const char *what) {
    if (rc == 0) {
        fprintf(stderr, "[%s] FAIL: %s succeeded, expected error \"%s\"\n", tag, what, want);
        return 1;
    }
    const char *err = graphdb_last_error();
    if (!err || !strstr(err, want)) {
        fprintf(stderr, "[%s] FAIL: %s: expected error containing \"%s\", got \"%s\"\n",
                tag, what, want, err ? err : "(null)");
        return 1;
    }
    return 0;
}

static const char *NESTED_ERR = "a Write transaction is already active; nested transactions are not supported";
static const char *NESTED_READ_ERR = "begin_read: a Write transaction is already active";
static const char *READ_ONLY_ERR = "write operations are not permitted inside a read transaction";

/* Open a fresh DB at <tmpdir>/<name>. Caller must close. */
static GraphDB *fresh_db(const char *tmpdir, const char *name) {
    char path[1024];
    snprintf(path, sizeof(path), "%s/%s", tmpdir, name);
    /* Best-effort cleanup of any prior file from a crashed run. */
    unlink(path);
    GraphDB *db = NULL;
    if (graphdb_open(path, &db) != 0 || db == NULL) {
        fprintf(stderr, "fresh_db(%s) failed: %s\n", path, graphdb_last_error());
        return NULL;
    }
    return db;
}

/* Run a query; return the i64 value at (0,0) or -1 on error. */
static long long count_persons(GraphDB *db) {
    GraphResult *res = NULL;
    if (graphdb_query(db, "MATCH (n:Person) RETURN count(n) AS c", &res) != 0) {
        return -1;
    }
    long long n = graphdb_result_value_i64(res, 0, 0);
    graphdb_result_free(res);
    return n;
}

/* ----------------------------------------------------------------------- */
/* BC-01 — explicit rollback discards writes                                */
/* ----------------------------------------------------------------------- */
static int bc_01_explicit_rollback(const char *tmpdir) {
    const char *tag = "BC-01";
    GraphDB *db = fresh_db(tmpdir, "bc01.sqlite");
    if (!db) return fail(tag, "open");
    int rc = 1;
    GraphResult *res = NULL;
    if (graphdb_tx_begin_write(db) != 0) { rc = fail(tag, "begin_write"); goto out; }
    if (graphdb_tx_execute(db, "CREATE (n:Person {name: 'Alice'})", &res) != 0) {
        rc = fail(tag, "tx_execute"); goto out;
    }
    graphdb_result_free(res); res = NULL;
    if (graphdb_tx_rollback(db) != 0) { rc = fail(tag, "rollback"); goto out; }
    if (count_persons(db) != 0) { rc = fail(tag, "data leaked through rollback"); goto out; }
    rc = pass(tag);
out:
    if (res) graphdb_result_free(res);
    graphdb_close(db);
    return rc;
}

/* ----------------------------------------------------------------------- */
/* BC-02 — commit persists writes                                          */
/* ----------------------------------------------------------------------- */
static int bc_02_commit_persists(const char *tmpdir) {
    const char *tag = "BC-02";
    GraphDB *db = fresh_db(tmpdir, "bc02.sqlite");
    if (!db) return fail(tag, "open");
    int rc = 1;
    GraphResult *res = NULL;
    if (graphdb_tx_begin_write(db) != 0) { rc = fail(tag, "begin_write"); goto out; }
    if (graphdb_tx_execute(db, "CREATE (n:Person {name: 'Alice'})", &res) != 0) {
        rc = fail(tag, "tx_execute"); goto out;
    }
    graphdb_result_free(res); res = NULL;
    if (graphdb_tx_commit(db) != 0) { rc = fail(tag, "commit"); goto out; }
    if (count_persons(db) != 1) { rc = fail(tag, "commit did not persist"); goto out; }
    rc = pass(tag);
out:
    if (res) graphdb_result_free(res);
    graphdb_close(db);
    return rc;
}

/* ----------------------------------------------------------------------- */
/* BC-04 — nested begin returns an error                                   */
/* ----------------------------------------------------------------------- */
static int bc_04_nested_begin(const char *tmpdir) {
    const char *tag = "BC-04";
    GraphDB *db = fresh_db(tmpdir, "bc04.sqlite");
    if (!db) return fail(tag, "open");
    int rc = 1;
    GraphResult *res = NULL;
    if (graphdb_tx_begin_write(db) != 0) { rc = fail(tag, "begin_write"); goto out; }
    if (expect_error(tag, graphdb_tx_begin_write(db), NESTED_ERR, "nested begin_write")) goto out;
    if (expect_error(tag, graphdb_tx_begin_read(db), NESTED_READ_ERR, "nested begin_read")) goto out;
    /* First tx still usable */
    if (graphdb_tx_execute(db, "CREATE (n:Person {name: 'Alice'})", &res) != 0) {
        rc = fail(tag, "first tx Execute"); goto out;
    }
    graphdb_result_free(res); res = NULL;
    if (graphdb_tx_commit(db) != 0) { rc = fail(tag, "commit"); goto out; }
    if (count_persons(db) != 1) { rc = fail(tag, "first-tx writes lost"); goto out; }
    rc = pass(tag);
out:
    if (res) graphdb_result_free(res);
    graphdb_close(db);
    return rc;
}

/* ----------------------------------------------------------------------- */
/* BC-05 — write inside a read transaction returns an error                 */
/* ----------------------------------------------------------------------- */
static int bc_05_write_in_read_tx(const char *tmpdir) {
    const char *tag = "BC-05";
    GraphDB *db = fresh_db(tmpdir, "bc05.sqlite");
    if (!db) return fail(tag, "open");
    int rc = 1;
    GraphResult *res = NULL;
    if (graphdb_tx_begin_read(db) != 0) { rc = fail(tag, "begin_read"); goto out; }
    if (expect_error(tag, graphdb_tx_execute(db, "CREATE (n:Person {name: 'Alice'})", &res),
                     READ_ONLY_ERR, "write inside read tx")) {
        goto out;
    }
    /* The read transaction stays usable. */
    if (graphdb_tx_execute(db, "MATCH (n) RETURN count(n) AS c", &res) != 0) {
        rc = fail(tag, "read after rejected write"); goto out;
    }
    if (graphdb_result_value_i64(res, 0, 0) != 0) { rc = fail(tag, "unexpected count"); goto out; }
    rc = pass(tag);
out:
    if (res) graphdb_result_free(res);
    graphdb_tx_rollback(db);
    graphdb_close(db);
    return rc;
}

/* ----------------------------------------------------------------------- */
/* BC-06 — commit/rollback without an active txn returns an error          */
/* ----------------------------------------------------------------------- */
static int bc_06_commit_without_tx(const char *tmpdir) {
    const char *tag = "BC-06";
    GraphDB *db = fresh_db(tmpdir, "bc06.sqlite");
    if (!db) return fail(tag, "open");
    int rc = 1;
    if (expect_error(tag, graphdb_tx_commit(db), "no active transaction to commit", "commit-with-no-tx")) goto out;
    if (expect_error(tag, graphdb_tx_rollback(db), "no active transaction to roll back", "rollback-with-no-tx")) goto out;
    if (graphdb_tx_begin_write(db) != 0) { rc = fail(tag, "begin_write"); goto out; }
    if (graphdb_tx_commit(db) != 0) { rc = fail(tag, "commit"); goto out; }
    if (expect_error(tag, graphdb_tx_commit(db), "no active transaction to commit", "double-commit")) goto out;
    rc = pass(tag);
out:
    graphdb_close(db);
    return rc;
}

/* ----------------------------------------------------------------------- */
/* BC-07 — operations after the txn finishes (FFI variant)                  */
/* ----------------------------------------------------------------------- */
/* The FFI binding exposes a flat handle API: tx state lives on the GraphDB,
 * not on a per-tx object. After graphdb_tx_commit, graphdb_tx_execute on the
 * same handle implicitly auto-starts a fresh transaction (Database::execute
 * auto-tx — see CLAUDE.md). That's a deliberate quality-of-life difference
 * from the wrapped-tx-object bindings (Python, Node, Go), where the tx
 * object becomes invalid after commit.
 *
 * What still must error in the C binding: graphdb_tx_rollback after commit,
 * since there is genuinely no transaction to roll back. That is the
 * portion of BC-07 that maps cleanly onto the flat handle model.
 */
static int bc_07_use_after_commit(const char *tmpdir) {
    const char *tag = "BC-07";
    GraphDB *db = fresh_db(tmpdir, "bc07.sqlite");
    if (!db) return fail(tag, "open");
    int rc = 1;
    GraphResult *res = NULL;
    if (graphdb_tx_begin_write(db) != 0) { rc = fail(tag, "begin_write"); goto out; }
    if (graphdb_tx_execute(db, "CREATE (n:Person {name: 'A'})", &res) != 0) {
        rc = fail(tag, "tx_execute"); goto out;
    }
    graphdb_result_free(res); res = NULL;
    if (graphdb_tx_commit(db) != 0) { rc = fail(tag, "commit"); goto out; }
    /* auto-tx: this succeeds and implicitly opens+commits its own tx */
    if (graphdb_tx_execute(db, "CREATE (n:Person {name: 'B'})", &res) != 0) {
        rc = fail(tag, "auto-tx execute after commit"); goto out;
    }
    if (res) { graphdb_result_free(res); res = NULL; }
    if (expect_error(tag, graphdb_tx_rollback(db), "no active transaction to roll back", "rollback after commit")) goto out;
    rc = pass(tag);
out:
    if (res) graphdb_result_free(res);
    graphdb_close(db);
    return rc;
}

/* ----------------------------------------------------------------------- */
/* BC-09 — closing the DB with an open tx must not corrupt the file        */
/* ----------------------------------------------------------------------- */
static int bc_09_close_with_open_tx(const char *tmpdir) {
    const char *tag = "BC-09";
    char path[1024];
    snprintf(path, sizeof(path), "%s/%s", tmpdir, "bc09.sqlite");
    unlink(path);

    GraphDB *db = NULL;
    if (graphdb_open(path, &db) != 0 || !db) return fail(tag, "open");
    GraphResult *res = NULL;
    if (graphdb_tx_begin_write(db) != 0) {
        graphdb_close(db); return fail(tag, "begin_write");
    }
    if (graphdb_tx_execute(db, "CREATE (n:Person {name: 'Lost'})", &res) != 0) {
        graphdb_close(db); return fail(tag, "tx_execute");
    }
    graphdb_result_free(res); res = NULL;
    /* Close without commit/rollback. Core must auto-rollback. */
    graphdb_close(db);

    /* Reopen and confirm. */
    GraphDB *db2 = NULL;
    if (graphdb_open(path, &db2) != 0 || !db2) return fail(tag, "reopen");
    if (count_persons(db2) != 0) {
        graphdb_close(db2); return fail(tag, "data leaked through close-without-commit");
    }
    /* File must still be writeable. */
    if (graphdb_tx_begin_write(db2) != 0) {
        graphdb_close(db2); return fail(tag, "post-recovery begin_write");
    }
    if (graphdb_tx_execute(db2, "CREATE (n:Person {name: 'Recovered'})", &res) != 0) {
        graphdb_close(db2); return fail(tag, "post-recovery execute");
    }
    graphdb_result_free(res); res = NULL;
    if (graphdb_tx_commit(db2) != 0) {
        graphdb_close(db2); return fail(tag, "post-recovery commit");
    }
    long long n = count_persons(db2);
    graphdb_close(db2);
    if (n != 1) return fail(tag, "post-recovery count != 1");
    return pass(tag);
}

/* ----------------------------------------------------------------------- */
/* BC-10 — result handles freeable after the producing tx ends             */
/* ----------------------------------------------------------------------- */
static int bc_10_result_after_commit(const char *tmpdir) {
    const char *tag = "BC-10";
    GraphDB *db = fresh_db(tmpdir, "bc10.sqlite");
    if (!db) return fail(tag, "open");
    int rc = 1;
    GraphResult *res = NULL;
    if (graphdb_tx_begin_write(db) != 0) { rc = fail(tag, "begin_write"); goto out; }
    if (graphdb_tx_execute(
            db,
            "CREATE (n:Person {name: 'Alice'}) RETURN n.name AS name",
            &res) != 0) {
        rc = fail(tag, "tx_execute"); goto out;
    }
    if (graphdb_tx_commit(db) != 0) { rc = fail(tag, "commit"); goto out; }
    /* Read result after commit — must still be valid. */
    const char *got = graphdb_result_value_str(res, 0, 0);
    if (!got || strcmp(got, "Alice") != 0) {
        rc = fail(tag, "post-commit result_value_str");
        goto out;
    }
    graphdb_result_free(res); res = NULL;
    rc = pass(tag);
out:
    if (res) graphdb_result_free(res);
    graphdb_close(db);
    return rc;
}

/* ----------------------------------------------------------------------- */
/* BC-11 — fulltext index DDL: create, use via CONTAINS, drop              */
/* ----------------------------------------------------------------------- */
static int bc_11_fulltext_ddl(const char *tmpdir) {
    GraphDB *db = fresh_db(tmpdir, "bc11.db");
    if (!db) return fail("BC-11", "open failed");

    /* Open a write tx and create a fulltext index inside it. */
    if (graphdb_tx_begin_write(db) != 0) {
        graphdb_close(db);
        return fail("BC-11", "tx_begin_write failed");
    }
    if (graphdb_create_fulltext_index(db, "Doc", "body") != 0) {
        graphdb_tx_rollback(db);
        graphdb_close(db);
        return fail("BC-11", "create_fulltext_index failed");
    }

    /* Insert a row through the same tx. */
    GraphResult *res = NULL;
    if (graphdb_tx_execute(db,
            "CREATE (:Doc {body: 'hello world'})", &res) != 0) {
        graphdb_tx_rollback(db);
        graphdb_close(db);
        return fail("BC-11", "insert failed");
    }
    graphdb_result_free(res);

    if (graphdb_tx_commit(db) != 0) {
        graphdb_close(db);
        return fail("BC-11", "commit failed");
    }

    /* CONTAINS query must route through FTS and return the row. */
    if (graphdb_query(db,
            "MATCH (n:Doc) WHERE n.body CONTAINS 'hello' RETURN n.body",
            &res) != 0) {
        graphdb_close(db);
        return fail("BC-11", "contains query failed");
    }
    if (graphdb_result_row_count(res) != 1) {
        graphdb_result_free(res);
        graphdb_close(db);
        return fail("BC-11", "expected 1 row from CONTAINS");
    }
    graphdb_result_free(res);

    /* Drop the index inside a fresh tx. */
    if (graphdb_tx_begin_write(db) != 0) {
        graphdb_close(db);
        return fail("BC-11", "second tx_begin_write failed");
    }
    if (graphdb_drop_fulltext_index(db, "Doc", "body") != 0) {
        graphdb_tx_rollback(db);
        graphdb_close(db);
        return fail("BC-11", "drop_fulltext_index failed");
    }
    if (graphdb_tx_commit(db) != 0) {
        graphdb_close(db);
        return fail("BC-11", "second commit failed");
    }

    /* Dropping again must error. */
    if (graphdb_tx_begin_write(db) != 0) {
        graphdb_close(db);
        return fail("BC-11", "third tx_begin_write failed");
    }
    if (expect_error("BC-11", graphdb_drop_fulltext_index(db, "Doc", "body"),
                     "fulltext index", "dropping missing fulltext index")) {
        graphdb_tx_rollback(db);
        graphdb_close(db);
        return 1;
    }
    graphdb_tx_rollback(db);

    /* Secondary index round-trip. */
    if (graphdb_tx_begin_write(db) != 0) {
        graphdb_close(db);
        return fail("BC-11", "secondary tx_begin_write failed");
    }
    if (graphdb_create_index(db, "Doc", "body") != 0) {
        graphdb_tx_rollback(db);
        graphdb_close(db);
        return fail("BC-11", "create_index failed");
    }
    if (expect_error("BC-11", graphdb_create_index(db, "Doc", "body"),
                     "index already exists: :Doc(body)", "duplicate create_index")) {
        graphdb_tx_rollback(db);
        graphdb_close(db);
        return 1;
    }
    if (graphdb_drop_index(db, "Doc", "body") != 0) {
        graphdb_tx_rollback(db);
        graphdb_close(db);
        return fail("BC-11", "drop_index failed");
    }
    if (graphdb_tx_commit(db) != 0) {
        graphdb_close(db);
        return fail("BC-11", "secondary commit failed");
    }

    /* Case-insensitive fulltext index round-trip. */
    if (graphdb_tx_begin_write(db) != 0) {
        graphdb_close(db);
        return fail("BC-11", "ci tx_begin_write failed");
    }
    if (graphdb_create_fulltext_index_ci(db, "Doc", "body") != 0) {
        graphdb_tx_rollback(db);
        graphdb_close(db);
        return fail("BC-11", "create_fulltext_index_ci failed");
    }
    if (graphdb_tx_execute(db,
            "CREATE (:Doc {body: 'Hello World'})", &res) != 0) {
        graphdb_tx_rollback(db);
        graphdb_close(db);
        return fail("BC-11", "ci insert failed");
    }
    graphdb_result_free(res);
    if (graphdb_tx_commit(db) != 0) {
        graphdb_close(db);
        return fail("BC-11", "ci commit failed");
    }

    /* CI index must serve the toLower CONTAINS idiom. Two Doc nodes exist
     * by now: 'hello world' (created earlier) and 'Hello World' (just
     * created); both match. (A plain CONTAINS stays case-sensitive: an
     * index never changes results.) */
    if (graphdb_query(db,
            "MATCH (n:Doc) WHERE toLower(n.body) CONTAINS toLower('Hello') RETURN n.body",
            &res) != 0) {
        graphdb_close(db);
        return fail("BC-11", "ci contains query failed");
    }
    if (graphdb_result_row_count(res) != 2) {
        graphdb_result_free(res);
        graphdb_close(db);
        return fail("BC-11", "expected 2 rows from CI CONTAINS");
    }
    graphdb_result_free(res);

    if (graphdb_tx_begin_write(db) != 0) {
        graphdb_close(db);
        return fail("BC-11", "ci drop tx_begin_write failed");
    }
    if (graphdb_drop_fulltext_index(db, "Doc", "body") != 0) {
        graphdb_tx_rollback(db);
        graphdb_close(db);
        return fail("BC-11", "ci drop_fulltext_index failed");
    }
    if (graphdb_tx_commit(db) != 0) {
        graphdb_close(db);
        return fail("BC-11", "ci drop commit failed");
    }

    /* Word-tokenized (unicode61) fulltext index round-trip via fts.search. */
    if (graphdb_tx_begin_write(db) != 0) {
        graphdb_close(db);
        return fail("BC-11", "word tx_begin_write failed");
    }
    if (graphdb_create_fulltext_index_word(db, "Doc", "body") != 0) {
        graphdb_tx_rollback(db);
        graphdb_close(db);
        return fail("BC-11", "create_fulltext_index_word failed");
    }
    if (graphdb_tx_execute(db,
            "CREATE (:Doc {body: 'rust systems programming'})", &res) != 0) {
        graphdb_tx_rollback(db);
        graphdb_close(db);
        return fail("BC-11", "word insert failed");
    }
    graphdb_result_free(res);
    if (graphdb_tx_commit(db) != 0) {
        graphdb_close(db);
        return fail("BC-11", "word commit failed");
    }

    /* fts.search must return the newly inserted Doc. */
    if (graphdb_query(db,
            "CALL fts.search('Doc', 'body', 'rust') YIELD node, score RETURN score",
            &res) != 0) {
        graphdb_close(db);
        return fail("BC-11", "fts.search query failed");
    }
    if (graphdb_result_row_count(res) != 1) {
        graphdb_result_free(res);
        graphdb_close(db);
        return fail("BC-11", "expected 1 row from fts.search");
    }
    graphdb_result_free(res);

    if (graphdb_tx_begin_write(db) != 0) {
        graphdb_close(db);
        return fail("BC-11", "word drop tx_begin_write failed");
    }
    if (graphdb_drop_fulltext_index(db, "Doc", "body") != 0) {
        graphdb_tx_rollback(db);
        graphdb_close(db);
        return fail("BC-11", "word drop_fulltext_index failed");
    }
    if (graphdb_tx_commit(db) != 0) {
        graphdb_close(db);
        return fail("BC-11", "word drop commit failed");
    }

    /* Multi-property word index round-trip via fts.search('*', ...). */
    if (graphdb_tx_begin_write(db) != 0) {
        graphdb_close(db);
        return fail("BC-11", "multi tx_begin_write failed");
    }
    const char *props[] = {"title", "body"};
    if (graphdb_create_fulltext_index_word_multi(db, "Article", props, 2) != 0) {
        graphdb_tx_rollback(db);
        graphdb_close(db);
        return fail("BC-11", "create_fulltext_index_word_multi failed");
    }
    if (graphdb_tx_execute(db,
            "CREATE (:Article {title: 'rust', body: 'memory safe systems'})",
            &res) != 0) {
        graphdb_tx_rollback(db);
        graphdb_close(db);
        return fail("BC-11", "multi insert failed");
    }
    graphdb_result_free(res);
    if (graphdb_tx_commit(db) != 0) {
        graphdb_close(db);
        return fail("BC-11", "multi commit failed");
    }
    if (graphdb_query(db,
            "CALL fts.search('Article', '*', 'memory') YIELD node, score RETURN score",
            &res) != 0) {
        graphdb_close(db);
        return fail("BC-11", "multi fts.search query failed");
    }
    if (graphdb_result_row_count(res) != 1) {
        graphdb_result_free(res);
        graphdb_close(db);
        return fail("BC-11", "expected 1 row from multi fts.search");
    }
    graphdb_result_free(res);

    graphdb_close(db);
    return pass("BC-11");
}

/* ----------------------------------------------------------------------- */
/* snapshot — graphdb_snapshot_to                                           */
/* ----------------------------------------------------------------------- */
static int snap_basic(const char *tmpdir) {
    const char *tag = "SNAP-basic";
    GraphDB *db = fresh_db(tmpdir, "snap_src.db");
    if (!db) return fail(tag, "open");
    int rc = 1;
    char dst[1024];
    snprintf(dst, sizeof(dst), "%s/snap_dst.db", tmpdir);
    unlink(dst);
    char wal[1100], shm[1100];
    snprintf(wal, sizeof(wal), "%s-wal", dst);
    snprintf(shm, sizeof(shm), "%s-shm", dst);
    unlink(wal); unlink(shm);

    GraphResult *seed = NULL;
    if (graphdb_execute(db, "CREATE (:Person {name: 'Alice'}), (:Person {name: 'Bob'})", &seed) != 0) {
        rc = fail(tag, "seed execute"); goto out;
    }
    graphdb_result_free(seed);
    if (graphdb_snapshot_to(db, dst) != 0) { rc = fail(tag, "snapshot_to"); goto out; }

    struct stat st;
    if (stat(dst, &st) != 0) { rc = fail(tag, "dst missing"); goto out; }
    if (stat(wal, &st) == 0) { rc = fail(tag, "dst-wal should not exist"); goto out; }
    if (stat(shm, &st) == 0) { rc = fail(tag, "dst-shm should not exist"); goto out; }

    GraphDB *snap = NULL;
    if (graphdb_open(dst, &snap) != 0) { rc = fail(tag, "open snapshot"); goto out; }
    if (count_persons(snap) != 2) { rc = fail(tag, "person count != 2"); graphdb_close(snap); goto out; }
    graphdb_close(snap);
    rc = pass(tag);
out:
    graphdb_close(db);
    unlink(dst); unlink(wal); unlink(shm);
    return rc;
}

static int snap_rejects_existing(const char *tmpdir) {
    const char *tag = "SNAP-rejects-existing";
    GraphDB *db = fresh_db(tmpdir, "snap_src2.db");
    if (!db) return fail(tag, "open");
    int rc = 1;
    char dst[1024];
    snprintf(dst, sizeof(dst), "%s/snap_existing.db", tmpdir);
    FILE *f = fopen(dst, "w"); if (f) fclose(f);
    if (graphdb_snapshot_to(db, dst) == 0) { rc = fail(tag, "should have failed on existing target"); goto out; }
    rc = pass(tag);
out:
    graphdb_close(db);
    unlink(dst);
    return rc;
}

/* ----------------------------------------------------------------------- */
/* BC-12 — composite index DDL: create, query, drop                        */
/* ----------------------------------------------------------------------- */
static int bc_12_composite_index_ddl(const char *tmpdir) {
    const char *tag = "BC-12";
    GraphDB *db = fresh_db(tmpdir, "bc12.db");
    if (!db) return fail(tag, "open failed");

    /* Create a composite index inside a write tx. */
    if (graphdb_tx_begin_write(db) != 0) {
        graphdb_close(db);
        return fail(tag, "tx_begin_write failed");
    }
    const char *props[] = {"tenant_id", "ext_id"};
    if (graphdb_create_composite_index(db, "Person", props, 2) != 0) {
        graphdb_tx_rollback(db);
        graphdb_close(db);
        return fail(tag, "create_composite_index failed");
    }

    /* Insert nodes through the same tx. */
    GraphResult *res = NULL;
    if (graphdb_tx_execute(db,
            "CREATE (:Person {tenant_id: 'acme', ext_id: 'u1', name: 'Alice'})", &res) != 0) {
        graphdb_tx_rollback(db);
        graphdb_close(db);
        return fail(tag, "insert Alice failed");
    }
    graphdb_result_free(res); res = NULL;
    if (graphdb_tx_execute(db,
            "CREATE (:Person {tenant_id: 'acme', ext_id: 'u2', name: 'Bob'})", &res) != 0) {
        graphdb_tx_rollback(db);
        graphdb_close(db);
        return fail(tag, "insert Bob failed");
    }
    graphdb_result_free(res); res = NULL;
    if (graphdb_tx_commit(db) != 0) {
        graphdb_close(db);
        return fail(tag, "commit failed");
    }

    /* Query using both indexed properties — should hit the composite index. */
    if (graphdb_query(db,
            "MATCH (n:Person) WHERE n.tenant_id = 'acme' AND n.ext_id = 'u1' RETURN n.name",
            &res) != 0) {
        graphdb_close(db);
        return fail(tag, "lookup query failed");
    }
    if (graphdb_result_row_count(res) != 1) {
        graphdb_result_free(res);
        graphdb_close(db);
        return fail(tag, "expected 1 row from composite index lookup");
    }
    const char *name = graphdb_result_value_str(res, 0, 0);
    if (!name || strcmp(name, "Alice") != 0) {
        graphdb_result_free(res);
        graphdb_close(db);
        return fail(tag, "wrong name returned");
    }
    graphdb_result_free(res); res = NULL;

    /* Duplicate create must error. */
    if (graphdb_tx_begin_write(db) != 0) {
        graphdb_close(db);
        return fail(tag, "second tx_begin_write failed");
    }
    if (expect_error(tag, graphdb_create_composite_index(db, "Person", props, 2),
                     "index already exists: :Person(", "duplicate composite index")) {
        graphdb_tx_rollback(db);
        graphdb_close(db);
        return 1;
    }

    /* Drop the composite index. */
    if (graphdb_drop_composite_index(db, "Person", props, 2) != 0) {
        graphdb_tx_rollback(db);
        graphdb_close(db);
        return fail(tag, "drop_composite_index failed");
    }
    if (graphdb_tx_commit(db) != 0) {
        graphdb_close(db);
        return fail(tag, "drop commit failed");
    }

    /* Dropping again must error. */
    if (graphdb_tx_begin_write(db) != 0) {
        graphdb_close(db);
        return fail(tag, "third tx_begin_write failed");
    }
    if (expect_error(tag, graphdb_drop_composite_index(db, "Person", props, 2),
                     "index not found: :Person(", "dropping missing composite index")) {
        graphdb_tx_rollback(db);
        graphdb_close(db);
        return 1;
    }
    graphdb_tx_rollback(db);

    graphdb_close(db);
    return pass(tag);
}

/* ----------------------------------------------------------------------- */
/* BC-13: a failed statement inside a transaction is rolled back alone      */
/* ----------------------------------------------------------------------- */
static int bc_13_statement_atomicity(const char *tmpdir) {
    const char *tag = "BC-13";
    GraphDB *db = fresh_db(tmpdir, "bc13.sqlite");
    if (!db) return fail(tag, "open");
    int rc = 1;
    GraphResult *res = NULL;
    if (graphdb_tx_begin_write(db) != 0) { rc = fail(tag, "begin_write"); goto out; }
    if (graphdb_tx_execute(db, "CREATE (n:Person {name: 'Kept'})", &res) != 0) {
        rc = fail(tag, "first statement"); goto out;
    }
    graphdb_result_free(res); res = NULL;
    /* Writes a node for the first row, then fails on the second. */
    if (expect_error(tag,
                     graphdb_tx_execute(db,
                         "UNWIND [1, 'a'] AS i CREATE (:Person {name: 'Lost'}) RETURN i - 1 AS x",
                         &res),
                     "TypeError(", "failing statement")) {
        goto out;
    }
    if (graphdb_tx_execute(db, "CREATE (n:Person {name: 'After'})", &res) != 0) {
        rc = fail(tag, "statement after the failed one"); goto out;
    }
    graphdb_result_free(res); res = NULL;
    if (graphdb_tx_commit(db) != 0) { rc = fail(tag, "commit"); goto out; }
    if (count_persons(db) != 2) { rc = fail(tag, "expected exactly Kept + After"); goto out; }
    rc = pass(tag);
out:
    if (res) graphdb_result_free(res);
    graphdb_close(db);
    return rc;
}

/* ----------------------------------------------------------------------- */
/* RES-01: value strings stay valid until their result is freed            */
/* ----------------------------------------------------------------------- */
static int res_01_value_str_lifetime(const char *tmpdir) {
    const char *tag = "RES-01";
    GraphDB *db = fresh_db(tmpdir, "res01.sqlite");
    if (!db) return fail(tag, "open");
    GraphResult *r1 = NULL, *r2 = NULL;
    int rc = 1;
    if (graphdb_query(db, "RETURN 'aaaa' AS x, 'bbbb' AS y", &r1) != 0) { rc = fail(tag, "q1"); goto out; }
    if (graphdb_query(db, "RETURN 'cccc' AS x", &r2) != 0) { rc = fail(tag, "q2"); goto out; }
    const char *a = graphdb_result_value_str(r1, 0, 0);
    const char *b = graphdb_result_value_str(r1, 0, 1);
    const char *c = graphdb_result_value_str(r2, 0, 0);
    if (!a || !b || !c) { rc = fail(tag, "NULL value string"); goto out; }
    if (strcmp(a, "aaaa") != 0 || strcmp(b, "bbbb") != 0 || strcmp(c, "cccc") != 0) {
        rc = fail(tag, "earlier value_str pointer was invalidated"); goto out;
    }
    rc = pass(tag);
out:
    if (r1) graphdb_result_free(r1);
    if (r2) graphdb_result_free(r2);
    graphdb_close(db);
    return rc;
}

/* ----------------------------------------------------------------------- */
/* PAR-01: JSON parameters                                                 */
/* ----------------------------------------------------------------------- */
static int par_01_params(const char *tmpdir) {
    const char *tag = "PAR-01";
    GraphDB *db = fresh_db(tmpdir, "par01.sqlite");
    if (!db) return fail(tag, "open");
    GraphResult *res = NULL;
    int rc = 1;
    if (graphdb_execute_params(db, "CREATE (:Person {name: $n, tags: $t})",
                               "{\"n\": \"Ann\", \"t\": [\"a\", \"b\"]}", &res) != 0) {
        rc = fail(tag, "execute_params"); goto out;
    }
    graphdb_result_free(res); res = NULL;
    if (graphdb_query_params(db, "MATCH (p:Person {name: $n}) RETURN size(p.tags) AS s",
                             "{\"n\": \"Ann\"}", &res) != 0) {
        rc = fail(tag, "query_params"); goto out;
    }
    if (graphdb_result_row_count(res) != 1 || graphdb_result_value_i64(res, 0, 0) != 2) {
        rc = fail(tag, "unexpected params result"); goto out;
    }
    graphdb_result_free(res); res = NULL;
    if (expect_error(tag, graphdb_query_params(db, "RETURN $x AS x", "[1]", &res),
                     "invalid params JSON: expected an object", "non-object params")) {
        goto out;
    }
    rc = pass(tag);
out:
    if (res) graphdb_result_free(res);
    graphdb_close(db);
    return rc;
}

/* ----------------------------------------------------------------------- */
/* SP-01: shortestPath over untyped relationships (GitHub issue #2)          */
/* ----------------------------------------------------------------------- */
static int sp_01_untyped_shortest_path(const char *tmpdir) {
    const char *tag = "SP-01";
    GraphDB *db = fresh_db(tmpdir, "sp01.db");
    if (!db) return fail(tag, "open failed");

    GraphResult *res = NULL;
    if (graphdb_execute(db,
            "CREATE (a:P {name: 'A'})-[:KNOWS]->(b:P {name: 'B'})"
            "-[:WORKS_WITH]->(c:P {name: 'C'}), (d:P {name: 'D'}), "
            "(a)-[:LIKES]->(e:P {name: 'E'})", &res) != 0) {
        graphdb_close(db);
        return fail(tag, "create failed");
    }
    graphdb_result_free(res);

    res = NULL;
    if (graphdb_query(db,
            "MATCH p = shortestPath((a)-[*]->(b)) "
            "RETURN a.name AS src, b.name AS dst, length(p) AS len "
            "ORDER BY src, dst", &res) != 0) {
        graphdb_close(db);
        return fail(tag, "shortestPath query failed");
    }
    static const char *want_src[] = {"A", "A", "A", "B"};
    static const char *want_dst[] = {"B", "C", "E", "C"};
    static const long long want_len[] = {1, 2, 1, 1};
    if (graphdb_result_row_count(res) != 4) {
        graphdb_result_free(res);
        graphdb_close(db);
        return fail(tag, "expected 4 rows");
    }
    for (int64_t i = 0; i < 4; i++) {
        const char *src = graphdb_result_value_str(res, i, 0);
        int src_ok = src && strcmp(src, want_src[i]) == 0;
        const char *dst = graphdb_result_value_str(res, i, 1);
        int dst_ok = dst && strcmp(dst, want_dst[i]) == 0;
        long long len = graphdb_result_value_i64(res, i, 2);
        if (!src_ok || !dst_ok || len != want_len[i]) {
            graphdb_result_free(res);
            graphdb_close(db);
            return fail(tag, "unexpected row");
        }
    }
    graphdb_result_free(res);

    /* Unreachable pair: plain MATCH drops the row. */
    res = NULL;
    if (graphdb_query(db,
            "MATCH (c:P {name: 'C'}), (a:P {name: 'A'}), "
            "p = shortestPath((c)-[*]->(a)) RETURN p", &res) != 0) {
        graphdb_close(db);
        return fail(tag, "unreachable query failed");
    }
    int64_t rows = graphdb_result_row_count(res);
    graphdb_result_free(res);
    graphdb_close(db);
    if (rows != 0) return fail(tag, "expected 0 rows for unreachable pair");
    return pass(tag);
}

int main(int argc, char **argv) {
    const char *tmpdir = (argc >= 2) ? argv[1] : "/tmp/graphdblite-bc";
    if (mkdir(tmpdir, 0700) != 0) {
        /* OK if it already exists */
    }

    scenario_fn scenarios[] = {
        bc_01_explicit_rollback,
        bc_02_commit_persists,
        bc_04_nested_begin,
        bc_05_write_in_read_tx,
        bc_06_commit_without_tx,
        bc_07_use_after_commit,
        bc_09_close_with_open_tx,
        bc_10_result_after_commit,
        bc_11_fulltext_ddl,
        bc_12_composite_index_ddl,
        bc_13_statement_atomicity,
        res_01_value_str_lifetime,
        par_01_params,
        snap_basic,
        snap_rejects_existing,
        sp_01_untyped_shortest_path,
    };
    size_t n = sizeof(scenarios) / sizeof(scenarios[0]);
    int failures = 0;
    for (size_t i = 0; i < n; i++) {
        if (scenarios[i](tmpdir) != 0) failures++;
    }
    fprintf(stdout, "\nSummary: %zu scenarios, %d failures\n", n, failures);
    return failures == 0 ? 0 : 1;
}
