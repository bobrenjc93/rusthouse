use rusthouse::{
    ColumnDefinition, CreateTableStatement, DataType, Field, IdentifierContext, ParseError,
    ParseErrorKind, ParseLimits, Table, parse_create_table, parse_create_table_with_limits,
};

fn parse_error(input: &str) -> ParseError {
    parse_create_table(input).expect_err("input should be rejected")
}

#[test]
fn parses_every_supported_column_type() {
    let statement = parse_create_table(
        "CREATE TABLE readings (sequence Int64, value Float64, active Bool, label String)",
    )
    .unwrap();

    assert_eq!(
        statement,
        CreateTableStatement {
            name: "readings".to_owned(),
            columns: vec![
                ColumnDefinition {
                    name: "sequence".to_owned(),
                    data_type: DataType::Int64,
                },
                ColumnDefinition {
                    name: "value".to_owned(),
                    data_type: DataType::Float64,
                },
                ColumnDefinition {
                    name: "active".to_owned(),
                    data_type: DataType::Bool,
                },
                ColumnDefinition {
                    name: "label".to_owned(),
                    data_type: DataType::String,
                },
            ],
        }
    );
}

#[test]
fn parsed_columns_form_a_storage_schema_without_type_conversion() {
    let statement = parse_create_table("CREATE TABLE events (id Int64, active Bool)").unwrap();
    let fields = statement
        .columns
        .into_iter()
        .map(|column| Field::new(column.name, column.data_type))
        .collect();
    let table = Table::new(fields).unwrap();

    assert_eq!(table.fields()[0].data_type(), DataType::Int64);
    assert_eq!(
        table.fields()[1].data_type(),
        rusthouse::sql::DataType::Bool
    );
}

#[test]
fn accepts_keyword_casing_and_sql_whitespace() {
    let statement = parse_create_table(
        "\r\n cReAtE\tTaBlE Metrics\x0c(\n ID iNt64 ,\r Ratio FLOAT64, Enabled bOoL, Note sTrInG\n); \t",
    )
    .unwrap();

    assert_eq!(statement.name, "Metrics");
    assert_eq!(
        statement.columns,
        [
            ColumnDefinition {
                name: "ID".to_owned(),
                data_type: DataType::Int64,
            },
            ColumnDefinition {
                name: "Ratio".to_owned(),
                data_type: DataType::Float64,
            },
            ColumnDefinition {
                name: "Enabled".to_owned(),
                data_type: DataType::Bool,
            },
            ColumnDefinition {
                name: "Note".to_owned(),
                data_type: DataType::String,
            },
        ]
    );
}

#[test]
fn rejects_empty_column_declarations_at_their_byte_positions() {
    for (input, position) in [
        ("CREATE TABLE empty ()", 20),
        ("CREATE TABLE empty (   )", 23),
        ("CREATE TABLE empty (, id Int64)", 20),
        ("CREATE TABLE empty (id Int64,, name String)", 29),
        ("CREATE TABLE empty (id Int64,   )", 32),
        ("CREATE TABLE empty (id Int64,", 29),
    ] {
        let error = parse_error(input);
        assert_eq!(error.position, position, "input: {input:?}");
        assert_eq!(error.kind, ParseErrorKind::EmptyColumn, "input: {input:?}");
    }
}

#[test]
fn rejects_duplicate_columns_case_insensitively() {
    let input = "CREATE TABLE events (event_id Int64, EVENT_ID String)";
    let error = parse_error(input);

    assert_eq!(error.position, input.find("EVENT_ID").unwrap());
    assert_eq!(
        error.kind,
        ParseErrorKind::DuplicateColumn {
            name: "EVENT_ID".to_owned(),
            first_position: input.find("event_id").unwrap(),
        }
    );
}

#[test]
fn rejects_invalid_table_and_column_identifiers() {
    let table_input = "CREATE TABLE 9events (id Int64)";
    let table_error = parse_error(table_input);
    assert_eq!(table_error.position, table_input.find('9').unwrap());
    assert_eq!(
        table_error.kind,
        ParseErrorKind::InvalidIdentifier {
            context: IdentifierContext::Table,
            identifier: "9events".to_owned(),
        }
    );

    let column_input = "CREATE TABLE events (event-id Int64)";
    let column_error = parse_error(column_input);
    assert_eq!(column_error.position, column_input.find('-').unwrap());
    assert_eq!(
        column_error.kind,
        ParseErrorKind::InvalidIdentifier {
            context: IdentifierContext::Column,
            identifier: "event-id".to_owned(),
        }
    );

    let unicode_input = "CREATE TABLE events (cafe_é String)";
    let unicode_error = parse_error(unicode_input);
    assert_eq!(unicode_error.position, unicode_input.find('é').unwrap());
    assert!(matches!(
        unicode_error.kind,
        ParseErrorKind::InvalidIdentifier {
            context: IdentifierContext::Column,
            ..
        }
    ));
}

