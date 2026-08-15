use std::{
    fs::File,
    io::{BufReader, Read, Seek, SeekFrom, Write},
    sync::Arc,
};

#[cfg(any(feature = "high-cardinality-dev", test))]
use crate::scalable_extent::{UntrustedRecordPointer, ValidatedRecordPointer};
use crate::{
    Error, Result,
    scalable_extent::{ByteLength, FileOffset, SnapshotBounds},
};

#[derive(Clone, Debug)]
pub(crate) struct SnapshotFile {
    file: Arc<File>,
    bounds: SnapshotBounds,
}

/// Proof that the backing file physically reaches an offset.
///
/// The field is private to this module and the only constructor names the
/// obligation: a `write_all` of `B` bytes that started at `start` and returned
/// `Ok` implies `physical_len >= start + B`, because `write_all` is
/// all-or-error. It is a `u64` newtype, so producing one costs no allocation
/// and no syscall — which is the entire point on the append path.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WrittenThrough(u64);

impl WrittenThrough {
    /// Records that a completed write reached `end_offset`.
    ///
    /// Call sites must have observed the write return `Ok`; passing a
    /// prospective offset is what this type exists to make visible in review.
    pub(crate) const fn after_write(end_offset: u64) -> Self {
        Self(end_offset)
    }

    pub(crate) const fn end_offset(self) -> u64 {
        self.0
    }
}

impl SnapshotFile {
    pub(crate) fn new(file: File) -> Result<Self> {
        Self::from_file(file)
    }

    pub(crate) fn from_file(file: File) -> Result<Self> {
        let len = file.metadata()?.len();
        Self::from_file_with_len(file, len)
    }

    pub(crate) fn from_file_with_len(file: File, len: u64) -> Result<Self> {
        let bounds = checked_snapshot_bounds(&file, len)?;
        Ok(Self {
            file: Arc::new(file),
            bounds,
        })
    }

    pub(crate) const fn len(&self) -> u64 {
        self.bounds.logical_len().get()
    }

    /// Rebinds to an arbitrary length, paying the `fstat` that proves the file
    /// reaches it.
    ///
    /// Two callers, and they are the two places a snapshot moves to a length
    /// nobody just wrote: the scalable family, which shrinks or re-pins one
    /// (`stream.rs`, behind `high-cardinality-dev`), and `VarveFile::follow`,
    /// which extends a read-only handle to a length a *different* process
    /// wrote. The append path uses [`Self::with_written_len`] instead, which
    /// proves the same fact from the write that just returned and issues no
    /// syscall.
    ///
    /// It carried a `dead_code` exemption naming `stream.rs` as the only
    /// caller, which stopped being true when `follow` landed — and `follow` is
    /// ungated, so the exemption was suppressing a lint that no longer fires
    /// under any configuration.
    pub(crate) fn with_len(&self, len: u64) -> Result<Self> {
        let bounds = checked_snapshot_bounds(&self.file, len)?;
        Ok(Self {
            file: Arc::clone(&self.file),
            bounds,
        })
    }

    /// Rebinds to the end of a write that has already completed.
    ///
    /// [`Self::with_len`] establishes exactly one fact — `len <= physical_len`
    /// — and the only way to learn `physical_len` from nothing is an `fstat`.
    /// A caller that has just returned from `write_all` knows the fact for
    /// free, so it hands over a [`WrittenThrough`] instead of a bare `u64` and
    /// no metadata syscall is issued. That is one of the two per-record
    /// syscalls the append hot path used to pay.
    ///
    /// Still fallible, and still growth-only: a witness is proof that the file
    /// reaches `end_offset`, never proof that a *shorter* snapshot is right, so
    /// this refuses to shrink. Shrinking a snapshot needs `with_len`, which
    /// pays the `fstat` and is not on the append path.
    ///
    /// What is given up relative to `with_len` is an incidental side effect of
    /// the `fstat`: it also noticed a file truncated under the writer by
    /// something outside this process. That now surfaces on the next read as
    /// `UnexpectedEof` / `SnapshotRangeOutOfBounds` rather than at append time
    /// — still an error, never silently wrong data — and the writer holds the
    /// single-writer advisory lock that puts the scenario out of contract.
    pub(crate) fn with_written_len(&self, written: WrittenThrough) -> Result<Self> {
        let len = written.end_offset();
        if len < self.len() {
            return Err(Error::SnapshotRangeOutOfBounds {
                offset: 0,
                len,
                snapshot_len: self.len(),
            });
        }
        Ok(Self {
            file: Arc::clone(&self.file),
            bounds: SnapshotBounds::new(ByteLength::new(len)),
        })
    }

