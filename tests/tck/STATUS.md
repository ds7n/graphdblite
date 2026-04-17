# TCK Conformance Status

Last updated: 2026-04-16

## Current pass rate

```
5 features, 112 scenarios
112 passed, 0 failed (100%)
372/372 steps passed
```

## Pilot feature files

| Feature | Scenarios | Passing | Notes |
|---------|-----------|---------|-------|
| Literals1 (Boolean and Null) | 6 | 6 | |
| Literals2 (Integer) | 12 | 12 | |
| Match1 (Match nodes) | 6 + 80 outlines | all | Multi-label nodes supported |
| Return1 | 2 | 2 | |
| With1 (Forward variable) | 6 | 6 | |

## Phase 4 — scaling up (not yet started)

- Vendor the full `tck/features/` tree (hundreds of feature files)
- Add `tests/tck/skiplist.txt` for known-failing scenarios
- Flip exit status: harness fails on non-skiplisted regressions
- Track pass-rate over time
