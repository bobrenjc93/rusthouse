use rusthouse::{
    ComparisonOperator, NullOrder, OrderDirection, ParseError, ParseLimits, parse_create_table,
    parse_select,
};

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
        assert_eq!(statement.predicate(), None, "{input:?}");
        assert_eq!(statement.order_by(), None, "{input:?}");
        assert_eq!(statement.limit(), None, "{input:?}");
    }
}

#[test]
fn parses_explicit_order_direction_null_placement_and_limit() {
    let cases = [
        (
            "SELECT value FROM events ORDER BY value ASC NULLS FIRST LIMIT 7",
            OrderDirection::Asc,
            NullOrder::First,
        ),
        (
            "select value from events order by value asc nulls last limit 7;",
            OrderDirection::Asc,
            NullOrder::Last,
        ),
        (
            "SELECT value FROM events ORDER\tBY\nvalue DESC NULLS FIRST LIMIT 7 ; ",
            OrderDirection::Desc,
            NullOrder::First,
        ),
        (
            "SELECT value FROM events ORDER BY value DESC NULLS LAST LIMIT 7",
            OrderDirection::Desc,
            NullOrder::Last,
        ),
    ];

    for (input, direction, null_order) in cases {
        let statement = parse_select(input, ParseLimits::default()).unwrap();
        let order_by = statement.order_by().unwrap();

        assert_eq!(order_by.column_name().as_str(), "value", "{input:?}");
        assert_eq!(order_by.direction(), direction, "{input:?}");
        assert_eq!(order_by.null_order(), null_order, "{input:?}");
        assert_eq!(statement.limit(), Some(7), "{input:?}");
    }
}

#[test]
fn order_by_requires_every_explicit_component_and_a_limit() {
    for input in [
        "SELECT c FROM t ORDER c ASC NULLS FIRST LIMIT 1",
        "SELECT c FROM t ORDER BY c NULLS FIRST LIMIT 1",
        "SELECT c FROM t ORDER BY c ASC FIRST LIMIT 1",
        "SELECT c FROM t ORDER BY c ASC NULLS LIMIT 1",
        "SELECT c FROM t ORDER BY c ASC NULLS FIRST",
    ] {
        assert!(
            matches!(
                parse_select(input, ParseLimits::default()),
                Err(ParseError::UnexpectedInput { .. })
            ),
            "{input:?}"
        );
    }
}

#[test]
fn parses_every_where_comparison_operator() {
    let cases = [
        ("=", ComparisonOperator::Eq),
        ("!=", ComparisonOperator::Ne),
        ("<>", ComparisonOperator::Ne),
        ("<", ComparisonOperator::Lt),
        ("<=", ComparisonOperator::Le),
        (">", ComparisonOperator::Gt),
        (">=", ComparisonOperator::Ge),
    ];

    for (sql_operator, expected) in cases {
        let input = format!("SELECT value FROM events WHERE value {sql_operator} 7");
        let statement = parse_select(&input, ParseLimits::default()).unwrap();
        let predicate = statement.predicate().unwrap();

        assert_eq!(predicate.column_name().as_str(), "value", "{input:?}");
        assert_eq!(predicate.operator(), expected, "{input:?}");
        assert_eq!(predicate.value(), 7, "{input:?}");
    }
}

#[test]
fn parses_where_comparison_casing_whitespace_bounds_and_limit() {
    let cases = [
        (
            "SELECT value FROM events WHERE value = 7",
            ComparisonOperator::Eq,
            7,
            None,
        ),
        (
            "select value from events where value!=-9;",
            ComparisonOperator::Ne,
            -9,
            None,
        ),
        (
            " SELECT value FROM events WhErE value >= +0 LIMIT 2 ; ",
            ComparisonOperator::Ge,
            0,
            Some(2),
        ),
        (
            "SELECT value FROM events WHERE value <= -9223372036854775808",
            ComparisonOperator::Le,
            i64::MIN,
            None,
        ),
        (
            "SELECT value FROM events WHERE value<9223372036854775807;",
            ComparisonOperator::Lt,
            i64::MAX,
            None,
        ),
    ];

    for (input, operator, value, limit) in cases {
        let statement = parse_select(input, ParseLimits::default()).unwrap();
        let predicate = statement.predicate().unwrap();

        assert_eq!(predicate.column_name().as_str(), "value", "{input:?}");
        assert_eq!(predicate.operator(), operator, "{input:?}");
        assert_eq!(predicate.value(), value, "{input:?}");
        assert_eq!(statement.limit(), limit, "{input:?}");
    }
}

