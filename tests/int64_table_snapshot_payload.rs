use rusthouse::snapshot::{
    INT64_TABLE_INT64_TAG, INT64_TABLE_NOT_NULL_TAG, INT64_TABLE_NULLABLE_TAG,
    INT64_TABLE_PAYLOAD_FIXED_LEN, INT64_TABLE_PAYLOAD_MAGIC, INT64_TABLE_PAYLOAD_VERSION,
    NULLABLE_I64_NULL_TAG, NULLABLE_I64_VALUE_TAG,
};
use rusthouse::{
    Int64Table, Int64TablePayloadCodec, Int64TablePayloadError, Schema, SnapshotCodec,
};

const TYPE_OFFSET: usize = INT64_TABLE_PAYLOAD_MAGIC.len() + std::mem::size_of::<u16>();
const NULLABILITY_OFFSET: usize = TYPE_OFFSET + std::mem::size_of::<u8>();
const NAME_LENGTH_OFFSET: usize = NULLABILITY_OFFSET + std::mem::size_of::<u8>();
const NAME_OFFSET: usize = NAME_LENGTH_OFFSET + std::mem::size_of::<u64>();

fn table(name: &str, nullable: bool, row_cap: usize, rows: &[Option<i64>]) -> Int64Table {
    let mut table = Int64Table::new(Schema::int64(name, nullable), row_cap);
    table.append_batch(rows).unwrap();
    table
}

fn offsets(name_len: usize) -> (usize, usize, usize) {
    let row_cap = NAME_OFFSET + name_len;
    let row_count = row_cap + std::mem::size_of::<u64>();
    let rows = row_count + std::mem::size_of::<u64>();
    (row_cap, row_count, rows)
}

#[test]
fn writes_the_documented_layout_and_round_trips_all_table_metadata() {
    let name = "métric";
    let rows = [Some(i64::MIN), None, Some(i64::MAX)];
    let source = table(name, true, 5, &rows);
    let encoded_len = INT64_TABLE_PAYLOAD_FIXED_LEN + name.len() + 19;
    let codec = Int64TablePayloadCodec::new(name.len(), 5, encoded_len);
    let mut expected = Vec::new();
    expected.extend_from_slice(&INT64_TABLE_PAYLOAD_MAGIC);
    expected.extend_from_slice(&INT64_TABLE_PAYLOAD_VERSION.to_le_bytes());
    expected.push(INT64_TABLE_INT64_TAG);
    expected.push(INT64_TABLE_NULLABLE_TAG);
    expected.extend_from_slice(&(name.len() as u64).to_le_bytes());
    expected.extend_from_slice(name.as_bytes());
    expected.extend_from_slice(&5_u64.to_le_bytes());
    expected.extend_from_slice(&3_u64.to_le_bytes());
    expected.push(NULLABLE_I64_VALUE_TAG);
    expected.extend_from_slice(&i64::MIN.to_le_bytes());
    expected.push(NULLABLE_I64_NULL_TAG);
    expected.push(NULLABLE_I64_VALUE_TAG);
    expected.extend_from_slice(&i64::MAX.to_le_bytes());

    let payload = codec.encode(&source).unwrap();
    let reopened = codec.decode(&payload).unwrap();

    assert_eq!(payload, expected);
    assert_eq!(reopened, source);
    assert_eq!(codec.max_name_len(), name.len());
    assert_eq!(codec.max_rows(), 5);
    assert_eq!(codec.max_payload_len(), encoded_len);
}

#[test]
fn round_trips_an_empty_non_nullable_table_and_its_unused_row_capacity() {
    let source = table("id", false, 7, &[]);
    let codec = Int64TablePayloadCodec::new(2, 7, INT64_TABLE_PAYLOAD_FIXED_LEN + 2);

    let payload = codec.encode(&source).unwrap();
    let reopened = codec.decode(&payload).unwrap();

    assert_eq!(payload[NULLABILITY_OFFSET], INT64_TABLE_NOT_NULL_TAG);
    assert_eq!(reopened, source);
    assert_eq!(reopened.row_cap(), 7);
}

#[test]
fn encode_rejects_oversized_names_row_caps_and_payloads() {
    let named = table("name", false, 1, &[Some(1)]);
    assert_eq!(
        Int64TablePayloadCodec::new(3, 1, 128).encode(&named),
        Err(Int64TablePayloadError::NameTooLong {
            name_len: 4,
            max_name_len: 3,
        })
    );

    assert_eq!(
        Int64TablePayloadCodec::new(4, 0, 128).encode(&named),
        Err(Int64TablePayloadError::RowCapLimitExceeded {
            row_cap: 1,
            max_rows: 0,
        })
    );

    let exact_len = INT64_TABLE_PAYLOAD_FIXED_LEN + 4 + 9;
    assert_eq!(
        Int64TablePayloadCodec::new(4, 1, exact_len - 1).encode(&named),
        Err(Int64TablePayloadError::PayloadTooLarge {
            payload_len: exact_len as u64,
            max_payload_len: exact_len - 1,
        })
    );
}

