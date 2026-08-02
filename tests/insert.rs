use rusthouse::{
    Catalog, DataType, ExecuteInsertError, InsertError, InsertInto, Keyword, MAX_INPUT_BYTES,
    MAX_TOKENS, ParseErrorKind, Value, ValueRef, ValueType, execute_insert, parse_create_table,
    parse_insert,
};

fn events_catalog() -> Catalog {
    let mut catalog = Catalog::new();
    catalog
        .create_table(
            parse_create_table(
                "CREATE TABLE Events (id Int64, score Float64, active Bool, label String)",
            )
            .unwrap(),
        )
        .unwrap();
    catalog
}

#[test]
fn parses_every_literal_type_and_doubled_quotes() {
    let statement = parse_insert(
        "iNsErT InTo Events VaLuEs (-9223372036854775808, +42, .5, -2.5E+3, TRUE, false, 'O''Brien', '', NULL);",
    )
    .unwrap();

    assert_eq!(
        statement,
        InsertInto {
            table_name: "Events".to_owned(),
            values: vec![
                Value::Int64(i64::MIN),
                Value::Int64(42),
                Value::Float64(0.5),
                Value::Float64(-2500.0),
                Value::Bool(true),
                Value::Bool(false),
                Value::from("O'Brien"),
                Value::from(""),
                Value::Null,
            ],
        }
    );
}

#[test]
fn parses_signed_leading_decimal_floats() {
    let statement = parse_insert("INSERT INTO metrics VALUES (-.5, +.25)").unwrap();

    assert_eq!(
        statement.values,
        vec![Value::Float64(-0.5), Value::Float64(0.25)]
    );
}

#[test]
fn executes_against_the_catalog_in_schema_order() {
    let mut catalog = events_catalog();

    execute_insert(
        "INSERT INTO eVeNtS VALUES (7, 1.25, true, 'it''s ready');",
        &mut catalog,
    )
    .unwrap();

    assert_eq!(
        catalog.table("events").unwrap().row(0),
        Some(vec![
            ValueRef::Int64(7),
            ValueRef::Float64(1.25),
            ValueRef::Bool(true),
            ValueRef::String("it's ready"),
        ])
    );
}

#[test]
fn returns_typed_parse_errors_without_mutating_the_catalog() {
    let cases = [
        (
            "SELECT * FROM Events",
            ParseErrorKind::ExpectedKeyword {
                keyword: Keyword::Insert,
            },
        ),
        (
            "INSERT Events VALUES (1)",
            ParseErrorKind::ExpectedKeyword {
                keyword: Keyword::Into,
            },
        ),
        (
            "INSERT INTO Events (1)",
            ParseErrorKind::ExpectedKeyword {
                keyword: Keyword::Values,
            },
        ),
        (
            "INSERT INTO Events VALUES 1",
            ParseErrorKind::ExpectedLeftParenthesis,
        ),
        (
            "INSERT INTO Events VALUES (unknown)",
            ParseErrorKind::ExpectedValue,
        ),
        (
            "INSERT INTO Events VALUES (9223372036854775808)",
            ParseErrorKind::InvalidIntegerLiteral {
                found: "9223372036854775808".to_owned(),
            },
        ),
        (
            "INSERT INTO Events VALUES (1e)",
            ParseErrorKind::InvalidFloatLiteral {
                found: "1e".to_owned(),
            },
        ),
        (
            "INSERT INTO Events VALUES (1e309)",
            ParseErrorKind::NonFiniteFloatLiteral {
                found: "1e309".to_owned(),
            },
        ),
        (
            "INSERT INTO Events VALUES ('open)",
            ParseErrorKind::UnterminatedString,
        ),
        (
            "INSERT INTO Events VALUES (1, 2), (3, 4)",
            ParseErrorKind::TrailingInput,
        ),
    ];

    for (sql, expected_kind) in cases {
        let mut catalog = events_catalog();
        let before = catalog.clone();
        let ExecuteInsertError::Parse(error) = execute_insert(sql, &mut catalog).unwrap_err()
        else {
            panic!("{sql:?} should return a parse error");
        };
        assert_eq!(error.kind, expected_kind, "{sql:?}");
        assert_eq!(catalog, before, "{sql:?}");
    }
}

#[test]
fn unknown_table_errors_preserve_the_requested_spelling_and_catalog() {
    let mut catalog = events_catalog();
    let before = catalog.clone();

    assert_eq!(
        execute_insert(
            "INSERT INTO Missing VALUES (1, 2.0, true, 'x')",
            &mut catalog,
        ),
        Err(ExecuteInsertError::UnknownTable {
            name: "Missing".to_owned(),
        })
    );
    assert_eq!(catalog, before);
}

#[test]
fn insertion_errors_leave_an_existing_table_unchanged() {
    let cases = [
        (
            "INSERT INTO Events VALUES (2, 2.0, false)",
            InsertError::Shape {
                expected: 4,
                actual: 3,
            },
        ),
        (
            "INSERT INTO Events VALUES ()",
            InsertError::Shape {
                expected: 4,
                actual: 0,
            },
        ),
        (
            "INSERT INTO Events VALUES (true, 2.0, false, 'bad')",
            InsertError::TypeMismatch {
                column: 0,
                column_name: "id".to_owned(),
                expected: DataType::Int64,
                actual: ValueType::Bool,
            },
        ),
        (
            "INSERT INTO Events VALUES (2, 2.0, false, NULL)",
            InsertError::NullNotAllowed {
                column: 3,
                column_name: "label".to_owned(),
            },
        ),
    ];

    for (sql, expected_error) in cases {
        let mut catalog = events_catalog();
        execute_insert(
            "INSERT INTO Events VALUES (1, 1.0, true, 'existing')",
            &mut catalog,
        )
        .unwrap();
        let before = catalog.clone();

        assert_eq!(
            execute_insert(sql, &mut catalog),
            Err(ExecuteInsertError::Insertion(expected_error)),
            "{sql:?}"
        );
        assert_eq!(catalog, before, "{sql:?}");
    }
}

#[test]
fn enforces_the_existing_input_and_token_limits() {
    let prefix = "INSERT INTO t VALUES ('";
    let suffix = "')";
    let at_limit = format!(
        "{prefix}{}{suffix}",
        "x".repeat(MAX_INPUT_BYTES - prefix.len() - suffix.len())
    );
    assert_eq!(at_limit.len(), MAX_INPUT_BYTES);
    assert!(parse_insert(&at_limit).is_ok());

    let over_limit = format!("{at_limit} ");
    let error = parse_insert(&over_limit).unwrap_err();
    assert_eq!(
        error.kind,
        ParseErrorKind::InputTooLong {
            limit: MAX_INPUT_BYTES,
            actual: MAX_INPUT_BYTES + 1,
        }
    );

    let at_token_limit = "x ".repeat(MAX_TOKENS);
    assert_eq!(
        parse_insert(&at_token_limit).unwrap_err().kind,
        ParseErrorKind::ExpectedKeyword {
            keyword: Keyword::Insert,
        }
    );
    let over_token_limit = format!("{at_token_limit}x");
    assert_eq!(
        parse_insert(&over_token_limit).unwrap_err().kind,
        ParseErrorKind::TooManyTokens { limit: MAX_TOKENS }
    );
}
