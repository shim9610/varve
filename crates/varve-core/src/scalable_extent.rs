#[cfg(any(feature = "high-cardinality-dev", test))]
use crate::native_layout::{native_record_footer_len, native_record_header_len};
use crate::{Error, Result};

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct FileOffset(u64);

impl FileOffset {
    pub(crate) const ZERO: Self = Self(0);

    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    pub(crate) const fn get(self) -> u64 {
        self.0
    }

    pub(crate) fn checked_add(self, len: ByteLength) -> Result<Self> {
        self.checked_add_for(len, "file offset")
    }

    fn checked_add_for(self, len: ByteLength, resource: &'static str) -> Result<Self> {
        self.0
            .checked_add(len.0)
            .map(Self)
            .ok_or(Error::ResourceArithmeticOverflow { resource })
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ByteLength(u64);

impl ByteLength {
    #[cfg(any(feature = "high-cardinality-dev", test))]
    pub(crate) const ZERO: Self = Self(0);

    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    pub(crate) const fn get(self) -> u64 {
        self.0
    }

    #[cfg(any(feature = "high-cardinality-dev", test))]
    pub(crate) fn checked_add(self, other: Self) -> Result<Self> {
        self.checked_add_for(other, "byte length")
    }

    pub(crate) fn try_usize(self) -> Result<usize> {
        usize::try_from(self.0).map_err(|_| Error::LengthOverflow { value: self.0 })
    }

    #[cfg(any(feature = "high-cardinality-dev", test))]
    fn checked_add_for(self, other: Self, resource: &'static str) -> Result<Self> {
        self.0
            .checked_add(other.0)
            .map(Self)
            .ok_or(Error::ResourceArithmeticOverflow { resource })
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(crate) struct SnapshotBounds {
    logical_len: ByteLength,
}

impl SnapshotBounds {
    pub(crate) const fn new(logical_len: ByteLength) -> Self {
        Self { logical_len }
    }

    pub(crate) const fn logical_len(self) -> ByteLength {
        self.logical_len
    }

    pub(crate) fn validate_range(self, offset: FileOffset, len: ByteLength) -> Result<FileOffset> {
        let end = offset.checked_add_for(len, "snapshot range")?;
        if end.0 > self.logical_len.0 {
            return Err(Error::SnapshotRangeOutOfBounds {
                offset: offset.0,
                len: len.0,
                snapshot_len: self.logical_len.0,
            });
        }
        Ok(end)
    }

    #[cfg(any(feature = "high-cardinality-dev", test))]
    pub(crate) fn validate_native_record(
        self,
        pointer: UntrustedRecordPointer,
        has_footer: bool,
    ) -> Result<ValidatedRecordPointer> {
        self.validate(
            pointer,
            ByteLength::new(native_record_header_len()),
            ByteLength::new(if has_footer {
                native_record_footer_len()
            } else {
                0
            }),
        )
    }

    #[cfg(any(feature = "high-cardinality-dev", test))]
    fn validate(
        self,
        pointer: UntrustedRecordPointer,
        header_len: ByteLength,
        footer_len: ByteLength,
    ) -> Result<ValidatedRecordPointer> {
        let span = RecordSpan::from_physical_len(
            self,
            FileOffset::new(pointer.record_offset),
            ByteLength::new(pointer.physical_len),
            header_len,
            footer_len,
        )?;

        // Point reads materialize each component. Prove those conversions before
        // any positional read or allocation can observe this pointer.
        span.header_len.try_usize()?;
        span.payload_len.try_usize()?;
        span.footer_len.try_usize()?;

        Ok(ValidatedRecordPointer { span })
    }
}

#[cfg(any(feature = "high-cardinality-dev", test))]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(crate) struct RecordSpan {
    record_offset: FileOffset,
    header_len: ByteLength,
    payload_offset: FileOffset,
    payload_len: ByteLength,
    footer_offset: FileOffset,
    footer_len: ByteLength,
    end: FileOffset,
}

#[cfg(any(feature = "high-cardinality-dev", test))]
impl RecordSpan {
    fn from_physical_len(
        bounds: SnapshotBounds,
        record_offset: FileOffset,
        physical_len: ByteLength,
        header_len: ByteLength,
        footer_len: ByteLength,
    ) -> Result<Self> {
        if header_len == ByteLength::ZERO {
            return Err(invalid_record_pointer());
        }
        let minimum_len = header_len.checked_add(footer_len)?;
        if physical_len < minimum_len {
            return Err(invalid_record_pointer());
        }

        let payload_len = ByteLength::new(physical_len.0 - minimum_len.0);
        let payload_offset = record_offset.checked_add_for(header_len, "record header extent")?;
        let footer_offset = payload_offset.checked_add_for(payload_len, "record payload extent")?;

        Self::from_parts(
            bounds,
            record_offset,
            header_len,
            payload_offset,
            payload_len,
            footer_offset,
            footer_len,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn from_parts(
        bounds: SnapshotBounds,
        record_offset: FileOffset,
        header_len: ByteLength,
        payload_offset: FileOffset,
        payload_len: ByteLength,
        footer_offset: FileOffset,
        footer_len: ByteLength,
    ) -> Result<Self> {
        if header_len == ByteLength::ZERO {
            return Err(invalid_record_pointer());
        }

        let expected_payload_offset =
            record_offset.checked_add_for(header_len, "record header extent")?;
        if payload_offset != expected_payload_offset {
            return Err(invalid_record_pointer());
        }

        let expected_footer_offset =
            payload_offset.checked_add_for(payload_len, "record payload extent")?;
        if footer_offset != expected_footer_offset {
            return Err(invalid_record_pointer());
        }

        let end = footer_offset.checked_add_for(footer_len, "record footer extent")?;
        let physical_len = ByteLength::new(
            end.0
                .checked_sub(record_offset.0)
                .ok_or_else(invalid_record_pointer)?,
        );
        let validated_end = bounds.validate_range(record_offset, physical_len)?;
        debug_assert_eq!(validated_end, end);

        Ok(Self {
            record_offset,
            header_len,
            payload_offset,
            payload_len,
            footer_offset,
            footer_len,
            end,
        })
    }

    pub(crate) const fn record_offset(self) -> FileOffset {
        self.record_offset
    }

    #[cfg(test)]
    pub(crate) const fn header_len(self) -> ByteLength {
        self.header_len
    }

    pub(crate) const fn payload_offset(self) -> FileOffset {
        self.payload_offset
    }

    pub(crate) const fn payload_len(self) -> ByteLength {
        self.payload_len
    }

    pub(crate) const fn footer_offset(self) -> FileOffset {
        self.footer_offset
    }

    #[cfg(test)]
    pub(crate) const fn footer_len(self) -> ByteLength {
        self.footer_len
    }

    pub(crate) const fn end(self) -> FileOffset {
        self.end
    }

    #[cfg(test)]
    pub(crate) const fn physical_len(self) -> ByteLength {
        ByteLength(self.end.0 - self.record_offset.0)
    }
}

#[cfg(any(feature = "high-cardinality-dev", test))]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(crate) struct UntrustedRecordPointer {
    record_offset: u64,
    physical_len: u64,
}

#[cfg(any(feature = "high-cardinality-dev", test))]
impl UntrustedRecordPointer {
    pub(crate) const fn new(record_offset: u64, physical_len: u64) -> Self {
        Self {
            record_offset,
            physical_len,
        }
    }
}

#[cfg(any(feature = "high-cardinality-dev", test))]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(crate) struct ValidatedRecordPointer {
    span: RecordSpan,
}

#[cfg(any(feature = "high-cardinality-dev", test))]
impl ValidatedRecordPointer {
    pub(crate) const fn span(self) -> RecordSpan {
        self.span
    }
}

#[cfg(any(feature = "high-cardinality-dev", test))]
fn invalid_record_pointer() -> Error {
    Error::InvalidIndexCheckpoint
}

#[cfg(test)]
mod tests {
    use super::*;

    const PIB: u64 = 1 << 50;
    const HEADER_LEN: ByteLength = ByteLength::new(32);
    const FOOTER_LEN: ByteLength = ByteLength::new(32);

    #[test]
    fn one_pib_offsets_and_bounds_remain_exact() {
        let record_len = ByteLength::new(80);
        let bounds = SnapshotBounds::new(ByteLength::new(PIB + record_len.get()));
        let pointer = UntrustedRecordPointer::new(PIB, record_len.get());
        let validated = bounds.validate(pointer, HEADER_LEN, FOOTER_LEN).unwrap();
        let span = validated.span();

        assert_eq!(bounds.logical_len().get(), PIB + 80);
        assert_eq!(span.record_offset().get(), PIB);
        assert_eq!(span.header_len().get(), 32);
        assert_eq!(span.payload_offset().get(), PIB + 32);
        assert_eq!(span.payload_len().get(), 16);
        assert_eq!(span.footer_offset().get(), PIB + 48);
        assert_eq!(span.footer_len().get(), 32);
        assert_eq!(span.end().get(), PIB + 80);
        assert_eq!(span.physical_len().get(), 80);
    }

    #[test]
    fn snapshot_ranges_cover_the_full_u64_domain_without_saturation() {
        let bounds = SnapshotBounds::new(ByteLength::new(u64::MAX));
        assert_eq!(
            bounds
                .validate_range(FileOffset::new(u64::MAX - 1), ByteLength::new(1))
                .unwrap()
                .get(),
            u64::MAX
        );
        assert_eq!(
            bounds
                .validate_range(FileOffset::new(u64::MAX), ByteLength::ZERO)
                .unwrap()
                .get(),
            u64::MAX
        );

        assert!(matches!(
            bounds.validate_range(FileOffset::new(u64::MAX), ByteLength::new(1)),
            Err(Error::ResourceArithmeticOverflow {
                resource: "snapshot range"
            })
        ));
        assert!(matches!(
            SnapshotBounds::new(ByteLength::new(10))
                .validate_range(FileOffset::new(11), ByteLength::ZERO),
            Err(Error::SnapshotRangeOutOfBounds {
                offset: 11,
                len: 0,
                snapshot_len: 10
            })
        ));
    }

    #[test]
    fn primitive_checked_additions_reject_u64_overflow() {
        assert!(matches!(
            FileOffset::new(u64::MAX).checked_add(ByteLength::new(1)),
            Err(Error::ResourceArithmeticOverflow {
                resource: "file offset"
            })
        ));
        assert!(matches!(
            ByteLength::new(u64::MAX).checked_add(ByteLength::new(1)),
            Err(Error::ResourceArithmeticOverflow {
                resource: "byte length"
            })
        ));
        assert_eq!(
            FileOffset::ZERO
                .checked_add(ByteLength::new(PIB))
                .unwrap()
                .get(),
            PIB
        );
    }

    #[test]
    fn record_validation_rejects_framing_below_minimum() {
        let bounds = SnapshotBounds::new(ByteLength::new(1_000));
        assert!(matches!(
            bounds.validate(UntrustedRecordPointer::new(100, 63), HEADER_LEN, FOOTER_LEN),
            Err(Error::InvalidIndexCheckpoint)
        ));
        assert!(matches!(
            bounds.validate(
                UntrustedRecordPointer::new(100, 0),
                ByteLength::ZERO,
                ByteLength::ZERO
            ),
            Err(Error::InvalidIndexCheckpoint)
        ));
        assert!(matches!(
            bounds.validate(
                UntrustedRecordPointer::new(0, u64::MAX),
                ByteLength::new(u64::MAX),
                ByteLength::new(1)
            ),
            Err(Error::ResourceArithmeticOverflow {
                resource: "byte length"
            })
        ));
    }

    #[test]
    fn record_parts_reject_gaps_overlaps_and_reverse_ordering() {
        let bounds = SnapshotBounds::new(ByteLength::new(1_000));
        let common = (
            bounds,
            FileOffset::new(100),
            HEADER_LEN,
            ByteLength::new(10),
            FOOTER_LEN,
        );

        assert!(matches!(
            RecordSpan::from_parts(
                common.0,
                common.1,
                common.2,
                FileOffset::new(131),
                common.3,
                FileOffset::new(141),
                common.4,
            ),
            Err(Error::InvalidIndexCheckpoint)
        ));
        assert!(matches!(
            RecordSpan::from_parts(
                common.0,
                common.1,
                common.2,
                FileOffset::new(132),
                common.3,
                FileOffset::new(141),
                common.4,
            ),
            Err(Error::InvalidIndexCheckpoint)
        ));
        assert!(matches!(
            RecordSpan::from_parts(
                common.0,
                common.1,
                common.2,
                FileOffset::new(99),
                common.3,
                FileOffset::new(109),
                common.4,
            ),
            Err(Error::InvalidIndexCheckpoint)
        ));
    }

    #[test]
    fn record_validation_checks_every_extent_at_the_u64_boundary() {
        let bounds = SnapshotBounds::new(ByteLength::new(u64::MAX));

        let exact = bounds
            .validate(
                UntrustedRecordPointer::new(u64::MAX - 64, 64),
                HEADER_LEN,
                FOOTER_LEN,
            )
            .unwrap()
            .span();
        assert_eq!(exact.end().get(), u64::MAX);

        assert!(matches!(
            bounds.validate(
                UntrustedRecordPointer::new(u64::MAX - 31, 32),
                HEADER_LEN,
                ByteLength::ZERO
            ),
            Err(Error::ResourceArithmeticOverflow {
                resource: "record header extent"
            })
        ));
        assert!(matches!(
            bounds.validate(
                UntrustedRecordPointer::new(u64::MAX - 40, 41),
                HEADER_LEN,
                ByteLength::ZERO
            ),
            Err(Error::ResourceArithmeticOverflow {
                resource: "record payload extent"
            })
        ));
        assert!(matches!(
            bounds.validate(
                UntrustedRecordPointer::new(u64::MAX - 63, 64),
                HEADER_LEN,
                ByteLength::new(1)
            ),
            Err(Error::ResourceArithmeticOverflow {
                resource: "record footer extent"
            })
        ));
    }

    #[test]
    fn record_validation_rejects_extent_past_snapshot() {
        let bounds = SnapshotBounds::new(ByteLength::new(200));
        assert!(matches!(
            bounds.validate(UntrustedRecordPointer::new(150, 64), HEADER_LEN, FOOTER_LEN),
            Err(Error::SnapshotRangeOutOfBounds {
                offset: 150,
                len: 64,
                snapshot_len: 200
            })
        ));
    }

    #[test]
    fn empty_payload_is_valid_when_the_frame_is_complete() {
        let bounds = SnapshotBounds::new(ByteLength::new(164));
        let span = bounds
            .validate(UntrustedRecordPointer::new(100, 64), HEADER_LEN, FOOTER_LEN)
            .unwrap()
            .span();
        assert_eq!(span.payload_len(), ByteLength::ZERO);
        assert_eq!(span.payload_offset(), span.footer_offset());
    }

    #[test]
    fn usize_conversion_is_explicit_and_platform_checked() {
        let pebibyte = ByteLength::new(PIB).try_usize();
        if usize::BITS > 50 {
            assert_eq!(pebibyte.unwrap() as u64, PIB);
        } else {
            assert!(matches!(
                pebibyte,
                Err(Error::LengthOverflow { value: PIB })
            ));
        }

        let maximum = ByteLength::new(u64::MAX).try_usize();
        if usize::BITS < u64::BITS {
            assert!(matches!(
                maximum,
                Err(Error::LengthOverflow { value: u64::MAX })
            ));
        } else {
            assert_eq!(maximum.unwrap(), usize::MAX);
        }
    }
}
