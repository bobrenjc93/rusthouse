use rusthouse::{ParseError, ParseLimits, parse_create_table, parse_select};

#[test]
fn parses_casing_whitespace_and_optional_semicolon() {
    let cases = [
        ("SELECT value FROM events", "value", "events"),
        ("  select\tReading\nfrom\rMetrics  ", "Reading", "Metrics"),
        ("SeLeCt _event2 FrOm table_1;", "_event2", "table_1"),
        ("\nSELECT x FROM y \t; \r\n", "x", "y"),
    ];

    for (input, column_name, table_name) in cases {
        let statement = parse_select(input, ParseLimits::default()).unwrap();

        assert_eq!(statement.column_name().as_str(), column_name, "{input:?}");
        assert_eq!(statement.table_name().as_str(), table_name, "{input:?}");
    }
}

#[test]
fn selects_a_from_identifier_accepted_by_create_table() {
    let create = parse_create_table("CREATE TABLE t (FROM Int64)", ParseLimits::default()).unwrap();
    let select = parse_select("SELECT FROM FROM t", ParseLimits::default()).unwrap();

    assert_eq!(select.column_name(), create.column().name());
    assert_eq!(select.table_name(), create.table_name());
}

#[test]
fn accepts_statement_and_identifiers_exactly_at_their_limits() {
    let input = "SELECT column12 FROM table123;";
    let statement = parse_select(input, ParseLimits::new(input.len(), 8)).unwrap();

    assert_eq!(statement.column_name().as_str(), "column12");
    assert_eq!(statement.table_name().as_str(), "table123");
}

#[test]
fn rejects_inputs_over_resource_limits_with_typed_byte_offsets() {
    let input = "SELECT column12 FROM table123;";
    let cases = [
        (
            input,
            ParseLimits::new(input.len() - 1, 8),
            ParseError::StatementTooLong {
                bytes: input.len(),
                max_bytes: input.len() - 1,
            },
        ),
        (
            "SELECT column123 FROM t",
            ParseLimits::new(64, 8),
            ParseError::IdentifierTooLong {
                offset: 7,
                bytes: 9,
                max_bytes: 8,
            },
        ),
        (
            "SELECT c FROM table1234",
            ParseLimits::new(64, 8),
            ParseError::IdentifierTooLong {
                offset: 14,
                bytes: 9,
                max_bytes: 8,
            },
        ),
    ];

    for (input, limits, expected) in cases {
        assert_eq!(parse_select(input, limits), Err(expected), "{input:?}");
    }
}

#[test]
fn rejects_malformed_projections_with_byte_offsets() {
    let cases = [
        (
            "SELECT * FROM t",
            ParseError::UnexpectedInput {
                offset: 7,
                expected: "identifier",
            },
        ),
        (
            "SELECT 1 FROM t",
            ParseError::UnexpectedInput {
                offset: 7,
                expected: "identifier",
            },
        ),
        (
            "SELECT FROM t",
            ParseError::UnexpectedInput {
                offset: 7,
                expected: "identifier",
            },
        ),
        (
            "SELECT a,b FROM t",
            ParseError::UnexpectedInput {
                offset: 8,
                expected: "whitespace before FROM",
            },
        ),
        (
            "SELECT a + b FROM t",
            ParseError::UnexpectedInput {
                offset: 9,
                expected: "FROM",
            },
        ),
        (
            "SELECT a AS b FROM t",
            ParseError::UnexpectedInput {
                offset: 9,
                expected: "FROM",
            },
        ),
    ];

    for (input, expected) in cases {
        assert_eq!(
            parse_select(input, ParseLimits::default()),
            Err(expected),
            "{input:?}"
        );
    }
}

#[test]
fn rejects_missing_clauses_with_byte_offsets() {
    let cases = [
        (
            "",
            ParseError::UnexpectedInput {
                offset: 0,
                expected: "SELECT",
            },
        ),
        (
            "SELECT",
            ParseError::UnexpectedInput {
                offset: 6,
                expected: "whitespace after SELECT",
            },
        ),
        (
            "SELECT ",
            ParseError::UnexpectedInput {
                offset: 7,
                expected: "identifier",
            },
        ),
        (
            "SELECT c",
            ParseError::UnexpectedInput {
                offset: 8,
                expected: "whitespace before FROM",
            },
        ),
        (
            "SELECT c ",
            ParseError::UnexpectedInput {
                offset: 9,
                expected: "FROM",
            },
        ),
        (
            "SELECT c t",
            ParseError::UnexpectedInput {
                offset: 9,
                expected: "FROM",
            },
        ),
        (
            "SELECT c FROM",
            ParseError::UnexpectedInput {
                offset: 13,
                expected: "whitespace after FROM",
            },
        ),
        (
            "SELECT c FROM ",
            ParseError::UnexpectedInput {
                offset: 14,
                expected: "identifier",
            },
        ),
    ];

    for (input, expected) in cases {
        assert_eq!(
            parse_select(input, ParseLimits::default()),
            Err(expected),
            "{input:?}"
        );
    }
}

#[test]
fn rejects_trailing_input_after_the_table_or_one_semicolon() {
    let cases = [
        ("SELECT c FROM t extra", 16),
        ("SELECT c FROM t, u", 15),
        ("SELECT c FROM t;;", 16),
        ("SELECT c FROM t; SELECT x FROM u", 17),
        ("SELECT c FROM t ; ;", 18),
    ];

    for (input, offset) in cases {
        assert_eq!(
            parse_select(input, ParseLimits::default()),
            Err(ParseError::TrailingInput { offset }),
            "{input:?}"
        );
    }
}
