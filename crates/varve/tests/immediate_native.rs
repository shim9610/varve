use varve::{ImmediatePolicy, VarveBlock, varve_format};

varve_format! {
    pub format NativeImmediate {
        magic: b"IMMN";
        version: 1;
        schema_hash: computed;
        index: scan_on_open;
        limits {
            file_len: 1_048_576;
            records: 10_000;
            index_bytes: 1_048_576;
            scan_bytes: 1_048_576;
            record_payload: 1024;
            logical_payload: 1024;
            materialized_bytes: 1_048_576;
            segments: 10_000;
        }
        blocks {
            fixed NativeSample(id = 1, immediate_if = native_condition) { value: u64 }
            fixed NativeBarrier(id = 2, durability = immediate) { value: u64 }
        }
    }
}

fn native_condition(value: &NativeSample) -> bool {
    value.value == 42
}

#[derive(varve::VarveBlock)]
#[varve(id = 10, durability = "immediate")]
struct DerivedBarrier {
    value: u64,
}

#[test]
fn declarations_and_native_writer_api_work_without_scalable_features() -> varve::Result<()> {
    assert!(std::hint::black_box(
        <NativeBarrier as VarveBlock>::IMMEDIATE
    ));
    assert!(std::hint::black_box(
        <DerivedBarrier as VarveBlock>::IMMEDIATE
    ));
    assert!(NativeSample { value: 42 }.immediate_if());
    assert!(!NativeSample { value: 0 }.immediate_if());
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("native.varve");
    let mut writer = NativeImmediate::create_writer(&path)?;
    writer.set_immediate_policy(ImmediatePolicy::new().after_records(2))?;
    writer.push_native_sample(&NativeSample { value: 0 })?;
    writer.push_native_sample(&NativeSample { value: 42 })?;
    writer.push_native_barrier(&NativeBarrier { value: 1 })?;
    writer.immediate()?;
    drop(writer);
    let reader = NativeImmediate::open_reader(&path)?;
    assert_eq!(reader.native_samples()?.len(), 2);
    Ok(())
}
