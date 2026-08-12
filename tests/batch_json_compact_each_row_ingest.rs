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

fn database(sql_type: &str, row_cap: usize) -> Database {
    let mut database = Database::with_table_limits(TableLimits::new(row_cap, 1, row_cap));
    database
        .execute(&format!("CREATE TABLE readings (value {sql_type});"))
        .unwrap();
    database
}

fn query(database: &mut Database) -> QueryResult {
    match database
        .execute("SELECT value FROM readings;")
        .unwrap()
        .remove(0)
    {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

fn values(database: &mut Database) -> Vec<Vec<Value>> {
    query(database).rows
}

fn limits(input: &[u8], rows: usize) -> JsonCompactEachRowIngestLimits {
    JsonCompactEachRowIngestLimits::new(input.len(), rows, rows)
}

#[test]
fn writer_output_round_trips_signed_int64_and_nullable_rows() {
    let mut integers = database("Int64", 3);
    integers
        .execute("INSERT INTO readings VALUES (-9223372036854775808), (-7), (9223372036854775807);")
        .unwrap();
    let expected_integers = query(&mut integers);
    let mut integer_input = Vec::new();
    write_json_compact_each_row(&mut integer_input, &expected_integers).unwrap();
    assert_eq!(
        integer_input,
        b"[-9223372036854775808]\n[-7]\n[9223372036854775807]\n"
    );

    let mut integer_target = database("Int64", 3);
    assert_eq!(
        integer_target.ingest_json_compact_each_row(
            "readings",
            &integer_input,
            limits(&integer_input, 3),
        ),
        Ok(3),
    );
    assert_eq!(query(&mut integer_target), expected_integers);

    let mut nullable = database("Nullable(Int64)", 3);
    nullable
        .execute("INSERT INTO readings VALUES (-9), (NULL), (11);")
        .unwrap();
    let expected_nullable = query(&mut nullable);
    let mut nullable_input = Vec::new();
    write_json_compact_each_row(&mut nullable_input, &expected_nullable).unwrap();
    assert_eq!(nullable_input, b"[-9]\n[null]\n[11]\n");

    let mut nullable_target = database("Nullable(Int64)", 3);
    assert_eq!(
        nullable_target.ingest_json_compact_each_row(
            "readings",
            &nullable_input,
            limits(&nullable_input, 3),
        ),
        Ok(3),
    );
    assert_eq!(query(&mut nullable_target), expected_nullable);
}

#[test]
fn empty_input_and_all_null_writer_output_are_supported() {
    let mut target = database("Nullable(Int64)", 3);
    assert_eq!(
        target.ingest_json_compact_each_row(
            "readings",
            b"",
            JsonCompactEachRowIngestLimits::new(0, 0, 0),
        ),
        Ok(0),
    );

    let mut source = database("Nullable(Int64)", 3);
    source
        .execute("INSERT INTO readings VALUES (NULL), (NULL), (NULL);")
        .unwrap();
    let expected = query(&mut source);
    let mut input = Vec::new();
    write_json_compact_each_row(&mut input, &expected).unwrap();
    assert_eq!(input, b"[null]\n[null]\n[null]\n");
    assert_eq!(
        target.ingest_json_compact_each_row("readings", &input, limits(&input, 3)),
        Ok(3),
    );
    assert_eq!(query(&mut target), expected);
}

#[test]
fn malformed_late_row_and_integer_overflow_roll_back_every_prepared_row() {
    let mut malformed = database("Int64", 4);
    malformed
        .execute("INSERT INTO readings VALUES (7);")
        .unwrap();
    let input = b"[1]\n[-2]\n[true]\n";
    assert_eq!(
        malformed.ingest_json_compact_each_row("readings", input, limits(input, 3)),
        Err(JsonCompactEachRowIngestError::InvalidValue { line: 3, column: 2 }),
    );
    assert_eq!(values(&mut malformed), [vec![Value::Int64(7)]]);

    let mut overflow = database("Int64", 3);
    overflow
        .execute("INSERT INTO readings VALUES (8);")
        .unwrap();
    let input = b"[1]\n[9223372036854775808]\n";
    assert_eq!(
        overflow.ingest_json_compact_each_row("readings", input, limits(input, 2)),
        Err(JsonCompactEachRowIngestError::IntegerOverflow { line: 2, column: 2 }),
    );
    assert_eq!(values(&mut overflow), [vec![Value::Int64(8)]]);
}

#[test]
fn utf8_json_shape_integer_grammar_line_endings_and_nullability_are_validated() {
    let cases: &[(&[u8], JsonCompactEachRowIngestError)] = &[
        (
            b"\xff",
            JsonCompactEachRowIngestError::InvalidUtf8 { valid_up_to: 0 },
        ),
        (
            b"{}\n",
            JsonCompactEachRowIngestError::InvalidJson { line: 1, column: 1 },
        ),
        (
            b"[]\n",
            JsonCompactEachRowIngestError::WrongValueCount { line: 1, actual: 0 },
        ),
        (
            b"[1,2]\n",
            JsonCompactEachRowIngestError::WrongValueCount { line: 1, actual: 2 },
        ),
        (
            b"[01]\n",
            JsonCompactEachRowIngestError::InvalidValue { line: 1, column: 2 },
        ),
        (
            b"[+1]\n",
            JsonCompactEachRowIngestError::InvalidValue { line: 1, column: 2 },
        ),
        (
            b"[1.0]\n",
            JsonCompactEachRowIngestError::InvalidValue { line: 1, column: 2 },
        ),
        (
            b"[1]\r[2]\n",
            JsonCompactEachRowIngestError::InvalidLineEnding { line: 1 },
        ),
        (
            b"[null]\n",
            JsonCompactEachRowIngestError::NullForNonNullable { line: 1, column: 2 },
        ),
    ];

    for (input, expected) in cases {
        let mut database = database("Int64", 1);
        assert_eq!(
            database.ingest_json_compact_each_row(
                "readings",
                input,
                JsonCompactEachRowIngestLimits::new(input.len(), 1, usize::MAX),
            ),
            Err(expected.clone()),
            "input {:?}",
            String::from_utf8_lossy(input),
        );
        assert!(values(&mut database).is_empty());
    }

    let crlf = b" [ -0 ] \r\n\t[ 2 ]\t\r\n";
    let mut database = database("Int64", 2);
    assert_eq!(
        database.ingest_json_compact_each_row("readings", crlf, limits(crlf, 2)),
        Ok(2),
    );
    assert_eq!(
        values(&mut database),
        [vec![Value::Int64(0)], vec![Value::Int64(2)]]
    );
}

#[test]
fn exact_byte_row_value_and_table_limits_succeed() {
    let input = b"[-1]\n[2]\n";
    let mut database = database("Int64", 2);
    assert_eq!(
        database.ingest_json_compact_each_row(
            "readings",
            input,
            JsonCompactEachRowIngestLimits::new(input.len(), 2, 2),
        ),
        Ok(2),
    );
    assert_eq!(
        values(&mut database),
        [vec![Value::Int64(-1)], vec![Value::Int64(2)]]
    );
}

#[test]
fn byte_row_value_and_table_capacity_failures_preserve_existing_rows() {
    let input = b"[1]\n[2]\n";

    let mut byte_limited = database("Int64", 3);
    byte_limited
        .execute("INSERT INTO readings VALUES (9);")
        .unwrap();
    assert_eq!(
        byte_limited.ingest_json_compact_each_row(
            "readings",
            input,
            JsonCompactEachRowIngestLimits::new(input.len() - 1, 2, 2),
        ),
        Err(JsonCompactEachRowIngestError::ByteLimitExceeded {
            bytes: input.len(),
            max_bytes: input.len() - 1,
        }),
    );

    let mut row_limited = database("Int64", 3);
    row_limited
        .execute("INSERT INTO readings VALUES (9);")
        .unwrap();
    assert_eq!(
        row_limited.ingest_json_compact_each_row(
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

    let mut value_limited = database("Int64", 3);
    value_limited
        .execute("INSERT INTO readings VALUES (9);")
        .unwrap();
    assert_eq!(
        value_limited.ingest_json_compact_each_row(
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

    let mut capacity_limited = database("Int64", 2);
    capacity_limited
        .execute("INSERT INTO readings VALUES (9);")
        .unwrap();
    assert_eq!(
        capacity_limited.ingest_json_compact_each_row(
            "readings",
            input,
            JsonCompactEachRowIngestLimits::new(input.len(), 2, 2),
        ),
        Err(JsonCompactEachRowIngestError::Database(
            Error::ResourceLimitExceeded {
                resource: "table rows",
                actual: 3,
                max: 2,
            }
        )),
    );

    for database in [
        &mut byte_limited,
        &mut row_limited,
        &mut value_limited,
        &mut capacity_limited,
    ] {
        assert_eq!(values(database), [vec![Value::Int64(9)]]);
    }
}

#[test]
fn only_one_physical_int64_column_is_supported() {
    let input = b"[1]\n";
    let mut multiple = Database::new();
    multiple
        .execute("CREATE TABLE metrics (id Int64, other Int64);")
        .unwrap();
    assert_eq!(
        multiple.ingest_json_compact_each_row("metrics", input, limits(input, 1)),
        Err(JsonCompactEachRowIngestError::UnsupportedColumnCount {
            table: "metrics".to_owned(),
            actual: 2,
        }),
    );

    let mut strings = Database::new();
    strings
        .execute("CREATE TABLE labels (value String);")
        .unwrap();
    assert_eq!(
        strings.ingest_json_compact_each_row("labels", input, limits(input, 1)),
        Err(JsonCompactEachRowIngestError::UnsupportedColumnType {
            column: "value".to_owned(),
            data_type: DataType::String,
        }),
    );
}

#[test]
fn shared_blocking_and_nonblocking_apis_append_complete_inputs() {
    let database = SharedDatabase::with_max_rows_per_table(3);
    database
        .execute("CREATE TABLE readings (value Nullable(Int64));")
        .unwrap();
    let first = b"[-1]\n[null]\n";
    let second = b"[2]\n";
    assert_eq!(
        database.ingest_json_compact_each_row("readings", first, limits(first, 2)),
        Ok(2),
    );
    assert_eq!(
        database.try_ingest_json_compact_each_row("readings", second, limits(second, 1)),
        Ok(1),
    );
    assert_eq!(
        database.query("SELECT value FROM readings;").unwrap().rows,
        [
            vec![Value::Int64(-1)],
            vec![Value::Null(DataType::Int64)],
            vec![Value::Int64(2)],
        ]
    );
}

struct InaccessibleInput;

impl AsRef<[u8]> for InaccessibleInput {
    fn as_ref(&self) -> &[u8] {
        panic!("contended ingestion must not access its input")
    }
}

#[test]
fn shared_nonblocking_contention_returns_before_lookup_or_input_access() {
    let inner = Arc::new(RwLock::new(database("Int64", 1)));
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

    let (sender, receiver) = mpsc::channel();
    let blocking_database = database.clone();
    let worker = thread::spawn(move || {
        let input = b"[7]\n";
        sender
            .send(blocking_database.ingest_json_compact_each_row(
                "readings",
                input,
                limits(input, 1),
            ))
            .unwrap();
    });
    assert!(receiver.recv_timeout(Duration::from_millis(100)).is_err());
    drop(reader);
    assert_eq!(receiver.recv_timeout(Duration::from_secs(2)), Ok(Ok(1)));
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
}
