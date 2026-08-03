use rusthouse::snapshot::NULLABLE_I64_PAYLOAD_HEADER_LEN;
use rusthouse::{
    Int64Table, JoinLimits, NullableI64PayloadCodec, ParseLimits, Schema, SnapshotCodec,
    execute_select, inner_equi_join_nullable_i64, parse_select,
};

fn table(values: &[Option<i64>]) -> Int64Table {
    let mut table = Int64Table::new(Schema::int64("key", true), values.len());
    table.append_batch(values).unwrap();
    table
}

fn snapshot_round_trip(rows: &[Option<i64>]) -> Vec<Option<i64>> {
    let encoded_len = NULLABLE_I64_PAYLOAD_HEADER_LEN
        + rows
            .iter()
            .map(|value| 1 + usize::from(value.is_some()) * std::mem::size_of::<i64>())
            .sum::<usize>();
    let payload_codec = NullableI64PayloadCodec::new(rows.len(), encoded_len);
    let snapshot_codec = SnapshotCodec::new(encoded_len);

    let payload = payload_codec.encode(rows).unwrap();
    let envelope = snapshot_codec.encode(&payload).unwrap();
    let decoded_payload = snapshot_codec.decode(&envelope).unwrap();

    payload_codec.decode(decoded_payload).unwrap()
}

#[test]
fn borrowed_selects_and_restored_snapshots_produce_the_same_join() {
    let left = table(&[Some(7), None, Some(7), Some(9)]);
    let right = table(&[Some(7), Some(9), None, Some(7)]);
    let left_select =
        parse_select("SELECT key FROM left_rows LIMIT 3", ParseLimits::default()).unwrap();
    let right_select =
        parse_select("SELECT key FROM right_rows LIMIT 4", ParseLimits::default()).unwrap();

    let left_rows = execute_select("left_rows", &left, &left_select).unwrap();
    let right_rows = execute_select("right_rows", &right, &right_select).unwrap();
    assert_eq!(left_rows.as_ref(), &[Some(7), None, Some(7)]);
    assert!(std::ptr::eq(left_rows.as_ptr(), left.values().as_ptr()));
    assert!(std::ptr::eq(right_rows.as_ptr(), right.values().as_ptr()));

    let restored_left = snapshot_round_trip(left_rows.as_ref());
    let restored_right = snapshot_round_trip(right_rows.as_ref());
    let limits = JoinLimits::new(4, 4);

    let live_matches =
        inner_equi_join_nullable_i64(left_rows.as_ref(), right_rows.as_ref(), limits).unwrap();
    let restored_matches =
        inner_equi_join_nullable_i64(&restored_left, &restored_right, limits).unwrap();

    assert_eq!(restored_left, left_rows.as_ref());
    assert_eq!(restored_right, right_rows.as_ref());
    assert_eq!(restored_matches, live_matches);
    assert_eq!(
        live_matches
            .into_iter()
            .map(|pair| pair.into_pair())
            .collect::<Vec<_>>(),
        vec![(0, 0), (0, 3), (2, 0), (2, 3)]
    );
}
