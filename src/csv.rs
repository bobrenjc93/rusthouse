use std::io::{self, Write};

use crate::QueryResult;

/// Writes a query result as RFC 4180-style CSV, including a header row.
pub fn write_csv<W: Write>(writer: &mut W, result: &QueryResult) -> io::Result<()> {
    write_record(
        writer,
        result.columns.iter().map(|column| column.name.as_str()),
    )?;
    for row in &result.rows {
        write_record(writer, row.iter().map(|value| value.display_text()))?;
    }
    Ok(())
}

fn write_record<W, I, S>(writer: &mut W, values: I) -> io::Result<()>
where
    W: Write,
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    for (index, value) in values.into_iter().enumerate() {
        if index != 0 {
            writer.write_all(b",")?;
        }
        write_field(writer, value.as_ref())?;
    }
    writer.write_all(b"\n")
}

fn write_field<W: Write>(writer: &mut W, value: &str) -> io::Result<()> {
    if value
        .bytes()
        .any(|byte| matches!(byte, b',' | b'"' | b'\n' | b'\r'))
    {
        writer.write_all(b"\"")?;
        let mut parts = value.split('"');
        if let Some(first) = parts.next() {
            writer.write_all(first.as_bytes())?;
        }
        for part in parts {
            writer.write_all(b"\"\"")?;
            writer.write_all(part.as_bytes())?;
        }
        writer.write_all(b"\"")
    } else {
        writer.write_all(value.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ColumnDefinition, DataType, Value};

    #[test]
    fn escapes_headers_and_values() {
        let result = QueryResult {
            columns: vec![ColumnDefinition {
                name: "say,what".into(),
                data_type: DataType::String,
            }],
            rows: vec![
                vec![Value::String("plain".into())],
                vec![Value::String("a,\"b\"\nnext".into())],
            ],
        };
        let mut output = Vec::new();
        write_csv(&mut output, &result).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "\"say,what\"\nplain\n\"a,\"\"b\"\"\nnext\"\n"
        );
    }
}
