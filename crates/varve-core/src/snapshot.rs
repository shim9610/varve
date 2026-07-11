use std::{
    fs::File,
    io::{BufReader, Read, Seek, SeekFrom, Write},
    sync::Arc,
};

use crate::{Error, Result};

#[derive(Clone, Debug)]
pub(crate) struct SnapshotFile {
    file: Arc<File>,
    len: u64,
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
        let physical_len = file.metadata()?.len();
        if len > physical_len {
            return Err(Error::SnapshotRangeOutOfBounds {
                offset: 0,
                len,
                snapshot_len: physical_len,
            });
        }
        Ok(Self {
            file: Arc::new(file),
            len,
        })
    }

    pub(crate) const fn len(&self) -> u64 {
        self.len
    }

    pub(crate) fn with_len(&self, len: u64) -> Self {
        Self {
            file: Arc::clone(&self.file),
            len,
        }
    }

    pub(crate) fn cursor_at(&self, offset: u64) -> Result<SnapshotCursor> {
        if offset > self.len {
            return Err(Error::SnapshotRangeOutOfBounds {
                offset,
                len: 0,
                snapshot_len: self.len,
            });
        }
        let mut reader = BufReader::with_capacity(64 * 1024, self.file.try_clone()?);
        reader.seek(SeekFrom::Start(offset))?;
        Ok(SnapshotCursor {
            reader,
            len: self.len,
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

    pub(crate) fn read_vec_at(
        &self,
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
        let len = usize::try_from(len).map_err(|_| Error::LengthOverflow { value: len })?;
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
            self.read_exact_at(offset + copied, &mut buffer[..chunk_len])?;
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
        let end = offset
            .checked_add(len)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "snapshot range",
            })?;
        if end > self.len {
            return Err(Error::SnapshotRangeOutOfBounds {
                offset,
                len,
                snapshot_len: self.len,
            });
        }
        Ok(())
    }
}

pub(crate) struct SnapshotCursor {
    reader: BufReader<File>,
    len: u64,
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
        let len = usize::try_from(len).map_err(|_| Error::LengthOverflow { value: len })?;
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
        let end = offset
            .checked_add(len)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "snapshot cursor range",
            })?;
        if end > self.len {
            return Err(Error::SnapshotRangeOutOfBounds {
                offset,
                len,
                snapshot_len: self.len,
            });
        }
        Ok(())
    }
}

#[cfg(unix)]
fn read_at(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buffer, offset)
}

#[cfg(windows)]
fn read_at(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_read(buffer, offset)
}

#[cfg(not(any(unix, windows)))]
fn read_at(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
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
    fn snapshot_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SnapshotFile>();
    }
}
