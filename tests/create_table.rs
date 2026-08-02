use rusthouse::{
    Catalog, CatalogError, CreateTableError, DataType, SchemaError, execute_create_table,
    parse_create_table,
};

#[test]
fn creates_all_benchmark_schema_profiles() {
    let profiles = [
        (
            "CREATE TABLE narrow (id Int64, value Float64)",
            "narrow",
            vec![DataType::Int64, DataType::Float64],
        ),
        (
            "create table dimensions (id int64, region string, active bool);",
            "dimensions",
            vec![DataType::Int64, DataType::String, DataType::Bool],
        ),
        (
            "CREATE TABLE mixed (event_id Int64, score Float64, enabled Bool, label String);",
            "mixed",
            vec![
                DataType::Int64,
                DataType::Float64,
                DataType::Bool,
                DataType::String,
            ],
        ),
    ];

    let mut catalog = Catalog::new();
    for (sql, table_name, expected_types) in profiles {
        execute_create_table(&mut catalog, sql).expect("benchmark schema is accepted");
        let table = catalog.table(table_name).expect("table was registered");
        assert_eq!(
            table
                .schema()
                .columns()
                .iter()
                .map(|column| column.data_type())
                .collect::<Vec<_>>(),
            expected_types
        );
        assert_eq!(table.row_count(), 0);
        assert!(table.columns().iter().all(|column| column.is_empty()));
    }
    assert_eq!(catalog.len(), 3);
}

#[test]
fn preserves_column_order_and_identifier_spelling() {
    let statement = parse_create_table(
        r#"CrEaTe TaBlE "Daily Metrics" ("Event ID" iNt64, Ratio FLOAT64, Ready BOOL, Note STRING);"#,
    )
    .expect("supported keywords and types are case-insensitive");

    assert_eq!(statement.table_name(), "Daily Metrics");
    let columns = statement.schema().columns();
    assert_eq!(columns[0].name(), "Event ID");
    assert_eq!(columns[1].name(), "Ratio");
    assert_eq!(columns[2].name(), "Ready");
    assert_eq!(columns[3].name(), "Note");
}

#[test]
fn malformed_definitions_report_positions_and_leave_catalog_unchanged() {
    let mut catalog = Catalog::new();
    execute_create_table(&mut catalog, "CREATE TABLE existing (id Int64)").unwrap();

    let malformed = [
        "",
        "CREATE",
        "CREATE TABLE",
        "CREATE TABLE bad",
        "CREATE TABLE bad ()",
        "CREATE TABLE bad (id)",
        "CREATE TABLE bad (id Int64,)",
        "CREATE TABLE bad (id Int64) extra",
        "CREATE TABLE bad (id Int64);;",
        "CREATE TABLE bad (id Int64); CREATE TABLE other (id Int64)",
    ];

    for sql in malformed {
        let before = catalog.clone();
        let error = execute_create_table(&mut catalog, sql).expect_err(sql);
        assert!(
            matches!(
                error,
                CreateTableError::Syntax { .. } | CreateTableError::MultipleStatements { .. }
            ),
            "unexpected error for {sql:?}: {error:?}"
        );
        assert_eq!(catalog, before, "{sql:?} changed the catalog");
    }

    assert_eq!(
        execute_create_table(&mut Catalog::new(), "CREATE TABLE bad id Int64)"),
        Err(CreateTableError::Syntax {
            position: 17,
            expected: "`(`",
        })
    );
}

#[test]
fn unknown_type_is_typed_and_positional_without_catalog_mutation() {
    let mut catalog = Catalog::new();
    let before = catalog.clone();

    assert_eq!(
        execute_create_table(&mut catalog, "CREATE TABLE bad (id UInt64)"),
        Err(CreateTableError::UnknownType {
            name: "UInt64".to_owned(),
            position: 21,
        })
    );
    assert_eq!(catalog, before);
}

#[test]
fn duplicate_columns_use_schema_validation_without_catalog_mutation() {
    let mut catalog = Catalog::new();
    execute_create_table(&mut catalog, "CREATE TABLE existing (kept String)").unwrap();
    let before = catalog.clone();

    assert_eq!(
        execute_create_table(
            &mut catalog,
            "CREATE TABLE duplicate_columns (id Int64, id String)",
        ),
        Err(CreateTableError::Schema(SchemaError::DuplicateColumn {
            name: "id".to_owned(),
        }))
    );
    assert_eq!(catalog, before);
    assert!(!catalog.contains_table("duplicate_columns"));
}

#[test]
fn duplicate_tables_return_catalog_error_and_preserve_original_table() {
    let mut catalog = Catalog::new();
    execute_create_table(&mut catalog, "CREATE TABLE events (id Int64)").unwrap();
    let before = catalog.clone();

    assert_eq!(
        execute_create_table(&mut catalog, "CREATE TABLE events (replacement String)"),
        Err(CreateTableError::Catalog(CatalogError::DuplicateTable {
            name: "events".to_owned(),
        }))
    );
    assert_eq!(catalog, before);
    assert_eq!(
        catalog.table("events").unwrap().schema().columns()[0].name(),
        "id"
    );
}

#[test]
fn empty_quoted_names_flow_through_existing_validation_boundaries() {
    let mut catalog = Catalog::new();

    assert_eq!(
        execute_create_table(&mut catalog, r#"CREATE TABLE valid ("" Int64)"#),
        Err(CreateTableError::Schema(SchemaError::EmptyColumnName {
            index: 0,
        }))
    );
    assert_eq!(
        execute_create_table(&mut catalog, r#"CREATE TABLE "" (id Int64)"#),
        Err(CreateTableError::Catalog(CatalogError::EmptyTableName))
    );
    assert!(catalog.is_empty());
}
