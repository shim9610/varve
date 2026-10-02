#!/usr/bin/env python3
"""Timed matrix generation campaign, Linux resource monitor and owned cleanup."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil

from load_scalable import ROOT, atomic_json, execute
from qualify_scalable import source_identity


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--seconds", type=int, default=1800)
    parser.add_argument("--readers", type=int, default=8)
    parser.add_argument("--rows", type=int, default=65536)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--data-root", type=Path, required=True)
    args = parser.parse_args()
    if args.seconds <= 0 or not 0 <= args.readers <= 64 or args.rows <= 0 or args.rows % 256:
        parser.error("positive seconds/rows, rows divisible by 256, readers 0..64 required")
    output, data = args.output.resolve(), args.data_root.resolve()
    if output == data or data in output.parents or output in data.parents:
        parser.error("output and data directories must be separate")
    if output.exists() or data.exists():
        parser.error("output and data directories must be new")
    # Live generation, replacement, pinned old descriptors and append history.
    if shutil.disk_usage(data.parent).free < args.rows * 4096 * 8 + 1024**3:
        parser.error("insufficient disk reserve for live/pinned/replacement generations")
    binary = ROOT / "target/release/examples/matrix_soak"
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    identity = source_identity()
    output.mkdir(parents=True)
    data.mkdir()
    marker = data / ".matrix-soak-owner"
    marker.write_text(str(output))
    report = {"source": identity, "binary_sha256": digest, "config": vars(args).copy(),
              "status": "running", "scope": "single writer; independent pinned/following/fresh readers; CRC; sync; flush; reopen; explicit compaction"}
    report["config"] = {k: str(v) if isinstance(v, Path) else v for k, v in report["config"].items()}
    atomic_json(output / "report.json", report)
    try:
        result = execute([str(binary), str(data / "matrix.varve"), str(args.seconds),
                          str(args.readers), str(args.rows)], output, "matrix", os.environ.copy(),
                         args.seconds + 600, "soak_complete", data)
        report["result"] = result
        report["files"] = [{"name": p.name, "logical_bytes": p.stat().st_size,
                            "allocated_bytes": p.stat().st_blocks * 512}
                           for p in data.iterdir() if p.is_file() and p != marker]
        report["source_after"] = source_identity()
        if identity != report["source_after"] or digest != hashlib.sha256(binary.read_bytes()).hexdigest():
            raise RuntimeError("source or binary changed during run")
        if result["status"] != "passed":
            raise RuntimeError("matrix campaign failed; data retained")
        completed = next(e for e in result["events"] if e.get("event") == "soak_complete")
        if completed["elapsed_seconds"] < args.seconds:
            raise RuntimeError("campaign ended before requested duration")
        if marker.read_text() != str(output) or data.is_symlink():
            raise RuntimeError("cleanup ownership check failed")
        shutil.rmtree(data)
        report["cleanup_verified"] = not data.exists()
        report["status"] = "passed"
    except (Exception, KeyboardInterrupt) as error:
        report["status"] = "failed"
        report["error"] = repr(error)
    atomic_json(output / "report.json", report)
    print(json.dumps({"status": report["status"], "report": str(output / "report.json")}))
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
