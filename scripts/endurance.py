#!/usr/bin/env python3
"""Linux endurance entry point. Matrix readers live for the whole run; I/O rotates real-file cases."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import shutil
import signal
import subprocess
import sys
import time

from load_scalable import ROOT, GIB, atomic_json
from qualify_scalable import source_identity

CASES = ("raw-20g", "indexed-20g", "indexed-r8-20g")


def checked_result(code, report, mode):
    if code != 0 or report.get("status") != "passed":
        raise RuntimeError("child failed; retain logs/data and do not retry automatically")
    if mode == "matrix" and report.get("cleanup_verified") is not True:
        raise RuntimeError("matrix cleanup was not verified")
    if mode == "io" and (not report.get("cases") or
                         any(c.get("status") != "passed" for c in report["cases"])):
        raise RuntimeError("I/O report has missing or failed cases")


def run_child(command, log, timeout, heartbeat):
    with log.open("w") as stream:
        process = subprocess.Popen(command, cwd=ROOT, stdout=stream,
                                   stderr=subprocess.STDOUT, start_new_session=True)
        started = time.monotonic()
        try:
            while process.poll() is None:
                heartbeat(process.pid)
                if time.monotonic() - started > timeout:
                    raise TimeoutError(f"child timeout: {log}")
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    pass
            return process.returncode
        except BaseException:
            # The existing drivers catch SIGINT and kill/reap their separately
            # grouped workload before returning. Do not SIGKILL the driver first.
            if process.poll() is None:
                family = descendants(process.pid)
                os.killpg(process.pid, signal.SIGINT)
                try:
                    process.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    family.update(descendants(process.pid))
                # Also reap detached workload groups if a driver failed to
                # cooperate. Check Linux start times to avoid PID reuse.
                for pid, born in family.items():
                    if process_identity(pid) == born:
                        try:
                            os.kill(pid, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                process.wait()
            raise


def process_identity(pid):
    try:
        return Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[19]
    except (OSError, IndexError):
        return None


def descendants(root_pid):
    parents, identities = {}, {}
    for entry in Path("/proc").iterdir():
        if entry.name.isdigit():
            try:
                fields = (entry / "stat").read_text().rsplit(")", 1)[1].split()
                pid = int(entry.name)
                parents[pid], identities[pid] = int(fields[1]), fields[19]
            except (OSError, ValueError, IndexError):
                pass
    family = {root_pid}
    while True:
        expanded = family | {pid for pid, parent in parents.items() if parent in family}
        if expanded == family:
            return {pid: identities[pid] for pid in family if pid in identities}
        family = expanded


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mode", choices=("matrix", "io"), required=True)
    duration = parser.add_mutually_exclusive_group()
    duration.add_argument("--hours", type=float, default=24)
    duration.add_argument("--seconds", type=int, help="short validation only")
    parser.add_argument("--rows", type=int, default=65536)
    parser.add_argument("--readers", type=int, default=8)
    parser.add_argument("--file-gib", type=float, default=20)
    parser.add_argument("--epoch-mib", type=int, default=256)
    parser.add_argument("--read-seconds", type=int, default=30)
    parser.add_argument("--cycle-timeout", type=int, default=3600)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--data-root", type=Path, required=True)
    parser.add_argument("--plan", action="store_true", help="print preflight/commands only; do not build or run")
    args = parser.parse_args(argv)
    if not math.isfinite(args.hours) or args.hours <= 0 or not math.isfinite(args.file_gib):
        parser.error("finite positive duration and file size required")
    seconds = args.seconds if args.seconds is not None else math.ceil(args.hours * 3600)
    total = int(args.file_gib * GIB)
    epoch = args.epoch_mib * 1024**2
    if seconds <= 0 or args.rows <= 0 or args.rows % 256 or not 1 <= args.readers <= 64:
        parser.error("positive duration, rows divisible by 256, readers 1..64 required")
    if epoch <= 0 or total < 2 * epoch or total % (2 * epoch) or args.read_seconds <= 0 or args.cycle_timeout <= 0:
        parser.error("file size must contain an even number of epochs; timeouts must be positive")
    output, data = args.output.resolve(), args.data_root.resolve()
    for path in (output, data):
        if path == ROOT or ROOT in path.parents or path in ROOT.parents:
            parser.error("reports/data must be outside the source checkout")
        if path.exists() or not path.parent.is_dir():
            parser.error("use new directories with existing parents")
    if output == data or output in data.parents or data in output.parents:
        parser.error("output and data roots must be separate")
    if platform.system() != "Linux":
        parser.error("the resource monitor requires Linux")
    # Reserve report growth too (~20 KB/s estimate), rather than treating a
    # three-day resource trace as free. Never delete caches or reduce the load.
    required = (args.rows * 4096 * 8 if args.mode == "matrix" else math.ceil(total * 1.025)) + 2 * GIB + seconds * 20000
    free = shutil.disk_usage(data.parent).free
    binary = "matrix_soak" if args.mode == "matrix" else "scalable_load"
    build = ["cargo", "build", "--locked", "--release", "-p", "varve",
             "--features", "integrity", "--example", binary]
    plan = {"mode": args.mode, "requested_seconds": seconds, "free_bytes": free,
            "required_free_bytes": required, "disk_ready": free >= required,
            "build": build, "output": str(output), "data_root": str(data),
            "io_cases": CASES if args.mode == "io" else None,
            "duration_policy": "matrix: one continuous reader lifetime; io: finish current case and at least one complete rotation"}
    if args.plan:
        print(json.dumps(plan, indent=2))
        return 0 if plan["disk_ready"] else 2
    if not plan["disk_ready"]:
        parser.error(f"insufficient disk: need {required/GIB:.2f} GiB, have {free/GIB:.2f} GiB")
    output.mkdir()
    data.mkdir()
    owner = data / ".endurance-owner"
    owner.write_text(str(output))
    identity = source_identity()
    report = {"status": "running", "plan": plan, "source": identity,
              "config": {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
              "started_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "cycles": []}

    def heartbeat(pid=None):
        report["active_pid"] = pid
        report["heartbeat_utc"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        atomic_json(output / "report.json", report)

    def interrupted(_signum, _frame):
        raise KeyboardInterrupt

    previous_term = signal.signal(signal.SIGTERM, interrupted)
    try:
        report["phase"] = "build"
        heartbeat()
        if run_child(build, output / "build.log", 1800, heartbeat) != 0:
            raise RuntimeError("release build failed")
        executable = ROOT / "target/release/examples" / binary
        binary_hash = hashlib.sha256(executable.read_bytes()).hexdigest()
        report["binary_sha256"] = binary_hash
        started = time.monotonic()
        cycle = 0
        while True:
            if identity != source_identity() or binary_hash != hashlib.sha256(executable.read_bytes()).hexdigest():
                raise RuntimeError("source/binary changed during campaign")
            name = f"{cycle+1:06d}"
            logs, files = output / name, data / name
            if args.mode == "matrix":
                command = [sys.executable, str(ROOT / "scripts/soak_matrix.py"),
                           "--seconds", str(seconds), "--rows", str(args.rows), "--readers", str(args.readers)]
                timeout = seconds + 660
            else:
                command = [sys.executable, str(ROOT / "scripts/load_scalable.py"), "--suite", "large",
                           "--only", CASES[cycle % len(CASES)], "--total-gib", str(args.file_gib),
                           "--epoch-mib", str(args.epoch_mib), "--read-seconds", str(args.read_seconds),
                           "--timeout", str(args.cycle_timeout)]
                timeout = args.cycle_timeout
            command += ["--output", str(logs), "--data-root", str(files)]
            record = {"cycle": cycle+1, "command": command, "report": str(logs / "report.json"), "status": "running"}
            report["cycles"].append(record)
            report["phase"] = name
            heartbeat()
            code = run_child(command, output / f"{name}.log", timeout, heartbeat)
            child = json.loads((logs / "report.json").read_text())
            checked_result(code, child, args.mode)
            if args.mode == "io":
                marker = files / ".varve-load-owner"
                if files.is_symlink() or marker.read_text() != str(logs) or set(files.iterdir()) != {marker}:
                    raise RuntimeError("I/O data cleanup/ownership mismatch")
                marker.unlink()
                files.rmdir()
            record["status"] = "passed"
            cycle += 1
            report["elapsed_seconds"] = time.monotonic() - started
            heartbeat()
            if args.mode == "matrix" or (cycle >= len(CASES) and report["elapsed_seconds"] >= seconds):
                break
        if identity != source_identity() or binary_hash != hashlib.sha256(executable.read_bytes()).hexdigest():
            raise RuntimeError("source/binary changed during campaign")
        if report["elapsed_seconds"] < seconds:
            raise RuntimeError("requested duration was not completed")
        if data.is_symlink() or owner.read_text() != str(output) or set(data.iterdir()) != {owner}:
            raise RuntimeError("endurance cleanup/ownership mismatch")
        owner.unlink()
        data.rmdir()
        report["cleanup_verified"] = True
        report["status"] = "passed"
    except (Exception, KeyboardInterrupt) as error:
        report["status"] = "interrupted" if isinstance(error, KeyboardInterrupt) else "failed"
        report["error"] = repr(error)
    finally:
        signal.signal(signal.SIGTERM, previous_term)
        heartbeat()
    print(json.dumps({"status": report["status"], "report": str(output / "report.json")}))
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
