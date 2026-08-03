use rusthouse::{ParseError, ParseLimits, parse_grouped_count};

#[test]
fn parses_the_exact_grouped_count_shape_with_an_optional_semicolon() {
    let cases = [
        ("SELECT c, COUNT(*) FROM t GROUP BY c", ("c", "t", "c")),
        (
            " \nSeLeCt group_key , count(*) FrOm event_rows gRoUp bY group_key ;\t",
            ("group_key", "event_rows", "group_key"),
        ),
    ];

    for (input, expected) in cases {
        let statement = parse_grouped_count(input, ParseLimits::default()).unwrap();

        assert_eq!(statement.column_name().as_str(), expected.0, "{input:?}");
        assert_eq!(statement.table_name().as_str(), expected.1, "{input:?}");
        assert_eq!(
            statement.group_by_column_name().as_str(),
            expected.2,
            "{input:?}"
        );
    }
}

#[test]
fn retains_selected_and_grouped_identifiers_independently() {
    let statement = parse_grouped_count(
        "SELECT selected, COUNT(*) FROM events GROUP BY grouped",
        ParseLimits::default(),
    )
    .unwrap();

    assert_eq!(statement.column_name().as_str(), "selected");
    assert_eq!(statement.group_by_column_name().as_str(), "grouped");
}

#[test]
fn applies_statement_and_identifier_byte_bounds() {
    let input = "SELECT key, COUNT(*) FROM events GROUP BY key;";
    assert_eq!(
        parse_grouped_count(input, ParseLimits::new(input.len() - 1, 128)),
        Err(ParseError::StatementTooLong {
            bytes: input.len(),
            max_bytes: input.len() - 1,
        })
    );

    for (input, identifier) in [
        ("SELECT selected, COUNT(*) FROM t GROUP BY c", "selected"),
        ("SELECT c, COUNT(*) FROM events7 GROUP BY c", "events7"),
        ("SELECT c, COUNT(*) FROM t GROUP BY grouped", "grouped"),
    ] {
        assert_eq!(
            parse_grouped_count(input, ParseLimits::new(input.len(), 6)),
            Err(ParseError::IdentifierTooLong {
                offset: input.find(identifier).unwrap(),
                bytes: identifier.len(),
                max_bytes: 6,
            }),
            "{input:?}"
        );
    }
}

#[test]
fn rejects_aggregate_variations_and_extra_clauses() {
    for input in [
        "SELECT c COUNT(*) FROM t GROUP BY c",
        "SELECT c, SUM(*) FROM t GROUP BY c",
        "SELECT c, COUNT(c) FROM t GROUP BY c",
        "SELECT c, COUNT(*) AS count FROM t GROUP BY c",
        "SELECT c, COUNT(*) FROM t WHERE c = 1 GROUP BY c",
        "SELECT c, COUNT(*) FROM t GROUP BY c LIMIT 1",
        "SELECT c, COUNT(*) FROM t GROUP BY c;;",
    ] {
        assert!(
            parse_grouped_count(input, ParseLimits::default()).is_err(),
            "{input:?}"
        );
    }
}
