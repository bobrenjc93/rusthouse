use rusthouse::snapshot::{SnapshotError, SnapshotStore};
use rusthouse::table_snapshot::{
    TABLE_PAYLOAD_MAGIC, TABLE_PAYLOAD_VERSION, TableSnapshotError, TableSnapshotLocation,
};
use rusthouse::{DataType, Field, Table, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const INT64_TAG: u8 = 1;
const BOOL_TAG: u8 = 3;
const STRING_TAG: u8 = 4;

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(test_name: &str) -> Self {
        let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("table-snapshot-tests")
            .join(format!("{test_name}-{}-{sequence}", std::process::id()));
        fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }

    fn snapshot(&self) -> PathBuf {
        self.0.join("table.snapshot")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("remove test directory");
    }
}

#[test]
fn round_trips_a_mixed_table_and_reopens_every_property() {
    let directory = TestDirectory::new("mixed-round-trip");
    let path = directory.snapshot();
    let store = SnapshotStore::new(4 * 1024);
    let fields = vec![
        Field::new("sequence", DataType::Int64),
        Field::new("reading", DataType::Float64),
        Field::new("healthy", DataType::Bool),
        Field::new("region", DataType::String),
    ];
    let mut table = Table::with_row_limit(fields.clone(), 17).unwrap();
    table
        .insert_batch(vec![
            vec![
                Value::Int64(i64::MIN),
                Value::Float64(-0.0),
                Value::Bool(false),
                Value::String(String::new()),
            ],
            vec![
                Value::Int64(42),
                Value::Float64(f64::INFINITY),
                Value::Bool(true),
                Value::String("northwest".to_owned()),
            ],
            vec![
                Value::Int64(i64::MAX),
                Value::Float64(3.5),
                Value::Bool(true),
                Value::String("caf\u{e9}".to_owned()),
            ],
        ])
        .unwrap();

    store.write_table(&path, &table).unwrap();
    let reopened = store.read_table(&path).unwrap();

    assert_eq!(reopened.fields(), fields);
    assert_eq!(reopened.row_limit(), 17);
    assert_eq!(reopened.len(), 3);
    assert_eq!(
        reopened.int64_column("sequence").unwrap(),
        [i64::MIN, 42, i64::MAX]
    );
    assert_eq!(
        reopened
            .float64_column("reading")
            .unwrap()
            .iter()
            .map(|value| value.to_bits())
            .collect::<Vec<_>>(),
        [
            (-0.0_f64).to_bits(),
            f64::INFINITY.to_bits(),
            3.5_f64.to_bits()
        ]
    );
    assert_eq!(
        reopened.bool_column("healthy").unwrap().collect::<Vec<_>>(),
        [false, true, true]
    );
    assert_eq!(
        reopened.string_column("region").unwrap(),
        ["", "northwest", "caf\u{e9}"]
    );
}

#[test]
fn rejects_unknown_schema_and_column_type_tags() {
    let mut invalid_schema_tag = header(0, 0, 1);
    field(&mut invalid_schema_tag, 99, b"id");
    push_u64(&mut invalid_schema_tag, 1);
    assert!(matches!(
        decode_error("schema-tag", &invalid_schema_tag),
        TableSnapshotError::InvalidTypeTag {
            location: TableSnapshotLocation::Field { field: 0 },
            tag: 99,
        }
    ));

    let mut invalid_column_tag = header(0, 0, 1);
    field(&mut invalid_column_tag, INT64_TAG, b"id");
    push_u64(&mut invalid_column_tag, 1);
    invalid_column_tag.push(99);
    assert!(matches!(
        decode_error("column-tag", &invalid_column_tag),
        TableSnapshotError::InvalidTypeTag {
            location: TableSnapshotLocation::Column { column: 0 },
            tag: 99,
        }
    ));
}

#[test]
fn rejects_unbounded_or_truncated_declared_lengths() {
    let enormous_field_count = header(0, 0, u64::MAX);
    assert!(matches!(
        decode_error("field-count", &enormous_field_count),
        TableSnapshotError::InvalidLength {
            location: TableSnapshotLocation::Schema,
            declared: u64::MAX,
            ..
        }
    ));

    let mut enormous_name = header(0, 0, 1);
    enormous_name.push(INT64_TAG);
    push_u64(&mut enormous_name, u64::MAX);
    push_u64(&mut enormous_name, 1);
    assert!(matches!(
        decode_error("field-name-length", &enormous_name),
        TableSnapshotError::InvalidLength {
            location: TableSnapshotLocation::Field { field: 0 },
            ..
        }
    ));

    let mut missing_values = header(1, 1, 1);
    field(&mut missing_values, INT64_TAG, b"id");
    push_u64(&mut missing_values, 1);
    column_header(&mut missing_values, INT64_TAG, 1);
    assert!(matches!(
        decode_error("missing-values", &missing_values),
        TableSnapshotError::InvalidLength {
            location: TableSnapshotLocation::Column { column: 0 },
            declared: 1,
            maximum: 0,
        }
    ));
}

