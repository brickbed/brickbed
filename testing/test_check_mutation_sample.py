"""Tests for the mutation release-gate parser."""

from __future__ import annotations

import importlib.util
import io
import json
import os
import subprocess
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

    def test_allowlist_unescapes_a_mutant_name_with_a_pipe(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            allowlist = Path(temporary) / "allowlist.md"
            allowlist.write_text(
                "| Mutant | Reason | Tracking issue | Expires |\n"
                "| --- | --- | --- | --- |\n"
                "| replace ^ with \\| in encode_value | Equivalent: sign bit is zero | #8 | 2099-01-01 |\n",
                encoding="utf-8",
            )

            entries = CHECK.allowlisted_mutants(allowlist)

        self.assertEqual(
            entries,
            {"replace ^ with | in encode_value": CHECK.dt.date(2099, 1, 1)},
        )

    def test_mutation_script_reads_the_v27_nested_output_directory(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fake_bin = root / "bin"
            fake_bin.mkdir()
            output_dir = root / "missing" / "parent" / "mutants"
            cargo = fake_bin / "cargo"
            cargo.write_text(
                "#!/usr/bin/env bash\n"
                "set -euo pipefail\n"
                "output=\n"
                "while (($#)); do\n"
                "  if [[ $1 == --output ]]; then output=$2; shift 2; else shift; fi\n"
                "done\n"
                "[[ -d $(dirname \"$output\") ]] || exit 88\n"
                "mkdir -p \"$output/mutants.out\"\n"
                "printf '%s' '{\"end_time\":\"2026-08-08T00:00:00Z\",\"caught\":1,\"missed\":0,\"timeout\":0,\"unviable\":0,\"outcomes\":[{\"scenario\":\"Baseline\",\"summary\":\"Success\"}]}' > \"$output/mutants.out/outcomes.json\"\n"
                ": > \"$output/mutants.out/missed.txt\"\n",
                encoding="utf-8",
            )
            cargo.chmod(0o755)
            environment = os.environ | {
                "PATH": f"{fake_bin}{os.pathsep}{os.environ['PATH']}",
                "MUTANTS_OUTPUT_DIR": str(output_dir),
            }

            result = subprocess.run(
                ["bash", str(ROOT / "scripts" / "mutation-sample.sh")],
                cwd=ROOT,
                env=environment,
                capture_output=True,
                text=True,
                check=False,
            )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Mutation sample: 1 caught", result.stdout)


if __name__ == "__main__":
    unittest.main()
