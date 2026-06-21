use super::adapter::{
    GROUP_PATH, RawDataIndex, RawIndexMode, TdmsChannelValues, TdmsObject, TdmsProperty,
    TdmsPropertyValue, TdmsRawValues, TdmsTimestampValue, channel_path,
};

pub(super) fn first_tdms_channel_values(include_unit_channels: bool) -> Vec<TdmsChannelValues> {
    let mut channels = vec![
        channel("Int8", TdmsRawValues::I8(vec![-1, 2])),
        channel("Int16", TdmsRawValues::I16(vec![-300, 400])),
        channel("Int32", TdmsRawValues::I32(vec![-70_000, 80_000])),
        channel(
            "Int64",
            TdmsRawValues::I64(vec![-9_000_000_000, 10_000_000_000]),
        ),
        channel("Uint8", TdmsRawValues::U8(vec![1, 250])),
        channel("Uint16", TdmsRawValues::U16(vec![500, 60_000])),
        channel("Uint32", TdmsRawValues::U32(vec![70_000, 4_000_000_000])),
        channel(
            "Uint64",
            TdmsRawValues::U64(vec![9_000_000_000, 18_000_000_000_000_000_000]),
        ),
        channel("Float32", TdmsRawValues::F32(vec![1.25, -2.5])),
        channel("Float64", TdmsRawValues::F64(vec![3.5, -4.75])),
    ];
    if include_unit_channels {
        channels.extend([
            channel(
                "Float32Unit",
                TdmsRawValues::F32WithUnit(vec![10.25, -20.5]),
            ),
            channel(
                "Float64Unit",
                TdmsRawValues::F64WithUnit(vec![30.5, -40.75]),
            ),
        ]);
    }
    channels.extend([
        channel("Boolean", TdmsRawValues::Bool(vec![true, false])),
        channel(
            "String",
            TdmsRawValues::String(vec!["red".to_string(), "blue".to_string()]),
        ),
        channel(
            "Timestamp",
            TdmsRawValues::Timestamp(vec![
                TdmsTimestampValue {
                    second_fractions: 0,
                    seconds: 3_660_681_600,
                },
                TdmsTimestampValue {
                    second_fractions: 9_223_372_036_854_775_808,
                    seconds: 3_660_681_600,
                },
            ]),
        ),
        channel(
            "Complex64",
            TdmsRawValues::ComplexF32(vec![(1.0, 2.0), (-3.0, 4.0)]),
        ),
        channel(
            "Complex128",
            TdmsRawValues::ComplexF64(vec![(5.0, 6.0), (-7.0, 8.0)]),
        ),
    ]);
    channels
}

pub(super) fn changed_tdms_channel_values(include_unit_channels: bool) -> Vec<TdmsChannelValues> {
    let mut channels = vec![
        channel("Int8", TdmsRawValues::I8(vec![3])),
        channel("Int16", TdmsRawValues::I16(vec![-500])),
        channel("Int32", TdmsRawValues::I32(vec![90_000])),
        channel("Int64", TdmsRawValues::I64(vec![11_000_000_000])),
        channel("Uint8", TdmsRawValues::U8(vec![42])),
        channel("Uint16", TdmsRawValues::U16(vec![1234])),
        channel("Uint32", TdmsRawValues::U32(vec![5_000_000])),
        channel(
            "Uint64",
            TdmsRawValues::U64(vec![17_000_000_000_000_000_000]),
        ),
        channel("Float32", TdmsRawValues::F32(vec![3.75])),
        channel("Float64", TdmsRawValues::F64(vec![6.25])),
    ];
    if include_unit_channels {
        channels.extend([
            channel("Float32Unit", TdmsRawValues::F32WithUnit(vec![50.25])),
            channel("Float64Unit", TdmsRawValues::F64WithUnit(vec![60.125])),
        ]);
    }
    channels.extend([
        channel("Boolean", TdmsRawValues::Bool(vec![true])),
        channel("String", TdmsRawValues::String(vec!["cyan".to_string()])),
        channel(
            "Timestamp",
            TdmsRawValues::Timestamp(vec![TdmsTimestampValue {
                second_fractions: 0,
                seconds: 3_660_681_601,
            }]),
        ),
        channel("Complex64", TdmsRawValues::ComplexF32(vec![(5.0, -6.0)])),
        channel("Complex128", TdmsRawValues::ComplexF64(vec![(9.0, -10.0)])),
    ]);
    channels
}

