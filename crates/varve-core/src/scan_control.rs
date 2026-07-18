use std::num::NonZeroU64;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

const DEFAULT_PROGRESS_RECORDS: u64 = 16_384;
const DEFAULT_PROGRESS_BYTES: u64 = 16 * 1024 * 1024;

/// Identifies the point in a scan at which progress was observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanProgressPhase {
    /// The scan has not completed a record yet.
    Started,
    /// The scan has completed at least one record.
    Running,
    /// The scan completed successfully and is ready to publish its result.
    Complete,
}

/// A synchronous snapshot of an explicit scan's progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanProgress {
    pub phase: ScanProgressPhase,
    pub records: u64,
    pub scanned_bytes: u64,
    pub current_offset: u64,
    pub snapshot_len: u64,
}

/// A cloneable cooperative-cancellation token for explicit scans.
#[derive(Clone, Debug, Default)]
pub struct ScanCancellationToken(Arc<AtomicBool>);

impl ScanCancellationToken {
    /// Creates a token whose cancellation flag is initially clear.
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation of scans using this token.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Returns whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Configures the cadence of running progress notifications.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanProgressOptions {
    pub every_records: Option<NonZeroU64>,
    pub every_bytes: Option<NonZeroU64>,
}

impl Default for ScanProgressOptions {
    fn default() -> Self {
        Self {
            every_records: NonZeroU64::new(DEFAULT_PROGRESS_RECORDS),
            every_bytes: NonZeroU64::new(DEFAULT_PROGRESS_BYTES),
        }
    }
}

/// Configures progress reporting and cancellation for an explicit scan.
#[derive(Clone, Copy, Debug, Default)]
pub struct ScanOptions<'a> {
    pub progress: ScanProgressOptions,
    pub cancellation: Option<&'a ScanCancellationToken>,
}

/// Internal cancellation result mapped to the public error by scan entry points.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScanCancelled {
    pub(crate) progress: ScanProgress,
}

/// Allocation-free progress bookkeeping shared by the explicit scan entry points.
pub(crate) struct ScanProgressDriver<'a> {
    cancellation: Option<&'a ScanCancellationToken>,
    cadence: ScanProgressOptions,
    record_region_offset: u64,
    progress: ScanProgress,
    notified_records: u64,
    notified_bytes: u64,
}

impl<'a> ScanProgressDriver<'a> {
    pub(crate) fn new(
        options: ScanOptions<'a>,
        record_region_offset: u64,
        snapshot_len: u64,
    ) -> Self {
        debug_assert!(record_region_offset <= snapshot_len);
        Self {
            cancellation: options.cancellation,
            cadence: options.progress,
            record_region_offset,
            progress: ScanProgress {
                phase: ScanProgressPhase::Started,
                records: 0,
                scanned_bytes: 0,
                current_offset: record_region_offset,
                snapshot_len,
            },
            notified_records: 0,
            notified_bytes: 0,
        }
    }

    pub(crate) fn start<F>(&mut self, observer: &mut F) -> Result<(), ScanCancelled>
    where
        F: FnMut(ScanProgress),
    {
        self.check_cancelled()?;
        observer(self.progress);
        self.check_cancelled()
    }

    pub(crate) fn record_completed<F>(
        &mut self,
        current_offset: u64,
        observer: &mut F,
    ) -> Result<(), ScanCancelled>
    where
        F: FnMut(ScanProgress),
    {
        debug_assert!(self.progress.phase != ScanProgressPhase::Complete);
        debug_assert!(current_offset >= self.progress.current_offset);
        debug_assert!(current_offset <= self.progress.snapshot_len);

        self.progress.phase = ScanProgressPhase::Running;
        self.progress.records = self
            .progress
            .records
            .checked_add(1)
            .expect("scan record count exceeds u64");
        self.progress.current_offset = current_offset;
        self.progress.scanned_bytes = current_offset - self.record_region_offset;

        self.check_cancelled()?;
        if self.notification_due() {
            self.notified_records = self.progress.records;
            self.notified_bytes = self.progress.scanned_bytes;
            observer(self.progress);
            self.check_cancelled()?;
        }
        Ok(())
    }

