# SQLite application_id and file(1) registration

## Identifier

graphdblite stamps every database file it creates with:

| Field             | Value                                | Where                       |
|-------------------|--------------------------------------|-----------------------------|
| `application_id`  | `0x4744424C` (1,195,655,756, "GDBL") | byte 68 of the SQLite header |
| `user_version`    | `1`                                  | byte 60 of the SQLite header |

The constants live in `src/db.rs` as `GRAPHDBLITE_APPLICATION_ID` and
`GRAPHDBLITE_SCHEMA_VERSION`. `Database::open` checks `application_id` on
every open: a value of `0` means a fresh or pre-marker file (we stamp it),
the matching value passes through, and any other nonzero value is rejected
with a clear error so users don't accidentally point graphdblite at a
foreign SQLite file (Firefox cookies, GeoPackage, etc.).

`user_version` provides forward-compat hardening — if a future build
writes a newer schema layout, older builds will refuse to open the file
rather than silently misinterpret it.

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
mygraph.db: SQLite 3.x database, application id 1195655756, user version 1, ...
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

2. **Email the patch.** Send `docs/sqlite-magic.patch` (in this directory)
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
