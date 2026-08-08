use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

use rusthouse::batch::engine::{Database, QueryResult, ResultColumn, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::format::write_tsv;
use rusthouse::batch::tsv::{DEFAULT_MAX_TSV_BYTES, TsvIngestError, TsvIngestLimits};
use rusthouse::batch::value::{DataType, Value};
use rusthouse::{SharedDatabase, SharedDatabaseError};

const HEADER: &str = "id\tscore\tactive\tlabel";

fn database(row_cap: usize) -> Database {
    let mut database = Database::with_max_rows_per_table(row_cap);
    database
        .execute("CREATE TABLE metrics (id Int64, score Float64, active Bool, label String);")
        .expect("create typed table");
    database
}

fn query(database: &mut Database, sql: &str) -> QueryResult {
    match database.execute(sql).unwrap().remove(0) {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

fn limits(input: &[u8]) -> TsvIngestLimits {
    TsvIngestLimits::new(input.len(), 10, 40)
}

fn expected_result() -> QueryResult {
    QueryResult {
        columns: vec![
            ResultColumn {
                name: "id".to_owned(),
                data_type: DataType::Int64,
            },
            ResultColumn {
                name: "score".to_owned(),
                data_type: DataType::Float64,
            },
            ResultColumn {
                name: "active".to_owned(),
                data_type: DataType::Bool,
            },
            ResultColumn {
                name: "label".to_owned(),
                data_type: DataType::String,
            },
        ],
        rows: vec![
            vec![
                Value::Int64(i64::MIN),
                Value::Float64(2.5),
                Value::Bool(true),
                Value::String(
                    "slash\\tab\tcarriage\rline\nnull\0back\u{08}form\u{0c}' snow ☃".to_owned(),
                ),
            ],
            vec![
                Value::Int64(i64::MAX),
                Value::Float64(-0.125),
                Value::Bool(false),
                Value::String(String::new()),
            ],
        ],
    }
}

fn shared_database(row_cap: usize) -> SharedDatabase {
    let database = SharedDatabase::with_max_rows_per_table(row_cap);
    database
        .execute("CREATE TABLE metrics (id Int64, score Float64, active Bool, label String);")
        .expect("create typed table");
    database
}

struct InaccessibleInput;

impl AsRef<[u8]> for InaccessibleInput {
    fn as_ref(&self) -> &[u8] {
        panic!("contended or poisoned ingestion must not access its input")
    }
}

#[test]
fn writer_output_round_trips_all_types_and_escapes_with_lf_and_crlf() {
    let expected = expected_result();
    let mut lf = Vec::new();
    write_tsv(&mut lf, &expected).unwrap();
    assert_eq!(
        String::from_utf8(lf.clone()).unwrap(),
        concat!(
            "id\tscore\tactive\tlabel\n",
            "-9223372036854775808\t2.5\ttrue\tslash\\\\tab\\tcarriage\\rline\\nnull\\0back\\bform\\f\\' snow ☃\n",
            "9223372036854775807\t-0.125\tfalse\t\n",
        )
    );

    let crlf = String::from_utf8(lf.clone())
        .unwrap()
        .replace('\n', "\r\n")
        .into_bytes();
    for input in [&lf, &crlf] {
        let mut database = database(2);
        assert_eq!(
            database
                .ingest_tsv_with_names("metrics", input, TsvIngestLimits::new(input.len(), 2, 8),)
                .unwrap(),
            2
        );
        assert_eq!(
            query(
                &mut database,
                "SELECT id, score, active, label FROM metrics ORDER BY id;",
            ),
            expected
        );
    }
}

#[test]
fn every_header_permutation_selects_types_and_restores_schema_order() {
    let names = ["id", "score", "active", "label"];
    let encoded_fields = [
        "-9223372036854775808",
        "2.5",
        "true",
        "slash\\\\tab\\tcarriage\\rline\\nnull\\0back\\bform\\f\\' snow ☃",
    ];
    let expected = expected_result().rows.remove(0);

    for first in 0..4 {
        for second in 0..4 {
            for third in 0..4 {
                for fourth in 0..4 {
                    let permutation = [first, second, third, fourth];
                    if permutation
                        .iter()
                        .enumerate()
                        .any(|(index, value)| permutation[..index].contains(value))
                    {
                        continue;
                    }

                    let header = permutation.map(|index| names[index]).join("\t");
                    let values = permutation.map(|index| encoded_fields[index]).join("\t");
                    let input = format!("{header}\n{values}\n");
                    let mut database = database(1);
                    assert_eq!(
                        database.ingest_tsv_with_names(
                            "metrics",
                            input.as_bytes(),
                            TsvIngestLimits::new(input.len(), 1, 4),
                        ),
                        Ok(1),
                        "permutation: {header}"
                    );
                    assert_eq!(
                        query(
                            &mut database,
                            "SELECT id, score, active, label FROM metrics;",
                        )
                        .rows,
                        [expected.clone()],
                        "permutation: {header}"
                    );
                }
            }
        }
    }
}

#[test]
fn exact_byte_row_value_and_table_limits_succeed() {
    let input = b"id\tscore\tactive\tlabel\n1\t1.5\ttrue\tone\n2\t2.5\tfalse\ttwo\n";
    let mut database = database(2);

    assert_eq!(
        database
            .ingest_tsv_with_names("metrics", input, TsvIngestLimits::new(input.len(), 2, 8),)
            .unwrap(),
        2
    );
    assert_eq!(
        query(&mut database, "SELECT id FROM metrics ORDER BY id;").rows,
        [vec![Value::Int64(1)], vec![Value::Int64(2)]]
    );
}

#[test]
fn try_ingest_writer_output_succeeds_at_exact_limits() {
    let expected = expected_result();
    let mut input = Vec::new();
    write_tsv(&mut input, &expected).expect("write TabSeparatedWithNames");
    let database = shared_database(2);

    assert_eq!(
        database
            .try_ingest_tsv_with_names("metrics", &input, TsvIngestLimits::new(input.len(), 2, 8),)
            .expect("ingest writer output at exact byte, row, value, and table limits"),
        2
    );
    assert_eq!(
        database
            .query("SELECT id, score, active, label FROM metrics ORDER BY id;")
            .expect("query imported rows"),
        expected
    );
}

#[test]
fn try_ingest_late_tsv_error_is_typed_and_rolls_back_every_parsed_row() {
    let database = shared_database(3);
    database
        .execute("INSERT INTO metrics VALUES (9, 9.0, true, 'existing');")
        .unwrap();
    let input = b"label\tactive\tid\tscore\nvalid\ttrue\t1\t1.5\nlate\tfalse\t2\tNaN\n";

    assert_eq!(
        database.try_ingest_tsv_with_names(
            "metrics",
            input,
            TsvIngestLimits::new(input.len(), 2, 8),
        ),
        Err(SharedDatabaseError::TsvIngest(
            TsvIngestError::InvalidValue {
                line: 3,
                column: 4,
                expected: DataType::Float64,
            }
        ))
    );
    assert_eq!(
        database
            .query("SELECT id, label FROM metrics;")
            .unwrap()
            .rows,
        [vec![Value::Int64(9), Value::String("existing".to_owned())]]
    );
}

#[test]
fn try_ingest_returns_busy_without_lookup_or_input_access_for_reader_and_writer() {
    let inner = Arc::new(RwLock::new(Database::new()));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));

    let reader = inner.read().unwrap();
    let (reader_sender, reader_receiver) = mpsc::channel();
    let reader_database = database.clone();
    let reader_worker = thread::spawn(move || {
        reader_sender
            .send(reader_database.try_ingest_tsv_with_names(
                "missing",
                InaccessibleInput,
                TsvIngestLimits::new(0, 0, 0),
            ))
            .unwrap();
    });
    let reader_result = reader_receiver.recv_timeout(Duration::from_secs(2));
    drop(reader);
    reader_worker.join().unwrap();
    assert_eq!(
        reader_result,
        Ok(Err(SharedDatabaseError::DatabaseBusy)),
        "reader contention must return without waiting"
    );

    let writer = inner.write().unwrap();
    let (writer_sender, writer_receiver) = mpsc::channel();
    let writer_database = database.clone();
    let writer_worker = thread::spawn(move || {
        writer_sender
            .send(writer_database.try_ingest_tsv_with_names(
                "missing",
                InaccessibleInput,
                TsvIngestLimits::new(0, 0, 0),
            ))
            .unwrap();
    });
    let writer_result = writer_receiver.recv_timeout(Duration::from_secs(2));
    drop(writer);
    writer_worker.join().unwrap();
    assert_eq!(
        writer_result,
        Ok(Err(SharedDatabaseError::DatabaseBusy)),
        "writer contention must return without waiting"
    );
}

#[test]
fn try_ingest_preserves_tsv_limits_and_table_capacity_without_partial_rows() {
    let database = shared_database(2);
    database
        .execute("INSERT INTO metrics VALUES (9, 9.0, true, 'existing');")
        .unwrap();
    let input = b"id\tscore\tactive\tlabel\n1\t1.5\ttrue\tone\n2\t2.5\tfalse\ttwo\n";

    assert_eq!(
        database.try_ingest_tsv_with_names(
            "metrics",
            input,
            TsvIngestLimits::new(input.len(), 1, 8),
        ),
        Err(SharedDatabaseError::TsvIngest(
            TsvIngestError::RowLimitExceeded {
                line: 3,
                rows: 2,
                max_rows: 1,
            }
        ))
    );
    assert_eq!(
        database.try_ingest_tsv_with_names(
            "metrics",
            input,
            TsvIngestLimits::new(input.len(), 2, 8),
        ),
        Err(SharedDatabaseError::TsvIngest(TsvIngestError::Database(
            Error::ResourceLimitExceeded {
                resource: "table rows",
                actual: 3,
                max: 2,
            }
        )))
    );
    assert_eq!(
        database.query("SELECT id FROM metrics;").unwrap().rows,
        [vec![Value::Int64(9)]]
    );
}

#[test]
fn try_ingestion_reports_lock_poisoning_before_lookup_or_input_access() {
    let inner = Arc::new(RwLock::new(Database::new()));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let poisoner = thread::spawn(move || {
        let _guard = inner.write().unwrap();
        panic!("poison database lock");
    });
    assert!(poisoner.join().is_err());

    assert_eq!(
        database.try_ingest_tsv_with_names(
            "missing",
            InaccessibleInput,
            TsvIngestLimits::new(0, 0, 0),
        ),
        Err(SharedDatabaseError::LockPoisoned)
    );
}

#[test]
fn exceeded_ingest_limits_fail_late_without_mutation() {
    let input = b"id\tscore\tactive\tlabel\n1\t1.5\ttrue\tone\n2\t2.5\tfalse\ttwo\n";

    let cases = [
        (
            TsvIngestLimits::new(input.len() - 1, 2, 8),
            TsvIngestError::ByteLimitExceeded {
                bytes: input.len(),
                max_bytes: input.len() - 1,
            },
        ),
        (
            TsvIngestLimits::new(input.len(), 1, 8),
            TsvIngestError::RowLimitExceeded {
                line: 3,
                rows: 2,
                max_rows: 1,
            },
        ),
        (
            TsvIngestLimits::new(input.len(), 2, 7),
            TsvIngestError::ValueLimitExceeded {
                line: 3,
                values: 8,
                max_values: 7,
            },
        ),
    ];

    for (limits, expected) in cases {
        let mut database = database(3);
        database
            .execute("INSERT INTO metrics VALUES (9, 9.0, true, 'existing');")
            .unwrap();
        assert_eq!(
            database.ingest_tsv_with_names("metrics", input, limits),
            Err(expected)
        );
        assert_eq!(
            query(&mut database, "SELECT id, label FROM metrics;").rows,
            [vec![Value::Int64(9), Value::String("existing".to_owned())]]
        );
    }
}

#[test]
fn exceeded_table_capacity_rolls_back_the_complete_input() {
    let input = b"id\tscore\tactive\tlabel\n2\t2.0\tfalse\ttwo\n3\t3.0\ttrue\tthree\n";
    let mut database = database(2);
    database
        .execute("INSERT INTO metrics VALUES (1, 1.0, true, 'one');")
        .unwrap();

    assert_eq!(
        database.ingest_tsv_with_names("metrics", input, limits(input)),
        Err(TsvIngestError::Database(Error::ResourceLimitExceeded {
            resource: "table rows",
            actual: 3,
            max: 2,
        }))
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT id, score, active, label FROM metrics;",
        )
        .rows,
        [vec![
            Value::Int64(1),
            Value::Float64(1.0),
            Value::Bool(true),
            Value::String("one".to_owned()),
        ]]
    );
}

