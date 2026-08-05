use rusthouse::batch::csv::{CsvIngestError, CsvIngestLimits};
use rusthouse::batch::engine::{Database, QueryResult, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::value::{DataType, Value};

const HEADER: &str = "id,score,active,label";

fn database(row_cap: usize) -> Database {
    let mut database = Database::with_max_rows_per_table(row_cap);
    database
        .execute("CREATE TABLE metrics (id Int64, score Float64, active Bool, label String);")
        .expect("create typed table");
    database
}

fn query(database: &mut Database, sql: &str) -> QueryResult {
    match database
        .execute(sql)
        .expect("query succeeds")
        .into_iter()
        .next()
        .expect("one statement result")
    {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected one query result"),
    }
}

fn generous_limits(input: &[u8]) -> CsvIngestLimits {
    CsvIngestLimits::new(input.len(), 10, 40)
}

#[test]
fn ingests_all_four_types_with_lf_and_crlf_and_selects_them_back() {
    let mut database = database(4);
    let lf = b"id,score,active,label\n-1,2.5,true,alpha\n2,-3e2,false,\n";
    let crlf = b"id,score,active,label\r\n3,0.125,true,snowman \xE2\x98\x83\r\n";

    assert_eq!(
        database
            .ingest_csv_with_names("metrics", lf, generous_limits(lf))
            .unwrap(),
        2
    );
    assert_eq!(
        database
            .ingest_csv_with_names("metrics", crlf, generous_limits(crlf))
            .unwrap(),
        1
    );

    assert_eq!(
        query(
            &mut database,
            "SELECT id, score, active, label FROM metrics ORDER BY id;",
        )
        .rows,
        [
            vec![
                Value::Int64(-1),
                Value::Float64(2.5),
                Value::Bool(true),
                Value::String("alpha".to_owned()),
            ],
            vec![
                Value::Int64(2),
                Value::Float64(-300.0),
                Value::Bool(false),
                Value::String(String::new()),
            ],
            vec![
                Value::Int64(3),
                Value::Float64(0.125),
                Value::Bool(true),
                Value::String("snowman ☃".to_owned()),
            ],
        ]
    );
}

#[test]
fn decodes_single_line_quoted_strings_among_typed_fields() {
    let input = concat!(
        "id,score,active,label\n",
        "1,1.0,true,\"\"\n",
        "2,2.0,false,\"comma,value\"\n",
        "3,3.0,true,\"say \"\"hello\"\"\"\n",
        "4,4.0,false,plain\n",
    )
    .as_bytes();
    let mut database = database(4);

    assert_eq!(
        database
            .ingest_csv_with_names("metrics", input, CsvIngestLimits::new(input.len(), 4, 16),)
            .unwrap(),
        4
    );
    assert_eq!(
        query(&mut database, "SELECT id, label FROM metrics ORDER BY id;",).rows,
        [
            vec![Value::Int64(1), Value::String(String::new())],
            vec![Value::Int64(2), Value::String("comma,value".to_owned()),],
            vec![Value::Int64(3), Value::String("say \"hello\"".to_owned()),],
            vec![Value::Int64(4), Value::String("plain".to_owned())],
        ]
    );
}

#[test]
fn quoted_string_can_precede_more_typed_fields() {
    let input = b"id,label,active\n1,\"left,right\",true\n2,\"\",false\n";
    let mut database = Database::with_max_rows_per_table(2);
    database
        .execute("CREATE TABLE messages (id Int64, label String, active Bool);")
        .unwrap();

    assert_eq!(
        database
            .ingest_csv_with_names("messages", input, CsvIngestLimits::new(input.len(), 2, 6),)
            .unwrap(),
        2
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT id, label, active FROM messages ORDER BY id;",
        )
        .rows,
        [
            vec![
                Value::Int64(1),
                Value::String("left,right".to_owned()),
                Value::Bool(true),
            ],
            vec![
                Value::Int64(2),
                Value::String(String::new()),
                Value::Bool(false),
            ],
        ]
    );
}