#[test]
fn rejects_invalid_and_overflowing_where_literals_with_byte_offsets() {
    let invalid = [
        "SELECT c FROM t WHERE c = ",
        "SELECT c FROM t WHERE c = NULL",
        "SELECT c FROM t WHERE c = --1",
    ];

    for input in invalid {
        let offset = input.find("= ").unwrap() + 2;
        assert_eq!(
            parse_select(input, ParseLimits::default()),
            Err(ParseError::InvalidInt64 { offset }),
            "{input:?}"
        );
    }

    let input = "SELECT c FROM t WHERE c = 1.5";
    assert_eq!(
        parse_select(input, ParseLimits::default()),
        Err(ParseError::InvalidInt64 {
            offset: input.find('.').unwrap(),
        })
    );

    for input in [
        "SELECT c FROM t WHERE c = -9223372036854775809",
        "SELECT c FROM t WHERE c = 9223372036854775808",
    ] {
        assert_eq!(
            parse_select(input, ParseLimits::default()),
            Err(ParseError::Int64Overflow {
                offset: input.find("= ").unwrap() + 2,
            }),
            "{input:?}"
        );
    }
}

#[test]
fn bounds_the_where_identifier() {
    let input = "SELECT c FROM t WHERE column123 = 1";
    assert_eq!(
        parse_select(input, ParseLimits::new(input.len(), 8)),
        Err(ParseError::IdentifierTooLong {
            offset: input.find("column123").unwrap(),
            bytes: 9,
            max_bytes: 8,
        })
    );
}

#[test]
fn rejects_missing_and_malformed_comparison_operators() {
    let missing_column = "SELECT c FROM t WHERE ";
    assert_eq!(
        parse_select(missing_column, ParseLimits::default()),
        Err(ParseError::UnexpectedInput {
            offset: 22,
            expected: "identifier",
        })
    );

    let prefix = "SELECT c FROM t WHERE c ";
    for malformed in [
        "1", "==", "!==", "<<", ">>", "=>", "=<", "><", "<=>", "!", "! =",
    ] {
        let input = format!("{prefix}{malformed} 1");

        assert_eq!(
            parse_select(&input, ParseLimits::default()),
            Err(ParseError::UnexpectedInput {
                offset: prefix.len(),
                expected: "comparison operator",
            }),
            "{input:?}"
        );
    }
}

#[test]
fn parses_zero_exact_and_platform_maximum_limits() {
    let cases = [
        ("SELECT value FROM events LIMIT 0", 0),
        ("select value from events limit 25;", 25),
        (
            &format!("SELECT value FROM events LiMiT {} ;", usize::MAX),
            usize::MAX,
        ),
    ];

    for (input, expected) in cases {
        let statement = parse_select(input, ParseLimits::default()).unwrap();
        assert_eq!(statement.limit(), Some(expected), "{input:?}");
    }
}

#[test]
fn rejects_malformed_and_overflowing_limits_with_byte_offsets() {
    let malformed = [
        "SELECT value FROM events LIMIT ",
        "SELECT value FROM events LIMIT -1",
        "SELECT value FROM events LIMIT 1.5",
        "SELECT value FROM events LIMIT many",
    ];

    for input in malformed {
        let value_offset = input.find("LIMIT").unwrap() + "LIMIT".len() + 1;
        assert_eq!(
            parse_select(input, ParseLimits::default()),
            Err(ParseError::InvalidLimit {
                offset: value_offset,
            }),
            "{input:?}"
        );
    }

    let input = format!("SELECT value FROM events LIMIT {}0", usize::MAX);
    let value_offset = input.find(usize::MAX.to_string().as_str()).unwrap();
    assert_eq!(
        parse_select(&input, ParseLimits::default()),
        Err(ParseError::LimitOverflow {
            offset: value_offset,
        })
    );
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