#[test]
fn malformed_headers_rows_values_and_escapes_preserve_existing_rows() {
    let cases = [
        (
            format!("{HEADER}\n1\t1.0\ttrue\tok\nno\t2.0\tfalse\tbad\n").into_bytes(),
            TsvIngestError::InvalidValue {
                line: 3,
                column: 1,
                expected: DataType::Int64,
            },
        ),
        (
            format!("{HEADER}\n1\t1.0\ttrue\tok\n2\tNaN\tfalse\tbad\n").into_bytes(),
            TsvIngestError::InvalidValue {
                line: 3,
                column: 2,
                expected: DataType::Float64,
            },
        ),
        (
            format!("{HEADER}\n1\t1.0\ttrue\tok\n2\t2.0\tTRUE\tbad\n").into_bytes(),
            TsvIngestError::InvalidValue {
                line: 3,
                column: 3,
                expected: DataType::Bool,
            },
        ),
        (
            format!("{HEADER}\n1\t1.0\ttrue\tok\n2\t2.0\tfalse\tbad\\x\n").into_bytes(),
            TsvIngestError::InvalidEscape { line: 3, column: 4 },
        ),
        (
            format!("{HEADER}\n1\t1.0\ttrue\tok\n2\t2.0\tfalse\n").into_bytes(),
            TsvIngestError::WrongColumnCount {
                line: 3,
                expected: 4,
                actual: 3,
            },
        ),
    ];

    for (input, expected) in cases {
        let mut database = database(4);
        database
            .execute("INSERT INTO metrics VALUES (9, 9.0, true, 'existing');")
            .unwrap();
        assert_eq!(
            database.ingest_tsv_with_names("metrics", &input, limits(&input)),
            Err(expected)
        );
        assert_eq!(
            query(&mut database, "SELECT id FROM metrics;").rows,
            [vec![Value::Int64(9)]]
        );
    }
}

