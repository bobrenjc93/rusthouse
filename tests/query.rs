use rusthouse::{DataType, QueryError, Value, execute};

#[test]
fn executes_every_supported_literal_type() {
    let cases = [
        (
            "SELECT -9223372036854775808 AS minimum",
            "minimum",
            DataType::Int64,
            Value::Int64(i64::MIN),
        ),
        (
            "select +3.25e1 as measurement;",
            "measurement",
            DataType::Float64,
            Value::Float64(32.5),
        ),
        (
            "SELECT FaLsE AS enabled",
            "enabled",
            DataType::Bool,
            Value::Bool(false),
        ),
        (
            "SELECT 'it''s ready' AS message",
            "message",
            DataType::String,
            Value::String("it's ready".to_owned()),
        ),
    ];

    for (sql, name, data_type, value) in cases {
        let result = execute(sql).unwrap();
        assert_eq!(result.columns.len(), 1);
        assert_eq!(result.columns[0].name, name);
        assert_eq!(result.columns[0].data_type, data_type);
        assert_eq!(result.rows, vec![vec![value]]);
    }
}

#[test]
fn accepts_quoted_identifiers() {
    let result = execute("SELECT 7 AS \"daily total\"").unwrap();
    assert_eq!(result.columns[0].name, "daily total");
}

#[test]
fn rejects_malformed_or_out_of_scope_sql() {
    for sql in [
        "",
        "SELECT",
        "SELECT value AS alias",
        "SELECT 1 alias",
        "SELECT 'unterminated AS alias",
        "SELECT 1 AS alias FROM table",
        "SELECT 1 AS first; SELECT 2 AS second",
    ] {
        assert!(execute(sql).is_err(), "query unexpectedly succeeded: {sql}");
    }
}

#[test]
fn reports_numeric_range_errors_with_typed_variants() {
    assert!(matches!(
        execute("SELECT 9223372036854775808 AS too_large"),
        Err(QueryError::IntegerOutOfRange { .. })
    ));
    assert!(matches!(
        execute("SELECT 1e999 AS too_large"),
        Err(QueryError::NonFiniteFloat { .. })
    ));
}
