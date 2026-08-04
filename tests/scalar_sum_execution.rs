use rusthouse::{
    AggregateError, AggregateLimits, Catalog, CatalogError, CatalogLimits, Int64Table, ParseLimits,
    ScanError, ScanLimits, Schema, SelectExecutionError, execute_scalar_sum,
    execute_scalar_sum_with_limits, parse_scalar_sum,
};

fn table(values: &[Option<i64>]) -> Int64Table {
    let mut table = Int64Table::new(Schema::int64("value", true), values.len());
    table.append_batch(values).unwrap();
    table
}

fn execute(input: &str, table: &Int64Table) -> Option<i64> {
    let statement = parse_scalar_sum(input, ParseLimits::default()).unwrap();
    execute_scalar_sum(
        "readings",
        table,
        &statement,
        AggregateLimits::new(table.values().len(), table.values().len()),
    )
    .unwrap()
}

#[test]
fn empty_and_all_null_inputs_return_sql_null() {
    assert_eq!(
        execute("SELECT SUM(value) FROM readings", &table(&[])),
        None
    );
    assert_eq!(
        execute(
            "SELECT SUM(value) FROM readings;",
            &table(&[None, None, None]),
        ),
        None
    );
}

#[test]
fn populated_input_sums_non_null_values() {
    assert_eq!(
        execute(
            "SELECT SUM(value) FROM readings",
            &table(&[Some(7), None, Some(-2), Some(0), None]),
        ),
        Some(5)
    );
}

#[test]
fn comparison_filters_support_every_operator_and_exclude_nulls() {
    let table = table(&[None, Some(-2), Some(0), Some(3), Some(7), None]);
    let cases = [
        ("=", Some(3)),
        ("!=", Some(5)),
        ("<>", Some(5)),
        ("<", Some(-2)),
        ("<=", Some(1)),
        (">", Some(7)),
        (">=", Some(10)),
    ];

    for (operator, expected) in cases {
        let input = format!("SELECT SUM(value) FROM readings WHERE value {operator} 3;");
        assert_eq!(execute(&input, &table), expected, "{input:?}");
    }
}

#[test]
fn empty_and_null_only_filtered_selections_return_sql_null() {
    let values = table(&[None, Some(1), None, Some(2)]);
    assert_eq!(
        execute("SELECT SUM(value) FROM readings WHERE value > 10", &values,),
        None
    );
    assert_eq!(
        execute(
            "SELECT SUM(value) FROM readings WHERE value < 0;",
            &table(&[None, None]),
        ),
        None
    );
}

#[test]
fn validates_table_and_column_identifiers_before_aggregation() {
    let table = table(&[Some(1), None]);
    let cases = [
        (
            "SELECT SUM(value) FROM Readings",
            SelectExecutionError::UnknownTable {
                name: "Readings".to_owned(),
            },
        ),
        (
            "SELECT SUM(other) FROM readings",
            SelectExecutionError::UnknownColumn {
                name: "other".to_owned(),
            },
        ),
        (
            "SELECT SUM(value) FROM readings WHERE other = 1",
            SelectExecutionError::UnknownColumn {
                name: "other".to_owned(),
            },
        ),
    ];

    for (input, expected) in cases {
        let statement = parse_scalar_sum(input, ParseLimits::default()).unwrap();
        assert_eq!(
            execute_scalar_sum("readings", &table, &statement, AggregateLimits::new(0, 0)),
            Err(expected),
            "{input:?}"
        );
    }
}

#[test]
fn filtered_sum_preserves_scan_and_aggregate_limits() {
    let table = table(&[Some(1), None, Some(2)]);
    let statement = parse_scalar_sum(
        "SELECT SUM(value) FROM readings WHERE value >= 1",
        ParseLimits::default(),
    )
    .unwrap();

    assert_eq!(
        execute_scalar_sum_with_limits(
            "readings",
            &table,
            &statement,
            ScanLimits::new(3, 2),
            AggregateLimits::new(3, 2),
        ),
        Ok(Some(3))
    );
    assert_eq!(
        execute_scalar_sum_with_limits(
            "readings",
            &table,
            &statement,
            ScanLimits::new(2, 2),
            AggregateLimits::new(3, 2),
        ),
        Err(SelectExecutionError::Scan(ScanError::InputLimitExceeded {
            rows: 3,
            max_rows: 2,
        }))
    );
    assert_eq!(
        execute_scalar_sum_with_limits(
            "readings",
            &table,
            &statement,
            ScanLimits::new(3, 1),
            AggregateLimits::new(3, 2),
        ),
        Err(SelectExecutionError::Scan(ScanError::ResultLimitExceeded {
            rows: 2,
            max_rows: 1,
        }))
    );
    assert_eq!(
        execute_scalar_sum_with_limits(
            "readings",
            &table,
            &statement,
            ScanLimits::new(3, 2),
            AggregateLimits::new(2, 2),
        ),
        Err(SelectExecutionError::Aggregate(
            AggregateError::InputLimitExceeded {
                rows: 3,
                max_rows: 2,
            }
        ))
    );
    assert_eq!(
        execute_scalar_sum_with_limits(
            "readings",
            &table,
            &statement,
            ScanLimits::new(3, 2),
            AggregateLimits::new(3, 1),
        ),
        Err(SelectExecutionError::Aggregate(
            AggregateError::SelectionLimitExceeded {
                rows: 2,
                max_rows: 1,
            }
        ))
    );
}