#[test]
fn rejects_unknown_types_at_the_type_name() {
    let input = "CREATE TABLE events (created_at Timestamp)";
    let error = parse_error(input);

    assert_eq!(error.position, input.find("Timestamp").unwrap());
    assert_eq!(
        error.kind,
        ParseErrorKind::UnknownType {
            type_name: "Timestamp".to_owned(),
        }
    );
}

#[test]
fn rejects_trailing_syntax_after_the_statement_or_terminator() {
    for input in [
        "CREATE TABLE t (id Int64) garbage",
        "CREATE TABLE t (id Int64); SELECT id FROM t",
        "CREATE TABLE t (id Int64);;",
    ] {
        let error = parse_error(input);
        let closing = input.find(')').unwrap();
        let expected_position = input[closing + 1..]
            .bytes()
            .position(|byte| !matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c | b';'))
            .map_or_else(|| input.rfind(';').unwrap(), |offset| closing + 1 + offset);

        assert_eq!(error.position, expected_position, "input: {input:?}");
        assert_eq!(error.kind, ParseErrorKind::TrailingSyntax);
    }
}

#[test]
fn enforces_input_limit_at_the_exact_byte_boundary() {
    let input = "CREATE TABLE t (id Int64)";
    let exact_limits = ParseLimits::new(input.len(), 1);
    assert!(parse_create_table_with_limits(input, exact_limits).is_ok());

    let error = parse_create_table_with_limits(input, ParseLimits::new(input.len() - 1, 1))
        .expect_err("one byte over the limit should fail");
    assert_eq!(error.position, input.len() - 1);
    assert_eq!(
        error.kind,
        ParseErrorKind::InputTooLong {
            limit: input.len() - 1,
            actual: input.len(),
        }
    );

    let empty_error = parse_create_table_with_limits("C", ParseLimits::new(0, 1)).unwrap_err();
    assert_eq!(empty_error.position, 0);
    assert!(matches!(
        empty_error.kind,
        ParseErrorKind::InputTooLong {
            limit: 0,
            actual: 1
        }
    ));
}

#[test]
fn enforces_column_limit_at_the_next_column_boundary() {
    let input = "CREATE TABLE t (a Int64, b Bool)";
    assert!(parse_create_table_with_limits(input, ParseLimits::new(input.len(), 2)).is_ok());

    let error = parse_create_table_with_limits(input, ParseLimits::new(input.len(), 1))
        .expect_err("second column should exceed the limit");
    assert_eq!(error.position, input.find("b Bool").unwrap());
    assert_eq!(error.kind, ParseErrorKind::TooManyColumns { limit: 1 });

    let zero_error =
        parse_create_table_with_limits("CREATE TABLE t (a Int64)", ParseLimits::new(usize::MAX, 0))
            .unwrap_err();
    assert_eq!(zero_error.position, 16);
    assert_eq!(zero_error.kind, ParseErrorKind::TooManyColumns { limit: 0 });
}

#[test]
fn reports_deterministic_errors_for_malformed_input() {
    let cases = [
        (
            "",
            0,
            ParseErrorKind::ExpectedKeyword {
                expected: "CREATE",
                found: None,
            },
        ),
        (
            "CREATE",
            6,
            ParseErrorKind::ExpectedKeyword {
                expected: "TABLE",
                found: None,
            },
        ),
        (
            "CREATE TABLE (id Int64)",
            13,
            ParseErrorKind::ExpectedIdentifier {
                context: IdentifierContext::Table,
            },
        ),
        (
            "CREATE TABLE t id Int64)",
            15,
            ParseErrorKind::ExpectedToken { expected: "'('" },
        ),
        ("CREATE TABLE t (id)", 18, ParseErrorKind::ExpectedType),
        (
            "CREATE TABLE t (id Int64 name String)",
            25,
            ParseErrorKind::ExpectedToken {
                expected: "',' or ')'",
            },
        ),
        (
            "CREATE TABLE t (id Int64",
            24,
            ParseErrorKind::ExpectedToken {
                expected: "',' or ')'",
            },
        ),
    ];

    for (input, position, kind) in cases {
        let error = parse_error(input);
        assert_eq!(error.position, position, "input: {input:?}");
        assert_eq!(error.kind, kind, "input: {input:?}");
    }
}

#[test]
fn rejects_non_create_table_statements() {
    for input in [
        "SELECT * FROM system.tables",
        "INSERT INTO t VALUES (1)",
        "DROP TABLE t",
        "ALTER TABLE t ADD COLUMN value Int64",
    ] {
        let error = parse_error(input);
        assert_eq!(error.position, 0, "input: {input:?}");
        assert!(matches!(
            error.kind,
            ParseErrorKind::ExpectedKeyword {
                expected: "CREATE",
                ..
            }
        ));
    }
}

#[test]
fn parse_errors_implement_standard_error_display() {
    let error = parse_error("CREATE TABLE t (id Decimal)");
    let standard_error: &dyn std::error::Error = &error;

    assert_eq!(
        standard_error.to_string(),
        "SQL parse error at byte 19: unknown column type \"Decimal\""
    );
}
