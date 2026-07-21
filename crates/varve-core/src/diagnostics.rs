use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt::Debug;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::{
    Error, FormatSpec, MatrixCellStatus, MatrixDimensions, MatrixKey, Result, VarveBlock,
    VarveFile, VarveKeyedBlock, VarveMatrixBlock, collections::MaterializationBudget,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticDomain {
    FormatDefinition,
    CallerUsage,
    FileData,
    Environment,
    FeatureGate,
    LibraryInvariant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: DiagnosticSeverity,
    pub domain: DiagnosticDomain,
    pub code: &'static str,
    pub message: String,
    pub hint: Option<String>,
}

impl Diagnostic {
    pub fn info(domain: DiagnosticDomain, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            severity: DiagnosticSeverity::Info,
            domain,
            code,
            message: message.into(),
            hint: None,
        }
    }

    pub fn warning(
        domain: DiagnosticDomain,
        code: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity: DiagnosticSeverity::Warning,
            domain,
            code,
            message: message.into(),
            hint: None,
        }
    }

    pub fn error(domain: DiagnosticDomain, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            severity: DiagnosticSeverity::Error,
            domain,
            code,
            message: message.into(),
            hint: None,
        }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormatDiagnostics {
    pub subject: String,
    pub computed_schema_hash: u64,
    pub items: Vec<Diagnostic>,
}

impl FormatDiagnostics {
    pub fn new(subject: impl Into<String>, spec: FormatSpec) -> Self {
        Self {
            subject: subject.into(),
            computed_schema_hash: spec.computed_schema_hash(),
            items: Vec::new(),
        }
    }

    pub fn passed(&self) -> bool {
        !self
            .items
            .iter()
            .any(|item| item.severity == DiagnosticSeverity::Error)
    }

    pub fn warnings(&self) -> impl Iterator<Item = &Diagnostic> {
        self.items
            .iter()
            .filter(|item| item.severity == DiagnosticSeverity::Warning)
    }

    pub fn errors(&self) -> impl Iterator<Item = &Diagnostic> {
        self.items
            .iter()
            .filter(|item| item.severity == DiagnosticSeverity::Error)
    }

