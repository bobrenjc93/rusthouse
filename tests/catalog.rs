use rusthouse::{Catalog, CatalogError, ColumnSchema, DataType, Schema, Value, ValueRef};

fn schema() -> Schema {
    Schema::new(vec![ColumnSchema::new("value", DataType::Int64)]).unwrap()
}

#[test]
fn creates_and_looks_up_independent_bounded_tables() {
    let mut catalog = Catalog::new(2);

    catalog.create_table("first", schema(), 1).unwrap();
    catalog.create_table("second", schema(), 3).unwrap();

    catalog
        .table_mut("first")
        .unwrap()
        .insert_row(vec![Value::Int64(7)])
        .unwrap();
    catalog
        .table_mut("second")
        .unwrap()
        .insert_rows(vec![vec![Value::Int64(11)], vec![Value::Int64(13)]])
        .unwrap();

    assert_eq!(catalog.len(), 2);
    assert_eq!(catalog.table_limit(), 2);
    assert!(!catalog.is_empty());
    assert_eq!(catalog.table("first").unwrap().row_limit(), 1);
    assert_eq!(catalog.table("first").unwrap().row_count(), 1);
    assert_eq!(
        catalog.table("first").unwrap().value(0, 0),
        Some(ValueRef::Int64(7))
    );
    assert_eq!(catalog.table("second").unwrap().row_limit(), 3);
    assert_eq!(catalog.table("second").unwrap().row_count(), 2);
    assert_eq!(
        catalog.table("second").unwrap().value(1, 0),
        Some(ValueRef::Int64(13))
    );
    assert!(catalog.table("missing").is_none());
}

#[test]
fn rejects_an_empty_name_without_mutation() {
    let mut catalog = Catalog::new(2);
    catalog.create_table("existing", schema(), 1).unwrap();
    let before = catalog.clone();

    let error = catalog.create_table("", schema(), 4).unwrap_err();

    assert_eq!(error, CatalogError::EmptyName);
    assert_eq!(catalog, before);
}

#[test]
fn rejects_a_duplicate_name_without_mutation() {
    let mut catalog = Catalog::new(2);
    catalog.create_table("events", schema(), 1).unwrap();
    catalog
        .table_mut("events")
        .unwrap()
        .insert_row(vec![Value::Int64(7)])
        .unwrap();
    let before = catalog.clone();

    let error = catalog.create_table("events", schema(), 99).unwrap_err();

    assert_eq!(
        error,
        CatalogError::DuplicateTable {
            name: "events".to_owned(),
        }
    );
    assert_eq!(catalog, before);
}

#[test]
fn rejects_tables_beyond_capacity_without_mutation() {
    let mut catalog = Catalog::new(1);
    catalog.create_table("events", schema(), 1).unwrap();
    let before = catalog.clone();

    let error = catalog.create_table("metrics", schema(), 2).unwrap_err();

    assert_eq!(error, CatalogError::TableLimitExceeded { limit: 1 });
    assert_eq!(catalog, before);
    assert!(catalog.table("metrics").is_none());
}

#[test]
fn a_zero_capacity_catalog_rejects_its_first_table() {
    let mut catalog = Catalog::new(0);
    let before = catalog.clone();

    let error = catalog.create_table("events", schema(), 1).unwrap_err();

    assert_eq!(error, CatalogError::TableLimitExceeded { limit: 0 });
    assert_eq!(catalog, before);
    assert!(catalog.is_empty());
}