    pub(crate) fn complete<F>(&mut self, observer: &mut F) -> Result<(), ScanCancelled>
    where
        F: FnMut(ScanProgress),
    {
        debug_assert!(self.progress.phase != ScanProgressPhase::Complete);
        self.progress.phase = ScanProgressPhase::Complete;
        observer(self.progress);
        self.check_cancelled()
    }

    fn notification_due(&self) -> bool {
        self.cadence
            .every_records
            .is_some_and(|every| self.progress.records - self.notified_records >= every.get())
            || self.cadence.every_bytes.is_some_and(|every| {
                self.progress.scanned_bytes - self.notified_bytes >= every.get()
            })
    }

    fn check_cancelled(&self) -> Result<(), ScanCancelled> {
        if self
            .cancellation
            .is_some_and(ScanCancellationToken::is_cancelled)
        {
            Err(ScanCancelled {
                progress: self.progress,
            })
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nonzero(value: u64) -> Option<NonZeroU64> {
        NonZeroU64::new(value)
    }

    #[test]
    fn emits_started_running_and_complete_at_coalesced_cadence() {
        let options = ScanOptions {
            progress: ScanProgressOptions {
                every_records: nonzero(3),
                every_bytes: nonzero(5),
            },
            cancellation: None,
        };
        let mut driver = ScanProgressDriver::new(options, 10, 30);
        let mut observed = Vec::new();

        driver
            .start(&mut |progress| observed.push(progress))
            .unwrap();
        driver
            .record_completed(12, &mut |progress| observed.push(progress))
            .unwrap();
        driver
            .record_completed(16, &mut |progress| observed.push(progress))
            .unwrap();
        driver
            .record_completed(18, &mut |progress| observed.push(progress))
            .unwrap();
        driver
            .record_completed(21, &mut |progress| observed.push(progress))
            .unwrap();
        driver
            .record_completed(24, &mut |progress| observed.push(progress))
            .unwrap();
        driver
            .complete(&mut |progress| observed.push(progress))
            .unwrap();

        assert_eq!(
            observed,
            [
                ScanProgress {
                    phase: ScanProgressPhase::Started,
                    records: 0,
                    scanned_bytes: 0,
                    current_offset: 10,
                    snapshot_len: 30,
                },
                ScanProgress {
                    phase: ScanProgressPhase::Running,
                    records: 2,
                    scanned_bytes: 6,
                    current_offset: 16,
                    snapshot_len: 30,
                },
                ScanProgress {
                    phase: ScanProgressPhase::Running,
                    records: 4,
                    scanned_bytes: 11,
                    current_offset: 21,
                    snapshot_len: 30,
                },
                ScanProgress {
                    phase: ScanProgressPhase::Complete,
                    records: 5,
                    scanned_bytes: 14,
                    current_offset: 24,
                    snapshot_len: 30,
                },
            ]
        );
    }

    #[test]
    fn emitted_counts_bytes_and_offsets_are_monotonic() {
        let options = ScanOptions {
            progress: ScanProgressOptions {
                every_records: nonzero(1),
                every_bytes: None,
            },
            cancellation: None,
        };
        let mut driver = ScanProgressDriver::new(options, 100, 180);
        let mut observed = Vec::new();
        driver
            .start(&mut |progress| observed.push(progress))
            .unwrap();
        for offset in [101, 119, 120, 179] {
            driver
                .record_completed(offset, &mut |progress| observed.push(progress))
                .unwrap();
        }
        driver
            .complete(&mut |progress| observed.push(progress))
            .unwrap();

        for pair in observed.windows(2) {
            assert!(pair[0].records <= pair[1].records);
            assert!(pair[0].scanned_bytes <= pair[1].scanned_bytes);
            assert!(pair[0].current_offset <= pair[1].current_offset);
            assert_eq!(pair[0].snapshot_len, pair[1].snapshot_len);
        }
    }

    #[test]
    fn pre_cancel_skips_started_callback_and_reports_initial_progress() {
        let token = ScanCancellationToken::default();
        token.cancel();
        let options = ScanOptions {
            cancellation: Some(&token),
            ..ScanOptions::default()
        };
        let mut driver = ScanProgressDriver::new(options, 32, 96);
        let mut callback_count = 0;

        let cancelled = driver
            .start(&mut |_| callback_count += 1)
            .expect_err("pre-cancelled scan must stop");

        assert_eq!(callback_count, 0);
        assert_eq!(
            cancelled.progress,
            ScanProgress {
                phase: ScanProgressPhase::Started,
                records: 0,
                scanned_bytes: 0,
                current_offset: 32,
                snapshot_len: 96,
            }
        );
    }

    #[test]
    fn callback_can_cancel_through_a_cloned_token() {
        let token = ScanCancellationToken::default();
        let callback_token = token.clone();
        let options = ScanOptions {
            progress: ScanProgressOptions {
                every_records: nonzero(1),
                every_bytes: None,
            },
            cancellation: Some(&token),
        };
        let mut driver = ScanProgressDriver::new(options, 8, 40);
        let mut observer = |progress: ScanProgress| {
            if progress.phase == ScanProgressPhase::Running {
                callback_token.cancel();
            }
        };

        driver.start(&mut observer).unwrap();
        let cancelled = driver
            .record_completed(20, &mut observer)
            .expect_err("callback cancellation must be observed");

        assert!(token.is_cancelled());
        assert_eq!(
            cancelled.progress,
            ScanProgress {
                phase: ScanProgressPhase::Running,
                records: 1,
                scanned_bytes: 12,
                current_offset: 20,
                snapshot_len: 40,
            }
        );
    }

    #[test]
    fn cancellation_after_a_record_reports_exact_unnotified_progress() {
        let token = ScanCancellationToken::default();
        let options = ScanOptions {
            progress: ScanProgressOptions {
                every_records: None,
                every_bytes: None,
            },
            cancellation: Some(&token),
        };
        let mut driver = ScanProgressDriver::new(options, 100, 200);
        let mut observed = Vec::new();
        driver
            .start(&mut |progress| observed.push(progress))
            .unwrap();
        driver
            .record_completed(120, &mut |progress| observed.push(progress))
            .unwrap();
        token.cancel();

        let cancelled = driver
            .record_completed(157, &mut |progress| observed.push(progress))
            .expect_err("record-boundary cancellation must be observed");

        assert_eq!(observed.len(), 1);
        assert_eq!(
            cancelled.progress,
            ScanProgress {
                phase: ScanProgressPhase::Running,
                records: 2,
                scanned_bytes: 57,
                current_offset: 157,
                snapshot_len: 200,
            }
        );
    }

    #[test]
    fn cancellation_from_complete_callback_reports_complete_progress() {
        let token = ScanCancellationToken::default();
        let callback_token = token.clone();
        let options = ScanOptions {
            progress: ScanProgressOptions {
                every_records: None,
                every_bytes: None,
            },
            cancellation: Some(&token),
        };
        let mut driver = ScanProgressDriver::new(options, 64, 128);
        driver.start(&mut |_| {}).unwrap();
        driver.record_completed(96, &mut |_| {}).unwrap();

        let cancelled = driver
            .complete(&mut |progress| {
                assert_eq!(progress.phase, ScanProgressPhase::Complete);
                callback_token.cancel();
            })
            .expect_err("final callback cancellation must be observed");

        assert_eq!(
            cancelled.progress,
            ScanProgress {
                phase: ScanProgressPhase::Complete,
                records: 1,
                scanned_bytes: 32,
                current_offset: 96,
                snapshot_len: 128,
            }
        );
    }

    #[test]
    fn progress_bookkeeping_performs_no_allocations() {
        let token = ScanCancellationToken::new();
        let options = ScanOptions {
            progress: ScanProgressOptions {
                every_records: nonzero(1),
                every_bytes: nonzero(1),
            },
            cancellation: Some(&token),
        };
        let mut driver = ScanProgressDriver::new(options, 64, 4_096);
        let mut observer = |_progress: ScanProgress| {};

        let (result, allocations) = crate::pib_probe::count_allocations_during(|| {
            driver.start(&mut observer)?;
            for offset in 65..=4_096 {
                driver.record_completed(offset, &mut observer)?;
            }
            driver.complete(&mut observer)
        });

        assert!(result.is_ok());
        assert_eq!(allocations, 0);
    }
}
