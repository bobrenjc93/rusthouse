use rusthouse::{
    Catalog, CatalogError, CatalogLimits, InsertError, InsertExecutionError, ParseError,
    ParseLimits, ScanError, ScanLimits, SelectExecutionError,
};

fn catalog(max_tables: usize, max_rows_per_table: usize) -> Catalog {
    Catalog::new(CatalogLimits::new(max_tables, max_rows_per_table))
}

#[test]
fn executes_a_bounded_multi_table_sql_lifecycle() {
    let limits = ParseLimits::default();
    let mut catalog = catalog(2, 3);
    catalog
        .execute_create("CREATE TABLE readings (value Int64 NULL)", limits)
        .unwrap();
    catalog
        .execute_create("CREATE TABLE counts (value Int64 NOT NULL)", limits)
        .unwrap();

    for input in [
        "INSERT INTO readings VALUES (7)",
        "INSERT INTO readings VALUES (NULL)",
        "INSERT INTO readings VALUES (9)",
        "INSERT INTO counts VALUES (2)",
    ] {
        catalog.execute_insert(input, limits).unwrap();
    }

    let selected = catalog
        .execute_select("SELECT value FROM readings LIMIT 2", limits)
        .unwrap();

    assert_eq!(catalog.len(), 2);
    assert_eq!(selected.as_ref(), &[Some(7), None]);
    assert!(std::ptr::eq(
        selected.as_ptr(),
        catalog.table("readings").unwrap().values().as_ptr()
    ));
    assert_eq!(
        catalog
            .execute_select("SELECT value FROM counts", limits)
            .unwrap()
            .as_ref(),
        &[Some(2)]
    );
}

#[test]
fn executes_where_equality_end_to_end_with_null_semantics() {
    let limits = ParseLimits::default();
    let mut catalog = catalog(1, 5);
    catalog
        .execute_create("CREATE TABLE readings (value Int64 NULL)", limits)
        .unwrap();
    for input in [
        "INSERT INTO readings VALUES (7)",
        "INSERT INTO readings VALUES (NULL)",
        "INSERT INTO readings VALUES (2)",
        "INSERT INTO readings VALUES (7)",
    ] {
        catalog.execute_insert(input, limits).unwrap();
    }

    let selected = catalog
        .execute_select("SELECT value FROM readings WHERE value = 7;", limits)
        .unwrap();

    assert_eq!(selected.as_ref(), &[Some(7), Some(7)]);
    assert!(matches!(selected, std::borrow::Cow::Owned(_)));
}

#[test]
fn catalog_where_execution_exposes_scan_bounds() {
    let parse_limits = ParseLimits::default();
    let mut catalog = catalog(1, 3);
    catalog
        .execute_create("CREATE TABLE events (value Int64)", parse_limits)
        .unwrap();
    for input in [
        "INSERT INTO events VALUES (1)",
        "INSERT INTO events VALUES (NULL)",
        "INSERT INTO events VALUES (1)",
    ] {
        catalog.execute_insert(input, parse_limits).unwrap();
    }

    assert_eq!(
        catalog.execute_select_with_limits(
            "SELECT value FROM events WHERE value = 1",
            parse_limits,
            ScanLimits::new(2, 3),
        ),
        Err(CatalogError::Select(SelectExecutionError::Scan(
            ScanError::InputLimitExceeded {
                rows: 3,
                max_rows: 2,
            }
        )))
    );
    assert_eq!(
        catalog.execute_select_with_limits(
            "SELECT value FROM events WHERE value = 1",
            parse_limits,
            ScanLimits::new(3, 1),
        ),
        Err(CatalogError::Select(SelectExecutionError::Scan(
            ScanError::ResultLimitExceeded {
                rows: 2,
                max_rows: 1,
            }
        )))
    );
}

#[test]
fn create_enforces_exact_names_and_the_table_bound_atomically() {
    let limits = ParseLimits::default();
    let mut catalog = catalog(1, 2);
    catalog
        .execute_create("CREATE TABLE Events (value Int64)", limits)
        .unwrap();

    assert_eq!(
        catalog.execute_create("CREATE TABLE Events (other Int64)", limits),
        Err(CatalogError::TableAlreadyExists {
            name: "Events".to_owned(),
        })
    );
    assert_eq!(
        catalog.execute_create("CREATE TABLE events (value Int64)", limits),
        Err(CatalogError::TableLimitExceeded {
            tables: 2,
            max_tables: 1,
        })
    );

    assert_eq!(catalog.len(), 1);
    assert_eq!(
        catalog.table("Events").unwrap().schema().column().name(),
        "value"
    );
    assert!(catalog.table("events").is_none());
}

