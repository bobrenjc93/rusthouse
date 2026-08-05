use std::io::{self, Read};

use rusthouse::{
    Catalog, CatalogCsvIngestError, CatalogCsvReaderIngestError, CatalogLimits, CsvIngestError,
    CsvIngestLimits, CsvReaderIngestError, InsertError, ParseLimits,
};

fn catalog(row_cap: usize) -> Catalog {
    Catalog::new(CatalogLimits::new(2, row_cap))
}

struct CountingReader<'a> {
    input: &'a [u8],
    position: usize,
}

impl<'a> CountingReader<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self { input, position: 0 }
    }
}

impl Read for CountingReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let available = &self.input[self.position..];
        let bytes_read = available.len().min(buffer.len());
        buffer[..bytes_read].copy_from_slice(&available[..bytes_read]);
        self.position += bytes_read;
        Ok(bytes_read)
    }
}

struct FailsInsteadOfEof<'a> {
    input: &'a [u8],
    position: usize,
}

impl Read for FailsInsteadOfEof<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.position == self.input.len() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "intentional reader failure",
            ));
        }

        let available = &self.input[self.position..];
        let bytes_read = available.len().min(buffer.len());
        buffer[..bytes_read].copy_from_slice(&available[..bytes_read]);
        self.position += bytes_read;
        Ok(bytes_read)
    }
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

#[test]
fn reader_ingest_at_exact_byte_limit_succeeds_and_rows_are_selectable() {
    let parse_limits = ParseLimits::default();
    let mut catalog = catalog(3);
    catalog
        .execute_create("CREATE TABLE readings (value Int64 NULL)", parse_limits)
        .unwrap();
    let input = b"value\n7\nNULL\n-2\n";
    let mut reader = CountingReader::new(input);

    let rows = catalog
        .ingest_csv_with_names_from_reader(
            "readings",
            &mut reader,
            CsvIngestLimits::new(input.len(), 3),
        )
        .unwrap();

    assert_eq!(rows, 3);
    assert_eq!(reader.position, input.len());
    assert_eq!(
        catalog
            .execute_select("SELECT value FROM readings", parse_limits)
            .unwrap()
            .as_ref(),
        &[Some(7), None, Some(-2)]
    );
}

#[test]
fn reader_table_lookup_is_exact_and_happens_before_consuming_input() {
    let mut catalog = catalog(1);
    catalog
        .execute_create(
            "CREATE TABLE Readings (value Int64)",
            ParseLimits::default(),
        )
        .unwrap();
    let mut reader = CountingReader::new(b"value\n1\n");

    let error = catalog
        .ingest_csv_with_names_from_reader("readings", &mut reader, CsvIngestLimits::new(32, 1))
        .unwrap_err();

    assert!(matches!(
        error,
        CatalogCsvReaderIngestError::UnknownTable { ref name } if name == "readings"
    ));
    assert_eq!(reader.position, 0);
    assert!(catalog.table("Readings").unwrap().is_empty());
}

#[test]
fn reader_oversized_input_stops_at_detection_byte_without_mutation() {
    let parse_limits = ParseLimits::default();
    let mut catalog = catalog(3);
    catalog
        .execute_create("CREATE TABLE readings (value Int64)", parse_limits)
        .unwrap();
    catalog
        .execute_insert("INSERT INTO readings VALUES (9)", parse_limits)
        .unwrap();
    let input = b"value\n1\n2\n";
    let max_bytes = input.len() - 3;
    let mut reader = CountingReader::new(input);

    let error = catalog
        .ingest_csv_with_names_from_reader(
            "readings",
            &mut reader,
            CsvIngestLimits::new(max_bytes, 2),
        )
        .unwrap_err();

    assert!(matches!(
        error,
        CatalogCsvReaderIngestError::Reader(CsvReaderIngestError::ByteLimitExceeded {
            bytes,
            max_bytes: limit,
        }) if bytes == max_bytes + 1 && limit == max_bytes
    ));
    assert_eq!(reader.position, max_bytes + 1);
    assert_eq!(catalog.table("readings").unwrap().values(), &[Some(9)]);
}

#[test]
fn reader_failure_remains_typed_and_prevents_mutation() {
    let parse_limits = ParseLimits::default();
    let mut catalog = catalog(2);
    catalog
        .execute_create("CREATE TABLE readings (value Int64)", parse_limits)
        .unwrap();
    catalog
        .execute_insert("INSERT INTO readings VALUES (9)", parse_limits)
        .unwrap();
    let reader = FailsInsteadOfEof {
        input: b"value\nnot-an-int\n",
        position: 0,
    };

    let error = catalog
        .ingest_csv_with_names_from_reader("readings", reader, CsvIngestLimits::new(64, 1))
        .unwrap_err();

    match error {
        CatalogCsvReaderIngestError::Reader(CsvReaderIngestError::Read(error)) => {
            assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);
            assert_eq!(error.to_string(), "intentional reader failure");
        }
        other => panic!("expected typed read failure, got {other:?}"),
    }
    assert_eq!(catalog.table("readings").unwrap().values(), &[Some(9)]);
}

#[test]
fn reader_malformed_csv_is_typed_and_atomic() {
    let parse_limits = ParseLimits::default();
    let mut catalog = catalog(3);
    catalog
        .execute_create("CREATE TABLE readings (value Int64)", parse_limits)
        .unwrap();
    catalog
        .execute_insert("INSERT INTO readings VALUES (9)", parse_limits)
        .unwrap();

    let error = catalog
        .ingest_csv_with_names_from_reader(
            "readings",
            &b"value\n1\n2,3\n"[..],
            CsvIngestLimits::new(64, 2),
        )
        .unwrap_err();

    assert!(matches!(
        error,
        CatalogCsvReaderIngestError::Reader(CsvReaderIngestError::Csv(
            CsvIngestError::WrongColumnCount {
                line: 3,
                columns: 2,
            }
        ))
    ));
    assert_eq!(catalog.table("readings").unwrap().values(), &[Some(9)]);
}

#[test]
fn reader_table_row_cap_failure_is_typed_and_rolls_back() {
    let parse_limits = ParseLimits::default();
    let mut catalog = catalog(2);
    catalog
        .execute_create("CREATE TABLE readings (value Int64)", parse_limits)
        .unwrap();
    catalog
        .execute_insert("INSERT INTO readings VALUES (9)", parse_limits)
        .unwrap();

    let error = catalog
        .ingest_csv_with_names_from_reader(
            "readings",
            &b"value\n1\n2\n"[..],
            CsvIngestLimits::new(64, 2),
        )
        .unwrap_err();

    assert!(matches!(
        error,
        CatalogCsvReaderIngestError::Reader(CsvReaderIngestError::Csv(
            CsvIngestError::TableInsert(InsertError::RowCapExceeded {
                row_cap: 2,
                current_rows: 1,
                incoming_rows: 2,
            })
        ))
    ));
    assert_eq!(catalog.table("readings").unwrap().values(), &[Some(9)]);
}