#[test]
fn exact_byte_row_and_value_limits_succeed() {
    let input = b"id,score,active,label\n1,1.5,true,one\n2,2.5,false,two\n";
    let mut database = database(2);

    assert_eq!(
        database
            .ingest_csv_with_names("metrics", input, CsvIngestLimits::new(input.len(), 2, 8),)
            .unwrap(),
        2
    );
    assert_eq!(
        query(&mut database, "SELECT id FROM metrics ORDER BY id;").rows,
        [vec![Value::Int64(1)], vec![Value::Int64(2)]]
    );
}

#[test]
fn exceeded_byte_row_and_value_limits_leave_the_table_empty() {
    let input = b"id,score,active,label\n1,1.5,true,one\n2,2.5,false,two\n";

    let mut byte_limited = database(3);
    assert_eq!(
        byte_limited.ingest_csv_with_names(
            "metrics",
            input,
            CsvIngestLimits::new(input.len() - 1, 2, 8),
        ),
        Err(CsvIngestError::ByteLimitExceeded {
            bytes: input.len(),
            max_bytes: input.len() - 1,
        })
    );
    assert!(
        query(&mut byte_limited, "SELECT id FROM metrics;")
            .rows
            .is_empty()
    );

    let mut row_limited = database(3);
    assert_eq!(
        row_limited.ingest_csv_with_names(
            "metrics",
            input,
            CsvIngestLimits::new(input.len(), 1, 8),
        ),
        Err(CsvIngestError::RowLimitExceeded {
            line: 3,
            rows: 2,
            max_rows: 1,
        })
    );
    assert!(
        query(&mut row_limited, "SELECT id FROM metrics;")
            .rows
            .is_empty()
    );

    let mut value_limited = database(3);
    assert_eq!(
        value_limited.ingest_csv_with_names(
            "metrics",
            input,
            CsvIngestLimits::new(input.len(), 2, 7),
        ),
        Err(CsvIngestError::ValueLimitExceeded {
            line: 3,
            values: 8,
            max_values: 7,
        })
    );
    assert!(
        query(&mut value_limited, "SELECT id FROM metrics;")
            .rows
            .is_empty()
    );
}

