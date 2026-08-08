use rusthouse::batch::csv::{CsvIngestError, CsvIngestLimits};
use rusthouse::batch::engine::{Database, QueryResult, ResultColumn, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::format::write_csv;
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
fn accepts_every_header_permutation_and_reorders_quoted_typed_fields() {
    let fields = [
        ("id", "\"-7\""),
        ("score", "\"2.5\""),
        ("active", "\"true\""),
        ("label", "\"comma, \"\"quoted\"\"\nline\""),
    ];
    let mut tested = 0;

    for first in 0..fields.len() {
        for second in 0..fields.len() {
            for third in 0..fields.len() {
                for fourth in 0..fields.len() {
                    let order = [first, second, third, fourth];
                    if first == second
                        || first == third
                        || first == fourth
                        || second == third
                        || second == fourth
                        || third == fourth
                    {
                        continue;
                    }

                    let header = order
                        .iter()
                        .map(|&index| fields[index].0)
                        .collect::<Vec<_>>()
                        .join(",");
                    let record = order
                        .iter()
                        .map(|&index| fields[index].1)
                        .collect::<Vec<_>>()
                        .join(",");
                    let input = format!("{header}\n{record}\n");
                    let mut database = database(1);

                    assert_eq!(
                        database.ingest_csv_with_names(
                            "metrics",
                            input.as_bytes(),
                            CsvIngestLimits::new(input.len(), 1, 4),
                        ),
                        Ok(1),
                        "header permutation: {header}",
                    );
                    assert_eq!(
                        query(
                            &mut database,
                            "SELECT id, score, active, label FROM metrics;",
                        )
                        .rows,
                        [vec![
                            Value::Int64(-7),
                            Value::Float64(2.5),
                            Value::Bool(true),
                            Value::String("comma, \"quoted\"\nline".to_owned()),
                        ]],
                        "header permutation: {header}",
                    );
                    tested += 1;
                }
            }
        }
    }

    assert_eq!(tested, 24);
}

#[test]
fn ingests_quoted_scalars_and_mixed_fields_with_lf_and_crlf() {
    let mut database = database(4);
    let lf = concat!(
        "id,score,active,label\n",
        "\"-9223372036854775808\",\"2.5\",\"true\",\"quoted\"\n",
        "7,-3e2,\"false\",plain\n",
    )
    .as_bytes();
    let crlf = concat!(
        "id,score,active,label\r\n",
        "\"+0\",0.125,true,\"\"\r\n",
        "9,\"4.5\",\"false\",mixed\r\n",
    )
    .as_bytes();

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
        2
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
                Value::String("quoted".to_owned()),
            ],
            vec![
                Value::Int64(0),
                Value::Float64(0.125),
                Value::Bool(true),
                Value::String(String::new()),
            ],
            vec![
                Value::Int64(7),
                Value::Float64(-300.0),
                Value::Bool(false),
                Value::String("plain".to_owned()),
            ],
            vec![
                Value::Int64(9),
                Value::Float64(4.5),
                Value::Bool(false),
                Value::String("mixed".to_owned()),
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
fn preserves_multiline_strings_and_mixed_lf_and_crlf_endings() {
    let input = concat!(
        "id,score,active,label\r\n",
        "1,1.5,true,\"first LF\nsecond\"\r\n",
        "2,-2.5,false,\"first CRLF\r\nsecond, \"\"quoted\"\"\"\n",
        "3,0.0,true,plain",
    )
    .as_bytes();
    let mut database = database(3);

    assert_eq!(
        database
            .ingest_csv_with_names("metrics", input, CsvIngestLimits::new(input.len(), 3, 12),)
            .unwrap(),
        3
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT id, score, active, label FROM metrics ORDER BY id;",
        )
        .rows,
        [
            vec![
                Value::Int64(1),
                Value::Float64(1.5),
                Value::Bool(true),
                Value::String("first LF\nsecond".to_owned()),
            ],
            vec![
                Value::Int64(2),
                Value::Float64(-2.5),
                Value::Bool(false),
                Value::String("first CRLF\r\nsecond, \"quoted\"".to_owned()),
            ],
            vec![
                Value::Int64(3),
                Value::Float64(0.0),
                Value::Bool(true),
                Value::String("plain".to_owned()),
            ],
        ]
    );
}

#[test]
fn multiline_records_obey_logical_row_and_value_limits() {
    let input = concat!(
        "id,score,active,label\n",
        "1,1.0,true,\"three\nphysical\nlines\"\n",
        "2,2.0,false,two\n",
    )
    .as_bytes();

    let mut row_limited = database(3);
    assert_eq!(
        row_limited.ingest_csv_with_names(
            "metrics",
            input,
            CsvIngestLimits::new(input.len(), 1, 8),
        ),
        Err(CsvIngestError::RowLimitExceeded {
            line: 5,
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
            line: 5,
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
fn csv_export_with_multiline_strings_round_trips_into_typed_ingest() {
    let exported_result = QueryResult {
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
                Value::Int64(1),
                Value::Float64(1.5),
                Value::Bool(true),
                Value::String("LF\nline".to_owned()),
            ],
            vec![
                Value::Int64(2),
                Value::Float64(-2.25),
                Value::Bool(false),
                Value::String("CRLF\r\nquote \" and comma,".to_owned()),
            ],
        ],
    };
    let mut csv = Vec::new();
    write_csv(&mut csv, &exported_result).unwrap();
    assert_eq!(
        String::from_utf8(csv.clone()).unwrap(),
        concat!(
            "id,score,active,label\n",
            "1,1.5,true,\"LF\nline\"\n",
            "2,-2.25,false,\"CRLF\r\nquote \"\" and comma,\"\n",
        )
    );

    let mut database = database(2);
    assert_eq!(
        database
            .ingest_csv_with_names("metrics", &csv, CsvIngestLimits::new(csv.len(), 2, 8),)
            .unwrap(),
        2
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT id, score, active, label FROM metrics ORDER BY id;",
        ),
        exported_result
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
    let input = b"label,active,id,score\none,true,1,1.5\ntwo,false,2,2.5\n";
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
    let input = b"label,active,id,score\none,true,1,1.5\ntwo,false,2,2.5\n";

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
    let input = b"label,id,active,score\ntwo,2,false,2.0\nthree,3,true,3.0\n";
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
            format!("{HEADER}\n1,1.0,true,ok\n2,\"not-a-float\",false,bad\n").into_bytes(),
            CsvIngestError::InvalidValue {
                line: 3,
                column: 2,
                expected: DataType::Float64,
            },
        ),
        (
            format!("{HEADER}\n1,\"2.0\nstill quoted\",false,bad\n").into_bytes(),
            CsvIngestError::InvalidValue {
                line: 2,
                column: 2,
                expected: DataType::Float64,
            },
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
fn invalid_quoted_scalar_values_report_their_schema_types() {
    let cases = [
        (
            "\"not-an-int\",2.0,true,bad",
            CsvIngestError::InvalidValue {
                line: 3,
                column: 1,
                expected: DataType::Int64,
            },
        ),
        (
            "2,\"NaN\",true,bad",
            CsvIngestError::InvalidValue {
                line: 3,
                column: 2,
                expected: DataType::Float64,
            },
        ),
        (
            "2,\"1e999\",true,bad",
            CsvIngestError::InvalidValue {
                line: 3,
                column: 2,
                expected: DataType::Float64,
            },
        ),
        (
            "2,2.0,\"TRUE\",bad",
            CsvIngestError::InvalidValue {
                line: 3,
                column: 3,
                expected: DataType::Bool,
            },
        ),
    ];

    for (invalid_row, expected) in cases {
        let input = format!("{HEADER}\n1,1.0,true,valid\n{invalid_row}\n");
        let mut database = database(3);

        assert_eq!(
            database.ingest_csv_with_names(
                "metrics",
                input.as_bytes(),
                generous_limits(input.as_bytes()),
            ),
            Err(expected),
            "input: {input:?}",
        );
        assert!(
            query(&mut database, "SELECT id FROM metrics;")
                .rows
                .is_empty(),
            "a quoted type error must roll back earlier parsed rows",
        );
    }
}

#[test]
fn late_quoted_type_error_with_reordered_header_rolls_back_for_lf_and_crlf() {
    for line_ending in ["\n", "\r\n"] {
        let input = format!(
            "label,active,id,score{line_ending}\"valid\",\"true\",\"1\",\"1.0\"{line_ending}bad,false,2,\"NaN\"{line_ending}"
        );
        let mut database = database(4);
        database
            .execute("INSERT INTO metrics VALUES (9, 9.0, true, 'existing');")
            .unwrap();

        assert_eq!(
            database.ingest_csv_with_names(
                "metrics",
                input.as_bytes(),
                generous_limits(input.as_bytes()),
            ),
            Err(CsvIngestError::InvalidValue {
                line: 3,
                column: 4,
                expected: DataType::Float64,
            }),
            "line ending: {line_ending:?}",
        );
        assert_eq!(
            query(&mut database, "SELECT id, label FROM metrics;").rows,
            [vec![Value::Int64(9), Value::String("existing".to_owned()),]],
            "line ending: {line_ending:?}",
        );
    }
}

#[test]
fn late_unclosed_multiline_record_preserves_existing_rows() {
    let input = concat!(
        "id,score,active,label\n",
        "1,1.0,true,\"valid\nquoted\"\r\n",
        "2,2.0,false,\"late\r\n",
        "unclosed",
    )
    .as_bytes();
    let mut database = database(4);
    database
        .execute("INSERT INTO metrics VALUES (9, 9.0, true, 'existing');")
        .unwrap();

    assert_eq!(
        database.ingest_csv_with_names("metrics", input, generous_limits(input)),
        Err(CsvIngestError::MalformedQuoting { line: 4, column: 4 })
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

    let header_mismatch = b"label,score,active,ID\none,1.0,true,1\n";
    assert_eq!(
        database.ingest_csv_with_names(
            "metrics",
            header_mismatch,
            generous_limits(header_mismatch),
        ),
        Err(CsvIngestError::HeaderMismatch {
            column: 4,
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

    let quoted_bare_cr = b"id,score,active,label\n1,1.0,true,\"one\rtwo\"\n";
    assert_eq!(
        database.ingest_csv_with_names("metrics", quoted_bare_cr, generous_limits(quoted_bare_cr),),
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

#[test]
fn duplicate_missing_and_unknown_header_names_preserve_existing_rows() {
    let cases = [
        (
            b"label,id,score,id\none,1,1.0,1\n".as_slice(),
            CsvIngestError::DuplicateHeaderColumn {
                column: 4,
                name: "id".to_owned(),
            },
        ),
        (
            b"label,id,score\none,1,1.0\n".as_slice(),
            CsvIngestError::HeaderColumnCount {
                expected: 4,
                actual: 3,
            },
        ),
        (
            b"label,id,mystery,score\none,1,true,1.0\n".as_slice(),
            CsvIngestError::UnknownHeaderColumn {
                column: 3,
                name: "mystery".to_owned(),
            },
        ),
    ];

    for (input, expected) in cases {
        let mut database = database(2);
        database
            .execute("INSERT INTO metrics VALUES (9, 9.0, true, 'existing');")
            .unwrap();

        assert_eq!(
            database.ingest_csv_with_names("metrics", input, generous_limits(input)),
            Err(expected),
        );
        assert_eq!(
            query(&mut database, "SELECT id, label FROM metrics;").rows,
            [vec![Value::Int64(9), Value::String("existing".to_owned())]],
        );
    }
}
