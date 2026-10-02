#!/usr/bin/env python3
"""Reproducible scalable-I/O qualification. Python stdlib only; see docs/self-check-guide.md."""

import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import subprocess
import sys
import time


ROOT = Path(__file__).resolve().parents[1]
SUMMARY = re.compile(r"test result: ok\. (\d+) passed; (\d+) failed;")


def capture(command):
    return subprocess.check_output(command, cwd=ROOT, text=True, timeout=30).strip()


def source_identity():
    # Include untracked source files: a HEAD hash alone does not identify the
    # code a local qualification actually compiled. Reports belong in target/.
    names = subprocess.check_output(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
        cwd=ROOT, timeout=30,
    ).split(b"\0")
    digest = hashlib.sha256()
    for name in sorted(set(names) - {b""}):
        path = ROOT / os.fsdecode(name)
        digest.update(name + b"\0")
        digest.update(hashlib.sha256(path.read_bytes()).digest() if path.is_file() else b"deleted")
    return {
        "head": capture(["git", "rev-parse", "HEAD"]),
        "status": capture(["git", "status", "--short"]),
        "source_sha256": digest.hexdigest(),
    }


def classify(returncode, output, expected, required_marker=None, timed_out=False):
    if timed_out:
        return "failed", "process-tree timeout"
    if returncode != 0:
        return "failed", f"exit code {returncode}"
    if "sparse probe skipped:" in output or "sparse offset smoke skipped:" in output:
        return "unsupported", "filesystem did not execute the sparse probe"
    summaries = SUMMARY.findall(output)
    if len(summaries) != 1 or tuple(map(int, summaries[0])) != (expected, 0):
        return "failed", f"expected exactly {expected} passing tests (empty/filtered suites fail)"
    if "Varve test artifact cleanup: verified empty" not in output:
        return "failed", "test-runner cleanup was not verified"
    if required_marker and required_marker not in output:
        return "failed", f"missing execution evidence: {required_marker}"
    return "passed", None


def stop_tree(process):
    if os.name == "nt":
        subprocess.run(["taskkill", "/PID", str(process.pid), "/T", "/F"],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                       check=False, timeout=30)
    else:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    process.wait(timeout=30)


def run_command(command, environment, log, timeout):
    started = time.monotonic()
    timed_out = False
    with log.open("w", encoding="utf-8") as output:
        process = subprocess.Popen(
            command, cwd=ROOT, env=environment, stdout=output,
            stderr=subprocess.STDOUT, start_new_session=os.name != "nt",
            creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0,
        )
        try:
            process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
            stop_tree(process)
        except BaseException:
            stop_tree(process)
            raise
    return process.returncode, timed_out, round(time.monotonic() - started, 3)


def test_command(target, name=None, ignored=False, core=False):
    command = ["cargo", "run", "--locked", "-p", "varve-test-runner", "--",
               "test", "--locked", "-p", "varve-core" if core else "varve", "--all-features"]
    command += ["--lib"] if core else ["--test", target]
    if name:
        command.append(name)
    command += ["--", "--nocapture", "--test-threads=1"]
    if name:
        command.append("--exact")
    if ignored:
        command.append("--ignored")
    return command


def campaign(profile, seeds, epochs, kill_cycles, require_pib):
    stages = []

    def add(name, command, expected=1, env=None, marker=None):
        stages.append({"name": name, "command": command, "expected_tests": expected,
                       "environment": env or {}, "required_marker": marker, "status": "not_run"})

    add("regression", test_command("scalable_qualification"), expected=4)
    add("immediate-policy", test_command("immediate_policy"), expected=6)
    add("immediate-native", test_command("immediate_native"), expected=1)
    add("reader-management", test_command("scalable_reader_management"), expected=6)
    add("schema-key-kinds", test_command("schema_key_kinds"), expected=7)
    add("finite-key-limit", test_command("finite_key_limit"), expected=1)
    add("finite-slot-storage", test_command("finite_slot_storage"), expected=7, marker="FINITE_SLOT_CRASH aborts=62")
    add("native-redb-oracle", test_command("native_redb_differential"), marker="ORACLE seed=42")
    add("index-redb-oracle", test_command(None, "disk_index::oracle::randomized_pages_match_redb_across_splits_and_old_roots", core=True), marker="INDEX_ORACLE seed=42")
    add("reader-follow", test_command("scalable_reader_follow"), expected=7, marker="FOLLOW_CONCURRENT readers=8 generations=100")
    add("reader-open-churn", test_command(
        "scalable_qualification", "opening_readers_does_not_interrupt_the_single_writer"))
    add("fault-matrix", test_command("scalable_crash_faults", "enabled::scalable_crash_fault_matrix"))
    for seed in seeds:
        add(f"history-{seed}", test_command("scalable_qualification", "model_history_stress", True),
            env={"VARVE_QUAL_SEED": str(seed), "VARVE_QUAL_EPOCHS": str(epochs)},
            marker=f"QUAL_HISTORY seed={seed} epochs={epochs}")
    add("external-kill", test_command("scalable_qualification", "external_kill_stress", True),
        env={"VARVE_QUAL_KILL_CYCLES": str(kill_cycles)}, marker=f"QUAL_KILL epoch={kill_cycles}")
    if profile == "qualification":
        add("memory-growth", test_command("high_cardinality", "cardinality_growth_respects_memory_budget", True),
            marker='QUAL_MEMORY {"keys":1000000,')
    add("sparse-tib", test_command(None, "pib_probe::real_file_positional_io_at_one_tib_smoke", True, True),
        marker="sparse offset smoke passed:")
    if require_pib:
        add("sparse-pib", test_command(None, "pib_probe::real_file_positional_io_at_one_pib", True, True),
            env={"VARVE_REQUIRE_PIB_SPARSE": "1"}, marker="PiB sparse probe passed:")
    return stages


