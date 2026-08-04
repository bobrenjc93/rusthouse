use rusthouse::{
    AggregateError, AggregateLimits, Catalog, CatalogError, CatalogLimits, Int64Table, ParseLimits,
    Schema, SelectExecutionError, execute_scalar_min, parse_scalar_min,
};

fn table(values: &[Option<i64>]) -> Int64Table {
    let mut table = Int64Table::new(Schema::int64("value", true), values.len());
    table.append_batch(values).unwrap();
    table
}

fn execute(input: &str, table: &Int64Table) -> Option<i64> {
    let statement = parse_scalar_min(input, ParseLimits::default()).unwrap();
    execute_scalar_min(
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
        execute("SELECT MIN(value) FROM readings", &table(&[])),
        None
    );
    assert_eq!(
        execute(
            "SELECT MIN(value) FROM readings;",
            &table(&[None, None, None]),
        ),
        None
    );
}

#[test]
fn duplicates_nulls_and_integer_extremes_return_the_minimum() {
    let values = table(&[
        Some(i64::MAX),
        None,
        Some(0),
        Some(i64::MIN),
        Some(i64::MIN),
        None,
    ]);

    assert_eq!(
        execute("SELECT MIN(value) FROM readings", &values),
        Some(i64::MIN)
    );
}

#[test]
fn validates_exact_table_and_column_identifiers_before_aggregation() {
    let table = table(&[Some(1), None]);
    let cases = [
        (
            "SELECT MIN(value) FROM Readings",
            SelectExecutionError::UnknownTable {
                name: "Readings".to_owned(),
            },
        ),
        (
            "SELECT MIN(Value) FROM readings",
            SelectExecutionError::UnknownColumn {
                name: "Value".to_owned(),
            },
        ),
        (
            "SELECT MIN(other) FROM readings",
            SelectExecutionError::UnknownColumn {
                name: "other".to_owned(),
            },
        ),
    ];

    for (input, expected) in cases {
        let statement = parse_scalar_min(input, ParseLimits::default()).unwrap();
        assert_eq!(
            execute_scalar_min("readings", &table, &statement, AggregateLimits::new(0, 0)),
            Err(expected),
            "{input:?}"
        );
    }
}

#[test]
fn preserves_exact_and_exceeded_aggregate_limits() {
    let table = table(&[Some(2), None, Some(1)]);
    let statement =
        parse_scalar_min("SELECT MIN(value) FROM readings", ParseLimits::default()).unwrap();

    assert_eq!(
        execute_scalar_min("readings", &table, &statement, AggregateLimits::new(3, 3)),
        Ok(Some(1))
    );
    assert_eq!(
        execute_scalar_min("readings", &table, &statement, AggregateLimits::new(2, 3)),
        Err(SelectExecutionError::Aggregate(
            AggregateError::InputLimitExceeded {
                rows: 3,
                max_rows: 2,
            }
        ))
    );
    assert_eq!(
        execute_scalar_min("readings", &table, &statement, AggregateLimits::new(3, 2)),
        Err(SelectExecutionError::Aggregate(
            AggregateError::SelectionLimitExceeded {
                rows: 3,
                max_rows: 2,
            }
        ))
    );
}

#[test]
fn catalog_parses_and_executes_minimum_with_supplied_limits() {
    let parse_limits = ParseLimits::default();
    let mut catalog = Catalog::new(CatalogLimits::new(1, 5));
    catalog
        .execute_create("CREATE TABLE readings (value Int64 NULL)", parse_limits)
        .unwrap();
    for input in [
        "INSERT INTO readings VALUES (7)",
        "INSERT INTO readings VALUES (NULL)",
        "INSERT INTO readings VALUES (-2)",
        "INSERT INTO readings VALUES (-2)",
        "INSERT INTO readings VALUES (9223372036854775807)",
    ] {
        catalog.execute_insert(input, parse_limits).unwrap();
    }

    assert_eq!(
        catalog.execute_scalar_min(
            "SELECT MIN(value) FROM readings;",
            parse_limits,
            AggregateLimits::new(5, 5),
        ),
        Ok(Some(-2))
    );
    assert_eq!(
        catalog.execute_scalar_min(
            "SELECT MIN(value) FROM Readings",
            parse_limits,
            AggregateLimits::new(5, 5),
        ),
        Err(CatalogError::Select(SelectExecutionError::UnknownTable {
            name: "Readings".to_owned(),
        }))
    );
}
