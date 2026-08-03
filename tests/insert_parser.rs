use rusthouse::{
    IdentifierContext, InsertParseLimits, InsertStatement, ParseError, ParseErrorKind, Value,
    parse_insert, parse_insert_with_limits,
};

fn parse_error(input: &str) -> ParseError {
    parse_insert(input).expect_err("input should be rejected")
}

fn limits_for(input: &str, rows: usize, values: usize, string_bytes: usize) -> InsertParseLimits {
    InsertParseLimits::new(input.len(), rows, values, string_bytes)
}

#[test]
fn parses_multiple_rows_with_every_literal_type() {
    let statement =
        parse_insert("INSERT INTO readings VALUES (1, -2.5, true, 'first'), (+3, .5e1, FALSE, '')")
            .unwrap();

    assert_eq!(
        statement,
        InsertStatement {
            name: "readings".to_owned(),
            rows: vec![
                vec![
                    Value::Int64(1),
                    Value::Float64(-2.5),
                    Value::Bool(true),
                    Value::String("first".to_owned()),
                ],
                vec![
                    Value::Int64(3),
                    Value::Float64(5.0),
                    Value::Bool(false),
                    Value::String(String::new()),
                ],
            ],
        }
    );
}

#[test]
fn accepts_keyword_casing_whitespace_and_one_terminator() {
    let statement = parse_insert(
        "\r\n iNsErT\tInTo Events\x0cVaLuEs\n( -0, 1., TrUe, 'line\nvalue' ),\r(+2, 3E-2, false, 'x') ; \t",
    )
    .unwrap();

    assert_eq!(statement.name, "Events");
    assert_eq!(statement.rows.len(), 2);
    assert_eq!(statement.rows[0][0], Value::Int64(0));
    assert_eq!(statement.rows[0][1], Value::Float64(1.0));
    assert_eq!(
        statement.rows[0][3],
        Value::String("line\nvalue".to_owned())
    );
}

#[test]
fn parses_int64_and_float64_boundaries() {
    let statement = parse_insert(
        "INSERT INTO numbers VALUES (-9223372036854775808, 9223372036854775807, -1.7976931348623157e308, 1.7976931348623157e308, 5e-324)",
    )
    .unwrap();

    assert_eq!(statement.rows[0][0], Value::Int64(i64::MIN));
    assert_eq!(statement.rows[0][1], Value::Int64(i64::MAX));
    assert_eq!(statement.rows[0][2], Value::Float64(-f64::MAX));
    assert_eq!(statement.rows[0][3], Value::Float64(f64::MAX));
    assert_eq!(statement.rows[0][4], Value::Float64(f64::from_bits(1)));
}

#[test]
fn decodes_doubled_quotes_without_treating_row_syntax_as_special() {
    let statement = parse_insert(
        "INSERT INTO messages VALUES ('can''t'), ('a, b) and '';'' then (done)'), ('''')",
    )
    .unwrap();

    assert_eq!(
        statement.rows,
        [
            vec![Value::String("can't".to_owned())],
            vec![Value::String("a, b) and ';' then (done)".to_owned())],
            vec![Value::String("'".to_owned())],
        ]
    );
}

#[test]
fn reports_integer_and_float_overflow_at_literal_starts() {
    for literal in ["9223372036854775808", "-9223372036854775809"] {
        let input = format!("INSERT INTO t VALUES ({literal})");
        let error = parse_error(&input);
        assert_eq!(error.position, input.find(literal).unwrap());
        assert_eq!(
            error.kind,
            ParseErrorKind::IntegerLiteralOutOfRange {
                literal: literal.to_owned(),
            }
        );
    }

    for literal in ["1e309", "-1e309"] {
        let input = format!("INSERT INTO t VALUES ({literal})");
        let error = parse_error(&input);
        assert_eq!(error.position, input.find(literal).unwrap());
        assert_eq!(
            error.kind,
            ParseErrorKind::FloatLiteralOutOfRange {
                literal: literal.to_owned(),
            }
        );
    }
}

