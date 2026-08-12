use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

use rusthouse::batch::engine::{Database, QueryResult, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::format::write_json_compact_each_row;
use rusthouse::batch::json_compact_each_row::{
    JsonCompactEachRowIngestError, JsonCompactEachRowIngestLimits,
};
use rusthouse::batch::value::{DataType, Value};
use rusthouse::{SharedDatabase, SharedDatabaseError, TableLimits};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    let [StatementResult::Query(result)] = results.as_slice() else {
        panic!("expected one query result");
    };
    result.clone()
}

fn values(database: &mut Database, table: &str) -> Vec<Vec<Value>> {
    query(database, &format!("SELECT value FROM {table};")).rows
}

fn pair_values(database: &mut Database, table: &str) -> Vec<Vec<Value>> {
    query(
        database,
        &format!("SELECT left_value, right_value FROM {table};"),
    )
    .rows
}

#[test]
fn writer_output_round_trips_signed_int64_extremes() {
    let mut source = Database::new();
    source
        .execute(
            "CREATE TABLE source (value Int64); \
             INSERT INTO source VALUES \
             (-9223372036854775808), (-7), (0), (9223372036854775807);",
        )
        .unwrap();
    let result = query(&mut source, "SELECT value FROM source;");
    let mut input = Vec::new();
    write_json_compact_each_row(&mut input, &result).unwrap();
    assert_eq!(
        input,
        b"[-9223372036854775808]\n[-7]\n[0]\n[9223372036854775807]\n"
    );

    let mut target = Database::with_max_rows_per_table(4);
    target
        .execute("CREATE TABLE target (value Int64);")
        .unwrap();
    assert_eq!(
        target.ingest_json_compact_each_row(
            "target",
            &input,
            JsonCompactEachRowIngestLimits::new(input.len(), 4, 4),
        ),
        Ok(4),
    );
    assert_eq!(values(&mut target, "target"), result.rows);
}

#[test]
fn two_column_writer_output_round_trips_signed_int64_extremes() {
    let mut source = Database::new();
    source
        .execute(
            "CREATE TABLE source (left_value Int64, right_value Int64); \
             INSERT INTO source VALUES \
             (-9223372036854775808, 9223372036854775807), \
             (9223372036854775807, -9223372036854775808), (-7, 0);",
        )
        .unwrap();
    let result = query(&mut source, "SELECT left_value, right_value FROM source;");
    let mut input = Vec::new();
    write_json_compact_each_row(&mut input, &result).unwrap();
    assert_eq!(
        input,
        concat!(
            "[-9223372036854775808,9223372036854775807]\n",
            "[9223372036854775807,-9223372036854775808]\n",
            "[-7,0]\n",
        )
        .as_bytes(),
    );

    let mut target = Database::with_table_limits(TableLimits::new(3, 2, 6));
    target
        .execute("CREATE TABLE target (left_value Int64, right_value Int64);")
        .unwrap();
    assert_eq!(
        target.ingest_json_compact_each_row(
            "target",
            &input,
            JsonCompactEachRowIngestLimits::new(input.len(), 3, 6),
        ),
        Ok(3),
    );
    assert_eq!(pair_values(&mut target, "target"), result.rows);
}

#[test]
fn empty_and_all_null_writer_inputs_round_trip_for_nullable_int64() {
    let mut source = Database::new();
    source
        .execute(
            "CREATE TABLE source (value Nullable(Int64)); \
             INSERT INTO source VALUES (NULL), (NULL);",
        )
        .unwrap();
    let result = query(&mut source, "SELECT value FROM source;");
    let mut input = Vec::new();
    write_json_compact_each_row(&mut input, &result).unwrap();
    assert_eq!(input, b"[null]\n[null]\n");

    let mut target = Database::with_max_rows_per_table(2);
    target
        .execute("CREATE TABLE target (value Nullable(Int64));")
        .unwrap();
    assert_eq!(
        target.ingest_json_compact_each_row(
            "target",
            b"",
            JsonCompactEachRowIngestLimits::new(0, 0, 0),
        ),
        Ok(0),
    );
    assert_eq!(
        target.ingest_json_compact_each_row(
            "target",
            &input,
            JsonCompactEachRowIngestLimits::new(input.len(), 2, 2),
        ),
        Ok(2),
    );
    assert_eq!(values(&mut target, "target"), result.rows);

    let mut non_nullable = Database::new();
    non_nullable
        .execute("CREATE TABLE target (value Int64); INSERT INTO target VALUES (9);")
        .unwrap();
    let null = b"[null]\n";
    assert_eq!(
        non_nullable.ingest_json_compact_each_row(
            "target",
            null,
            JsonCompactEachRowIngestLimits::new(null.len(), 1, 1),
        ),
        Err(JsonCompactEachRowIngestError::NullNotAllowed { line: 1, column: 1 }),
    );
    assert_eq!(values(&mut non_nullable, "target"), [vec![Value::Int64(9)]]);
}

