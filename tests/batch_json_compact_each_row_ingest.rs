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

fn float_bits(database: &mut Database, table: &str) -> Vec<u64> {
    values(database, table)
        .into_iter()
        .map(|row| match row.as_slice() {
            [Value::Float64(value)] => value.to_bits(),
            _ => panic!("expected one Float64 value"),
        })
        .collect()
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
fn writer_output_round_trips_float64_extrema_subnormals_and_signed_zero_exactly() {
    let expected = [
        f64::MIN,
        -7.25,
        -0.0,
        0.0,
        f64::from_bits(1),
        -f64::from_bits(1),
        f64::MIN_POSITIVE,
        f64::MAX,
    ];
    let sql_values = expected
        .iter()
        .map(|value| format!("({})", Value::Float64(*value).as_display_string()))
        .collect::<Vec<_>>()
        .join(", ");
    let mut source = Database::new();
    source
        .execute(&format!(
            "CREATE TABLE source (value Float64); INSERT INTO source VALUES {sql_values};"
        ))
        .unwrap();
    assert_eq!(
        float_bits(&mut source, "source"),
        expected
            .iter()
            .map(|value| value.to_bits())
            .collect::<Vec<_>>()
    );

    let result = query(&mut source, "SELECT value FROM source;");
    let mut input = Vec::new();
    write_json_compact_each_row(&mut input, &result).unwrap();

    let mut target = Database::with_max_rows_per_table(expected.len());
    target
        .execute("CREATE TABLE target (value Float64);")
        .unwrap();
    assert_eq!(
        target.ingest_json_compact_each_row(
            "target",
            &input,
            JsonCompactEachRowIngestLimits::new(input.len(), expected.len(), expected.len()),
        ),
        Ok(expected.len()),
    );
    assert_eq!(
        float_bits(&mut target, "target"),
        expected
            .iter()
            .map(|value| value.to_bits())
            .collect::<Vec<_>>()
    );
}

#[test]
fn float64_accepts_every_finite_json_number_form() {
    let input = concat!(
        "[42]\n",
        "[-7.25]\n",
        "[6.022e23]\n",
        "[1.7976931348623157e308]\n",
        "[-1.7976931348623157E+308]\n",
        "[5e-324]\n",
        "[0]\n",
        "[-0]\n",
        "[0.0]\n",
        "[-0.0]\n",
        "[0e0]\n",
        "[-0E+7]\n",
    );
    let expected = [
        42.0,
        -7.25,
        6.022e23,
        f64::MAX,
        f64::MIN,
        f64::from_bits(1),
        0.0,
        -0.0,
        0.0,
        -0.0,
        0.0,
        -0.0,
    ];
    let mut database = Database::with_max_rows_per_table(expected.len());
    database
        .execute("CREATE TABLE readings (value Float64);")
        .unwrap();

    assert_eq!(
        database.ingest_json_compact_each_row(
            "readings",
            input,
            JsonCompactEachRowIngestLimits::new(input.len(), expected.len(), expected.len()),
        ),
        Ok(expected.len()),
    );
    assert_eq!(
        float_bits(&mut database, "readings"),
        expected
            .iter()
            .map(|value| value.to_bits())
            .collect::<Vec<_>>()
    );
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

    for floating_number in [b"[1]\n[2.0]\n".as_slice(), b"[1]\n[2e0]\n".as_slice()] {
        assert_eq!(
            database.ingest_json_compact_each_row(
                "readings",
                floating_number,
                JsonCompactEachRowIngestLimits::new(floating_number.len(), 2, 2),
            ),
            Err(JsonCompactEachRowIngestError::InvalidValue {
                line: 2,
                column: 1,
                expected: DataType::Int64,
            }),
        );
    }

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
fn float64_late_overflow_null_and_nonnumeric_values_roll_back_every_prepared_row() {
    let mut database = Database::with_max_rows_per_table(3);
    database
        .execute("CREATE TABLE readings (value Float64); INSERT INTO readings VALUES (9.5);")
        .unwrap();

    let overflow = b"[1.25]\n[1e309]\n";
    assert_eq!(
        database.ingest_json_compact_each_row(
            "readings",
            overflow,
            JsonCompactEachRowIngestLimits::new(overflow.len(), 2, 2),
        ),
        Err(JsonCompactEachRowIngestError::FloatOverflow { line: 2, column: 1 }),
    );

    let null = b"[1.25]\n[null]\n";
    assert_eq!(
        database.ingest_json_compact_each_row(
            "readings",
            null,
            JsonCompactEachRowIngestLimits::new(null.len(), 2, 2),
        ),
        Err(JsonCompactEachRowIngestError::NullNotAllowed { line: 2, column: 1 }),
    );

    for input in [
        b"[1.25]\n[\"2.5\"]\n".as_slice(),
        b"[1.25]\n[true]\n".as_slice(),
        b"[1.25]\n[{\"number\":2.5}]\n".as_slice(),
        b"[1.25]\n[[2.5]]\n".as_slice(),
    ] {
        assert_eq!(
            database.ingest_json_compact_each_row(
                "readings",
                input,
                JsonCompactEachRowIngestLimits::new(input.len(), 2, 2),
            ),
            Err(JsonCompactEachRowIngestError::InvalidValue {
                line: 2,
                column: 1,
                expected: DataType::Float64,
            }),
        );
    }

    assert_eq!(float_bits(&mut database, "readings"), [9.5_f64.to_bits()]);
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
fn float64_exact_input_and_table_limits_succeed_and_excess_is_atomic() {
    let mut database = Database::with_table_limits(TableLimits::new(3, 1, 3));
    database
        .execute("CREATE TABLE readings (value Float64); INSERT INTO readings VALUES (9.5);")
        .unwrap();
    let input = b"[-0]\n[5e-324]\n";
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

    let one_more = b"[3.5]\n";
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
        float_bits(&mut database, "readings"),
        [
            9.5_f64.to_bits(),
            (-0.0_f64).to_bits(),
            f64::from_bits(1).to_bits()
        ]
    );
}

#[test]
fn rejects_targets_outside_the_one_column_numeric_subset_without_mutation() {
    let input = b"[1]\n";
    let limits = JsonCompactEachRowIngestLimits::new(input.len(), 1, 1);
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE wide (value Int64, other Int64); \
             CREATE TABLE text_value (value String);",
        )
        .unwrap();

    assert_eq!(
        database.ingest_json_compact_each_row("wide", input, limits),
        Err(JsonCompactEachRowIngestError::UnsupportedColumnCount { actual: 2 }),
    );
    assert_eq!(
        database.ingest_json_compact_each_row("text_value", input, limits),
        Err(JsonCompactEachRowIngestError::UnsupportedColumnType {
            column: "value".to_owned(),
            actual: DataType::String,
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
        .execute("CREATE TABLE readings (value Int64);")
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
        let input = b"[1]\n";
        sender
            .send(blocking_database.ingest_json_compact_each_row(
                "readings",
                input,
                JsonCompactEachRowIngestLimits::new(input.len(), 1, 1),
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

    let second = b"[-2]\n";
    assert_eq!(
        database.try_ingest_json_compact_each_row(
            "readings",
            second,
            JsonCompactEachRowIngestLimits::new(second.len(), 1, 1),
        ),
        Ok(1),
    );
    let malformed = b"[3]\n[late]\n";
    assert!(matches!(
        database.try_ingest_json_compact_each_row(
            "readings",
            malformed,
            JsonCompactEachRowIngestLimits::new(malformed.len(), 2, 2),
        ),
        Err(SharedDatabaseError::JsonCompactEachRowIngest(
            JsonCompactEachRowIngestError::InvalidJson { line: 2, .. }
        ))
    ));
    assert_eq!(
        database.query("SELECT value FROM readings;").unwrap().rows,
        [vec![Value::Int64(1)], vec![Value::Int64(-2)]],
    );
}

#[test]
fn float64_shared_apis_retain_the_lock_through_validation_and_atomic_append() {
    let mut initial = Database::with_max_rows_per_table(3);
    initial
        .execute("CREATE TABLE readings (value Float64);")
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
        let input = b"[1]\n";
        sender
            .send(blocking_database.ingest_json_compact_each_row(
                "readings",
                input,
                JsonCompactEachRowIngestLimits::new(input.len(), 1, 1),
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

    let subnormal = b"[5e-324]\n";
    assert_eq!(
        database.try_ingest_json_compact_each_row(
            "readings",
            subnormal,
            JsonCompactEachRowIngestLimits::new(subnormal.len(), 1, 1),
        ),
        Ok(1),
    );
    let late_null = b"[3.5]\n[null]\n";
    assert_eq!(
        database.try_ingest_json_compact_each_row(
            "readings",
            late_null,
            JsonCompactEachRowIngestLimits::new(late_null.len(), 2, 2),
        ),
        Err(SharedDatabaseError::JsonCompactEachRowIngest(
            JsonCompactEachRowIngestError::NullNotAllowed { line: 2, column: 1 }
        )),
    );
    let result = database.query("SELECT value FROM readings;").unwrap();
    assert_eq!(
        result
            .rows
            .iter()
            .map(|row| match row.as_slice() {
                [Value::Float64(value)] => value.to_bits(),
                _ => panic!("expected one Float64 value"),
            })
            .collect::<Vec<_>>(),
        [1.0_f64.to_bits(), f64::from_bits(1).to_bits()],
    );
}
