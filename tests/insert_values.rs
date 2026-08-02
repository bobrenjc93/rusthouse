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
fn inserts_multiple_mixed_type_rows_with_escaped_strings() {
    let mut catalog = catalog_with_all_types();

    let statement = parse_insert_values(
        r#"InSeRt InTo "Daily Metrics" VaLuEs
           (-7, +1.25e1, TRUE, 'customer''s note'),
           (0, -3.5, false, 'comma, inside'),
           (42, 0.0, TRUE, 'last')"#,
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
    assert_eq!(
        statement.rows(),
        [
            vec![
                Value::Int64(-7),
                Value::Float64(12.5),
                Value::Bool(true),
                Value::String("customer's note".to_owned()),
            ],
            vec![
                Value::Int64(0),
                Value::Float64(-3.5),
                Value::Bool(false),
                Value::String("comma, inside".to_owned()),
            ],
            vec![
                Value::Int64(42),
                Value::Float64(0.0),
                Value::Bool(true),
                Value::String("last".to_owned()),
            ],
        ]
    );

    execute_insert_values(
        &mut catalog,
        r#"INSERT INTO "Daily Metrics" VALUES
           (-7, +1.25e1, TRUE, 'customer''s note'),
           (0, -3.5, false, 'comma, inside'),
           (42, 0.0, TRUE, 'last');"#,
    )
    .expect("the rows match the table");

    let table = catalog.table("Daily Metrics").unwrap();
    assert_eq!(table.row_count(), 3);
    assert_eq!(table.columns()[0], Column::Int64(vec![-7, 0, 42]));
    assert_eq!(table.columns()[1], Column::Float64(vec![12.5, -3.5, 0.0]));
    assert_eq!(table.columns()[2], Column::Bool(vec![true, false, true]));
    assert_eq!(
        table.columns()[3],
        Column::String(vec![
            "customer's note".to_owned(),
            "comma, inside".to_owned(),
            "last".to_owned(),
        ])
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
        "INSERT INTO missing VALUES (1) (2)",
        "INSERT INTO missing VALUES (1),",
        "INSERT INTO missing VALUES (1),, (2)",
        "INSERT INTO missing VALUES (1), 2",
        "INSERT INTO missing VALUES (1), ()",
        "INSERT INTO missing VALUES (1), (2,)",
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
fn late_width_and_type_errors_report_tuple_index_and_are_atomic() {
    let mut catalog = catalog_with_all_types();
    let invalid = [
        (
            "INSERT INTO \"Daily Metrics\" VALUES \
             (1, 2.0, true, 'first'), \
             (2, 3.0, false, 'second'), \
             (3, 4.0, true)",
            InsertError::RowWidth {
                expected: 4,
                actual: 3,
            },
        ),
        (
            "INSERT INTO \"Daily Metrics\" VALUES \
             (1, 2.0, true, 'first'), \
             (2, 3.0, false, 'second'), \
             (3, 4, true, 'third')",
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
            Err(InsertValuesError::BatchInsert {
                tuple_index: 2,
                source: expected,
            })
        );
        assert_eq!(catalog, before, "{sql:?} changed storage");
    }
}

#[test]
fn single_row_errors_preserve_the_existing_insert_error_contract() {
    let mut catalog = catalog_with_all_types();
    let before = catalog.clone();
    let expected = InsertError::RowWidth {
        expected: 4,
        actual: 3,
    };

    assert_eq!(
        InsertValuesError::from(expected.clone()),
        InsertValuesError::Insert(expected.clone())
    );
    assert_eq!(
        execute_insert_values(
            &mut catalog,
            "INSERT INTO \"Daily Metrics\" VALUES (1, 2.0, true)",
        ),
        Err(InsertValuesError::Insert(expected))
    );
    assert_eq!(catalog, before);
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
