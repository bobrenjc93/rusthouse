use rusthouse::{
    Catalog, CatalogError, CatalogLimits, ColumnDefinition, ColumnType, CreateTable,
    IdentifierError, MAX_COLUMNS, MAX_INPUT_BYTES, QueryLimits, SchemaError, Value, execute_select,
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
fn created_table_supports_transactional_batch_insertion_and_bounded_scans() {
    let mut catalog = Catalog::new();
    catalog
        .create_table(parse_create_table("CREATE TABLE Events (id Int64, label String)").unwrap())
        .unwrap();

    catalog
        .table_mut("EVENTS")
        .unwrap()
        .insert_rows(&[
            vec![Value::Int64(1), Value::from("first")],
            vec![Value::Int64(2), Value::from("second")],
        ])
        .unwrap();

    let table = catalog.table("events").unwrap();
    let result = execute_select(
        "SELECT * FROM Events",
        catalog.table_name("events").unwrap(),
        table,
        QueryLimits::new(1),
    )
    .unwrap();

    assert_eq!(
        result.rows,
        vec![vec![Value::Int64(1), Value::from("first")]]
    );
    assert!(result.truncated);
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
        Err(CatalogError::InvalidColumnName {
            column: 0,
            reason: IdentifierError::Empty,
        })
    );
    assert!(catalog.is_empty());
}

#[test]
fn forged_asts_enforce_the_exact_column_count_boundary() {
    let columns = (0..MAX_COLUMNS)
        .map(|index| ColumnDefinition {
            name: format!("c{index}"),
            column_type: ColumnType::Int64,
        })
        .collect::<Vec<_>>();
    let mut catalog = Catalog::new();

    catalog
        .create_table(CreateTable {
            name: "at_limit".to_owned(),
            columns: columns.clone(),
        })
        .unwrap();
    assert_eq!(
        catalog.table("at_limit").unwrap().schema().len(),
        MAX_COLUMNS
    );

    let mut over_limit = columns;
    over_limit.push(ColumnDefinition {
        name: "extra".to_owned(),
        column_type: ColumnType::Int64,
    });
    let error = catalog
        .create_table(CreateTable {
            name: "over_limit".to_owned(),
            columns: over_limit,
        })
        .unwrap_err();
    assert_eq!(
        error,
        CatalogError::TooManyColumns {
            limit: MAX_COLUMNS,
            actual: MAX_COLUMNS + 1,
        }
    );
    assert!(catalog.table("over_limit").is_none());
}

#[test]
fn forged_asts_enforce_the_exact_definition_size_boundary() {
    let fixed_bytes =
        "CREATE TABLE ".len() + "t".len() + "(".len() + " ".len() + "Int64".len() + ")".len();
    let at_limit_name = "c".repeat(MAX_INPUT_BYTES - fixed_bytes);
    let mut catalog = Catalog::new();

    catalog
        .create_table(CreateTable {
            name: "t".to_owned(),
            columns: vec![ColumnDefinition {
                name: at_limit_name.clone(),
                column_type: ColumnType::Int64,
            }],
        })
        .unwrap();

    let error = catalog
        .create_table(CreateTable {
            name: "u".to_owned(),
            columns: vec![ColumnDefinition {
                name: format!("{at_limit_name}c"),
                column_type: ColumnType::Int64,
            }],
        })
        .unwrap_err();
    assert_eq!(
        error,
        CatalogError::DefinitionTooLong {
            limit: MAX_INPUT_BYTES,
            actual: MAX_INPUT_BYTES + 1,
        }
    );
    assert!(catalog.table("u").is_none());
}

#[test]
fn forged_asts_reject_invalid_identifiers() {
    let cases = [
        (
            "",
            CatalogError::InvalidTableName {
                reason: IdentifierError::Empty,
            },
        ),
        (
            "1table",
            CatalogError::InvalidTableName {
                reason: IdentifierError::InvalidStart { character: '1' },
            },
        ),
        (
            "bad-name",
            CatalogError::InvalidTableName {
                reason: IdentifierError::InvalidCharacter {
                    character: '-',
                    position: 3,
                },
            },
        ),
        (
            "café",
            CatalogError::InvalidTableName {
                reason: IdentifierError::InvalidCharacter {
                    character: 'é',
                    position: 3,
                },
            },
        ),
    ];

    for (name, expected) in cases {
        let mut catalog = Catalog::new();
        let error = catalog
            .create_table(CreateTable {
                name: name.to_owned(),
                columns: vec![ColumnDefinition {
                    name: "id".to_owned(),
                    column_type: ColumnType::Int64,
                }],
            })
            .unwrap_err();
        assert_eq!(error, expected);
        assert!(catalog.is_empty());
    }

    let mut catalog = Catalog::new();
    let error = catalog
        .create_table(CreateTable {
            name: "valid_table".to_owned(),
            columns: vec![ColumnDefinition {
                name: "bad column".to_owned(),
                column_type: ColumnType::Int64,
            }],
        })
        .unwrap_err();
    assert_eq!(
        error,
        CatalogError::InvalidColumnName {
            column: 0,
            reason: IdentifierError::InvalidCharacter {
                character: ' ',
                position: 3,
            },
        }
    );
    assert!(catalog.is_empty());
}

#[test]
fn forged_asts_reject_case_insensitive_duplicate_columns() {
    let mut catalog = Catalog::new();
    let error = catalog
        .create_table(CreateTable {
            name: "duplicates".to_owned(),
            columns: vec![
                ColumnDefinition {
                    name: "UserId".to_owned(),
                    column_type: ColumnType::Int64,
                },
                ColumnDefinition {
                    name: "userid".to_owned(),
                    column_type: ColumnType::String,
                },
            ],
        })
        .unwrap_err();

    assert_eq!(
        error,
        CatalogError::DuplicateColumn {
            name: "userid".to_owned(),
            first_column: 0,
            duplicate_column: 1,
        }
    );
    assert!(catalog.is_empty());
}

#[test]
fn lookups_reject_names_over_the_input_limit() {
    let mut catalog = Catalog::new();
    catalog
        .create_table(parse_create_table("CREATE TABLE events (id Int64)").unwrap())
        .unwrap();
    let over_limit = "a".repeat(MAX_INPUT_BYTES + 1);

    assert!(catalog.table_mut(&over_limit).is_none());
    assert!(catalog.table(&over_limit).is_none());
    assert_eq!(catalog.table_name(&over_limit), None);
    assert!(catalog.table("EVENTS").is_some());
    assert_eq!(catalog.table_name("EVENTS"), Some("events"));
}
