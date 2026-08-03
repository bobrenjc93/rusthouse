use rusthouse::{
    CsvIngestError, CsvIngestLimits, InsertError, Int64Table, Schema, ingest_csv_with_names,
};

fn table(nullable: bool, row_cap: usize) -> Int64Table {
    Int64Table::new(Schema::int64("value", nullable), row_cap)
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
