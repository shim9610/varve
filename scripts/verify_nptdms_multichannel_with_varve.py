#!/usr/bin/env python3
"""Parse and append an npTDMS-authored scalar type matrix with Varve."""

from __future__ import annotations

import argparse
import subprocess
import sys
import tempfile
from pathlib import Path

from verify_tdms_with_nptdms import assert_channel_matrix, changed_values, first_values


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

    output = args.output or Path(tempfile.gettempdir()) / "nptdms-varve-type-matrix.tdms"
    output = output.resolve()
    for stale in [output, output.with_suffix(".vtidx"), Path(str(output) + "_index")]:
        if stale.exists():
            stale.unlink()

    with TdmsWriter(str(output), version=4713) as writer:
        writer.write_segment(
            [
                RootObject(properties={"title": "npTDMS scalar type matrix smoke"}),
                GroupObject("Measured Data", properties={"operator": "Ada", "verified": True}),
                *channel_objects(np, first_values(include_unit=False)),
            ]
        )
        writer.write_segment(channel_objects(np, changed_values(include_unit=False)))

    tdms = TdmsFile.read(output, raw_timestamps=True)
    assert tdms.properties["title"] == "npTDMS scalar type matrix smoke"
    assert tdms["Measured Data"].properties["operator"] == "Ada"
    assert tdms["Measured Data"].properties["verified"] is True
    assert_channel_matrix(tdms["Measured Data"], include_unit=False, include_same=False, appended=False)

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
            "append",
            str(output),
        ],
        check=True,
    )

    appended = TdmsFile.read(output, raw_timestamps=True)
    assert appended["Measured Data"]["Float64"].properties["append_source"] == (
        "varve-open-layout-writer"
    )
    assert_channel_matrix(
        appended["Measured Data"],
        include_unit=False,
        include_same=False,
        appended=True,
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

    print(f"Varve example parsed and appended npTDMS scalar type matrix {output}")
    return 0


def channel_objects(np, values_by_name: dict[str, list]):
    return [
        channel_object(np, name, values)
        for name, values in values_by_name.items()
    ]


def channel_object(np, name: str, values: list):
    from nptdms import ChannelObject

    return ChannelObject(
        "Measured Data",
        name,
        channel_data(np, name, values),
        properties={"unit_string": name},
    )


def channel_data(np, name: str, values: list):
    if name == "Int8":
        return np.array(values, dtype=np.int8)
    if name == "Int16":
        return np.array(values, dtype=np.int16)
    if name == "Int32":
        return np.array(values, dtype=np.int32)
    if name == "Int64":
        return np.array(values, dtype=np.int64)
    if name == "Uint8":
        return np.array(values, dtype=np.uint8)
    if name == "Uint16":
        return np.array(values, dtype=np.uint16)
    if name == "Uint32":
        return np.array(values, dtype=np.uint32)
    if name == "Uint64":
        return np.array(values, dtype=np.uint64)
    if name == "Float32":
        return np.array(values, dtype=np.float32)
    if name == "Float64":
        return np.array(values, dtype=np.float64)
    if name == "Boolean":
        return np.array(values, dtype=np.bool_)
    if name == "String":
        return np.array(values)
    if name == "Timestamp":
        return np.array([tdms_timestamp_to_datetime64(np, value) for value in values])
    if name == "Complex64":
        return np.array([complex(real, imaginary) for real, imaginary in values], dtype=np.complex64)
    if name == "Complex128":
        return np.array([complex(real, imaginary) for real, imaginary in values], dtype=np.complex128)
    raise AssertionError(f"npTDMS writer does not author {name} in this harness")


def tdms_timestamp_to_datetime64(np, value: tuple[int, int]):
    fractions, seconds = value
    micros = int(round((fractions / 2**64) * 1_000_000))
    return (
        np.datetime64("1904-01-01T00:00:00.000000", "us")
        + np.timedelta64(seconds, "s")
        + np.timedelta64(micros, "us")
    )


if __name__ == "__main__":
    raise SystemExit(main())