def write_report(path, report):
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    temporary.replace(path)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", choices=["smoke", "qualification"], default="smoke")
    parser.add_argument("--seed", type=int, action="append", help="repeatable u64 seed; recorded in report")
    parser.add_argument("--epochs", type=int)
    parser.add_argument("--kill-cycles", type=int)
    parser.add_argument("--timeout", type=int, default=1800, help="seconds per stage, including compilation")
    parser.add_argument("--require-pib", action="store_true", help="fail unless real 1 PiB sparse probe executes")
    parser.add_argument("--output", type=Path, help="new directory outside tracked sources (default: target/qualification/<UTC>)")
    args = parser.parse_args()
    full = args.profile == "qualification"
    seeds = args.seed or ([0, 1, 42, 2**64 - 1] if full else [42])
    epochs = args.epochs if args.epochs is not None else (64 if full else 8)
    cycles = args.kill_cycles if args.kill_cycles is not None else (64 if full else 8)
    if epochs < (64 if full else 4) or cycles < (64 if full else 4):
        parser.error("qualification requires >=64 epochs and kill cycles; smoke requires >=4")
    if len(set(seeds)) != len(seeds) or any(not 0 <= seed < 2**64 for seed in seeds):
        parser.error("seeds must be distinct unsigned 64-bit integers")
    if full and len(seeds) < 4:
        parser.error("qualification requires at least four distinct seeds")
    if args.timeout <= 0:
        parser.error("timeout must be positive")
    timestamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
    output = (args.output or ROOT / "target" / "qualification" / timestamp).resolve()
    output.mkdir(parents=True, exist_ok=False)
    report_path = output / "report.json"
    environment = os.environ.copy()
    environment["CARGO_TERM_COLOR"] = "never"
    report = {
        "schema_version": 1, "started_utc": timestamp, "workspace": str(ROOT),
        "profile": args.profile, "campaign_status": "running", "stabilization_approved": False,
        "source": source_identity(), "platform": platform.platform(), "architecture": platform.machine(),
        "python": sys.version, "rustc": capture(["rustc", "-Vv"]), "cargo": capture(["cargo", "-V"]),
        "disk_free_bytes": shutil.disk_usage(output).free,
        "build_environment": {k: v for k, v in environment.items() if k in (
            "RUSTFLAGS", "RUSTUP_TOOLCHAIN", "CARGO_BUILD_JOBS", "CARGO_TARGET_DIR",
            "CARGO_PROFILE_DEV_DEBUG", "CARGO_PROFILE_TEST_DEBUG", "CARGO_INCREMENTAL", "VARVE_PIB_TEST_DIR")},
        "seeds": seeds, "epochs": epochs, "kill_cycles": cycles, "stage_timeout_seconds": args.timeout,
        "not_covered": ["cross-platform results from other hosts", "power loss / torn storage writes",
                        "long sanitizer and fuzz campaigns", "released sidecar fixture compatibility",
                        "production workload soak and resource exhaustion"],
        "stages": campaign(args.profile, seeds, epochs, cycles, args.require_pib),
    }
    if not full:
        report["not_covered"].append("10000/100000/1000000-key memory growth")
    if not args.require_pib:
        report["not_covered"].append("1 PiB sparse positional I/O")
    write_report(report_path, report)
    print(f"Qualification report: {report_path}", flush=True)
    try:
        for stage in report["stages"]:
            print(f"Running {stage['name']} ...", flush=True)
            log = output / (stage["name"] + ".log")
            stage["log"] = str(log)
            stage["status"] = "running"
            write_report(report_path, report)
            stage_env = environment | stage["environment"]
            code, timeout, elapsed = run_command(stage["command"], stage_env, log, args.timeout)
            contents = log.read_text(encoding="utf-8", errors="replace")
            status, reason = classify(code, contents, stage["expected_tests"], stage["required_marker"], timeout)
            stage.update(status=status, reason=reason, returncode=code, timed_out=timeout, elapsed_seconds=elapsed)
            stage["memory_samples"] = [json.loads(line) for line in re.findall(r"QUAL_MEMORY (\{[^\n]+\})", contents)]
            write_report(report_path, report)
            print(f"{stage['name']}: {status} ({elapsed}s){': ' + reason if reason else ''}", flush=True)
        report["source_after"] = source_identity()
        report["source_unchanged"] = report["source"] == report["source_after"]
        passed = report["source_unchanged"] and all(s["status"] == "passed" for s in report["stages"])
        report["campaign_status"] = "passed" if passed else "failed"
    except (Exception, KeyboardInterrupt) as error:
        report["campaign_status"] = "interrupted" if isinstance(error, KeyboardInterrupt) else "failed"
        report["error"] = repr(error)
        for stage in report["stages"]:
            if stage["status"] == "running":
                stage["status"] = "interrupted" if isinstance(error, KeyboardInterrupt) else "failed"
        passed = False
    write_report(report_path, report)
    print(f"Campaign: {report['campaign_status']}; this does not approve feature stabilization.\n{report_path}", flush=True)
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())
