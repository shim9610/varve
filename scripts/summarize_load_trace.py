#!/usr/bin/env python3
"""Aggregate strace -ff -ttt -T -yy output by native/index/state file and syscall."""
import argparse
from collections import defaultdict
import json
from pathlib import Path
import re

CALL = re.compile(r"^\d+\.\d+\s+(\w+)\((.*)\)\s+=\s+(-?\d+).*<([\d.]+)>$")
RESUMED = re.compile(r"^\d+\.\d+\s+<\.\.\. (\w+) resumed>(.*)")


def category(line):
    for suffix, label in [("data.varve.vki", "index"), ("data.varve.vks", "state"),
                          ("data.varve.lock", "writer_marker"), ("data.varve", "native")]:
        if suffix in line:
            return label
    return "other"


def aggregate(directory):
    groups = defaultdict(lambda: {"calls": 0, "errors": 0, "returned_bytes": 0, "durations_us": []})
    pending_count = unmatched = 0
    for path in sorted(directory.glob("trace.*")):
        pending = None
        with path.open() as file:
            for line in file:
                line = line.strip()
                if "<unfinished ...>" in line:
                    pending = line.split("<unfinished ...>")[0]
                    continue
                resumed = RESUMED.match(line)
                if resumed:
                    if pending is None:
                        unmatched += 1
                        continue
                    line = pending + resumed[2]
                    pending = None
                match = CALL.match(line)
                if not match:
                    if "+++" not in line and "---" not in line:
                        unmatched += 1
                    continue
                syscall, arguments, returned, seconds = match.groups()
                result = int(returned)
                group = groups[(category(arguments), syscall)]
                group["calls"] += 1
                group["errors"] += int(result < 0)
                if syscall in ("read", "write", "pread64", "pwrite64"):
                    group["returned_bytes"] += max(0, result)
                group["durations_us"].append(float(seconds) * 1e6)
        pending_count += int(pending is not None)
    rows = []
    for (target, syscall), group in sorted(groups.items()):
        times = sorted(group.pop("durations_us"))
        rows.append({"target": target, "syscall": syscall, **group,
                     "summed_seconds": sum(times) / 1e6,
                     **{f"p{p}_us": times[min(len(times) - 1, (len(times) * p + 99) // 100 - 1)]
                        for p in (50, 95, 99)}, "max_us": times[-1]})
    return {"directory": str(directory.resolve()), "rows": rows,
            "unmatched_lines": unmatched, "unfinished_at_exit": pending_count,
            "notes": ["strace changes timings; use uninstrumented runs for throughput.",
                      "Syscall durations are summed per thread and can overlap.",
                      "Returned bytes are syscall bytes, not physical media bytes."]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directories", type=Path, nargs="+")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    reports = [aggregate(path) for path in args.directories]
    if not any(report["rows"] for report in reports):
        parser.error("no parseable trace files")
    args.output.write_text(json.dumps(reports, indent=2) + "\n")


if __name__ == "__main__":
    main()
