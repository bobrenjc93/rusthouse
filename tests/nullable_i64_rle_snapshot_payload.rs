use rusthouse::snapshot::{
    NULLABLE_I64_RLE_NULL_RUN_TAG, NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN,
    NULLABLE_I64_RLE_PAYLOAD_MAGIC, NULLABLE_I64_RLE_PAYLOAD_VERSION,
    NULLABLE_I64_RLE_VALUE_RUN_TAG,
};
use rusthouse::{
    NullableI64PayloadCodec, NullableI64RlePayloadCodec, NullableI64RlePayloadError, SnapshotCodec,
};

fn header(row_count: u64, run_count: u64) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&NULLABLE_I64_RLE_PAYLOAD_MAGIC);
    payload.extend_from_slice(&NULLABLE_I64_RLE_PAYLOAD_VERSION.to_le_bytes());
    payload.extend_from_slice(&row_count.to_le_bytes());
    payload.extend_from_slice(&run_count.to_le_bytes());
    payload
}

fn push_null_run(payload: &mut Vec<u8>, run_length: u64) {
    payload.push(NULLABLE_I64_RLE_NULL_RUN_TAG);
    payload.extend_from_slice(&run_length.to_le_bytes());
}

fn push_value_run(payload: &mut Vec<u8>, run_length: u64, value: i64) {
    payload.push(NULLABLE_I64_RLE_VALUE_RUN_TAG);
    payload.extend_from_slice(&run_length.to_le_bytes());
    payload.extend_from_slice(&value.to_le_bytes());
}

#[test]
fn writes_versioned_maximal_runs_and_round_trips_mixed_rows() {
    let rows = [
        None,
        None,
        Some(-7),
        Some(-7),
        Some(-7),
        None,
        Some(i64::MAX),
        Some(i64::MAX),
        Some(i64::MIN),
    ];
    let mut expected = header(9, 5);
    push_null_run(&mut expected, 2);
    push_value_run(&mut expected, 3, -7);
    push_null_run(&mut expected, 1);
    push_value_run(&mut expected, 2, i64::MAX);
    push_value_run(&mut expected, 1, i64::MIN);
    let codec = NullableI64RlePayloadCodec::new(rows.len(), 5, expected.len());

    let payload = codec.encode(&rows).unwrap();

    assert_eq!(codec.max_rows(), rows.len());
    assert_eq!(codec.max_runs(), 5);
    assert_eq!(codec.max_payload_len(), expected.len());
    assert_eq!(payload, expected);
    assert_eq!(codec.decode(&payload), Ok(rows.to_vec()));
}

#[test]
fn encodes_empty_rows_with_no_runs() {
    let codec = NullableI64RlePayloadCodec::new(0, 0, NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN);

    let payload = codec.encode(&[]).unwrap();

    assert_eq!(payload, header(0, 0));
    assert_eq!(codec.decode(&payload), Ok(Vec::new()));
}

#[test]
fn measurably_shrinks_a_repeated_present_value() {
    let rows = vec![Some(42); 1_000];
    let legacy = NullableI64PayloadCodec::new(rows.len(), usize::MAX)
        .encode(&rows)
        .unwrap();
    let compressed = NullableI64RlePayloadCodec::new(rows.len(), 1, usize::MAX)
        .encode(&rows)
        .unwrap();

    assert_eq!(
        compressed.len(),
        NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN + 1 + 8 + 8
    );
    assert!(compressed.len() * 100 < legacy.len());
}

#[test]
fn accepts_row_run_and_byte_counts_exactly_at_the_limits() {
    let rows = [None, None, Some(9), Some(9), None];
    let unrestricted = NullableI64RlePayloadCodec::new(5, 3, usize::MAX);
    let payload = unrestricted.encode(&rows).unwrap();
    let exact = NullableI64RlePayloadCodec::new(5, 3, payload.len());

    assert_eq!(exact.encode(&rows), Ok(payload.clone()));
    assert_eq!(exact.decode(&payload), Ok(rows.to_vec()));
}

