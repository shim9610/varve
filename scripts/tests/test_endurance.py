"""Fail-closed verdicts, ownership guards, and process cleanup for the endurance driver."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import endurance


class EnduranceTests(unittest.TestCase):
    def test_rejects_false_success(self):
        for code, data, mode in [
            (1, {"status": "passed", "cleanup_verified": True}, "matrix"),
            (0, {"status": "running"}, "matrix"),
            (0, {"status": "passed"}, "matrix"),
            (0, {"status": "passed", "cases": []}, "io"),
            (0, {"status": "passed", "cases": [{"status": "contract_failure"}]}, "io"),
        ]:
            with self.subTest(code=code, data=data, mode=mode):
                with self.assertRaises(RuntimeError):
                    endurance.checked_result(code, data, mode)
        endurance.checked_result(0, {"status": "passed", "cleanup_verified": True}, "matrix")
        endurance.checked_result(0, {"status": "passed", "cases": [{"status": "passed"}]}, "io")

    def test_plan_does_not_create_directories(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            output, data = root / "report", root / "data"
            result = subprocess.run([sys.executable, endurance.__file__, "--mode", "matrix",
                                     "--seconds", "1", "--rows", "256", "--output", str(output),
                                     "--data-root", str(data), "--plan"], capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads(result.stdout)["requested_seconds"], 1)
            self.assertFalse(output.exists())
            self.assertFalse(data.exists())

    def test_existing_data_is_never_removed(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            sentinel = root / "keep"
            sentinel.write_text("keep")
            result = subprocess.run([sys.executable, endurance.__file__, "--mode", "matrix",
                                     "--output", str(root / "out"), "--data-root", str(root)],
                                    capture_output=True)
            self.assertEqual(result.returncode, 2)
            self.assertEqual(sentinel.read_text(), "keep")

    @unittest.skipUnless(sys.platform == "linux", "Linux process tree cleanup")
    def test_timeout_stops_detached_descendant(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            pid_file = root / "pid"
            child = "import time; time.sleep(120)"
            parent = ("import subprocess,sys,time,pathlib; "
                      f"p=subprocess.Popen([sys.executable,'-c',{child!r}],start_new_session=True); "
                      f"pathlib.Path({str(pid_file)!r}).write_text(str(p.pid)); time.sleep(120)")
            with self.assertRaises(TimeoutError):
                endurance.run_child([sys.executable, "-c", parent], root / "run.log", .1, lambda _: None)
            pid = int(pid_file.read_text())
            # A killed orphan may briefly remain a zombie awaiting init's reap.
            stat = Path(f"/proc/{pid}/stat")
            if stat.exists():
                import time
                for _ in range(50):
                    if not stat.exists() or stat.read_text().rsplit(")", 1)[1].split()[0] == "Z":
                        break
                    time.sleep(.02)
                if stat.exists():
                    self.assertEqual(stat.read_text().rsplit(")", 1)[1].split()[0], "Z")


if __name__ == "__main__":
    unittest.main()
