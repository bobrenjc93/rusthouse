use std::io::{self, Read};

use rusthouse::{
    CsvIngestError, CsvIngestLimits, CsvReaderIngestError, InsertError, Int64Table, Schema,
    ingest_csv_with_names, ingest_csv_with_names_from_reader,
};

fn table(nullable: bool, row_cap: usize) -> Int64Table {
    Int64Table::new(Schema::int64("value", nullable), row_cap)
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
fn ingests_lf_records_with_null_and_integer_extremes() {
    let mut table = table(true, 4);
    let input = format!("value\n{}\nNULL\n+0\n{}\n", i64::MIN, i64::MAX);

    let rows = ingest_csv_with_names(&mut table, input, CsvIngestLimits::new(128, 4)).unwrap();

    assert_eq!(rows, 4);
    assert_eq!(
        table.values(),
        &[Some(i64::MIN), None, Some(0), Some(i64::MAX)]
    );
}

#[test]
fn ingests_crlf_records_without_a_final_line_ending() {
    let mut table = table(true, 3);

    let rows = ingest_csv_with_names(
        &mut table,
        b"value\r\n-1\r\nNULL\r\n2",
        CsvIngestLimits::new(64, 3),
    )
    .unwrap();

    assert_eq!(rows, 3);
    assert_eq!(table.values(), &[Some(-1), None, Some(2)]);
}

#[test]
fn exact_byte_limit_succeeds_and_exceeded_limit_is_atomic() {
    let input = b"value\n1\n2\n";
    let mut exact = table(false, 3);
    exact.append(Some(0)).unwrap();

    let rows =
        ingest_csv_with_names(&mut exact, input, CsvIngestLimits::new(input.len(), 2)).unwrap();
    assert_eq!(rows, 2);
    assert_eq!(exact.values(), &[Some(0), Some(1), Some(2)]);

    let mut exceeded = table(false, 3);
    exceeded.append(Some(0)).unwrap();
    let error = ingest_csv_with_names(
        &mut exceeded,
        input,
        CsvIngestLimits::new(input.len() - 1, 2),
    )
    .unwrap_err();

    assert_eq!(
        error,
        CsvIngestError::ByteLimitExceeded {
            bytes: input.len(),
            max_bytes: input.len() - 1,
        }
    );
    assert_eq!(exceeded.values(), &[Some(0)]);
}

#[test]
fn reader_exact_limit_succeeds_and_oversized_input_stops_after_detection_byte() {
    let input = b"value\n1\n2\n";
    let mut exact_reader = CountingReader::new(input);
    let mut exact = table(false, 3);
    exact.append(Some(0)).unwrap();

    let rows = ingest_csv_with_names_from_reader(
        &mut exact,
        &mut exact_reader,
        CsvIngestLimits::new(input.len(), 2),
    )
    .unwrap();

    assert_eq!(rows, 2);
    assert_eq!(exact_reader.position, input.len());
    assert_eq!(exact.values(), &[Some(0), Some(1), Some(2)]);

    let max_bytes = input.len() - 3;
    let mut oversized_reader = CountingReader::new(input);
    let mut oversized = table(false, 3);
    oversized.append(Some(9)).unwrap();
    let error = ingest_csv_with_names_from_reader(
        &mut oversized,
        &mut oversized_reader,
        CsvIngestLimits::new(max_bytes, 2),
    )
    .unwrap_err();

    assert!(matches!(
        error,
        CsvReaderIngestError::ByteLimitExceeded {
            bytes,
            max_bytes: limit,
        } if bytes == max_bytes + 1 && limit == max_bytes
    ));
    assert_eq!(oversized_reader.position, max_bytes + 1);
    assert_eq!(oversized.values(), &[Some(9)]);
}

#[test]
fn reader_failure_is_typed_and_prevents_parsing_or_appending() {
    let mut table = table(false, 2);
    table.append(Some(9)).unwrap();
    let reader = FailsInsteadOfEof {
        input: b"value\nnot-an-int\n",
        position: 0,
    };

    let error = ingest_csv_with_names_from_reader(&mut table, reader, CsvIngestLimits::new(64, 1))
        .unwrap_err();

    match error {
        CsvReaderIngestError::Read(error) => {
            assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);
            assert_eq!(error.to_string(), "intentional reader failure");
        }
        other => panic!("expected typed read failure, got {other:?}"),
    }
    assert_eq!(table.values(), &[Some(9)]);
}

#[test]
fn reader_malformed_csv_is_wrapped_after_complete_read_and_is_atomic() {
    let input = b"value\n1\n2,3\n";
    let mut reader = CountingReader::new(input);
    let mut table = table(false, 3);
    table.append(Some(9)).unwrap();

    let error = ingest_csv_with_names_from_reader(
        &mut table,
        &mut reader,
        CsvIngestLimits::new(input.len(), 2),
    )
    .unwrap_err();

    assert!(matches!(
        error,
        CsvReaderIngestError::Csv(CsvIngestError::WrongColumnCount {
            line: 3,
            columns: 2,
        })
    ));
    assert_eq!(reader.position, input.len());
    assert_eq!(table.values(), &[Some(9)]);
}