#[test]
fn late_malformed_rows_overflow_and_invalid_utf8_roll_back_every_prepared_row() {
    let mut database = Database::with_max_rows_per_table(4);
    database
        .execute("CREATE TABLE readings (value Int64); INSERT INTO readings VALUES (9);")
        .unwrap();

    let overflow = b"[1]\n[-9223372036854775809]\n";
    assert_eq!(
        database.ingest_json_compact_each_row(
            "readings",
            overflow,
            JsonCompactEachRowIngestLimits::new(overflow.len(), 2, 2),
        ),
        Err(JsonCompactEachRowIngestError::IntegerOverflow { line: 2, column: 1 }),
    );

    let wrong_width = b"[1]\n[2,3]\n";
    assert_eq!(
        database.ingest_json_compact_each_row(
            "readings",
            wrong_width,
            JsonCompactEachRowIngestLimits::new(wrong_width.len(), 2, 3),
        ),
        Err(JsonCompactEachRowIngestError::WrongColumnCount {
            line: 2,
            expected: 1,
            actual: 2,
        }),
    );

    let three_values = b"[2,3,4]\n";
    assert_eq!(
        database.ingest_json_compact_each_row(
            "readings",
            three_values,
            JsonCompactEachRowIngestLimits::new(three_values.len(), 1, 2),
        ),
        Err(JsonCompactEachRowIngestError::ValueLimitExceeded {
            line: 1,
            values: 3,
            max_values: 2,
        }),
    );
    assert_eq!(
        database.ingest_json_compact_each_row(
            "readings",
            three_values,
            JsonCompactEachRowIngestLimits::new(three_values.len(), 1, 3),
        ),
        Err(JsonCompactEachRowIngestError::WrongColumnCount {
            line: 1,
            expected: 1,
            actual: 3,
        }),
    );

    let malformed_token = b"[1]\n[late]\n";
    assert!(matches!(
        database.ingest_json_compact_each_row(
            "readings",
            malformed_token,
            JsonCompactEachRowIngestLimits::new(malformed_token.len(), 2, 2),
        ),
        Err(JsonCompactEachRowIngestError::InvalidJson { line: 2, .. })
    ));
    let malformed_wide_array = b"[1,2,late]\n";
    assert!(matches!(
        database.ingest_json_compact_each_row(
            "readings",
            malformed_wide_array,
            JsonCompactEachRowIngestLimits::new(malformed_wide_array.len(), 1, 3),
        ),
        Err(JsonCompactEachRowIngestError::InvalidJson { line: 1, .. })
    ));

    let empty_array = b"[1]\n[]\n";
    assert_eq!(
        database.ingest_json_compact_each_row(
            "readings",
            empty_array,
            JsonCompactEachRowIngestLimits::new(empty_array.len(), 2, 2),
        ),
        Err(JsonCompactEachRowIngestError::WrongColumnCount {
            line: 2,
            expected: 1,
            actual: 0,
        }),
    );

    let invalid_value = b"[1]\n[\"2\"]\n";
    assert_eq!(
        database.ingest_json_compact_each_row(
            "readings",
            invalid_value,
            JsonCompactEachRowIngestLimits::new(invalid_value.len(), 2, 2),
        ),
        Err(JsonCompactEachRowIngestError::InvalidValue {
            line: 2,
            column: 1,
            expected: DataType::Int64,
        }),
    );

    for valid_but_unsupported in [
        b"[true]\n".as_slice(),
        b"[{\"nested\":[1,null,false]}]\n".as_slice(),
        b"[[1,2,3]]\n".as_slice(),
    ] {
        assert_eq!(
            database.ingest_json_compact_each_row(
                "readings",
                valid_but_unsupported,
                JsonCompactEachRowIngestLimits::new(valid_but_unsupported.len(), 1, 1),
            ),
            Err(JsonCompactEachRowIngestError::InvalidValue {
                line: 1,
                column: 1,
                expected: DataType::Int64,
            }),
        );
    }

    let invalid_shape = b"[1]\n[2] trailing\n";
    assert!(matches!(
        database.ingest_json_compact_each_row(
            "readings",
            invalid_shape,
            JsonCompactEachRowIngestLimits::new(invalid_shape.len(), 2, 2),
        ),
        Err(JsonCompactEachRowIngestError::InvalidJson { line: 2, .. })
    ));

    let invalid_utf8 = b"[1]\n[\xff]\n";
    assert_eq!(
        database.ingest_json_compact_each_row(
            "readings",
            invalid_utf8,
            JsonCompactEachRowIngestLimits::new(invalid_utf8.len(), 2, 2),
        ),
        Err(JsonCompactEachRowIngestError::InvalidUtf8 { valid_up_to: 5 }),
    );
    assert_eq!(values(&mut database, "readings"), [vec![Value::Int64(9)]],);
}

