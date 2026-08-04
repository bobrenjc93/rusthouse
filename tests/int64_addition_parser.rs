use rusthouse::{ParseError, ParseLimits, SelectProjection, parse_select};

#[test]
fn parses_int64_addition_bounds_limits_and_semicolons() {
    let cases = [
        ("SELECT c + 0 FROM t", 0, None),
        ("select c+1 from t limit 0;", 1, Some(0)),
        (
            " SELECT c \t+\n-9223372036854775808 FROM t LIMIT 2 ; ",
            i64::MIN,
            Some(2),
        ),
        ("SELECT c + +9223372036854775807 FROM t;", i64::MAX, None),
    ];

    for (input, expected_addend, expected_limit) in cases {
        let statement = parse_select(input, ParseLimits::default()).unwrap();

        assert_eq!(statement.column_name().as_str(), "c", "{input:?}");
        assert_eq!(statement.projection().int64_addend(), Some(expected_addend));
        assert_eq!(statement.limit(), expected_limit, "{input:?}");
        assert!(matches!(
            statement.projection(),
            SelectProjection::Int64Addition { .. }
        ));
    }
}

#[test]
fn bounds_the_addition_column_identifier() {
    let input = "SELECT column123 + 1 FROM t";

    assert_eq!(
        parse_select(input, ParseLimits::new(input.len(), 8)),
        Err(ParseError::IdentifierTooLong {
            offset: 7,
            bytes: 9,
            max_bytes: 8,
        })
    );
}

#[test]
fn parses_addition_on_a_column_named_from() {
    for input in ["SELECT FROM + 1 FROM t", "SELECT FROM+1 FROM t;"] {
        let statement = parse_select(input, ParseLimits::default()).unwrap();

        assert_eq!(statement.column_name().as_str(), "FROM", "{input:?}");
        assert_eq!(statement.projection().int64_addend(), Some(1), "{input:?}");
        assert_eq!(statement.table_name().as_str(), "t", "{input:?}");
    }
}

#[test]
fn rejects_malformed_and_overflowing_addition_literals_with_offsets() {
    for input in [
        "SELECT c + FROM t",
        "SELECT c + NULL FROM t",
        "SELECT c + --1 FROM t",
    ] {
        assert_eq!(
            parse_select(input, ParseLimits::default()),
            Err(ParseError::InvalidInt64 {
                offset: input.find('+').unwrap() + 2,
            }),
            "{input:?}"
        );
    }

    let input = "SELECT c + 1.5 FROM t";
    assert_eq!(
        parse_select(input, ParseLimits::default()),
        Err(ParseError::InvalidInt64 {
            offset: input.find('.').unwrap(),
        })
    );

    for input in [
        "SELECT c + -9223372036854775809 FROM t",
        "SELECT c + 9223372036854775808 FROM t",
    ] {
        assert_eq!(
            parse_select(input, ParseLimits::default()),
            Err(ParseError::Int64Overflow {
                offset: input.find('+').unwrap() + 2,
            }),
            "{input:?}"
        );
    }
}
