use std::io::{self, Write};

use rusthouse::{ColumnSchema, DataType, Schema, Table, Value, write_csv};

#[test]
fn writes_all_value_types_with_stable_scalar_encodings() {
    let schema = Schema::new(vec![
        ColumnSchema::new("integer", DataType::Int64, true),
        ColumnSchema::new("float", DataType::Float64, true),
        ColumnSchema::new("boolean", DataType::Bool, true),
        ColumnSchema::new("string", DataType::String, true),
    ])
    .unwrap();
    let mut table = Table::new(schema);
    table
        .insert_rows(&[
            vec![
                Value::Int64(i64::MIN),
                Value::Float64(1.25),
                Value::Bool(true),
                Value::from("plain"),
            ],
            vec![
                Value::Int64(0),
                Value::Float64(-0.0),
                Value::Bool(false),
                Value::from(""),
            ],
            vec![Value::Null, Value::Null, Value::Null, Value::Null],
        ])
        .unwrap();

    let mut output = Vec::new();
    write_csv(&mut output, &table).unwrap();

    assert_eq!(
        String::from_utf8(output).unwrap(),
        "integer,float,boolean,string\n\
         -9223372036854775808,1.25,true,plain\n\
         0,-0,false,\n\
         \\N,\\N,\\N,\\N\n"
    );
}

#[test]
fn quotes_special_characters_in_headers_and_string_cells() {
    let fields = [
        "simple",
        "with,comma",
        "with\"quote",
        "with\rcarriage",
        "with\nline",
        "all,\"three\r\n",
    ];
    let schema = Schema::new(
        fields
            .iter()
            .map(|name| ColumnSchema::new(*name, DataType::String, false))
            .collect(),
    )
    .unwrap();
    let mut table = Table::new(schema);
    table.insert_row(&fields.map(Value::from)).unwrap();

    let mut output = Vec::new();
    write_csv(&mut output, &table).unwrap();

    let record = "simple,\"with,comma\",\"with\"\"quote\",\"with\rcarriage\",\"with\nline\",\"all,\"\"three\r\n\"\n";
    assert_eq!(
        String::from_utf8(output).unwrap(),
        format!("{record}{record}")
    );
}

#[test]
fn distinguishes_null_marker_from_a_string_and_writes_header_for_empty_table() {
    let schema = Schema::new(vec![ColumnSchema::new(r"\N", DataType::String, true)]).unwrap();
    let mut table = Table::new(schema.clone());

    let mut output = Vec::new();
    write_csv(&mut output, &table).unwrap();
    assert_eq!(
        output,
        br#""\N"
"#
    );

    table
        .insert_rows(&[vec![Value::Null], vec![Value::from(r"\N")]])
        .unwrap();
    output.clear();
    write_csv(&mut output, &table).unwrap();
    assert_eq!(output, b"\"\\N\"\n\\N\n\"\\N\"\n");
}

#[test]
fn propagates_a_writer_failure_without_visiting_the_rest_of_the_table() {
    let schema = Schema::new(vec![ColumnSchema::new("value", DataType::String, false)]).unwrap();
    let mut table = Table::new(schema);
    table.insert_row(&[Value::from("first")]).unwrap();
    table.insert_row(&[Value::from("second")]).unwrap();
    let mut writer = FailingWriter::new(b"value\nfir".len());

    let error = write_csv(&mut writer, &table).unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::Other);
    assert_eq!(error.to_string(), "configured writer failure");
    assert_eq!(writer.output, b"value\nfir");
}

struct FailingWriter {
    output: Vec<u8>,
    byte_limit: usize,
}

impl FailingWriter {
    fn new(byte_limit: usize) -> Self {
        Self {
            output: Vec::new(),
            byte_limit,
        }
    }
}

impl Write for FailingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let remaining = self.byte_limit.saturating_sub(self.output.len());
        if remaining == 0 {
            return Err(io::Error::other("configured writer failure"));
        }

        let written = remaining.min(buffer.len());
        self.output.extend_from_slice(&buffer[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
