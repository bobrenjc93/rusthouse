use rusthouse::formats::{
    CsvBatchReader, CsvExportOptions, CsvOptions, FormatError, LimitKind, MAX_JSON_NESTING_DEPTH,
    NdjsonOptions, export_csv, export_ndjson, ingest_csv, ingest_ndjson,
};
use rusthouse::{Column, ColumnBatch, DataType, Field, Schema, Table};
use std::io::Cursor;

fn all_types_schema() -> Schema {
    Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("score", DataType::Float64, true),
        Field::new("active", DataType::Bool, false),
        Field::new("note", DataType::String, true),
    ])
    .unwrap()
}

fn source_table() -> Table {
    let schema = all_types_schema();
    let batch = ColumnBatch::new(
        &schema,
        vec![
            Column::Int64(vec![Some(1), Some(2), Some(3), Some(4), Some(5)]),
            Column::Float64(vec![Some(1.25), None, Some(-0.5), Some(1e100), Some(0.0)]),
            Column::Bool(vec![
                Some(true),
                Some(false),
                Some(true),
                Some(false),
                Some(true),
            ]),
            Column::String(vec![
                Some("plain".to_owned()),
                Some("comma, quote \" and newline\nnext".to_owned()),
                Some("\\N".to_owned()),
                Some(String::new()),
                None,
            ]),
        ],
    )
    .unwrap();
    let mut table = Table::new(schema);
    table.append_batch(&batch).unwrap();
    table
}

#[test]
fn csv_round_trip_spans_multiple_batches_and_escapes_null_token() {
    let source = source_table();
    let mut encoded = Vec::new();
    export_csv(&mut encoded, &source, &CsvExportOptions::default()).unwrap();
    let text = std::str::from_utf8(&encoded).unwrap();
    assert!(text.contains("\"comma, quote \"\" and newline\nnext\""));
    assert!(text.contains("\"\\N\""));

    let mut destination = Table::new(source.schema().clone());
    let mut options = CsvOptions::default();
    options.limits.batch_rows = 2;
    assert_eq!(
        ingest_csv(Cursor::new(encoded), &mut destination, options).unwrap(),
        5
    );
    assert_eq!(destination, source);
}