#[test]
fn two_column_wrong_widths_types_and_late_failures_roll_back_the_batch() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE pairs (left_value Int64, right_value Int64); \
             INSERT INTO pairs VALUES (9, 90);",
        )
        .unwrap();

    for (input, expected) in [
        (
            b"[1]\n".as_slice(),
            JsonCompactEachRowIngestError::WrongColumnCount {
                line: 1,
                expected: 2,
                actual: 1,
            },
        ),
        (
            b"[1,2,3]\n".as_slice(),
            JsonCompactEachRowIngestError::WrongColumnCount {
                line: 1,
                expected: 2,
                actual: 3,
            },
        ),
    ] {
        assert_eq!(
            database.ingest_json_compact_each_row(
                "pairs",
                input,
                JsonCompactEachRowIngestLimits::new(input.len(), 1, 3),
            ),
            Err(expected),
        );
    }

    let invalid_second = b"[1,\"2\"]\n";
    assert_eq!(
        database.ingest_json_compact_each_row(
            "pairs",
            invalid_second,
            JsonCompactEachRowIngestLimits::new(invalid_second.len(), 1, 2),
        ),
        Err(JsonCompactEachRowIngestError::InvalidValue {
            line: 1,
            column: 2,
            expected: DataType::Int64,
        }),
    );

    let null_second = b"[1,null]\n";
    assert_eq!(
        database.ingest_json_compact_each_row(
            "pairs",
            null_second,
            JsonCompactEachRowIngestLimits::new(null_second.len(), 1, 2),
        ),
        Err(JsonCompactEachRowIngestError::NullNotAllowed { line: 1, column: 2 }),
    );

    let late_overflow = b"[1,2]\n[3,9223372036854775808]\n";
    assert_eq!(
        database.ingest_json_compact_each_row(
            "pairs",
            late_overflow,
            JsonCompactEachRowIngestLimits::new(late_overflow.len(), 2, 4),
        ),
        Err(JsonCompactEachRowIngestError::IntegerOverflow { line: 2, column: 2 }),
    );

    let late_malformed = b"[1,2]\n[3,false]\n";
    assert_eq!(
        database.ingest_json_compact_each_row(
            "pairs",
            late_malformed,
            JsonCompactEachRowIngestLimits::new(late_malformed.len(), 2, 4),
        ),
        Err(JsonCompactEachRowIngestError::InvalidValue {
            line: 2,
            column: 2,
            expected: DataType::Int64,
        }),
    );

    assert_eq!(
        pair_values(&mut database, "pairs"),
        [vec![Value::Int64(9), Value::Int64(90)]],
    );
}