#[test]
fn rejects_malformed_and_unsupported_literals() {
    for literal in [".", "+", "1e", "1.2.3", "12x", "NaN", "inf", "NULL"] {
        let input = format!("INSERT INTO t VALUES ({literal})");
        let error = parse_error(&input);
        assert_eq!(error.position, input.find(literal).unwrap(), "{literal:?}");
        assert_eq!(
            error.kind,
            ParseErrorKind::InvalidLiteral {
                literal: literal.to_owned(),
            },
            "{literal:?}"
        );
    }
}

#[test]
fn rejects_empty_values_and_malformed_rows_at_detection_points() {
    let cases = [
        ("INSERT INTO t VALUES ()", 22, ParseErrorKind::EmptyRow),
        (
            "INSERT INTO t VALUES (,1)",
            22,
            ParseErrorKind::ExpectedValue,
        ),
        (
            "INSERT INTO t VALUES (1,,2)",
            24,
            ParseErrorKind::ExpectedValue,
        ),
        (
            "INSERT INTO t VALUES (1,)",
            24,
            ParseErrorKind::ExpectedValue,
        ),
        (
            "INSERT INTO t VALUES (1",
            23,
            ParseErrorKind::ExpectedToken {
                expected: "',' or ')'",
            },
        ),
        (
            "INSERT INTO t VALUES (1),",
            25,
            ParseErrorKind::ExpectedToken { expected: "'('" },
        ),
    ];

    for (input, position, kind) in cases {
        let error = parse_error(input);
        assert_eq!(error.position, position, "input: {input:?}");
        assert_eq!(error.kind, kind, "input: {input:?}");
    }
}

#[test]
fn reports_unterminated_strings_at_end_of_input() {
    for input in [
        "INSERT INTO t VALUES ('open",
        "INSERT INTO t VALUES ('escaped''",
    ] {
        let error = parse_error(input);
        assert_eq!(error.position, input.len());
        assert_eq!(error.kind, ParseErrorKind::UnterminatedString);
    }
}

#[test]
fn enforces_input_limit_at_the_exact_byte_boundary() {
    let input = "INSERT INTO t VALUES (1)";
    assert!(parse_insert_with_limits(input, limits_for(input, 1, 1, 0)).is_ok());

    let limits = InsertParseLimits::new(input.len() - 1, 1, 1, 0);
    let error = parse_insert_with_limits(input, limits).unwrap_err();
    assert_eq!(error.position, input.len() - 1);
    assert_eq!(
        error.kind,
        ParseErrorKind::InputTooLong {
            limit: input.len() - 1,
            actual: input.len(),
        }
    );
}

#[test]
fn enforces_row_limit_at_the_next_row_boundary() {
    let input = "INSERT INTO t VALUES (1), (2)";
    assert!(parse_insert_with_limits(input, limits_for(input, 2, 1, 0)).is_ok());

    let error = parse_insert_with_limits(input, limits_for(input, 1, 1, 0)).unwrap_err();
    assert_eq!(error.position, input.rfind('(').unwrap());
    assert_eq!(error.kind, ParseErrorKind::TooManyRows { limit: 1 });

    let one_row = "INSERT INTO t VALUES (1)";
    let zero_error = parse_insert_with_limits(one_row, limits_for(one_row, 0, 1, 0)).unwrap_err();
    assert_eq!(zero_error.position, one_row.find('(').unwrap());
    assert_eq!(zero_error.kind, ParseErrorKind::TooManyRows { limit: 0 });
}

