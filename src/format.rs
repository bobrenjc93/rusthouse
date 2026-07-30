use std::collections::HashSet;
use std::fmt::Write as FmtWrite;
use std::io::{self, Write as IoWrite};

use crate::engine::QueryResult;
use crate::value::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Table,
    Csv,
    Json,
    JsonEachRow,
}

impl OutputFormat {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "table" => Some(Self::Table),
            "csv" => Some(Self::Csv),
            "json" => Some(Self::Json),
            "jsoneachrow" => Some(Self::JsonEachRow),
            _ => None,
        }
    }
}

/// Render a result into an owned string.
///
/// # Panics
///
/// Panics when JSONEachRow output has duplicate column names. Use
/// [`try_render`] when the result schema is not already known to be valid.
#[must_use]
pub fn render(result: &QueryResult, format: OutputFormat) -> String {
    try_render(result, format)
        .unwrap_or_else(|error| panic!("could not render {format:?}: {error}"))
}

/// Render a result, returning schema errors for object-based formats.
pub fn try_render(result: &QueryResult, format: OutputFormat) -> io::Result<String> {
    match format {
        OutputFormat::Table => Ok(render_table(result)),
        OutputFormat::Csv => Ok(render_csv(result)),
        OutputFormat::Json => Ok(render_json(result)),
        OutputFormat::JsonEachRow => render_json_each_row(result),
    }
}

/// Validate that a result can be represented by the selected format.
pub fn validate(result: &QueryResult, format: OutputFormat) -> io::Result<()> {
    if format != OutputFormat::JsonEachRow {
        return Ok(());
    }

    let mut names = HashSet::with_capacity(result.columns.len());
    for column in &result.columns {
        if !names.insert(column.name.to_ascii_lowercase()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "duplicate JSONEachRow output column '{}'; column names must be unique",
                    column.name
                ),
            ));
        }
    }
    Ok(())
}

/// Write a result in the selected format.
///
/// JSONEachRow is emitted one record at a time, so serialized output is not
/// accumulated into a second result-sized allocation.
pub fn write<W: IoWrite>(
    result: &QueryResult,
    format: OutputFormat,
    writer: &mut W,
) -> io::Result<()> {
    if format != OutputFormat::JsonEachRow {
        return writer.write_all(render(result, format).as_bytes());
    }

    validate(result, format)?;
    for row in &result.rows {
        let record = render_json_each_row_record(result, row);
        writer.write_all(record.as_bytes())?;
        writer.write_all(b"\n")?;
    }
    Ok(())
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

fn render_json_each_row(result: &QueryResult) -> io::Result<String> {
    let mut output = Vec::new();
    write(result, OutputFormat::JsonEachRow, &mut output)?;
    Ok(String::from_utf8(output).expect("JSON output is UTF-8"))
}

fn render_json_each_row_record(result: &QueryResult, row: &[Value]) -> String {
    debug_assert_eq!(result.columns.len(), row.len());
    let mut output = String::from("{");
    for (index, (column, value)) in result.columns.iter().zip(row).enumerate() {
        if index > 0 {
            output.push(',');
        }
        write_json_string(&mut output, &column.name);
        output.push(':');
        write_json_value(&mut output, value);
    }
    output.push('}');
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
    fn renders_json_each_row_as_independent_objects() {
        assert_eq!(
            render(&result(), OutputFormat::JsonEachRow),
            "{\"id\":1,\"note\":\"quote: \\\", comma\"}\n"
        );
    }

    #[test]
    fn json_each_row_rejects_duplicate_names_before_writing() {
        let result = QueryResult {
            columns: vec![
                ResultColumn {
                    name: "id".to_owned(),
                    data_type: DataType::Int64,
                },
                ResultColumn {
                    name: "ID".to_owned(),
                    data_type: DataType::Int64,
                },
            ],
            rows: vec![vec![Value::Int64(1), Value::Int64(1)]],
        };
        let mut output = Vec::new();

        let error = write(&result, OutputFormat::JsonEachRow, &mut output)
            .expect_err("duplicate output names must fail");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(
            error
                .to_string()
                .contains("duplicate JSONEachRow output column 'ID'")
        );
        assert!(output.is_empty());
        assert!(try_render(&result, OutputFormat::JsonEachRow).is_err());
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