#[test]
fn exact_table_capacity_succeeds_and_exceeded_capacity_rolls_back() {
    let input = b"id,score,active,label\n2,2.0,false,two\n3,3.0,true,three\n";
    let mut exact = database(3);
    exact
        .execute("INSERT INTO metrics VALUES (1, 1.0, true, 'one');")
        .unwrap();
    assert_eq!(
        exact
            .ingest_csv_with_names("metrics", input, generous_limits(input))
            .unwrap(),
        2
    );

    let mut exceeded = database(2);
    exceeded
        .execute("INSERT INTO metrics VALUES (1, 1.0, true, 'one');")
        .unwrap();
    assert_eq!(
        exceeded.ingest_csv_with_names("metrics", input, generous_limits(input)),
        Err(CsvIngestError::Database(Error::ResourceLimitExceeded {
            resource: "table rows",
            actual: 3,
            max: 2,
        }))
    );
    assert_eq!(
        query(
            &mut exceeded,
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
fn malformed_rows_and_typed_values_roll_back_prior_parsed_records() {
    let cases = [
        (
            format!("{HEADER}\n1,1.0,true,ok\nno,2.0,false,bad\n").into_bytes(),
            CsvIngestError::InvalidValue {
                line: 3,
                column: 1,
                expected: DataType::Int64,
            },
        ),
        (
            format!("{HEADER}\n1,1.0,true,ok\n2,NaN,false,bad\n").into_bytes(),
            CsvIngestError::InvalidValue {
                line: 3,
                column: 2,
                expected: DataType::Float64,
            },
        ),
        (
            format!("{HEADER}\n1,1.0,true,ok\n2,not-a-float,false,bad\n").into_bytes(),
            CsvIngestError::InvalidValue {
                line: 3,
                column: 2,
                expected: DataType::Float64,
            },
        ),
        (
            format!("{HEADER}\n1,1.0,true,ok\n2,2.0,TRUE,bad\n").into_bytes(),
            CsvIngestError::InvalidValue {
                line: 3,
                column: 3,
                expected: DataType::Bool,
            },
        ),
        (
            format!("{HEADER}\n1,1.0,true,ok\n2,2.0,false\n").into_bytes(),
            CsvIngestError::WrongColumnCount {
                line: 3,
                expected: 4,
                actual: 3,
            },
        ),
        (
            format!("{HEADER}\n1,1.0,true,ok\n2,2.0,false,comma,value\n").into_bytes(),
            CsvIngestError::WrongColumnCount {
                line: 3,
                expected: 4,
                actual: 5,
            },
        ),
        (
            format!("{HEADER}\n1,1.0,true,ok\n2,\"2.0\",false,bad\n").into_bytes(),
            CsvIngestError::QuotingNotSupported { line: 3, column: 2 },
        ),
        (
            format!("{HEADER}\n1,1.0,true,ok\n2,2.0,false,unquoted\"quote\n").into_bytes(),
            CsvIngestError::MalformedQuoting { line: 3, column: 4 },
        ),
        (
            format!("{HEADER}\n1,1.0,true,ok\n2,2.0,false,\"closed\"junk\n").into_bytes(),
            CsvIngestError::MalformedQuoting { line: 3, column: 4 },
        ),
    ];

    for (input, expected) in cases {
        let mut database = database(4);
        assert_eq!(
            database.ingest_csv_with_names("metrics", &input, generous_limits(&input)),
            Err(expected)
        );
        assert!(
            query(&mut database, "SELECT id FROM metrics;")
                .rows
                .is_empty(),
            "a valid record before a malformed one must not be appended"
        );
    }
}

#[test]
fn embedded_record_break_and_late_malformed_quote_preserve_existing_rows() {
    let input = concat!(
        "id,score,active,label\n",
        "1,1.0,true,\"valid,quoted\"\n",
        "2,2.0,false,\"two\n",
        "lines\"\n",
    )
    .as_bytes();
    let mut database = database(4);
    database
        .execute("INSERT INTO metrics VALUES (9, 9.0, true, 'existing');")
        .unwrap();

    assert_eq!(
        database.ingest_csv_with_names("metrics", input, generous_limits(input)),
        Err(CsvIngestError::MalformedQuoting { line: 3, column: 4 })
    );
    assert_eq!(
        query(&mut database, "SELECT id, label FROM metrics;").rows,
        [vec![Value::Int64(9), Value::String("existing".to_owned()),]]
    );
}

#[test]
fn validates_utf8_line_endings_header_and_table_before_mutation() {
    let mut database = database(2);
    assert_eq!(
        database.ingest_csv_with_names("metrics", b"", CsvIngestLimits::new(0, 0, 0)),
        Err(CsvIngestError::MissingHeader { line: 1 })
    );

    let short_header = b"id,score,active\n";
    assert_eq!(
        database.ingest_csv_with_names("metrics", short_header, generous_limits(short_header)),
        Err(CsvIngestError::HeaderColumnCount {
            expected: 4,
            actual: 3,
        })
    );

    let header_mismatch = b"ID,score,active,label\n1,1.0,true,one\n";
    assert_eq!(
        database.ingest_csv_with_names(
            "metrics",
            header_mismatch,
            generous_limits(header_mismatch),
        ),
        Err(CsvIngestError::HeaderMismatch {
            column: 1,
            expected: "id".to_owned(),
        })
    );

    let quoted_header = b"id,score,active,\"label\"\n1,1.0,true,one\n";
    assert_eq!(
        database.ingest_csv_with_names("metrics", quoted_header, generous_limits(quoted_header),),
        Err(CsvIngestError::QuotingNotSupported { line: 1, column: 4 })
    );

    let invalid_utf8 = b"id,score,active,label\n1,1.0,true,\xff\n";
    assert_eq!(
        database.ingest_csv_with_names("metrics", invalid_utf8, generous_limits(invalid_utf8)),
        Err(CsvIngestError::InvalidUtf8 { valid_up_to: 33 })
    );

    let bare_cr = b"id,score,active,label\n1,1.0,true,one\r";
    assert_eq!(
        database.ingest_csv_with_names("metrics", bare_cr, generous_limits(bare_cr)),
        Err(CsvIngestError::InvalidLineEnding { line: 2 })
    );

    let valid = b"id,score,active,label\n1,1.0,true,one\n";
    assert_eq!(
        database.ingest_csv_with_names("missing", valid, generous_limits(valid)),
        Err(CsvIngestError::Database(Error::TableNotFound(
            "missing".to_owned()
        )))
    );
    assert!(
        query(&mut database, "SELECT id FROM metrics;")
            .rows
            .is_empty()
    );
}
