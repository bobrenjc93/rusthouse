use rusthouse::sql::{
    ColumnDefinition, CreateTableStatement, DataType, MAX_COLUMNS, MAX_SQL_BYTES, ParseError,
    parse_create_table,
};

#[test]
fn parses_ordered_typed_columns_with_varied_whitespace_and_case() {
    let sql = concat!(
        "\tCrEaTe\nTaBlE metrics\r\n(\n",
        "  id iNt64,\tvalue FLOAT64,\n",
        "  enabled boOL, label string\r\n); \n",
    );

    let statement = parse_create_table(sql).unwrap();

    assert_eq!(
        statement,
        CreateTableStatement {
            table_name: "metrics".to_owned(),
            columns: vec![
                ColumnDefinition {
                    name: "id".to_owned(),
                    data_type: DataType::Int64,
                },
                ColumnDefinition {
                    name: "value".to_owned(),
                    data_type: DataType::Float64,
                },
                ColumnDefinition {
                    name: "enabled".to_owned(),
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
fn accepts_no_semicolon_and_identifier_digits_after_the_first_character() {
    let statement = parse_create_table("CREATE TABLE t_2 (column1 Int64)").unwrap();

    assert_eq!(statement.table_name, "t_2");
    assert_eq!(statement.columns[0].name, "column1");
}

#[test]
fn rejects_malformed_schemas_at_the_relevant_position() {
    let cases = [
        ("", 0, "CREATE"),
        ("CREAT TABLE t (x Int64)", 0, "CREATE"),
        ("CREATE t (x Int64)", 7, "TABLE"),
        ("CREATE TABLE 2bad (x Int64)", 13, "table name"),
        ("CREATE TABLE t", 14, "'('"),
        ("CREATE TABLE t ()", 16, "column name"),
        ("CREATE TABLE t (x)", 17, "column type"),
        ("CREATE TABLE t (x Int64 y String)", 24, "',' or ')'"),
        ("CREATE TABLE t (x Int64,)", 24, "column name"),
        ("CREATE TABLE t (x Int64", 23, "',' or ')'"),
    ];

    for (sql, expected_position, expected_description) in cases {
        let error = parse_create_table(sql).unwrap_err();
        assert_eq!(error.position(), expected_position, "SQL: {sql:?}");
        assert!(
            matches!(
                error,
                ParseError::Syntax { expected, .. } if expected == expected_description
            ),
            "SQL: {sql:?}, error: {error:?}"
        );
    }
}

#[test]
fn distinguishes_unsupported_types_and_reports_their_position() {
    let sql = "CREATE TABLE events (id UInt64)";

    assert_eq!(
        parse_create_table(sql),
        Err(ParseError::UnsupportedType {
            position: sql.find("UInt64").unwrap(),
            type_name: "UInt64".to_owned(),
        })
    );
}

#[test]
fn rejects_trailing_tokens_and_statements() {
    let cases = [
        "CREATE TABLE t (x Int64) unexpected",
        "CREATE TABLE t (x Int64); SELECT x FROM t",
        "CREATE TABLE t (x Int64);;",
    ];

    for sql in cases {
        let expected_position = if let Some(position) = sql.find("unexpected") {
            position
        } else if let Some(position) = sql.find("SELECT") {
            position
        } else {
            sql.len() - 1
        };
        assert_eq!(
            parse_create_table(sql),
            Err(ParseError::TrailingInput {
                position: expected_position,
            }),
            "SQL: {sql:?}"
        );
    }
}

#[test]
fn enforces_the_sql_byte_limit_at_its_exact_boundary() {
    let statement = "CREATE TABLE t (x Int64)";
    let at_limit = format!("{statement}{}", " ".repeat(MAX_SQL_BYTES - statement.len()));
    assert_eq!(at_limit.len(), MAX_SQL_BYTES);
    assert!(parse_create_table(&at_limit).is_ok());

    let over_limit = format!("{at_limit} ");
    assert_eq!(
        parse_create_table(&over_limit),
        Err(ParseError::SqlTooLarge {
            position: MAX_SQL_BYTES,
            max_bytes: MAX_SQL_BYTES,
            actual_bytes: MAX_SQL_BYTES + 1,
        })
    );
}

#[test]
fn enforces_the_column_limit_at_its_exact_boundary() {
    let at_limit = create_statement_with_columns(MAX_COLUMNS);
    let statement = parse_create_table(&at_limit).unwrap();
    assert_eq!(statement.columns.len(), MAX_COLUMNS);

    let over_limit = create_statement_with_columns(MAX_COLUMNS + 1);
    let extra_column_position = over_limit.find(&format!("c{MAX_COLUMNS} Int64")).unwrap();
    assert_eq!(
        parse_create_table(&over_limit),
        Err(ParseError::TooManyColumns {
            position: extra_column_position,
            max_columns: MAX_COLUMNS,
        })
    );
}

fn create_statement_with_columns(count: usize) -> String {
    let columns = (0..count)
        .map(|index| format!("c{index} Int64"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("CREATE TABLE wide ({columns})")
}