#[test]
fn exact_input_and_table_limits_succeed_and_every_exceeded_limit_is_atomic() {
    let mut database = Database::with_table_limits(TableLimits::new(3, 1, 3));
    database
        .execute("CREATE TABLE readings (value Int64); INSERT INTO readings VALUES (0);")
        .unwrap();
    let input = b"[1]\n[2]\n";
    assert_eq!(
        database.ingest_json_compact_each_row(
            "readings",
            input,
            JsonCompactEachRowIngestLimits::new(input.len(), 2, 2),
        ),
        Ok(2),
    );

    assert_eq!(
        database.ingest_json_compact_each_row(
            "readings",
            input,
            JsonCompactEachRowIngestLimits::new(input.len() - 1, 2, 2),
        ),
        Err(JsonCompactEachRowIngestError::ByteLimitExceeded {
            bytes: input.len(),
            max_bytes: input.len() - 1,
        }),
    );
    assert_eq!(
        database.ingest_json_compact_each_row(
            "readings",
            input,
            JsonCompactEachRowIngestLimits::new(input.len(), 1, 2),
        ),
        Err(JsonCompactEachRowIngestError::RowLimitExceeded {
            line: 2,
            rows: 2,
            max_rows: 1,
        }),
    );
    assert_eq!(
        database.ingest_json_compact_each_row(
            "readings",
            input,
            JsonCompactEachRowIngestLimits::new(input.len(), 2, 1),
        ),
        Err(JsonCompactEachRowIngestError::ValueLimitExceeded {
            line: 2,
            values: 2,
            max_values: 1,
        }),
    );

    let one_more = b"[3]\n";
    assert_eq!(
        database.ingest_json_compact_each_row(
            "readings",
            one_more,
            JsonCompactEachRowIngestLimits::new(one_more.len(), 1, 1),
        ),
        Err(JsonCompactEachRowIngestError::Database(
            Error::ResourceLimitExceeded {
                resource: "table rows",
                actual: 4,
                max: 3,
            }
        )),
    );
    assert_eq!(
        values(&mut database, "readings"),
        [
            vec![Value::Int64(0)],
            vec![Value::Int64(1)],
            vec![Value::Int64(2)],
        ],
    );
}

#[test]
fn two_column_byte_row_value_and_cell_bounds_are_atomic() {
    let mut database = Database::with_table_limits(TableLimits::new(3, 2, 4));
    database
        .execute("CREATE TABLE pairs (left_value Int64, right_value Int64);")
        .unwrap();
    let input = b"[1,10]\n[2,20]\n";

    assert_eq!(
        database.ingest_json_compact_each_row(
            "pairs",
            input,
            JsonCompactEachRowIngestLimits::new(input.len() - 1, 2, 4),
        ),
        Err(JsonCompactEachRowIngestError::ByteLimitExceeded {
            bytes: input.len(),
            max_bytes: input.len() - 1,
        }),
    );
    assert_eq!(
        database.ingest_json_compact_each_row(
            "pairs",
            input,
            JsonCompactEachRowIngestLimits::new(input.len(), 1, 4),
        ),
        Err(JsonCompactEachRowIngestError::RowLimitExceeded {
            line: 2,
            rows: 2,
            max_rows: 1,
        }),
    );
    assert_eq!(
        database.ingest_json_compact_each_row(
            "pairs",
            input,
            JsonCompactEachRowIngestLimits::new(input.len(), 2, 3),
        ),
        Err(JsonCompactEachRowIngestError::ValueLimitExceeded {
            line: 2,
            values: 4,
            max_values: 3,
        }),
    );
    assert!(pair_values(&mut database, "pairs").is_empty());

    assert_eq!(
        database.ingest_json_compact_each_row(
            "pairs",
            input,
            JsonCompactEachRowIngestLimits::new(input.len(), 2, 4),
        ),
        Ok(2),
    );
    let one_more = b"[3,30]\n";
    assert_eq!(
        database.ingest_json_compact_each_row(
            "pairs",
            one_more,
            JsonCompactEachRowIngestLimits::new(one_more.len(), 1, 2),
        ),
        Err(JsonCompactEachRowIngestError::Database(
            Error::ResourceLimitExceeded {
                resource: "table cells",
                actual: 6,
                max: 4,
            }
        )),
    );
    assert_eq!(
        pair_values(&mut database, "pairs"),
        [
            vec![Value::Int64(1), Value::Int64(10)],
            vec![Value::Int64(2), Value::Int64(20)],
        ],
    );
}

