use rusthouse::{
    ColumnDefinition, ColumnType, CreateTable, Keyword, MAX_COLUMNS, MAX_INPUT_BYTES, MAX_TOKENS,
    ParseErrorKind, parse_create_table,
};

#[test]
fn parses_case_insensitive_keywords_and_all_column_types() {
    let statement = parse_create_table(
        "cReAtE tAbLe Metrics (count iNt64, ratio FLOAT64, ready bOoL, label string);",
    )
    .unwrap();

    assert_eq!(
        statement,
        CreateTable {
            name: "Metrics".to_owned(),
            columns: vec![
                ColumnDefinition {
                    name: "count".to_owned(),
                    column_type: ColumnType::Int64,
                },
                ColumnDefinition {
                    name: "ratio".to_owned(),
                    column_type: ColumnType::Float64,
                },
                ColumnDefinition {
                    name: "ready".to_owned(),
                    column_type: ColumnType::Bool,
                },
                ColumnDefinition {
                    name: "label".to_owned(),
                    column_type: ColumnType::String,
                },
            ],
        }
    );
}

#[test]
fn returns_typed_positional_errors_for_malformed_definitions() {
    let missing_create = parse_create_table("TABLE t (id Int64)").unwrap_err();
    assert_eq!(missing_create.position, 0);
    assert_eq!(
        missing_create.kind,
        ParseErrorKind::ExpectedKeyword {
            keyword: Keyword::Create
        }
    );

    let empty_columns = parse_create_table("CREATE TABLE t ()").unwrap_err();
    assert_eq!(empty_columns.position, 16);
    assert_eq!(empty_columns.kind, ParseErrorKind::ExpectedIdentifier);

    let missing_type = parse_create_table("CREATE TABLE t (id, name String)").unwrap_err();
    assert_eq!(missing_type.position, 18);
    assert_eq!(missing_type.kind, ParseErrorKind::ExpectedColumnType);

    let unknown_type = parse_create_table("CREATE TABLE t (id UInt64)").unwrap_err();
    assert_eq!(unknown_type.position, 19);
    assert_eq!(
        unknown_type.kind,
        ParseErrorKind::UnknownColumnType {
            found: "UInt64".to_owned()
        }
    );

    let missing_comma = parse_create_table("CREATE TABLE t (id Int64 name String)").unwrap_err();
    assert_eq!(missing_comma.position, 25);
    assert_eq!(
        missing_comma.kind,
        ParseErrorKind::ExpectedCommaOrRightParenthesis
    );

    let trailing_comma = parse_create_table("CREATE TABLE t (id Int64,)").unwrap_err();
    assert_eq!(trailing_comma.position, 25);
    assert_eq!(trailing_comma.kind, ParseErrorKind::ExpectedIdentifier);
}

#[test]
fn rejects_duplicate_columns_case_insensitively() {
    let error = parse_create_table("CREATE TABLE t (UserId Int64, userid String)").unwrap_err();

    assert_eq!(error.position, 30);
    assert_eq!(
        error.kind,
        ParseErrorKind::DuplicateColumn {
            name: "userid".to_owned(),
            first_position: 16,
        }
    );
}

#[test]
fn rejects_trailing_input_after_statement_or_semicolon() {
    for (sql, trailing) in [
        ("CREATE TABLE t (id Int64) CREATE", "CREATE"),
        ("CREATE TABLE t (id Int64); TABLE", "TABLE"),
        ("CREATE TABLE t (id Int64);;", ";"),
    ] {
        let error = parse_create_table(sql).unwrap_err();
        assert_eq!(error.kind, ParseErrorKind::TrailingInput);
        assert_eq!(error.position, sql.rfind(trailing).unwrap());
    }
}

#[test]
fn reports_unexpected_characters_at_their_byte_offset() {
    let error = parse_create_table("CREATE TABLE café (id Int64)").unwrap_err();

    assert_eq!(error.position, 16);
    assert_eq!(
        error.kind,
        ParseErrorKind::UnexpectedCharacter { character: 'é' }
    );
}

#[test]
fn accepts_the_input_byte_limit_and_rejects_the_next_byte() {
    let base = "CREATE TABLE t (id Int64)";
    let at_limit = format!("{base}{}", " ".repeat(MAX_INPUT_BYTES - base.len()));
    assert_eq!(at_limit.len(), MAX_INPUT_BYTES);
    assert!(parse_create_table(&at_limit).is_ok());

    let over_limit = format!("{at_limit} ");
    let error = parse_create_table(&over_limit).unwrap_err();
    assert_eq!(error.position, MAX_INPUT_BYTES);
    assert_eq!(
        error.kind,
        ParseErrorKind::InputTooLong {
            limit: MAX_INPUT_BYTES,
            actual: MAX_INPUT_BYTES + 1,
        }
    );
}

#[test]
fn accepts_the_token_limit_and_rejects_the_next_token() {
    let at_limit = "x ".repeat(MAX_TOKENS);
    let syntax_error = parse_create_table(&at_limit).unwrap_err();
    assert_eq!(
        syntax_error.kind,
        ParseErrorKind::ExpectedKeyword {
            keyword: Keyword::Create
        }
    );

    let over_limit = format!("{at_limit}x");
    let error = parse_create_table(&over_limit).unwrap_err();
    assert_eq!(error.position, at_limit.len());
    assert_eq!(
        error.kind,
        ParseErrorKind::TooManyTokens { limit: MAX_TOKENS }
    );
}

#[test]
fn accepts_the_column_limit_and_rejects_the_next_definition() {
    let definitions = (0..MAX_COLUMNS)
        .map(|index| format!("c{index} Int64"))
        .collect::<Vec<_>>()
        .join(",");
    let at_limit = format!("CREATE TABLE t ({definitions})");
    let statement = parse_create_table(&at_limit).unwrap();
    assert_eq!(statement.columns.len(), MAX_COLUMNS);

    let extra_column = format!("c{MAX_COLUMNS} Int64");
    let over_limit = format!("CREATE TABLE t ({definitions},{extra_column})");
    let error = parse_create_table(&over_limit).unwrap_err();
    assert_eq!(error.position, over_limit.rfind(&extra_column).unwrap());
    assert_eq!(
        error.kind,
        ParseErrorKind::TooManyColumns { limit: MAX_COLUMNS }
    );
}
