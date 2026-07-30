use std::fmt::Write as _;
use std::io;

use crate::engine::{QueryResult, ResultColumn};
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
    let mut output = Vec::new();
    write_csv_header(&mut output, &result.columns).expect("writing to Vec cannot fail");
    write_csv_rows(&mut output, &result.rows).expect("writing to Vec cannot fail");
    String::from_utf8(output).expect("CSV renderer only writes UTF-8")
}

/// Write a CSV header without buffering the complete rendered result.
pub fn write_csv_header<W: io::Write + ?Sized>(
    output: &mut W,
    columns: &[ResultColumn],
) -> io::Result<()> {
    write_csv_row(output, columns.iter().map(|column| column.name.as_str()))
}

/// Write positional CSV rows without buffering the complete rendered result.
pub fn write_csv_rows<W: io::Write + ?Sized>(
    output: &mut W,
    rows: &[Vec<Value>],
) -> io::Result<()> {
    for row in rows {
        for (index, value) in row.iter().enumerate() {
            if index > 0 {
                output.write_all(b",")?;
            }
            if let Value::String(value) = value {
                write_csv_field(output, value)?;
            } else {
                write_csv_field(output, &value.as_display_string())?;
            }
        }
        output.write_all(b"\n")?;
    }
    Ok(())
}

fn write_csv_row<'a, W: io::Write + ?Sized>(
    output: &mut W,
    values: impl Iterator<Item = &'a str>,
) -> io::Result<()> {
    for (index, value) in values.enumerate() {
        if index > 0 {
            output.write_all(b",")?;
        }
        write_csv_field(output, value)?;
    }
    output.write_all(b"\n")
}

fn write_csv_field<W: io::Write + ?Sized>(output: &mut W, value: &str) -> io::Result<()> {
    if !value.contains([',', '"', '\n', '\r']) {
        return output.write_all(value.as_bytes());
    }

    output.write_all(b"\"")?;
    let mut remaining = value;
    while let Some(position) = remaining.find('"') {
        output.write_all(&remaining.as_bytes()[..position])?;
        output.write_all(b"\"\"")?;
        remaining = &remaining[position + 1..];
    }
    output.write_all(remaining.as_bytes())?;
    output.write_all(b"\"")
}

/// Render one result set with explicit column metadata and positional rows.
///
/// Positional rows preserve every value even when output column names repeat.
fn render_json(result: &QueryResult) -> String {
    let mut output = Vec::new();
    write_json_query_start(&mut output, &result.columns).expect("writing to Vec cannot fail");
    let mut first_row = true;
    write_json_rows(&mut output, &result.rows, &mut first_row).expect("writing to Vec cannot fail");
    write_json_query_end(&mut output).expect("writing to Vec cannot fail");
    String::from_utf8(output).expect("JSON renderer only writes UTF-8")
}

/// Write JSON query metadata and open its positional row array.
pub fn write_json_query_start<W: io::Write + ?Sized>(
    output: &mut W,
    columns: &[ResultColumn],
) -> io::Result<()> {
    output.write_all(b"{\"columns\":[")?;
    for (index, column) in columns.iter().enumerate() {
        if index > 0 {
            output.write_all(b",")?;
        }
        output.write_all(b"{\"name\":")?;
        write_json_string(output, &column.name)?;
        output.write_all(b",\"type\":")?;
        write_json_string(output, &column.data_type.to_string())?;
        output.write_all(b"}")?;
    }
    output.write_all(b"],\"rows\":[")
}

/// Write JSON rows, preserving comma state across calls.
pub fn write_json_rows<W: io::Write + ?Sized>(
    output: &mut W,
    rows: &[Vec<Value>],
    first_row: &mut bool,
) -> io::Result<()> {
    for row in rows {
        if !*first_row {
            output.write_all(b",")?;
        }
        *first_row = false;
        output.write_all(b"[")?;
        for (column_index, value) in row.iter().enumerate() {
            if column_index > 0 {
                output.write_all(b",")?;
            }
            write_json_value(output, value)?;
        }
        output.write_all(b"]")?;
    }
    Ok(())
}

/// Close the row array and query object opened by [`write_json_query_start`].
pub fn write_json_query_end<W: io::Write + ?Sized>(output: &mut W) -> io::Result<()> {
    output.write_all(b"]}")
}

fn write_json_value<W: io::Write + ?Sized>(output: &mut W, value: &Value) -> io::Result<()> {
    match value {
        Value::Int64(value) => write!(output, "{value}"),
        Value::Float64(value) => {
            output.write_all(Value::Float64(*value).as_display_string().as_bytes())
        }
        Value::Bool(value) => write!(output, "{value}"),
        Value::String(value) => write_json_string(output, value),
    }
}

fn write_json_string<W: io::Write + ?Sized>(output: &mut W, value: &str) -> io::Result<()> {
    output.write_all(b"\"")?;
    for character in value.chars() {
        match character {
            '"' => output.write_all(b"\\\"")?,
            '\\' => output.write_all(b"\\\\")?,
            '\u{08}' => output.write_all(b"\\b")?,
            '\u{0c}' => output.write_all(b"\\f")?,
            '\n' => output.write_all(b"\\n")?,
            '\r' => output.write_all(b"\\r")?,
            '\t' => output.write_all(b"\\t")?,
            value if value.is_control() => {
                write!(output, "\\u{:04x}", value as u32)?;
            }
            value => {
                let mut encoded = [0; 4];
                output.write_all(value.encode_utf8(&mut encoded).as_bytes())?;
            }
        }
    }
    output.write_all(b"\"")
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
