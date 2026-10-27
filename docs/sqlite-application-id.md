# SQLite application_id and file(1) registration

## Identifier

graphdblite stamps every database file it creates with:

| Field             | Value                                | Where                       |
|-------------------|--------------------------------------|-----------------------------|
| `application_id`  | `0x4744424C` (1,195,655,756, "GDBL") | byte 68 of the SQLite header |
| `user_version`    | `2`                                  | byte 60 of the SQLite header |

The constants live in `src/db.rs` as `GRAPHDBLITE_APPLICATION_ID` and
`GRAPHDBLITE_SCHEMA_VERSION`. `Database::open` checks the file before it
modifies anything (no WAL switch, no tables, no stamp until the checks
pass):

| File | Result |
|------|--------|
| `application_id` = `0x4744424C` | opened |
| `application_id` = `0`, empty or containing only graphdblite tables (a fresh file, or one written before the marker existed) | opened and stamped |
| `application_id` = `0` with any other table, index, view or trigger | rejected (`SQLITE_NOTADB`), file left untouched |
| `application_id` = `0` with a table named like a graphdblite core table (`nodes`, `adj_out`, `adj_in`, `edge_props`, `metadata`) that lacks graphdblite's columns, declared types or primary key | rejected (`SQLITE_NOTADB`), file left untouched (no WAL switch) |
| any other nonzero `application_id` (Firefox cookies, GeoPackage, ...) | rejected (`SQLITE_NOTADB`) |
| `user_version` greater than this build's `GRAPHDBLITE_SCHEMA_VERSION` | rejected (written by a newer release) |
| a trigger on, or a view named like, a graphdblite table | rejected: graphdblite never creates them, so they were planted by another tool or a crafted file |

Every connection is also opened with `trusted_schema` off and
`SQLITE_DBCONFIG_DEFENSIVE` on, so SQL stored in the file cannot call
application functions and the library cannot be made to write the schema
or FTS5 shadow tables directly.

`user_version` provides forward-compat hardening: if a future build writes
a newer schema layout, older builds will refuse to open the file rather
than silently misinterpret it.

### Schema versions and migrations

| `user_version` | Releases | Layout |
|----------------|----------|--------|
| `0` / `1` | 0.1.x | index tables `node_idx_<label>_<prop>`, `node_fts_<label>_<prop>`, `node_fts_multi_<label>_<n>` (and, in unreleased builds, raw-case `node_idx$<Label>$<prop>` / `node_fts$<Label>$<prop>`) |
| `2` | current | `node_idx$<label>$<p1>$...`, `node_fts$<label>$<prop>$`, `node_fts_multi$<label>$<n>`, with each uppercase letter written as `^` + lowercase (`Person` becomes `^person`) |

Version 2 fixes two naming problems: SQLite table names are
case-insensitive, so indexes on `:Person(name)` and `:person(name)` used
to collide, and the `_` separator was ambiguous for labels or properties
containing `_`. `Database::open` migrates a version 0/1 file once, inside a
write transaction (`src/storage/migrate.rs`):

- raw-case `$` tables are renamed (FTS5 renames its shadow tables too);
- `_`-named tables are dropped and rebuilt from node data under the new
  name. When the `_` split is ambiguous (`node_idx_A_b_c` is `(A_b, c)` or
  `(A, b_c)`), every reading whose label has nodes carrying the property
  gets an index; if no reading has data the empty index is dropped and a
  `tracing` warning names it (recreate it if still needed).

Opening a version 2 file with a 0.1.x build fails with the "newer than this
build supports" error.

## Why "GDBL"?

- ASCII bytes `G`, `D`, `B`, `L` for "graphdblite".
- Uppercase matches the dominant convention in SQLite's existing
  `magic.txt` registry (`GPKG`, `GP10`, `Esri`, `MPBX`).
- No collision with any registered application_id as of the magic.txt
  snapshot taken when this was implemented (see "Conflict check" below).

## file(1) output today

Even without an upstream `magic.txt` entry, `file(1)` reports the raw
application_id from the SQLite header:

```
$ file mygraph.db
mygraph.db: SQLite 3.x database, application id 1195655756, user version 2, ...
```

After upstream registration, the relevant line will read:

```
$ file mygraph.db
mygraph.db: graphdblite graph database
```

## Upstream registration procedure

SQLite maintains the registry as a single file at
`https://sqlite.org/src/file/magic.txt` (mirrored at
`https://github.com/sqlite/sqlite/blob/master/magic.txt`). Registration
is done by emailing the SQLite maintainer with a patch.

1. **Verify no conflict.** Pull the latest `magic.txt` and confirm
   `0x4744424c` does not appear:
   ```bash
   curl -fsSL https://raw.githubusercontent.com/sqlite/sqlite/master/magic.txt \
     | grep -i 4744424c
   # (no output) → still free
   ```

2. **Email the patch.** Send `contrib/sqlite-magic.patch`
   as an attachment or inline diff to **drh@sqlite.org** (D. Richard Hipp).
   A short message is sufficient — for example:

   > Subject: SQLite magic.txt registration request: graphdblite (0x4744424C)
   >
   > Hi Richard,
   >
   > Could you please add graphdblite to the SQLite magic.txt registry?
   > graphdblite is an embedded graph database that uses SQLite as its
   > storage backend.
   >
   > Application ID: 0x4744424C ("GDBL")
   > Project: https://… (URL of the canonical repo)
   >
   > Patch attached.
   >
   > Thanks!

3. **After acceptance**, update this document to record the registration
   date and SQLite version that first ships the entry.

### Status

| Date | Status |
|------|--------|
| _(unset)_ | Reserved; patch not yet sent upstream |

When the upstream patch lands, replace this row with `<YYYY-MM-DD>` and
`Registered in SQLite <version>`.

## Conflict check (recorded for future audits)

The registry contained these entries the day "GDBL" was reserved:

| Hex          | ASCII  | Application                       |
|--------------|--------|-----------------------------------|
| `0x0f055111` | —      | Fossil repository                 |
| `0x0f055112` | —      | Fossil checkout                   |
| `0x0f055113` | —      | Fossil global configuration       |
| `0x42654462` | `BeDb` | Bentley BeSQLite                  |
| `0x42654c6e` | `BeLn` | Bentley Localization              |
| `0x47504b47` | `GPKG` | OGC GeoPackage                    |
| `0x47503130` | `GP10` | OGC GeoPackage 1.0                |
| `0x45737269` | `Esri` | Esri Spatially-Enabled Database   |
| `0x4d504258` | `MPBX` | MBTiles tileset                   |
| `0x6a035744` | —      | TeXnicard card database           |
| `0x5f4d544e` | `_MTN` | Monotone (uses `user_version`)    |

If you re-verify before sending the patch and find a new entry that
collides with `0x4744424C`, pick a different four-byte ASCII code,
update `GRAPHDBLITE_APPLICATION_ID` in `src/db.rs`, bump
`GRAPHDBLITE_SCHEMA_VERSION` (since old files become incompatible), and
regenerate the patch.
