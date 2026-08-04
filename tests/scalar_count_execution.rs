use rusthouse::{
    AggregateError, AggregateLimits, Catalog, CatalogError, CatalogLimits, Int64Table, ParseLimits,
    ScanError, ScanLimits, Schema, SelectExecutionError, execute_scalar_count,
    execute_scalar_count_with_limits, parse_scalar_count,
};

fn table(values: &[Option<i64>]) -> Int64Table {
    let mut table = Int64Table::new(Schema::int64("value", true), values.len());
    table.append_batch(values).unwrap();
    table
}

fn execute(input: &str, table: &Int64Table) -> u64 {
    let statement = parse_scalar_count(input, ParseLimits::default()).unwrap();
    execute_scalar_count(
        "readings",
        table,
        &statement,
        AggregateLimits::new(table.values().len(), table.values().len()),
    )
    .unwrap()
}

#[test]
fn empty_input_returns_zero_for_both_count_forms() {
    let table = table(&[]);

    assert_eq!(execute("SELECT COUNT(*) FROM readings", &table), 0);
    assert_eq!(execute("SELECT COUNT(value) FROM readings;", &table), 0);
}

#[test]
fn all_null_input_counts_rows_only_for_count_star() {
    let table = table(&[None, None, None]);

    assert_eq!(execute("SELECT COUNT(*) FROM readings", &table), 3);
    assert_eq!(execute("SELECT COUNT(value) FROM readings", &table), 0);
}

#[test]
fn populated_input_distinguishes_rows_from_non_null_values() {
    let table = table(&[Some(7), None, Some(-2), Some(0), None]);

    assert_eq!(execute("SELECT COUNT(*) FROM readings", &table), 5);
    assert_eq!(execute("SELECT COUNT(value) FROM readings", &table), 3);
}

#[test]
fn filters_both_count_forms_with_every_comparison_operator_and_sql_null_semantics() {
    let table = table(&[
        None,
        Some(i64::MIN),
        Some(-2),
        Some(0),
        Some(2),
        Some(i64::MAX),
        None,
    ]);
    let cases = [
        ("=", 1),
        ("!=", 4),
        ("<>", 4),
        ("<", 2),
        ("<=", 3),
        (">", 2),
        (">=", 3),
    ];

    for argument in ["*", "value"] {
        for (operator, expected) in cases {
            let input = format!("SELECT COUNT({argument}) FROM readings WHERE value {operator} 0;");
            assert_eq!(execute(&input, &table), expected, "{input:?}");
        }
    }
}

#[test]
fn filtered_counts_return_zero_when_no_rows_match() {
    let table = table(&[None, Some(i64::MIN), Some(i64::MAX), None]);

    for argument in ["*", "value"] {
        assert_eq!(
            execute(
                &format!(
                    "SELECT COUNT({argument}) FROM readings WHERE value > {}",
                    i64::MAX
                ),
                &table,
            ),
            0
        );
    }
}

#[test]
fn counts_values_even_when_their_sum_would_overflow() {
    for values in [[Some(i64::MAX), Some(1)], [Some(i64::MIN), Some(-1)]] {
        let table = table(&values);

        assert_eq!(execute("SELECT COUNT(*) FROM readings", &table), 2);
        assert_eq!(execute("SELECT COUNT(value) FROM readings", &table), 2);
    }
}

#[test]
fn validates_table_and_column_identifiers_before_aggregation() {
    let table = table(&[Some(1), None]);
    let cases = [
        (
            "SELECT COUNT(*) FROM Readings",
            SelectExecutionError::UnknownTable {
                name: "Readings".to_owned(),
            },
        ),
        (
            "SELECT COUNT(other) FROM readings",
            SelectExecutionError::UnknownColumn {
                name: "other".to_owned(),
            },
        ),
        (
            "SELECT COUNT(*) FROM readings WHERE other = 1",
            SelectExecutionError::UnknownColumn {
                name: "other".to_owned(),
            },
        ),
    ];

    for (input, expected) in cases {
        let statement = parse_scalar_count(input, ParseLimits::default()).unwrap();
        assert_eq!(
            execute_scalar_count("readings", &table, &statement, AggregateLimits::new(0, 0),),
            Err(expected),
            "{input:?}"
        );
    }
}

