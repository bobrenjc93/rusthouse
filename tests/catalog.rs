use rusthouse::{
    Catalog, CatalogError, ParseLimits, materialize_create_table, parse_create_table, parse_insert,
};

fn table_entry(name: &str, row_cap: usize) -> rusthouse::TableEntry {
    let statement = parse_create_table(
        &format!("CREATE TABLE {name} (value Int64)"),
        ParseLimits::default(),
    )
    .unwrap();
    materialize_create_table(statement, row_cap)
}

#[test]
fn new_and_default_catalogs_are_empty() {
    let catalog = Catalog::new();
    let default_catalog = Catalog::default();

    assert!(catalog.is_empty());
    assert_eq!(catalog.len(), 0);
    assert!(catalog.get("anything").is_none());
    assert!(default_catalog.is_empty());
}

#[test]
fn registers_and_looks_up_one_entry_by_exact_name() {
    let mut catalog = Catalog::new();

    catalog.register(table_entry("Metrics", 8)).unwrap();

    assert!(!catalog.is_empty());
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog.get("Metrics").unwrap().table().row_cap(), 8);
    assert!(catalog.get("metrics").is_none());
    assert!(catalog.get("Missing").is_none());
}

#[test]
fn mutable_lookup_updates_the_registered_table() {
    let mut catalog = Catalog::new();
    catalog.register(table_entry("events", 2)).unwrap();

    catalog
        .get_mut("events")
        .unwrap()
        .table_mut()
        .append(Some(41))
        .unwrap();

    assert_eq!(catalog.get("events").unwrap().table().values(), &[Some(41)]);
}

#[test]
fn parsed_create_catalog_and_insert_statement_compose() {
    let create = parse_create_table(
        "CREATE TABLE Events (value Int64 NOT NULL)",
        ParseLimits::default(),
    )
    .unwrap();
    let mut catalog = Catalog::new();
    catalog
        .register(materialize_create_table(create, 1))
        .unwrap();

    let insert = parse_insert("INSERT INTO Events VALUES (-7)", ParseLimits::default()).unwrap();
    catalog
        .get_mut(insert.table_name().as_str())
        .unwrap()
        .table_mut()
        .append(insert.value())
        .unwrap();

    assert_eq!(catalog.get("Events").unwrap().table().values(), &[Some(-7)]);
}

#[test]
fn occupied_catalog_rejects_registration_without_replacing_existing_entry() {
    let mut catalog = Catalog::new();
    catalog.register(table_entry("retained", 2)).unwrap();
    catalog
        .get_mut("retained")
        .unwrap()
        .table_mut()
        .append(Some(7))
        .unwrap();

    let error = catalog.register(table_entry("rejected", 9)).unwrap_err();

    assert_eq!(error, CatalogError::Occupied);
    assert_eq!(error.to_string(), "catalog already contains a table");
    assert_eq!(catalog.len(), 1);
    assert_eq!(
        catalog.get("retained").unwrap().table().values(),
        &[Some(7)]
    );
    assert!(catalog.get("rejected").is_none());
}
