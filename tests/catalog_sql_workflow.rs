use rusthouse::{
    Catalog, ColumnSchema, DataType, Value, execute_create_table, execute_table_select,
};

#[test]
fn creates_populates_and_projects_a_table_through_the_shared_catalog() {
    let mut catalog = Catalog::new();
    execute_create_table(
        &mut catalog,
        r#"CREATE TABLE "Daily Metrics" (
            "Event ID" Int64,
            score Float64,
            active Bool,
            label String
        )"#,
    )
    .expect("DDL creates the catalog table");

    catalog
        .table_mut("Daily Metrics")
        .expect("the created table is mutable through the catalog")
        .insert_batch(vec![
            vec![1_i64.into(), 9.5_f64.into(), true.into(), "first".into()],
            vec![
                2_i64.into(),
                (-3.25_f64).into(),
                false.into(),
                "second".into(),
            ],
        ])
        .expect("rows match the SQL-defined schema");

    let result = execute_table_select(
        &catalog,
        r#"SELECT label, "Event ID", active, score FROM "Daily Metrics";"#,
    )
    .expect("projection resolves the SQL-created table and columns");

    assert_eq!(
        result.headers(),
        [
            ColumnSchema::new("label", DataType::String),
            ColumnSchema::new("Event ID", DataType::Int64),
            ColumnSchema::new("active", DataType::Bool),
            ColumnSchema::new("score", DataType::Float64),
        ]
    );
    assert_eq!(
        result.rows(),
        [
            vec![
                Value::String("first".to_owned()),
                Value::Int64(1),
                Value::Bool(true),
                Value::Float64(9.5),
            ],
            vec![
                Value::String("second".to_owned()),
                Value::Int64(2),
                Value::Bool(false),
                Value::Float64(-3.25),
            ],
        ]
    );
}
