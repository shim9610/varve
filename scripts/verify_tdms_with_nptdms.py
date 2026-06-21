#!/usr/bin/env python3
"""Verify a Varve-based TDMS example file against npTDMS."""

from __future__ import annotations

import argparse
import math
import subprocess
import sys
import tempfile
from pathlib import Path


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--output",
        type=Path,
        default=None,
        help="TDMS output path; defaults to a temporary file.",
    )
    parser.add_argument(
        "--no-cargo",
        action="store_true",
        help="Skip cargo run and verify an existing --output file.",
    )
    args = parser.parse_args()

    try:
        from nptdms import TdmsFile
    except ImportError:
        print(
            "npTDMS is required. Install it with: python -m pip install nptdms",
            file=sys.stderr,
        )
        return 2

    output = args.output or Path(tempfile.gettempdir()) / "varve-nptdms-harness.tdms"
    output = output.resolve()

    if not args.no_cargo:
        subprocess.run(
            [
                "cargo",
                "run",
                "-p",
                "varve",
                "--example",
                "tdms_physical_adapter",
                "--",
                "write",
                str(output),
            ],
            check=True,
        )

    tdms = TdmsFile.read(output)
    assert tdms.properties["title"] == "Varve TDMS adapter proof smoke"

    group = tdms["Measured Data"]
    amplitude = group["Amplitude"]
    assert amplitude.properties["unit_string"] == "V"
    assert math.isclose(amplitude.properties["wf_increment"], 0.001)
    assert amplitude.properties["adapter_enabled"] is True
    assert amplitude.properties["sample_count"] == 8
    assert amplitude.properties["segment_note"] == "same raw index reused"

    phase = group["Phase"]
    assert phase.properties["unit_string"] == "rad"
    assert math.isclose(phase.properties["wf_increment"], 0.001)
    assert phase.properties["sample_count"] == 8

    amplitude_values = [float(value) for value in amplitude[:]]
    assert amplitude_values == [0.10, 0.20, 0.30, 0.40, 0.50, 0.60, 0.70, 0.80]

    phase_values = [float(value) for value in phase[:]]
    assert phase_values == [1.00, 1.10, 1.20, 1.30, 1.40, 1.50, 1.60, 1.70]

    subprocess.run(
        [
            "cargo",
            "run",
            "-p",
            "varve",
            "--example",
            "tdms_physical_adapter",
            "--",
            "inspect",
            str(output),
        ],
        check=True,
    )
    subprocess.run(
        [
            "cargo",
            "run",
            "-p",
            "varve",
            "--example",
            "tdms_physical_adapter",
            "--",
            "read-bytes",
            str(output),
        ],
        check=True,
    )

    print(f"npTDMS verified {output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