#[test]
fn reader_row_limits_and_table_row_caps_preserve_existing_rows() {
    let mut row_limited = table(false, 4);
    row_limited.append(Some(9)).unwrap();
    let error = ingest_csv_with_names_from_reader(
        &mut row_limited,
        &b"value\n1\n2\n3\n"[..],
        CsvIngestLimits::new(64, 2),
    )
    .unwrap_err();

    assert!(matches!(
        error,
        CsvReaderIngestError::Csv(CsvIngestError::RowLimitExceeded {
            line: 4,
            rows: 3,
            max_rows: 2,
        })
    ));
    assert_eq!(row_limited.values(), &[Some(9)]);

    let mut table_limited = table(false, 2);
    table_limited.append(Some(9)).unwrap();
    let error = ingest_csv_with_names_from_reader(
        &mut table_limited,
        &b"value\n1\n2\n"[..],
        CsvIngestLimits::new(64, 2),
    )
    .unwrap_err();

    assert!(matches!(
        error,
        CsvReaderIngestError::Csv(CsvIngestError::TableInsert(InsertError::RowCapExceeded {
            row_cap: 2,
            current_rows: 1,
            incoming_rows: 2,
        }))
    ));
    assert_eq!(table_limited.values(), &[Some(9)]);
}

#[test]
fn exact_row_limit_succeeds_and_next_record_reports_its_line() {
    let mut exact = table(false, 2);
    let rows =
        ingest_csv_with_names(&mut exact, "value\n1\n2\n", CsvIngestLimits::new(32, 2)).unwrap();
    assert_eq!(rows, 2);

    let mut exceeded = table(false, 4);
    exceeded.append(Some(9)).unwrap();
    let error = ingest_csv_with_names(
        &mut exceeded,
        "value\n1\n2\n3\n",
        CsvIngestLimits::new(32, 2),
    )
    .unwrap_err();

    assert_eq!(
        error,
        CsvIngestError::RowLimitExceeded {
            line: 4,
            rows: 3,
            max_rows: 2,
        }
    );
    assert_eq!(exceeded.values(), &[Some(9)]);
}

#[test]
fn rejects_missing_and_mismatched_headers() {
    let mut table = table(true, 2);

    assert_eq!(
        ingest_csv_with_names(&mut table, "", CsvIngestLimits::new(16, 1)),
        Err(CsvIngestError::MissingHeader { line: 1 })
    );
    assert_eq!(
        ingest_csv_with_names(&mut table, "other\n1\n", CsvIngestLimits::new(16, 1)),
        Err(CsvIngestError::HeaderMismatch {
            line: 1,
            expected: "value".to_owned(),
        })
    );
    assert!(table.is_empty());
}

#[test]
fn malformed_records_have_typed_line_errors_and_do_not_append() {
    let cases = [
        ("value\n1\n\n", CsvIngestError::EmptyRecord { line: 3 }),
        (
            "value\n1\n2,3\n",
            CsvIngestError::WrongColumnCount {
                line: 3,
                columns: 2,
            },
        ),
        ("value\n1\n 2\n", CsvIngestError::InvalidInt64 { line: 3 }),
        (
            "value\n1\n9223372036854775808\n",
            CsvIngestError::InvalidInt64 { line: 3 },
        ),
        (
            "value\n1\n-9223372036854775809\n",
            CsvIngestError::InvalidInt64 { line: 3 },
        ),
        (
            "value\n1\n\"2\"\n",
            CsvIngestError::InvalidInt64 { line: 3 },
        ),
    ];

    for (input, expected) in cases {
        let mut table = table(true, 4);
        table.append(Some(10)).unwrap();

        let error =
            ingest_csv_with_names(&mut table, input, CsvIngestLimits::new(64, 3)).unwrap_err();

        assert_eq!(error, expected, "input: {input:?}");
        assert_eq!(table.values(), &[Some(10)], "input: {input:?}");
    }
}

#[test]
fn null_in_non_nullable_table_reports_the_source_line_atomically() {
    let mut table = table(false, 4);
    table.append(Some(10)).unwrap();

    let error = ingest_csv_with_names(
        &mut table,
        "value\n1\nNULL\n2\n",
        CsvIngestLimits::new(64, 3),
    )
    .unwrap_err();

    assert_eq!(
        error,
        CsvIngestError::NullNotAllowed {
            line: 3,
            column: "value".to_owned(),
        }
    );
    assert_eq!(table.values(), &[Some(10)]);
}

#[test]
fn table_row_cap_failure_is_preserved_and_atomic() {
    let mut table = table(false, 2);
    table.append(Some(10)).unwrap();

    let error = ingest_csv_with_names(&mut table, "value\n1\n2\n", CsvIngestLimits::new(32, 2))
        .unwrap_err();

    assert_eq!(
        error,
        CsvIngestError::TableInsert(InsertError::RowCapExceeded {
            row_cap: 2,
            current_rows: 1,
            incoming_rows: 2,
        })
    );
    assert_eq!(table.values(), &[Some(10)]);
}

#[test]
fn header_only_input_is_a_valid_empty_batch() {
    for input in ["value", "value\n", "value\r\n"] {
        let mut table = table(true, 1);
        table.append(None).unwrap();

        let rows =
            ingest_csv_with_names(&mut table, input, CsvIngestLimits::new(input.len(), 0)).unwrap();

        assert_eq!(rows, 0);
        assert_eq!(table.values(), &[None]);
    }
}

#[test]
fn lone_carriage_return_is_not_a_supported_line_ending() {
    let mut table = table(false, 1);

    let error =
        ingest_csv_with_names(&mut table, "value\r1", CsvIngestLimits::new(16, 1)).unwrap_err();

    assert_eq!(
        error,
        CsvIngestError::HeaderMismatch {
            line: 1,
            expected: "value".to_owned(),
        }
    );
    assert!(table.is_empty());
}
