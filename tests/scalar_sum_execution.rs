use rusthouse::{
    AggregateError, AggregateLimits, Catalog, CatalogError, CatalogLimits, Int64Table, ParseLimits,
    Schema, SelectExecutionError, execute_scalar_sum, parse_scalar_sum,
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
            "SELECT SUM(value) FROM readings;",
            parse_limits,
            AggregateLimits::new(3, 3),
        ),
        Ok(Some(5))
    );
    assert_eq!(
        catalog.execute_scalar_sum(
            "SELECT SUM(value) FROM readings",
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
}
