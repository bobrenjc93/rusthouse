use std::fmt::Write;
use std::io::Write as IoWrite;

use crate::engine::{QueryResult, ResultColumn, RowSink};
use crate::error::{Error, Result};
use crate::value::{Value, ValueRef};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Table,
    Csv,
    Json,
}

impl OutputFormat {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "table" => Some(Self::Table),
            "csv" => Some(Self::Csv),
            "json" => Some(Self::Json),
            _ => None,
        }
    }
}

/// Streams CSV or JSON query results to an [`std::io::Write`] destination.
///
/// JSON is emitted as one document with a top-level `results` array. CSV
/// result sets are separated by a blank line. Call [`Self::finish`] whenever
/// completed output should be kept, including after a later execution error,
/// to close JSON output and flush the destination.
#[derive(Debug)]
pub struct StreamingWriter<W> {
    output: W,
    format: OutputFormat,
    query_count: usize,
    row_count: usize,
    query_open: bool,
}

impl<W: IoWrite> StreamingWriter<W> {
    /// Creates a streaming CSV or JSON writer.
    pub fn new(output: W, format: OutputFormat) -> Result<Self> {
        if format == OutputFormat::Table {
            return Err(Error::InvalidQuery(
                "streaming table output is not supported".to_owned(),
            ));
        }
        Ok(Self {
            output,
            format,
            query_count: 0,
            row_count: 0,
            query_open: false,
        })
    }

    /// Completes the output document, flushes it, and returns the destination.
    pub fn finish(mut self) -> Result<W> {
        if self.query_open {
            return Err(Error::Output(
                "cannot finish while a query result is open".to_owned(),
            ));
        }
        if self.format == OutputFormat::Json {
            if self.query_count == 0 {
                write_output(&mut self.output, b"{\"results\":[]}\n")?;
            } else {
                write_output(&mut self.output, b"]}\n")?;
            }
        }
        self.output
            .flush()
            .map_err(|error| output_error("could not flush output", error))?;
        Ok(self.output)
    }
}

impl<W: IoWrite> RowSink for StreamingWriter<W> {
    fn begin_query(&mut self, columns: &[ResultColumn]) -> Result<()> {
        if self.query_open {
            return Err(Error::Output(
                "cannot start a query while another result is open".to_owned(),
            ));
        }

        match self.format {
            OutputFormat::Csv => {
                if self.query_count > 0 {
                    write_output(&mut self.output, b"\n")?;
                }
                write_csv_fields(
                    &mut self.output,
                    columns.iter().map(|column| column.name.as_str()),
                )?;
            }
            OutputFormat::Json => {
                if self.query_count == 0 {
                    write_output(&mut self.output, b"{\"results\":[")?;
                } else {
                    write_output(&mut self.output, b",")?;
                }
                write_output(&mut self.output, b"{\"columns\":[")?;
                for (index, column) in columns.iter().enumerate() {
                    if index > 0 {
                        write_output(&mut self.output, b",")?;
                    }
                    write_output(&mut self.output, b"{\"name\":")?;
                    write_json_string_io(&mut self.output, &column.name)?;
                    write_output(&mut self.output, b",\"type\":")?;
                    write_json_string_io(&mut self.output, &column.data_type.to_string())?;
                    write_output(&mut self.output, b"}")?;
                }
                write_output(&mut self.output, b"],\"rows\":[")?;
            }
            OutputFormat::Table => unreachable!("table format is rejected by the constructor"),
        }

        self.row_count = 0;
        self.query_open = true;
        Ok(())
    }

    fn write_row(&mut self, row: &[ValueRef<'_>]) -> Result<()> {
        if !self.query_open {
            return Err(Error::Output(
                "cannot write a row without an open query result".to_owned(),
            ));
        }

        match self.format {
            OutputFormat::Csv => write_csv_values(&mut self.output, row)?,
            OutputFormat::Json => {
                if self.row_count > 0 {
                    write_output(&mut self.output, b",")?;
                }
                write_output(&mut self.output, b"[")?;
                for (index, value) in row.iter().copied().enumerate() {
                    if index > 0 {
                        write_output(&mut self.output, b",")?;
                    }
                    write_json_value_io(&mut self.output, value)?;
                }
                write_output(&mut self.output, b"]")?;
            }
            OutputFormat::Table => unreachable!("table format is rejected by the constructor"),
        }
        self.row_count += 1;
        Ok(())
    }

    fn end_query(&mut self) -> Result<()> {
        if !self.query_open {
            return Err(Error::Output(
                "cannot end a query result that is not open".to_owned(),
            ));
        }
        if self.format == OutputFormat::Json {
            write_output(&mut self.output, b"]}")?;
        }
        self.query_count += 1;
        self.query_open = false;
        Ok(())
    }
}

fn write_output(output: &mut impl IoWrite, bytes: &[u8]) -> Result<()> {
    output
        .write_all(bytes)
        .map_err(|error| output_error("could not write output", error))
}

fn output_error(context: &str, error: std::io::Error) -> Error {
    Error::Output(format!("{context}: {error}"))
}

fn write_csv_fields<'a>(
    output: &mut impl IoWrite,
    values: impl Iterator<Item = &'a str>,
) -> Result<()> {
    for (index, value) in values.enumerate() {
        if index > 0 {
            write_output(output, b",")?;
        }
        write_csv_field_io(output, value)?;
    }
    write_output(output, b"\n")
}

