# Temporal types: design notes

`src/temporal.rs` implements Cypher's six temporal types. Since v0.3.0 the
types **own their data as plain integer fields** and use
[`jiff`](https://crates.io/crates/jiff) only as a calendar and timezone
engine. This doc lists what is load-bearing so future maintainers know what
they can and can't simplify.

## The six types

| Cypher type | Fields | Notes |
|---|---|---|
| `CypherDate` | `year: i32, month: u8, day: u8` | |
| `CypherLocalTime` | `hour, minute, second: u8, nanosecond: u32` | |
| `CypherTime` | `time: CypherLocalTime, offset_seconds: i32` | |
| `CypherLocalDateTime` | `date: CypherDate, time: CypherLocalTime` | |
| `CypherDateTime` | `local: CypherLocalDateTime, offset_seconds: i32, tz_name: Option<String>` | `tz_name` is load-bearing, see below |
| `CypherDuration` | `months, days, seconds, nanos: i64` | Calendar duration, not a nanosecond span |

## Load-bearing invariants

### 1. The on-disk format is plain integers, independent of any date library

Values are stored positionally with `rmp_serde::to_vec` in node and edge
property blobs and in btree index keys. Each type serializes exactly the
fields above, in that order, as `i32` year, `u32` month/day/hour/minute/
second/nanosecond, `i32` offset, then an optional trailing `tz_name`.
Changing any of this breaks existing database files and needs a
`GRAPHDBLITE_SCHEMA_VERSION` bump plus a migration.

`tests/temporal_compat.rs` pins it: the `[wire]` section compares exact
bytes, and `[readback]` reads `tests/fixtures/temporal_v0_2.db` (written by
the pre-jiff build, never regenerated) and must display every value
identically. Never re-bless those two sections to make a change pass.

Because the fields are plain integers, decoding never depends on jiff:
values written by older builds outside jiff's range (years beyond
-9999..=9999, leap-second nanoseconds >= 1e9) still decode and display.

### 2. Supported range: years -9999 to 9999

New values outside -9999..=9999 or with nanosecond >= 1_000_000_000 are
rejected at construction with `NumberOutOfRange` ("temporal year is out of
range"). Legacy stored values outside the range decode fine and only error
when used in arithmetic or conversion (`to_jiff()` fails), never panic.

Epoch seconds/millis and sort keys use integer civil-day math
(`days_from_civil`) rather than `jiff::Timestamp`, whose range stops about
26 hours short of the year-9999 edges; ordering must never fail. Named-zone
conversions go through `Timestamp` and can return `NumberOutOfRange` within
about a day of the range ends.

### 3. `CypherDuration` is a calendar duration

Four separate fields because months have no fixed length (`2026-01-31 +
P1M` is `2026-02-28`) and days have no fixed length under DST (`+ P1D`
keeps the local wall time). Components normalize only within their tier
(`nanos` carry into `seconds`; nothing carries into `days` or `months`).
Do not collapse it into a nanosecond span; the TCK pins these cases.

### 4. Fractional month cascade: 30.436875 days per month

`Duration({months: 1.5})` and `"P1.5M"` cascade the fraction into days
using 365.2425 / 12 = 30.436875 (the Gregorian mean month), not 30.

### 5. Duration division and multiplication via i128

`eval_duration_div` / `eval_duration_mul` flatten to total nanoseconds in
`i128` using `AVG_SECONDS_PER_MONTH = 2_629_746`, operate, then
redistribute. Approximate by design. All temporal arithmetic is checked:
overflow is `NumberOutOfRange`, never a panic or silent clamp.

### 6. Named timezones and DST

`CypherDateTime.tz_name` stores the IANA zone; `offset_seconds` is the
offset resolved at construction. The name is the source of truth when
arithmetic crosses a DST change. Zones come from jiff's **bundled** tzdb
(`tzdb-bundle-always`), so results are identical on every platform and do
not depend on the host's zoneinfo. Zone lookup is exact and case-sensitive
(`Europe/Stockholm`, not `europe/stockholm`).

Gap (spring-forward) and fold (fall-back) handling differs by call site on
purpose; each replicates the pre-jiff behaviour explicitly instead of using
jiff's default disambiguation:

| Site | Fold | Gap |
|---|---|---|
| Construction from a local time + zone (`resolve_tz_name_at`, `datetime.truncate` with a zone) | earlier offset | error "ambiguous or invalid datetime in timezone" |
| `effective_offset_dst` (`duration.between`/`inMonths`/`inDays`/`inSeconds` mixing local and zoned) | earlier offset | offset 0 |
| Zoned `+`/`-` duration (`shift_zoned` / `local_to_utc`, java.time semantics) | keep the original offset if still valid, else earlier | offset from one day earlier (moves later by the gap length) |

For zoned arithmetic, months and days shift the local date-time, then
seconds and nanoseconds shift the instant, then the offset is re-resolved.
With `tz_name = None` (explicit `+05:00` offset) the offset is fixed.

### 7. ISO-8601 parsing and formatting are hand-written

The TCK needs formats strict RFC 3339 parsers reject (`+0530`, compact
`20260101`, truncated `T14`, week dates, ordinal dates). Parsing and
`Display` are implemented in `temporal.rs` over the plain fields, so any
stored year prints the same way regardless of the calendar engine.

## What jiff does

Calendar validity, weekday / ISO week / ordinal / quarter, month arithmetic
with month-end clamping, day differences, tz offset lookup, and
epoch-to-civil conversion. Everything above that is Cypher-specific
(storage, parsing, formatting, duration semantics, DST policy) is ours.
