use rusthouse::{
    ComparisonOperator, ComparisonPredicate, IdentifierContext, OrderByClause, OrderDirection,
    ParseError, ParseErrorKind, SelectParseLimits, SelectProjection, SelectStatement, Value,
    parse_select, parse_select_with_limits,
};

fn parse_error(input: &str) -> ParseError {
    parse_select(input).expect_err("input should be rejected")
}

#[test]
fn parses_wildcard_and_named_projections() {
    assert_eq!(
        parse_select("SELECT * FROM events").unwrap(),
        SelectStatement {
            projections: SelectProjection::All,
            table: "events".to_owned(),
            predicate: None,
            order_by: None,
            limit: None,
        }
    );

    assert_eq!(
        parse_select("\r\n sElEcT id,\tLabel, active\x0cFrOm Events ; \n").unwrap(),
        SelectStatement {
            projections: SelectProjection::Columns(vec![
                "id".to_owned(),
                "Label".to_owned(),
                "active".to_owned(),
            ]),
            table: "Events".to_owned(),
            predicate: None,
            order_by: None,
            limit: None,
        }
    );
}

#[test]
fn parses_count_all_with_an_optional_alias_and_predicate() {
    assert_eq!(
        parse_select("SELECT COUNT(*) FROM events").unwrap(),
        SelectStatement {
            projections: SelectProjection::CountAll { alias: None },
            table: "events".to_owned(),
            predicate: None,
            order_by: None,
            limit: None,
        }
    );

    assert_eq!(
        parse_select("select count ( * ) as Matching from Events where active = true").unwrap(),
        SelectStatement {
            projections: SelectProjection::CountAll {
                alias: Some("Matching".to_owned()),
            },
            table: "Events".to_owned(),
            predicate: Some(ComparisonPredicate {
                column: "active".to_owned(),
                operator: ComparisonOperator::Equal,
                value: Value::Bool(true),
            }),
            order_by: None,
            limit: None,
        }
    );

    let modified =
        parse_select("SELECT COUNT(*) FROM events ORDER BY active DESC LIMIT 0").unwrap();
    assert_eq!(
        modified.order_by,
        Some(OrderByClause {
            column: "active".to_owned(),
            direction: OrderDirection::Descending,
        })
    );
    assert_eq!(modified.limit, Some(0));
}

#[test]
fn parses_wildcard_adjacent_to_select_and_from_keywords() {
    let expected = SelectStatement {
        projections: SelectProjection::All,
        table: "events".to_owned(),
        predicate: None,
        order_by: None,
        limit: None,
    };

    for input in [
        "SELECT* FROM events",
        "SELECT *FROM events",
        "SELECT*FROM events",
    ] {
        assert_eq!(parse_select(input).unwrap(), expected, "input: {input:?}");
    }
}

#[test]
fn parses_single_column_ordering_and_limit_in_clause_order() {
    let ascending = parse_select("SELECT id FROM events ORDER BY score").unwrap();
    assert_eq!(
        ascending.order_by,
        Some(OrderByClause {
            column: "score".to_owned(),
            direction: OrderDirection::Ascending,
        })
    );
    assert_eq!(ascending.limit, None);

    let descending =
        parse_select("SELECT * FROM events WHERE active = true oRdEr bY score dEsC LiMiT 5;")
            .unwrap();
    assert_eq!(
        descending.order_by,
        Some(OrderByClause {
            column: "score".to_owned(),
            direction: OrderDirection::Descending,
        })
    );
    assert!(descending.predicate.is_some());
    assert_eq!(descending.limit, Some(5));

    let explicit = parse_select("SELECT * FROM events ORDER BY score ASC LIMIT 0").unwrap();
    assert_eq!(
        explicit.order_by.unwrap().direction,
        OrderDirection::Ascending
    );
    assert_eq!(explicit.limit, Some(0));
}

#[test]
fn parses_limit_without_ordering() {
    let statement = parse_select("SELECT * FROM events WHERE active = true LIMIT 25").unwrap();
    assert_eq!(statement.order_by, None);
    assert_eq!(statement.limit, Some(25));
}

#[test]
fn rejects_multiple_order_keys_expressions_and_misordered_clauses() {
    let cases = [
        ("SELECT * FROM events ORDER BY score, id", ","),
        ("SELECT * FROM events ORDER BY score + 1", "+"),
        ("SELECT * FROM events ORDER BY lower(score)", "("),
        (
            "SELECT * FROM events ORDER BY score WHERE active = true",
            "WHERE",
        ),
        ("SELECT * FROM events LIMIT 1 ORDER BY score", "ORDER"),
    ];

    for (input, marker) in cases {
        let error = parse_error(input);
        assert_eq!(
            error.position,
            input.find(marker).unwrap(),
            "input: {input:?}"
        );
        assert_eq!(
            error.kind,
            ParseErrorKind::TrailingSyntax,
            "input: {input:?}"
        );
    }
}

