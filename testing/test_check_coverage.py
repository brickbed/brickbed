"""Focused tests for the LCOV ratchet implementation."""

from __future__ import annotations

import importlib.util
import io
import json
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "check_coverage", ROOT / "scripts" / "check-coverage.py"
)
assert SPEC and SPEC.loader
CHECK = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = CHECK
SPEC.loader.exec_module(CHECK)


class CheckCoverageTests(unittest.TestCase):
    def run_gate(
        self,
        lcov: str,
        baseline: float,
        changed: dict[str, set[int]] | None = None,
        additions: set[str] | None = None,
        changed_minimum: float = 90.0,
        critical_minimum: float = 95.0,
    ) -> tuple[int, str]:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            report = root / "report.lcov"
            policy = root / "policy.json"
            baseline_path = root / "baseline.json"
            report.write_text(lcov, encoding="utf-8")
            policy.write_text(json.dumps({
                "changed_line_minimum_percent": changed_minimum,
                "new_critical_rust_module_minimum_percent": critical_minimum,
                "covered_source_roots": {"rust": "server/src"},
                "new_critical_rust_module_prefixes": ["server/src/db/"],
            }), encoding="utf-8")
            baseline_path.write_text(json.dumps({
                "suites": {"rust": {"line_coverage_percent": baseline}}
            }), encoding="utf-8")
            output = io.StringIO()
            argv = [
                "check-coverage.py", "--policy", str(policy), "--baseline", str(baseline_path),
                "--report", f"rust={report}=.",
            ]
            with mock.patch.object(CHECK, "changed_added_lines", return_value=changed or {}), \
                 mock.patch.object(CHECK, "new_files", return_value=additions or set()), \
                 mock.patch.object(sys, "argv", argv), \
                 redirect_stdout(output):
                status = CHECK.main()
        return status, output.getvalue()

    def test_lcov_parser_normalizes_relative_source_paths(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            report = root / "coverage.lcov"
            report.write_text("SF:src/client.ts\nDA:2,3\nDA:5,0\nend_of_record\n")

            parsed = CHECK.parse_lcov(report, root / "client", root.resolve())

        self.assertEqual(parsed, {"client/src/client.ts": {2: 3, 5: 0}})
        total = CHECK.report_total(parsed)
        self.assertEqual((total.hit, total.found), (1, 2))
        self.assertEqual(total.percent, 50.0)

    def test_changed_unreported_production_file_is_covered_by_the_gate(self) -> None:
        changed = {"server/src/storage/new.rs": {10, 11}}
        result = CHECK.changed_source_lines(
            changed,
            {"rust": {}},
            {"rust": "server/src"},
        )

        self.assertEqual(result["rust"], changed)
        missing = CHECK.coverage_for(result["rust"]["server/src/storage/new.rs"], {})
        self.assertEqual((missing.hit, missing.found), (0, 2))

    def test_changed_type_only_lines_are_not_counted_when_reported_file_has_no_line(self) -> None:
        changed = {"clients/typescript/src/client.ts": {4, 9}}
        report = {"clients/typescript/src/client.ts": {9: 1}}
        result = CHECK.changed_source_lines(
            changed,
            {"typescript-client": report},
            {"typescript-client": "clients/typescript/src"},
        )

        executable = result["typescript-client"]["clients/typescript/src/client.ts"].intersection(
            report["clients/typescript/src/client.ts"]
        )
        covered = CHECK.coverage_for(executable, report["clients/typescript/src/client.ts"])
        self.assertEqual((covered.hit, covered.found), (1, 1))

    def test_gate_fails_on_overall_baseline_regression(self) -> None:
        status, output = self.run_gate(
            "SF:server/src/db/mod.rs\nDA:1,1\nDA:2,0\nend_of_record\n",
            baseline=60.0,
        )

        self.assertEqual(status, 1)
        self.assertIn("line coverage fell", output)

    def test_gate_fails_below_changed_line_threshold(self) -> None:
        status, output = self.run_gate(
            "SF:server/src/db/mod.rs\nDA:1,1\nDA:2,0\nend_of_record\n",
            baseline=0.0,
            changed={"server/src/db/mod.rs": {1, 2}},
        )

        self.assertEqual(status, 1)
        self.assertIn("changed production lines are 50.00%", output)

    def test_gate_fails_a_new_critical_module_missing_from_lcov(self) -> None:
        status, output = self.run_gate(
            "SF:server/src/index.rs\nDA:1,1\nend_of_record\n",
            baseline=0.0,
            changed={"server/src/db/mod.rs": {1, 2}},
            additions={"server/src/db/mod.rs"},
        )

        self.assertEqual(status, 1)
        self.assertIn("new critical module server/src/db/mod.rs is 0.00%", output)

    def test_gate_accepts_exact_threshold_boundaries(self) -> None:
        status, output = self.run_gate(
            "SF:server/src/db/mod.rs\nDA:1,1\nDA:2,1\nend_of_record\n",
            baseline=100.0,
            changed={"server/src/db/mod.rs": {1, 2}},
            additions={"server/src/db/mod.rs"},
            changed_minimum=100.0,
            critical_minimum=100.0,
        )

        self.assertEqual(status, 0)
        self.assertNotIn("Gate failures", output)

    def test_new_files_treats_rename_destination_as_a_new_critical_path(self) -> None:
        with mock.patch.object(CHECK, "merge_base", return_value="base"), \
             mock.patch.object(CHECK.subprocess, "check_output", return_value="server/src/db/mod.rs\n") as command:
            additions = CHECK.new_files("origin/main")

        self.assertEqual(additions, {"server/src/db/mod.rs"})
        self.assertIn("--diff-filter=AR", command.call_args.args[0])


if __name__ == "__main__":
    unittest.main()
