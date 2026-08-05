use rusthouse::{ParseError, ParseLimits, parse_left_join};

#[test]
fn parses_only_the_narrow_left_join_with_casing_whitespace_and_semicolon() {
    for (input, projected) in [
        (
            "SELECT right_key FROM left_rows LEFT JOIN right_rows ON left_key = right_key",
            "right_key",
        ),
        (
            "  select RightKey\nfrom LeftRows left\tjoin RightRows on LeftKey=RightKey;  ",
            "RightKey",
        ),
        (
            "SeLeCt value_2 FROM table_1 LeFt JoIn table_2 ON _key = value_2 ;\r\n",
            "value_2",
        ),
    ] {
        let statement = parse_left_join(input, ParseLimits::default()).unwrap();

        assert_eq!(statement.projected_column_name().as_str(), projected);
    }

    let statement = parse_left_join(
        "SELECT value FROM lhs LEFT JOIN rhs ON left_id = right_id;",
        ParseLimits::default(),
    )
    .unwrap();
    assert_eq!(statement.projected_column_name().as_str(), "value");
    assert_eq!(statement.left_table_name().as_str(), "lhs");
    assert_eq!(statement.right_table_name().as_str(), "rhs");
    assert_eq!(statement.left_column_name().as_str(), "left_id");
    assert_eq!(statement.right_column_name().as_str(), "right_id");
}

#[test]
fn rejects_forms_outside_the_narrow_left_join_grammar() {
    for input in [
        "SELECT value FROM lhs JOIN rhs ON value = value",
        "SELECT value FROM lhs INNER JOIN rhs ON value = value",
        "SELECT value FROM lhs LEFT OUTER JOIN rhs ON value = value",
        "SELECT value FROM lhs RIGHT JOIN rhs ON value = value",
        "SELECT value FROM lhs LEFT JOIN rhs USING (value)",
        "SELECT value FROM lhs LEFT JOIN rhs ON value == value",
        "SELECT value FROM lhs LEFT JOIN rhs ON value = value WHERE value = 1",
        "SELECT rhs.value FROM lhs LEFT JOIN rhs ON value = value",
    ] {
        assert!(
            parse_left_join(input, ParseLimits::default()).is_err(),
            "{input:?}"
        );
    }
}

#[test]
fn enforces_statement_and_identifier_bounds() {
    let input = "SELECT value FROM lhs LEFT JOIN rhs ON left_id = value";
    assert_eq!(
        parse_left_join(input, ParseLimits::new(input.len() - 1, 128)),
        Err(ParseError::StatementTooLong {
            bytes: input.len(),
            max_bytes: input.len() - 1,
        })
    );
    assert_eq!(
        parse_left_join(input, ParseLimits::new(input.len(), 4)),
        Err(ParseError::IdentifierTooLong {
            offset: 7,
            bytes: 5,
            max_bytes: 4,
        })
    );
}