#[test]
fn reports_positioned_order_and_limit_errors() {
    let missing_by = "SELECT * FROM events ORDER score";
    let error = parse_error(missing_by);
    assert_eq!(error.position, missing_by.find("score").unwrap());
    assert!(matches!(
        error.kind,
        ParseErrorKind::ExpectedKeyword { expected: "BY", .. }
    ));

    let missing_column = "SELECT * FROM events ORDER BY";
    let error = parse_error(missing_column);
    assert_eq!(error.position, missing_column.len());
    assert_eq!(
        error.kind,
        ParseErrorKind::ExpectedIdentifier {
            context: IdentifierContext::Column,
        }
    );

    for (input, kind) in [
        ("SELECT * FROM events LIMIT", ParseErrorKind::ExpectedLimit),
        (
            "SELECT * FROM events LIMIT -1",
            ParseErrorKind::InvalidLimit {
                literal: "-1".to_owned(),
            },
        ),
        (
            "SELECT * FROM events LIMIT 1.5",
            ParseErrorKind::InvalidLimit {
                literal: "1.5".to_owned(),
            },
        ),
    ] {
        let error = parse_error(input);
        let value_position = input.find("LIMIT").unwrap() + "LIMIT".len();
        let expected_position = input[value_position..]
            .find(|character: char| !character.is_ascii_whitespace())
            .map_or(input.len(), |offset| value_position + offset);
        assert_eq!(error.position, expected_position, "input: {input:?}");
        assert_eq!(error.kind, kind, "input: {input:?}");
    }

    let overflow = format!("SELECT * FROM events LIMIT {}0", usize::MAX);
    let error = parse_error(&overflow);
    assert_eq!(
        error.kind,
        ParseErrorKind::LimitOutOfRange {
            literal: format!("{}0", usize::MAX),
        }
    );
}

#[test]
fn parses_all_comparison_operators_without_required_whitespace() {
    let cases = [
        ("=", ComparisonOperator::Equal),
        ("!=", ComparisonOperator::NotEqual),
        ("<>", ComparisonOperator::NotEqual),
        ("<", ComparisonOperator::LessThan),
        ("<=", ComparisonOperator::LessThanOrEqual),
        (">", ComparisonOperator::GreaterThan),
        (">=", ComparisonOperator::GreaterThanOrEqual),
    ];

    for (syntax, operator) in cases {
        let input = format!("SELECT id FROM events WHERE sequence{syntax}42");
        let statement = parse_select(&input).unwrap();
        assert_eq!(
            statement.predicate,
            Some(ComparisonPredicate {
                column: "sequence".to_owned(),
                operator,
                value: Value::Int64(42),
            }),
            "input: {input:?}"
        );
    }
}

#[test]
fn predicates_reuse_every_supported_literal_type() {
    let cases = [
        ("id = -9223372036854775808", Value::Int64(i64::MIN)),
        ("ratio >= .5e1", Value::Float64(5.0)),
        ("active != FALSE", Value::Bool(false)),
        ("label = 'can''t'", Value::String("can't".to_owned())),
    ];

    for (predicate, expected_value) in cases {
        let input = format!("SELECT * FROM readings WHERE {predicate}");
        assert_eq!(
            parse_select(&input).unwrap().predicate.unwrap().value,
            expected_value,
            "input: {input:?}"
        );
    }
}