#[test]
fn decode_rejects_oversized_input_names_row_caps_and_row_counts() {
    let source = table("name", true, 2, &[Some(1)]);
    let permissive = Int64TablePayloadCodec::new(8, 8, 128);
    let payload = permissive.encode(&source).unwrap();

    let mut oversized_payload = payload.clone();
    oversized_payload.push(0xaa);
    assert_eq!(
        Int64TablePayloadCodec::new(8, 8, payload.len()).decode(&oversized_payload),
        Err(Int64TablePayloadError::PayloadTooLarge {
            payload_len: oversized_payload.len() as u64,
            max_payload_len: payload.len(),
        })
    );

    let mut oversized_name = payload.clone();
    oversized_name[NAME_LENGTH_OFFSET..NAME_OFFSET].copy_from_slice(&5_u64.to_le_bytes());
    assert_eq!(
        Int64TablePayloadCodec::new(4, 8, 128).decode(&oversized_name),
        Err(Int64TablePayloadError::NameTooLong {
            name_len: 5,
            max_name_len: 4,
        })
    );

    let (row_cap_offset, row_count_offset, _) = offsets(4);
    let mut oversized_cap = payload.clone();
    oversized_cap[row_cap_offset..row_count_offset].copy_from_slice(&9_u64.to_le_bytes());
    assert_eq!(
        permissive.decode(&oversized_cap),
        Err(Int64TablePayloadError::RowCapLimitExceeded {
            row_cap: 9,
            max_rows: 8,
        })
    );

    let mut oversized_rows = payload;
    oversized_rows[row_count_offset..row_count_offset + 8].copy_from_slice(&9_u64.to_le_bytes());
    assert_eq!(
        permissive.decode(&oversized_rows),
        Err(Int64TablePayloadError::RowLimitExceeded {
            row_count: 9,
            max_rows: 8,
        })
    );
}

#[test]
fn rejects_incompatible_headers_and_every_unknown_tag_kind() {
    let source = table("id", true, 1, &[Some(1)]);
    let codec = Int64TablePayloadCodec::new(2, 1, 64);
    let payload = codec.encode(&source).unwrap();

    let mut bad_magic = payload.clone();
    bad_magic[0] ^= 1;
    let mut found = INT64_TABLE_PAYLOAD_MAGIC;
    found[0] ^= 1;
    assert_eq!(
        codec.decode(&bad_magic),
        Err(Int64TablePayloadError::IncompatibleMagic { found })
    );

    let mut bad_version = payload.clone();
    bad_version[INT64_TABLE_PAYLOAD_MAGIC.len()..TYPE_OFFSET].copy_from_slice(&2_u16.to_le_bytes());
    assert_eq!(
        codec.decode(&bad_version),
        Err(Int64TablePayloadError::UnsupportedVersion {
            found: 2,
            supported: INT64_TABLE_PAYLOAD_VERSION,
        })
    );

    let mut bad_type = payload.clone();
    bad_type[TYPE_OFFSET] = 0x7e;
    assert_eq!(
        codec.decode(&bad_type),
        Err(Int64TablePayloadError::UnknownColumnTypeTag { tag: 0x7e })
    );

    let mut bad_nullability = payload.clone();
    bad_nullability[NULLABILITY_OFFSET] = 0x7d;
    assert_eq!(
        codec.decode(&bad_nullability),
        Err(Int64TablePayloadError::UnknownNullabilityTag { tag: 0x7d })
    );

    let (_, _, rows_offset) = offsets(2);
    let mut bad_row = payload;
    bad_row[rows_offset] = 0x7c;
    assert_eq!(
        codec.decode(&bad_row),
        Err(Int64TablePayloadError::UnknownRowTag {
            row_index: 0,
            tag: 0x7c,
        })
    );
}

#[test]
fn rejects_invalid_utf8_nullability_and_rows_beyond_the_persisted_cap() {
    let nullable = table("id", true, 1, &[None]);
    let codec = Int64TablePayloadCodec::new(2, 1, 64);
    let payload = codec.encode(&nullable).unwrap();

    let mut invalid_utf8 = payload.clone();
    invalid_utf8[NAME_OFFSET] = 0xff;
    assert_eq!(
        codec.decode(&invalid_utf8),
        Err(Int64TablePayloadError::InvalidColumnNameUtf8 {
            valid_up_to: 0,
            error_len: Some(1),
        })
    );

    let mut forbidden_null = payload.clone();
    forbidden_null[NULLABILITY_OFFSET] = INT64_TABLE_NOT_NULL_TAG;
    assert_eq!(
        codec.decode(&forbidden_null),
        Err(Int64TablePayloadError::NullNotAllowed { row_index: 0 })
    );

    let (row_cap_offset, row_count_offset, _) = offsets(2);
    let mut over_cap = payload;
    over_cap[row_cap_offset..row_count_offset].copy_from_slice(&0_u64.to_le_bytes());
    assert_eq!(
        codec.decode(&over_cap),
        Err(Int64TablePayloadError::RowsExceedRowCap {
            row_count: 1,
            row_cap: 0,
        })
    );
}

