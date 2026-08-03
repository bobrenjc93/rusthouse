use rusthouse::{ParseError, ParseLimits, parse_select_distinct};

#[test]
fn parses_casing_whitespace_and_optional_semicolon() {
    let cases = [
        ("SELECT DISTINCT value FROM events", "value", "events"),
        (
            "  select\tdistinct\nReading\rfrom\tMetrics  ",
            "Reading",
            "Metrics",
        ),
        (
            "SeLeCt DiStInCt _event2 FrOm table_1;",
            "_event2",
            "table_1",
        ),
        ("\nSELECT DISTINCT x FROM y \t; \r\n", "x", "y"),
    ];

    for (input, column_name, table_name) in cases {
        let statement = parse_select_distinct(input, ParseLimits::default()).unwrap();

        assert_eq!(statement.column_name().as_str(), column_name, "{input:?}");
        assert_eq!(statement.table_name().as_str(), table_name, "{input:?}");
    }
}

#[test]
fn accepts_statement_and_identifiers_exactly_at_their_limits() {
    let input = "SELECT DISTINCT column12 FROM table123;";
    let statement = parse_select_distinct(input, ParseLimits::new(input.len(), 8)).unwrap();

    assert_eq!(statement.column_name().as_str(), "column12");
    assert_eq!(statement.table_name().as_str(), "table123");
}

#[test]
fn rejects_inputs_over_statement_and_identifier_limits() {
    let input = "SELECT DISTINCT column12 FROM table123;";
    assert_eq!(
        parse_select_distinct(input, ParseLimits::new(input.len() - 1, 8)),
        Err(ParseError::StatementTooLong {
            bytes: input.len(),
            max_bytes: input.len() - 1,
        })
    );

    for (input, offset) in [
        ("SELECT DISTINCT column123 FROM t", 16),
        ("SELECT DISTINCT c FROM table1234", 23),
    ] {
        assert_eq!(
            parse_select_distinct(input, ParseLimits::new(input.len(), 8)),
            Err(ParseError::IdentifierTooLong {
                offset,
                bytes: 9,
                max_bytes: 8,
            }),
            "{input:?}"
        );
    }
}

#[test]
fn rejects_non_distinct_or_extended_select_forms() {
    for input in [
        "SELECT value FROM events",
        "SELECT DISTINCT value FROM events LIMIT 1",
        "SELECT DISTINCT value FROM events WHERE value = 1",
        "SELECT DISTINCT value FROM events;;",
    ] {
        assert!(
            parse_select_distinct(input, ParseLimits::default()).is_err(),
            "{input:?}"
        );
    }
}

#[test]
fn rejects_missing_projection_and_table_identifiers() {
    for input in [
        "SELECT DISTINCT FROM events",
        "SELECT DISTINCT value FROM",
        "SELECT DISTINCT * FROM events",
    ] {
        assert!(
            matches!(
                parse_select_distinct(input, ParseLimits::default()),
                Err(ParseError::UnexpectedInput { .. })
            ),
            "{input:?}"
        );
    }
}
