#!/usr/bin/env python3
"""Run the ignored finite-key representation benchmark with controlled workloads.

Build first: cargo test -p varve --all-features --release --test
finite_key_benchmark --no-run. Pass the resulting test executable as --binary.
Each process creates, fully validates and removes its own tempfile directory.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import statistics
import subprocess
import time


def counters(pid):
    result = {}
    for source in ("status", "io"):
        try:
            for line in Path(f"/proc/{pid}/{source}").read_text().splitlines():
                name, _, value = line.partition(":")
                if name in ("VmHWM", "rchar", "wchar", "read_bytes", "write_bytes"):
                    result[name] = int(value.strip().split()[0])
        except (FileNotFoundError, ProcessLookupError, PermissionError):
            pass
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--baseline-binary", type=Path,
                        help="compare old and new finite-enum engines, instead of raw vs enum")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--bulk-gib", type=int, default=4)
    parser.add_argument("--locality-only", action="store_true",
                        help="counterfactual key ordering, 63-record batches matching the bulk workload")
    args = parser.parse_args()
    if args.repeats < 1 or args.bulk_gib < 1:
        parser.error("repeats and bulk-gib must be positive")
    binary = args.binary.resolve()
    baseline = args.baseline_binary.resolve() if args.baseline_binary else None
    representations = ("before", "after") if baseline else ("raw", "finite")
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    data = output / "data"
    data.mkdir()
    workloads = [
        dict(name="numeric-64", family="numeric", keys=64, records=1_000_000, payload=0, sync=16384, queries=200_000),
        dict(name="numeric-4096", family="numeric", keys=4096, records=1_000_000, payload=0, sync=16384, queries=200_000),
        dict(name="label-64", family="label", keys=64, records=1_000_000, payload=0, sync=16384, queries=200_000),
        dict(name="bulk-4096", family="numeric", keys=4096, records=args.bulk_gib * 16384, payload=65536, sync=1024, queries=4096),
    ]
    if args.locality_only:
        workloads = [dict(name=f"locality-{order}", family="numeric", keys=4096,
                          records=65536, payload=0, sync=1024, queries=4096,
                          key_order=order, batch_records=63) for order in ("schema", "wire")]
    report = dict(status="running", platform=platform.platform(),
                  binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                  repeats=args.repeats, workloads=workloads, cases=[],
                  scope="Same native engine and logical key domain; raw tuple/label vs schema enum. CRC enabled, 4 MiB append buffer, 8 MiB per-reader index cache. Sequential writer and read phases. Fresh reader first pass is OS-warm, not physical cold-disk latency. RSS/I/O are sampled process metrics; not host-device bandwidth.")
    if baseline:
        report["baseline_binary_sha256"] = hashlib.sha256(baseline.read_bytes()).hexdigest()
        report["scope"] = report["scope"].replace(
            "Same native engine and logical key domain; raw tuple/label vs schema enum.",
            "Identical finite schema enum inputs; previous B-tree engine vs direct slot engine.")

    def save():
        (output / "report.json").write_text(json.dumps(report, indent=2) + "\n")

    save()
    for workload in workloads:
        for repeat in range(args.repeats):
            order = representations if repeat % 2 == 0 else representations[::-1]
            for representation in order:
                mode = f"{'finite' if baseline else representation}_{workload['family']}"
                executable = baseline if representation == "before" else binary
                name = f"{workload['name']}-{repeat+1}-{representation}"
                environment = os.environ | {
                    "VARVE_BENCH_MODE": mode, "VARVE_BENCH_DATA": str(data),
                    "VARVE_BENCH_KEY_ORDER": workload.get("key_order", "schema"),
                    "VARVE_BENCH_BATCH_RECORDS": str(workload.get("batch_records", 16384)),
                    **{f"VARVE_BENCH_{key.upper()}": str(workload[key])
                       for key in ("keys", "records", "payload", "sync", "queries")},
                }
                log = output / f"{name}.log"
                started = time.monotonic()
                highwater = {}
                with log.open("w") as stream:
                    process = subprocess.Popen([str(executable), "--ignored", "--exact",
                                                "compare_finite_representation", "--nocapture"],
                                               env=environment, stdout=stream, stderr=subprocess.STDOUT)
                    while process.poll() is None:
                        for key, value in counters(process.pid).items():
                            highwater[key] = max(value, highwater.get(key, 0))
                        time.sleep(0.02)
                if process.returncode:
                    report.update(status="failed", failure_log=str(log))
                    save()
                    raise SystemExit(f"Failed: {log}")
                lines = [line.split("FINITE_BENCH ", 1)[1] for line in log.read_text().splitlines()
                         if "FINITE_BENCH " in line]
                if len(lines) != 1 or any(data.iterdir()):
                    raise RuntimeError("missing benchmark result or temporary file cleanup failed")
                result = json.loads(lines[0])
                result.update(workload=workload["name"], repeat=repeat+1,
                              representation=representation, elapsed_seconds=time.monotonic()-started,
                              sampled_process_counters=highwater, cleanup_verified=True)
                report["cases"].append(result)
                save()
                print(f"{name}: write={result['write_ms']:.1f} ms, query={result['warm_query_ms']:.1f} ms", flush=True)
    summaries = []
    metrics = ("write_ms", "sync_ms", "open_median_ms", "first_pass_ms", "warm_query_ms",
               "scan_ms", "compact_ms", "input_conversion_ms", "native_bytes", "index_bytes", "compacted_index_bytes")
    for workload in workloads:
        summary = dict(workload=workload["name"], metrics={})
        for metric in metrics:
            groups = {r: [c[metric] for c in report["cases"]
                          if c["workload"] == workload["name"] and c["representation"] == r]
                      for r in representations}
            medians = {r: statistics.median(values) for r, values in groups.items()}
            summary["metrics"][metric] = dict(**medians,
                **{("after_change_percent" if baseline else "finite_change_percent"):
                   (medians[representations[1]] / medians[representations[0]] - 1) * 100},
                ranges={r: [min(values), max(values)] for r, values in groups.items()})
        summaries.append(summary)
    report.update(status="passed", summaries=summaries)
    save()


if __name__ == "__main__":
    main()
