use std::io::{self, Write};

use rusthouse::csv::write_csv;
use rusthouse::{ColumnSchema, DataType, Schema, Table, TableLimits, Value};

fn table(columns: &[(&str, DataType)]) -> Table {
    let schema = Schema::new(
        columns
            .iter()
            .map(|(name, data_type)| ColumnSchema::new(*name, *data_type))
            .collect(),
    )
    .unwrap();
    Table::new(schema, TableLimits::default()).unwrap()
}

#[test]
fn streams_mixed_typed_columns_in_schema_and_row_order() {
    let mut table = table(&[
        ("id", DataType::Int64),
        ("score", DataType::Float64),
        ("active", DataType::Bool),
        ("label", DataType::String),
    ]);
    table
        .insert_batch(vec![
            vec![
                Value::Int64(-7),
                Value::Float64(1.25),
                Value::Bool(true),
                Value::String("first".into()),
            ],
            vec![
                Value::Int64(42),
                Value::Float64(-0.5),
                Value::Bool(false),
                Value::String("second".into()),
            ],
        ])
        .unwrap();

    let mut output = Vec::new();
    write_csv(&table, &mut output).unwrap();

    assert_eq!(
        String::from_utf8(output).unwrap(),
        "id,score,active,label\r\n-7,1.25,true,first\r\n42,-0.5,false,second\r\n"
    );
}

#[test]
fn writes_only_the_schema_header_for_an_empty_table() {
    let table = table(&[("id", DataType::Int64), ("name", DataType::String)]);
    let mut output = Vec::new();

    write_csv(&table, &mut output).unwrap();

    assert_eq!(output, b"id,name\r\n");
}

#[test]
fn applies_rfc_4180_escaping_to_headers_and_utf8_string_boundaries() {
    let mut table = table(&[
        ("plain", DataType::String),
        ("with,comma", DataType::String),
        ("with\"quote", DataType::String),
        ("with\rreturn", DataType::String),
        ("with\nline", DataType::String),
    ]);
    table
        .insert_row(vec![
            Value::String("café".into()),
            Value::String(",edge,".into()),
            Value::String("\"quoted\"".into()),
            Value::String("left\rright".into()),
            Value::String("top\r\nbottom".into()),
        ])
        .unwrap();

    let mut output = Vec::new();
    write_csv(&table, &mut output).unwrap();

    assert_eq!(
        String::from_utf8(output).unwrap(),
        concat!(
            "plain,\"with,comma\",\"with\"\"quote\",\"with\rreturn\",\"with\nline\"\r\n",
            "café,\",edge,\",\"\"\"quoted\"\"\",\"left\rright\",\"top\r\nbottom\"\r\n",
        )
    );
}

#[test]
fn propagates_writer_errors() {
    let table = table(&[("identifier", DataType::Int64)]);
    let expected = io::Error::new(io::ErrorKind::Other, "output unavailable");
    let mut writer = FailingWriter {
        remaining: 4,
        error: Some(expected),
    };

    let error = write_csv(&table, &mut writer).unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::Other);
    assert_eq!(error.to_string(), "output unavailable");
}

struct FailingWriter {
    remaining: usize,
    error: Option<io::Error>,
}

impl Write for FailingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            return Err(self.error.take().unwrap());
        }

        let written = self.remaining.min(bytes.len());
        self.remaining -= written;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