#[test]
fn unfiltered_sum_does_not_consume_scan_limits() {
    let table = table(&[Some(1), Some(2)]);
    let statement =
        parse_scalar_sum("SELECT SUM(value) FROM readings", ParseLimits::default()).unwrap();

    assert_eq!(
        execute_scalar_sum_with_limits(
            "readings",
            &table,
            &statement,
            ScanLimits::new(0, 0),
            AggregateLimits::new(2, 2),
        ),
        Ok(Some(3))
    );
}

#[test]
fn preserves_exact_and_exceeded_aggregate_limits() {
    let table = table(&[Some(1), None, Some(2)]);
    let statement =
        parse_scalar_sum("SELECT SUM(value) FROM readings", ParseLimits::default()).unwrap();

    assert_eq!(
        execute_scalar_sum("readings", &table, &statement, AggregateLimits::new(3, 3)),
        Ok(Some(3))
    );
    assert_eq!(
        execute_scalar_sum("readings", &table, &statement, AggregateLimits::new(2, 3)),
        Err(SelectExecutionError::Aggregate(
            AggregateError::InputLimitExceeded {
                rows: 3,
                max_rows: 2,
            }
        ))
    );
    assert_eq!(
        execute_scalar_sum("readings", &table, &statement, AggregateLimits::new(3, 2)),
        Err(SelectExecutionError::Aggregate(
            AggregateError::SelectionLimitExceeded {
                rows: 3,
                max_rows: 2,
            }
        ))
    );
}

#[test]
fn preserves_typed_positive_and_negative_overflow_errors() {
    for (values, expected_sum) in [
        ([Some(i64::MAX), Some(1)], i64::MAX as i128 + 1),
        ([Some(i64::MIN), Some(-1)], i64::MIN as i128 - 1),
    ] {
        let table = table(&values);
        let statement =
            parse_scalar_sum("SELECT SUM(value) FROM readings", ParseLimits::default()).unwrap();

        assert_eq!(
            execute_scalar_sum("readings", &table, &statement, AggregateLimits::new(2, 2),),
            Err(SelectExecutionError::Aggregate(
                AggregateError::SumOverflow { sum: expected_sum }
            ))
        );
    }
}

#[test]
fn overflow_is_computed_only_over_matching_rows() {
    let table = table(&[Some(i64::MAX), Some(1), Some(-1)]);
    assert_eq!(
        execute(
            "SELECT SUM(value) FROM readings WHERE value < 9223372036854775807",
            &table,
        ),
        Some(0)
    );

    let statement = parse_scalar_sum(
        "SELECT SUM(value) FROM readings WHERE value > 0",
        ParseLimits::default(),
    )
    .unwrap();
    assert_eq!(
        execute_scalar_sum("readings", &table, &statement, AggregateLimits::new(3, 2),),
        Err(SelectExecutionError::Aggregate(
            AggregateError::SumOverflow {
                sum: i64::MAX as i128 + 1,
            }
        ))
    );
}

#[test]
fn catalog_parses_and_executes_sum_with_supplied_limits() {
    let parse_limits = ParseLimits::default();
    let mut catalog = Catalog::new(CatalogLimits::new(1, 3));
    catalog
        .execute_create("CREATE TABLE readings (value Int64 NULL)", parse_limits)
        .unwrap();
    for input in [
        "INSERT INTO readings VALUES (7)",
        "INSERT INTO readings VALUES (NULL)",
        "INSERT INTO readings VALUES (-2)",
    ] {
        catalog.execute_insert(input, parse_limits).unwrap();
    }

    assert_eq!(
        catalog.execute_scalar_sum(
            "SELECT SUM(value) FROM readings WHERE value < 0;",
            parse_limits,
            AggregateLimits::new(3, 3),
        ),
        Ok(Some(-2))
    );
    assert_eq!(
        catalog.execute_scalar_sum_with_limits(
            "SELECT SUM(value) FROM readings WHERE value != 0",
            parse_limits,
            ScanLimits::new(3, 1),
            AggregateLimits::new(3, 3),
        ),
        Err(CatalogError::Select(SelectExecutionError::Scan(
            ScanError::ResultLimitExceeded {
                rows: 2,
                max_rows: 1,
            }
        )))
    );
}