#[test]
fn validates_header_utf8_line_endings_and_escape_grammar() {
    let mut database = database(2);
    let cases = [
        (Vec::new(), TsvIngestError::MissingHeader { line: 1 }),
        (
            b"id\tscore\tactive\n".to_vec(),
            TsvIngestError::HeaderColumnCount {
                expected: 4,
                actual: 3,
            },
        ),
        (
            b"ID\tscore\tactive\tlabel\n".to_vec(),
            TsvIngestError::HeaderMismatch {
                column: 1,
                expected: "id".to_owned(),
            },
        ),
        (
            b"id\tscore\tactive\tmystery\n".to_vec(),
            TsvIngestError::UnknownHeaderColumn {
                column: 4,
                name: "mystery".to_owned(),
            },
        ),
        (
            b"id\tscore\tactive\tid\n".to_vec(),
            TsvIngestError::DuplicateHeaderColumn {
                column: 4,
                name: "id".to_owned(),
            },
        ),
        (
            b"id\tscore\tactive\tlabel\\\n".to_vec(),
            TsvIngestError::InvalidEscape { line: 1, column: 4 },
        ),
        (
            b"id\tscore\tactive\tlabel\n1\t1.0\ttrue\tone\r".to_vec(),
            TsvIngestError::InvalidLineEnding { line: 2 },
        ),
        (
            b"id\tscore\tactive\tlabel\n1\t1.0\ttrue\t\xff\n".to_vec(),
            TsvIngestError::InvalidUtf8 { valid_up_to: 33 },
        ),
    ];

    for (input, expected) in cases {
        assert_eq!(
            database.ingest_tsv_with_names("metrics", &input, limits(&input)),
            Err(expected),
            "input: {input:?}"
        );
    }
    assert!(
        query(&mut database, "SELECT id FROM metrics;")
            .rows
            .is_empty()
    );
}