#[test]
fn ndjson_round_trip_escapes_strings_and_allows_reordered_fields() {
    let source = source_table();
    let mut encoded = Vec::new();
    export_ndjson(&mut encoded, &source).unwrap();
    let text = std::str::from_utf8(&encoded).unwrap();
    assert!(text.contains(r#"comma, quote \" and newline\nnext"#));
    assert!(text.contains("\"score\":null"));

    let mut destination = Table::new(source.schema().clone());
    let mut options = NdjsonOptions::default();
    options.limits.batch_rows = 2;
    assert_eq!(
        ingest_ndjson(Cursor::new(encoded), &mut destination, options).unwrap(),
        5
    );
    assert_eq!(destination, source);

    let schema = Schema::new(vec![
        Field::new("a", DataType::Int64, false),
        Field::new("b", DataType::Bool, false),
    ])
    .unwrap();
    let mut reordered = Table::new(schema);
    ingest_ndjson(
        Cursor::new(
            br#"{"b":true,"a":7}
"#,
        ),
        &mut reordered,
        NdjsonOptions::default(),
    )
    .unwrap();
    assert_eq!(reordered.columns()[0], Column::Int64(vec![Some(7)]));
}

#[test]
fn readers_emit_fixed_size_typed_batches() {
    let schema = Schema::new(vec![Field::new("id", DataType::Int64, false)]).unwrap();
    let mut options = CsvOptions::default();
    options.limits.batch_rows = 2;
    let reader =
        CsvBatchReader::new(Cursor::new(b"id\n1\n2\n3\n4\n5\n"), &schema, options).unwrap();
    let batches: Vec<_> = reader.collect::<Result<_, _>>().unwrap();
    assert_eq!(
        batches.iter().map(ColumnBatch::rows).collect::<Vec<_>>(),
        vec![2, 2, 1]
    );
    assert_eq!(batches[2].columns()[0], Column::Int64(vec![Some(5)]));
}

#[test]
fn extreme_batch_size_allocates_only_for_rows_read() {
    let fields = (0..1_024)
        .map(|index| Field::new(format!("field_{index}"), DataType::String, true))
        .collect();
    let schema = Schema::new(fields).unwrap();

    let mut csv_options = CsvOptions {
        has_header: false,
        ..CsvOptions::default()
    };
    csv_options.limits.batch_rows = usize::MAX;
    let mut csv_table = Table::new(schema.clone());
    assert_eq!(
        ingest_csv(Cursor::new(&[]), &mut csv_table, csv_options).unwrap(),
        0
    );

    let mut json_options = NdjsonOptions::default();
    json_options.limits.batch_rows = usize::MAX;
    let mut json_table = Table::new(schema);
    assert_eq!(
        ingest_ndjson(Cursor::new(&[]), &mut json_table, json_options).unwrap(),
        0
    );
}

#[test]
fn malformed_late_records_leave_destination_unchanged() {
    let mut destination = source_table();
    let original = destination.clone();
    let mut csv_options = CsvOptions::default();
    csv_options.limits.batch_rows = 1;
    let csv = b"id,score,active,note\n10,1.0,true,valid\ninvalid,2.0,false,later\n";
    assert!(matches!(
        ingest_csv(Cursor::new(csv), &mut destination, csv_options),
        Err(FormatError::Conversion { row: 2, .. })
    ));
    assert_eq!(destination, original);

    let mut json_options = NdjsonOptions::default();
    json_options.limits.batch_rows = 1;
    let ndjson = br#"{"id":10,"score":1.0,"active":true,"note":"valid"}
{"id":11,"score":2.0,"active":false,"extra":"later"}
"#;
    assert!(matches!(
        ingest_ndjson(Cursor::new(ndjson), &mut destination, json_options),
        Err(FormatError::UnknownField { row: 2, .. })
    ));
    assert_eq!(destination, original);
}

#[test]
fn csv_null_empty_and_quoted_null_are_distinct() {
    let schema = Schema::new(vec![Field::new("value", DataType::String, true)]).unwrap();
    let mut table = Table::new(schema.clone());
    let options = CsvOptions {
        has_header: false,
        ..CsvOptions::default()
    };
    ingest_csv(Cursor::new(b"\\N\n\"\\N\"\n\n"), &mut table, options).unwrap();
    assert_eq!(
        table.columns()[0],
        Column::String(vec![None, Some("\\N".to_owned()), Some(String::new())])
    );

    let non_nullable = Schema::new(vec![Field::new("value", DataType::String, false)]).unwrap();
    let mut table = Table::new(non_nullable);
    assert!(matches!(
        ingest_csv(
            Cursor::new(b"\\N\n"),
            &mut table,
            CsvOptions {
                has_header: false,
                ..CsvOptions::default()
            }
        ),
        Err(FormatError::NullNotAllowed { .. })
    ));
}

#[test]
fn exact_byte_row_field_record_and_string_boundaries_are_enforced() {
    let schema = Schema::new(vec![Field::new("v", DataType::String, false)]).unwrap();
    let input = b"abc\n";
    let mut exact = CsvOptions {
        has_header: false,
        ..CsvOptions::default()
    };
    exact.limits.max_input_bytes = input.len() as u64;
    exact.limits.max_rows = 1;
    exact.limits.max_field_bytes = 3;
    exact.limits.max_string_bytes = 3;
    exact.limits.max_record_bytes = 3;
    ingest_csv(
        Cursor::new(input),
        &mut Table::new(schema.clone()),
        exact.clone(),
    )
    .unwrap();

    for (options, kind) in [
        (
            {
                let mut value = exact.clone();
                value.limits.max_input_bytes -= 1;
                value
            },
            LimitKind::InputBytes,
        ),
        (
            {
                let mut value = exact.clone();
                value.limits.max_field_bytes -= 1;
                value
            },
            LimitKind::FieldBytes,
        ),
        (
            {
                let mut value = exact.clone();
                value.limits.max_string_bytes -= 1;
                value
            },
            LimitKind::StringBytes,
        ),
        (
            {
                let mut value = exact.clone();
                value.limits.max_record_bytes -= 1;
                value
            },
            LimitKind::RecordBytes,
        ),
    ] {
        let error = ingest_csv(
            Cursor::new(input),
            &mut Table::new(schema.clone()),
            options.clone(),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            FormatError::LimitExceeded { kind: actual, .. } if actual == kind
        ));
    }

    let mut row_options = exact;
    row_options.limits.max_input_bytes = 8;
    let error = ingest_csv(
        Cursor::new(b"abc\ndef\n"),
        &mut Table::new(schema),
        row_options,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        FormatError::LimitExceeded {
            kind: LimitKind::Rows,
            ..
        }
    ));
}

#[test]
fn json_field_count_depth_and_decoded_string_boundaries_are_enforced() {
    let schema = Schema::new(vec![Field::new("v", DataType::String, false)]).unwrap();
    let mut exact = NdjsonOptions::default();
    exact.limits.max_string_bytes = 1;
    exact.limits.max_field_bytes = 8;
    exact.limits.max_nesting_depth = 1;
    exact.limits.max_fields_per_row = 1;
    ingest_ndjson(
        Cursor::new(
            br#"{"v":"\u0061"}
"#,
        ),
        &mut Table::new(schema.clone()),
        exact.clone(),
    )
    .unwrap();

    let string_error = ingest_ndjson(
        Cursor::new(
            br#"{"v":"\u0061b"}
"#,
        ),
        &mut Table::new(schema.clone()),
        exact.clone(),
    )
    .unwrap_err();
    assert!(matches!(
        string_error,
        FormatError::LimitExceeded {
            kind: LimitKind::StringBytes,
            ..
        }
    ));

    let depth_error = ingest_ndjson(
        Cursor::new(
            br#"{"v":[]}
"#,
        ),
        &mut Table::new(schema.clone()),
        exact.clone(),
    )
    .unwrap_err();
    assert!(matches!(
        depth_error,
        FormatError::LimitExceeded {
            kind: LimitKind::NestingDepth,
            ..
        }
    ));

    let field_error = ingest_ndjson(
        Cursor::new(
            br#"{"v":"a","extra":1}
"#,
        ),
        &mut Table::new(schema),
        exact,
    )
    .unwrap_err();
    assert!(matches!(
        field_error,
        FormatError::LimitExceeded {
            kind: LimitKind::FieldsPerRow,
            ..
        }
    ));
}