    fn push(&mut self, diagnostic: Diagnostic) {
        self.items.push(diagnostic);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelfTestStepStatus {
    Passed,
    Failed,
    Skipped,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelfTestStepReport {
    pub name: String,
    pub status: SelfTestStepStatus,
    pub domain: Option<DiagnosticDomain>,
    pub message: String,
    pub hint: Option<String>,
}

impl SelfTestStepReport {
    fn passed(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: SelfTestStepStatus::Passed,
            domain: None,
            message: message.into(),
            hint: None,
        }
    }

    fn skipped(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: SelfTestStepStatus::Skipped,
            domain: Some(DiagnosticDomain::CallerUsage),
            message: message.into(),
            hint: None,
        }
    }

    fn failed(
        name: impl Into<String>,
        domain: DiagnosticDomain,
        message: impl Into<String>,
        hint: Option<String>,
    ) -> Self {
        Self {
            name: name.into(),
            status: SelfTestStepStatus::Failed,
            domain: Some(domain),
            message: message.into(),
            hint,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormatSelfTestReport {
    pub diagnostics: FormatDiagnostics,
    pub steps: Vec<SelfTestStepReport>,
}

impl FormatSelfTestReport {
    pub fn passed(&self) -> bool {
        self.diagnostics.passed()
            && self
                .steps
                .iter()
                .all(|step| step.status != SelfTestStepStatus::Failed)
    }

    pub fn failures(&self) -> impl Iterator<Item = &SelfTestStepReport> {
        self.steps
            .iter()
            .filter(|step| step.status == SelfTestStepStatus::Failed)
    }
}

type Probe = Box<dyn Fn(&mut VarveFile) -> Result<()> + 'static>;

struct SelfTestCase {
    name: String,
    write: Option<Probe>,
    read: Option<Probe>,
}

pub struct FormatSelfTest {
    spec: FormatSpec,
    path: PathBuf,
    dims: Option<MatrixDimensions>,
    cleanup: bool,
    cases: Vec<SelfTestCase>,
    block_ordinals: HashMap<u32, usize>,
}

impl FormatSelfTest {
    pub fn new<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Self {
        Self {
            spec,
            path: path.as_ref().to_path_buf(),
            dims: None,
            cleanup: false,
            cases: Vec::new(),
            block_ordinals: HashMap::new(),
        }
    }

    pub fn with_dims(mut self, dims: MatrixDimensions) -> Self {
        self.dims = Some(dims);
        self
    }

    pub fn cleanup(mut self, enabled: bool) -> Self {
        self.cleanup = enabled;
        self
    }

    pub fn with_block<T>(mut self, value: T) -> Self
    where
        T: VarveBlock + Clone + PartialEq + Debug + 'static,
    {
        let ordinal = *self.block_ordinals.entry(T::ID).or_insert(0);
        *self.block_ordinals.entry(T::ID).or_insert(0) += 1;
        let write_value = value.clone();
        let expected = value;
        self.cases.push(SelfTestCase {
            name: format!("block {} ordinal {}", T::ID, ordinal),
            write: Some(Box::new(move |file| {
                file.push(&write_value)?;
                Ok(())
            })),
            read: Some(Box::new(move |file| {
                let actual = file.blocks::<T>()?.get(ordinal)?;
                if actual.as_ref() == Some(&expected) {
                    Ok(())
                } else {
                    Err(Error::InvalidFormatSpec(
                        "self-test block roundtrip mismatch",
                    ))
                }
            })),
        });
        self
    }

    pub fn with_keyed_block<T>(mut self, value: T) -> Self
    where
        T: VarveKeyedBlock + Clone + PartialEq + Debug + 'static,
        T::Key: Debug,
    {
        // API2-03: compile-time keyedness contract; the self-test write/read
        // closures below rely on `T` being genuinely keyed.
        let () = crate::traits::KeyedBlockContract::<T>::OK;
        let key = value.key();
        let write_value = value.clone();
        let expected = value;
        self.cases.push(SelfTestCase {
            name: format!("keyed block {} key {:?}", T::ID, key),
            write: Some(Box::new(move |file| {
                // API2-05: keyed self-test writes go through the maintaining
                // keyed path, so a keyed-chaining format is exercised with a
                // real predecessor chain rather than a truncated one.
                file.push_keyed(&write_value)?;
                Ok(())
            })),
            read: Some(Box::new(move |file| {
                let actual = file.keyed_blocks::<T>()?.get(&key)?;
                if actual.as_ref() == Some(&expected) {
                    Ok(())
                } else {
                    Err(Error::InvalidFormatSpec(
                        "self-test keyed block roundtrip mismatch",
                    ))
                }
            })),
        });
        self
    }

    pub fn with_matrix_cell<T>(mut self, key: MatrixKey, value: T) -> Self
    where
        T: VarveMatrixBlock + Clone + PartialEq + Debug + 'static,
    {
        let write_value = value.clone();
        let expected = value;
        self.cases.push(SelfTestCase {
            name: format!("matrix block {} key {:?}", T::ID, key),
            write: Some(Box::new(move |file| {
                file.write_matrix_cell(key, &write_value)?;
                file.commit_matrix_cell::<T>(key)?;
                Ok(())
            })),
            read: Some(Box::new(move |file| {
                let status = file.matrix_cell_status::<T>(key)?;
                if status != MatrixCellStatus::Committed {
                    return Err(Error::MatrixNotCommitted);
                }
                let actual = file.read_matrix_cell::<T>(key)?;
                if actual == expected {
                    Ok(())
                } else {
                    Err(Error::InvalidFormatSpec(
                        "self-test matrix cell roundtrip mismatch",
                    ))
                }
            })),
        });
        self
    }

    pub fn with_uncommitted_matrix_cell<T>(mut self, key: MatrixKey, value: T) -> Self
    where
        T: VarveMatrixBlock + Clone + Debug + 'static,
    {
        self.cases.push(SelfTestCase {
            name: format!("uncommitted matrix block {} key {:?}", T::ID, key),
            write: Some(Box::new(move |file| {
                file.write_matrix_cell(key, &value)?;
                Ok(())
            })),
            read: Some(Box::new(move |file| {
                let status = file.matrix_cell_status::<T>(key)?;
                if status != MatrixCellStatus::NotCommitted {
                    return Err(Error::InvalidFormatSpec(
                        "self-test expected matrix cell to be uncommitted",
                    ));
                }
                match file.read_matrix_cell::<T>(key) {
                    Err(Error::MatrixNotCommitted) => Ok(()),
                    Ok(_) => Err(Error::InvalidFormatSpec(
                        "self-test uncommitted matrix read succeeded",
                    )),
                    Err(error) => Err(error),
                }
            })),
        });
        self
    }

    pub fn with_matrix_aux(
        mut self,
        name: impl Into<String>,
        offset: u64,
        payload: impl Into<Vec<u8>>,
    ) -> Self {
        let name = name.into();
        let write_name = name.clone();
        let read_name = name.clone();
        let payload = payload.into();
        let expected = payload.clone();
        self.cases.push(SelfTestCase {
            name: format!(
                "matrix aux {name} range {offset}..{}",
                offset + payload.len() as u64
            ),
            write: Some(Box::new(move |file| {
                file.write_matrix_aux(&write_name, offset, &payload)
            })),
            read: Some(Box::new(move |file| {
                let actual = file.read_matrix_aux(&read_name, offset, expected.len() as u64)?;
                if actual == expected {
                    Ok(())
                } else {
                    Err(Error::InvalidFormatSpec("self-test matrix aux mismatch"))
                }
            })),
        });
        self
    }

    pub fn run(self) -> FormatSelfTestReport {
        let mut report = FormatSelfTestReport {
            diagnostics: diagnose_spec(self.spec),
            steps: Vec::new(),
        };
        if !report.diagnostics.passed() {
            report.steps.push(SelfTestStepReport::skipped(
                "create",
                "format diagnostics has errors; fix the spec before running IO probes",
            ));
            return report;
        }

        // API-01/API2-02: the self-test must never truncate or delete a
        // pre-existing caller file. Both paths rely on atomic
        // exclusive-create constructors that keep the exclusively created
        // handle locked from the claim through complete initialization, so
        // there is no window in which the claimed pathname is re-opened by
        // name and a concurrent swap could redirect a truncating constructor.
        // Cleanup below only ever removes objects this run proved it created.
        let mut writer = if self.spec.has_matrix_blocks() {
            let Some(dims) = self.dims.clone() else {
                report.steps.push(SelfTestStepReport::failed(
                    "create matrix file",
                    DiagnosticDomain::CallerUsage,
                    "matrix dimensions were not supplied to the self-test",
                    Some("call .with_dims(MatrixDimensions::from_pairs(...))".to_string()),
                ));
                return report;
            };
            let created = record_create_result(
                &mut report,
                "create matrix file",
                VarveFile::create_new_with_dims(self.spec, &self.path, dims),
            );
            let Some(writer) = created else {
                // Creation failed without leaving a handle that proves what
                // now sits at the pathname, so the target is left untouched;
                // only the marker the failed claim may have created is tidied
                // through the identity-checked protocol.
                remove_unowned_lock_marker(&mut report, &self.path);
                return report;
            };
            writer
        } else {
            let created = record_create_result(
                &mut report,
                "create file",
                VarveFile::create_new(self.spec, &self.path),
            );
            let Some(writer) = created else {
                // Creation failed without proving ownership of the path, so
                // the target is left untouched; only the marker the failed
                // claim may have created is tidied through the
                // identity-checked protocol.
                remove_unowned_lock_marker(&mut report, &self.path);
                return report;
            };
            writer
        };
        // API2-02: remember which file object this run created so cleanup can
        // refuse to delete a file another process has since swapped in at the
        // same pathname. If the identity cannot be captured, cleanup skips
        // the native file rather than guessing.
        let created_identity = writer.native_object_identity().ok();

        let mut write_failed = false;
        for case in &self.cases {
            if let Some(write) = &case.write {
                let name = format!("write {}", case.name);
                if run_probe(&mut report, name, || write(&mut writer)).is_err() {
                    write_failed = true;
                }
            }
        }

        if !write_failed {
            run_probe(&mut report, "flush", || writer.flush()).ok();
        }
        drop(writer);

        if write_failed {
            if self.cleanup {
                cleanup_run_artifacts(&mut report, &self.path, created_identity.as_deref());
            }
            return report;
        }

        let mut reader = record_result(
            &mut report,
            "open readonly",
            VarveFile::open_readonly(self.spec, &self.path),
        );
        let Some(mut reader) = reader.take() else {
            if self.cleanup {
                cleanup_run_artifacts(&mut report, &self.path, created_identity.as_deref());
            }
            return report;
        };

        for case in &self.cases {
            if let Some(read) = &case.read {
                let name = format!("read {}", case.name);
                run_probe(&mut report, name, || read(&mut reader)).ok();
            }
        }

        if self.cleanup {
            drop(reader);
            cleanup_run_artifacts(&mut report, &self.path, created_identity.as_deref());
        }
        report
    }
}

pub fn diagnose_spec(spec: FormatSpec) -> FormatDiagnostics {
    let mut report = FormatDiagnostics::new("format spec", spec);
    match spec.validate() {
        Ok(()) => report.push(Diagnostic::info(
            DiagnosticDomain::FormatDefinition,
            "format.spec.valid",
            "format spec validation passed",
        )),
        Err(error) => {
            let (domain, hint) = classify_error_with_hint(&error);
            report.push(
                Diagnostic::error(domain, "format.spec.invalid", error.to_string()).with_hint(hint),
            );
            return report;
        }
    }

    if spec.blocks.is_empty() && spec.matrix_blocks.is_empty() {
        report.push(
            Diagnostic::warning(
                DiagnosticDomain::FormatDefinition,
                "format.blocks.empty",
                "format has no registered user blocks",
            )
            .with_hint("register at least one fixed, variable, or matrix block"),
        );
    }

    if spec.schema_hash == 0 {
        report.push(
            Diagnostic::warning(
                DiagnosticDomain::FormatDefinition,
                "format.schema_hash.unpinned",
                "schema_hash is 0, so header-level schema pinning is disabled",
            )
            .with_hint(
                "use schema_hash: computed; or pin computed_schema_hash() before stable files",
            ),
        );
    } else if spec.schema_hash != spec.computed_schema_hash() {
        report.push(
            Diagnostic::warning(
                DiagnosticDomain::FormatDefinition,
                "format.schema_hash.literal_mismatch",
                "pinned schema_hash differs from computed_schema_hash()",
            )
            .with_hint("this is allowed except for format-contract compression, but check that the pinned value is intentional"),
        );
    }

    if matches!(
        spec.integrity_policy,
        crate::IntegrityPolicy::Crc32 | crate::IntegrityPolicy::Crc32WithHeader
    ) && !cfg!(feature = "integrity")
    {
        report.push(
            Diagnostic::error(
                DiagnosticDomain::FeatureGate,
                "feature.integrity.disabled",
                "format requests crc32 integrity but the integrity feature is not enabled",
            )
            .with_hint("enable the varve/integrity feature for this build"),
        );
    }

    if uses_zstd_compression(spec) && !cfg!(feature = "compression-zstd") {
        report.push(
            Diagnostic::error(
                DiagnosticDomain::FeatureGate,
                "feature.compression_zstd.disabled",
                "format can write zstd-compressed records but compression-zstd is not enabled",
            )
            .with_hint("enable the varve/compression-zstd feature or disable compression"),
        );
    }

    if spec.has_matrix_blocks() && spec.matrix_dimensions.is_empty() {
        report.push(
            Diagnostic::error(
                DiagnosticDomain::FormatDefinition,
                "matrix.dimensions.empty",
                "matrix blocks require declared dimensions",
            )
            .with_hint("declare dims { ... } in the format or provide matrix descriptors manually"),
        );
    }

    report.push(Diagnostic::info(
        DiagnosticDomain::FormatDefinition,
        "format.schema_hash.computed",
        format!(
            "computed schema hash is {:#018x}",
            spec.computed_schema_hash()
        ),
    ));
    report
}

pub fn diagnose_file<P: AsRef<Path>>(spec: FormatSpec, path: P) -> FormatDiagnostics {
    let path = path.as_ref();
    let mut report = diagnose_spec(spec);
    report.subject = format!("file {}", path.display());
    if !report.passed() {
        return report;
    }

    match VarveFile::inspect_writer_lock(path) {
        Ok(Some(lock)) => report.push(
            Diagnostic::warning(
                DiagnosticDomain::Environment,
                "file.writer_lock.present",
                format!("writer lock is present for process {:?}", lock.process_id),
            )
            .with_hint("snapshot reads may still work, but writes should wait for or explicitly handle the lock"),
        ),
        Ok(None) => {}
        Err(error) => {
            let (domain, hint) = classify_error_with_hint(&error);
            report.push(
                Diagnostic::warning(domain, "file.writer_lock.inspect_failed", error.to_string())
                    .with_hint(hint),
            );
        }
    }

    let file = match VarveFile::open_readonly(spec, path) {
        Ok(file) => file,
        Err(error) => {
            let (domain, hint) = classify_error_with_hint(&error);
            report.push(
                Diagnostic::error(domain, "file.open_readonly.failed", error.to_string())
                    .with_hint(hint),
            );
            return report;
        }
    };

    report.push(Diagnostic::info(
        DiagnosticDomain::FileData,
        "file.open_readonly.ok",
        "file opened with the supplied format spec",
    ));

    let mut materialization = MaterializationBudget::new(spec);
    for entry in file.index_entries() {
        let validation = (|| {
            let logical_len = entry.logical_payload_len_snapshot(spec, file.snapshot())?;
            materialization.consume(logical_len)?;
            entry
                .read_logical_payload_snapshot(spec, file.snapshot())
                .map(|_| ())
        })();
        if let Err(error) = validation {
            let (domain, hint) = classify_error_with_hint(&error);
            report.push(
                Diagnostic::error(
                    domain,
                    "file.record.payload_invalid",
                    format!(
                        "record block {} at offset {} failed logical payload validation: {}",
                        entry.block_id, entry.record_offset, error
                    ),
                )
                .with_hint(hint),
            );
        }
    }
    report.push(Diagnostic::info(
        DiagnosticDomain::FileData,
        "file.records.scanned",
        format!(
            "{} append-log records validated",
            file.index_entries().len()
        ),
    ));

    match file.schema_manifest() {
        Ok(Some(manifest)) => {
            if manifest.format_version != spec.version
                || manifest.endian != spec.endian
                || manifest.schema_hash != spec.schema_hash
            {
                report.push(
                    Diagnostic::warning(
                        DiagnosticDomain::FileData,
                        "file.manifest.mismatch",
                        "embedded manifest does not match the supplied static spec",
                    )
                    .with_hint(
                        "ensure the reader is using the exact format registry that wrote the file",
                    ),
                );
            } else {
                report.push(Diagnostic::info(
                    DiagnosticDomain::FileData,
                    "file.manifest.ok",
                    "embedded manifest matches the supplied static spec",
                ));
            }
        }
        Ok(None) if spec.manifest_policy == crate::ManifestPolicy::Embedded => report.push(
            Diagnostic::warning(
                DiagnosticDomain::FileData,
                "file.manifest.missing",
                "format requests embedded manifests but no manifest record was found",
            )
            .with_hint("older or manually written files may be missing diagnostic manifests"),
        ),
        Ok(None) => {}
        Err(error) => {
            let (domain, hint) = classify_error_with_hint(&error);
            report.push(
                Diagnostic::error(domain, "file.manifest.invalid", error.to_string())
                    .with_hint(hint),
            );
        }
    }

    if spec.has_matrix_blocks() {
        let matrix = file.matrix_recovery_report();
        for finding in &matrix.findings {
            report.push(
                Diagnostic::warning(
                    DiagnosticDomain::FileData,
                    "file.matrix.recovery_finding",
                    format!(
                        "matrix recovery finding {:?} with severity {:?}",
                        finding.kind, finding.severity
                    ),
                )
                .with_hint(
                    "inspect MatrixRecoveryReport and apply caller-approved recovery actions",
                ),
            );
        }
        report.push(Diagnostic::info(
            DiagnosticDomain::FileData,
            "file.matrix.recovery_report",
            format!(
                "{} matrix findings, {} recommended actions",
                matrix.findings.len(),
                matrix.recommended_actions.len()
            ),
        ));
    }

    report
}

pub fn classify_error(error: &Error) -> DiagnosticDomain {
    match error {
        Error::InvalidFormatSpec(_) | Error::ReservedBlockId(_) => {
            DiagnosticDomain::FormatDefinition
        }

        Error::UnregisteredBlock(_)
        | Error::BlockVersionMismatch { .. }
        | Error::BlockKindMismatch { .. }
        | Error::BlockSchemaFingerprintMismatch { .. }
        | Error::BlockKeyednessMismatch { .. }
        | Error::SchemaHashMismatch { .. }
        | Error::FormatVersionMismatch { .. }
        | Error::EndianMismatch { .. }
        | Error::ReplaceSizeMismatch { .. }
        | Error::ReplacementKeyMismatch
        | Error::MissingMergeTarget
        | Error::MigrationBlockIdMismatch { .. }
        | Error::MatrixDimensionsRequired
        | Error::MatrixDimensionMissing(_)
        | Error::MatrixDimensionMismatch { .. }
        | Error::MatrixBlockMissing(_)
        | Error::MatrixCommitMissing(_)
        | Error::MatrixKeyOutOfBounds { .. }
        | Error::MatrixAuxMissing(_)
        | Error::MatrixAuxOutOfBounds { .. }
        | Error::MatrixSizeMismatch { .. }
        | Error::MatrixNumericOutOfBounds { .. }
        | Error::MatrixNotCommitted
        | Error::MatrixCellNotWritten
        | Error::LayoutFieldMissing(_)
        | Error::LayoutFieldUnexpected(_)
        | Error::LayoutFieldTypeMismatch(_)
        | Error::LayoutSegmentMissing(_)
        | Error::LayoutRepeatedOnceSegment { .. }
        | Error::LayoutSegmentIndexOutOfBounds { .. }
        | Error::AdapterDiagnostic(_)
        | Error::InvalidAdapterExtension(_)
        | Error::SequenceExhausted
        | Error::MissingResourceLimit { .. }
        | Error::TrustedUnboundedRequiresExplicitApi { .. }
        | Error::ZeroCopyBlockKindMismatch { .. }
        | Error::ZeroCopyEndianMismatch { .. }
        | Error::ZeroCopyPayloadSizeMismatch { .. }
        | Error::ZeroCopyAlignmentMismatch { .. }
        | Error::KeyedChainRequiresKeyedApi { .. } => DiagnosticDomain::CallerUsage,

        #[cfg(feature = "high-cardinality-dev")]
        Error::StreamingUnsupported => DiagnosticDomain::FeatureGate,

        #[cfg(feature = "high-cardinality-dev")]
        Error::InvalidBatchOptions { .. } | Error::ScanCancelled { .. } => {
            DiagnosticDomain::CallerUsage
        }

        #[cfg(feature = "high-cardinality-dev")]
        Error::IndexBusy => DiagnosticDomain::Environment,

        Error::IntegrityFeatureDisabled | Error::CompressionFeatureDisabled => {
            DiagnosticDomain::FeatureGate
        }

        Error::Io(_)
        | Error::AllocationFailed { .. }
        | Error::WriterLockHeld(_)
        | Error::WriterLockMalformed(_)
        | Error::WriterPoisoned(_)
        | Error::WriteRollbackFailed { .. }
        | Error::PublishedButRebindFailed { .. }
        | Error::PublishedButParentSyncPending { .. }
        // The commit marker is in the file; only the durability request the
        // operating system was asked for failed.
        | Error::CommittedButDurabilityUnproven { .. }
        // The commit bit is in the file; only the durability request the
        // operating system was asked for failed (round 11).
        | Error::MatrixCommittedButDurabilityUnproven { .. }
        | Error::ReplacePublicationIndeterminate { .. } => DiagnosticDomain::Environment,

        #[cfg(feature = "high-cardinality-dev")]
        Error::PublishedButIndexStale { .. } => DiagnosticDomain::Environment,

        Error::InvalidMagic
        | Error::UnsupportedContainer
        | Error::UnsupportedEndian(_)
        | Error::UnexpectedEof
        | Error::LengthOverflow { .. }
        | Error::InvalidUtf8
        | Error::TrailingBytes { .. }
        | Error::InvalidCanonicalEncoding(_)
        | Error::LimitExceeded { .. }
        | Error::ResourceArithmeticOverflow { .. }
        | Error::SnapshotRangeOutOfBounds { .. }
        | Error::MissingField { .. }
        | Error::WireTypeMismatch { .. }
        | Error::UnknownWireType(_)
        | Error::CorruptTail { .. }
        | Error::InvalidIndexCheckpoint
        | Error::ChecksumMismatch { .. }
        | Error::UnsupportedCompressionAlgorithm(_)
        | Error::InvalidCompressionHeader
        | Error::InvalidChunkedBytes
        | Error::ChunkChecksumMismatch { .. }
        | Error::UnknownRecordFlags(_)
        | Error::InvalidRecordFooter { .. }
        | Error::InvalidCommitMarker { .. }
        | Error::DecompressedLengthMismatch { .. }
        | Error::DecompressedLengthLimitExceeded { .. }
        | Error::InvalidSchemaManifest
        | Error::MmapEntryNotInSnapshot
        | Error::MmapPayloadOutOfBounds { .. }
        | Error::InvalidMatrixSidecar
        | Error::MatrixSidecarMismatch(_)
        | Error::MatrixSidecarChecksumMismatch { .. }
        | Error::MatrixChecksumMismatch { .. }
        | Error::MatrixCommitQuarantined(_)
        | Error::MatrixFatalCorruption
        | Error::InvalidMatrixLayout
        | Error::LayoutLiteralMismatch { .. }
        | Error::LayoutAmbiguousSegment { .. }
        | Error::LayoutNoMatchingSegment { .. }
        | Error::LayoutTruncatedLeadIn { .. }
        | Error::LayoutTruncatedHeader { .. }
        | Error::LayoutInvalidSegmentBounds { .. }
        | Error::AdapterBounds { .. }
        | Error::AdapterUnsupportedType { .. }
        | Error::AdapterInvalidLength { .. } => DiagnosticDomain::FileData,

        #[cfg(feature = "high-cardinality-dev")]
        Error::DiskIndex(_) => DiagnosticDomain::FileData,

        Error::WriterLockBreakRefused(_) => DiagnosticDomain::CallerUsage,
        // F-04: the durable write itself succeeded; the caller's own hook is
        // what failed, so the fault is in caller-supplied code.
        Error::MatrixCommittedButHookFailed { .. } => DiagnosticDomain::CallerUsage,
        // F-07: the marker pathname is aliased or is not a regular file, which
        // is a property of the environment the file lives in.
        Error::WriterLockMarkerNotDedicated { .. } => DiagnosticDomain::Environment,
        Error::MatrixLayoutMissing => DiagnosticDomain::LibraryInvariant,
    }
}

pub fn error_hint(error: &Error) -> &'static str {
    // A stale or foreign sidecar has one documented recovery, so it earns a
    // variant-specific hint instead of the generic file-data one (STO-01).
    #[cfg(feature = "high-cardinality-dev")]
    if let Error::DiskIndex(disk_index) = error
        && matches!(
            **disk_index,
            crate::disk_index::DiskIndexError::PrimaryGenerationMismatch
                | crate::disk_index::DiskIndexError::IdentityMismatch
        )
    {
        return "the sidecar does not describe this generation of the primary file; \
                republish it with rebuild_disk_index (never trust it)";
    }
    match classify_error(error) {
        DiagnosticDomain::FormatDefinition => "inspect the format declaration and generated schema",
        DiagnosticDomain::CallerUsage => {
            "check the API call, block type, key, dimensions, or commit state"
        }
        DiagnosticDomain::FileData => {
            "the file bytes do not match the supplied format or failed integrity checks"
        }
        DiagnosticDomain::Environment => {
            "check filesystem access, writer locks, and concurrent processes"
        }
        DiagnosticDomain::FeatureGate => {
            "enable the required Cargo feature or disable the policy that needs it"
        }
        DiagnosticDomain::LibraryInvariant => {
            "this suggests an internal invariant failure; minimize the case and report it"
        }
    }
}

fn classify_error_with_hint(error: &Error) -> (DiagnosticDomain, String) {
    (classify_error(error), error_hint(error).to_string())
}

fn uses_zstd_compression(spec: FormatSpec) -> bool {
    let global = matches!(
        spec.compression_policy,
        crate::CompressionPolicy::VariableBlocks(compression)
            if compression.algorithm == crate::CompressionAlgorithm::Zstd
    );
    let block = spec
        .block_compression
        .iter()
        .any(|descriptor| descriptor.compression.algorithm == crate::CompressionAlgorithm::Zstd);
    global || block
}

/// Like [`record_result`], but reports an already-existing target path as a
/// caller-usage failure with a hint instead of a generic environment error.
fn record_create_result<T>(
    report: &mut FormatSelfTestReport,
    name: impl Into<String>,
    result: Result<T>,
) -> Option<T> {
    match result {
        Err(Error::Io(error)) if error.kind() == ErrorKind::AlreadyExists => {
            report.steps.push(SelfTestStepReport::failed(
                name,
                DiagnosticDomain::CallerUsage,
                format!(
                    "target path already exists; the self-test never truncates or deletes pre-existing files: {error}"
                ),
                Some("pass a fresh temporary path that does not exist yet".to_string()),
            ));
            None
        }
        other => record_result(report, name, other),
    }
}

fn record_result<T>(
    report: &mut FormatSelfTestReport,
    name: impl Into<String>,
    result: Result<T>,
) -> Option<T> {
    let name = name.into();
    match result {
        Ok(value) => {
            report.steps.push(SelfTestStepReport::passed(name, "ok"));
            Some(value)
        }
        Err(error) => {
            let (domain, hint) = classify_error_with_hint(&error);
            report.steps.push(SelfTestStepReport::failed(
                name,
                domain,
                error.to_string(),
                Some(hint),
            ));
            None
        }
    }
}

fn run_probe<F>(report: &mut FormatSelfTestReport, name: impl Into<String>, run: F) -> Result<()>
where
    F: FnOnce() -> Result<()>,
{
    let name = name.into();
    match run() {
        Ok(()) => {
            report.steps.push(SelfTestStepReport::passed(name, "ok"));
            Ok(())
        }
        Err(error) => {
            let (mut domain, mut hint) = classify_error_with_hint(&error);
            if matches!(
                error,
                Error::InvalidFormatSpec(
                    "self-test block roundtrip mismatch"
                        | "self-test keyed block roundtrip mismatch"
                        | "self-test matrix cell roundtrip mismatch"
                        | "self-test matrix aux mismatch"
                        | "self-test expected matrix cell to be uncommitted"
                        | "self-test uncommitted matrix read succeeded"
                )
            ) {
                domain = DiagnosticDomain::LibraryInvariant;
                hint = "if this uses a custom codec, verify that codec first; otherwise this is a likely Varve bug".to_string();
            }
            report.steps.push(SelfTestStepReport::failed(
                name,
                domain,
                error.to_string(),
                Some(hint),
            ));
            Err(error)
        }
    }
}

/// Outcome of an identity-checked destructive removal (API2-02, F-05).
///
/// Cleanup is a destructive operation performed on behalf of a caller who
/// asked for tidiness, never for correctness, so every way it can decline is a
/// distinct, reportable value rather than a silent `return`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ObjectRemoval {
    /// The pathname still named the expected object, and that object is gone.
    Removed,
    /// The pathname did not name the expected object; nothing was deleted.
    NotOwned,
    /// Deleting by this pathname could not be made safe on this platform and
    /// in this directory, so nothing was deleted. The payload is the reason.
    ///
    /// Only the POSIX removal path can produce this: Windows deletes through
    /// the same verified handle it checked, so it never has to decline for
    /// want of a safe pathname. The variant stays in the shared enum so the
    /// reporting code above is identical on both platforms.
    #[cfg_attr(windows, allow(dead_code))]
    Refused(&'static str),
    /// The removal was attempted on the verified object and failed.
    Failed,
}

/// Removes the artifacts this self-test run created, verifying identity
/// before every deletion (API2-02, F-05).
///
/// The native target is only removed while the pathname still resolves to the
/// file object this run created; a file another process has since swapped in
/// at the same pathname is left untouched. On platforms without an
/// unlink-by-handle primitive the removal is additionally confined to a
/// directory no other unprivileged principal can insert names into, and is
/// *refused* — with a typed, reported reason — rather than performed through an
/// unverifiable pathname. The writer-lock marker is only removed after
/// re-acquiring it through the standard writer-lock protocol, so a marker
/// locked or populated by a foreign writer survives.
fn cleanup_run_artifacts(
    report: &mut FormatSelfTestReport,
    path: &Path,
    created_identity: Option<&[u8]>,
) {
    if let Some(identity) = created_identity {
        match remove_path_if_same_object(path, identity) {
            ObjectRemoval::Removed | ObjectRemoval::NotOwned => {}
            ObjectRemoval::Refused(reason) => {
                report.steps.push(SelfTestStepReport::failed(
                    "cleanup",
                    DiagnosticDomain::Environment,
                    format!("refused to delete {} by pathname: {reason}", path.display()),
                    Some(
                        "run the self-test inside a directory only this user can write, \
                         or call .cleanup(false) and remove the artifact yourself"
                            .to_string(),
                    ),
                ));
            }
            ObjectRemoval::Failed => {
                report.steps.push(SelfTestStepReport::failed(
                    "cleanup",
                    DiagnosticDomain::Environment,
                    format!("failed to delete the self-test artifact {}", path.display()),
                    Some("remove the leftover artifact before rerunning".to_string()),
                ));
            }
        }
    }
    remove_unowned_lock_marker(report, path);
}

/// Deletes `path` only while the file object bound to the pathname is still
/// `expected_identity`.
///
/// The handle is opened without delete or write sharing, which pins the
/// pathname for the whole check-and-delete: no other process can delete or
/// rename over the name while the handle is open, and the deletion itself is
/// issued on that same verified handle, so the identity check cannot be
/// invalidated by a concurrent pathname swap.
#[cfg(windows)]
pub(crate) fn remove_path_if_same_object(path: &Path, expected_identity: &[u8]) -> ObjectRemoval {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_DISPOSITION_INFO, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FileDispositionInfo,
        SetFileInformationByHandle,
    };

    let Ok(file) = std::fs::OpenOptions::new()
        .access_mode(DELETE | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ)
        .open(path)
    else {
        return ObjectRemoval::NotOwned;
    };
    match crate::file::opened_file_identity(&file) {
        Ok(identity) if identity == expected_identity => {}
        _ => return ObjectRemoval::NotOwned,
    }
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: the handle stays open with DELETE access for the duration of
    // the call and the info pointer references a live FILE_DISPOSITION_INFO
    // of the size passed alongside it.
    let status = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle() as HANDLE,
            FileDispositionInfo,
            (&raw const disposition).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
    if status == 0 {
        ObjectRemoval::Failed
    } else {
        ObjectRemoval::Removed
    }
}

/// Deletes `path` only while the file object bound to the pathname is still
/// `expected_identity` (F-05).
///
/// POSIX has no unlink-by-handle: `unlink` names a *pathname*, so an identity
/// check on an open handle followed by an unlink of the same name is two
/// resolutions of a name another principal may rebind in between. Varve's Unix
/// writer locks are advisory and do not stop a non-cooperating pathname
/// mutator, so the previous code's acknowledgement of that interval was not a
/// defence. This version closes it with two independent measures:
///
/// 1. **Exclusive directory.** The parent directory is opened once, and every
///    later operation is issued *relative to that directory handle* rather than
///    by re-resolving the pathname, so no swap of a directory component can
///    redirect them. Removal proceeds only when that directory object is owned
///    by this effective user and grants insert/rename rights to no one else —
///    either because group and other have no write permission, or because the
///    sticky bit reserves renaming and unlinking of an existing entry to that
///    entry's owner. In any other directory the deletion is *refused*, not
///    performed unverified.
/// 2. **Re-verified identity.** Within that directory the object is opened
///    (without following a final symlink), its identity checked, then opened
///    and checked once more immediately before `unlinkat`, so an interposition
///    that beats the first check still has to beat the second.
///
/// The residual boundary is a principal that can already write the exclusively
/// owned directory — the same user, or root — which is outside Varve's threat
/// model because it can rewrite the artifact's contents anyway.
#[cfg(not(windows))]
pub(crate) fn remove_path_if_same_object(path: &Path, expected_identity: &[u8]) -> ObjectRemoval {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::AsRawFd;

    let Some(name) = path.file_name() else {
        return ObjectRemoval::Refused("the self-test pathname has no final component");
    };
    let Ok(name) = CString::new(name.as_bytes()) else {
        return ObjectRemoval::Refused("the self-test file name contains an interior NUL");
    };
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    let parent = parent.unwrap_or_else(|| Path::new("."));
    let Ok(directory) = std::fs::File::open(parent) else {
        return ObjectRemoval::Failed;
    };
    if let Err(reason) = directory_is_exclusively_owned(&directory) {
        return ObjectRemoval::Refused(reason);
    }

    let verify = || match open_at_identity(&directory, &name) {
        Some(identity) if identity == expected_identity => Some(true),
        Some(_) => Some(false),
        None => None,
    };
    match verify() {
        Some(true) => {}
        Some(false) | None => return ObjectRemoval::NotOwned,
    }
    #[cfg(test)]
    interpose_before_unlink();
    // The directory admits no foreign names, so this re-check can only fail
    // for a cooperating actor; it is kept because an identity check that is
    // not the last thing before the deletion is not an identity check.
    match verify() {
        Some(true) => {}
        Some(false) | None => return ObjectRemoval::NotOwned,
    }
    // SAFETY: `directory` is an open directory descriptor that outlives the
    // call and `name` is a NUL-terminated single path component.
    let status = unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) };
    if status == 0 {
        ObjectRemoval::Removed
    } else {
        ObjectRemoval::Failed
    }
}

