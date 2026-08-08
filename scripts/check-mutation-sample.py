#!/usr/bin/env python3
"""Validate the bounded cargo-mutants sample and its reviewed exceptions."""

from __future__ import annotations

import argparse
import datetime as dt
import json
import re
from pathlib import Path


def markdown_cells(line: str) -> list[str] | None:
    """Split one Markdown table row, preserving escaped literal pipes."""
    line = line.strip()
    if not line.startswith("|") or not line.endswith("|"):
        return None
    cells: list[str] = []
    cell: list[str] = []
    escaped = False
    for char in line[1:-1]:
        if escaped:
            cell.append(char)
            escaped = False
        elif char == "\\":
            escaped = True
        elif char == "|":
            cells.append("".join(cell).strip())
            cell = []
        else:
            cell.append(char)
    if escaped:
        cell.append("\\")
    cells.append("".join(cell).strip())
    return cells


def allowlisted_mutants(path: Path) -> dict[str, dt.date]:
    result: dict[str, dt.date] = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        cells = markdown_cells(line)
        if cells is None or len(cells) != 4:
            continue
        row = dict(zip(("mutant", "reason", "issue", "expires"), cells))
        if row["mutant"] in {"Mutant", "_None_"} or set(row["mutant"]) <= {"-", ":"}:
            continue
        if not row["reason"].startswith("Equivalent:"):
            raise ValueError(
                f"allowlist entry {row['mutant']!r} must prove equivalence (start with 'Equivalent:')"
            )
        if not re.match(r"(?:#\d+|[A-Z][A-Z0-9_-]*-\d+)$", row["issue"]):
            raise ValueError(f"allowlist entry {row['mutant']!r} needs a tracking issue")
        try:
            result[row["mutant"]] = dt.date.fromisoformat(row["expires"])
        except ValueError as error:
            raise ValueError(
                f"allowlist entry {row['mutant']!r} needs an ISO expiry date"
            ) from error
    return result


def missed_mutants(path: Path) -> set[str]:
    if not path.exists():
        return set()
    return {
        line.strip()
        for line in path.read_text(encoding="utf-8").splitlines()
        if line.strip() and not line.startswith("#")
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--outcomes", type=Path, required=True)
    parser.add_argument("--missed", type=Path, required=True)
    parser.add_argument("--allowlist", type=Path, required=True)
    parser.add_argument("--minimum", type=float, default=90.0)
    args = parser.parse_args()

    outcome = json.loads(args.outcomes.read_text(encoding="utf-8"))
    if not outcome.get("end_time"):
        print("mutation sample did not complete (outcomes.json has no end_time)")
        return 1
    baseline = [
        item for item in outcome.get("outcomes", [])
        if isinstance(item.get("scenario"), str) and item["scenario"].lower() == "baseline"
    ]
    if not baseline or any(item.get("summary") != "Success" for item in baseline):
        print("mutation baseline did not pass; do not score mutants from an invalid test run")
        return 1
    try:
        allowlist = allowlisted_mutants(args.allowlist)
    except ValueError as error:
        print(f"invalid mutation allowlist: {error}")
        return 1

    today = dt.date.today()
    expired = sorted(name for name, expiry in allowlist.items() if expiry < today)
    if expired:
        print("expired mutation allowlist entries: " + ", ".join(expired))
        return 1

    caught = int(outcome.get("caught", 0))
    missed = int(outcome.get("missed", 0))
    timeout = int(outcome.get("timeout", 0))
    unviable = int(outcome.get("unviable", 0))
    survivors = missed_mutants(args.missed)
    unknown = sorted(survivors - allowlist.keys())
    if unknown:
        print("unreviewed surviving mutants: " + ", ".join(unknown))
        return 1
    if len(survivors) != missed:
        print(
            f"missed.txt has {len(survivors)} survivors but outcomes.json reports {missed}; "
            "refuse to score an ambiguous sample"
        )
        return 1
    if timeout:
        print(f"mutation sample has {timeout} timeout(s); classify or fix them before release")
        return 1

    equivalent = len(survivors)
    eligible = caught + missed - equivalent
    if eligible <= 0:
        print("mutation sample has no non-equivalent viable mutants to score")
        return 1
    rate = caught * 100.0 / eligible
    print(
        "Mutation sample: "
        f"{caught} caught, {missed} missed ({equivalent} reviewed equivalent), "
        f"{unviable} unviable; kill rate {rate:.2f}% (minimum {args.minimum:.2f}%)"
    )
    if rate + 1e-9 < args.minimum:
        print("mutation kill rate is below the required minimum")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
