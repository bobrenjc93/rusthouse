use rusthouse::snapshot::{
    NULLABLE_I64_NULL_TAG, NULLABLE_I64_PAYLOAD_HEADER_LEN, NULLABLE_I64_VALUE_TAG,
};
use rusthouse::{NullableI64PayloadCodec, NullableI64PayloadError, SnapshotCodec};

#[test]
fn encodes_empty_rows_as_a_zero_row_count() {
    let codec = NullableI64PayloadCodec::new(0, NULLABLE_I64_PAYLOAD_HEADER_LEN);

    let payload = codec.encode(&[]).unwrap();

    assert_eq!(payload, 0_u64.to_le_bytes());
    assert_eq!(codec.decode(&payload), Ok(Vec::new()));
}

#[test]
fn writes_the_documented_tags_and_integer_boundaries() {
    let rows = [None, Some(i64::MIN), Some(i64::MAX)];
    let codec = NullableI64PayloadCodec::new(3, 27);
    let mut expected = Vec::new();
    expected.extend_from_slice(&3_u64.to_le_bytes());
    expected.push(NULLABLE_I64_NULL_TAG);
    expected.push(NULLABLE_I64_VALUE_TAG);
    expected.extend_from_slice(&i64::MIN.to_le_bytes());
    expected.push(NULLABLE_I64_VALUE_TAG);
    expected.extend_from_slice(&i64::MAX.to_le_bytes());

    let payload = codec.encode(&rows).unwrap();

    assert_eq!(payload, expected);
    assert_eq!(codec.decode(&payload), Ok(rows.to_vec()));
}

#[test]
fn accepts_row_and_byte_counts_exactly_at_the_limits() {
    let rows = [None, Some(i64::MIN), Some(i64::MAX)];
    let codec = NullableI64PayloadCodec::new(rows.len(), 27);
    let payload = codec.encode(&rows).unwrap();

    assert_eq!(codec.max_rows(), rows.len());
    assert_eq!(codec.max_payload_len(), payload.len());
    assert_eq!(codec.decode(&payload), Ok(rows.to_vec()));
}

#[test]
fn enforces_row_limits_during_encode_and_before_decode_allocation() {
    let codec = NullableI64PayloadCodec::new(1, 64);

    assert_eq!(
        codec.encode(&[None, None]),
        Err(NullableI64PayloadError::RowLimitExceeded {
            row_count: 2,
            max_rows: 1,
        })
    );

    let payload = 2_u64.to_le_bytes();
    assert_eq!(
        codec.decode(&payload),
        Err(NullableI64PayloadError::RowLimitExceeded {
            row_count: 2,
            max_rows: 1,
        })
    );
}

#[test]
fn enforces_byte_limits_during_encode_and_decode() {
    let codec = NullableI64PayloadCodec::new(1, 8);

    assert_eq!(
        codec.encode(&[None]),
        Err(NullableI64PayloadError::PayloadTooLarge {
            payload_len: 9,
            max_payload_len: 8,
        })
    );

    let mut payload = 0_u64.to_le_bytes().to_vec();
    payload.push(NULLABLE_I64_NULL_TAG);
    assert_eq!(
        codec.decode(&payload),
        Err(NullableI64PayloadError::PayloadTooLarge {
            payload_len: 9,
            max_payload_len: 8,
        })
    );
}

#[test]
fn rejects_truncated_row_counts_tags_and_values() {
    let codec = NullableI64PayloadCodec::new(2, 64);

    assert_eq!(
        codec.decode(&[0; NULLABLE_I64_PAYLOAD_HEADER_LEN - 1]),
        Err(NullableI64PayloadError::Truncated {
            expected_len: NULLABLE_I64_PAYLOAD_HEADER_LEN,
            actual_len: NULLABLE_I64_PAYLOAD_HEADER_LEN - 1,
        })
    );

    assert_eq!(
        codec.decode(&1_u64.to_le_bytes()),
        Err(NullableI64PayloadError::Truncated {
            expected_len: NULLABLE_I64_PAYLOAD_HEADER_LEN + 1,
            actual_len: NULLABLE_I64_PAYLOAD_HEADER_LEN,
        })
    );

    let mut truncated_value = 1_u64.to_le_bytes().to_vec();
    truncated_value.push(NULLABLE_I64_VALUE_TAG);
    truncated_value.extend_from_slice(&[0; 7]);
    assert_eq!(
        codec.decode(&truncated_value),
        Err(NullableI64PayloadError::Truncated {
            expected_len: NULLABLE_I64_PAYLOAD_HEADER_LEN + 1 + 8,
            actual_len: NULLABLE_I64_PAYLOAD_HEADER_LEN + 1 + 7,
        })
    );
}

#[test]
fn rejects_invalid_tags_with_the_row_index() {
    let codec = NullableI64PayloadCodec::new(2, 64);
    let mut payload = 2_u64.to_le_bytes().to_vec();
    payload.push(NULLABLE_I64_NULL_TAG);
    payload.push(0x7f);

    assert_eq!(
        codec.decode(&payload),
        Err(NullableI64PayloadError::InvalidTag {
            row_index: 1,
            tag: 0x7f,
        })
    );
}

#[test]
fn rejects_data_after_the_declared_rows() {
    let codec = NullableI64PayloadCodec::new(1, 64);
    let mut payload = 1_u64.to_le_bytes().to_vec();
    payload.push(NULLABLE_I64_NULL_TAG);
    payload.push(0xaa);

    assert_eq!(
        codec.decode(&payload),
        Err(NullableI64PayloadError::TrailingData {
            expected_len: NULLABLE_I64_PAYLOAD_HEADER_LEN + 1,
            actual_len: NULLABLE_I64_PAYLOAD_HEADER_LEN + 2,
        })
    );
}

#[test]
fn round_trips_nullable_rows_through_the_snapshot_envelope() {
    let rows = [Some(i64::MIN), None, Some(0), Some(i64::MAX)];
    let payload_codec = NullableI64PayloadCodec::new(4, 36);
    let snapshot_codec = SnapshotCodec::new(payload_codec.max_payload_len());

    let payload = payload_codec.encode(&rows).unwrap();
    let envelope = snapshot_codec.encode(&payload).unwrap();
    let decoded_payload = snapshot_codec.decode(&envelope).unwrap();

    assert_eq!(payload_codec.decode(decoded_payload), Ok(rows.to_vec()));
}
