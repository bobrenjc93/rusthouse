use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

use rusthouse::batch::csv::{CsvIngestError, CsvIngestLimits};
use rusthouse::batch::engine::{Database, QueryResult, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::value::{DataType, Value};
use rusthouse::{SharedDatabase, SharedDatabaseError, TableLimits};

fn database_with_limits(limits: TableLimits) -> Database {
    let mut database = Database::with_table_limits(limits);
    database
        .execute("CREATE TABLE metrics (id Int64, score Float64, active Bool, label String);")
        .expect("create typed table");
    database
}

fn database(row_cap: usize) -> Database {
    database_with_limits(TableLimits::new(row_cap, 4, row_cap.saturating_mul(4)))
}

fn query(database: &mut Database, sql: &str) -> QueryResult {
    match database
        .execute(sql)
        .expect("query succeeds")
        .into_iter()
        .next()
        .expect("one result")
    {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

fn ids(database: &mut Database) -> Vec<Vec<Value>> {
    query(database, "SELECT id FROM metrics ORDER BY id;").rows
}

#[test]
fn headerless_csv_ingests_all_types_in_schema_order_with_existing_csv_quoting() {
    let input = concat!(
        "-9223372036854775808,2.5,true,\"comma, \"\"quoted\"\"\nnext\"\r\n",
        "7,-3e2,false,plain\n",
    )
    .as_bytes();
    let mut database = database(2);

    assert_eq!(
        database.ingest_csv("metrics", input, CsvIngestLimits::new(input.len(), 2, 8),),
        Ok(2),
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT id, score, active, label FROM metrics ORDER BY id;",
        )
        .rows,
        [
            vec![
                Value::Int64(i64::MIN),
                Value::Float64(2.5),
                Value::Bool(true),
                Value::String("comma, \"quoted\"\nnext".to_owned()),
            ],
            vec![
                Value::Int64(7),
                Value::Float64(-300.0),
                Value::Bool(false),
                Value::String("plain".to_owned()),
            ],
        ],
    );
}

#[test]
fn headerless_null_token_is_physical_nullable_only_and_strings_retain_it() {
    let mut nullable = Database::new();
    nullable
        .execute("CREATE TABLE readings (value Nullable(Int64));")
        .unwrap();
    let nullable_input = b"-9223372036854775808\nNULL\n9223372036854775807\n";
    assert_eq!(
        nullable.ingest_csv(
            "readings",
            nullable_input,
            CsvIngestLimits::new(nullable_input.len(), 3, 3),
        ),
        Ok(3),
    );
    assert_eq!(
        query(&mut nullable, "SELECT value FROM readings;").rows,
        [
            vec![Value::Int64(i64::MIN)],
            vec![Value::Null(DataType::Int64)],
            vec![Value::Int64(i64::MAX)],
        ]
    );

    let mut required = Database::new();
    required
        .execute("CREATE TABLE required (value Int64);")
        .unwrap();
    assert_eq!(
        required.ingest_csv("required", b"NULL\n", CsvIngestLimits::new(5, 1, 1)),
        Err(CsvIngestError::InvalidValue {
            line: 1,
            column: 1,
            expected: DataType::Int64,
        }),
    );

    let mut strings = Database::new();
    strings
        .execute("CREATE TABLE strings (value String);")
        .unwrap();
    let string_input = b"NULL\n\"NULL\"\n";
    assert_eq!(
        strings.ingest_csv(
            "strings",
            string_input,
            CsvIngestLimits::new(string_input.len(), 2, 2),
        ),
        Ok(2),
    );
    assert_eq!(
        query(&mut strings, "SELECT value FROM strings;").rows,
        [
            vec![Value::String("NULL".to_owned())],
            vec![Value::String("NULL".to_owned())],
        ]
    );
}

#[test]
fn exact_input_and_remaining_table_limits_succeed_and_empty_input_is_a_no_op() {
    let mut database = database_with_limits(TableLimits::new(3, 4, 12));
    database
        .execute("INSERT INTO metrics VALUES (0, 0.0, false, 'existing');")
        .unwrap();
    let input = b"1,1.5,true,one\n2,2.5,false,two\n";

    assert_eq!(
        database.ingest_csv("metrics", input, CsvIngestLimits::new(input.len(), 2, 8),),
        Ok(2),
    );
    assert_eq!(
        database.ingest_csv("metrics", b"", CsvIngestLimits::new(0, 0, 0)),
        Ok(0),
    );
    assert_eq!(
        ids(&mut database),
        [
            vec![Value::Int64(0)],
            vec![Value::Int64(1)],
            vec![Value::Int64(2)],
        ],
    );
}

#[test]
fn every_headerless_record_is_data_and_late_validation_failures_are_atomic() {
    let valid_then_bad_type = b"1,1.5,true,valid\n2,NaN,false,late\n";
    let valid_two_rows = b"1,1.5,true,one\n2,2.5,false,two\n";

    let cases = [
        (
            valid_then_bad_type.as_slice(),
            CsvIngestLimits::new(valid_then_bad_type.len(), 2, 8),
            CsvIngestError::InvalidValue {
                line: 2,
                column: 2,
                expected: DataType::Float64,
            },
        ),
        (
            valid_two_rows.as_slice(),
            CsvIngestLimits::new(valid_two_rows.len(), 1, 8),
            CsvIngestError::RowLimitExceeded {
                line: 2,
                rows: 2,
                max_rows: 1,
            },
        ),
        (
            valid_two_rows.as_slice(),
            CsvIngestLimits::new(valid_two_rows.len(), 2, 7),
            CsvIngestError::ValueLimitExceeded {
                line: 2,
                values: 8,
                max_values: 7,
            },
        ),
    ];

    for (input, limits, expected) in cases {
        let mut database = database(4);
        database
            .execute("INSERT INTO metrics VALUES (9, 9.0, true, 'existing');")
            .unwrap();
        assert_eq!(database.ingest_csv("metrics", input, limits), Err(expected));
        assert_eq!(ids(&mut database), [vec![Value::Int64(9)]]);
    }

    let mut byte_limited = database(4);
    byte_limited
        .execute("INSERT INTO metrics VALUES (9, 9.0, true, 'existing');")
        .unwrap();
    assert_eq!(
        byte_limited.ingest_csv(
            "metrics",
            valid_two_rows,
            CsvIngestLimits::new(valid_two_rows.len() - 1, 2, 8),
        ),
        Err(CsvIngestError::ByteLimitExceeded {
            bytes: valid_two_rows.len(),
            max_bytes: valid_two_rows.len() - 1,
        }),
    );
    assert_eq!(ids(&mut byte_limited), [vec![Value::Int64(9)]]);

    let mut capacity_limited = database(2);
    capacity_limited
        .execute("INSERT INTO metrics VALUES (9, 9.0, true, 'existing');")
        .unwrap();
    assert_eq!(
        capacity_limited.ingest_csv(
            "metrics",
            valid_two_rows,
            CsvIngestLimits::new(valid_two_rows.len(), 2, 8),
        ),
        Err(CsvIngestError::Database(Error::ResourceLimitExceeded {
            resource: "table rows",
            actual: 3,
            max: 2,
        })),
    );
    assert_eq!(ids(&mut capacity_limited), [vec![Value::Int64(9)]]);

    let mut cell_limited = database_with_limits(TableLimits::new(4, 4, 8));
    cell_limited
        .execute("INSERT INTO metrics VALUES (9, 9.0, true, 'existing');")
        .unwrap();
    assert_eq!(
        cell_limited.ingest_csv(
            "metrics",
            valid_two_rows,
            CsvIngestLimits::new(valid_two_rows.len(), 2, 8),
        ),
        Err(CsvIngestError::Database(Error::ResourceLimitExceeded {
            resource: "table cells",
            actual: 12,
            max: 8,
        })),
    );
    assert_eq!(ids(&mut cell_limited), [vec![Value::Int64(9)]]);

    let mut header_is_data = database(1);
    let input = b"id,score,active,label\n";
    assert_eq!(
        header_is_data.ingest_csv("metrics", input, CsvIngestLimits::new(input.len(), 1, 4),),
        Err(CsvIngestError::InvalidValue {
            line: 1,
            column: 1,
            expected: DataType::Int64,
        }),
    );
}

#[test]
fn shared_blocking_and_nonblocking_headerless_apis_append_complete_inputs() {
    let database = SharedDatabase::with_max_rows_per_table(2);
    database
        .execute("CREATE TABLE metrics (id Int64, score Float64, active Bool, label String);")
        .unwrap();
    let first = b"1,1.5,true,one\n";
    let second = b"2,2.5,false,two\n";

    assert_eq!(
        database.ingest_csv("metrics", first, CsvIngestLimits::new(first.len(), 1, 4),),
        Ok(1),
    );
    assert_eq!(
        database.try_ingest_csv("metrics", second, CsvIngestLimits::new(second.len(), 1, 4),),
        Ok(1),
    );
    assert_eq!(
        database
            .query("SELECT id FROM metrics ORDER BY id;")
            .unwrap()
            .rows,
        [vec![Value::Int64(1)], vec![Value::Int64(2)]],
    );
}

struct InaccessibleInput;

impl AsRef<[u8]> for InaccessibleInput {
    fn as_ref(&self) -> &[u8] {
        panic!("a contended nonblocking ingestion must not access its input")
    }
}

#[test]
fn shared_headerless_apis_have_blocking_and_immediate_contention_semantics() {
    let initial = database(1);
    let inner = Arc::new(RwLock::new(initial));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));

    let reader = inner.read().unwrap();
    assert_eq!(
        database.try_ingest_csv("missing", InaccessibleInput, CsvIngestLimits::new(0, 0, 0),),
        Err(SharedDatabaseError::DatabaseBusy),
    );

    let (sender, receiver) = mpsc::channel();
    let blocking_database = database.clone();
    let worker = thread::spawn(move || {
        let input = b"1,1.0,true,ready\n";
        sender
            .send(blocking_database.ingest_csv(
                "metrics",
                input,
                CsvIngestLimits::new(input.len(), 1, 4),
            ))
            .unwrap();
    });
    assert!(
        receiver.recv_timeout(Duration::from_millis(100)).is_err(),
        "the blocking API must wait for the active reader",
    );
    drop(reader);
    assert_eq!(
        receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
        Ok(1)
    );
    worker.join().unwrap();

    let writer = inner.write().unwrap();
    assert_eq!(
        database.try_ingest_csv("missing", InaccessibleInput, CsvIngestLimits::new(0, 0, 0),),
        Err(SharedDatabaseError::DatabaseBusy),
    );
    drop(writer);

    assert_eq!(
        database.query("SELECT id FROM metrics;").unwrap().rows,
        [vec![Value::Int64(1)]],
    );
}
