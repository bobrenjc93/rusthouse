use rusthouse::{ParseError, ParseLimits, ScalarCountArgument, parse_scalar_count};

#[test]
fn parses_star_and_column_counts_with_an_optional_semicolon() {
    let cases = [
        ("SELECT COUNT(*) FROM t", None, "t"),
        (
            "SELECT COUNT(value) FROM readings;",
            Some("value"),
            "readings",
        ),
        (
            " \nSeLeCt CoUnT ( metric ) FrOm event_rows ;\t",
            Some("metric"),
            "event_rows",
        ),
    ];

    for (input, expected_column, expected_table) in cases {
        let statement = parse_scalar_count(input, ParseLimits::default()).unwrap();

        assert_eq!(
            statement.column_name().map(|name| name.as_str()),
            expected_column,
            "{input:?}"
        );
        assert_eq!(statement.table_name().as_str(), expected_table, "{input:?}");
        assert_eq!(
            matches!(statement.argument(), ScalarCountArgument::Star),
            expected_column.is_none(),
            "{input:?}"
        );
    }
}

#[test]
fn applies_statement_and_identifier_byte_bounds() {
    let input = "SELECT COUNT(column12) FROM table123;";
    let statement = parse_scalar_count(input, ParseLimits::new(input.len(), 8)).unwrap();
    assert_eq!(statement.column_name().unwrap().as_str(), "column12");
    assert_eq!(statement.table_name().as_str(), "table123");

    assert_eq!(
        parse_scalar_count(input, ParseLimits::new(input.len() - 1, 8)),
        Err(ParseError::StatementTooLong {
            bytes: input.len(),
            max_bytes: input.len() - 1,
        })
    );

    for (input, identifier) in [
        ("SELECT COUNT(column123) FROM t", "column123"),
        ("SELECT COUNT(*) FROM table1234", "table1234"),
    ] {
        assert_eq!(
            parse_scalar_count(input, ParseLimits::new(input.len(), 8)),
            Err(ParseError::IdentifierTooLong {
                offset: input.find(identifier).unwrap(),
                bytes: identifier.len(),
                max_bytes: 8,
            }),
            "{input:?}"
        );
    }
}

#[test]
fn rejects_unsupported_aggregate_shapes_and_extra_clauses() {
    for input in [
        "SELECT SUM(*) FROM t",
        "SELECT COUNT() FROM t",
        "SELECT COUNT(1) FROM t",
        "SELECT COUNT(c, d) FROM t",
        "SELECT COUNT(*) AS total FROM t",
        "SELECT COUNT(*) FROM t WHERE c = 1",
        "SELECT COUNT(*) FROM t GROUP BY c",
        "SELECT COUNT(*) FROM t LIMIT 1",
        "SELECT COUNT(*) FROM t;;",
    ] {
        assert!(
            parse_scalar_count(input, ParseLimits::default()).is_err(),
            "{input:?}"
        );
    }
}