    #[cfg(any(feature = "high-cardinality-dev", test))]
    pub(crate) fn validate(
        &self,
        pointer: UntrustedRecordPointer,
        has_footer: bool,
    ) -> Result<ValidatedRecordPointer> {
        self.bounds.validate_native_record(pointer, has_footer)
    }

    pub(crate) fn cursor_at(&self, offset: u64) -> Result<SnapshotCursor> {
        self.check_range(offset, 0)?;
        let mut reader = BufReader::with_capacity(64 * 1024, self.file.try_clone()?);
        reader.seek(SeekFrom::Start(offset))?;
        Ok(SnapshotCursor {
            reader,
            bounds: self.bounds,
            position: offset,
        })
    }

    pub(crate) fn try_clone_file(&self) -> Result<File> {
        Ok(self.file.try_clone()?)
    }

    pub(crate) fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> Result<()> {
        let len =
            u64::try_from(buffer.len()).map_err(|_| Error::LengthOverflow { value: u64::MAX })?;
        self.check_range(offset, len)?;

        let mut consumed = 0usize;
        while consumed < buffer.len() {
            let current_offset = offset
                .checked_add(
                    u64::try_from(consumed)
                        .map_err(|_| Error::LengthOverflow { value: u64::MAX })?,
                )
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "snapshot read offset",
                })?;
            match read_at(&self.file, &mut buffer[consumed..], current_offset) {
                Ok(0) => return Err(Error::UnexpectedEof),
                Ok(read) => consumed += read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    /// **This allocates.** [`read_into_at`](Self::read_into_at) takes the
    /// caller's buffer instead, which is the form the per-record read loops
    /// use: one buffer for the whole walk rather than one per record.
    pub(crate) fn read_vec_at(
        &self,
        offset: u64,
        len: u64,
        limit: u64,
        resource: &'static str,
    ) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.read_into_at(offset, len, limit, resource, &mut bytes)?;
        Ok(bytes)
    }