#[test]
fn json_nesting_configuration_is_stack_safe() {
    let schema = Schema::new(vec![Field::new("v", DataType::String, true)]).unwrap();
    let mut excessive = NdjsonOptions::default();
    excessive.limits.max_nesting_depth = 100_000;
    let error =
        ingest_ndjson(Cursor::new(&[]), &mut Table::new(schema.clone()), excessive).unwrap_err();
    assert!(matches!(error, FormatError::InvalidOption(_)));

    let mut at_ceiling = NdjsonOptions::default();
    at_ceiling.limits.max_nesting_depth = MAX_JSON_NESTING_DEPTH;
    let nested = format!(
        "{{\"v\":{}null{}}}\n",
        "[".repeat(MAX_JSON_NESTING_DEPTH),
        "]".repeat(MAX_JSON_NESTING_DEPTH)
    );
    let error =
        ingest_ndjson(Cursor::new(nested), &mut Table::new(schema), at_ceiling).unwrap_err();
    assert!(matches!(
        error,
        FormatError::LimitExceeded {
            kind: LimitKind::NestingDepth,
            limit,
            ..
        } if limit == MAX_JSON_NESTING_DEPTH as u64
    ));
}

#[test]
fn malformed_escaping_and_json_shape_fail_explicitly() {
    let string_schema = Schema::new(vec![Field::new("value", DataType::String, false)]).unwrap();
    let csv_error = ingest_csv(
        Cursor::new(b"\"closed\"x\n"),
        &mut Table::new(string_schema.clone()),
        CsvOptions {
            has_header: false,
            ..CsvOptions::default()
        },
    )
    .unwrap_err();
    assert!(matches!(csv_error, FormatError::CsvSyntax { .. }));

    for (input, expected) in [
        (
            br#"{"value":"a","value":"b"}
"#
            .as_slice(),
            "duplicate",
        ),
        (
            br#"{}
"#
            .as_slice(),
            "missing",
        ),
        (
            br#"{"other":"a"}
"#
            .as_slice(),
            "unknown",
        ),
    ] {
        let error = ingest_ndjson(
            Cursor::new(input),
            &mut Table::new(string_schema.clone()),
            NdjsonOptions::default(),
        )
        .unwrap_err();
        assert!(matches!(
            (expected, error),
            ("duplicate", FormatError::DuplicateField { .. })
                | ("missing", FormatError::MissingField { .. })
                | ("unknown", FormatError::UnknownField { .. })
        ));
    }

    let mut unicode = Table::new(string_schema);
    ingest_ndjson(
        Cursor::new(
            br#"{"value":"\ud83d\ude00"}
"#,
        ),
        &mut unicode,
        NdjsonOptions::default(),
    )
    .unwrap();
    assert_eq!(
        unicode.columns()[0],
        Column::String(vec![Some("\u{1f600}".to_owned())])
    );
}

#[test]
fn deterministic_random_strings_round_trip_through_both_formats() {
    let schema = Schema::new(vec![Field::new("text", DataType::String, true)]).unwrap();
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut next = || {
        state ^= state << 7;
        state ^= state >> 9;
        state ^= state << 8;
        state
    };
    let mut values = Vec::new();
    for row in 0..300 {
        if row % 11 == 0 {
            values.push(None);
            continue;
        }
        let length = (next() % 48) as usize;
        let mut value = String::new();
        for _ in 0..length {
            value.push(match next() % 12 {
                0 => ',',
                1 => '"',
                2 => '\n',
                3 => '\r',
                4 => '\t',
                5 => '\\',
                6 => '/',
                7 => '\u{08}',
                other => char::from(b'a' + other as u8),
            });
        }
        values.push(Some(value));
    }
    let batch = ColumnBatch::new(&schema, vec![Column::String(values)]).unwrap();
    let mut source = Table::new(schema.clone());
    source.append_batch(&batch).unwrap();

    let mut csv = Vec::new();
    export_csv(&mut csv, &source, &CsvExportOptions::default()).unwrap();
    let mut csv_table = Table::new(schema.clone());
    let mut csv_options = CsvOptions::default();
    csv_options.limits.batch_rows = 7;
    ingest_csv(Cursor::new(csv), &mut csv_table, csv_options).unwrap();
    assert_eq!(csv_table, source);

    let mut ndjson = Vec::new();
    export_ndjson(&mut ndjson, &source).unwrap();
    let mut json_table = Table::new(schema);
    let mut json_options = NdjsonOptions::default();
    json_options.limits.batch_rows = 7;
    ingest_ndjson(Cursor::new(ndjson), &mut json_table, json_options).unwrap();
    assert_eq!(json_table, source);
}
