use rusthouse::{ComparisonOperator, ParseError, ParseLimits, parse_scalar_sum};

#[test]
fn parses_column_sums_with_an_optional_semicolon() {
    let cases = [
        ("SELECT SUM(c) FROM t", "c", "t"),
        ("SELECT SUM(value) FROM readings;", "value", "readings"),
        (
            " \nSeLeCt SuM ( metric ) FrOm event_rows ;\t",
            "metric",
            "event_rows",
        ),
    ];

    for (input, expected_column, expected_table) in cases {
        let statement = parse_scalar_sum(input, ParseLimits::default()).unwrap();

        assert_eq!(
            statement.column_name().as_str(),
            expected_column,
            "{input:?}"
        );
        assert_eq!(statement.table_name().as_str(), expected_table, "{input:?}");
        assert_eq!(statement.predicate(), None, "{input:?}");
    }
}

#[test]
fn parses_every_comparison_operator() {
    let cases = [
        ("=", ComparisonOperator::Eq),
        ("!=", ComparisonOperator::Ne),
        ("<>", ComparisonOperator::Ne),
        ("<", ComparisonOperator::Lt),
        ("<=", ComparisonOperator::Le),
        (">", ComparisonOperator::Gt),
        (">=", ComparisonOperator::Ge),
    ];

    for (sql_operator, expected_operator) in cases {
        let input = format!("SELECT SUM(c) FROM t WHERE c {sql_operator} 7;");
        let statement = parse_scalar_sum(&input, ParseLimits::default()).unwrap();
        let predicate = statement.predicate().unwrap();

        assert_eq!(predicate.column_name().as_str(), "c", "{input:?}");
        assert_eq!(predicate.operator(), expected_operator, "{input:?}");
        assert_eq!(predicate.value(), 7, "{input:?}");
    }
}

#[test]
fn parses_comparison_whitespace_casing_and_int64_bounds() {
    let cases = [
        (
            " select sum ( value ) from readings where value<=-9223372036854775808 ",
            ComparisonOperator::Le,
            i64::MIN,
        ),
        (
            "SELECT SUM(value) FROM readings WhErE value > +9223372036854775807 ; ",
            ComparisonOperator::Gt,
            i64::MAX,
        ),
    ];

    for (input, expected_operator, expected_value) in cases {
        let statement = parse_scalar_sum(input, ParseLimits::default()).unwrap();
        let predicate = statement.predicate().unwrap();
        assert_eq!(predicate.operator(), expected_operator, "{input:?}");
        assert_eq!(predicate.value(), expected_value, "{input:?}");
    }
}

#[test]
fn applies_statement_and_identifier_byte_bounds() {
    let input = "SELECT SUM(column12) FROM table123;";
    let statement = parse_scalar_sum(input, ParseLimits::new(input.len(), 8)).unwrap();
    assert_eq!(statement.column_name().as_str(), "column12");
    assert_eq!(statement.table_name().as_str(), "table123");

    assert_eq!(
        parse_scalar_sum(input, ParseLimits::new(input.len() - 1, 8)),
        Err(ParseError::StatementTooLong {
            bytes: input.len(),
            max_bytes: input.len() - 1,
        })
    );

    for (input, identifier) in [
        ("SELECT SUM(column123) FROM t", "column123"),
        ("SELECT SUM(c) FROM table1234", "table1234"),
        ("SELECT SUM(c) FROM t WHERE predicate = 1", "predicate"),
    ] {
        assert_eq!(
            parse_scalar_sum(input, ParseLimits::new(input.len(), 8)),
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
        "SELECT SUM() FROM t",
        "SELECT SUM(1) FROM t",
        "SELECT SUM(c, d) FROM t",
        "SELECT COUNT(c) FROM t",
        "SELECT SUM(c) AS total FROM t",
        "SELECT SUM(c) FROM t WHERE c IS NULL",
        "SELECT SUM(c) FROM t WHERE c = NULL",
        "SELECT SUM(c) FROM t WHERE c === 1",
        "SELECT SUM(c) FROM t GROUP BY c",
        "SELECT SUM(c) FROM t LIMIT 1",
        "SELECT SUM(c) FROM t;;",
    ] {
        assert!(
            parse_scalar_sum(input, ParseLimits::default()).is_err(),
            "{input:?}"
        );
    }
}
