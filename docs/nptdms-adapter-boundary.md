# npTDMS Adapter Boundary

This document checks the npTDMS 1.10.0 public documentation against Varve's
generic APIs. It does not define TDMS as a Varve built-in feature. TDMS reader
and writer code in this repository is example/proof code showing that an
external adapter can be built on top of Varve's physical layout primitives.

Sources checked:

- https://nptdms.readthedocs.io/en/stable/apireference.html
- https://nptdms.readthedocs.io/en/stable/reading.html
- https://nptdms.readthedocs.io/en/stable/writing.html
- https://nptdms.readthedocs.io/en/stable/limitations.html

## Responsibility Rule

Varve is responsible for generic physical-layout capabilities:

- declaring file headers, segment lead-ins, metadata regions, raw regions, and
  footers;
- validating literal fields, caller fields, finalized offsets, and segment
  bounds;
- appending complete segments through buffered or streamed writes;
- exposing segment offsets, lengths, field values, metadata bytes, raw bytes,
  bounded metadata/raw ranges, and tolerant scan reports.

An external TDMS adapter is responsible for TDMS semantics:

- TDMS object paths, root/group/channel state, raw-data-index grammar, and
  same-as-previous object metadata;
- TDMS property value typing, timestamp conversion, waveform time-track
  calculation, scaling, DAQmx raw scaler interpretation, and interleaving;
- DataFrame/HDF export, `.tdms_index` sidecar policy, defragment/copy policy,
  and any public API that mimics npTDMS.

If Varve strict layout open succeeds and the adapter can read the required
metadata/raw ranges, failures in object assembly, typing, scaling, timestamps,
or exports are adapter issues. If a complete external file cannot be expressed
with Varve layout declarations, or if Varve rejects a file before the adapter can
report a documented physical status, that is a Varve generic API issue.

## Feature Matrix

| npTDMS documented feature | Varve generic API surface | Boundary |
| --- | --- | --- |
| `TdmsFile.read`, `open`, `read_metadata` | `open_layout_reader`, `inspect_layout_file`, `inspect_layout_file_report`, `segments`, `read_metadata`, `read_raw`, range readers | Varve validates and exposes physical bytes. The adapter decides eager, lazy, or metadata-only behavior. |
| Path-like and file-like inputs | Path-based layout APIs | The adapter public API may accept file objects by materializing them to an owned path or by providing its own transport layer. Varve core keeps path ownership for locks, snapshots, mmap, and range reads. |
| Group/channel dictionaries, names, paths, properties | Opaque metadata bytes plus optional Varve native keyed blocks for adapter-owned models | Caller-owned TDMS metadata parser/state. |
| Channel indexing, slices, `read_data(offset, length, scaled)` | `LayoutSegmentInfo` absolute offsets/lengths, `read_raw_range`, generated `read_*_raw_range` | Varve exposes byte windows. The adapter maps TDMS channel value indexes to byte ranges and applies scaling. |
| `data_chunks()` and per-channel chunk streaming | Segment enumeration, raw range reads, and `LayoutScanReport` complete-prefix evidence | Chunk grouping and channel offsets are adapter-owned. |
| Large-file workflows and memmap-style access | `LayoutReader::path`, public raw offsets/lengths, bounded range reads, streamed segment writes | Caller may mmap by path/range. Varve native mmap APIs are not TDMS-specific and custom-layout mmap helpers are not required for adapter correctness. |
| `file_status.incomplete_final_segment` | `inspect_layout_file_report` returns complete segments plus `LayoutTailInfo` for truncated/invalid tails | Varve reports the physical tail. The adapter computes per-channel expected/read values from TDMS metadata. |
| TDMS versions `4712` and `4713` | Caller-declared `u32 version` or equivalent lead-in field | Adapter chooses and validates version semantics. |
| `TdmsWriter.write_segment`, append mode | `create_layout_writer`, `open_layout_writer`, `write_segment`, `write_segment_streamed`, generated `write_*` and `write_*_streamed` | Varve writes/backpatches physical segments. Adapter creates TDMS metadata and raw payloads. |
| RootObject, GroupObject, ChannelObject write model | Caller fields plus opaque metadata/raw bytes | Adapter-owned object-to-metadata encoder. |
| `.tdms_index` sidecar | Separate custom layout declaration using `TDSh` lead-in, or caller-owned sidecar writer using the same metadata bytes | Varve does not provide a TDMS index policy; it provides generic physical layout writers. |
| `defragment` / copy selected channels | Read one external file through adapter, write another through Varve layout writer | Caller-owned transformation policy. |
| Property types: signed/unsigned ints, floats, strings, bools, timestamps | Opaque metadata bytes; native Varve examples can model these as blocks when desired | TDMS property wire grammar is adapter-owned. |
| Timestamp precision and `time_track` | Opaque property bytes and adapter-owned typed model | Caller converts TDMS seconds/fractions and waveform properties. |
| Scaled data and DAQmx raw scaler data | Opaque metadata/raw bytes, byte ranges, complete-prefix/tail report | Scaling and DAQmx interpretation are adapter-owned. |
| DataFrame/HDF conversion | No Varve-specific dependency required | Caller exports materialized data using its own dependencies. |
| `tdmsinfo`-style listing/debug output | `inspect_layout_file_report` and adapter metadata parser | Caller-owned CLI/UI behavior. |
| npTDMS limitations: no TDM/XML headers and no extended precision floats | No requirement for npTDMS parity | Varve may still host other external formats through custom layouts. |

## Current Proofs

- `scripts/verify_tdms_with_nptdms.py` checks a Varve-authored TDMS-style
  example file with npTDMS.
- `scripts/verify_nptdms_multichannel_with_varve.py` checks an npTDMS-authored
  two-channel, two-segment file with a Varve-based example parser.
- `scripts/verify_bmp_with_pillow.py` checks a non-TDMS custom physical layout
  against Pillow in both directions.
- `crates/varve/tests/layout_dsl.rs` covers strict custom-layout parsing,
  bounded range reads, streamed segment writes, and tolerant scan reports for a
  truncated custom-layout tail.

These tests are proof points for generic API sufficiency, not a promise that
Varve ships a production TDMS adapter.
