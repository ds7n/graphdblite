#!/usr/bin/env python3
"""Analyze TCK harness output and print pass-rate breakdown by area.

Usage:
    cargo test --test tck 2>&1 > /tmp/tck_output.txt
    uv run tests/tck/analyze.py /tmp/tck_output.txt

Reads cucumber-rs stdout, cross-references the skiplist, and prints a
markdown table of pass rates grouped by feature area.
"""

import re
import sys
from collections import Counter
from pathlib import Path

TCK_DIR = Path(__file__).parent
SKIPLIST = TCK_DIR / "skiplist.txt"


def load_skiplist() -> set[str]:
    """Load Feature::Scenario keys from skiplist.txt."""
    if not SKIPLIST.exists():
        return set()
    return {
        line.strip()
        for line in SKIPLIST.read_text().splitlines()
        if line.strip() and not line.startswith("#")
    }


def area_name(feature: str) -> str:
    """Extract base area name from a feature name (e.g. 'Create1 - ...' -> 'Create')."""
    m = re.match(r"^([A-Za-z]+)", feature)
    return m.group(1) if m else feature


def analyze(output: str) -> None:
    """Parse cucumber output and print area breakdown."""
    current_feature = ""
    current_scenario = ""
    has_failure = False

    # Per unique Feature::Scenario: track if any instance ever failed
    ever_failed: set[str] = set()
    seen: set[str] = set()

    for line in output.splitlines():
        fm = re.match(r"^Feature: (.+)$", line)
        if fm:
            if current_feature and current_scenario:
                key = f"{current_feature}::{current_scenario}"
                seen.add(key)
                if has_failure:
                    ever_failed.add(key)
            current_feature = fm.group(1).strip()
            current_scenario = ""
            has_failure = False
            continue

        sm = re.match(r"^  Scenario(?: Outline)?: (.+)$", line)
        if sm:
            if current_feature and current_scenario:
                key = f"{current_feature}::{current_scenario}"
                seen.add(key)
                if has_failure:
                    ever_failed.add(key)
            current_scenario = sm.group(1).strip()
            has_failure = False
            continue

        if line.startswith("   ✘"):
            has_failure = True

    # Last scenario
    if current_feature and current_scenario:
        key = f"{current_feature}::{current_scenario}"
        seen.add(key)
        if has_failure:
            ever_failed.add(key)

    skiplist = load_skiplist()

    # Count per area
    area_total: Counter[str] = Counter()
    area_skipped: Counter[str] = Counter()
    area_failed: Counter[str] = Counter()

    for key in seen:
        feat = key.split("::")[0]
        area = area_name(feat)
        area_total[area] += 1
        if key in skiplist:
            area_skipped[area] += 1
        elif key in ever_failed:
            area_failed[area] += 1

    # Print summary
    total_all = sum(area_total.values())
    total_skipped = sum(area_skipped.values())
    total_failed = sum(area_failed.values())
    total_passed = total_all - total_skipped - total_failed

    print(f"Total: {total_all} scenarios, {total_passed} passed, "
          f"{total_skipped} skiplisted, {total_failed} new failures\n")

    # Print table sorted by pass% descending
    print(f"| {'Area':<28} | {'Skip':>5} | {'Total':>5} | {'Pass%':>5} |")
    print(f"|{'-'*30}|{'-'*7}|{'-'*7}|{'-'*7}|")

    rows = []
    for area in area_total:
        total = area_total[area]
        skipped = area_skipped[area]
        failed = area_failed[area]
        passing = total - skipped - failed
        pct = passing / total * 100 if total > 0 else 0
        rows.append((area, skipped, total, pct))

    for area, skipped, total, pct in sorted(rows, key=lambda r: -r[3]):
        print(f"| {area:<28} | {skipped:>5} | {total:>5} | {pct:>4.0f}% |")


def main() -> None:
    """Entry point."""
    if len(sys.argv) < 2:
        print(f"Usage: {sys.argv[0]} <tck_output_file>", file=sys.stderr)
        print("  Run: cargo test --test tck 2>&1 > /tmp/tck_output.txt", file=sys.stderr)
        sys.exit(1)

    output_path = Path(sys.argv[1])
    if not output_path.exists():
        print(f"File not found: {output_path}", file=sys.stderr)
        sys.exit(1)

    analyze(output_path.read_text())


if __name__ == "__main__":
    main()
