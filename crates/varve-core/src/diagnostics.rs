use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt::Debug;
use std::fs::remove_file;
use std::path::{Path, PathBuf};

use crate::{
    Error, FormatSpec, MatrixCellStatus, MatrixDimensions, MatrixKey, Result, VarveBlock,
    VarveFile, VarveKeyedBlock, VarveMatrixBlock,
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
        let key = value.key();
        let write_value = value.clone();
        let expected = value;
        self.cases.push(SelfTestCase {
            name: format!("keyed block {} key {:?}", T::ID, key),
            write: Some(Box::new(move |file| {
                file.push(&write_value)?;
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

        let mut writer = if self.spec.has_matrix_blocks() {
            match self.dims.clone() {
                Some(dims) => record_result(
                    &mut report,
                    "create matrix file",
                    VarveFile::create_with_dims(self.spec, &self.path, dims),
                ),
                None => {
                    report.steps.push(SelfTestStepReport::failed(
                        "create matrix file",
                        DiagnosticDomain::CallerUsage,
                        "matrix dimensions were not supplied to the self-test",
                        Some("call .with_dims(MatrixDimensions::from_pairs(...))".to_string()),
                    ));
                    return report;
                }
            }
        } else {
            record_result(
                &mut report,
                "create file",
                VarveFile::create(self.spec, &self.path),
            )
        };

        let Some(mut writer) = writer.take() else {
            if self.cleanup {
                cleanup_path(&self.path);
            }
            return report;
        };

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
                cleanup_path(&self.path);
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
                cleanup_path(&self.path);
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
            cleanup_path(&self.path);
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

    if spec.integrity_policy == crate::IntegrityPolicy::Crc32 && !cfg!(feature = "integrity") {
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

    for entry in file.index_entries() {
        if let Err(error) = entry.read_logical_payload(spec, path) {
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
        | Error::SchemaHashMismatch { .. }
        | Error::FormatVersionMismatch { .. }
        | Error::EndianMismatch { .. }
        | Error::ReplaceSizeMismatch { .. }
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
        | Error::ZeroCopyBlockKindMismatch { .. }
        | Error::ZeroCopyEndianMismatch { .. }
        | Error::ZeroCopyPayloadSizeMismatch { .. }
        | Error::ZeroCopyAlignmentMismatch { .. } => DiagnosticDomain::CallerUsage,

        Error::IntegrityFeatureDisabled | Error::CompressionFeatureDisabled => {
            DiagnosticDomain::FeatureGate
        }

        Error::Io(_) | Error::WriterLockHeld(_) | Error::WriterLockMalformed(_) => {
            DiagnosticDomain::Environment
        }

        Error::InvalidMagic
        | Error::UnsupportedContainer
        | Error::UnsupportedEndian(_)
        | Error::UnexpectedEof
        | Error::LengthOverflow { .. }
        | Error::InvalidUtf8
        | Error::TrailingBytes { .. }
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
        | Error::InvalidMatrixLayout
        | Error::LayoutLiteralMismatch { .. }
        | Error::LayoutAmbiguousSegment { .. }
        | Error::LayoutNoMatchingSegment { .. }
        | Error::LayoutTruncatedLeadIn { .. }
        | Error::LayoutTruncatedHeader { .. }
        | Error::LayoutInvalidSegmentBounds { .. } => DiagnosticDomain::FileData,

        Error::WriterLockBreakRefused(_) => DiagnosticDomain::CallerUsage,
        Error::MatrixLayoutMissing => DiagnosticDomain::LibraryInvariant,
    }
}

pub fn error_hint(error: &Error) -> &'static str {
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

fn cleanup_path(path: &Path) {
    let _ = remove_file(path);
    let mut lock = OsString::from(path.as_os_str());
    lock.push(".lock");
    let _ = remove_file(PathBuf::from(lock));
}
