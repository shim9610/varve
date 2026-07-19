# Read And Allocation Safety

Varve does not use an unknown-length read as a parsing primitive. Production
read paths follow this order:

1. Read a fixed-size header or inspect a caller-provided bounded slice.
2. Decode lengths, counts, and offsets with checked arithmetic.
3. Validate the complete range against the captured file snapshot or enclosing
   slice.
4. Apply the resolved runtime limit for that resource.
5. Convert to `usize`, reserve fallibly, and only then read exactly that range.

EOF is not a framing mechanism. Append logs stop at the captured snapshot
length, sidecars must exactly match their declared total length, and sidecar
identity hashing consumes the extent captured from the already-open handle.
Declared ranges whose `offset + length` overflows `u64`, or whose end exceeds
the captured snapshot, are rejected before size conversion, reservation,
allocation, decompression, slicing, or mmap creation. A backing file truncated
after open is also rejected before mmap, including when the record index is
empty.

## Coverage

| Surface | Size source and boundary |
| --- | --- |
| Native records | fixed header, checked payload/footer extent, captured snapshot, physical/logical payload limits |
| Variable fields | enclosing payload slice, field header extent check, shared decoder materialization budget |
| Variable field-id bookkeeping | 8 bytes charged to the materialization budget per distinct field id above 63, before the set reserves; duplicates are detected first and charged nothing |
| Compression | declared logical size checked before decompression; default whole-value and chunk decoding are finite |
| Custom layouts | compile-time lead-in/footer widths; derived segment ranges checked against the snapshot and scan limits |
| Layout payload reads | declared metadata/raw ranges plus physical and materialized-byte limits |
| Matrix metadata | fixed matrix header, exact descriptor-derived table extents, matrix metadata/bitmap/CRC limits |
| Matrix commit/validity bitmaps | 4 KiB pages materialized only when they carry a set bit; an absent page is provably zero and is answered without I/O or allocation. The `matrix_bitmap` budget charges each page as it is materialized, before the memory is used |
| Matrix cells and aux | schema-derived stride or caller length, validated range, payload and materialization limits |
| Matrix CRC/zero scans | schema-derived exact range processed through a fixed 64 KiB stack buffer |
| Sidecars | fixed header, checked exact total extent, sidecar/materialization limits, and a finite 256 MiB default identity-scan ceiling |
| mmap | captured mapping length and every returned window checked against the mapped snapshot |
| Adapter cursor | caller-provided slice bounds and one cumulative finite materialization budget for all owned results |

Hostile mutation tests cover native record payload lengths, checkpoint index
extents, every VMAT `u64` extent field, custom-layout segment offsets and
lengths, sidecar range arithmetic, and backing-file truncation before mmap.
These tests wrap opens and reads with `catch_unwind`: success means a typed
error was returned without a panic, not merely that corrupt input was noticed.

The standard policy leaves append-growth totals uncapped so a valid log can
keep growing, but retains finite one-shot limits. Applications can set limits
at each open/create operation with `ResourceLimits`. APIs named
`*_trusted_unbounded` are explicit policy overrides for already trusted input;
they still parse known, checked extents rather than reading until EOF.

Low-level `RecordIndexEntry` path helpers are not snapshot identity APIs.
Their default forms are finite, and their `*_limited` forms require an explicit
caller ceiling. Prefer generated typed readers for ordinary untrusted input.