    /// Reads `len` bytes at `offset` into the caller's buffer.
    ///
    /// `out` ends **exactly** `len` bytes long, whatever it held before. That
    /// is load-bearing rather than tidy: the callers hand `out` straight to
    /// `verify_snapshot_record`, and a buffer left longer than the record —
    /// which a reused buffer is, after any larger record — would checksum
    /// trailing bytes from the previous read and report a healthy file as
    /// corrupt.
    ///
    /// The limit refusal and the fallible reservation both still precede the
    /// first byte written, so an oversized read is refused before the memory is
    /// taken. A buffer that already has the capacity keeps it: `try_reserve` is
    /// a no-op then, which is the whole reason the caller supplies one.
    pub(crate) fn read_into_at(
        &self,
        offset: u64,
        len: u64,
        limit: u64,
        resource: &'static str,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        if len > limit {
            return Err(Error::LimitExceeded {
                resource,
                actual: len,
                limit,
            });
        }
        self.check_range(offset, len)?;
        let len = ByteLength::new(len).try_usize()?;
        out.clear();
        out.try_reserve(len).map_err(|_| Error::AllocationFailed {
            resource,
            requested: u64::try_from(len).unwrap_or(u64::MAX),
        })?;
        out.resize(len, 0);
        self.read_exact_at(offset, out)
    }

    pub(crate) fn copy_range_to<W: Write>(
        &self,
        offset: u64,
        len: u64,
        output: &mut W,
    ) -> Result<()> {
        self.check_range(offset, len)?;
        let mut buffer = [0u8; 64 * 1024];
        let mut copied = 0u64;
        while copied < len {
            let remaining = len - copied;
            let chunk_len = usize::try_from(remaining.min(buffer.len() as u64))
                .map_err(|_| Error::LengthOverflow { value: remaining })?;
            let current_offset = FileOffset::new(offset)
                .checked_add(ByteLength::new(copied))?
                .get();
            self.read_exact_at(current_offset, &mut buffer[..chunk_len])?;
            output.write_all(&buffer[..chunk_len])?;
            copied =
                copied
                    .checked_add(chunk_len as u64)
                    .ok_or(Error::ResourceArithmeticOverflow {
                        resource: "snapshot copy length",
                    })?;
        }
        Ok(())
    }

    fn check_range(&self, offset: u64, len: u64) -> Result<()> {
        self.bounds
            .validate_range(FileOffset::new(offset), ByteLength::new(len))?;
        Ok(())
    }
}

pub(crate) struct SnapshotCursor {
    reader: BufReader<File>,
    bounds: SnapshotBounds,
    position: u64,
}

