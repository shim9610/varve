#!/usr/bin/env python3
"""Summarize load_scalable.py reports into CSV/JSON (Python standard library)."""
import argparse
import csv
import json
from pathlib import Path
import statistics

MIB = 1024**2
GIB = 1024**3


def quantile(events, percent):
    histograms = [e["latency"].get("histogram_buckets") for e in events]
    if not histograms or any(h is None for h in histograms):
        return None
    bins = [sum(values) for values in zip(*histograms)]
    target = (sum(bins) * percent + 99) // 100
    if not target:
        return None
    seen = 0
    for index, count in enumerate(bins):
        seen += count
        if seen >= target:
            power, sub = divmod(index, 8)
            return ((1 << power) + ((sub + 1) << max(0, power - 3))) / 1000
    raise ValueError("invalid latency histogram")


def summarize(path):
    report = json.loads(path.read_text())
    rows = []
    for case in report["cases"]:
        stages = {stage["phase"]: stage for stage in case["stages"]}
        if "write" not in stages:
            continue
        write = stages["write"]
        epochs = [e for e in write["events"] if e["event"] == "epoch"]
        readers = [e for e in write["events"] if e["event"] == "reader"]
        random_stage = stages.get("random", {})
        random_readers = [e for e in random_stage.get("events", []) if e["event"] == "reader"]
        config = case["config"]
        total = report["total_payload_bytes_per_case"]
        duration = write["elapsed_seconds"]
        update_seconds = (epochs[-1]["elapsed_seconds"] - epochs[len(epochs) // 2 - 1]["elapsed_seconds"]) if len(epochs) >= 2 else None
        files = {f["path"]: f for f in case.get("files", [])}
        sidecar_bytes = sum(f["logical_bytes"] for name, f in files.items() if name.endswith((".vki", ".vks")))
        disk = write.get("disk_delta", {}).get("vda", {})
        row = {
            "study": path.parent.name, "case": case["name"], "variant": config["name"],
            "status": case["status"], "instrumentation": report["instrumentation"],
            "payload_gib": total / GIB, "mode": config["mode"],
            "record_bytes": config.get("record", 65536), "batch_bytes": config.get("batch", 4 * MIB),
            "writer_readers": len(readers) if config["mode"] == "indexed" else config.get("readers", 0),
            "pinned_readers": config.get("pinned", 1) if readers else 0,
            "write_seconds": duration, "payload_mib_s": total / MIB / duration,
            "update_seconds": update_seconds,
            "update_mib_s": total / 2 / MIB / update_seconds if update_seconds else None,
            "guest_write_gib": write["child_write_bytes"] / GIB,
            "guest_read_gib_during_write": write["child_read_bytes"] / GIB,
            "write_amplification": write["child_write_bytes"] / total,
            "sidecar_mib": sidecar_bytes / MIB,
            "sidecar_allocated_mib": sum(f["allocated_bytes"] for name, f in files.items() if name.endswith((".vki", ".vks"))) / MIB,
            "native_allocated_gib": files.get("data.varve", {}).get("allocated_bytes", 0) / GIB,
            "user_seconds": write["user_seconds"], "system_seconds": write["system_seconds"],
            "cpu_cores_used": (write["user_seconds"] + write["system_seconds"]) / duration,
            "cgroup_throttled_seconds": write["cpu_stat_delta"].get("throttled_usec", 0) / 1e6,
            "peak_rss_mib": write["peak_rss_bytes"] / MIB if report["instrumentation"] == "none" else None,
            "peak_cgroup_gib_including_cache": write["peak_cgroup_memory_bytes"] / GIB,
            "vda_writes": disk.get("writes"),
            "vda_busy_fraction": disk.get("busy_ms", 0) / 1000 / duration,
            "vda_mean_write_ms": disk.get("write_ms", 0) / max(1, disk.get("writes", 0)),
            "vda_average_queue_depth": disk.get("weighted_ms", 0) / 1000 / duration,
            "append_seconds": sum(e["append_ns"] for e in epochs) / 1e9,
            "sync_seconds": sum(e["sync_ns"] for e in epochs) / 1e9,
            "mutation_seconds": sum(e["mutation_ns"] for e in epochs) / 1e9,
            "handoff_seconds": sum(e["handoff_ns"] for e in epochs) / 1e9,
            "concurrent_checked_lookups": sum(e["latency"]["count"] for e in readers),
            "concurrent_get_p99_upper_us": quantile(readers, 99),
            "random_qps": sum(e["latency"]["count"] for e in random_readers) / random_stage["elapsed_seconds"] if random_readers else None,
            "random_get_p50_upper_us": quantile(random_readers, 50),
            "random_get_p99_upper_us": quantile(random_readers, 99),
            "random_guest_read_gib": random_stage.get("child_read_bytes", 0) / GIB,
            "verify_seconds": stages.get("verify", {}).get("elapsed_seconds"),
            "verify_guest_read_gib": stages.get("verify", {}).get("child_read_bytes", 0) / GIB,
            "point_seconds": stages.get("point", {}).get("elapsed_seconds"),
            "delete_seconds": case.get("delete_seconds"),
            "free_gib_recovered": case.get("free_bytes_recovered", 0) / GIB,
            "report": str(path.resolve()),
        }
        rows.append(row)
    return rows


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("reports", type=Path, nargs="+")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    rows = [row for path in args.reports for row in summarize(path)]
    if not rows:
        parser.error("no executed write phases")
    args.output.mkdir(parents=True, exist_ok=True)
    with (args.output / "cases.csv").open("w", newline="") as file:
        writer = csv.DictWriter(file, fieldnames=list(rows[0]))
        writer.writeheader()
        writer.writerows(rows)
    groups = {}
    for row in rows:
        # Separate study sizes and instrumentation; never merge profiler timings
        # or different record/batch/reader configurations into normal throughput.
        key = (row["variant"], row["payload_gib"], row["instrumentation"])
        groups.setdefault(key, []).append(row)
    aggregates = []
    for (variant, size, instrument), values in groups.items():
        metrics = {}
        for field in rows[0]:
            samples = [row[field] for row in values if isinstance(row[field], (int, float))]
            if samples:
                metrics[field] = {"median": statistics.median(samples), "min": min(samples), "max": max(samples)}
        aggregates.append({"variant": variant, "payload_gib": size, "instrumentation": instrument,
                           "runs": len(values), "metrics": metrics})
    result = {"schema_version": 1, "rows": rows, "groups": aggregates,
              "completed_payload_gib": sum(r["payload_gib"] for r in rows if r["status"] in ("passed", "contract_failure")),
              "notes": ["Guest I/O accounting does not establish host media behavior.",
                        "Quantiles are merged logarithmic histogram upper bounds, not averaged percentiles.",
                        "First half is prefill; update_mib_s measures the concurrent second half.",
                        "Instrumented process RSS is omitted because the monitored PID is the profiler."]}
    (args.output / "summary.json").write_text(json.dumps(result, indent=2) + "\n")
    print(f"{len(rows)} cases; {result['completed_payload_gib']:.2f} GiB completed payload")


if __name__ == "__main__":
    main()
