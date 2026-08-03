use rusthouse::{
    Catalog, CatalogError, DataType, Field, QueryError, QueryPlan, RowSelection, Table, Value,
    parse_select,
};

#[test]
fn plans_and_executes_multi_key_grouping() {
    let mut catalog = Catalog::new();
    catalog
        .execute_create("CREATE TABLE events (category String, active Bool, value Int64)")
        .unwrap();
    catalog
        .execute_insert(
            "INSERT INTO events VALUES \
             ('a', true, 2), ('a', false, 4), ('a', true, 3), ('b', true, 8)",
        )
        .unwrap();
    let statement = parse_select(
        "SELECT category, active, count(*) AS rows, sum(value) AS total \
         FROM events GROUP BY category, active ORDER BY total DESC",
    )
    .unwrap();
    let source = catalog.table("events").unwrap();
    let plan = QueryPlan::build(source, &statement).unwrap();

    assert_eq!(
        plan.fields()
            .iter()
            .map(|field| (field.name(), field.data_type()))
            .collect::<Vec<_>>(),
        [
            ("category", DataType::String),
            ("active", DataType::Bool),
            ("rows", DataType::Int64),
            ("total", DataType::Int64),
        ]
    );
    let result = plan.execute(source, None).unwrap();
    assert_eq!(result.string_column("category").unwrap(), ["b", "a", "a"]);
    assert_eq!(
        result.bool_column("active").unwrap().collect::<Vec<_>>(),
        [true, true, false]
    );
    assert_eq!(result.int64_column("rows").unwrap(), [1, 2, 1]);
    assert_eq!(result.int64_column("total").unwrap(), [8, 5, 4]);
}

#[test]
fn rejects_plans_executed_with_a_different_schema_or_selection_length() {
    let source = Table::new(vec![Field::new("id", DataType::Int64)]).unwrap();
    let statement = parse_select("SELECT id AS event_id FROM events ORDER BY event_id").unwrap();
    let plan = QueryPlan::build(&source, &statement).unwrap();
    let other = Table::new(vec![Field::new("id", DataType::String)]).unwrap();

    assert_eq!(
        plan.execute(&other, None).unwrap_err(),
        QueryError::SourceSchemaMismatch
    );
    let selection = RowSelection::try_empty(1).unwrap();
    assert_eq!(
        plan.execute(&source, Some(&selection)).unwrap_err(),
        QueryError::SelectionLengthMismatch {
            table_rows: 0,
            selection_rows: 1,
        }
    );
}

#[test]
fn reports_checked_integer_aggregate_overflow() {
    let mut catalog = Catalog::new();
    catalog
        .execute_create("CREATE TABLE values_table (value Int64)")
        .unwrap();
    catalog
        .execute_insert(&format!(
            "INSERT INTO values_table VALUES ({}), (1)",
            i64::MAX
        ))
        .unwrap();

    assert!(matches!(
        catalog
            .execute_select("SELECT sum(value) FROM values_table")
            .unwrap_err(),
        CatalogError::Query {
            source: QueryError::Int64Overflow { ref field, .. },
            ..
        } if field == "value"
    ));
}

#[test]
fn grouped_float_keys_normalize_zero_and_nan_equivalence_classes() {
    let mut table = Table::new(vec![Field::new("value", DataType::Float64)]).unwrap();
    table
        .insert_batch([
            vec![Value::Float64(-0.0)],
            vec![Value::Float64(0.0)],
            vec![Value::Float64(f64::NAN)],
            vec![Value::Float64(f64::from_bits(f64::NAN.to_bits() + 1))],
        ])
        .unwrap();
    let statement = parse_select("SELECT value, count(*) AS rows FROM t GROUP BY value").unwrap();
    let result = QueryPlan::build(&table, &statement)
        .unwrap()
        .execute(&table, None)
        .unwrap();

    assert_eq!(result.len(), 2);
    assert_eq!(result.int64_column("rows").unwrap(), [2, 2]);
}