#[test]
fn rejects_truncation_at_fixed_name_metadata_tag_and_value_boundaries() {
    let source = table("name", true, 1, &[Some(7)]);
    let codec = Int64TablePayloadCodec::new(4, 1, 128);
    let payload = codec.encode(&source).unwrap();
    let (_, _, rows_offset) = offsets(4);

    assert_eq!(
        codec.decode(&payload[..NAME_OFFSET - 1]),
        Err(Int64TablePayloadError::Truncated {
            expected_len: NAME_OFFSET,
            actual_len: NAME_OFFSET - 1,
        })
    );
    assert_eq!(
        codec.decode(&payload[..NAME_OFFSET + 3]),
        Err(Int64TablePayloadError::Truncated {
            expected_len: NAME_OFFSET + 4,
            actual_len: NAME_OFFSET + 3,
        })
    );
    assert_eq!(
        codec.decode(&payload[..rows_offset - 1]),
        Err(Int64TablePayloadError::Truncated {
            expected_len: rows_offset,
            actual_len: rows_offset - 1,
        })
    );
    assert_eq!(
        codec.decode(&payload[..rows_offset]),
        Err(Int64TablePayloadError::Truncated {
            expected_len: rows_offset + 1,
            actual_len: rows_offset,
        })
    );
    assert_eq!(
        codec.decode(&payload[..payload.len() - 1]),
        Err(Int64TablePayloadError::Truncated {
            expected_len: payload.len(),
            actual_len: payload.len() - 1,
        })
    );
}

#[test]
fn rejects_bytes_after_the_declared_rows() {
    let source = table("id", false, 1, &[Some(7)]);
    let exact_codec = Int64TablePayloadCodec::new(2, 1, 64);
    let mut payload = exact_codec.encode(&source).unwrap();
    let expected_len = payload.len();
    payload.push(0xaa);

    assert_eq!(
        exact_codec.decode(&payload),
        Err(Int64TablePayloadError::TrailingData {
            expected_len,
            actual_len: expected_len + 1,
        })
    );
}

#[test]
fn round_trips_a_self_describing_table_through_the_snapshot_envelope() {
    let source = table("reading", true, 4, &[Some(i64::MIN), None, Some(9)]);
    let table_codec = Int64TablePayloadCodec::new(7, 4, 128);
    let payload = table_codec.encode(&source).unwrap();
    let snapshot_codec = SnapshotCodec::new(payload.len());

    let envelope = snapshot_codec.encode(&payload).unwrap();
    let decoded_payload = snapshot_codec.decode(&envelope).unwrap();

    assert_eq!(table_codec.decode(decoded_payload), Ok(source));
}

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::io::ErrorKind;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{Int64TablePayloadCodec, SnapshotCodec, table};

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let base =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("target/self-describing-snapshot-tests");
            fs::create_dir_all(&base).unwrap();

            loop {
                let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
                let path = base.join(format!("{}-{sequence}", std::process::id()));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("could not create test directory: {error}"),
                }
            }
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn composes_with_atomic_replacement_and_reopens_all_metadata() {
        let directory = TestDirectory::new();
        let path = directory.join("table.snapshot");
        let table_codec = Int64TablePayloadCodec::new(16, 5, 128);
        let snapshot_codec = SnapshotCodec::new(128);
        let old = table("old", false, 1, &[Some(1)]);
        let replacement = table("measure", true, 5, &[None, Some(i64::MAX)]);

        let old_payload = table_codec.encode(&old).unwrap();
        snapshot_codec.replace_file(&path, &old_payload).unwrap();
        let old_envelope = fs::read(&path).unwrap();

        let replacement_payload = table_codec.encode(&replacement).unwrap();
        snapshot_codec
            .replace_file(&path, &replacement_payload)
            .unwrap();
        let replacement_envelope = fs::read(path).unwrap();
        let decoded_payload = snapshot_codec.decode(&replacement_envelope).unwrap();

        assert_ne!(replacement_envelope, old_envelope);
        assert_eq!(table_codec.decode(decoded_payload), Ok(replacement));
        assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
    }
}
