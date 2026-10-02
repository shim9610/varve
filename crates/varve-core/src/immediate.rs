//! Caller-selected durability boundaries for append writers.

use crate::{AppendInfo, Error, Result};

/// The logical operation used when evaluating an Immediate condition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImmediateOperation {
    Append,
    Delete,
}

/// A prospective append boundary, including the record that triggers it.
/// Counters cover user appends/deletes since the last successful `sync` or
/// `immediate` on this handle. Bytes include native record framing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImmediateEvent {
    /// For a deletion this is the deleted block's id, not the tombstone id.
    pub block_id: u32,
    pub operation: ImmediateOperation,
    pub sequence: u64,
    pub record_bytes: u64,
    pub pending_records: u64,
    pub pending_bytes: u64,
}

/// OR-combined conditions for automatically calling `immediate()`.
///
/// The default has no automatic condition. Block declarations remain mandatory
/// regardless of this policy. Installing a policy does not reset counters or
/// itself perform I/O; it applies to the next append/deletion. Predicates must
/// be pure: batch writers may evaluate them before publishing the record.
#[derive(Clone, Copy, Debug, Default)]
pub struct ImmediatePolicy {
    records: Option<u64>,
    bytes: Option<u64>,
    predicate: Option<fn(ImmediateEvent) -> bool>,
}

impl ImmediatePolicy {
    pub const fn new() -> Self {
        Self {
            records: None,
            bytes: None,
            predicate: None,
        }
    }

    /// Trigger at this many pending records. Zero is rejected on installation.
    pub const fn after_records(mut self, records: u64) -> Self {
        self.records = Some(records);
        self
    }

    /// Trigger at this many pending native bytes. Zero is rejected on installation.
    pub const fn after_bytes(mut self, bytes: u64) -> Self {
        self.bytes = Some(bytes);
        self
    }

    /// Add an application condition, such as a block id or sequence boundary.
    pub const fn when(mut self, predicate: fn(ImmediateEvent) -> bool) -> Self {
        self.predicate = Some(predicate);
        self
    }

    pub(crate) fn validate(self) -> Result<Self> {
        if self.records == Some(0) || self.bytes == Some(0) {
            return Err(Error::InvalidImmediatePolicy);
        }
        Ok(self)
    }

    pub(crate) fn matches(self, event: ImmediateEvent) -> bool {
        self.records
            .is_some_and(|limit| event.pending_records >= limit)
            || self.bytes.is_some_and(|limit| event.pending_bytes >= limit)
            || self.predicate.is_some_and(|predicate| predicate(event))
    }
}

#[derive(Debug, Default)]
pub(crate) struct ImmediateState {
    pub(crate) policy: ImmediatePolicy,
    records: u64,
    bytes: u64,
}

impl ImmediateState {
    pub(crate) fn event(
        &self,
        block_id: u32,
        operation: ImmediateOperation,
        info: AppendInfo,
        record_bytes: u64,
        staged_records: u64,
        staged_bytes: u64,
    ) -> ImmediateEvent {
        ImmediateEvent {
            block_id,
            operation,
            sequence: info.sequence,
            record_bytes,
            pending_records: self.records.saturating_add(staged_records),
            pending_bytes: self.bytes.saturating_add(staged_bytes),
        }
    }

    pub(crate) fn advance(&mut self, records: u64, bytes: u64) {
        self.records = self.records.saturating_add(records);
        self.bytes = self.bytes.saturating_add(bytes);
    }

    pub(crate) fn reset(&mut self) {
        self.records = 0;
        self.bytes = 0;
    }
}

pub(crate) fn appended_immediate_error(sequence: u64, source: Error) -> Error {
    Error::AppendedButImmediateFailed {
        sequence,
        source: Box::new(source),
    }
}
