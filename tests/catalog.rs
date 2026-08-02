use rusthouse::csv::write_csv;
use rusthouse::sql::{CreateTableStatement, parse_create_table, parse_insert};
use rusthouse::{Catalog, CatalogError, DataType, TableError, TableLimits, Value};

fn limits(max_columns: usize, max_rows: usize, max_string_bytes: usize) -> TableLimits {
    TableLimits {
        max_columns,
        max_rows,
        max_string_bytes,
    }
}

#[test]
fn creates_and_inserts_by_case_insensitive_table_name() {
    let statement = parse_create_table(
        "CREATE TABLE Events (event_id Int64, score Float64, active Bool, label String)",
    )
    .unwrap();
    let mut catalog = Catalog::new(limits(4, 2, 9));

    catalog.create_table(statement).unwrap();
    catalog
        .insert_batch(
            "eVeNtS",
            vec![
                vec![
                    Value::Int64(10),
                    Value::Float64(1.5),
                    Value::Bool(true),
                    Value::String("first".into()),
                ],
                vec![
                    Value::Int64(-2),
                    Value::Float64(-0.25),
                    Value::Bool(false),
                    Value::String("last".into()),
                ],
            ],
        )
        .unwrap();

    let table = catalog.table("EVENTS").unwrap();
    let ordered_schema = table
        .schema()
        .columns()
        .iter()
        .map(|column| (column.name(), column.data_type()))
        .collect::<Vec<_>>();
    assert_eq!(
        ordered_schema,
        vec![
            ("event_id", DataType::Int64),
            ("score", DataType::Float64),
            ("active", DataType::Bool),
            ("label", DataType::String),
        ]
    );
    assert_eq!(
        table.column("event_id").unwrap().as_int64(),
        Some(&[10, -2][..])
    );
    assert_eq!(
        table.column("score").unwrap().as_float64(),
        Some(&[1.5, -0.25][..])
    );
    assert_eq!(
        table.column("active").unwrap().as_bool(),
        Some(&[true, false][..])
    );
    assert_eq!(
        table.column("label").unwrap().as_string(),
        Some(&["first".to_owned(), "last".to_owned()][..])
    );
    assert_eq!(table.limits(), limits(4, 2, 9));
}

#[test]
fn rejects_duplicate_and_invalid_creates_without_changing_the_catalog() {
    let mut catalog = Catalog::new(limits(1, 1, 0));
    catalog
        .create_table(parse_create_table("CREATE TABLE Metrics (value Int64)").unwrap())
        .unwrap();
    let snapshot = catalog.clone();

    let duplicate = parse_create_table("CREATE TABLE mEtRiCs (other Bool)").unwrap();
    assert_eq!(
        catalog.create_table(duplicate),
        Err(CatalogError::DuplicateTable {
            name: "mEtRiCs".into()
        })
    );
    assert_eq!(catalog, snapshot);

    let too_wide = parse_create_table("CREATE TABLE wide (left Int64, right Bool)").unwrap();
    assert_eq!(
        catalog.create_table(too_wide),
        Err(CatalogError::Table(TableError::ColumnLimitExceeded {
            limit: 1,
            attempted: 2,
        }))
    );
    assert_eq!(catalog, snapshot);
    assert_eq!(catalog.len(), 1);
}

#[test]
fn reports_missing_tables_without_mutating_any_existing_table() {
    let mut catalog = Catalog::default();
    catalog
        .create_table(parse_create_table("CREATE TABLE present (value Int64)").unwrap())
        .unwrap();
    let snapshot = catalog.clone();

    assert_eq!(
        catalog.table("absent"),
        Err(CatalogError::TableNotFound {
            name: "absent".into()
        })
    );
    assert_eq!(
        catalog.insert_batch("Missing", vec![vec![Value::Int64(1)]]),
        Err(CatalogError::TableNotFound {
            name: "Missing".into()
        })
    );
    assert_eq!(catalog, snapshot);
}

#[test]
fn rejected_named_batches_are_atomic_at_resource_and_type_limits() {
    let mut catalog = Catalog::new(limits(2, 3, 4));
    catalog
        .create_table(parse_create_table("CREATE TABLE bounded (id Int64, text String)").unwrap())
        .unwrap();
    catalog
        .insert_batch(
            "bounded",
            vec![vec![Value::Int64(1), Value::String("ok".into())]],
        )
        .unwrap();

    let snapshot = catalog.clone();
    assert_eq!(
        catalog.insert_batch(
            "BOUNDED",
            vec![
                vec![Value::Int64(2), Value::String(String::new())],
                vec![Value::Int64(3), Value::String(String::new())],
                vec![Value::Int64(4), Value::String(String::new())],
            ],
        ),
        Err(CatalogError::Table(TableError::RowLimitExceeded {
            limit: 3,
            attempted: 4,
        }))
    );
    assert_eq!(catalog, snapshot);

    assert_eq!(
        catalog.insert_batch(
            "bounded",
            vec![vec![Value::Int64(2), Value::String("more".into()),]],
        ),
        Err(CatalogError::Table(TableError::StringByteLimitExceeded {
            limit: 4,
            attempted: 6,
        }))
    );
    assert_eq!(catalog, snapshot);

    assert_eq!(
        catalog.insert_batch(
            "bounded",
            vec![
                vec![Value::Int64(2), Value::String("x".into())],
                vec![Value::Bool(false), Value::String("y".into())],
            ],
        ),
        Err(CatalogError::Table(TableError::TypeMismatch {
            row: 1,
            column: 0,
            column_name: "id".into(),
            expected: DataType::Int64,
            actual: DataType::Bool,
        }))
    );
    assert_eq!(catalog, snapshot);
}

#[test]
fn duplicate_columns_from_a_constructed_statement_do_not_create_a_table() {
    let mut catalog = Catalog::default();
    let mut statement = parse_create_table("CREATE TABLE invalid (value Int64)").unwrap();
    statement.columns.push(statement.columns[0].clone());
    let expected = CatalogError::Table(TableError::DuplicateColumnName {
        name: "value".into(),
    });

    assert_eq!(catalog.create_table(statement), Err(expected));
    assert!(catalog.is_empty());
    assert_eq!(
        catalog.table("invalid"),
        Err(CatalogError::TableNotFound {
            name: "invalid".into()
        })
    );
}

#[test]
fn catalog_accepts_the_parsers_typed_statement_result() {
    let statement: CreateTableStatement =
        parse_create_table("CREATE TABLE direct (flag Bool)").unwrap();
    let mut catalog = Catalog::default();

    catalog.create_table(statement).unwrap();

    assert_eq!(
        catalog.table("direct").unwrap().schema().columns()[0].data_type(),
        DataType::Bool
    );
}

#[test]
fn parsed_statements_flow_through_the_catalog_to_csv() {
    let mut catalog = Catalog::default();
    catalog
        .create_table(parse_create_table("CREATE TABLE Events (id Int64, label String)").unwrap())
        .unwrap();

    catalog
        .insert(parse_insert("INSERT INTO eVeNtS VALUES (1, 'first'), (2, 'with,comma')").unwrap())
        .unwrap();

    let mut output = Vec::new();
    write_csv(catalog.table("EVENTS").unwrap(), &mut output).unwrap();
    assert_eq!(
        String::from_utf8(output).unwrap(),
        "id,label\r\n1,first\r\n2,\"with,comma\"\r\n"
    );
}
