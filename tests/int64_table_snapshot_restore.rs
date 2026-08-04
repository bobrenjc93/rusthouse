use rusthouse::snapshot::{NULLABLE_I64_NULL_TAG, NULLABLE_I64_PAYLOAD_HEADER_LEN};
use rusthouse::{
    InsertError, Int64TableRestoreError, NullableI64PayloadCodec, NullableI64PayloadError, Schema,
    SnapshotCodec, SnapshotError, restore_int64_table,
};

fn envelope_for(
    rows: &[Option<i64>],
    snapshot_codec: SnapshotCodec,
    payload_codec: NullableI64PayloadCodec,
) -> Vec<u8> {
    let payload = payload_codec.encode(rows).unwrap();
    snapshot_codec.encode(&payload).unwrap()
}

#[test]
fn restores_an_empty_table_with_the_requested_schema_and_cap() {
    let payload_codec = NullableI64PayloadCodec::new(0, NULLABLE_I64_PAYLOAD_HEADER_LEN);
    let snapshot_codec = SnapshotCodec::new(NULLABLE_I64_PAYLOAD_HEADER_LEN);
    let envelope = envelope_for(&[], snapshot_codec, payload_codec);

    let table = restore_int64_table(
        &envelope,
        Schema::int64("event_id", false),
        4,
        snapshot_codec,
        payload_codec,
    )
    .unwrap();

    assert!(table.is_empty());
    assert_eq!(table.schema(), &Schema::int64("event_id", false));
    assert_eq!(table.row_cap(), 4);
}

#[test]
fn restores_nullable_rows_and_integer_extremes_in_order() {
    let rows = [Some(i64::MIN), None, Some(0), Some(i64::MAX)];
    let payload_codec = NullableI64PayloadCodec::new(rows.len(), 36);
    let snapshot_codec = SnapshotCodec::new(36);
    let envelope = envelope_for(&rows, snapshot_codec, payload_codec);

    let table = restore_int64_table(
        &envelope,
        Schema::int64("reading", true),
        rows.len(),
        snapshot_codec,
        payload_codec,
    )
    .unwrap();

    assert_eq!(table.values(), rows);
}

#[test]
fn preserves_corrupt_envelope_errors() {
    let payload_codec = NullableI64PayloadCodec::new(1, 17);
    let snapshot_codec = SnapshotCodec::new(17);
    let mut envelope = envelope_for(&[Some(7)], snapshot_codec, payload_codec);
    *envelope.last_mut().unwrap() ^= 1;

    let error = restore_int64_table(
        &envelope,
        Schema::int64("reading", true),
        1,
        snapshot_codec,
        payload_codec,
    )
    .unwrap_err();

    assert!(matches!(
        error,
        Int64TableRestoreError::Envelope(SnapshotError::ChecksumMismatch { .. })
    ));
}

#[test]
fn preserves_envelope_trailing_byte_errors() {
    let payload_codec = NullableI64PayloadCodec::new(0, NULLABLE_I64_PAYLOAD_HEADER_LEN);
    let snapshot_codec = SnapshotCodec::new(NULLABLE_I64_PAYLOAD_HEADER_LEN);
    let mut envelope = envelope_for(&[], snapshot_codec, payload_codec);
    envelope.push(0xaa);
    let actual_len = envelope.len();

    let error = restore_int64_table(
        &envelope,
        Schema::int64("reading", true),
        0,
        snapshot_codec,
        payload_codec,
    )
    .unwrap_err();

    assert_eq!(
        error,
        Int64TableRestoreError::Envelope(SnapshotError::TrailingBytes {
            expected_len: actual_len - 1,
            actual_len,
        })
    );
}

#[test]
fn preserves_payload_trailing_data_errors() {
    let mut payload = 1_u64.to_le_bytes().to_vec();
    payload.push(NULLABLE_I64_NULL_TAG);
    payload.push(0xaa);
    let snapshot_codec = SnapshotCodec::new(payload.len());
    let payload_codec = NullableI64PayloadCodec::new(1, payload.len());
    let envelope = snapshot_codec.encode(&payload).unwrap();

    let error = restore_int64_table(
        &envelope,
        Schema::int64("reading", true),
        1,
        snapshot_codec,
        payload_codec,
    )
    .unwrap_err();

    assert_eq!(
        error,
        Int64TableRestoreError::Payload(NullableI64PayloadError::TrailingData {
            expected_len: NULLABLE_I64_PAYLOAD_HEADER_LEN + 1,
            actual_len: NULLABLE_I64_PAYLOAD_HEADER_LEN + 2,
        })
    );
}

#[test]
fn preserves_payload_limit_errors_independently_of_the_envelope_limit() {
    let envelope_payload_codec = NullableI64PayloadCodec::new(1, 17);
    let snapshot_codec = SnapshotCodec::new(17);
    let envelope = envelope_for(&[Some(7)], snapshot_codec, envelope_payload_codec);
    let restore_payload_codec = NullableI64PayloadCodec::new(1, 16);

    let error = restore_int64_table(
        &envelope,
        Schema::int64("reading", true),
        1,
        snapshot_codec,
        restore_payload_codec,
    )
    .unwrap_err();

    assert_eq!(
        error,
        Int64TableRestoreError::Payload(NullableI64PayloadError::PayloadTooLarge {
            payload_len: 17,
            max_payload_len: 16,
        })
    );
}

#[test]
fn rejects_null_for_a_non_nullable_schema_without_returning_a_table() {
    let rows = [Some(1), None, Some(2)];
    let payload_codec = NullableI64PayloadCodec::new(rows.len(), 27);
    let snapshot_codec = SnapshotCodec::new(27);
    let envelope = envelope_for(&rows, snapshot_codec, payload_codec);

    let error = restore_int64_table(
        &envelope,
        Schema::int64("reading", false),
        rows.len(),
        snapshot_codec,
        payload_codec,
    )
    .unwrap_err();

    assert_eq!(
        error,
        Int64TableRestoreError::Table(InsertError::NullNotAllowed {
            column: "reading".to_owned(),
        })
    );
}

#[test]
fn preserves_table_row_cap_errors_without_returning_partial_rows() {
    let rows = [Some(1), Some(2)];
    let payload_codec = NullableI64PayloadCodec::new(rows.len(), 26);
    let snapshot_codec = SnapshotCodec::new(26);
    let envelope = envelope_for(&rows, snapshot_codec, payload_codec);

    let error = restore_int64_table(
        &envelope,
        Schema::int64("reading", false),
        1,
        snapshot_codec,
        payload_codec,
    )
    .unwrap_err();

    assert_eq!(
        error,
        Int64TableRestoreError::Table(InsertError::RowCapExceeded {
            row_cap: 1,
            current_rows: 0,
            incoming_rows: 2,
        })
    );
}

#[test]
fn accepts_rows_at_every_exact_limit() {
    let rows = [None, Some(i64::MAX)];
    let exact_payload_len = 18;
    let payload_codec = NullableI64PayloadCodec::new(rows.len(), exact_payload_len);
    let snapshot_codec = SnapshotCodec::new(exact_payload_len);
    let envelope = envelope_for(&rows, snapshot_codec, payload_codec);

    let table = restore_int64_table(
        &envelope,
        Schema::int64("reading", true),
        rows.len(),
        snapshot_codec,
        payload_codec,
    )
    .unwrap();

    assert_eq!(table.row_count(), table.row_cap());
    assert_eq!(table.values(), rows);
}