#[test]
fn enforces_input_limit_at_the_exact_byte_boundary() {
    let input = "SELECT id FROM events WHERE id = 1";
    assert!(parse_select_with_limits(input, SelectParseLimits::new(input.len(), 1)).is_ok());

    let error =
        parse_select_with_limits(input, SelectParseLimits::new(input.len() - 1, usize::MAX))
            .unwrap_err();
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
fn enforces_projection_limit_at_the_next_projection() {
    let input = "SELECT first, second FROM events";
    assert!(parse_select_with_limits(input, SelectParseLimits::new(input.len(), 2)).is_ok());

    let error =
        parse_select_with_limits(input, SelectParseLimits::new(input.len(), 1)).unwrap_err();
    assert_eq!(error.position, input.find("second").unwrap());
    assert_eq!(error.kind, ParseErrorKind::TooManyProjections { limit: 1 });

    for input in [
        "SELECT id FROM events",
        "SELECT * FROM events",
        "SELECT COUNT(*) FROM events",
    ] {
        let error =
            parse_select_with_limits(input, SelectParseLimits::new(input.len(), 0)).unwrap_err();
        assert_eq!(
            error.position,
            input
                .find("COUNT")
                .unwrap_or_else(|| input.find(['i', '*']).unwrap()),
            "{input:?}"
        );
        assert_eq!(error.kind, ParseErrorKind::TooManyProjections { limit: 0 });
    }

    let count = "SELECT COUNT(*) FROM events";
    assert!(parse_select_with_limits(count, SelectParseLimits::new(count.len(), 1)).is_ok());
}

#[test]
fn reports_positioned_projection_and_identifier_errors() {
    let cases = [
        ("SELECT FROM events", "FROM"),
        ("SELECT id, FROM events", "FROM"),
        ("SELECT id,,name FROM events", ",name"),
    ];
    for (input, marker) in cases {
        let error = parse_error(input);
        assert_eq!(error.position, input.find(marker).unwrap(), "{input:?}");
        assert_eq!(error.kind, ParseErrorKind::ExpectedProjection, "{input:?}");
    }

    let input = "SELECT event-id FROM events";
    let error = parse_error(input);
    assert_eq!(error.position, input.find('-').unwrap());
    assert_eq!(
        error.kind,
        ParseErrorKind::InvalidIdentifier {
            context: IdentifierContext::Column,
            identifier: "event-id".to_owned(),
        }
    );

    let input = "SELECT * FROM 9events";
    let error = parse_error(input);
    assert_eq!(error.position, input.find('9').unwrap());
    assert!(matches!(
        error.kind,
        ParseErrorKind::InvalidIdentifier {
            context: IdentifierContext::Table,
            ..
        }
    ));
}

#[test]
fn reports_positioned_predicate_errors() {
    let cases = [
        (
            "SELECT * FROM t WHERE",
            "WHERE".len() + "SELECT * FROM t ".len(),
            ParseErrorKind::ExpectedIdentifier {
                context: IdentifierContext::Column,
            },
        ),
        (
            "SELECT * FROM t WHERE id",
            "SELECT * FROM t WHERE id".len(),
            ParseErrorKind::ExpectedComparisonOperator,
        ),
        (
            "SELECT * FROM t WHERE id == 1",
            "SELECT * FROM t WHERE id ".len(),
            ParseErrorKind::InvalidComparisonOperator {
                operator: "==".to_owned(),
            },
        ),
        (
            "SELECT * FROM t WHERE id LIKE 1",
            "SELECT * FROM t WHERE id ".len(),
            ParseErrorKind::ExpectedComparisonOperator,
        ),
        (
            "SELECT * FROM t WHERE id =",
            "SELECT * FROM t WHERE id =".len(),
            ParseErrorKind::ExpectedValue,
        ),
    ];

    for (input, position, kind) in cases {
        let error = parse_error(input);
        assert_eq!(error.position, position, "input: {input:?}");
        assert_eq!(error.kind, kind, "input: {input:?}");
    }

    let unterminated = "SELECT * FROM t WHERE label = 'open";
    let error = parse_error(unterminated);
    assert_eq!(error.position, unterminated.len());
    assert_eq!(error.kind, ParseErrorKind::UnterminatedString);
}

#[test]
fn rejects_aliases_aggregates_compound_predicates_and_unsupported_result_clauses() {
    let cases = [
        ("SELECT id AS event_id FROM events", "AS"),
        ("SELECT SUM(id) FROM events", "("),
        ("SELECT COUNT(id) FROM events", "id"),
        ("SELECT COUNT(DISTINCT id) FROM events", "DISTINCT"),
        ("SELECT COUNT(*), id FROM events", ","),
        ("SELECT id, COUNT(*) FROM events", "("),
        ("SELECT DISTINCT id FROM events", "id"),
        ("SELECT * FROM events AS e", "AS"),
        ("SELECT * FROM events WHERE id = 1 AND active = true", "AND"),
        ("SELECT * FROM events WHERE id = 1 OR id = 2", "OR"),
        ("SELECT COUNT(*) FROM events GROUP BY active", "GROUP"),
        ("SELECT * FROM events GROUP BY id", "GROUP"),
    ];

    for (input, marker) in cases {
        let error = parse_error(input);
        assert_eq!(
            error.position,
            input.find(marker).unwrap(),
            "input: {input:?}"
        );
    }
}

#[test]
fn select_parse_errors_implement_standard_error_display() {
    let error = parse_error("SELECT * FROM t WHERE id == 1");
    let standard_error: &dyn std::error::Error = &error;

    assert_eq!(
        standard_error.to_string(),
        "SQL parse error at byte 25: invalid comparison operator \"==\""
    );
}
