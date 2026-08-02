use rusthouse::sql::{
    InsertParseLimits, InsertStatement, MAX_INSERT_ROWS, MAX_INSERT_STRING_BYTES,
    MAX_INSERT_VALUES, MAX_SQL_BYTES, ParseError, Value, parse_insert, parse_insert_with_limits,
};
use rusthouse::{ColumnSchema, DataType, Schema, Table, TableLimits};

#[test]
fn parses_every_literal_type_escaping_and_multiple_rows() {
    let sql = concat!(
        "\tInSeRt\nInTo readings VaLuEs\r\n",
        "(-9223372036854775808, +1.25, TrUe, 'it''s café'),",
        "(+9223372036854775807, -.5e+2, FALSE, '''quoted'''); \n",
    );

    assert_eq!(
        parse_insert(sql).unwrap(),
        InsertStatement {
            table_name: "readings".into(),
            rows: vec![
                vec![
                    Value::Int64(i64::MIN),
                    Value::Float64(1.25),
                    Value::Bool(true),
                    Value::String("it's café".into()),
                ],
                vec![
                    Value::Int64(i64::MAX),
                    Value::Float64(-50.0),
                    Value::Bool(false),
                    Value::String("'quoted'".into()),
                ],
            ],
        }
    );
}

#[test]
fn accepts_decimal_and_exponent_forms_with_optional_signs() {
    let statement = parse_insert("INSERT INTO numbers VALUES (1., .25, +2e3, -4E-2, -0)").unwrap();

    assert_eq!(
        statement.rows[0],
        vec![
            Value::Float64(1.0),
            Value::Float64(0.25),
            Value::Float64(2000.0),
            Value::Float64(-0.04),
            Value::Int64(0),
        ]
    );
}

#[test]
fn parsed_rows_are_directly_accepted_by_atomic_batch_insertion() {
    let statement =
        parse_insert("INSERT INTO events VALUES (1, true, 'first'), (2, false, 'next')").unwrap();
    let schema = Schema::new(vec![
        ColumnSchema::new("id", DataType::Int64),
        ColumnSchema::new("active", DataType::Bool),
        ColumnSchema::new("label", DataType::String),
    ])
    .unwrap();
    let mut table = Table::new(
        schema,
        TableLimits {
            max_columns: 3,
            max_rows: 2,
            max_cells: 6,
            max_string_bytes: 9,
        },
    )
    .unwrap();

    table.insert_batch(statement.rows).unwrap();

    assert_eq!(table.row_count(), 2);
    assert_eq!(table.column("id").unwrap().as_int64(), Some(&[1, 2][..]));
    assert_eq!(
        table.column("label").unwrap().as_string().unwrap(),
        &["first".to_owned(), "next".to_owned()]
    );
}

#[test]
fn rejects_malformed_statements_at_the_relevant_byte() {
    let cases = [
        ("", 0, "INSERT"),
        ("INSER INTO t VALUES (1)", 0, "INSERT"),
        ("INSERT t VALUES (1)", 7, "INTO"),
        ("INSERT INTO 2bad VALUES (1)", 12, "table name"),
        ("INSERT INTO t VALUE (1)", 14, "VALUES"),
        ("INSERT INTO t VALUES", 20, "'('"),
        ("INSERT INTO t VALUES ()", 22, "literal"),
        ("INSERT INTO t VALUES (1,)", 24, "literal"),
        ("INSERT INTO t VALUES (1 2)", 24, "',' or ')'"),
        ("INSERT INTO t VALUES (1", 23, "',' or ')'"),
        ("INSERT INTO t VALUES (1),", 25, "'('"),
        ("INSERT INTO t VALUES (NULL)", 22, "literal"),
    ];

    for (sql, expected_position, expected) in cases {
        let error = parse_insert(sql).unwrap_err();
        assert_eq!(error.position(), expected_position, "SQL: {sql:?}");
        assert!(
            matches!(error, ParseError::Syntax { expected: actual, .. } if actual == expected),
            "SQL: {sql:?}, error: {error:?}"
        );
    }
}

#[test]
fn rejects_malformed_numeric_literals_and_unterminated_strings() {
    for literal in ["+", "-", ".", "1e", "1e+", "1.2.3", "--1", "12abc"] {
        let sql = format!("INSERT INTO t VALUES ({literal})");
        let position = sql.find(literal).unwrap();
        assert_eq!(
            parse_insert(&sql),
            Err(ParseError::Syntax {
                position,
                expected: "literal",
                found: Some(literal.into()),
            }),
            "literal: {literal:?}"
        );
    }

    let unterminated = "INSERT INTO t VALUES ('not finished";
    assert_eq!(
        parse_insert(unterminated),
        Err(ParseError::Syntax {
            position: unterminated.len(),
            expected: "closing quote",
            found: None,
        })
    );
}