pub(super) fn same_tdms_channel_values(include_unit_channels: bool) -> Vec<TdmsChannelValues> {
    let mut channels = vec![
        channel("Int8", TdmsRawValues::I8(vec![-4])),
        channel("Int16", TdmsRawValues::I16(vec![600])),
        channel("Int32", TdmsRawValues::I32(vec![-100_000])),
        channel("Int64", TdmsRawValues::I64(vec![-12_000_000_000])),
        channel("Uint8", TdmsRawValues::U8(vec![7])),
        channel("Uint16", TdmsRawValues::U16(vec![4321])),
        channel("Uint32", TdmsRawValues::U32(vec![6_000_000])),
        channel(
            "Uint64",
            TdmsRawValues::U64(vec![16_000_000_000_000_000_000]),
        ),
        channel("Float32", TdmsRawValues::F32(vec![-4.5])),
        channel("Float64", TdmsRawValues::F64(vec![-7.5])),
    ];
    if include_unit_channels {
        channels.extend([
            channel("Float32Unit", TdmsRawValues::F32WithUnit(vec![-70.5])),
            channel("Float64Unit", TdmsRawValues::F64WithUnit(vec![-80.875])),
        ]);
    }
    channels.extend([
        channel("Boolean", TdmsRawValues::Bool(vec![false])),
        channel("String", TdmsRawValues::String(vec!["gold".to_string()])),
        channel(
            "Timestamp",
            TdmsRawValues::Timestamp(vec![TdmsTimestampValue {
                second_fractions: 4_611_686_018_427_387_904,
                seconds: 3_660_681_601,
            }]),
        ),
        channel("Complex64", TdmsRawValues::ComplexF32(vec![(-7.0, -8.0)])),
        channel(
            "Complex128",
            TdmsRawValues::ComplexF64(vec![(-11.0, -12.0)]),
        ),
    ]);
    channels
}

pub(super) fn append_tdms_channel_values(include_unit_channels: bool) -> Vec<TdmsChannelValues> {
    let mut channels = vec![
        channel("Int8", TdmsRawValues::I8(vec![5])),
        channel("Int16", TdmsRawValues::I16(vec![-700])),
        channel("Int32", TdmsRawValues::I32(vec![110_000])),
        channel("Int64", TdmsRawValues::I64(vec![13_000_000_000])),
        channel("Uint8", TdmsRawValues::U8(vec![8])),
        channel("Uint16", TdmsRawValues::U16(vec![5432])),
        channel("Uint32", TdmsRawValues::U32(vec![7_000_000])),
        channel(
            "Uint64",
            TdmsRawValues::U64(vec![15_000_000_000_000_000_000]),
        ),
        channel("Float32", TdmsRawValues::F32(vec![5.5])),
        channel("Float64", TdmsRawValues::F64(vec![8.5])),
    ];
    if include_unit_channels {
        channels.extend([
            channel("Float32Unit", TdmsRawValues::F32WithUnit(vec![90.5])),
            channel("Float64Unit", TdmsRawValues::F64WithUnit(vec![100.625])),
        ]);
    }
    channels.extend([
        channel("Boolean", TdmsRawValues::Bool(vec![true])),
        channel("String", TdmsRawValues::String(vec!["navy".to_string()])),
        channel(
            "Timestamp",
            TdmsRawValues::Timestamp(vec![TdmsTimestampValue {
                second_fractions: 0,
                seconds: 3_660_681_602,
            }]),
        ),
        channel("Complex64", TdmsRawValues::ComplexF32(vec![(9.0, 10.0)])),
        channel("Complex128", TdmsRawValues::ComplexF64(vec![(13.0, 14.0)])),
    ]);
    channels
}

