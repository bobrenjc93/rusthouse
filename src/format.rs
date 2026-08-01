use std::io::Write;

use crate::engine::QueryResult;
use crate::error::{Error, Result};
use crate::value::Value;

pub const MAX_OUTPUT_BYTES: usize = 256 * 1024 * 1024;

/// A CSVWithNames-compatible, byte-limited result writer.
pub struct CsvWriter<W> {
    output: W,
    written: usize,
    limit: usize,
}

impl<W: Write> CsvWriter<W> {
    pub fn new(output: W, limit: usize) -> Self {
        Self {
            output,
            written: 0,
            limit,
        }
    }

    pub fn write_result(&mut self, result: &QueryResult) -> Result<()> {
        for (index, column) in result.columns.iter().enumerate() {
            if index > 0 {
                self.write(b",")?;
            }
            self.write_quoted(&column.name)?;
        }
        self.write(b"\n")?;

        for row in &result.rows {
            for (index, value) in row.iter().enumerate() {
                if index > 0 {
                    self.write(b",")?;
                }
                self.write_value(value)?;
            }
            self.write(b"\n")?;
        }
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        self.output.flush().map_err(Error::from)
    }

    fn write_value(&mut self, value: &Value) -> Result<()> {
        match value {
            Value::Null => self.write(b"\\N"),
            Value::Int64(value) => self.write(value.to_string().as_bytes()),
            Value::Float64(value) => self.write(value.to_string().as_bytes()),
            Value::Bool(value) => self.write(if *value { b"true" } else { b"false" }),
            Value::String(value) => self.write_quoted(value),
        }
    }

    fn write_quoted(&mut self, value: &str) -> Result<()> {
        self.write(b"\"")?;
        let mut start = 0;
        for (index, byte) in value.bytes().enumerate() {
            if byte == b'"' {
                self.write(&value.as_bytes()[start..index])?;
                self.write(b"\"\"")?;
                start = index + 1;
            }
        }
        self.write(&value.as_bytes()[start..])?;
        self.write(b"\"")
    }

    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        if self.written.saturating_add(bytes.len()) > self.limit {
            return Err(Error::Limit {
                resource: "CSV output bytes",
                limit: self.limit,
            });
        }
        self.output.write_all(bytes)?;
        self.written += bytes.len();
        Ok(())
    }
}

pub fn write_csv<W: Write>(result: &QueryResult, output: W) -> Result<()> {
    let mut writer = CsvWriter::new(output, MAX_OUTPUT_BYTES);
    writer.write_result(result)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::ResultColumn;

    #[test]
    fn quotes_csv_strings_and_names() {
        let result = QueryResult {
            columns: vec![ResultColumn {
                name: "a\"b".to_owned(),
                data_type: None,
                nullable: true,
            }],
            rows: vec![vec![Value::String("x,\"y\"".to_owned())], vec![Value::Null]],
        };
        let mut output = Vec::new();
        write_csv(&result, &mut output).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "\"a\"\"b\"\n\"x,\"\"y\"\"\"\n\\N\n"
        );
    }

    #[test]
    fn enforces_the_output_limit() {
        let result = QueryResult {
            columns: vec![ResultColumn {
                name: "value".to_owned(),
                data_type: Some(crate::DataType::String),
                nullable: false,
            }],
            rows: vec![vec![Value::String("large".to_owned())]],
        };
        let error = CsvWriter::new(Vec::new(), 4)
            .write_result(&result)
            .unwrap_err();
        assert!(matches!(
            error,
            Error::Limit {
                resource: "CSV output bytes",
                limit: 4
            }
        ));
    }
}
