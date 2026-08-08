use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

use rusthouse::batch::engine::{Database, QueryResult, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::tsv::{TsvIngestError, TsvIngestLimits};
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
fn headerless_tsv_ingests_all_types_in_schema_order_with_existing_escapes() {
    let input = concat!(
        "-9223372036854775808\t2.5\ttrue\tslash\\\\tab\\tcarriage\\rline\\nnull\\0back\\bform\\f\\' snow ☃\r\n",
        "7\t-3e2\tfalse\tplain\n",
    )
    .as_bytes();
    let mut database = database(2);

    assert_eq!(
        database.ingest_tsv("metrics", input, TsvIngestLimits::new(input.len(), 2, 8)),
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
                Value::String(
                    "slash\\tab\tcarriage\rline\nnull\0back\u{08}form\u{0c}' snow ☃".to_owned(),
                ),
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
fn exact_input_and_remaining_table_limits_succeed_and_empty_input_is_a_no_op() {
    let mut database = database_with_limits(TableLimits::new(3, 4, 12));
    database
        .execute("INSERT INTO metrics VALUES (0, 0.0, false, 'existing');")
        .unwrap();
    let input = b"1\t1.5\ttrue\tone\n2\t2.5\tfalse\ttwo\n";

    assert_eq!(
        database.ingest_tsv("metrics", input, TsvIngestLimits::new(input.len(), 2, 8)),
        Ok(2),
    );
    assert_eq!(
        database.ingest_tsv("metrics", b"", TsvIngestLimits::new(0, 0, 0)),
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
fn every_line_is_data_and_late_parse_and_limit_failures_are_atomic() {
    let valid_then_bad_type = b"1\t1.5\ttrue\tvalid\n2\tNaN\tfalse\tlate\n";
    let valid_then_bad_escape = b"1\t1.5\ttrue\tvalid\n2\t2.5\tfalse\tbad\\x\n";
    let valid_two_rows = b"1\t1.5\ttrue\tone\n2\t2.5\tfalse\ttwo\n";

    let cases = [
        (
            valid_then_bad_type.as_slice(),
            TsvIngestLimits::new(valid_then_bad_type.len(), 2, 8),
            TsvIngestError::InvalidValue {
                line: 2,
                column: 2,
                expected: DataType::Float64,
            },
        ),
        (
            valid_then_bad_escape.as_slice(),
            TsvIngestLimits::new(valid_then_bad_escape.len(), 2, 8),
            TsvIngestError::InvalidEscape { line: 2, column: 4 },
        ),
        (
            valid_two_rows.as_slice(),
            TsvIngestLimits::new(valid_two_rows.len(), 1, 8),
            TsvIngestError::RowLimitExceeded {
                line: 2,
                rows: 2,
                max_rows: 1,
            },
        ),
        (
            valid_two_rows.as_slice(),
            TsvIngestLimits::new(valid_two_rows.len(), 2, 7),
            TsvIngestError::ValueLimitExceeded {
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
        assert_eq!(database.ingest_tsv("metrics", input, limits), Err(expected));
        assert_eq!(ids(&mut database), [vec![Value::Int64(9)]]);
    }

    let mut byte_limited = database(4);
    byte_limited
        .execute("INSERT INTO metrics VALUES (9, 9.0, true, 'existing');")
        .unwrap();
    assert_eq!(
        byte_limited.ingest_tsv(
            "metrics",
            valid_two_rows,
            TsvIngestLimits::new(valid_two_rows.len() - 1, 2, 8),
        ),
        Err(TsvIngestError::ByteLimitExceeded {
            bytes: valid_two_rows.len(),
            max_bytes: valid_two_rows.len() - 1,
        }),
    );
    assert_eq!(ids(&mut byte_limited), [vec![Value::Int64(9)]]);

    let mut header_is_data = database(1);
    let input = b"id\tscore\tactive\tlabel\n";
    assert_eq!(
        header_is_data.ingest_tsv("metrics", input, TsvIngestLimits::new(input.len(), 1, 4),),
        Err(TsvIngestError::InvalidValue {
            line: 1,
            column: 1,
            expected: DataType::Int64,
        }),
    );
}

#[test]
fn row_and_cell_capacity_failures_roll_back_the_complete_input() {
    let input = b"1\t1.5\ttrue\tone\n2\t2.5\tfalse\ttwo\n";
    let cases = [
        (
            TableLimits::new(2, 4, 8),
            Error::ResourceLimitExceeded {
                resource: "table rows",
                actual: 3,
                max: 2,
            },
        ),
        (
            TableLimits::new(4, 4, 8),
            Error::ResourceLimitExceeded {
                resource: "table cells",
                actual: 12,
                max: 8,
            },
        ),
    ];

    for (table_limits, expected) in cases {
        let mut database = database_with_limits(table_limits);
        database
            .execute("INSERT INTO metrics VALUES (9, 9.0, true, 'existing');")
            .unwrap();
        assert_eq!(
            database.ingest_tsv("metrics", input, TsvIngestLimits::new(input.len(), 2, 8),),
            Err(TsvIngestError::Database(expected)),
        );
        assert_eq!(ids(&mut database), [vec![Value::Int64(9)]]);
    }
}

#[test]
fn shared_blocking_and_nonblocking_headerless_apis_append_complete_inputs() {
    let database = SharedDatabase::with_max_rows_per_table(2);
    database
        .execute("CREATE TABLE metrics (id Int64, score Float64, active Bool, label String);")
        .unwrap();
    let first = b"1\t1.5\ttrue\tone\n";
    let second = b"2\t2.5\tfalse\ttwo\n";

    assert_eq!(
        database.ingest_tsv("metrics", first, TsvIngestLimits::new(first.len(), 1, 4)),
        Ok(1),
    );
    assert_eq!(
        database.try_ingest_tsv("metrics", second, TsvIngestLimits::new(second.len(), 1, 4),),
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
        database.try_ingest_tsv("missing", InaccessibleInput, TsvIngestLimits::new(0, 0, 0),),
        Err(SharedDatabaseError::DatabaseBusy),
    );

    let (sender, receiver) = mpsc::channel();
    let blocking_database = database.clone();
    let worker = thread::spawn(move || {
        let input = b"1\t1.0\ttrue\tready\n";
        sender
            .send(blocking_database.ingest_tsv(
                "metrics",
                input,
                TsvIngestLimits::new(input.len(), 1, 4),
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
        Ok(1),
    );
    worker.join().unwrap();

    let writer = inner.write().unwrap();
    assert_eq!(
        database.try_ingest_tsv("missing", InaccessibleInput, TsvIngestLimits::new(0, 0, 0),),
        Err(SharedDatabaseError::DatabaseBusy),
    );
    drop(writer);

    assert_eq!(
        database.query("SELECT id FROM metrics;").unwrap().rows,
        [vec![Value::Int64(1)]],
    );
}