fn write_csv_values(output: &mut impl IoWrite, values: &[ValueRef<'_>]) -> Result<()> {
    for (index, value) in values.iter().copied().enumerate() {
        if index > 0 {
            write_output(output, b",")?;
        }
        match value {
            ValueRef::String(value) => write_csv_field_io(output, value)?,
            ValueRef::Int64(value) => write_output(output, value.to_string().as_bytes())?,
            ValueRef::Float64(value) => {
                write_output(output, Value::Float64(value).as_display_string().as_bytes())?
            }
            ValueRef::Bool(value) => write_output(output, if value { b"true" } else { b"false" })?,
        }
    }
    write_output(output, b"\n")
}

fn write_csv_field_io(output: &mut impl IoWrite, value: &str) -> Result<()> {
    if !value.contains([',', '"', '\n', '\r']) {
        return write_output(output, value.as_bytes());
    }

    write_output(output, b"\"")?;
    let mut start = 0;
    for (index, byte) in value.bytes().enumerate() {
        if byte == b'"' {
            write_output(output, &value.as_bytes()[start..index])?;
            write_output(output, b"\"\"")?;
            start = index + 1;
        }
    }
    write_output(output, &value.as_bytes()[start..])?;
    write_output(output, b"\"")
}

fn write_json_value_io(output: &mut impl IoWrite, value: ValueRef<'_>) -> Result<()> {
    match value {
        ValueRef::Int64(value) => write_output(output, value.to_string().as_bytes()),
        ValueRef::Float64(value) => {
            write_output(output, Value::Float64(value).as_display_string().as_bytes())
        }
        ValueRef::Bool(value) => write_output(output, if value { b"true" } else { b"false" }),
        ValueRef::String(value) => write_json_string_io(output, value),
    }
}

fn write_json_string_io(output: &mut impl IoWrite, value: &str) -> Result<()> {
    write_output(output, b"\"")?;
    for character in value.chars() {
        let escaped = match character {
            '"' => Some("\\\""),
            '\\' => Some("\\\\"),
            '\u{08}' => Some("\\b"),
            '\u{0c}' => Some("\\f"),
            '\n' => Some("\\n"),
            '\r' => Some("\\r"),
            '\t' => Some("\\t"),
            _ => None,
        };
        if let Some(escaped) = escaped {
            write_output(output, escaped.as_bytes())?;
        } else if character.is_control() {
            write_output(output, format!("\\u{:04x}", character as u32).as_bytes())?;
        } else {
            let mut encoded = [0; 4];
            write_output(output, character.encode_utf8(&mut encoded).as_bytes())?;
        }
    }
    write_output(output, b"\"")
}

#[must_use]
pub fn render(result: &QueryResult, format: OutputFormat) -> String {
    match format {
        OutputFormat::Table => render_table(result),
        OutputFormat::Csv => render_csv(result),
        OutputFormat::Json => render_json(result),
    }
}

fn render_table(result: &QueryResult) -> String {
    let rendered_columns = result
        .columns
        .iter()
        .map(|column| escape_table_text(&column.name))
        .collect::<Vec<_>>();
    let rendered_rows = result
        .rows
        .iter()
        .map(|row| row.iter().map(table_value).collect::<Vec<_>>())
        .collect::<Vec<_>>();
    let widths = rendered_columns
        .iter()
        .enumerate()
        .map(|(column, name)| {
            rendered_rows
                .iter()
                .map(|row| row[column].chars().count())
                .max()
                .unwrap_or(0)
                .max(name.chars().count())
        })
        .collect::<Vec<_>>();

    let border = table_border(&widths);
    let mut output = String::new();
    output.push_str(&border);
    output.push('\n');
    table_row(
        &mut output,
        rendered_columns.iter().map(String::as_str),
        &widths,
    );
    output.push_str(&border);
    output.push('\n');
    for row in &rendered_rows {
        table_row(&mut output, row.iter().map(String::as_str), &widths);
    }
    output.push_str(&border);
    output
}

fn table_border(widths: &[usize]) -> String {
    let mut border = String::new();
    border.push('+');
    for width in widths {
        border.push_str(&"-".repeat(*width + 2));
        border.push('+');
    }
    border
}

fn table_row<'a>(output: &mut String, values: impl Iterator<Item = &'a str>, widths: &[usize]) {
    output.push('|');
    for (value, width) in values.zip(widths) {
        output.push(' ');
        output.push_str(value);
        output.push_str(&" ".repeat(width.saturating_sub(value.chars().count()) + 1));
        output.push('|');
    }
    output.push('\n');
}

fn table_value(value: &Value) -> String {
    escape_table_text(&value.as_display_string())
}