#[test]
fn filtered_counts_preserve_scan_and_aggregate_limits() {
    let table = table(&[Some(1), None, Some(1), Some(2), Some(3)]);

    for argument in ["*", "value"] {
        let statement = parse_scalar_count(
            &format!("SELECT COUNT({argument}) FROM readings WHERE value >= 1"),
            ParseLimits::default(),
        )
        .unwrap();

        assert_eq!(
            execute_scalar_count_with_limits(
                "readings",
                &table,
                &statement,
                ScanLimits::new(5, 4),
                AggregateLimits::new(5, 4),
            ),
            Ok(4),
            "exact bounds for COUNT({argument})"
        );
        assert_eq!(
            execute_scalar_count_with_limits(
                "readings",
                &table,
                &statement,
                ScanLimits::new(4, 4),
                AggregateLimits::new(5, 4),
            ),
            Err(SelectExecutionError::Scan(ScanError::InputLimitExceeded {
                rows: 5,
                max_rows: 4,
            })),
            "scan input bound for COUNT({argument})"
        );
        assert_eq!(
            execute_scalar_count_with_limits(
                "readings",
                &table,
                &statement,
                ScanLimits::new(5, 3),
                AggregateLimits::new(5, 4),
            ),
            Err(SelectExecutionError::Scan(ScanError::ResultLimitExceeded {
                rows: 4,
                max_rows: 3,
            })),
            "scan result bound for COUNT({argument})"
        );
        assert_eq!(
            execute_scalar_count_with_limits(
                "readings",
                &table,
                &statement,
                ScanLimits::new(5, 4),
                AggregateLimits::new(4, 4),
            ),
            Err(SelectExecutionError::Aggregate(
                AggregateError::InputLimitExceeded {
                    rows: 5,
                    max_rows: 4,
                }
            )),
            "aggregate input bound for COUNT({argument})"
        );
        assert_eq!(
            execute_scalar_count_with_limits(
                "readings",
                &table,
                &statement,
                ScanLimits::new(5, 4),
                AggregateLimits::new(5, 3),
            ),
            Err(SelectExecutionError::Aggregate(
                AggregateError::SelectionLimitExceeded {
                    rows: 4,
                    max_rows: 3,
                }
            )),
            "aggregate selection bound for COUNT({argument})"
        );
    }
}

#[test]
fn preserves_exact_and_exceeded_aggregate_limits() {
    let table = table(&[Some(1), None, Some(2)]);
    let statement =
        parse_scalar_count("SELECT COUNT(value) FROM readings", ParseLimits::default()).unwrap();

    assert_eq!(
        execute_scalar_count("readings", &table, &statement, AggregateLimits::new(3, 3),),
        Ok(2)
    );
    assert_eq!(
        execute_scalar_count("readings", &table, &statement, AggregateLimits::new(2, 3),),
        Err(SelectExecutionError::Aggregate(
            AggregateError::InputLimitExceeded {
                rows: 3,
                max_rows: 2,
            }
        ))
    );
    assert_eq!(
        execute_scalar_count("readings", &table, &statement, AggregateLimits::new(3, 2),),
        Err(SelectExecutionError::Aggregate(
            AggregateError::SelectionLimitExceeded {
                rows: 3,
                max_rows: 2,
            }
        ))
    );
}

#[test]
fn catalog_parses_and_executes_both_count_forms_with_supplied_limits() {
    let parse_limits = ParseLimits::default();
    let mut catalog = Catalog::new(CatalogLimits::new(1, 4));
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
        catalog.execute_scalar_count(
            "SELECT COUNT(*) FROM readings;",
            parse_limits,
            AggregateLimits::new(3, 3),
        ),
        Ok(3)
    );
    assert_eq!(
        catalog.execute_scalar_count(
            "SELECT COUNT(value) FROM readings",
            parse_limits,
            AggregateLimits::new(3, 3),
        ),
        Ok(2)
    );
    assert_eq!(
        catalog.execute_scalar_count(
            "SELECT COUNT(*) FROM readings",
            parse_limits,
            AggregateLimits::new(3, 2),
        ),
        Err(CatalogError::Select(SelectExecutionError::Aggregate(
            AggregateError::SelectionLimitExceeded {
                rows: 3,
                max_rows: 2,
            }
        )))
    );
    assert_eq!(
        catalog.execute_scalar_count_with_limits(
            "SELECT COUNT(*) FROM readings WHERE value >= 0",
            parse_limits,
            ScanLimits::new(3, 1),
            AggregateLimits::new(3, 1),
        ),
        Ok(1)
    );
}
