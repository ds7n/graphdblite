# Temporal types: Cypher-vs-chrono audit

`src/temporal.rs` (≈3000 LOC) wraps `chrono` + `chrono-tz` to implement
Cypher's six temporal types. Most of the bulk is serde, formatting, and
boilerplate. This doc lists the parts that **diverge from chrono on
purpose** so future maintainers know what's load-bearing and what's
just thin wrapping.

## The six types

| Cypher type        | Wraps                                        | Notes                                                     |
|--------------------|----------------------------------------------|-----------------------------------------------------------|
| `CypherDate`       | `chrono::NaiveDate`                          | Pure pass-through — no semantic divergence.               |
| `CypherLocalTime`  | `chrono::NaiveTime`                          | Pure pass-through.                                        |
| `CypherTime`       | `(chrono::NaiveTime, chrono::FixedOffset)`   | Pass-through; `FixedOffset` is chrono's.                  |
| `CypherLocalDateTime` | `chrono::NaiveDateTime`                  | Pass-through.                                             |
| `CypherDateTime`   | `(NaiveDateTime, FixedOffset, Option<String>)` | The `Option<String>` is **load-bearing** — see below.  |
| `CypherDuration`   | bespoke `{ months, days, seconds, nanos }`   | **Not** `chrono::Duration`. See below.                    |

Wrapper boilerplate (custom `PartialEq`, `Hash`, `Display`, `Serialize`)
exists because chrono's defaults don't match Cypher's serialization
shape (we need ISO-8601 with msgpack-friendly typing) or its hashing
contract (Cypher's `Eq` must be transitive across timezone-equivalent
values). This is uninteresting maintenance.

## Load-bearing divergences

### 1. `CypherDuration` is a calendar duration, not a fixed nanosecond span

`chrono::Duration` is `i64` nanoseconds. Cypher's `Duration` is
`(months, days, seconds, nanos)` — four fields, kept separate because:

- **Months don't have a fixed nanosecond length** (Feb vs Aug). Adding
  `Duration("P1M")` to `2026-01-31` should yield `2026-02-28`, not
  `2026-01-31 + 30*86400s`.
- **Days don't have a fixed length under DST**. Adding `Duration("P1D")`
  to `2026-03-13T12:00 America/New_York` should yield
  `2026-03-14T12:00`, even though the elapsed UTC seconds is 23h or 25h
  depending on DST direction.

The components are normalized only within their tier (`nanos` may carry
into `seconds`, but `seconds` never carries into `days` and `days` never
carries into `months`). This preserves the calendar semantics across
arithmetic.

If a future maintainer is tempted to "simplify" by collapsing
`CypherDuration` to `chrono::Duration`: don't. The TCK has scenarios
that pin every one of these edge cases.

### 2. Fractional month cascade — 30.436875 days/month

Lives in `from_map` and the ISO duration parser. When `Duration({months: 1.5})`
or `"P1.5M"` is constructed, the fractional component cascades into
`days` using `30.436875` (== 365.2425 ÷ 12 — the Gregorian-mean month).

| Constant       | Source              | Used for                         |
|----------------|---------------------|----------------------------------|
| `30.436875`    | `temporal.rs:2007`  | Fractional months → days         |
| `30.436875`    | `temporal.rs:2046`  | `cascade_fractional_months` helper |

This is **not** the same as `30` (which Neo4j historically used in some
contexts). The Gregorian mean was chosen to make `1Y == 12M` exactly
when collapsed via the cascade. Don't change this without a TCK rerun.

### 3. Duration division / multiplication via i128

`eval_duration_div` and `eval_duration_mul` (in `cypher::eval::temporal_ops`)
flatten the `(months, days, seconds, nanos)` quadruple into a total
nanosecond count using a fixed `AVG_SECONDS_PER_MONTH = 2_629_746`
(== 365.2425 × 86400 ÷ 12), do the arithmetic in `i128` to avoid
overflow on year-scale durations, and redistribute the result back into
the four-field shape.

This is approximate by design — `Duration("P1M") * 0.5` doesn't have a
single right answer, so Cypher picks "half a Gregorian-mean month" as
its definition. Documented behavior.

### 4. DST-aware operations on named-timezone DateTimes

`CypherDateTime`'s third field — `Option<String>` — stores the IANA
zone name (e.g. `"America/New_York"`) when the value originates from a
named-tz constructor. The `FixedOffset` is the *resolved* offset at
construction time; the `String` is the *source of truth* for re-resolving
when arithmetic crosses DST boundaries.

Affected operations:

- **`+ Duration` / `- Duration`** when the duration shifts the wall
  clock across a DST transition (`temporal.rs:1593-1631`). The base
  resolves the new offset in the stored zone via `chrono_tz`.
- **`duration.between` / `inMonths` / `inDays` / `inSeconds`** when one
  side is a named-tz DateTime and the other is local
  (`effective_offset_dst`, `temporal.rs:2300-2360`). The local side's
  offset is resolved in the named zone for the comparison.

If the `Option<String>` is `None` (e.g. constructed with an explicit
`+05:00` offset), DST awareness is impossible and the `FixedOffset` is
treated as fixed forever. This matches Neo4j semantics.

### 5. CypherDateTime serialization carries `tz_name`

The msgpack format of `CypherDateTime` includes the optional `tz_name`
so named-zone identity survives storage round-trips. Deserialization is
backwards-compatible: pre-existing data without `tz_name` deserializes
to `None` and operates as a fixed-offset datetime.

This is a *storage-format commitment* — changing the on-disk shape of
`CypherDateTime` requires a schema bump and migration.

### 6. ISO-8601 parsing accepts more than chrono's defaults

The Cypher TCK requires several formats that `chrono::DateTime::parse_from_rfc3339`
rejects:

- Offsets without a colon: `+0530` (chrono wants `+05:30`).
- Compact dates: `20260101` (chrono wants `2026-01-01`).
- Truncated times: `T14` (chrono wants `T14:00:00`).
- `Z` and `±HH:MM` interchangeable.

The parsers in `temporal.rs` (`parse_date`, `parse_time`,
`parse_datetime`, etc.) handle these inline rather than via a chrono
extension. Mostly this is uninteresting boilerplate, but it is
load-bearing — Cypher TCK scenarios assert these formats parse.

## What's *not* divergent

These look custom but are pure pass-through. If you find yourself
"refactoring" them, you're probably just reshuffling chrono calls:

- All `Display` impls — ISO-8601 formatting via chrono's `format!`.
- All `Serialize`/`Deserialize` outside `CypherDateTime`'s `tz_name` field.
- `Hash` on Date/LocalTime/LocalDateTime — wraps `chrono`'s `Hash`.
- All `+ Duration` / `- Duration` arithmetic that doesn't cross DST or
  involve calendar months — chrono's `checked_add_signed` does the work.
- `truncate()` — uses `chrono::DateTime::with_*` setters.
- Component accessors (`year()`, `month()`, `day()`, etc.) — direct
  chrono calls.

## Audit conclusion

The 3000 LOC is mostly serde + parser + Display boilerplate. The
load-bearing divergences are roughly six concepts (above), most of
which are <50 LOC each. Splitting `temporal.rs` into separate
per-type files would fragment the shared parser/format helpers
without isolating the divergent logic. The right "smaller surface"
move is documenting the divergences (this file) so future maintainers
know what they can and can't simplify. No file split warranted.
