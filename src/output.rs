use std::io::{self, Write};

use crate::{QueryResult, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Csv,
    Table,
    Json,
}

impl OutputFormat {
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "csv" => Some(Self::Csv),
            "table" => Some(Self::Table),
            "json" => Some(Self::Json),
            _ => None,
        }
    }
}

pub fn write_result(
    writer: &mut impl Write,
    result: &QueryResult,
    format: OutputFormat,
) -> io::Result<()> {
    match format {
        OutputFormat::Csv => write_csv(writer, result),
        OutputFormat::Table => write_table(writer, result),
        OutputFormat::Json => write_json(writer, result),
    }
}

fn write_csv(writer: &mut impl Write, result: &QueryResult) -> io::Result<()> {
    write_csv_record(writer, result.columns.iter().map(String::as_str))?;
    for row in &result.rows {
        write_csv_record(
            writer,
            row.iter().map(|value| match value {
                Value::Null => String::new(),
                value => value.to_string(),
            }),
        )?;
    }
    Ok(())
}

fn write_csv_record<S: AsRef<str>>(
    writer: &mut impl Write,
    fields: impl IntoIterator<Item = S>,
) -> io::Result<()> {
    let mut first = true;
    for field in fields {
        if !first {
            writer.write_all(b",")?;
        }
        first = false;
        let field = field.as_ref();
        if field.contains([',', '"', '\n', '\r']) {
            writer.write_all(b"\"")?;
            for part in field.split_inclusive('"') {
                writer.write_all(part.as_bytes())?;
                if part.ends_with('"') {
                    writer.write_all(b"\"")?;
                }
            }
            writer.write_all(b"\"")?;
        } else {
            writer.write_all(field.as_bytes())?;
        }
    }
    writer.write_all(b"\n")
}

fn write_table(writer: &mut impl Write, result: &QueryResult) -> io::Result<()> {
    writeln!(writer, "{}", result.columns.join("\t"))?;
    for row in &result.rows {
        let values = row.iter().map(Value::to_string).collect::<Vec<_>>();
        writeln!(writer, "{}", values.join("\t"))?;
    }
    Ok(())
}

fn write_json(writer: &mut impl Write, result: &QueryResult) -> io::Result<()> {
    writer.write_all(b"[")?;
    for (row_index, row) in result.rows.iter().enumerate() {
        if row_index > 0 {
            writer.write_all(b",")?;
        }
        writer.write_all(b"{")?;
        for (column_index, (column, value)) in result.columns.iter().zip(row).enumerate() {
            if column_index > 0 {
                writer.write_all(b",")?;
            }
            write_json_string(writer, column)?;
            writer.write_all(b":")?;
            match value {
                Value::Null => writer.write_all(b"null")?,
                Value::Int64(value) => write!(writer, "{value}")?,
                Value::Float64(value) if value.is_finite() => write!(writer, "{value}")?,
                Value::Float64(_) => writer.write_all(b"null")?,
                Value::Bool(value) => write!(writer, "{value}")?,
                Value::String(value) => write_json_string(writer, value)?,
            }
        }
        writer.write_all(b"}")?;
    }
    writer.write_all(b"]\n")
}

fn write_json_string(writer: &mut impl Write, value: &str) -> io::Result<()> {
    writer.write_all(b"\"")?;
    for character in value.chars() {
        match character {
            '"' => writer.write_all(b"\\\"")?,
            '\\' => writer.write_all(b"\\\\")?,
            '\n' => writer.write_all(b"\\n")?,
            '\r' => writer.write_all(b"\\r")?,
            '\t' => writer.write_all(b"\\t")?,
            character if character.is_control() => write!(writer, "\\u{:04x}", character as u32)?,
            character => write!(writer, "{character}")?,
        }
    }
    writer.write_all(b"\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_has_headers_and_quotes_fields() {
        let result = QueryResult {
            columns: vec!["name".into(), "value".into()],
            rows: vec![vec![Value::String("a,\"b".into()), Value::Null]],
        };
        let mut output = Vec::new();
        write_result(&mut output, &result, OutputFormat::Csv).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "name,value\n\"a,\"\"b\",\n"
        );
    }
}