#[test]
fn insert_preserves_lookup_storage_and_nullability_errors() {
    let limits = ParseLimits::default();
    let mut catalog = catalog(1, 1);
    catalog
        .execute_create("CREATE TABLE events (value Int64 NOT NULL)", limits)
        .unwrap();

    assert_eq!(
        catalog.execute_insert("INSERT INTO missing VALUES (1)", limits),
        Err(CatalogError::Insert(InsertExecutionError::UnknownTable {
            name: "missing".to_owned(),
        }))
    );
    assert_eq!(
        catalog.execute_insert("INSERT INTO events VALUES (NULL)", limits),
        Err(CatalogError::Insert(InsertExecutionError::Insert(
            InsertError::NullNotAllowed {
                column: "value".to_owned(),
            }
        )))
    );

    catalog
        .execute_insert("INSERT INTO events VALUES (1)", limits)
        .unwrap();
    assert!(matches!(
        catalog.execute_insert("INSERT INTO events VALUES (2)", limits),
        Err(CatalogError::Insert(InsertExecutionError::Insert(
            InsertError::RowCapExceeded { .. }
        )))
    ));
    assert_eq!(catalog.table("events").unwrap().values(), &[Some(1)]);
}

#[test]
fn multi_row_insert_is_atomic_for_parse_and_storage_failures() {
    let limits = ParseLimits::default();
    let mut catalog = catalog(1, 3);
    catalog
        .execute_create("CREATE TABLE events (value Int64 NOT NULL)", limits)
        .unwrap();
    catalog
        .execute_insert("INSERT INTO events VALUES (1)", limits)
        .unwrap();

    assert!(matches!(
        catalog.execute_insert("INSERT INTO events VALUES (2), (invalid)", limits),
        Err(CatalogError::Parse(ParseError::InvalidInt64 { .. }))
    ));
    assert_eq!(catalog.table("events").unwrap().values(), &[Some(1)]);

    assert!(matches!(
        catalog.execute_insert("INSERT INTO events VALUES (2), (NULL)", limits),
        Err(CatalogError::Insert(InsertExecutionError::Insert(
            InsertError::NullNotAllowed { .. }
        )))
    ));
    assert_eq!(catalog.table("events").unwrap().values(), &[Some(1)]);

    assert!(matches!(
        catalog.execute_insert("INSERT INTO events VALUES (2), (3), (4)", limits),
        Err(CatalogError::Insert(InsertExecutionError::Insert(
            InsertError::RowCapExceeded {
                row_cap: 3,
                current_rows: 1,
                incoming_rows: 3,
            }
        )))
    ));
    assert_eq!(catalog.table("events").unwrap().values(), &[Some(1)]);

    catalog
        .execute_insert("INSERT INTO events VALUES (2), (3)", limits)
        .unwrap();
    assert_eq!(
        catalog.table("events").unwrap().values(),
        &[Some(1), Some(2), Some(3)]
    );
}

#[test]
fn select_preserves_unknown_table_and_column_errors() {
    let limits = ParseLimits::default();
    let mut catalog = catalog(1, 1);
    catalog
        .execute_create("CREATE TABLE events (value Int64)", limits)
        .unwrap();

    assert_eq!(
        catalog.execute_select("SELECT value FROM missing", limits),
        Err(CatalogError::Select(SelectExecutionError::UnknownTable {
            name: "missing".to_owned(),
        }))
    );
    assert_eq!(
        catalog.execute_select("SELECT other FROM events LIMIT 0", limits),
        Err(CatalogError::Select(SelectExecutionError::UnknownColumn {
            name: "other".to_owned(),
        }))
    );
    assert_eq!(
        catalog.execute_select("SELECT value FROM events WHERE other = 1", limits),
        Err(CatalogError::Select(SelectExecutionError::UnknownColumn {
            name: "other".to_owned(),
        }))
    );
}

#[test]
fn parse_failures_do_not_change_the_catalog() {
    let limits = ParseLimits::default();
    let mut catalog = catalog(1, 1);

    assert_eq!(
        catalog.execute_create("CREATE events", limits),
        Err(CatalogError::Parse(ParseError::UnexpectedInput {
            offset: 7,
            expected: "TABLE",
        }))
    );
    assert!(catalog.is_empty());

    catalog
        .execute_create("CREATE TABLE events (value Int64)", limits)
        .unwrap();
    let original = catalog.table("events").unwrap().clone();
    assert!(matches!(
        catalog.execute_insert("INSERT events", limits),
        Err(CatalogError::Parse(_))
    ));
    assert_eq!(catalog.table("events"), Some(&original));
}
