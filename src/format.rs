use std::fmt::Write;

use crate::engine::QueryResult;
use crate::value::Value;

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

#[must_use]
pub fn render(result: &QueryResult, format: OutputFormat) -> String {
    match format {
        OutputFormat::Table => render_table(result),
        OutputFormat::Csv => render_csv(result),
        OutputFormat::Json => render_json(result),
    }
}

pub(crate) fn rendered_len(result: &QueryResult, format: OutputFormat) -> Option<usize> {
    match format {
        OutputFormat::Json => json_len(result),
        OutputFormat::Csv => csv_len(result),
        OutputFormat::Table => None,
    }
}

fn checked_add(total: &mut usize, amount: usize) -> Option<()> {
    *total = total.checked_add(amount)?;
    Some(())
}

fn json_len(result: &QueryResult) -> Option<usize> {
    let mut length = "{\"columns\":[".len();
    for (index, column) in result.columns.iter().enumerate() {
        checked_add(&mut length, usize::from(index > 0))?;
        checked_add(&mut length, "{\"name\":".len())?;
        checked_add(&mut length, json_string_len(&column.name)?)?;
        checked_add(&mut length, ",\"type\":".len())?;
        checked_add(&mut length, json_string_len(&column.data_type.to_string())?)?;
        checked_add(&mut length, 1)?;
    }
    checked_add(&mut length, "],\"rows\":[".len())?;
    for (row_index, row) in result.rows.iter().enumerate() {
        checked_add(&mut length, usize::from(row_index > 0) + 1)?;
        for (column_index, value) in row.iter().enumerate() {
            checked_add(&mut length, usize::from(column_index > 0))?;
            checked_add(&mut length, json_value_len(value)?)?;
        }
        checked_add(&mut length, 1)?;
    }
    checked_add(&mut length, "]}".len())?;
    Some(length)
}

fn json_value_len(value: &Value) -> Option<usize> {
    match value {
        Value::Int64(value) => Some(value.to_string().len()),
        Value::Float64(value) => Some(Value::Float64(*value).as_display_string().len()),
        Value::Bool(value) => Some(value.to_string().len()),
        Value::String(value) => json_string_len(value),
    }
}

fn json_string_len(value: &str) -> Option<usize> {
    let mut length = 2_usize;
    for character in value.chars() {
        checked_add(
            &mut length,
            match character {
                '"' | '\\' | '\u{08}' | '\u{0c}' | '\n' | '\r' | '\t' => 2,
                value if value.is_control() => 6,
                value => value.len_utf8(),
            },
        )?;
    }
    Some(length)
}

fn csv_len(result: &QueryResult) -> Option<usize> {
    let mut length = csv_row_len(result.columns.iter().map(|column| column.name.as_str()))?;
    for row in &result.rows {
        let values = row.iter().map(Value::as_display_string).collect::<Vec<_>>();
        checked_add(&mut length, csv_row_len(values.iter().map(String::as_str))?)?;
    }
    Some(length)
}

fn csv_row_len<'a>(values: impl Iterator<Item = &'a str>) -> Option<usize> {
    let mut length = 1_usize;
    for (index, value) in values.enumerate() {
        checked_add(&mut length, usize::from(index > 0))?;
        if value.contains([',', '"', '\n', '\r']) {
            checked_add(&mut length, value.len())?;
            checked_add(
                &mut length,
                value.bytes().filter(|byte| *byte == b'"').count(),
            )?;
            checked_add(&mut length, 2)?;
        } else {
            checked_add(&mut length, value.len())?;
        }
    }
    Some(length)
}

/// Render all query results from a statement batch.
///
/// JSON is returned as one document. Non-JSON result sets are separated by a
/// blank line; command results do not produce output.
#[must_use]
pub fn render_results(results: &[QueryResult], format: OutputFormat) -> String {
    if format == OutputFormat::Json {
        let rendered = results
            .iter()
            .map(|result| render(result, format))
            .collect::<Vec<_>>()
            .join(",");
        return format!("{{\"results\":[{rendered}]}}\n");
    }

    let mut output = String::new();
    for (index, result) in results.iter().enumerate() {
        if index > 0 {
            output.push('\n');
        }
        let rendered = render(result, format);
        output.push_str(&rendered);
        if !rendered.ends_with('\n') {
            output.push('\n');
        }
    }
    output
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
    fn renders_multiple_results_as_one_json_document() {
        let result = result();
        let output = render_results(&[result.clone(), result], OutputFormat::Json);
        assert!(output.starts_with("{\"results\":[{"));
        assert_eq!(output.matches("\"columns\"").count(), 2);
        assert!(output.ends_with("]}\n"));
    }

    #[test]
    fn exact_lengths_match_json_and_csv_rendering() {
        let mut result = result();
        result.rows.push(vec![
            Value::Int64(-42),
            Value::String("quote: \"; controls: \n\u{08}; unicode: cafe\u{301}".to_owned()),
        ]);
        for format in [OutputFormat::Json, OutputFormat::Csv] {
            assert_eq!(
                rendered_len(&result, format),
                Some(render(&result, format).len())
            );
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