#[test]
fn rejects_targets_outside_the_one_or_two_column_int64_subset_without_mutation() {
    let input = b"[1]\n";
    let limits = JsonCompactEachRowIngestLimits::new(input.len(), 1, 1);
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE wide (value Int64, other Int64, third Int64); \
             CREATE TABLE text_value (value String); \
             CREATE TABLE mixed_pair (left_value Int64, right_value String); \
             CREATE TABLE nullable_pair (left_value Int64, right_value Nullable(Int64));",
        )
        .unwrap();

    assert_eq!(
        database.ingest_json_compact_each_row("wide", input, limits),
        Err(JsonCompactEachRowIngestError::UnsupportedColumnCount { actual: 3 }),
    );
    assert_eq!(
        database.ingest_json_compact_each_row("text_value", input, limits),
        Err(JsonCompactEachRowIngestError::UnsupportedColumnType {
            column: "value".to_owned(),
            actual: DataType::String,
        }),
    );
    let pair_input = b"[1,2]\n";
    let pair_limits = JsonCompactEachRowIngestLimits::new(pair_input.len(), 1, 2);
    assert_eq!(
        database.ingest_json_compact_each_row("mixed_pair", pair_input, pair_limits),
        Err(JsonCompactEachRowIngestError::UnsupportedColumnType {
            column: "right_value".to_owned(),
            actual: DataType::String,
        }),
    );
    assert_eq!(
        database.ingest_json_compact_each_row("nullable_pair", pair_input, pair_limits),
        Err(JsonCompactEachRowIngestError::UnsupportedNullableColumn {
            column: "right_value".to_owned(),
        }),
    );
    assert!(values(&mut database, "text_value").is_empty());
}

struct InaccessibleInput;

impl AsRef<[u8]> for InaccessibleInput {
    fn as_ref(&self) -> &[u8] {
        panic!("contended nonblocking ingestion must not access its input")
    }
}

#[test]
fn shared_blocking_and_nonblocking_apis_preserve_contention_and_rollback_semantics() {
    let mut initial = Database::with_max_rows_per_table(3);
    initial
        .execute("CREATE TABLE readings (left_value Int64, right_value Int64);")
        .unwrap();
    let inner = Arc::new(RwLock::new(initial));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));

    let reader = inner.read().unwrap();
    assert_eq!(
        database.try_ingest_json_compact_each_row(
            "missing",
            InaccessibleInput,
            JsonCompactEachRowIngestLimits::new(0, 0, 0),
        ),
        Err(SharedDatabaseError::DatabaseBusy),
    );

    let blocking_database = database.clone();
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let input = b"[1,10]\n";
        sender
            .send(blocking_database.ingest_json_compact_each_row(
                "readings",
                input,
                JsonCompactEachRowIngestLimits::new(input.len(), 1, 2),
            ))
            .unwrap();
    });
    assert!(receiver.recv_timeout(Duration::from_millis(100)).is_err());
    drop(reader);
    assert_eq!(
        receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
        Ok(1),
    );
    worker.join().unwrap();

    let writer = inner.write().unwrap();
    assert_eq!(
        database.try_ingest_json_compact_each_row(
            "missing",
            InaccessibleInput,
            JsonCompactEachRowIngestLimits::new(0, 0, 0),
        ),
        Err(SharedDatabaseError::DatabaseBusy),
    );
    drop(writer);

    let second = b"[-2,-20]\n";
    assert_eq!(
        database.try_ingest_json_compact_each_row(
            "readings",
            second,
            JsonCompactEachRowIngestLimits::new(second.len(), 1, 2),
        ),
        Ok(1),
    );
    let malformed = b"[3,30]\n[4,late]\n";
    assert!(matches!(
        database.try_ingest_json_compact_each_row(
            "readings",
            malformed,
            JsonCompactEachRowIngestLimits::new(malformed.len(), 2, 4),
        ),
        Err(SharedDatabaseError::JsonCompactEachRowIngest(
            JsonCompactEachRowIngestError::InvalidJson { line: 2, .. }
        ))
    ));
    assert_eq!(
        database
            .query("SELECT left_value, right_value FROM readings;")
            .unwrap()
            .rows,
        [
            vec![Value::Int64(1), Value::Int64(10)],
            vec![Value::Int64(-2), Value::Int64(-20)],
        ],
    );
}
