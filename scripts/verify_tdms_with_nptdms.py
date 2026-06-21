#!/usr/bin/env python3
"""Generate a Varve-authored TDMS file and verify it with npTDMS."""

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
                "tdms_physical_writer",
                "--",
                str(output),
            ],
            check=True,
        )

    tdms = TdmsFile.read(output)
    assert tdms.properties["title"] == "Varve TDMS compatibility smoke"

    group = tdms["Measured Data"]
    channel = group["Amplitude"]
    assert channel.properties["unit_string"] == "V"
    assert math.isclose(channel.properties["wf_increment"], 0.001)
    assert channel.properties["sample_count"] == 6

    values = [float(value) for value in channel[:]]
    assert values == [0.10, 0.20, 0.30, 0.40, 0.50, 0.60]

    print(f"npTDMS verified {output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