/// Reads the identity of the entry `name` inside the already-opened directory
/// `directory`, without re-resolving any part of the pathname and without
/// following a final symlink.
#[cfg(not(windows))]
fn open_at_identity(directory: &std::fs::File, name: &std::ffi::CStr) -> Option<Vec<u8>> {
    use std::os::unix::io::{AsRawFd, FromRawFd};

    // SAFETY: `directory` is an open directory descriptor that outlives the
    // call and `name` is a NUL-terminated single path component.
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        return None;
    }
    // SAFETY: `openat` returned a fresh owned descriptor that nothing else
    // holds, so `File` takes sole ownership of it and closes it on drop.
    let file = unsafe { std::fs::File::from_raw_fd(descriptor) };
    crate::file::opened_file_identity(&file).ok()
}

/// Reports whether `directory` is a directory only this effective user can
/// bind names in.
///
/// Rename and unlink of an existing entry require write permission on the
/// directory, so a directory that grants no write permission to group or other
/// cannot have its entries replaced by another unprivileged principal. A
/// world-writable directory with the sticky bit set is equally safe for an
/// entry this run owns, because the sticky bit reserves renaming and unlinking
/// of an entry to that entry's owner — which is why the ordinary shared
/// temporary directory is not refused.
#[cfg(not(windows))]
fn directory_is_exclusively_owned(
    directory: &std::fs::File,
) -> std::result::Result<(), &'static str> {
    use std::os::unix::fs::MetadataExt;

    let Ok(metadata) = directory.metadata() else {
        return Err("the parent directory's ownership could not be read");
    };
    if !metadata.is_dir() {
        return Err("the self-test artifact's parent is not a directory");
    }
    // SAFETY: `geteuid` reads process state, takes no arguments and cannot
    // fail.
    let effective_user = unsafe { libc::geteuid() };
    if metadata.uid() != effective_user && metadata.uid() != 0 {
        return Err("the parent directory is owned by another user");
    }
    // Numeric POSIX mode bits rather than the `libc` constants, whose integer
    // width differs between Unix targets: group write, other write, sticky.
    let mode = u64::from(metadata.mode());
    let shared_write = mode & 0o022 != 0;
    let sticky = mode & 0o1000 != 0;
    if shared_write && !sticky {
        return Err("the parent directory is writable by other users and is not sticky");
    }
    Ok(())
}

