//! Renderers for structured query results.

use std::fmt::Write;

use crate::engine::QueryResult;
use crate::error::{Error, Resource, Result};
use crate::value::Value;

/// A supported query output representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// A bordered, human-readable table.
    Table,
    /// Comma-separated values with a header row.
    Csv,
    /// A JSON object containing column metadata and positional rows.
    Json,
}

impl OutputFormat {
    /// Parses `table`, `csv`, or `json` without regard to ASCII case.
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

/// Renders one query result in the requested output format.
#[must_use]
pub fn render(result: &QueryResult, format: OutputFormat) -> String {
    render_with_limit(result, format, usize::MAX)
        .expect("unbounded rendering cannot exceed its limit")
}

/// Renders one query result while retaining at most `max_bytes` output bytes.
///
/// The returned limit error reports the exact number of bytes the complete
/// rendering would have produced.
pub fn render_with_limit(
    result: &QueryResult,
    format: OutputFormat,
    max_bytes: usize,
) -> Result<String> {
    match format {
        OutputFormat::Table => render_table(result, max_bytes),
        OutputFormat::Csv => render_csv(result, max_bytes),
        OutputFormat::Json => render_json(result, max_bytes),
    }
}

fn render_table(result: &QueryResult, max_bytes: usize) -> Result<String> {
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
    let mut output = LimitedOutput::new(max_bytes);
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
    output.finish()
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

fn table_row<'a>(
    output: &mut LimitedOutput,
    values: impl Iterator<Item = &'a str>,
    widths: &[usize],
) {
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

fn render_csv(result: &QueryResult, max_bytes: usize) -> Result<String> {
    let mut output = LimitedOutput::new(max_bytes);
    write_csv_row(
        &mut output,
        result.columns.iter().map(|column| column.name.as_str()),
    );
    for row in &result.rows {
        let values = row.iter().map(Value::as_display_string).collect::<Vec<_>>();
        write_csv_row(&mut output, values.iter().map(String::as_str));
    }
    output.finish()
}

fn write_csv_row<'a>(output: &mut LimitedOutput, values: impl Iterator<Item = &'a str>) {
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
fn render_json(result: &QueryResult, max_bytes: usize) -> Result<String> {
    let mut output = LimitedOutput::new(max_bytes);
    output.push_str("{\"columns\":[");
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
    output.finish()
}

fn write_json_value(output: &mut LimitedOutput, value: &Value) {
    match value {
        Value::Int64(value) => write!(output, "{value}").expect("writing to String cannot fail"),
        Value::Float64(value) => output.push_str(&Value::Float64(*value).as_display_string()),
        Value::Bool(value) => write!(output, "{value}").expect("writing to String cannot fail"),
        Value::String(value) => write_json_string(output, value),
    }
}

fn write_json_string(output: &mut LimitedOutput, value: &str) {
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

struct LimitedOutput {
    value: String,
    limit: usize,
    attempted: usize,
}

impl LimitedOutput {
    fn new(limit: usize) -> Self {
        Self {
            value: String::new(),
            limit,
            attempted: 0,
        }
    }

    fn push(&mut self, character: char) {
        let mut encoded = [0; 4];
        self.push_str(character.encode_utf8(&mut encoded));
    }

    fn push_str(&mut self, value: &str) {
        let previous = self.attempted;
        self.attempted = self.attempted.saturating_add(value.len());
        if previous <= self.limit && self.attempted <= self.limit {
            self.value.push_str(value);
        }
    }

    fn finish(self) -> Result<String> {
        if self.attempted > self.limit {
            Err(Error::ResourceLimitExceeded {
                resource: Resource::RenderedBytes,
                limit: self.limit,
                actual: self.attempted,
            })
        } else {
            Ok(self.value)
        }
    }
}

impl Write for LimitedOutput {
    fn write_str(&mut self, value: &str) -> std::fmt::Result {
        self.push_str(value);
        Ok(())
    }
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