impl SnapshotCursor {
    pub(crate) fn read_exact_at(&mut self, offset: u64, buffer: &mut [u8]) -> Result<()> {
        let len = u64::try_from(buffer.len()).map_err(|_| Error::ResourceArithmeticOverflow {
            resource: "snapshot cursor read length",
        })?;
        self.check_range(offset, len)?;
        self.seek_to(offset)?;

        let mut consumed = 0usize;
        while consumed < buffer.len() {
            match self.reader.read(&mut buffer[consumed..]) {
                Ok(0) => return Err(Error::UnexpectedEof),
                Ok(read) => {
                    consumed += read;
                    self.position = self.position.checked_add(read as u64).ok_or(
                        Error::ResourceArithmeticOverflow {
                            resource: "snapshot cursor position",
                        },
                    )?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    pub(crate) fn read_vec_at(
        &mut self,
        offset: u64,
        len: u64,
        limit: u64,
        resource: &'static str,
    ) -> Result<Vec<u8>> {
        if len > limit {
            return Err(Error::LimitExceeded {
                resource,
                actual: len,
                limit,
            });
        }
        self.check_range(offset, len)?;
        let len = ByteLength::new(len).try_usize()?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(len)
            .map_err(|_| Error::AllocationFailed {
                resource,
                requested: u64::try_from(len).unwrap_or(u64::MAX),
            })?;
        bytes.resize(len, 0);
        self.read_exact_at(offset, &mut bytes)?;
        Ok(bytes)
    }

    fn seek_to(&mut self, offset: u64) -> Result<()> {
        if offset == self.position {
            return Ok(());
        }
        let relative = i128::from(offset) - i128::from(self.position);
        if let Ok(relative) = i64::try_from(relative) {
            self.reader.seek_relative(relative)?;
        } else {
            self.reader.seek(SeekFrom::Start(offset))?;
        }
        self.position = offset;
        Ok(())
    }

    fn check_range(&self, offset: u64, len: u64) -> Result<()> {
        self.bounds
            .validate_range(FileOffset::new(offset), ByteLength::new(len))?;
        Ok(())
    }
}

#[cfg(any(test, feature = "scalable-fault-injection"))]
std::thread_local! {
    /// Metadata syscalls this thread has issued from
    /// [`checked_snapshot_bounds`].
    ///
    /// Fault-testing hook only. The append path must not advance it at all —
    /// it rebinds through [`SnapshotFile::with_written_len`], which proves the
    /// same fact from the write that just returned — so a regression test can
    /// delta-measure an append window and assert zero for any record count.
    static SNAPSHOT_BOUNDS_FSTATS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Counts one `checked_snapshot_bounds` metadata syscall on this thread. Inert
/// outside tests and without the `scalable-fault-injection` feature.
#[inline]
fn count_snapshot_bounds_fstat() {
    #[cfg(any(test, feature = "scalable-fault-injection"))]
    SNAPSHOT_BOUNDS_FSTATS.with(|count| count.set(count.get().saturating_add(1)));
}

/// Reads and clears this thread's `checked_snapshot_bounds` syscall count.
#[cfg(any(test, feature = "scalable-fault-injection"))]
pub(crate) fn take_snapshot_bounds_fstats() -> u64 {
    SNAPSHOT_BOUNDS_FSTATS.with(|count| count.replace(0))
}

fn checked_snapshot_bounds(file: &File, len: u64) -> Result<SnapshotBounds> {
    count_snapshot_bounds_fstat();
    let physical_len = file.metadata()?.len();
    SnapshotBounds::new(ByteLength::new(physical_len))
        .validate_range(FileOffset::ZERO, ByteLength::new(len))?;
    Ok(SnapshotBounds::new(ByteLength::new(len)))
}

#[cfg(unix)]
pub(crate) fn read_at(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buffer, offset)
}

#[cfg(windows)]
pub(crate) fn read_at(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_read(buffer, offset)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn read_at(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
    let mut file = file.try_clone()?;
    file.seek(SeekFrom::Start(offset))?;
    file.read(buffer)
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{File, OpenOptions, remove_file},
        io::Write,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;

    static NEXT_PATH: AtomicU64 = AtomicU64::new(0);

    fn temp_path() -> PathBuf {
        let id = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("varve-snapshot-{}-{id}.bin", std::process::id()))
    }

    #[test]
    fn positional_reads_stay_bound_to_the_open_object() {
        let path = temp_path();
        let replacement = path.with_extension("replacement");
        std::fs::write(&path, b"original").unwrap();
        let snapshot = SnapshotFile::new(File::open(&path).unwrap()).unwrap();

        std::fs::write(&replacement, b"replaced").unwrap();
        remove_file(&path).unwrap();
        std::fs::rename(&replacement, &path).unwrap();

        let mut bytes = [0u8; 8];
        snapshot.read_exact_at(0, &mut bytes).unwrap();
        assert_eq!(&bytes, b"original");
        assert_eq!(snapshot.len(), 8);
        assert_ne!(snapshot.len(), 0);
        remove_file(path).unwrap();
    }

    #[test]
    fn snapshot_length_does_not_grow_with_the_file() {
        let path = temp_path();
        std::fs::write(&path, b"old").unwrap();
        let snapshot = SnapshotFile::new(File::open(&path).unwrap()).unwrap();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"new")
            .unwrap();

        assert!(matches!(
            snapshot.read_vec_at(0, 6, 6, "test"),
            Err(Error::SnapshotRangeOutOfBounds {
                snapshot_len: 3,
                ..
            })
        ));
        remove_file(path).unwrap();
    }

    #[test]
    fn snapshot_length_rebinding_and_growth_are_fallible() {
        let path = temp_path();
        std::fs::write(&path, b"old").unwrap();
        let snapshot = SnapshotFile::new(File::open(&path).unwrap()).unwrap();

        assert!(matches!(
            snapshot.with_len(4),
            Err(Error::SnapshotRangeOutOfBounds {
                offset: 0,
                len: 4,
                snapshot_len: 3
            })
        ));

        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"!")
            .unwrap();
        let grown = snapshot.with_len(4).unwrap();
        let shortened = grown.with_len(2).unwrap();

        assert_eq!(snapshot.len(), 3);
        assert_eq!(grown.len(), 4);
        assert_eq!(grown.read_vec_at(0, 4, 4, "test").unwrap(), b"old!");
        assert_eq!(shortened.len(), 2);
        assert!(matches!(
            shortened.read_vec_at(2, 1, 1, "test"),
            Err(Error::SnapshotRangeOutOfBounds {
                offset: 2,
                len: 1,
                snapshot_len: 2
            })
        ));
        remove_file(path).unwrap();
    }

    #[test]
    fn record_pointer_validation_uses_native_minimum_framing() {
        let path = temp_path();
        let header_len = crate::native_layout::native_record_header_len();
        let footer_len = crate::native_layout::native_record_footer_len();
        let physical_len = header_len + footer_len;
        std::fs::write(&path, vec![0; physical_len as usize]).unwrap();
        let snapshot = SnapshotFile::new(File::open(&path).unwrap()).unwrap();

        assert!(matches!(
            snapshot.validate(UntrustedRecordPointer::new(0, header_len - 1), false),
            Err(Error::InvalidIndexCheckpoint)
        ));

        let no_footer = snapshot
            .validate(UntrustedRecordPointer::new(0, header_len), false)
            .unwrap()
            .span();
        assert_eq!(no_footer.header_len().get(), header_len);
        assert_eq!(no_footer.payload_len(), ByteLength::ZERO);
        assert_eq!(no_footer.footer_len(), ByteLength::ZERO);

        let with_footer = snapshot
            .validate(UntrustedRecordPointer::new(0, physical_len), true)
            .unwrap()
            .span();
        assert_eq!(with_footer.header_len().get(), header_len);
        assert_eq!(with_footer.payload_len(), ByteLength::ZERO);
        assert_eq!(with_footer.footer_len().get(), footer_len);
        remove_file(path).unwrap();
    }

    #[test]
    fn validated_logical_eof_can_be_shorter_than_physical_eof() {
        let path = temp_path();
        std::fs::write(&path, b"valid-torn").unwrap();
        let snapshot = SnapshotFile::from_file_with_len(File::open(&path).unwrap(), 5).unwrap();
        assert_eq!(snapshot.read_vec_at(0, 5, 5, "test").unwrap(), b"valid");
        assert!(matches!(
            snapshot.read_vec_at(5, 1, 1, "test"),
            Err(Error::SnapshotRangeOutOfBounds {
                snapshot_len: 5,
                ..
            })
        ));
        remove_file(path).unwrap();
    }

    #[test]
    fn range_copy_and_limit_checks_are_bounded() {
        let path = temp_path();
        std::fs::write(&path, b"0123456789").unwrap();
        let snapshot = SnapshotFile::new(File::open(&path).unwrap()).unwrap();
        let mut copied = Vec::new();
        snapshot.copy_range_to(2, 5, &mut copied).unwrap();
        assert_eq!(copied, b"23456");
        assert!(matches!(
            snapshot.read_vec_at(0, 5, 4, "test payload"),
            Err(Error::LimitExceeded { .. })
        ));
        assert!(snapshot.try_clone_file().is_ok());
        remove_file(path).unwrap();
    }

    #[test]
    fn hostile_overflow_ranges_fail_before_read_or_allocation() {
        let path = temp_path();
        std::fs::write(&path, b"x").unwrap();
        let snapshot = SnapshotFile::new(File::open(&path).unwrap()).unwrap();

        assert!(matches!(
            snapshot.read_vec_at(u64::MAX, 1, 1, "test"),
            Err(Error::ResourceArithmeticOverflow {
                resource: "snapshot range"
            })
        ));

        let mut cursor = snapshot.cursor_at(0).unwrap();
        assert!(matches!(
            cursor.read_vec_at(u64::MAX, 1, 1, "test"),
            Err(Error::ResourceArithmeticOverflow {
                resource: "snapshot range"
            })
        ));
        remove_file(path).unwrap();
    }

    #[test]
    fn snapshot_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SnapshotFile>();
    }
}
