use rusthouse::{
    Catalog, CatalogError, Column, ColumnSchema, DataType, Schema, TableNotFoundError,
};

fn schema(column_name: &str, data_type: DataType) -> Schema {
    Schema::new(vec![ColumnSchema::new(column_name, data_type)]).expect("test schema is valid")
}

#[test]
fn rejected_creation_preserves_the_original_table() {
    let mut catalog = Catalog::new();

    assert_eq!(
        catalog.create_table("", schema("ignored", DataType::Bool)),
        Err(CatalogError::EmptyTableName)
    );
    assert!(catalog.is_empty());

    catalog
        .create_table("events", schema("id", DataType::Int64))
        .expect("table name is available");
    catalog
        .table_mut("events")
        .expect("table exists")
        .insert_row(vec![7_i64.into()])
        .expect("row is valid");
    let original = catalog.table("events").expect("table exists").clone();

    assert_eq!(
        catalog.create_table("events", schema("replacement", DataType::String)),
        Err(CatalogError::DuplicateTable {
            name: "events".to_owned(),
        })
    );
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog.table("events"), Ok(&original));
}

#[test]
fn lookup_is_exact_and_reports_the_requested_name() {
    let mut catalog = Catalog::new();
    catalog
        .create_table("Events", schema("id", DataType::Int64))
        .expect("table name is available");

    assert_eq!(
        catalog.table("events"),
        Err(TableNotFoundError {
            name: "events".to_owned(),
        })
    );
    assert_eq!(
        catalog.table_mut("missing"),
        Err(TableNotFoundError {
            name: "missing".to_owned(),
        })
    );
    assert_eq!(
        catalog
            .table("Events")
            .expect("exact name exists")
            .row_count(),
        0
    );
}

#[test]
fn mutations_are_isolated_by_table_name() {
    let mut catalog = Catalog::new();
    catalog
        .create_table("first", schema("value", DataType::Int64))
        .expect("table name is available");
    catalog
        .create_table("second", schema("value", DataType::Int64))
        .expect("table name is available");

    catalog
        .table_mut("first")
        .expect("first table exists")
        .insert_row(vec![11_i64.into()])
        .expect("row is valid");

    assert_eq!(
        catalog
            .table("first")
            .expect("first table exists")
            .row_count(),
        1
    );
    assert_eq!(
        catalog
            .table("first")
            .expect("first table exists")
            .column(0),
        Some(&Column::Int64(vec![11]))
    );
    assert_eq!(
        catalog
            .table("second")
            .expect("second table exists")
            .row_count(),
        0
    );
    assert_eq!(
        catalog
            .table("second")
            .expect("second table exists")
            .column(0),
        Some(&Column::Int64(Vec::new()))
    );
}
