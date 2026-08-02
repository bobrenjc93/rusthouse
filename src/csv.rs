//! CSV rendering for typed query results.

use std::io::{self, Write};

use crate::{QueryResult, Value};

/// Write one query result as RFC 4180-style CSV with a header row.
pub fn write_query(mut writer: impl Write, result: &QueryResult) -> io::Result<()> {
    for (index, column) in result.columns().iter().enumerate() {
        write_separator(&mut writer, index)?;
        write_field(&mut writer, column.name())?;
    }
    writer.write_all(b"\n")?;

    for row in result.rows() {
        for (index, value) in row.iter().enumerate() {
            write_separator(&mut writer, index)?;
            match value {
                Value::Int64(value) => write!(writer, "{value}")?,
                Value::Float64(value) => write!(writer, "{value}")?,
                Value::Bool(value) => write!(writer, "{value}")?,
                Value::String(value) => write_field(&mut writer, value)?,
            }
        }
        writer.write_all(b"\n")?;
    }
    Ok(())
}

fn write_separator(writer: &mut impl Write, index: usize) -> io::Result<()> {
    if index > 0 {
        writer.write_all(b",")?;
    }
    Ok(())
}

fn write_field(writer: &mut impl Write, value: &str) -> io::Result<()> {
    if !value
        .bytes()
        .any(|byte| matches!(byte, b',' | b'"' | b'\r' | b'\n'))
    {
        return writer.write_all(value.as_bytes());
    }

    writer.write_all(b"\"")?;
    for byte in value.bytes() {
        if byte == b'"' {
            writer.write_all(b"\"\"")?;
        } else {
            writer.write_all(&[byte])?;
        }
    }
    writer.write_all(b"\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_commas_quotes_and_line_breaks() {
        let cases = [
            ("plain", "plain"),
            ("a,b", "\"a,b\""),
            ("say \"hi\"", "\"say \"\"hi\"\"\""),
            ("two\nlines", "\"two\nlines\""),
        ];

        for (input, expected) in cases {
            let mut output = Vec::new();
            write_field(&mut output, input).expect("write field");
            assert_eq!(String::from_utf8(output).expect("UTF-8"), expected);
        }
    }
}
