use std::path::Path;

use super::adapter::{
    GROUP_PATH, TdmsPropertyValue, TdmsState, channel_path, inspect_example, load_tdms_state,
};
use super::example_data::{expected_chunk_count, expected_tdms_channels};
use varve::{AdapterCheckStatus, AdapterInputFile};

pub fn read_and_verify(path: &Path) -> varve::Result<()> {
    let adapter_report = inspect_example(path)?;
    if adapter_report.status() == AdapterCheckStatus::Failed {
        return Err(varve::Error::AdapterDiagnostic(
            "TDMS example adapter inspection failed",
        ));
    }

    let input = AdapterInputFile::from_path(path);
    let report = load_tdms_state(input.path())?;
    match report.state.property("/", "title") {
        TdmsPropertyValue::String(title) if title == "npTDMS scalar type matrix smoke" => {
            let appended = report
                .state
                .property_opt(channel_path("Float64"), "append_source")
                .is_some();
            assert_eq!(report.chunk_count, expected_chunk_count(true, appended));
            assert_eq!(
                report.state.property(GROUP_PATH, "operator"),
                TdmsPropertyValue::String("Ada".to_string())
            );
            assert_eq!(
                report.state.property(GROUP_PATH, "verified"),
                TdmsPropertyValue::Bool(true)
            );
            assert_eq!(
                report
                    .state
                    .property(channel_path("Boolean"), "unit_string"),
                TdmsPropertyValue::String("Boolean".to_string())
            );
            if appended {
                assert_eq!(
                    report
                        .state
                        .property(channel_path("Float64"), "append_source"),
                    TdmsPropertyValue::String("varve-open-layout-writer".to_string())
                );
            }
            assert_tdms_matrix(&report.state, false, false, appended);
        }
        TdmsPropertyValue::String(title) if title == "Varve TDMS adapter proof smoke" => {
            let appended = report
                .state
                .property_opt(channel_path("Float64"), "append_source")
                .is_some();
            assert_eq!(report.chunk_count, expected_chunk_count(false, appended));
            assert_eq!(
                report
                    .state
                    .property(channel_path("Float64"), "unit_string"),
                TdmsPropertyValue::String("Float64".to_string())
            );
            assert_eq!(
                report
                    .state
                    .property(channel_path("Boolean"), "adapter_enabled"),
                TdmsPropertyValue::Bool(true)
            );
            assert_eq!(
                report
                    .state
                    .property(channel_path("String"), "sample_count"),
                TdmsPropertyValue::I64(if appended { 5 } else { 4 })
            );
            assert_eq!(
                report
                    .state
                    .property(channel_path("String"), "segment_note"),
                TdmsPropertyValue::String("same raw index reused".to_string())
            );
            if appended {
                assert_eq!(
                    report
                        .state
                        .property(channel_path("Float64"), "append_source"),
                    TdmsPropertyValue::String("varve-open-layout-writer".to_string())
                );
            }
            assert_tdms_matrix(&report.state, true, true, appended);
        }
        other => panic!("unexpected TDMS example title {other:?}"),
    }
    Ok(())
}

pub fn read_and_verify_bytes(bytes: &[u8]) -> varve::Result<()> {
    let input = AdapterInputFile::from_bytes("tdms", bytes)?;
    read_and_verify(input.path())
}

fn assert_tdms_matrix(
    state: &TdmsState,
    include_unit_channels: bool,
    include_same_index_segment: bool,
    appended: bool,
) {
    let expected =
        expected_tdms_channels(include_unit_channels, include_same_index_segment, appended)
            .expect("expected TDMS channel matrix");
    for channel in expected {
        assert_eq!(state.raw_values(channel_path(channel.name)), channel.values);
    }
}
