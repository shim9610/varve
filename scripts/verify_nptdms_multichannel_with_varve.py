#!/usr/bin/env python3
"""Parse an npTDMS-authored file with a Varve-based example adapter."""

from __future__ import annotations

import argparse
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
    args = parser.parse_args()

    try:
        import numpy as np
        from nptdms import ChannelObject, GroupObject, RootObject, TdmsFile, TdmsWriter
    except ImportError:
        print(
            "npTDMS and NumPy are required. Install with: "
            "python -m pip install -r scripts/requirements-tdms-harness.txt",
            file=sys.stderr,
        )
        return 2

    output = args.output or Path(tempfile.gettempdir()) / "nptdms-varve-multichannel.tdms"
    output = output.resolve()
    if output.exists():
        output.unlink()

    with TdmsWriter(str(output)) as writer:
        writer.write_segment(
            [
                RootObject(properties={"title": "npTDMS multichannel smoke"}),
                GroupObject("Bench", properties={"operator": "Ada"}),
                ChannelObject(
                    "Bench",
                    "Voltage",
                    np.array([1.0, 2.0, 3.0], dtype=np.float64),
                    properties={"unit_string": "V"},
                ),
                ChannelObject(
                    "Bench",
                    "Current",
                    np.array([0.10, 0.20, 0.30], dtype=np.float64),
                    properties={"unit_string": "A"},
                ),
            ]
        )
        writer.write_segment(
            [
                ChannelObject(
                    "Bench",
                    "Voltage",
                    np.array([4.0], dtype=np.float64),
                ),
                ChannelObject(
                    "Bench",
                    "Current",
                    np.array([0.40], dtype=np.float64),
                ),
            ]
        )

    tdms = TdmsFile.read(output)
    assert tdms.properties["title"] == "npTDMS multichannel smoke"
    assert tdms["Bench"].properties["operator"] == "Ada"
    assert [float(value) for value in tdms["Bench"]["Voltage"][:]] == [1.0, 2.0, 3.0, 4.0]
    assert [float(value) for value in tdms["Bench"]["Current"][:]] == [0.10, 0.20, 0.30, 0.40]

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
            "read-bytes",
            str(output),
        ],
        check=True,
    )

    print(f"Varve example parsed npTDMS multichannel file {output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