#[test]
fn enforces_value_limit_at_the_next_value_boundary() {
    let input = "INSERT INTO t VALUES (1, true)";
    assert!(parse_insert_with_limits(input, limits_for(input, 1, 2, 0)).is_ok());

    let error = parse_insert_with_limits(input, limits_for(input, 1, 1, 0)).unwrap_err();
    assert_eq!(error.position, input.find("true").unwrap());
    assert_eq!(error.kind, ParseErrorKind::TooManyValues { limit: 1 });

    let one_value = "INSERT INTO t VALUES (1)";
    let zero_error =
        parse_insert_with_limits(one_value, limits_for(one_value, 1, 0, 0)).unwrap_err();
    assert_eq!(zero_error.position, one_value.find('1').unwrap());
    assert_eq!(zero_error.kind, ParseErrorKind::TooManyValues { limit: 0 });
}

#[test]
fn string_limit_counts_decoded_utf8_bytes_at_the_exact_boundary() {
    let input = "INSERT INTO t VALUES ('é''x')";
    assert!(parse_insert_with_limits(input, limits_for(input, 1, 1, 4)).is_ok());

    let error = parse_insert_with_limits(input, limits_for(input, 1, 1, 3)).unwrap_err();
    assert_eq!(error.position, input.find('x').unwrap());
    assert_eq!(error.kind, ParseErrorKind::StringTooLong { limit: 3 });

    let empty = "INSERT INTO t VALUES ('')";
    assert!(parse_insert_with_limits(empty, limits_for(empty, 1, 1, 0)).is_ok());

    let quote = "INSERT INTO t VALUES ('''')";
    let quote_error = parse_insert_with_limits(quote, limits_for(quote, 1, 1, 0)).unwrap_err();
    assert_eq!(quote_error.position, quote.find("'''").unwrap() + 1);
    assert_eq!(quote_error.kind, ParseErrorKind::StringTooLong { limit: 0 });
}

#[test]
fn rejects_invalid_table_names_and_trailing_syntax() {
    let invalid = "INSERT INTO 9events VALUES (1)";
    let invalid_error = parse_error(invalid);
    assert_eq!(invalid_error.position, invalid.find('9').unwrap());
    assert_eq!(
        invalid_error.kind,
        ParseErrorKind::InvalidIdentifier {
            context: IdentifierContext::Table,
            identifier: "9events".to_owned(),
        }
    );

    for input in [
        "INSERT INTO t VALUES (1) garbage",
        "INSERT INTO t VALUES (1); INSERT INTO t VALUES (2)",
        "INSERT INTO t VALUES (1);;",
    ] {
        let error = parse_error(input);
        assert_eq!(error.kind, ParseErrorKind::TrailingSyntax, "{input:?}");
    }
}

#[test]
fn reports_missing_statement_parts_with_typed_errors() {
    let cases = [
        (
            "",
            0,
            ParseErrorKind::ExpectedKeyword {
                expected: "INSERT",
                found: None,
            },
        ),
        (
            "INSERT t VALUES (1)",
            7,
            ParseErrorKind::ExpectedKeyword {
                expected: "INTO",
                found: Some("t".to_owned()),
            },
        ),
        (
            "INSERT INTO (1)",
            12,
            ParseErrorKind::ExpectedIdentifier {
                context: IdentifierContext::Table,
            },
        ),
        (
            "INSERT INTO t (1)",
            14,
            ParseErrorKind::ExpectedKeyword {
                expected: "VALUES",
                found: None,
            },
        ),
        (
            "INSERT INTO t VALUES",
            20,
            ParseErrorKind::ExpectedToken { expected: "'('" },
        ),
    ];

    for (input, position, kind) in cases {
        let error = parse_error(input);
        assert_eq!(error.position, position, "input: {input:?}");
        assert_eq!(error.kind, kind, "input: {input:?}");
    }
}

#[test]
fn insert_parse_errors_implement_standard_error_display() {
    let error = parse_error("INSERT INTO t VALUES (9223372036854775808)");
    let standard_error: &dyn std::error::Error = &error;

    assert_eq!(
        standard_error.to_string(),
        "SQL parse error at byte 22: integer literal \"9223372036854775808\" is outside the Int64 range"
    );
}
