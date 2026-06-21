#!/usr/bin/env python3
"""Verify a Varve-based TDMS example file against npTDMS."""

from __future__ import annotations

import argparse
import math
import subprocess
import sys
import tempfile
from pathlib import Path


CHANNEL_TYPES = {
    "Int8": "Int8",
    "Int16": "Int16",
    "Int32": "Int32",
    "Int64": "Int64",
    "Uint8": "Uint8",
    "Uint16": "Uint16",
    "Uint32": "Uint32",
    "Uint64": "Uint64",
    "Float32": "SingleFloat",
    "Float64": "DoubleFloat",
    "Float32Unit": "SingleFloatWithUnit",
    "Float64Unit": "DoubleFloatWithUnit",
    "Boolean": "Boolean",
    "String": "String",
    "Timestamp": "TimeStamp",
    "Complex64": "ComplexSingleFloat",
    "Complex128": "ComplexDoubleFloat",
}


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

    assert_varve_authored_tdms(TdmsFile.read(output, raw_timestamps=True), appended=False)

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

    assert_varve_authored_tdms(TdmsFile.read(output, raw_timestamps=True), appended=True)

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
    assert group.properties["operator"] == "Varve"
    assert group.properties["verified"] is True

    float64 = group["Float64"]
    assert float64.properties["unit_string"] == "Float64"
    assert math.isclose(float64.properties["wf_increment"], 0.001)
    if appended:
        assert float64.properties["append_source"] == "varve-open-layout-writer"

    boolean = group["Boolean"]
    assert boolean.properties["adapter_enabled"] is True

    string = group["String"]
    assert string.properties["sample_count"] == (5 if appended else 4)
    assert string.properties["segment_note"] == "same raw index reused"

    assert_channel_matrix(group, include_unit=True, include_same=True, appended=appended)


def assert_channel_matrix(group, include_unit: bool, include_same: bool, appended: bool) -> None:
    expected = expected_values(include_unit=include_unit, include_same=include_same, appended=appended)
    assert sorted(channel.name for channel in group.channels()) == sorted(expected)
    for name, values in expected.items():
        channel = group[name]
        assert channel.data_type.__name__ == CHANNEL_TYPES[name]
        assert channel.properties["unit_string"] == name
        assert_values_equal(name, channel[:], values)


def expected_values(include_unit: bool, include_same: bool, appended: bool) -> dict[str, list]:
    values = first_values(include_unit)
    merge_values(values, changed_values(include_unit))
    if include_same:
        merge_values(values, same_index_values(include_unit))
    if appended:
        merge_values(values, append_values(include_unit))
    return values


def merge_values(current: dict[str, list], next_values: dict[str, list]) -> None:
    assert current.keys() == next_values.keys()
    for name, values in next_values.items():
        current[name].extend(values)


def first_values(include_unit: bool) -> dict[str, list]:
    values = {
        "Int8": [-1, 2],
        "Int16": [-300, 400],
        "Int32": [-70000, 80000],
        "Int64": [-9000000000, 10000000000],
        "Uint8": [1, 250],
        "Uint16": [500, 60000],
        "Uint32": [70000, 4000000000],
        "Uint64": [9000000000, 18000000000000000000],
        "Float32": [1.25, -2.5],
        "Float64": [3.5, -4.75],
        "Boolean": [True, False],
        "String": ["red", "blue"],
        "Timestamp": [
            (0, 3660681600),
            (9223372036854775808, 3660681600),
        ],
        "Complex64": [(1.0, 2.0), (-3.0, 4.0)],
        "Complex128": [(5.0, 6.0), (-7.0, 8.0)],
    }
    if include_unit:
        values["Float32Unit"] = [10.25, -20.5]
        values["Float64Unit"] = [30.5, -40.75]
    return values


def changed_values(include_unit: bool) -> dict[str, list]:
    values = {
        "Int8": [3],
        "Int16": [-500],
        "Int32": [90000],
        "Int64": [11000000000],
        "Uint8": [42],
        "Uint16": [1234],
        "Uint32": [5000000],
        "Uint64": [17000000000000000000],
        "Float32": [3.75],
        "Float64": [6.25],
        "Boolean": [True],
        "String": ["cyan"],
        "Timestamp": [(0, 3660681601)],
        "Complex64": [(5.0, -6.0)],
        "Complex128": [(9.0, -10.0)],
    }
    if include_unit:
        values["Float32Unit"] = [50.25]
        values["Float64Unit"] = [60.125]
    return values


def same_index_values(include_unit: bool) -> dict[str, list]:
    values = {
        "Int8": [-4],
        "Int16": [600],
        "Int32": [-100000],
        "Int64": [-12000000000],
        "Uint8": [7],
        "Uint16": [4321],
        "Uint32": [6000000],
        "Uint64": [16000000000000000000],
        "Float32": [-4.5],
        "Float64": [-7.5],
        "Boolean": [False],
        "String": ["gold"],
        "Timestamp": [(4611686018427387904, 3660681601)],
        "Complex64": [(-7.0, -8.0)],
        "Complex128": [(-11.0, -12.0)],
    }
    if include_unit:
        values["Float32Unit"] = [-70.5]
        values["Float64Unit"] = [-80.875]
    return values


def append_values(include_unit: bool) -> dict[str, list]:
    values = {
        "Int8": [5],
        "Int16": [-700],
        "Int32": [110000],
        "Int64": [13000000000],
        "Uint8": [8],
        "Uint16": [5432],
        "Uint32": [7000000],
        "Uint64": [15000000000000000000],
        "Float32": [5.5],
        "Float64": [8.5],
        "Boolean": [True],
        "String": ["navy"],
        "Timestamp": [(0, 3660681602)],
        "Complex64": [(9.0, 10.0)],
        "Complex128": [(13.0, 14.0)],
    }
    if include_unit:
        values["Float32Unit"] = [90.5]
        values["Float64Unit"] = [100.625]
    return values


def assert_values_equal(name: str, actual, expected: list) -> None:
    if name == "Timestamp":
        actual_values = [
            (int(fractions), int(seconds))
            for fractions, seconds in zip(actual["second_fractions"], actual["seconds"])
        ]
    elif name.startswith("Complex"):
        actual_values = [(float(value.real), float(value.imag)) for value in actual]
    elif name.startswith("Float"):
        actual_values = [float(value) for value in actual]
    elif name == "Boolean":
        actual_values = [bool(value) for value in actual]
    elif name == "String":
        actual_values = [str(value) for value in actual]
    else:
        actual_values = [int(value) for value in actual]

    if name.startswith("Float") or name.startswith("Complex"):
        assert_floatish_lists_equal(actual_values, expected)
    else:
        assert actual_values == expected


def assert_floatish_lists_equal(actual: list, expected: list) -> None:
    assert len(actual) == len(expected)
    for left, right in zip(actual, expected):
        if isinstance(right, tuple):
            assert math.isclose(left[0], right[0], rel_tol=0.0, abs_tol=1e-6)
            assert math.isclose(left[1], right[1], rel_tol=0.0, abs_tol=1e-6)
        else:
            assert math.isclose(left, right, rel_tol=0.0, abs_tol=1e-6)


if __name__ == "__main__":
    raise SystemExit(main())
