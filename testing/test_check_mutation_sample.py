"""Tests for the mutation release-gate parser."""

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
    "check_mutation_sample", ROOT / "scripts" / "check-mutation-sample.py"
)
assert SPEC and SPEC.loader
CHECK = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = CHECK
SPEC.loader.exec_module(CHECK)


class MutationSampleTests(unittest.TestCase):
    def run_gate(self, outcome: dict[str, object], missed: str = "", allowlist: str = "") -> tuple[int, str]:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            outcomes_path = root / "outcomes.json"
            missed_path = root / "missed.txt"
            allowlist_path = root / "allowlist.md"
            outcomes_path.write_text(json.dumps(outcome), encoding="utf-8")
            missed_path.write_text(missed, encoding="utf-8")
            allowlist_path.write_text(allowlist, encoding="utf-8")
            output = io.StringIO()
            argv = [
                "check-mutation-sample.py", "--outcomes", str(outcomes_path),
                "--missed", str(missed_path), "--allowlist", str(allowlist_path),
            ]
            with mock.patch.object(sys, "argv", argv), redirect_stdout(output):
                status = CHECK.main()
        return status, output.getvalue()

    @staticmethod
    def outcome(caught: int, missed: int = 0) -> dict[str, object]:
        return {
            "end_time": "2026-08-08T00:00:00Z",
            "caught": caught,
            "missed": missed,
            "timeout": 0,
            "unviable": 0,
            "outcomes": [{"scenario": "Baseline", "summary": "Success"}],
        }

    def test_valid_complete_sample_passes(self) -> None:
        status, output = self.run_gate(self.outcome(caught=9))

        self.assertEqual(status, 0)
        self.assertIn("kill rate 100.00%", output)

    def test_unreviewed_survivor_fails(self) -> None:
        status, output = self.run_gate(self.outcome(caught=9, missed=1), missed="replace x with y\n")

        self.assertEqual(status, 1)
        self.assertIn("unreviewed surviving mutants", output)

    def test_expired_equivalence_record_fails(self) -> None:
        allowlist = "| Mutant | Reason | Tracking issue | Expires |\n| --- | --- | --- | --- |\n| replace x with y | Equivalent: branch is unreachable | #123 | 2000-01-01 |\n"
        status, output = self.run_gate(
            self.outcome(caught=9, missed=1),
            missed="replace x with y\n",
            allowlist=allowlist,
        )

        self.assertEqual(status, 1)
        self.assertIn("expired mutation allowlist entries", output)


if __name__ == "__main__":
    unittest.main()
