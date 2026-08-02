use rusthouse::{DataType, QueryError, Value, execute, execute_batch};

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
fn executes_batches_in_statement_order() {
    let results =
        execute_batch("SELECT 1 AS first; SELECT 'two' AS second; SELECT FALSE AS third;").unwrap();

    assert_eq!(results.len(), 3);
    assert_eq!(results[0].columns[0].name, "first");
    assert_eq!(results[0].rows, vec![vec![Value::Int64(1)]]);
    assert_eq!(results[1].columns[0].name, "second");
    assert_eq!(results[1].rows, vec![vec![Value::String("two".to_owned())]]);
    assert_eq!(results[2].columns[0].name, "third");
    assert_eq!(results[2].rows, vec![vec![Value::Bool(false)]]);
}

#[test]
fn batch_separators_ignore_quoted_semicolons_and_empty_statements() {
    let results = execute_batch(
        r#";; SELECT 'one;still one' AS "value;name";;;
         SELECT 2 AS second;;"#,
    )
    .unwrap();

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].columns[0].name, "value;name");
    assert_eq!(
        results[0].rows,
        vec![vec![Value::String("one;still one".to_owned())]]
    );
    assert_eq!(results[1].rows, vec![vec![Value::Int64(2)]]);
}

#[test]
fn rejects_empty_and_malformed_batches() {
    for sql in [
        "; ; \n;;",
        "SELECT 1 AS first SELECT 2 AS second",
        "SELECT 1 AS first; SELECT AS broken",
        "SELECT 1 AS first; SELECT 'unterminated AS broken; SELECT 3 AS third",
        "SELECT 1 AS first; SELECT 2 AS \"unterminated; SELECT 3 AS third",
    ] {
        assert!(
            execute_batch(sql).is_err(),
            "batch unexpectedly succeeded: {sql}"
        );
    }
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
