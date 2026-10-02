"""Negative controls for the qualification harness's pass/fail decisions."""

import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location("qualify", Path(__file__).resolve().parents[1] / "qualify_scalable.py")
qualify = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(qualify)

PASS = "test result: ok. 1 passed; 0 failed; 0 ignored;\nVarve test artifact cleanup: verified empty\n"


class VerdictTests(unittest.TestCase):
    def test_valid_evidence(self):
        self.assertEqual(qualify.classify(0, PASS + "required", 1, "required"), ("passed", None))

    def test_false_greens_are_rejected(self):
        cases = [
            (0, PASS.replace("1 passed", "0 passed"), 1, None, False),
            (0, PASS.replace("1 passed", "2 passed"), 1, None, False),
            (0, PASS + PASS, 1, None, False),
            (1, PASS, 1, None, False),
            (0, "", 1, None, False),
            (0, PASS, 1, "missing marker", False),
            (0, PASS.splitlines()[0], 1, None, False),
            (0, PASS, 1, None, True),
        ]
        for case in cases:
            with self.subTest(case=case):
                self.assertEqual(qualify.classify(*case)[0], "failed")

    def test_sparse_skip_is_not_a_pass(self):
        for marker in ["PiB sparse probe skipped:", "sparse offset smoke skipped:"]:
            self.assertEqual(qualify.classify(0, PASS + marker, 1)[0], "unsupported")

    def test_full_campaign_has_all_required_gates(self):
        stages = qualify.campaign("qualification", [0, 1, 42, 2**64 - 1], 64, 64, True)
        names = [stage["name"] for stage in stages]
        self.assertEqual(len(names), 20)
        for name in ["regression", "immediate-policy", "immediate-native", "reader-open-churn", "reader-follow", "reader-management", "schema-key-kinds", "finite-key-limit", "finite-slot-storage", "native-redb-oracle", "index-redb-oracle", "fault-matrix", "external-kill", "memory-growth", "sparse-tib", "sparse-pib"]:
            self.assertIn(name, names)
        churn = next(stage for stage in stages if stage["name"] == "reader-open-churn")
        self.assertNotIn("--ignored", churn["command"])
        self.assertIn("--exact", churn["command"])
        for stage in stages:
            self.assertIn("--locked", stage["command"])
            self.assertEqual(stage["status"], "not_run")

    def test_rejects_reduced_qualification(self):
        for flags in [["--epochs", "4"], ["--kill-cycles", "4"], ["--seed", "42"]]:
            result = subprocess.run([sys.executable, str(Path(qualify.__file__)), "--profile", "qualification", *flags],
                                    capture_output=True, text=True, check=False)
            self.assertEqual(result.returncode, 2)

    def test_timeout_terminates_process(self):
        with tempfile.TemporaryDirectory() as directory:
            log = Path(directory) / "timeout.log"
            code, timed_out, elapsed = qualify.run_command(
                [sys.executable, "-c", "import time; print('started', flush=True); time.sleep(60)"],
                os.environ.copy(), log, 0.2,
            )
            self.assertTrue(timed_out)
            self.assertNotEqual(code, 0)
            self.assertLess(elapsed, 15)


if __name__ == "__main__":
    unittest.main()
