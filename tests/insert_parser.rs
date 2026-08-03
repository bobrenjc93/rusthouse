use rusthouse::{ParseError, ParseLimits, parse_insert};

#[test]
fn parses_casing_whitespace_null_and_integer_extremes() {
    let cases = [
        ("INSERT INTO events VALUES (0)", "events", Some(0)),
        (
            "  insert\tinto\nMetrics\rvalues\t( +42 )  ",
            "Metrics",
            Some(42),
        ),
        (
            "InSeRt INTO _hourly VaLuEs (-9223372036854775808)",
            "_hourly",
            Some(i64::MIN),
        ),
        (
            "INSERT INTO maximum VALUES (9223372036854775807)",
            "maximum",
            Some(i64::MAX),
        ),
        ("insert into nullable values (nUlL)", "nullable", None),
    ];

    for (input, table_name, value) in cases {
        let statement = parse_insert(input, ParseLimits::default()).unwrap();

        assert_eq!(statement.table_name().as_str(), table_name, "{input:?}");
        assert_eq!(statement.value(), value, "{input:?}");
    }
}

#[test]
fn accepts_statement_and_identifier_exactly_at_their_limits() {
    let input = "INSERT INTO table123 VALUES (-1)";
    let statement = parse_insert(input, ParseLimits::new(input.len(), 8)).unwrap();

    assert_eq!(statement.table_name().as_str(), "table123");
    assert_eq!(statement.value(), Some(-1));
}

#[test]
fn rejects_inputs_over_resource_limits_with_typed_errors() {
    let cases = [
        (
            "INSERT INTO t VALUES (1)",
            ParseLimits::new(23, 8),
            ParseError::StatementTooLong {
                bytes: 24,
                max_bytes: 23,
            },
        ),
        (
            "INSERT INTO table1234 VALUES (1)",
            ParseLimits::new(64, 8),
            ParseError::IdentifierTooLong {
                offset: 12,
                bytes: 9,
                max_bytes: 8,
            },
        ),
    ];

    for (input, limits, expected) in cases {
        assert_eq!(parse_insert(input, limits), Err(expected), "{input:?}");
    }
}

#[test]
fn rejects_overflow_malformed_literals_and_extra_rows_with_byte_offsets() {
    let cases = [
        (
            "INSERT INTO t VALUES (9223372036854775808)",
            ParseError::Int64Overflow { offset: 22 },
        ),
        (
            "INSERT INTO t VALUES (-9223372036854775809)",
            ParseError::Int64Overflow { offset: 22 },
        ),
        (
            "INSERT INTO t VALUES (12.5)",
            ParseError::InvalidInt64 { offset: 24 },
        ),
        (
            "INSERT INTO t VALUES (9223372036854775808x)",
            ParseError::InvalidInt64 { offset: 41 },
        ),
        (
            "INSERT INTO t VALUES (99999999999999999999.0)",
            ParseError::InvalidInt64 { offset: 42 },
        ),
        (
            "INSERT INTO t VALUES (9223372036854775808 x)",
            ParseError::InvalidInt64 { offset: 42 },
        ),
        (
            "INSERT INTO t VALUES (nope)",
            ParseError::InvalidInt64 { offset: 22 },
        ),
        (
            "INSERT INTO t VALUES (+)",
            ParseError::InvalidInt64 { offset: 22 },
        ),
        (
            "INSERT INTO t VALUES (1, 2)",
            ParseError::InvalidInt64 { offset: 23 },
        ),
        (
            "INSERT INTO t VALUES (1), (2)",
            ParseError::ExtraRows { offset: 24 },
        ),
        (
            "INSERT INTO t VALUES (1)  ,\n(2)",
            ParseError::ExtraRows { offset: 26 },
        ),
    ];

    for (input, expected) in cases {
        assert_eq!(
            parse_insert(input, ParseLimits::default()),
            Err(expected),
            "{input:?}"
        );
    }
}

#[test]
fn rejects_malformed_statements_and_trailing_input() {
    let cases = [
        (
            "INSERT t VALUES (1)",
            ParseError::UnexpectedInput {
                offset: 7,
                expected: "INTO",
            },
        ),
        (
            "INSERT INTO t VALUE (1)",
            ParseError::UnexpectedInput {
                offset: 14,
                expected: "VALUES",
            },
        ),
        (
            "INSERT INTO t VALUES 1",
            ParseError::UnexpectedInput {
                offset: 21,
                expected: "'('",
            },
        ),
        (
            "INSERT INTO t VALUES ()",
            ParseError::InvalidInt64 { offset: 22 },
        ),
        (
            "INSERT INTO t VALUES (NULLx)",
            ParseError::InvalidInt64 { offset: 22 },
        ),
        (
            "INSERT INTO t VALUES (1",
            ParseError::UnexpectedInput {
                offset: 23,
                expected: "')'",
            },
        ),
        (
            "INSERT INTO t VALUES (1);",
            ParseError::TrailingInput { offset: 24 },
        ),
        (
            "INSERT INTO t VALUES (1) SELECT",
            ParseError::TrailingInput { offset: 25 },
        ),
    ];

    for (input, expected) in cases {
        assert_eq!(
            parse_insert(input, ParseLimits::default()),
            Err(expected),
            "{input:?}"
        );
    }
}