#[test]
fn enforces_row_run_and_byte_limits_during_encode() {
    assert_eq!(
        NullableI64RlePayloadCodec::new(1, 2, 128).encode(&[None, None]),
        Err(NullableI64RlePayloadError::RowLimitExceeded {
            row_count: 2,
            max_rows: 1,
        })
    );
    assert_eq!(
        NullableI64RlePayloadCodec::new(2, 1, 128).encode(&[None, Some(1)]),
        Err(NullableI64RlePayloadError::RunLimitExceeded {
            run_count: 2,
            max_runs: 1,
        })
    );

    let rows = [Some(1)];
    let payload_len = NullableI64RlePayloadCodec::new(1, 1, usize::MAX)
        .encode(&rows)
        .unwrap()
        .len();
    assert_eq!(
        NullableI64RlePayloadCodec::new(1, 1, payload_len - 1).encode(&rows),
        Err(NullableI64RlePayloadError::PayloadTooLarge {
            payload_len: u64::try_from(payload_len).unwrap(),
            max_payload_len: payload_len - 1,
        })
    );
}

#[test]
fn enforces_declared_and_expanded_limits_before_decoding() {
    let payload = NullableI64RlePayloadCodec::new(3, 1, 128)
        .encode(&[Some(4); 3])
        .unwrap();
    assert_eq!(
        NullableI64RlePayloadCodec::new(2, 1, 128).decode(&payload),
        Err(NullableI64RlePayloadError::RowLimitExceeded {
            row_count: 3,
            max_rows: 2,
        })
    );
    assert_eq!(
        NullableI64RlePayloadCodec::new(3, 0, 128).decode(&payload),
        Err(NullableI64RlePayloadError::RunLimitExceeded {
            run_count: 1,
            max_runs: 0,
        })
    );
    assert_eq!(
        NullableI64RlePayloadCodec::new(3, 1, payload.len() - 1).decode(&payload),
        Err(NullableI64RlePayloadError::PayloadTooLarge {
            payload_len: u64::try_from(payload.len()).unwrap(),
            max_payload_len: payload.len() - 1,
        })
    );

    let mut expanded_over_limit = header(2, 1);
    push_null_run(&mut expanded_over_limit, 3);
    assert_eq!(
        NullableI64RlePayloadCodec::new(2, 1, expanded_over_limit.len())
            .decode(&expanded_over_limit),
        Err(NullableI64RlePayloadError::RowLimitExceeded {
            row_count: 3,
            max_rows: 2,
        })
    );
}

#[test]
fn rejects_incompatible_magic_and_unknown_versions() {
    let codec = NullableI64RlePayloadCodec::new(0, 0, NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN);
    let payload = header(0, 0);

    let mut incompatible = payload.clone();
    incompatible[0] ^= 1;
    assert!(matches!(
        codec.decode(&incompatible),
        Err(NullableI64RlePayloadError::IncompatibleMagic { .. })
    ));

    let mut unsupported = payload;
    let version_offset = NULLABLE_I64_RLE_PAYLOAD_MAGIC.len();
    unsupported[version_offset..version_offset + 2]
        .copy_from_slice(&(NULLABLE_I64_RLE_PAYLOAD_VERSION + 1).to_le_bytes());
    assert_eq!(
        codec.decode(&unsupported),
        Err(NullableI64RlePayloadError::UnsupportedVersion {
            found: NULLABLE_I64_RLE_PAYLOAD_VERSION + 1,
            supported: NULLABLE_I64_RLE_PAYLOAD_VERSION,
        })
    );
}

