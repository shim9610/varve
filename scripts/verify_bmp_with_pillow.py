#!/usr/bin/env python3
"""Cross-check Varve's custom physical layout API against Pillow BMP I/O."""

from __future__ import annotations

import argparse
import subprocess
import sys
import tempfile
from pathlib import Path

WIDTH = 3
HEIGHT = 2
PIXELS_TOP_DOWN = [
    (255, 0, 0),
    (0, 255, 0),
    (0, 0, 255),
    (0, 255, 255),
    (255, 0, 255),
    (255, 255, 0),
]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--varve-output",
        type=Path,
        default=None,
        help="Varve-authored BMP path; defaults to a temporary file.",
    )
    parser.add_argument(
        "--pillow-output",
        type=Path,
        default=None,
        help="Pillow-authored BMP path; defaults to a temporary file.",
    )
    args = parser.parse_args()

    try:
        from PIL import Image
    except ImportError:
        print(
            "Pillow is required. Install with: "
            "python -m pip install -r scripts/requirements-tdms-harness.txt",
            file=sys.stderr,
        )
        return 2

    tempdir = Path(tempfile.gettempdir())
    varve_output = (
        args.varve_output or tempdir / "varve-pillow-compat.bmp"
    ).resolve()
    pillow_output = (
        args.pillow_output or tempdir / "pillow-varve-compat.bmp"
    ).resolve()

    subprocess.run(
        [
            "cargo",
            "run",
            "-p",
            "varve",
            "--example",
            "bmp_physical",
            "--",
            "write",
            str(varve_output),
        ],
        check=True,
    )
    with Image.open(varve_output) as image:
        image = image.convert("RGB")
        assert image.size == (WIDTH, HEIGHT)
        flattened = (
            image.get_flattened_data()
            if hasattr(image, "get_flattened_data")
            else image.getdata()
        )
        assert list(flattened) == PIXELS_TOP_DOWN

    image = Image.new("RGB", (WIDTH, HEIGHT))
    image.putdata(PIXELS_TOP_DOWN)
    image.save(pillow_output, format="BMP")
    subprocess.run(
        [
            "cargo",
            "run",
            "-p",
            "varve",
            "--example",
            "bmp_physical",
            "--",
            "read",
            str(pillow_output),
        ],
        check=True,
    )

    print(f"Pillow verified Varve BMP {varve_output}")
    print(f"Varve verified Pillow BMP {pillow_output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
