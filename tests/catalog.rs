use rusthouse::{
    Catalog, CatalogError, CatalogLimits, ColumnDefinition, ColumnType, CreateTable, SchemaError,
    parse_create_table,
};

#[test]
fn creates_an_empty_non_nullable_table_from_parsed_sql() {
    let statement = parse_create_table(
        "CREATE TABLE Events (id Int64, score Float64, active Bool, label String)",
    )
    .unwrap();
    let mut catalog = Catalog::new();

    catalog.create_table(statement).unwrap();

    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog.table_name("events"), Some("Events"));
    let table = catalog.table("EVENTS").unwrap();
    assert!(table.is_empty());
    assert_eq!(table.schema().len(), 4);
    assert!(
        table
            .schema()
            .columns()
            .iter()
            .all(|column| !column.is_nullable())
    );
    assert_eq!(
        table.schema().column(0).unwrap().data_type(),
        ColumnType::Int64
    );
    assert_eq!(
        table.schema().column(3).unwrap().data_type(),
        ColumnType::String
    );
}

#[test]
fn rejects_duplicate_table_names_case_insensitively_without_replacing_the_table() {
    let mut catalog = Catalog::new();
    catalog
        .create_table(parse_create_table("CREATE TABLE Metrics (id Int64)").unwrap())
        .unwrap();

    let error = catalog
        .create_table(parse_create_table("CREATE TABLE metrics (label String)").unwrap())
        .unwrap_err();

    assert_eq!(
        error,
        CatalogError::DuplicateTable {
            name: "metrics".to_owned()
        }
    );
    assert_eq!(catalog.len(), 1);
    let table = catalog.table("MeTrIcS").unwrap();
    assert_eq!(table.schema().column(0).unwrap().name(), "id");
}

#[test]
fn accepts_the_exact_table_limit_and_rejects_the_next_table() {
    let limits = CatalogLimits::new(2);
    let mut catalog = Catalog::with_limits(limits);

    catalog
        .create_table(parse_create_table("CREATE TABLE first (id Int64)").unwrap())
        .unwrap();
    catalog
        .create_table(parse_create_table("CREATE TABLE second (id Int64)").unwrap())
        .unwrap();
    assert_eq!(catalog.len(), limits.max_tables);

    let error = catalog
        .create_table(parse_create_table("CREATE TABLE third (id Int64)").unwrap())
        .unwrap_err();
    assert_eq!(error, CatalogError::TableLimitExceeded { limit: 2 });
    assert_eq!(catalog.len(), 2);
    assert!(catalog.table("third").is_none());
}

#[test]
fn a_zero_table_limit_rejects_the_first_table() {
    let mut catalog = Catalog::with_limits(CatalogLimits::new(0));

    let error = catalog
        .create_table(parse_create_table("CREATE TABLE blocked (id Int64)").unwrap())
        .unwrap_err();

    assert_eq!(error, CatalogError::TableLimitExceeded { limit: 0 });
    assert!(catalog.is_empty());
}

#[test]
fn reports_schema_errors_for_manually_constructed_invalid_asts() {
    let mut catalog = Catalog::new();
    let invalid = CreateTable {
        name: "invalid".to_owned(),
        columns: Vec::new(),
    };

    assert_eq!(
        catalog.create_table(invalid),
        Err(CatalogError::InvalidSchema(SchemaError::Empty))
    );
    assert!(catalog.is_empty());

    let invalid = CreateTable {
        name: "invalid".to_owned(),
        columns: vec![ColumnDefinition {
            name: String::new(),
            column_type: ColumnType::Int64,
        }],
    };
    assert_eq!(
        catalog.create_table(invalid),
        Err(CatalogError::InvalidSchema(SchemaError::EmptyColumnName {
            column: 0
        }))
    );
    assert!(catalog.is_empty());
}
