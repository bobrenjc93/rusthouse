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
    let mut widths = result
        .columns
        .iter()
        .map(|column| escaped_table_width(&column.name))
        .collect::<Vec<_>>();
    for row in &result.rows {
        for (column, value) in row.iter().enumerate().take(widths.len()) {
            widths[column] = widths[column].max(table_value_width(value));
        }
    }

    let mut output = LimitedOutput::new(max_bytes);
    write_table_border(&mut output, &widths);
    output.push('\n');
    output.push('|');
    for (column, width) in result.columns.iter().zip(&widths) {
        output.push(' ');
        write_escaped_table_text(&mut output, &column.name);
        output.push_repeated(
            ' ',
            width.saturating_sub(escaped_table_width(&column.name)) + 1,
        );
        output.push('|');
    }
    output.push('\n');
    write_table_border(&mut output, &widths);
    output.push('\n');
    for row in &result.rows {
        output.push('|');
        for (value, width) in row.iter().zip(&widths) {
            output.push(' ');
            write_table_value(&mut output, value);
            output.push_repeated(' ', width.saturating_sub(table_value_width(value)) + 1);
            output.push('|');
        }
        output.push('\n');
    }
    write_table_border(&mut output, &widths);
    output.finish()
}

fn write_table_border(output: &mut LimitedOutput, widths: &[usize]) {
    output.push('+');
    for width in widths {
        output.push_repeated('-', width.saturating_add(2));
        output.push('+');
    }
}

fn table_value_width(value: &Value) -> usize {
    match value {
        Value::String(value) => escaped_table_width(value),
        value => escaped_table_width(&value.as_display_string()),
    }
}

fn write_table_value(output: &mut LimitedOutput, value: &Value) {
    match value {
        Value::String(value) => write_escaped_table_text(output, value),
        value => write_escaped_table_text(output, &value.as_display_string()),
    }
}

fn escaped_table_width(value: &str) -> usize {
    value
        .chars()
        .map(|character| match character {
            '\\' | '\n' | '\r' | '\t' => 2,
            value if value.is_control() => 4 + hex_digits(value as u32).max(4),
            _ => 1,
        })
        .sum()
}

fn hex_digits(mut value: u32) -> usize {
    let mut digits = 1;
    while value >= 16 {
        value /= 16;
        digits += 1;
    }
    digits
}

fn write_escaped_table_text(output: &mut LimitedOutput, value: &str) {
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
}

fn render_csv(result: &QueryResult, max_bytes: usize) -> Result<String> {
    let mut output = LimitedOutput::new(max_bytes);
    for (index, column) in result.columns.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        write_csv_text(&mut output, &column.name);
    }
    output.push('\n');
    for row in &result.rows {
        for (index, value) in row.iter().enumerate() {
            if index > 0 {
                output.push(',');
            }
            match value {
                Value::String(value) => write_csv_text(&mut output, value),
                value => write_csv_text(&mut output, &value.as_display_string()),
            }
        }
        output.push('\n');
    }
    output.finish()
}

fn write_csv_text(output: &mut LimitedOutput, value: &str) {
    if value.contains([',', '"', '\n', '\r']) {
        output.push('"');
        for character in value.chars() {
            if character == '"' {
                output.push('"');
            }
            output.push(character);
        }
        output.push('"');
    } else {
        output.push_str(value);
    }
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

    fn push_repeated(&mut self, character: char, count: usize) {
        let previous = self.attempted;
        self.attempted = self
            .attempted
            .saturating_add(character.len_utf8().saturating_mul(count));
        if previous <= self.limit && self.attempted <= self.limit {
            for _ in 0..count {
                self.value.push(character);
            }
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
