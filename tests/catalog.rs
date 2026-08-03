use std::error::Error;

use rusthouse::{
    Catalog, CatalogError, CatalogLimits, DataType, InsertParseLimits, ParseErrorKind, ParseLimits,
    TableError, parse_select,
};

#[test]
fn creates_and_looks_up_named_tables_case_insensitively() {
    let mut catalog = Catalog::new();

    catalog
        .execute_create("CREATE TABLE Events (EventID Int64, Active Bool)")
        .unwrap();
    catalog
        .execute_create("CREATE TABLE metrics (value Float64, label String)")
        .unwrap();

    assert_eq!(catalog.len(), 2);
    assert_eq!(
        catalog.table("events").unwrap().fields()[0].name(),
        "EventID"
    );
    assert_eq!(
        catalog.table("EVENTS").unwrap().fields()[1].data_type(),
        DataType::Bool
    );

    let mut names: Vec<_> = catalog.table_names().collect();
    names.sort_unstable();
    assert_eq!(names, ["Events", "metrics"]);
}

#[test]
fn executes_case_insensitive_multi_row_insert_with_every_type() {
    let mut catalog = Catalog::new();
    catalog
        .execute_create(
            "CREATE TABLE Readings (sequence Int64, value Float64, active Bool, label String)",
        )
        .unwrap();

    let inserted = catalog
        .execute_insert(
            "INSERT INTO rEaDiNgS VALUES (1, -2.5, true, 'first'), (+3, .5e1, FALSE, '')",
        )
        .unwrap();

    assert_eq!(inserted, 2);
    let table = catalog.table("READINGS").unwrap();
    assert_eq!(table.int64_column("sequence").unwrap(), [1, 3]);
    assert_eq!(table.float64_column("value").unwrap(), [-2.5, 5.0]);
    assert_eq!(
        table.bool_column("active").unwrap().collect::<Vec<_>>(),
        [true, false]
    );
    assert_eq!(table.string_column("label").unwrap(), ["first", ""]);
}

#[test]
fn missing_insert_target_is_typed_and_does_not_mutate_other_tables() {
    let mut catalog = Catalog::new();
    catalog
        .execute_create("CREATE TABLE retained (id Int64)")
        .unwrap();
    catalog
        .execute_insert("INSERT INTO retained VALUES (7)")
        .unwrap();

    let error = catalog
        .execute_insert("INSERT INTO Missing VALUES (8)")
        .unwrap_err();

    assert_eq!(
        error,
        CatalogError::TableNotFound {
            name: "Missing".to_owned(),
        }
    );
    assert_eq!(catalog.table("retained").unwrap().len(), 1);
    assert_eq!(
        catalog
            .table("retained")
            .unwrap()
            .int64_column("id")
            .unwrap(),
        [7]
    );
}

#[test]
fn schema_mismatches_are_typed_and_roll_back_complete_batches() {
    let mut catalog = Catalog::new();
    catalog
        .execute_create("CREATE TABLE events (id Int64, active Bool)")
        .unwrap();
    catalog
        .execute_insert("INSERT INTO events VALUES (1, true)")
        .unwrap();

    let width_error = catalog
        .execute_insert("INSERT INTO EVENTS VALUES (2, false), (3)")
        .unwrap_err();
    assert_eq!(
        width_error,
        CatalogError::TableInsertion {
            name: "EVENTS".to_owned(),
            source: TableError::RowWidthMismatch {
                row: 1,
                expected: 2,
                actual: 1,
            },
        }
    );
    assert_original_event(&catalog);

    let type_error = catalog
        .execute_insert("INSERT INTO events VALUES (2, false), (3, 'wrong')")
        .unwrap_err();
    assert_eq!(
        type_error,
        CatalogError::TableInsertion {
            name: "events".to_owned(),
            source: TableError::TypeMismatch {
                row: 1,
                column: 1,
                field: "active".to_owned(),
                expected: DataType::Bool,
                actual: DataType::String,
            },
        }
    );
    assert_eq!(
        type_error.source().unwrap().to_string(),
        "batch row 1, column 1 (`active`) has type String; expected Bool"
    );
    assert_original_event(&catalog);
}

#[test]
fn row_limit_failure_is_typed_and_rolls_back_the_complete_batch() {
    let limits = CatalogLimits::new(ParseLimits::default(), 1, 2);
    let mut catalog = Catalog::with_limits(limits);
    catalog
        .execute_create("CREATE TABLE bounded (id Int64)")
        .unwrap();
    catalog
        .execute_insert("INSERT INTO bounded VALUES (1)")
        .unwrap();

    let error = catalog
        .execute_insert("INSERT INTO BOUNDED VALUES (2), (3)")
        .unwrap_err();

    assert_eq!(
        error,
        CatalogError::TableInsertion {
            name: "BOUNDED".to_owned(),
            source: TableError::RowLimitExceeded {
                limit: 2,
                current: 1,
            },
        }
    );
    let table = catalog.table("bounded").unwrap();
    assert_eq!(table.len(), 1);
    assert_eq!(table.int64_column("id").unwrap(), [1]);
}

#[test]
fn execute_insert_uses_the_catalogs_bounded_parser() {
    let insert = "INSERT INTO bounded VALUES (1), (2)";
    let insert_limits = InsertParseLimits::new(insert.len(), 1, 1, 0);
    let limits =
        CatalogLimits::new(ParseLimits::default(), 1, 10).with_insert_parse_limits(insert_limits);
    let mut catalog = Catalog::with_limits(limits);
    catalog
        .execute_create("CREATE TABLE bounded (id Int64)")
        .unwrap();

    let error = catalog.execute_insert(insert).unwrap_err();

    assert!(matches!(
        error,
        CatalogError::Parse(ref parse_error)
            if parse_error.kind == ParseErrorKind::TooManyRows { limit: 1 }
    ));
    assert!(error.source().is_some());
    assert!(catalog.table("bounded").unwrap().is_empty());
    assert_eq!(catalog.limits().insert_parse, insert_limits);
}

#[test]
fn parsed_select_predicate_scans_rows_inserted_through_the_catalog() {
    let mut catalog = Catalog::new();
    catalog
        .execute_create("CREATE TABLE Events (id Int64, active Bool)")
        .unwrap();
    catalog
        .execute_insert("INSERT INTO events VALUES (1, false), (2, true), (3, true)")
        .unwrap();
    let statement = parse_select("SELECT id FROM EVENTS WHERE id >= 2").unwrap();
    let predicate = &statement.predicate_groups[0][0];

    let selection = catalog
        .table(&statement.table)
        .unwrap()
        .scan(&predicate.column, predicate.operator, &predicate.value)
        .unwrap();

    assert_eq!(selection.selected_rows().collect::<Vec<_>>(), [1, 2]);
}

fn assert_original_event(catalog: &Catalog) {
    let table = catalog.table("events").unwrap();
    assert_eq!(table.len(), 1);
    assert_eq!(table.int64_column("id").unwrap(), [1]);
    assert_eq!(
        table.bool_column("active").unwrap().collect::<Vec<_>>(),
        [true]
    );
}