#[test]
fn wide_header_is_counted_without_collecting_fields() {
    let mut database = database(1);
    let input = vec![b'\t'; DEFAULT_MAX_TSV_BYTES];

    assert_eq!(
        database.ingest_tsv_with_names("metrics", &input, TsvIngestLimits::new(input.len(), 0, 0),),
        Err(TsvIngestError::HeaderColumnCount {
            expected: 4,
            actual: DEFAULT_MAX_TSV_BYTES + 1,
        })
    );
    assert!(
        query(&mut database, "SELECT id FROM metrics;")
            .rows
            .is_empty()
    );
}

#[test]
fn shared_database_ingests_under_the_write_lock_and_wraps_errors() {
    let database = SharedDatabase::with_max_rows_per_table(2);
    database
        .execute("CREATE TABLE metrics (id Int64, score Float64, active Bool, label String);")
        .unwrap();
    let input = b"label\tactive\tid\tscore\r\nshared\\ntext\ttrue\t1\t2.5\r\n";

    assert_eq!(
        database
            .ingest_tsv_with_names("metrics", input, TsvIngestLimits::new(input.len(), 1, 4))
            .unwrap(),
        1
    );
    let bad = b"active\tlabel\tscore\tid\nfalse\tbad\tNaN\t2\n";
    assert_eq!(
        database.ingest_tsv_with_names("metrics", bad, limits(bad)),
        Err(SharedDatabaseError::TsvIngest(
            TsvIngestError::InvalidValue {
                line: 2,
                column: 3,
                expected: DataType::Float64,
            }
        ))
    );
    assert_eq!(
        database
            .query("SELECT id, label FROM metrics;")
            .unwrap()
            .rows,
        [vec![
            Value::Int64(1),
            Value::String("shared\ntext".to_owned())
        ]]
    );
}