/// Test-only interposition point between the identity check and the deletion
/// (F-05). Set by [`set_interposition`] in unit tests that model a hostile
/// pathname swap landing in exactly that interval.
#[cfg(all(test, not(windows)))]
fn interpose_before_unlink() {
    let hook = INTERPOSITION.lock().expect("interposition hook").take();
    if let Some(hook) = hook {
        hook();
    }
}

#[cfg(all(test, not(windows)))]
static INTERPOSITION: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>> =
    std::sync::Mutex::new(None);

#[cfg(all(test, not(windows)))]
fn set_interposition(hook: Box<dyn FnOnce() + Send>) {
    *INTERPOSITION.lock().expect("interposition hook") = Some(hook);
}

/// Removes the pathname's writer-lock marker only when this process can
/// re-acquire it through the standard writer-lock protocol (API2-02).
///
/// Acquisition with the default refuse policy fails while another writer
/// holds the marker guard lock or has populated the marker with its own run
/// token, so only a quiescent, unowned marker is ever deleted. The native
/// single-writer object lock stays authoritative regardless, so losing a
/// marker to this race can never admit a second writer.
/// Removes the writer-lock marker this run may have left behind.
///
/// API3-03: this used to unlink `<path>.lock` by pathname, guarded only by
/// re-acquiring the writer lock. On Unix that is the *same* shape F-05 fixed
/// for the artifact itself: Varve's Unix writer locks are advisory, so holding
/// one does not stop a non-cooperating principal that can bind names in the
/// parent directory from substituting a different object at the marker's name
/// before the unlink. Leaving one destructive path in this module outside the
/// hardened protocol would have made the F-05 fix incomplete.
///
/// The marker's identity is captured from an open handle and the deletion is
/// then issued through [`remove_path_if_same_object`], which on Unix confines
/// every operation to a directory-handle-relative `unlinkat` in a directory
/// this effective user exclusively controls, and on Windows deletes through
/// the same non-delete-shared handle it verified. A refusal or a swapped
/// pathname leaves the marker in place; that is a leftover file, never the
/// deletion of somebody else's object.
///
/// The outcome is reported so a refusal is visible rather than silent
/// (invariant: no destructive step declines without a typed outcome).
fn remove_unowned_lock_marker(report: &mut FormatSelfTestReport, path: &Path) {
    let mut lock = OsString::from(path.as_os_str());
    lock.push(".lock");
    let marker = PathBuf::from(lock);
    if !marker.exists() {
        // Avoid recreating a marker that is already gone: acquisition below
        // would create one just to delete it again.
        return;
    }
    // F-08: a failure to re-acquire the marker used to `return` silently, so a
    // self-test could report `passed = true` with zero cleanup failures while a
    // live 20-byte marker survived and the *next* run was refused by it. The
    // marker is deliberately still preserved - re-acquisition failing is
    // exactly the evidence that it may not be ours to delete - but the run no
    // longer claims to have left the directory clean.
    let guard = match crate::file::WriterLock::acquire(path) {
        Ok(guard) => guard,
        Err(error) => {
            report.steps.push(SelfTestStepReport::failed(
                "cleanup",
                DiagnosticDomain::Environment,
                format!(
                    "could not re-acquire the writer-lock marker {} to remove it, so it was left \
                     in place: {error}",
                    marker.display()
                ),
                Some(
                    "another writer may hold this path; remove the leftover .lock marker \
                     yourself before rerunning"
                        .to_string(),
                ),
            ));
            return;
        }
    };
    // The identity is captured while the writer lock is held, so it names the
    // marker object of *this* claim. The lock is then released before the
    // removal, because the lock itself holds the marker open and the hardened
    // removal deliberately opens without delete sharing. Releasing first is
    // safe for the property that matters: the removal re-verifies the
    // identity, so the worst case is that a marker object which is still the
    // same object is unlinked - a marker any writer recreates on demand -
    // never the deletion of a different object bound at the same name.
    let identity = std::fs::File::open(&marker)
        .ok()
        .and_then(|handle| crate::file::opened_file_identity(&handle).ok());
    drop(guard);
    let outcome = identity.map(|identity| remove_path_if_same_object(&marker, &identity));
    match outcome {
        Some(ObjectRemoval::Removed) | Some(ObjectRemoval::NotOwned) => {}
        // F-08: the identity could not be captured, so the marker was left
        // untouched. Same rule as the acquisition failure above - preserve it,
        // but do not report a clean run.
        None => {
            report.steps.push(SelfTestStepReport::failed(
                "cleanup",
                DiagnosticDomain::Environment,
                format!(
                    "could not identify the writer-lock marker {}, so it was left in place",
                    marker.display()
                ),
                Some("remove the leftover .lock marker before rerunning".to_string()),
            ));
        }
        Some(ObjectRemoval::Refused(reason)) => {
            report.steps.push(SelfTestStepReport::failed(
                "cleanup",
                DiagnosticDomain::Environment,
                format!(
                    "refused to delete the writer-lock marker {}: {reason}",
                    marker.display()
                ),
                Some(
                    "run the self-test inside a directory only this user can write,                      or remove the leftover .lock marker yourself"
                        .to_string(),
                ),
            ));
        }
        Some(ObjectRemoval::Failed) => {
            report.steps.push(SelfTestStepReport::failed(
                "cleanup",
                DiagnosticDomain::Environment,
                format!(
                    "failed to delete the writer-lock marker {}",
                    marker.display()
                ),
                Some("remove the leftover .lock marker before rerunning".to_string()),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// F-05. Cleanup must never delete an object it did not verify. This
    /// interposes a hostile pathname swap in the one interval the old code
    /// left open — after the identity check and before the deletion — and
    /// asserts that the substituted file survives.
    ///
    /// The probe runs inside a private temporary directory, i.e. a directory
    /// the removal path accepts as exclusively owned, so what is under test is
    /// the re-verified identity rather than the directory refusal.
    #[test]
    #[cfg(not(windows))]
    fn cleanup_does_not_delete_a_file_interposed_before_the_unlink() {
        const IMPOSTOR: &[u8] = b"not the self-test artifact";

        let directory = tempfile::tempdir().expect("private temp directory");
        let path = directory.path().join("interposed.vrv");
        std::fs::write(&path, b"created by this run").expect("create the artifact");
        let identity = crate::file::opened_file_identity(
            &std::fs::File::open(&path).expect("open the created artifact"),
        )
        .expect("identity of the created artifact");

        let target = path.clone();
        set_interposition(Box::new(move || {
            // A non-cooperating actor rebinds the pathname to a different
            // file object between the check and the deletion.
            std::fs::remove_file(&target).expect("unlink the verified object");
            std::fs::write(&target, IMPOSTOR).expect("bind an impostor to the pathname");
        }));

        assert_eq!(
            remove_path_if_same_object(&path, &identity),
            ObjectRemoval::NotOwned,
        );
        assert_eq!(
            std::fs::read(&path).expect("the interposed file must survive"),
            IMPOSTOR,
        );
    }

    /// Without interposition the same call removes the verified object, so the
    /// test above is proving a refusal rather than a broken cleanup path.
    #[test]
    fn cleanup_removes_the_verified_object() {
        let directory = tempfile::tempdir().expect("private temp directory");
        let path = directory.path().join("owned.vrv");
        std::fs::write(&path, b"created by this run").expect("create the artifact");
        let identity = crate::file::opened_file_identity(
            &std::fs::File::open(&path).expect("open the created artifact"),
        )
        .expect("identity of the created artifact");

        assert_eq!(
            remove_path_if_same_object(&path, &identity),
            ObjectRemoval::Removed,
        );
        assert!(!path.exists());
    }

    /// A pathname that no longer names the created object is left alone, and
    /// is reported as such rather than as a successful removal.
    #[test]
    fn cleanup_leaves_a_swapped_pathname_alone() {
        let directory = tempfile::tempdir().expect("private temp directory");
        let path = directory.path().join("swapped.vrv");
        std::fs::write(&path, b"created by this run").expect("create the artifact");
        let identity = crate::file::opened_file_identity(
            &std::fs::File::open(&path).expect("open the created artifact"),
        )
        .expect("identity of the created artifact");
        std::fs::remove_file(&path).expect("unlink the created object");
        std::fs::write(&path, b"someone else's file").expect("bind another object");

        assert_eq!(
            remove_path_if_same_object(&path, &identity),
            ObjectRemoval::NotOwned,
        );
        assert!(path.exists());
    }

    /// API3-03. The writer-lock marker is the one destructive path in this
    /// module that used to unlink by pathname, guarded only by re-acquiring an
    /// advisory writer lock. It now goes through the same identity-checked,
    /// directory-confined removal as the artifact, so an object that is not
    /// the marker this run opened must survive.
    ///
    /// This is a differential test of the primitive the call site now uses:
    /// against the old `remove_file(&marker)`, which named a pathname and
    /// verified nothing, the substituted object is deleted. That the call site
    /// still removes the marker it genuinely opened is asserted end to end by
    /// `crates/varve/tests/self_check.rs::self_test_cleanup_removes_files_created_by_the_run`
    /// and by the two `self_test_never_truncates_*` tests.
    #[test]
    fn lock_marker_removal_leaves_a_swapped_object_alone() {
        let directory = tempfile::tempdir().expect("private temp directory");
        let path = directory.path().join("marker-swap.vrv");
        let marker = directory.path().join("marker-swap.vrv.lock");
        std::fs::write(&path, b"artifact").expect("create the artifact");
        std::fs::write(&marker, b"someone else's file").expect("bind a foreign object");

        // The removal runs through the real call site, which captures the
        // marker's identity from its own open handle and then removes only
        // that object. Interposition is simulated by handing the identity of a
        // *different* object to the hardened primitive the call site uses -
        // the state a pathname rebind between capture and unlink produces.
        let elsewhere = directory.path().join("elsewhere");
        std::fs::write(&elsewhere, b"a different object").expect("create another object");
        let foreign = crate::file::opened_file_identity(
            &std::fs::File::open(&elsewhere).expect("open the other object"),
        )
        .expect("identity of the other object");

        assert_eq!(
            remove_path_if_same_object(&marker, &foreign),
            ObjectRemoval::NotOwned,
        );
        assert!(marker.exists(), "the substituted object must survive");
        assert_eq!(
            std::fs::read(&marker).expect("read the surviving object"),
            b"someone else's file",
        );
    }

    /// F-05. In a directory other unprivileged users can bind names in, the
    /// deletion is refused with a typed reason instead of being performed
    /// through a pathname that cannot be verified.
    #[test]
    #[cfg(not(windows))]
    fn cleanup_refuses_a_shared_writable_directory() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("private temp directory");
        let shared = directory.path().join("shared");
        std::fs::create_dir(&shared).expect("create the shared directory");
        // World-writable *without* the sticky bit: any local user could
        // rename over an entry here.
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777))
            .expect("relax the directory permissions");
        let path = shared.join("artifact.vrv");
        std::fs::write(&path, b"created by this run").expect("create the artifact");
        let identity = crate::file::opened_file_identity(
            &std::fs::File::open(&path).expect("open the created artifact"),
        )
        .expect("identity of the created artifact");

        assert!(matches!(
            remove_path_if_same_object(&path, &identity),
            ObjectRemoval::Refused(_),
        ));
        assert!(path.exists(), "a refused cleanup must delete nothing");
    }
}