pub(super) fn initial_tdms_objects(
    channels: &[TdmsChannelValues],
) -> varve::Result<Vec<TdmsObject<'static>>> {
    let mut objects = vec![
        TdmsObject {
            path: "/",
            raw_data_index: RawDataIndex::None,
            properties: vec![TdmsProperty {
                name: "title",
                value: TdmsPropertyValue::String("Varve TDMS adapter proof smoke".to_string()),
            }],
        },
        TdmsObject {
            path: GROUP_PATH,
            raw_data_index: RawDataIndex::None,
            properties: vec![
                TdmsProperty {
                    name: "operator",
                    value: TdmsPropertyValue::String("Varve".to_string()),
                },
                TdmsProperty {
                    name: "verified",
                    value: TdmsPropertyValue::Bool(true),
                },
            ],
        },
    ];
    for channel in channels {
        let mut properties = vec![
            TdmsProperty {
                name: "unit_string",
                value: TdmsPropertyValue::String(channel.name.to_string()),
            },
            TdmsProperty {
                name: "type_name",
                value: TdmsPropertyValue::String(channel.name.to_string()),
            },
        ];
        if channel.name == "Boolean" {
            properties.push(TdmsProperty {
                name: "adapter_enabled",
                value: TdmsPropertyValue::Bool(true),
            });
        }
        if channel.name == "Float64" {
            properties.push(TdmsProperty {
                name: "wf_increment",
                value: TdmsPropertyValue::F64(0.001),
            });
        }
        objects.push(TdmsObject {
            path: channel_path(channel.name),
            raw_data_index: channel.values.raw_index()?,
            properties,
        });
    }
    Ok(objects)
}

pub(super) fn channel_tdms_objects(
    channels: &[TdmsChannelValues],
    index_mode: RawIndexMode,
    sample_count: i64,
    segment_note: Option<&str>,
) -> varve::Result<Vec<TdmsObject<'static>>> {
    let mut objects = Vec::with_capacity(channels.len());
    for channel in channels {
        let mut properties = vec![TdmsProperty {
            name: "sample_count",
            value: TdmsPropertyValue::I64(sample_count),
        }];
        if sample_count == 5 && channel.name == "Float64" {
            properties.push(TdmsProperty {
                name: "append_source",
                value: TdmsPropertyValue::String("varve-open-layout-writer".to_string()),
            });
        }
        if channel.name == "String"
            && let Some(note) = segment_note
        {
            properties.push(TdmsProperty {
                name: "segment_note",
                value: TdmsPropertyValue::String(note.to_string()),
            });
        }
        objects.push(TdmsObject {
            path: channel_path(channel.name),
            raw_data_index: match index_mode {
                RawIndexMode::New => channel.values.raw_index()?,
                RawIndexMode::SameAsPrevious => RawDataIndex::SameAsPrevious,
            },
            properties,
        });
    }
    Ok(objects)
}

pub(super) fn encode_channel_values(channels: &[TdmsChannelValues]) -> varve::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for channel in channels {
        bytes.extend(channel.values.encode()?);
    }
    Ok(bytes)
}

pub(super) fn expected_tdms_channels(
    include_unit_channels: bool,
    include_same_index_segment: bool,
    appended: bool,
) -> varve::Result<Vec<TdmsChannelValues>> {
    let mut expected = first_tdms_channel_values(include_unit_channels);
    extend_channel_values(
        &mut expected,
        changed_tdms_channel_values(include_unit_channels),
    )?;
    if include_same_index_segment {
        extend_channel_values(
            &mut expected,
            same_tdms_channel_values(include_unit_channels),
        )?;
    }
    if appended {
        extend_channel_values(
            &mut expected,
            append_tdms_channel_values(include_unit_channels),
        )?;
    }
    Ok(expected)
}

pub(super) fn expected_chunk_count(nptdms_authored: bool, appended: bool) -> usize {
    let include_unit_channels = !nptdms_authored;
    let channel_count = first_tdms_channel_values(include_unit_channels).len();
    let base_segments = if nptdms_authored { 2 } else { 3 };
    let appended_segments = usize::from(appended);
    (base_segments + appended_segments) * channel_count
}

fn channel(name: &'static str, values: TdmsRawValues) -> TdmsChannelValues {
    TdmsChannelValues { name, values }
}

fn extend_channel_values(
    current: &mut [TdmsChannelValues],
    next: Vec<TdmsChannelValues>,
) -> varve::Result<()> {
    assert_eq!(current.len(), next.len());
    for (current, next) in current.iter_mut().zip(next) {
        assert_eq!(current.name, next.name);
        current.values.extend_with(next.values)?;
    }
    Ok(())
}
