use rusthouse::{
    Catalog, Column, DataType, InsertError, InsertValuesError, TableNotFoundError, Value,
    execute_create_table, execute_insert_values, parse_insert_values,
};

fn catalog_with_all_types() -> Catalog {
    let mut catalog = Catalog::new();
    execute_create_table(
        &mut catalog,
        r#"CREATE TABLE "Daily Metrics" (
            id Int64,
            score Float64,
            active Bool,
            note String
        )"#,
    )
    .expect("test table is valid");
    catalog
}

#[test]
fn inserts_one_escaped_all_type_row_with_an_optional_semicolon() {
    let mut catalog = catalog_with_all_types();

    let statement = parse_insert_values(
        r#"InSeRt InTo "Daily Metrics" VaLuEs (-7, +1.25e1, TRUE, 'customer''s note')"#,
    )
    .expect("the supported shape parses");
    assert_eq!(statement.table_name(), "Daily Metrics");
    assert_eq!(
        statement.values(),
        [
            Value::Int64(-7),
            Value::Float64(12.5),
            Value::Bool(true),
            Value::String("customer's note".to_owned()),
        ]
    );

    execute_insert_values(
        &mut catalog,
        r#"INSERT INTO "Daily Metrics" VALUES (-7, +1.25e1, TRUE, 'customer''s note');"#,
    )
    .expect("the row matches the table");

    let table = catalog.table("Daily Metrics").unwrap();
    assert_eq!(table.row_count(), 1);
    assert_eq!(table.columns()[0], Column::Int64(vec![-7]));
    assert_eq!(table.columns()[1], Column::Float64(vec![12.5]));
    assert_eq!(table.columns()[2], Column::Bool(vec![true]));
    assert_eq!(
        table.columns()[3],
        Column::String(vec!["customer's note".to_owned()])
    );
}

#[test]
fn malformed_statements_do_not_mutate_storage() {
    let malformed = [
        "",
        "INSERT",
        "INSERT INTO",
        "INSERT INTO missing",
        "INSERT INTO missing VALUES",
        "INSERT INTO missing VALUES ()",
        "INSERT INTO missing VALUES (1,)",
        "INSERT INTO missing VALUES (1) extra",
        "INSERT INTO missing VALUES (1), (2)",
        "INSERT INTO missing VALUES (1);;",
        "INSERT INTO missing VALUES (1); INSERT INTO missing VALUES (2)",
        "INSERT INTO missing SELECT 1",
    ];

    let mut catalog = catalog_with_all_types();
    for sql in malformed {
        let before = catalog.clone();
        let error = execute_insert_values(&mut catalog, sql).expect_err(sql);
        assert!(
            matches!(
                error,
                InsertValuesError::Syntax { .. } | InsertValuesError::MultipleStatements { .. }
            ),
            "unexpected error for {sql:?}: {error:?}"
        );
        assert_eq!(catalog, before, "{sql:?} changed storage");
    }
}

#[test]
fn missing_table_error_preserves_existing_storage() {
    let mut catalog = catalog_with_all_types();
    let before = catalog.clone();

    assert_eq!(
        execute_insert_values(&mut catalog, "INSERT INTO missing VALUES (1)"),
        Err(InsertValuesError::TableNotFound(TableNotFoundError {
            name: "missing".to_owned(),
        }))
    );
    assert_eq!(catalog, before);
}

#[test]
fn width_and_type_errors_are_atomic_and_typed() {
    let mut catalog = catalog_with_all_types();
    let invalid = [
        (
            "INSERT INTO \"Daily Metrics\" VALUES (1, 2.0, true)",
            InsertError::RowWidth {
                expected: 4,
                actual: 3,
            },
        ),
        (
            "INSERT INTO \"Daily Metrics\" VALUES (1, 2, true, 'note')",
            InsertError::TypeMismatch {
                column_index: 1,
                column_name: "score".to_owned(),
                expected: DataType::Float64,
                actual: DataType::Int64,
            },
        ),
    ];

    for (sql, expected) in invalid {
        let before = catalog.clone();
        assert_eq!(
            execute_insert_values(&mut catalog, sql),
            Err(InsertValuesError::Insert(expected))
        );
        assert_eq!(catalog, before, "{sql:?} changed storage");
    }
}

#[test]
fn invalid_typed_literals_do_not_reach_storage() {
    let mut catalog = catalog_with_all_types();
    let before = catalog.clone();

    assert!(matches!(
        execute_insert_values(
            &mut catalog,
            "INSERT INTO \"Daily Metrics\" VALUES (9223372036854775808, 2.0, true, 'note')"
        ),
        Err(InsertValuesError::InvalidInt64 { .. })
    ));
    assert!(matches!(
        execute_insert_values(
            &mut catalog,
            "INSERT INTO \"Daily Metrics\" VALUES (1, 1e999, true, 'note')"
        ),
        Err(InsertValuesError::InvalidFloat64 { .. })
    ));
    assert!(matches!(
        execute_insert_values(
            &mut catalog,
            "INSERT INTO \"Daily Metrics\" VALUES (1, 2.0, true, NULL)"
        ),
        Err(InsertValuesError::UnsupportedNull { .. })
    ));
    assert_eq!(catalog, before);
}
