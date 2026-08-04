use rusthouse::{ParseError, ParseLimits, ScalarMinStatement, parse_scalar_min};

fn assert_typed_statement(_: &ScalarMinStatement) {}

#[test]
fn parses_column_minimums_with_an_optional_semicolon() {
    let cases = [
        ("SELECT MIN(c) FROM t", "c", "t"),
        ("SELECT MIN(value) FROM readings;", "value", "readings"),
        (
            " \nSeLeCt MiN ( metric ) FrOm event_rows ;\t",
            "metric",
            "event_rows",
        ),
    ];

    for (input, expected_column, expected_table) in cases {
        let statement = parse_scalar_min(input, ParseLimits::default()).unwrap();

        assert_typed_statement(&statement);
        assert_eq!(
            statement.column_name().as_str(),
            expected_column,
            "{input:?}"
        );
        assert_eq!(statement.table_name().as_str(), expected_table, "{input:?}");
    }
}

#[test]
fn applies_exact_and_exceeded_statement_and_identifier_byte_bounds() {
    let input = "SELECT MIN(column12) FROM table123;";
    let statement = parse_scalar_min(input, ParseLimits::new(input.len(), 8)).unwrap();
    assert_eq!(statement.column_name().as_str(), "column12");
    assert_eq!(statement.table_name().as_str(), "table123");

    assert_eq!(
        parse_scalar_min(input, ParseLimits::new(input.len() - 1, 8)),
        Err(ParseError::StatementTooLong {
            bytes: input.len(),
            max_bytes: input.len() - 1,
        })
    );

    for (input, identifier) in [
        ("SELECT MIN(column123) FROM t", "column123"),
        ("SELECT MIN(c) FROM table1234", "table1234"),
    ] {
        assert_eq!(
            parse_scalar_min(input, ParseLimits::new(input.len(), 8)),
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
fn rejects_invalid_identifiers_and_unsupported_shapes() {
    for input in [
        "SELECT MIN(1) FROM t",
        "SELECT MIN(value-name) FROM t",
        "SELECT MIN(value) FROM 1readings",
        "SELECT MIN(*) FROM t",
        "SELECT MIN() FROM t",
        "SELECT MIN(c, d) FROM t",
        "SELECT MAX(c) FROM t",
        "SELECT MIN(c) AS minimum FROM t",
        "SELECT MIN(c) FROM t WHERE c > 0",
        "SELECT MIN(c) FROM t GROUP BY c",
        "SELECT MIN(c) FROM t LIMIT 1",
        "SELECT MIN(c) FROM t;;",
    ] {
        assert!(
            parse_scalar_min(input, ParseLimits::default()).is_err(),
            "{input:?}"
        );
    }
}
