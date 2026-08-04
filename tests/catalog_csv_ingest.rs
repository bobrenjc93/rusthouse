use rusthouse::{
    Catalog, CatalogCsvIngestError, CatalogLimits, CsvIngestError, CsvIngestLimits, InsertError,
    ParseLimits,
};

fn catalog(row_cap: usize) -> Catalog {
    Catalog::new(CatalogLimits::new(2, row_cap))
}

#[test]
fn creates_ingests_and_selects_csv_rows_with_nulls() {
    let parse_limits = ParseLimits::default();
    let mut catalog = catalog(4);
    catalog
        .execute_create("CREATE TABLE readings (value Int64 NULL)", parse_limits)
        .unwrap();

    let rows = catalog
        .ingest_csv_with_names(
            "readings",
            b"value\n7\nNULL\n-2\n",
            CsvIngestLimits::new(64, 3),
        )
        .unwrap();

    assert_eq!(rows, 3);
    assert_eq!(
        catalog
            .execute_select("SELECT value FROM readings", parse_limits)
            .unwrap()
            .as_ref(),
        &[Some(7), None, Some(-2)]
    );
}

#[test]
fn resolves_table_names_exactly_before_ingesting() {
    let mut catalog = catalog(2);
    catalog
        .execute_create(
            "CREATE TABLE Readings (value Int64)",
            ParseLimits::default(),
        )
        .unwrap();

    let error = catalog
        .ingest_csv_with_names("readings", b"value\n1\n", CsvIngestLimits::new(32, 1))
        .unwrap_err();

    assert_eq!(
        error,
        CatalogCsvIngestError::UnknownTable {
            name: "readings".to_owned(),
        }
    );
    assert!(catalog.table("Readings").unwrap().is_empty());
}

#[test]
fn malformed_csv_is_wrapped_and_leaves_existing_rows_unchanged() {
    let parse_limits = ParseLimits::default();
    let mut catalog = catalog(3);
    catalog
        .execute_create("CREATE TABLE readings (value Int64 NULL)", parse_limits)
        .unwrap();
    catalog
        .execute_insert("INSERT INTO readings VALUES (9)", parse_limits)
        .unwrap();

    let error = catalog
        .ingest_csv_with_names(
            "readings",
            b"value\n1\nnot-an-int\n",
            CsvIngestLimits::new(64, 2),
        )
        .unwrap_err();

    assert_eq!(
        error,
        CatalogCsvIngestError::Csv(CsvIngestError::InvalidInt64 { line: 3 })
    );
    assert_eq!(catalog.table("readings").unwrap().values(), &[Some(9)]);
}

#[test]
fn csv_row_limit_failure_rolls_back_the_complete_batch() {
    let mut catalog = catalog(4);
    catalog
        .execute_create(
            "CREATE TABLE readings (value Int64)",
            ParseLimits::default(),
        )
        .unwrap();

    let error = catalog
        .ingest_csv_with_names("readings", b"value\n1\n2\n3\n", CsvIngestLimits::new(64, 2))
        .unwrap_err();

    assert_eq!(
        error,
        CatalogCsvIngestError::Csv(CsvIngestError::RowLimitExceeded {
            line: 4,
            rows: 3,
            max_rows: 2,
        })
    );
    assert!(catalog.table("readings").unwrap().is_empty());
}

#[test]
fn table_row_cap_failure_is_wrapped_and_rolls_back_the_complete_batch() {
    let parse_limits = ParseLimits::default();
    let mut catalog = catalog(2);
    catalog
        .execute_create("CREATE TABLE readings (value Int64)", parse_limits)
        .unwrap();
    catalog
        .execute_insert("INSERT INTO readings VALUES (9)", parse_limits)
        .unwrap();

    let error = catalog
        .ingest_csv_with_names("readings", b"value\n1\n2\n", CsvIngestLimits::new(64, 2))
        .unwrap_err();

    assert_eq!(
        error,
        CatalogCsvIngestError::Csv(CsvIngestError::TableInsert(InsertError::RowCapExceeded {
            row_cap: 2,
            current_rows: 1,
            incoming_rows: 2,
        }))
    );
    assert_eq!(catalog.table("readings").unwrap().values(), &[Some(9)]);
}