fn escape_table_text(value: &str) -> String {
    let mut output = String::new();
    for character in value.chars() {
        match character {
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            value if value.is_control() => {
                write!(output, "\\u{{{:04x}}}", value as u32)
                    .expect("writing to String cannot fail");
            }
            value => output.push(value),
        }
    }
    output
}

fn render_csv(result: &QueryResult) -> String {
    let mut output = String::new();
    write_csv_row(
        &mut output,
        result.columns.iter().map(|column| column.name.as_str()),
    );
    for row in &result.rows {
        let values = row.iter().map(Value::as_display_string).collect::<Vec<_>>();
        write_csv_row(&mut output, values.iter().map(String::as_str));
    }
    output
}

fn write_csv_row<'a>(output: &mut String, values: impl Iterator<Item = &'a str>) {
    for (index, value) in values.enumerate() {
        if index > 0 {
            output.push(',');
        }
        if value.contains([',', '"', '\n', '\r']) {
            output.push('"');
            output.push_str(&value.replace('"', "\"\""));
            output.push('"');
        } else {
            output.push_str(value);
        }
    }
    output.push('\n');
}

/// Render one result set with explicit column metadata and positional rows.
///
/// Positional rows preserve every value even when output column names repeat.
fn render_json(result: &QueryResult) -> String {
    let mut output = String::from("{\"columns\":[");
    for (index, column) in result.columns.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        output.push_str("{\"name\":");
        write_json_string(&mut output, &column.name);
        output.push_str(",\"type\":");
        write_json_string(&mut output, &column.data_type.to_string());
        output.push('}');
    }
    output.push_str("],\"rows\":[");
    for (row_index, row) in result.rows.iter().enumerate() {
        if row_index > 0 {
            output.push(',');
        }
        output.push('[');
        for (column_index, value) in row.iter().enumerate() {
            if column_index > 0 {
                output.push(',');
            }
            write_json_value(&mut output, value);
        }
        output.push(']');
    }
    output.push_str("]}");
    output
}

fn write_json_value(output: &mut String, value: &Value) {
    match value {
        Value::Int64(value) => write!(output, "{value}").expect("writing to String cannot fail"),
        Value::Float64(value) => output.push_str(&Value::Float64(*value).as_display_string()),
        Value::Bool(value) => write!(output, "{value}").expect("writing to String cannot fail"),
        Value::String(value) => write_json_string(output, value),
    }
}

fn write_json_string(output: &mut String, value: &str) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{08}' => output.push_str("\\b"),
            '\u{0c}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            value if value.is_control() => {
                write!(output, "\\u{:04x}", value as u32).expect("writing to String cannot fail");
            }
            value => output.push(value),
        }
    }
    output.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::ResultColumn;
    use crate::value::DataType;

    fn result() -> QueryResult {
        QueryResult {
            columns: vec![
                ResultColumn {
                    name: "id".to_owned(),
                    data_type: DataType::Int64,
                },
                ResultColumn {
                    name: "note".to_owned(),
                    data_type: DataType::String,
                },
            ],
            rows: vec![vec![
                Value::Int64(1),
                Value::String("quote: \", comma".to_owned()),
            ]],
        }
    }

    #[test]
    fn renders_csv_escaping() {
        assert_eq!(
            render(&result(), OutputFormat::Csv),
            "id,note\n1,\"quote: \"\", comma\"\n"
        );
    }

    #[test]
    fn renders_json_with_schema_and_native_scalar_types() {
        assert_eq!(
            render(&result(), OutputFormat::Json),
            r#"{"columns":[{"name":"id","type":"Int64"},{"name":"note","type":"String"}],"rows":[[1,"quote: \", comma"]]}"#
        );
    }

    #[test]
    fn positional_json_preserves_duplicate_output_names() {
        let result = QueryResult {
            columns: vec![
                ResultColumn {
                    name: "id".to_owned(),
                    data_type: DataType::Int64,
                },
                ResultColumn {
                    name: "id".to_owned(),
                    data_type: DataType::String,
                },
            ],
            rows: vec![vec![Value::Int64(1), Value::String("x".to_owned())]],
        };

        assert_eq!(
            render(&result, OutputFormat::Json),
            r#"{"columns":[{"name":"id","type":"Int64"},{"name":"id","type":"String"}],"rows":[[1,"x"]]}"#
        );
    }

    #[test]
    fn renders_readable_table() {
        let rendered = render(&result(), OutputFormat::Table);
        assert!(rendered.contains("| id | note"));
        assert!(rendered.contains("| 1  | quote: \", comma"));
    }

    #[test]
    fn table_output_escapes_terminal_control_characters() {
        let result = QueryResult {
            columns: vec![ResultColumn {
                name: "text".to_owned(),
                data_type: DataType::String,
            }],
            rows: vec![vec![Value::String(
                "\u{1b}[31mred\u{07}\u{00}\u{7f}".to_owned(),
            )]],
        };

        let rendered = render(&result, OutputFormat::Table);
        assert!(rendered.contains(r"\u{001b}[31mred\u{0007}\u{0000}\u{007f}"));
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains('\u{07}'));
        assert!(!rendered.contains('\u{00}'));
        assert!(!rendered.contains('\u{7f}'));
    }
}
