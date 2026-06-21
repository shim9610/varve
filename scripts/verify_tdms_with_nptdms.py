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

    assert_varve_authored_tdms(TdmsFile.read(output), appended=False)

    subprocess.run(
        [
            "cargo",
            "run",
            "-p",
            "varve",
            "--example",
            "tdms_physical_adapter",
            "--",
            "append",
            str(output),
        ],
        check=True,
    )

    assert_varve_authored_tdms(TdmsFile.read(output), appended=True)

    subprocess.run(
        [
            "cargo",
            "run",
            "-p",
            "varve",
            "--example",
            "tdms_physical_adapter",
            "--",
            "read",
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

    print(f"npTDMS verified Varve create+append {output}")
    return 0


def assert_varve_authored_tdms(tdms, appended: bool) -> None:
    assert tdms.properties["title"] == "Varve TDMS adapter proof smoke"

    group = tdms["Measured Data"]
    amplitude = group["Amplitude"]
    assert amplitude.properties["unit_string"] == "V"
    assert math.isclose(amplitude.properties["wf_increment"], 0.001)
    assert amplitude.properties["adapter_enabled"] is True
    assert amplitude.properties["sample_count"] == (9 if appended else 8)
    assert amplitude.properties["segment_note"] == "same raw index reused"
    if appended:
        assert amplitude.properties["append_source"] == "varve-open-layout-writer"

    phase = group["Phase"]
    assert phase.properties["unit_string"] == "rad"
    assert math.isclose(phase.properties["wf_increment"], 0.001)
    assert phase.properties["sample_count"] == (9 if appended else 8)

    expected_amplitude = [0.10, 0.20, 0.30, 0.40, 0.50, 0.60, 0.70, 0.80]
    expected_phase = [1.00, 1.10, 1.20, 1.30, 1.40, 1.50, 1.60, 1.70]
    if appended:
        expected_amplitude.append(0.90)
        expected_phase.append(1.80)

    amplitude_values = [float(value) for value in amplitude[:]]
    assert amplitude_values == expected_amplitude
    phase_values = [float(value) for value in phase[:]]
    assert phase_values == expected_phase


if __name__ == "__main__":
    raise SystemExit(main())