#[test]
fn rejects_zero_length_runs_unknown_tags_and_row_count_mismatches() {
    let codec = NullableI64RlePayloadCodec::new(4, 1, 128);

    let mut zero_length = header(0, 1);
    push_null_run(&mut zero_length, 0);
    assert_eq!(
        codec.decode(&zero_length),
        Err(NullableI64RlePayloadError::ZeroLengthRun { run_index: 0 })
    );

    let mut unknown = header(1, 1);
    unknown.push(0x7f);
    assert_eq!(
        codec.decode(&unknown),
        Err(NullableI64RlePayloadError::UnknownRunTag {
            run_index: 0,
            tag: 0x7f,
        })
    );

    let mut mismatch = header(2, 1);
    push_null_run(&mut mismatch, 1);
    assert_eq!(
        codec.decode(&mismatch),
        Err(NullableI64RlePayloadError::RowCountMismatch {
            declared_rows: 2,
            decoded_rows: 1,
        })
    );
}

#[test]
fn rejects_run_length_sum_overflow() {
    let mut payload = header(0, 2);
    push_null_run(&mut payload, u64::MAX);
    push_null_run(&mut payload, 1);

    assert_eq!(
        NullableI64RlePayloadCodec::new(usize::MAX, 2, payload.len()).decode(&payload),
        Err(NullableI64RlePayloadError::RowCountOverflow {
            run_index: 1,
            decoded_rows: u64::MAX,
            run_length: 1,
        })
    );
}

#[test]
fn rejects_truncated_headers_runs_and_values() {
    let codec = NullableI64RlePayloadCodec::new(2, 1, 128);
    assert_eq!(
        codec.decode(&[0; NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN - 1]),
        Err(NullableI64RlePayloadError::Truncated {
            expected_len: NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN,
            actual_len: NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN - 1,
        })
    );

    let missing_tag = header(1, 1);
    assert_eq!(
        codec.decode(&missing_tag),
        Err(NullableI64RlePayloadError::Truncated {
            expected_len: NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN + 1,
            actual_len: NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN,
        })
    );

    let mut truncated_length = header(1, 1);
    truncated_length.push(NULLABLE_I64_RLE_NULL_RUN_TAG);
    truncated_length.extend_from_slice(&[0; 7]);
    assert_eq!(
        codec.decode(&truncated_length),
        Err(NullableI64RlePayloadError::Truncated {
            expected_len: NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN + 1 + 8,
            actual_len: NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN + 1 + 7,
        })
    );

    let mut truncated_value = header(1, 1);
    truncated_value.push(NULLABLE_I64_RLE_VALUE_RUN_TAG);
    truncated_value.extend_from_slice(&1_u64.to_le_bytes());
    truncated_value.extend_from_slice(&[0; 7]);
    assert_eq!(
        codec.decode(&truncated_value),
        Err(NullableI64RlePayloadError::Truncated {
            expected_len: NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN + 1 + 8 + 8,
            actual_len: NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN + 1 + 8 + 7,
        })
    );
}

#[test]
fn rejects_bytes_after_the_declared_runs() {
    let codec = NullableI64RlePayloadCodec::new(1, 1, 128);
    let mut payload = header(1, 1);
    push_null_run(&mut payload, 1);
    let expected_len = payload.len();
    payload.push(0xaa);

    assert_eq!(
        codec.decode(&payload),
        Err(NullableI64RlePayloadError::TrailingData {
            expected_len,
            actual_len: expected_len + 1,
        })
    );
}

#[test]
fn round_trips_through_a_snapshot_envelope() {
    let rows = [None, None, Some(-1), Some(-1), Some(8), None];
    let payload_codec = NullableI64RlePayloadCodec::new(rows.len(), 4, 128);
    let snapshot_codec = SnapshotCodec::new(payload_codec.max_payload_len());

    let payload = payload_codec.encode(&rows).unwrap();
    let envelope = snapshot_codec.encode(&payload).unwrap();
    let decoded_payload = snapshot_codec.decode(&envelope).unwrap();

    assert_eq!(payload_codec.decode(decoded_payload), Ok(rows.to_vec()));
}