#[test]
fn rejects_invalid_utf8_in_names_and_string_values() {
    let mut invalid_name = header(0, 0, 1);
    field(&mut invalid_name, INT64_TAG, &[0xff]);
    push_u64(&mut invalid_name, 1);
    assert!(matches!(
        decode_error("name-utf8", &invalid_name),
        TableSnapshotError::InvalidUtf8 {
            location: TableSnapshotLocation::Field { field: 0 },
            valid_up_to: 0,
            ..
        }
    ));

    let mut invalid_value = header(1, 1, 1);
    field(&mut invalid_value, STRING_TAG, b"text");
    push_u64(&mut invalid_value, 1);
    column_header(&mut invalid_value, STRING_TAG, 1);
    push_u64(&mut invalid_value, 1);
    invalid_value.push(0xff);
    assert!(matches!(
        decode_error("value-utf8", &invalid_value),
        TableSnapshotError::InvalidUtf8 {
            location: TableSnapshotLocation::StringValue { column: 0, row: 0 },
            valid_up_to: 0,
            ..
        }
    ));
}

#[test]
fn rejects_inconsistent_table_and_column_metadata() {
    let row_limit_error = header(1, 2, 1);
    assert!(matches!(
        decode_error("row-limit", &row_limit_error),
        TableSnapshotError::RowCountExceedsLimit {
            row_count: 2,
            row_limit: 1,
        }
    ));

    let mut column_count_error = header(0, 0, 1);
    field(&mut column_count_error, INT64_TAG, b"id");
    push_u64(&mut column_count_error, 0);
    assert!(matches!(
        decode_error("column-count", &column_count_error),
        TableSnapshotError::ColumnCountMismatch {
            expected: 1,
            actual: 0,
        }
    ));

    let mut column_type_error = header(0, 0, 1);
    field(&mut column_type_error, INT64_TAG, b"id");
    push_u64(&mut column_type_error, 1);
    column_header(&mut column_type_error, BOOL_TAG, 0);
    assert!(matches!(
        decode_error("column-type", &column_type_error),
        TableSnapshotError::ColumnTypeMismatch {
            column: 0,
            expected: DataType::Int64,
            actual: DataType::Bool,
        }
    ));

    let mut column_length_error = header(1, 1, 1);
    field(&mut column_length_error, INT64_TAG, b"id");
    push_u64(&mut column_length_error, 1);
    column_header(&mut column_length_error, INT64_TAG, 0);
    assert!(matches!(
        decode_error("column-length", &column_length_error),
        TableSnapshotError::ColumnLengthMismatch {
            column: 0,
            expected: 1,
            actual: 0,
        }
    ));
}

#[test]
fn rejects_invalid_booleans_trailing_data_and_oversized_writes() {
    let mut invalid_bool = header(1, 1, 1);
    field(&mut invalid_bool, BOOL_TAG, b"active");
    push_u64(&mut invalid_bool, 1);
    column_header(&mut invalid_bool, BOOL_TAG, 1);
    invalid_bool.push(2);
    assert!(matches!(
        decode_error("invalid-bool", &invalid_bool),
        TableSnapshotError::InvalidBooleanValue {
            column: 0,
            row: 0,
            value: 2,
        }
    ));

    let mut trailing = empty_int_table();
    trailing.push(0);
    assert!(matches!(
        decode_error("trailing", &trailing),
        TableSnapshotError::TrailingData { remaining: 1 }
    ));

    let directory = TestDirectory::new("oversized-write");
    let path = directory.snapshot();
    let table = Table::new(vec![Field::new("id", DataType::Int64)]).unwrap();
    let error = SnapshotStore::new(4)
        .write_table(&path, &table)
        .unwrap_err();
    assert!(matches!(
        error,
        TableSnapshotError::Envelope(SnapshotError::Oversized {
            max_payload_len: 4,
            ..
        })
    ));
    assert!(!path.exists());
}

fn decode_error(test_name: &str, payload: &[u8]) -> TableSnapshotError {
    let directory = TestDirectory::new(test_name);
    let path = directory.snapshot();
    let store = SnapshotStore::new(1024);
    store.write(&path, payload).expect("write payload envelope");
    store.read_table(&path).expect_err("payload should fail")
}

fn header(row_limit: u64, row_count: u64, field_count: u64) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&TABLE_PAYLOAD_MAGIC);
    payload.extend_from_slice(&TABLE_PAYLOAD_VERSION.to_le_bytes());
    push_u64(&mut payload, row_limit);
    push_u64(&mut payload, row_count);
    push_u64(&mut payload, field_count);
    payload
}

fn field(payload: &mut Vec<u8>, tag: u8, name: &[u8]) {
    payload.push(tag);
    push_u64(payload, name.len() as u64);
    payload.extend_from_slice(name);
}

fn column_header(payload: &mut Vec<u8>, tag: u8, value_count: u64) {
    payload.push(tag);
    push_u64(payload, value_count);
}

fn empty_int_table() -> Vec<u8> {
    let mut payload = header(0, 0, 1);
    field(&mut payload, INT64_TAG, b"id");
    push_u64(&mut payload, 1);
    column_header(&mut payload, INT64_TAG, 0);
    payload
}

fn push_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}
