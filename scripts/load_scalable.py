#!/usr/bin/env python3
"""Real-byte, Linux-observed load runs. See docs/self-check-guide.md."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import resource
import select
import shutil
import signal
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
GIB = 1024**3


def text(path):
    try:
        return Path(path).read_text()
    except (OSError, ProcessLookupError):
        return ""


def numbers(path):
    result = {}
    for line in text(path).splitlines():
        fields = line.replace(":", "").split()
        if len(fields) >= 2 and fields[1].isdigit():
            result[fields[0]] = int(fields[1])
    return result


def io_stat():
    return {parts[0]: {k: int(v) for k, v in (field.split("=") for field in parts[1:])}
            for line in text("/sys/fs/cgroup/io.stat").splitlines() if (parts := line.split())}


def disk_stat():
    names = ["reads", "reads_merged", "read_sectors", "read_ms", "writes", "writes_merged",
             "write_sectors", "write_ms", "in_flight", "busy_ms", "weighted_ms"]
    return {fields[2]: dict(zip(names, map(int, fields[3:14])))
            for line in text("/proc/diskstats").splitlines() if len(fields := line.split()) >= 14
            and not fields[2].startswith(("loop", "ram"))}


def snapshot(pid=None):
    value = {"monotonic": time.monotonic(), "cgroup_io": io_stat(), "disk": disk_stat(),
             "memory_current": int(text("/sys/fs/cgroup/memory.current").strip() or 0),
             "memory_stat": numbers("/sys/fs/cgroup/memory.stat"),
             "cpu_stat": numbers("/sys/fs/cgroup/cpu.stat"),
             "io_pressure": text("/sys/fs/cgroup/io.pressure").strip()}
    if pid:
        value["process_io"] = numbers(f"/proc/{pid}/io")
        value["process_status"] = {k: v for k, v in numbers(f"/proc/{pid}/status").items()
                                   if k in ("VmRSS", "VmHWM", "RssAnon", "RssFile", "Threads", "voluntary_ctxt_switches", "nonvoluntary_ctxt_switches")}
    return value


def deltas(before, after):
    return {key: {field: value - before.get(key, {}).get(field, 0) for field, value in fields.items()}
            for key, fields in after.items()}


def atomic_json(path, value):
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n")
    temporary.replace(path)


def evict_files(directory):
    advice = []
    for file in sorted(directory.glob("data.varve*")):
        if not file.is_file():
            continue
        try:
            fd = os.open(file, os.O_RDONLY)
            try:
                os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
            finally:
                os.close(fd)
            advice.append({"file": str(file), "advice": "DONTNEED requested (not a guarantee)"})
        except (OSError, AttributeError) as error:
            advice.append({"file": str(file), "error": str(error)})
    return advice


def execute(command, output, name, environment, timeout, required, data_root):
    stdout = output / f"{name}.jsonl"
    stderr = output / f"{name}.stderr"
    monitor = output / f"{name}.resources.jsonl"
    before = snapshot()
    usage_before = resource.getrusage(resource.RUSAGE_CHILDREN)
    started = time.monotonic()
    timed_out = False
    disk_limit = False
    with stdout.open("w") as out, stderr.open("w") as err, monitor.open("w") as samples:
        process = subprocess.Popen(command, cwd=ROOT, env=environment, stdout=out, stderr=err, start_new_session=True)
        pidfd = os.pidfd_open(process.pid)
        peak_rss = peak_anon = peak_memory = 0
        try:
            while process.poll() is None:
                sample = snapshot(process.pid)
                samples.write(json.dumps(sample) + "\n")
                peak_rss = max(peak_rss, sample["process_status"].get("VmHWM", 0) * 1024)
                peak_anon = max(peak_anon, sample["process_status"].get("RssAnon", 0) * 1024)
                peak_memory = max(peak_memory, sample["memory_current"])
                if shutil.disk_usage(data_root).free < 512 * 1024**2:
                    disk_limit = True
                    os.killpg(process.pid, signal.SIGKILL)
                    break
                if time.monotonic() - started > timeout:
                    timed_out = True
                    os.killpg(process.pid, signal.SIGKILL)
                    break
                # Wake at actual process exit instead of rounding short runs
                # up to the monitoring interval.
                select.select([pidfd], [], [], 0.2)
            process.wait(timeout=30)
        except BaseException:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait(timeout=30)
            raise
        finally:
            os.close(pidfd)
    elapsed = time.monotonic() - started
    after = snapshot()
    usage = resource.getrusage(resource.RUSAGE_CHILDREN)
    events = [json.loads(line) for line in stdout.read_text().splitlines() if line.strip()]
    markers = [event.get("event") for event in events]
    if disk_limit:
        status = "disk_reserve_exhausted"
    elif timed_out:
        status = "timeout"
    elif process.returncode == 0 and required in markers:
        status = "passed"
    elif name == "churn" and "contract_failure" in markers:
        status = "contract_failure"
    else:
        status = "failed"
    result = {"command": command, "status": status, "returncode": process.returncode,
              "disk_reserve_bytes": 512 * 1024**2,
              "elapsed_seconds": elapsed, "user_seconds": usage.ru_utime - usage_before.ru_utime,
              "system_seconds": usage.ru_stime - usage_before.ru_stime,
              "child_read_bytes": (usage.ru_inblock - usage_before.ru_inblock) * 512,
              "child_write_bytes": (usage.ru_oublock - usage_before.ru_oublock) * 512,
              "major_faults": usage.ru_majflt - usage_before.ru_majflt,
              "voluntary_switches": usage.ru_nvcsw - usage_before.ru_nvcsw,
              "involuntary_switches": usage.ru_nivcsw - usage_before.ru_nivcsw,
              "peak_rss_bytes": peak_rss, "peak_anon_bytes": peak_anon,
              "peak_cgroup_memory_bytes": peak_memory,
              "cgroup_io_delta": deltas(before["cgroup_io"], after["cgroup_io"]),
              "memory_stat_delta": {k: v - before["memory_stat"].get(k, 0) for k, v in after["memory_stat"].items()},
              "disk_delta": deltas(before["disk"], after["disk"]),
              "cpu_stat_delta": {k: v - before["cpu_stat"].get(k, 0) for k, v in after["cpu_stat"].items()},
              "events": events, "stdout": str(stdout), "stderr": str(stderr), "resources": str(monitor)}
    atomic_json(output / f"{name}.result.json", result)
    return result


def case_command(binary, case, directory, phase, total, epoch, read_seconds):
    values = {"phase": phase, "mode": case["mode"], "root": str(directory), "total-bytes": total,
              "epoch-bytes": epoch, "record-bytes": case.get("record", 65536),
              "batch-bytes": case.get("batch", 4 * 1024**2), "index-batch-records": case.get("index_batch", 16384),
              "cache-bytes": case.get("cache", 8 * 1024**2), "readers": case.get("readers", 0),
              "pinned-readers": case.get("pinned", 1),
              "crc": str(case.get("crc", True)).lower(), "reader-pause-us": case.get("pause_us", 0), "seconds": read_seconds}
    return [str(binary)] + [arg for key, value in values.items() for arg in ("--" + key, str(value))]


def cases(suite):
    if suite == "tuned":
        return [
            {"name": "raw-20g", "mode": "raw"},
            {"name": "indexed-20g", "mode": "indexed", "churn": True},
            {"name": "indexed-r4-20g", "mode": "indexed", "readers": 4},
            {"name": "indexed-r8-20g", "mode": "indexed", "readers": 8},
            {"name": "indexed-r5-refresh-only-20g", "mode": "indexed", "readers": 5, "pinned": 0},
            {"name": "indexed-batch16m-20g", "mode": "indexed", "batch": 16 * 1024**2},
            {"name": "indexed-r4-batch16m-20g", "mode": "indexed", "readers": 4, "batch": 16 * 1024**2},
            {"name": "indexed-record4k-20g", "mode": "indexed", "record": 4096},
        ]
    if suite == "large":
        return [
            {"name": "raw-20g", "mode": "raw"},
            {"name": "stream-20g", "mode": "stream"},
            {"name": "stream-r4-20g", "mode": "stream", "readers": 4},
            {"name": "indexed-20g", "mode": "indexed", "churn": True},
            {"name": "indexed-r4-20g", "mode": "indexed", "readers": 4},
            {"name": "indexed-r8-20g", "mode": "indexed", "readers": 8},
        ]
    if suite == "sweep":
        return [
            {"name": "indexed-batch-64k", "mode": "indexed", "batch": 65536},
            {"name": "indexed-batch-1m", "mode": "indexed", "batch": 1024**2},
            {"name": "indexed-batch-4m", "mode": "indexed"},
            {"name": "indexed-batch-16m", "mode": "indexed", "batch": 16 * 1024**2},
            {"name": "indexed-no-crc", "mode": "indexed", "crc": False},
            {"name": "indexed-record-4k", "mode": "indexed", "record": 4096},
            {"name": "indexed-cache-64m", "mode": "indexed", "cache": 64 * 1024**2},
            {"name": "indexed-r1", "mode": "indexed", "readers": 1},
            {"name": "indexed-r2", "mode": "indexed", "readers": 2},
            {"name": "indexed-r4", "mode": "indexed", "readers": 4},
            {"name": "indexed-r8", "mode": "indexed", "readers": 8},
            {"name": "indexed-r16", "mode": "indexed", "readers": 16},
            {"name": "indexed-r4-pause100us", "mode": "indexed", "readers": 4, "pause_us": 100},
        ]
    return [{"name": "raw-smoke", "mode": "raw"},
            {"name": "stream-smoke", "mode": "stream", "readers": 2},
            {"name": "indexed-smoke", "mode": "indexed", "readers": 4, "churn": True}]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--suite", choices=["smoke", "sweep", "large", "tuned"], default="smoke")
    parser.add_argument("--only", action="append", help="case name, repeat to select several")
    parser.add_argument("--total-gib", type=float)
    parser.add_argument("--epoch-mib", type=int)
    parser.add_argument("--repeat", type=int, default=1)
    parser.add_argument("--data-root", type=Path, required=True, help="new directory owned by this run")
    parser.add_argument("--output", type=Path, required=True, help="new report directory")
    parser.add_argument("--binary", type=Path, default=ROOT / "target/release/examples/scalable_load")
    parser.add_argument("--timeout", type=int, default=1800)
    parser.add_argument("--read-seconds", type=int, default=10)
    parser.add_argument("--instrument", choices=["none", "strace", "perf"], default="none")
    args = parser.parse_args()
    if platform.system() != "Linux":
        parser.error("the monitor currently requires Linux; the Rust workload is portable")
    selected = [case for case in cases(args.suite) if not args.only or case["name"] in args.only]
    if not selected or (args.only and set(args.only) != {case["name"] for case in selected}):
        parser.error("unknown or empty case selection")
    total = int((args.total_gib if args.total_gib is not None else {"large": 20, "tuned": 20, "sweep": 1, "smoke": 0.25}[args.suite]) * GIB)
    epoch = (args.epoch_mib or (32 if args.suite == "smoke" else 256)) * 1024**2
    if args.repeat < 1 or total < 2 * epoch or total % (2 * epoch) or args.read_seconds < 1:
        parser.error("need positive repetitions, at least two aligned epochs, and positive read duration")
    binary = args.binary.resolve(strict=True)
    binary_hash = hashlib.sha256(binary.read_bytes()).hexdigest()
    output = args.output.resolve(); output.mkdir(parents=True, exist_ok=False)
    data_root = args.data_root.resolve(); data_root.mkdir(parents=True, exist_ok=False)
    (data_root / ".varve-load-owner").write_text(str(output))
    environment = os.environ.copy()
    report = {"schema_version": 1, "suite": args.suite, "platform": platform.platform(),
              "binary": str(binary), "binary_sha256": binary_hash,
              "git_head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
              "git_status": subprocess.check_output(["git", "status", "--short"], cwd=ROOT, text=True),
              "cpu_max": text("/sys/fs/cgroup/cpu.max").strip(), "memory_max": text("/sys/fs/cgroup/memory.max").strip(),
              "instrumentation": args.instrument, "total_payload_bytes_per_case": total, "epoch_bytes": epoch,
              "status": "running", "cases": [],
              "scope": "ordinary files with every payload byte written; oracle-checked content; single writer and independent readers"}
    atomic_json(output / "report.json", report)
    source_files = subprocess.check_output(["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"], cwd=ROOT).split(b"\0")
    source_hashes = {os.fsdecode(name): hashlib.sha256((ROOT / os.fsdecode(name)).read_bytes()).hexdigest()
                     for name in sorted(set(source_files) - {b""}) if (ROOT / os.fsdecode(name)).is_file()}
    atomic_json(output / "source-files.json", source_hashes)
    markers = {"write": "write_complete", "verify": "verify_complete", "point": "point_complete",
               "random": "random_complete", "churn": "churn_complete", "restore": "restore_complete", "cross-read": "cross_read_complete"}
    try:
        for repeat in range(args.repeat):
            for case in selected:
                if hashlib.sha256(binary.read_bytes()).hexdigest() != binary_hash:
                    raise RuntimeError("benchmark binary changed during run")
                if shutil.disk_usage(data_root).free < total * 1.025 + GIB:
                    raise RuntimeError("insufficient free disk for payload, sidecars and 1 GiB reserve")
                name = f"{repeat + 1:02d}-{case['name']}"
                directory = data_root / name; directory.mkdir()
                logs = output / name; logs.mkdir()
                record = {"name": name, "config": case, "stages": [], "status": "running"}
                report["cases"].append(record)
                stages = ["write", "verify"]
                if case["mode"] == "indexed":
                    stages += ["point", "random"]
                    if case.get("churn"):
                        stages += ["cross-read", "churn", "restore", "point"]
                for index, phase in enumerate(stages):
                    print(f"{name}: {phase}", flush=True)
                    command = case_command(binary, case, directory, phase, total, epoch, args.read_seconds)
                    advice = evict_files(directory) if phase in ("verify", "point", "random") else []
                    if phase == "write" and args.instrument == "perf":
                        command = ["perf", "record", "-e", "cpu-clock:u", "-F", "99", "--call-graph", "fp", "-o", str(logs / "perf.data"), "--", *command]
                    elif phase == "write" and args.instrument == "strace":
                        command = ["strace", "-ff", "-ttt", "-T", "-yy", "-s", "0", "-e",
                                   "trace=read,write,pread64,pwrite64,fsync,fdatasync,openat,close,flock,rename,ftruncate,futex",
                                   "-o", str(logs / "trace"), *command]
                    # Keep the churn phase's name for explicit contract-failure
                    # classification, and distinguish the second point check.
                    stage_name = f"{phase}-after-recovery" if index > 5 and phase == "point" else phase
                    result = execute(command, logs, stage_name, environment, args.timeout, markers[phase], data_root)
                    result["cache_advice"] = advice
                    record["stages"].append({"phase": stage_name, **result})
                    if phase == "write":
                        record["files"] = [{"path": file.name, "logical_bytes": file.stat().st_size,
                                            "allocated_bytes": file.stat().st_blocks * 512}
                                           for file in directory.iterdir() if file.is_file()]
                        native = next(file for file in record["files"] if file["path"] == "data.varve")
                        if native["allocated_bytes"] < total * 0.99:
                            raise RuntimeError("native file is sparse/compressed below the real-byte acceptance bound")
                    atomic_json(output / "report.json", report)
                    print(f"  {result['status']}: {result['elapsed_seconds']:.3f}s, guest writes={result['child_write_bytes'] / GIB:.3f} GiB, reads={result['child_read_bytes'] / GIB:.3f} GiB", flush=True)
                    if result["status"] not in ("passed", "contract_failure"):
                        raise RuntimeError(f"{name}/{phase} failed; test data retained in {directory}")
                before_free = shutil.disk_usage(data_root).free
                before_delete = io_stat()
                deletion = time.monotonic()
                if directory.parent != data_root or directory.is_symlink() or (data_root / ".varve-load-owner").read_text() != str(output):
                    raise RuntimeError("refusing cleanup outside this run's owned directory")
                shutil.rmtree(directory)
                fd = os.open(data_root, os.O_RDONLY | os.O_DIRECTORY)
                try:
                    os.fsync(fd)
                finally:
                    os.close(fd)
                record["delete_seconds"] = time.monotonic() - deletion
                record["free_bytes_recovered"] = shutil.disk_usage(data_root).free - before_free
                record["delete_io_delta"] = deltas(before_delete, io_stat())
                record["status"] = "contract_failure" if any(stage["status"] == "contract_failure" for stage in record["stages"]) else "passed"
                atomic_json(output / "report.json", report)
        report["status"] = "contract_failure" if any(case["status"] == "contract_failure" for case in report["cases"]) else "passed"
    except (Exception, KeyboardInterrupt) as error:
        report["status"] = "interrupted" if isinstance(error, KeyboardInterrupt) else "failed"
        report["error"] = repr(error)
    atomic_json(output / "report.json", report)
    print(f"Study: {report['status']}\n{output / 'report.json'}", flush=True)
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
