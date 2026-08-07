#!/usr/bin/env python3
"""Enforce Brickbed's measured line-coverage ratchet from LCOV reports."""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable


@dataclass
class Coverage:
    hit: int = 0
    found: int = 0

    @property
    def percent(self) -> float:
        return 100.0 if self.found == 0 else self.hit * 100.0 / self.found

    def add(self, hits: int) -> None:
        self.found += 1
        if hits > 0:
            self.hit += 1


def parse_report_spec(value: str) -> tuple[str, Path, Path]:
    try:
        name, report, source_root = value.split("=", 2)
    except ValueError as error:
        raise argparse.ArgumentTypeError(
            "report must be NAME=LCOV_PATH=SOURCE_ROOT"
        ) from error
    return name, Path(report), Path(source_root)


def source_path(raw: str, root: Path, repo: Path) -> str:
    path = Path(raw)
    if not path.is_absolute():
        path = root / path
    try:
        return path.resolve().relative_to(repo).as_posix()
    except ValueError:
        return path.as_posix()


def parse_lcov(path: Path, source_root: Path, repo: Path) -> dict[str, dict[int, int]]:
    files: dict[str, dict[int, int]] = defaultdict(dict)
    current: str | None = None
    for raw in path.read_text(encoding="utf-8").splitlines():
        if raw.startswith("SF:"):
            current = source_path(raw[3:], source_root, repo)
        elif raw.startswith("DA:") and current is not None:
            line, hits, *_ = raw[3:].split(",")
            files[current][int(line)] = int(hits)
    return files


def coverage_for(lines: Iterable[int], report: dict[int, int]) -> Coverage:
    result = Coverage()
    for line in lines:
        result.add(report.get(line, 0))
    return result


def report_total(report: dict[str, dict[int, int]]) -> Coverage:
    result = Coverage()
    for lines in report.values():
        for hits in lines.values():
            result.add(hits)
    return result


def merge_base(base_ref: str) -> str:
    return subprocess.check_output(
        ["git", "merge-base", base_ref, "HEAD"], text=True
    ).strip()


def changed_added_lines(base_ref: str) -> dict[str, set[int]]:
    base = merge_base(base_ref)
    diff = subprocess.check_output(
        ["git", "diff", "--no-ext-diff", "--unified=0", f"{base}...HEAD", "--"],
        text=True,
    )
    result: dict[str, set[int]] = defaultdict(set)
    current: str | None = None
    for line in diff.splitlines():
        if line.startswith("+++ b/"):
            current = line[6:]
        elif line.startswith("@@") and current is not None:
            match = re.search(r"\+(\d+)(?:,(\d+))?", line)
            if match:
                start = int(match.group(1))
                length = int(match.group(2) or "1")
                result[current].update(range(start, start + length))
    return result


def changed_source_lines(
    changed: dict[str, set[int]],
    reports: dict[str, dict[str, dict[int, int]]],
    roots: dict[str, str],
) -> dict[str, dict[str, set[int]]]:
    result: dict[str, dict[str, set[int]]] = defaultdict(dict)
    for suite, report in reports.items():
        for filename, lines in changed.items():
            root = roots[suite].rstrip("/") + "/"
            extension = ".rs" if suite == "rust" else ".ts"
            if filename in report or (filename.startswith(root) and filename.endswith(extension)):
                result[suite][filename] = lines
    return result


def new_files(base_ref: str) -> set[str]:
    base = merge_base(base_ref)
    output = subprocess.check_output(
        ["git", "diff", "--name-only", "--diff-filter=AR", f"{base}...HEAD", "--"],
        text=True,
    )
    return set(filter(None, output.splitlines()))


def markdown_row(name: str, value: Coverage, minimum: float | None) -> str:
    target = "—" if minimum is None else f"≥ {minimum:.2f}%"
    return f"| {name} | {value.hit}/{value.found} | {value.percent:.2f}% | {target} |"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--policy", default="testing/coverage-policy.json")
    parser.add_argument("--baseline", default="testing/coverage-baseline.json")
    parser.add_argument("--base-ref", default=os.environ.get("COVERAGE_BASE_REF", "origin/main"))
    parser.add_argument("--summary", action="store_true")
    parser.add_argument("--report", action="append", required=True, type=parse_report_spec)
    args = parser.parse_args()

    repo = Path.cwd().resolve()
    policy = json.loads(Path(args.policy).read_text(encoding="utf-8"))
    baseline = json.loads(Path(args.baseline).read_text(encoding="utf-8"))
    reports = {
        name: parse_lcov(path, root, repo)
        for name, path, root in args.report
    }
    totals = {name: report_total(report) for name, report in reports.items()}
    failures: list[str] = []
    baseline_suites = baseline["suites"]

    for name, total in totals.items():
        minimum = baseline_suites[name]["line_coverage_percent"]
        if total.percent + 1e-9 < minimum:
            failures.append(
                f"{name} line coverage fell to {total.percent:.2f}%, below ratchet {minimum:.2f}%"
            )

    changed = changed_added_lines(args.base_ref)
    changed_reports = changed_source_lines(changed, reports, policy["covered_source_roots"])
    changed_total = Coverage()
    for suite, files in changed_reports.items():
        for filename, lines in files.items():
            report = reports[suite].get(filename, {})
            # LCOV only lists executable lines. Braces, comments, declarations,
            # and TypeScript type-only lines are deliberately not counted when
            # a file is instrumented. A changed production file missing from a
            # report is counted as uncovered instead of escaping the gate.
            executable = lines.intersection(report) if report else lines
            part = coverage_for(executable, report)
            changed_total.hit += part.hit
            changed_total.found += part.found
    changed_minimum = policy["changed_line_minimum_percent"]
    if changed_total.found and changed_total.percent + 1e-9 < changed_minimum:
        failures.append(
            f"changed production lines are {changed_total.percent:.2f}% covered, below {changed_minimum:.2f}%"
        )

    critical_prefixes = tuple(policy["new_critical_rust_module_prefixes"])
    for filename in new_files(args.base_ref):
        if not filename.startswith(critical_prefixes):
            continue
        report = reports.get("rust", {}).get(filename, {})
        # A new safety-critical module must appear in the Rust report.  Treat a
        # missing report entry as fully uncovered; otherwise an uninstrumented
        # file would incorrectly turn into a vacuous 100% result.
        executable = report.keys() if report else changed.get(filename, set())
        critical = coverage_for(executable, report)
        minimum = policy["new_critical_rust_module_minimum_percent"]
        if critical.percent + 1e-9 < minimum:
            failures.append(
                f"new critical module {filename} is {critical.percent:.2f}% covered, below {minimum:.2f}%"
            )

    lines = ["## Coverage", "", "| Suite | Lines | Coverage | Ratchet |", "| --- | ---: | ---: | ---: |"]
    for name, total in totals.items():
        lines.append(markdown_row(name, total, baseline_suites[name]["line_coverage_percent"]))
    if changed_total.found:
        lines.append(markdown_row("Changed production lines", changed_total, changed_minimum))
    else:
        lines.append("| Changed production lines | 0/0 | n/a | ≥ %.2f%% |" % changed_minimum)
    if failures:
        lines.extend(["", "### Gate failures", ""])
        lines.extend(f"- {failure}" for failure in failures)

    output = "\n".join(lines) + "\n"
    print(output, end="")
    if args.summary and os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as summary:
            summary.write(output)
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
