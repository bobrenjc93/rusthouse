//! CSVWithNames-compatible rendering for query results.

use std::io::{self, Write};

use crate::query::{QueryResult, Value};

/// Writes each query result as a header record followed by its scalar row.
pub fn write_results(results: &[QueryResult], writer: &mut impl Write) -> Result<(), io::Error> {
    for result in results {
        for (index, column) in result.columns.iter().enumerate() {
            write_separator(index, writer)?;
            write_quoted(&column.name, writer)?;
        }
        writer.write_all(b"\n")?;

        for (index, column) in result.columns.iter().enumerate() {
            write_separator(index, writer)?;
            match &column.value {
                Value::Int64(value) => write!(writer, "{value}")?,
                Value::Float64(value) => write!(writer, "{value}")?,
                Value::Bool(value) => write!(writer, "{value}")?,
                Value::String(value) => write_quoted(value, writer)?,
            }
        }
        writer.write_all(b"\n")?;
    }
    Ok(())
}

fn write_separator(index: usize, writer: &mut impl Write) -> Result<(), io::Error> {
    if index > 0 {
        writer.write_all(b",")?;
    }
    Ok(())
}

fn write_quoted(value: &str, writer: &mut impl Write) -> Result<(), io::Error> {
    writer.write_all(b"\"")?;
    for part in value.split_inclusive('"') {
        writer.write_all(part.as_bytes())?;
        if part.ends_with('"') {
            writer.write_all(b"\"")?;
        }
    }
    writer.write_all(b"\"")
}

#[cfg(test)]
mod tests {
    use crate::query::{Column, QueryResult, Value};

    use super::*;

    #[test]
    fn renders_headers_types_and_csv_escapes() {
        let result = QueryResult {
            columns: vec![
                Column {
                    name: "a,\"b".to_owned(),
                    value: Value::String("line 1\n\"line 2\"".to_owned()),
                },
                Column {
                    name: "number".to_owned(),
                    value: Value::Int64(7),
                },
            ],
        };
        let mut output = Vec::new();

        write_results(&[result], &mut output).unwrap();

        assert_eq!(
            String::from_utf8(output).unwrap(),
            "\"a,\"\"b\",\"number\"\n\"line 1\n\"\"line 2\"\"\",7\n"
        );
    }
}
