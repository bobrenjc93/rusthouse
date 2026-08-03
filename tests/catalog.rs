use std::error::Error;

use rusthouse::{
    Catalog, CatalogError, CatalogLimits, ColumnDefinition, CreateTableStatement, DataType,
    ParseErrorKind, ParseLimits, TableError,
};

#[test]
fn creates_and_looks_up_multiple_named_tables() {
    let mut catalog = Catalog::new();

    catalog
        .execute_create("CREATE TABLE Events (EventID Int64, Active Bool)")
        .unwrap();
    catalog
        .execute_create("CREATE TABLE metrics (value Float64, label String)")
        .unwrap();

    assert_eq!(catalog.len(), 2);
    assert!(!catalog.is_empty());
    assert_eq!(
        catalog.table("events").unwrap().fields()[0].name(),
        "EventID"
    );
    assert_eq!(
        catalog.table("EVENTS").unwrap().fields()[1].data_type(),
        DataType::Bool
    );
    assert_eq!(
        catalog.table("metrics").unwrap().fields()[0].data_type(),
        DataType::Float64
    );

    let mut names: Vec<_> = catalog.table_names().collect();
    names.sort_unstable();
    assert_eq!(names, ["Events", "metrics"]);
}

#[test]
fn preserves_schema_names_order_and_types() {
    let mut catalog = Catalog::new();
    let created = catalog
        .execute_create(
            "CREATE TABLE Readings (Sequence Int64, Value Float64, Enabled Bool, Note String)",
        )
        .unwrap();

    let schema: Vec<_> = created
        .fields()
        .iter()
        .map(|field| (field.name(), field.data_type()))
        .collect();
    assert_eq!(
        schema,
        [
            ("Sequence", DataType::Int64),
            ("Value", DataType::Float64),
            ("Enabled", DataType::Bool),
            ("Note", DataType::String),
        ]
    );
}

#[test]
fn duplicate_names_are_typed_case_insensitive_and_non_mutating() {
    let mut catalog = Catalog::new();
    catalog
        .execute_create("CREATE TABLE Events (id Int64)")
        .unwrap();

    let error = catalog
        .execute_create("CREATE TABLE eVeNtS (replacement String)")
        .unwrap_err();

    assert_eq!(
        error,
        CatalogError::DuplicateTable {
            name: "eVeNtS".to_owned(),
        }
    );
    assert_eq!(catalog.len(), 1);
    let table = catalog.table("events").unwrap();
    assert_eq!(table.fields().len(), 1);
    assert_eq!(table.fields()[0].name(), "id");
    assert_eq!(table.fields()[0].data_type(), DataType::Int64);
}

#[test]
fn missing_lookups_return_typed_errors_without_mutation() {
    let mut catalog = Catalog::new();

    assert_eq!(
        catalog.table("missing").unwrap_err(),
        CatalogError::TableNotFound {
            name: "missing".to_owned(),
        }
    );
    assert_eq!(
        catalog.table_mut("also_missing").unwrap_err(),
        CatalogError::TableNotFound {
            name: "also_missing".to_owned(),
        }
    );
    assert!(catalog.is_empty());
}

#[test]
fn parse_failures_are_typed_chained_and_non_mutating() {
    let mut catalog = Catalog::new();
    catalog
        .execute_create("CREATE TABLE retained (id Int64)")
        .unwrap();

    let error = catalog
        .execute_create("CREATE TABLE rejected (value Decimal)")
        .unwrap_err();

    assert!(matches!(
        error,
        CatalogError::Parse(ref parse_error)
            if parse_error.kind
                == ParseErrorKind::UnknownType {
                    type_name: "Decimal".to_owned()
                }
    ));
    assert!(error.source().is_some());
    assert_eq!(catalog.len(), 1);
    assert!(catalog.table("retained").is_ok());
    assert!(matches!(
        catalog.table("rejected"),
        Err(CatalogError::TableNotFound { .. })
    ));
}

#[test]
fn table_construction_failures_are_typed_chained_and_non_mutating() {
    let mut catalog = Catalog::new();
    let invalid = CreateTableStatement {
        name: "invalid".to_owned(),
        columns: Vec::new(),
    };

    let error = catalog.create_table(invalid).unwrap_err();

    assert_eq!(
        error,
        CatalogError::TableConstruction {
            name: "invalid".to_owned(),
            source: TableError::EmptySchema,
        }
    );
    assert_eq!(
        error.source().unwrap().to_string(),
        TableError::EmptySchema.to_string()
    );
    assert!(catalog.is_empty());

    let duplicate_fields = CreateTableStatement {
        name: "still_invalid".to_owned(),
        columns: vec![
            ColumnDefinition {
                name: "id".to_owned(),
                data_type: DataType::Int64,
            },
            ColumnDefinition {
                name: "id".to_owned(),
                data_type: DataType::String,
            },
        ],
    };
    assert!(matches!(
        catalog.create_table(duplicate_fields),
        Err(CatalogError::TableConstruction {
            source: TableError::DuplicateField { .. },
            ..
        })
    ));
    assert!(catalog.is_empty());
}

#[test]
fn enforces_parse_table_count_and_row_limits() {
    let sql = "CREATE TABLE bounded (id Int64, enabled Bool)";
    let limits = CatalogLimits::new(ParseLimits::new(sql.len(), 2), 1, 7);
    let mut catalog = Catalog::with_limits(limits);

    let table = catalog.execute_create(sql).unwrap();
    assert_eq!(table.row_limit(), 7);
    assert_eq!(catalog.limits(), limits);

    assert_eq!(
        catalog
            .execute_create("CREATE TABLE second (id Int64)")
            .unwrap_err(),
        CatalogError::TableLimitExceeded { limit: 1 }
    );
    assert_eq!(catalog.len(), 1);

    let mut column_limited =
        Catalog::with_limits(CatalogLimits::new(ParseLimits::new(usize::MAX, 1), 2, 0));
    let error = column_limited.execute_create(sql).unwrap_err();
    assert!(matches!(
        error,
        CatalogError::Parse(parse_error)
            if parse_error.kind == ParseErrorKind::TooManyColumns { limit: 1 }
    ));
    assert!(column_limited.is_empty());

    let mut input_limited =
        Catalog::with_limits(CatalogLimits::new(ParseLimits::new(sql.len() - 1, 2), 2, 0));
    let error = input_limited.execute_create(sql).unwrap_err();
    assert!(matches!(
        error,
        CatalogError::Parse(parse_error)
            if parse_error.kind
                == ParseErrorKind::InputTooLong {
                    limit: sql.len() - 1,
                    actual: sql.len(),
                }
    ));
    assert!(input_limited.is_empty());
}

#[test]
fn zero_table_limit_rejects_create_without_mutation() {
    let mut catalog = Catalog::with_limits(CatalogLimits::new(ParseLimits::default(), 0, 0));

    assert_eq!(
        catalog
            .execute_create("CREATE TABLE blocked (id Int64)")
            .unwrap_err(),
        CatalogError::TableLimitExceeded { limit: 0 }
    );
    assert!(catalog.is_empty());
}