#[test]
fn distinguishes_integer_overflow_and_non_finite_floats() {
    for literal in ["9223372036854775808", "-9223372036854775809"] {
        let sql = format!("INSERT INTO t VALUES ({literal})");
        let position = sql.find(literal).unwrap();
        assert_eq!(
            parse_insert(&sql),
            Err(ParseError::IntegerOverflow {
                position,
                literal: literal.into(),
            })
        );
    }

    for literal in ["1e309", "-1e309", "NaN", "+inf", "INFINITY"] {
        let sql = format!("INSERT INTO t VALUES ({literal})");
        let position = sql.find(literal).unwrap();
        assert_eq!(
            parse_insert(&sql),
            Err(ParseError::NonFiniteFloat {
                position,
                literal: literal.into(),
            })
        );
    }
}

#[test]
fn rejects_trailing_input_at_the_first_non_whitespace_byte() {
    for sql in [
        "INSERT INTO t VALUES (1) trailing",
        "INSERT INTO t VALUES (1); SELECT 1",
        "INSERT INTO t VALUES (1);;",
    ] {
        let position = if let Some(position) = sql.find("trailing") {
            position
        } else if let Some(position) = sql.find("SELECT") {
            position
        } else {
            sql.len() - 1
        };
        assert_eq!(
            parse_insert(sql),
            Err(ParseError::TrailingInput { position })
        );
    }
}

#[test]
fn enforces_the_sql_byte_limit_at_its_exact_boundary() {
    let statement = "INSERT INTO t VALUES (1)";
    let at_limit = format!("{statement}{}", " ".repeat(MAX_SQL_BYTES - statement.len()));
    assert_eq!(at_limit.len(), MAX_SQL_BYTES);
    assert!(parse_insert(&at_limit).is_ok());

    let over_limit = format!("{at_limit} ");
    assert_eq!(
        parse_insert(&over_limit),
        Err(ParseError::SqlTooLarge {
            position: MAX_SQL_BYTES,
            max_bytes: MAX_SQL_BYTES,
            actual_bytes: MAX_SQL_BYTES + 1,
        })
    );
}

#[test]
fn enforces_the_row_limit_at_its_exact_boundary() {
    let at_limit = insert_with_rows(MAX_INSERT_ROWS);
    assert_eq!(parse_insert(&at_limit).unwrap().rows.len(), MAX_INSERT_ROWS);

    let over_limit = insert_with_rows(MAX_INSERT_ROWS + 1);
    let extra_row_position = over_limit.rfind("(0)").unwrap();
    assert_eq!(
        parse_insert(&over_limit),
        Err(ParseError::TooManyRows {
            position: extra_row_position,
            max_rows: MAX_INSERT_ROWS,
        })
    );
}

#[test]
fn enforces_the_total_value_limit_at_its_exact_boundary() {
    let at_limit = insert_with_values(MAX_INSERT_VALUES);
    assert_eq!(
        parse_insert(&at_limit).unwrap().rows[0].len(),
        MAX_INSERT_VALUES
    );

    let over_limit = insert_with_values(MAX_INSERT_VALUES + 1);
    let extra_value_position = over_limit.rfind('0').unwrap();
    assert_eq!(
        parse_insert(&over_limit),
        Err(ParseError::TooManyValues {
            position: extra_value_position,
            max_values: MAX_INSERT_VALUES,
        })
    );
}

#[test]
fn enforces_decoded_string_bytes_at_the_exact_boundary() {
    let payload = "x".repeat(MAX_INSERT_STRING_BYTES);
    let at_limit = format!("INSERT INTO t VALUES ('{payload}')");
    assert!(parse_insert(&at_limit).is_ok());

    let over_limit = format!("INSERT INTO t VALUES ('{payload}x')");
    let position = over_limit.find('\'').unwrap();
    assert_eq!(
        parse_insert(&over_limit),
        Err(ParseError::StringByteLimitExceeded {
            position,
            max_bytes: MAX_INSERT_STRING_BYTES,
            attempted_bytes: MAX_INSERT_STRING_BYTES + 1,
        })
    );

    let limits = InsertParseLimits {
        max_string_bytes: 4,
        ..InsertParseLimits::default()
    };
    assert!(parse_insert_with_limits("INSERT INTO t VALUES ('a''é')", limits).is_ok());
    let cumulative_over_limit = "INSERT INTO t VALUES ('a''é', 'x')";
    assert_eq!(
        parse_insert_with_limits(cumulative_over_limit, limits),
        Err(ParseError::StringByteLimitExceeded {
            position: cumulative_over_limit.rfind("'x'").unwrap(),
            max_bytes: 4,
            attempted_bytes: 5,
        })
    );
}

fn insert_with_rows(count: usize) -> String {
    let rows = std::iter::repeat_n("(0)", count)
        .collect::<Vec<_>>()
        .join(",");
    format!("INSERT INTO t VALUES {rows}")
}

fn insert_with_values(count: usize) -> String {
    let values = std::iter::repeat_n("0", count)
        .collect::<Vec<_>>()
        .join(",");
    format!("INSERT INTO t VALUES ({values})")
}
