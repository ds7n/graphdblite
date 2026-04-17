#!/usr/bin/env python3
"""Identify which missing Cypher constructs block the most skiplisted scenarios.

Usage:
    uv run tests/tck/analyze_blockers.py

Reads the skiplist and feature files directly (no test run needed).
Reports which constructs, if implemented, would unblock the most scenarios.
"""

import re
import sys
from collections import Counter
from pathlib import Path

TCK_DIR = Path(__file__).parent
FEATURES_DIR = TCK_DIR / "features"
SKIPLIST_PATH = TCK_DIR / "skiplist.txt"

# Constructs we believe are not yet (fully) implemented.
# Order matters: more specific patterns should come before general ones
# where the same text could match both.
HARD_CONSTRUCTS = [
    ("write-result (CREATE/MERGE ... RETURN)", r"\bCREATE\b.*\bRETURN\b|\bMERGE\b.*\bRETURN\b"),
    ("CREATE (no RETURN)", r"\bCREATE\b(?!.*\bRETURN\b)"),
    ("DELETE/DETACH DELETE", r"\b(?:DETACH\s+)?DELETE\b"),
    ("SET property/label", r"\bSET\b"),
    ("REMOVE property/label", r"\bREMOVE\b"),
    ("MERGE", r"\bMERGE\b"),
    ("ORDER BY", r"\bORDER\s+BY\b"),
    ("SKIP", r"\bSKIP\b"),
    ("LIMIT", r"\bLIMIT\b"),
    ("CASE/WHEN", r"\bCASE\b"),
    ("OPTIONAL MATCH", r"\bOPTIONAL\s+MATCH\b"),
    ("DISTINCT", r"\bDISTINCT\b"),
    ("var-length rel *", r"\[\s*[:\w]*\s*\*"),
    ("list comprehension", r"\[[\w\s]+IN\b[^]]*\|"),
    ("pattern comprehension", r"\[\s*\(?[\w:]*\)?\s*[-<]"),
    ("EXISTS { subquery }", r"\bEXISTS\s*\{"),
    ("reduce()", r"\breduce\s*\("),
    ("shortestPath()", r"\bshortestPath\s*\("),
    ("temporal types", r"\b(?:datetime|date|time|duration|localtime|localdatetime)\s*\("),
    ("FOREACH", r"\bFOREACH\b"),
    ("UNION", r"\bUNION\b"),
    ("XOR", r"\bXOR\b"),
    ("parameter $param", r"\$\w+"),
    ("hex/octal literal", r"\b0[xo][0-9a-fA-F]+"),
    ("float literal", r"\b\d+\.\d+\b"),
    ("string functions", r"\b(?:toString|toInteger|toFloat|toBoolean|replace|substring|split|left|right|trim|ltrim|rtrim|toLower|toUpper)\s*\("),
    ("math functions", r"\b(?:abs|ceil|floor|round|sign|sqrt|log|exp|rand)\s*\("),
    ("list functions", r"\b(?:range|reverse|tail|head|last|keys|nodes|relationships|labels|size)\s*\("),
    ("aggregation (non-count)", r"\b(?:sum|avg|min|max|collect)\s*\("),
    ("list slicing [a..b]", r"\[\d*\.\.\d*\]"),
    ("map projection", r"\{[^}]*\.\w+"),
    ("IS NULL / IS NOT NULL", r"\bIS\s+(?:NOT\s+)?NULL\b"),
    ("NOT prefix", r"\bNOT\s+"),
    ("IN [list]", r"\bIN\s*\["),
    ("CONTAINS/STARTS/ENDS", r"\b(?:CONTAINS|STARTS\s+WITH|ENDS\s+WITH)\b"),
    ("single()/none()/any()/all()", r"\b(?:single|none|any|all)\s*\("),
    ("string concat +", r"'[^']*'\s*\+|'\s*\+\s*\w"),
    ("list indexing [n]", r"\w\[\d+\]|\w\[-\d+\]"),
]


def load_skiplist() -> set[str]:
    """Load Feature::Scenario keys from skiplist.txt."""
    if not SKIPLIST_PATH.exists():
        return set()
    return {
        line.strip()
        for line in SKIPLIST_PATH.read_text().splitlines()
        if line.strip() and not line.startswith("#")
    }


def parse_feature_file(path: Path, skiplist: set[str]) -> dict[str, str]:
    """Parse a .feature file, return {key: all_query_text} for skiplisted scenarios."""
    text = path.read_text()
    fm = re.search(r"^Feature:\s*(.+)$", text, re.MULTILINE)
    if not fm:
        return {}
    feature_name = fm.group(1).strip()

    scenarios: dict[str, str] = {}
    parts = re.split(r"^  Scenario(?: Outline)?:\s*(.+)$", text, flags=re.MULTILINE)
    for i in range(1, len(parts), 2):
        scenario_name = parts[i].strip()
        body = parts[i + 1] if i + 1 < len(parts) else ""
        key = f"{feature_name}::{scenario_name}"
        if key not in skiplist:
            continue
        queries = re.findall(r'"""\s*\n(.*?)\n\s*"""', body, re.DOTALL)
        scenarios[key] = "\n".join(queries)
    return scenarios


def classify(query_text: str) -> list[str]:
    """Return list of hard constructs found in the query text."""
    return [
        name
        for name, pat in HARD_CONSTRUCTS
        if re.search(pat, query_text, re.IGNORECASE | re.DOTALL)
    ]


def main() -> None:
    """Entry point."""
    skiplist = load_skiplist()
    if not skiplist:
        print("Skiplist is empty — nothing to analyze.", file=sys.stderr)
        sys.exit(0)

    # Collect all queries from skiplisted scenarios
    all_queries: dict[str, str] = {}
    for fpath in sorted(FEATURES_DIR.rglob("*.feature")):
        all_queries.update(parse_feature_file(fpath, skiplist))

    # Classify each scenario
    scenario_constructs: dict[str, list[str]] = {}
    for key, query_text in all_queries.items():
        scenario_constructs[key] = classify(query_text)

    # Count sole blockers and high-impact constructs
    single_blocker: Counter[str] = Counter()
    few_blockers: Counter[str] = Counter()

    for found in scenario_constructs.values():
        if len(found) == 1:
            single_blocker[found[0]] += 1
        if len(found) <= 2:
            for c in found:
                few_blockers[c] += 1

    print(f"Analyzed {len(all_queries)} skiplisted scenarios\n")

    print("=== Sole blocker (fixing this alone unblocks the scenario) ===\n")
    print(f"| {'Count':>5} | {'Impact':>6} | Construct |")
    print(f"|{'-'*7}|{'-'*8}|-----------|")
    for name, count in single_blocker.most_common(25):
        impact = few_blockers.get(name, count)
        print(f"| {count:>5} | {impact:>6} | {name} |")

    print(f"\n=== High-impact (in scenarios with <=2 missing constructs) ===\n")
    print(f"| {'Impact':>6} | Construct |")
    print(f"|{'-'*8}|-----------|")
    for name, count in few_blockers.most_common(25):
        print(f"| {count:>6} | {name} |")

    zero = sum(1 for v in scenario_constructs.values() if len(v) == 0)
    unmatched = len(skiplist) - len(all_queries)
    print(f"\nScenarios with 0 detected blockers: {zero} (harness/comparison issues)")
    print(f"Skiplisted but not matched to feature file: {unmatched}")


if __name__ == "__main__":
    main()
