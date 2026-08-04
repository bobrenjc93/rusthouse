use std::fmt::Write;
use std::io;

use crate::batch::engine::QueryResult;
use crate::batch::value::Value;

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
    write_csv(&mut output, result).expect("writing CSV to a Vec cannot fail");
    String::from_utf8(output).expect("CSV rendering preserves UTF-8")
}

/// Streams one CSVWithNames result without retaining a formatted output copy.
pub fn write_csv(output: &mut impl io::Write, result: &QueryResult) -> io::Result<()> {
    for (index, column) in result.columns.iter().enumerate() {
        if index > 0 {
            output.write_all(b",")?;
        }
        write_csv_field(output, &column.name)?;
    }
    output.write_all(b"\n")?;

    for row in &result.rows {
        for (index, value) in row.iter().enumerate() {
            if index > 0 {
                output.write_all(b",")?;
            }
            match value {
                Value::String(value) => write_csv_field(output, value)?,
                Value::Null(_) => output.write_all(b"NULL")?,
                value => write_csv_field(output, &value.as_display_string())?,
            }
        }
        output.write_all(b"\n")?;
    }
    Ok(())
}

fn write_csv_field(output: &mut impl io::Write, value: &str) -> io::Result<()> {
    if !value.contains([',', '"', '\n', '\r']) {
        return output.write_all(value.as_bytes());
    }

    output.write_all(b"\"")?;
    for (index, segment) in value.split('"').enumerate() {
        if index > 0 {
            output.write_all(b"\"\"")?;
        }
        output.write_all(segment.as_bytes())?;
    }
    output.write_all(b"\"")
}

fn render_json(result: &QueryResult) -> String {
    let mut output = Vec::new();
    write_json(&mut output, result).expect("writing JSON to a Vec cannot fail");
    String::from_utf8(output).expect("JSON rendering preserves UTF-8")
}

/// Streams one JSON result with explicit column metadata and positional rows.
///
/// Positional rows preserve every value even when output column names repeat.
pub fn write_json(output: &mut impl io::Write, result: &QueryResult) -> io::Result<()> {
    output.write_all(b"{\"columns\":[")?;
    for (index, column) in result.columns.iter().enumerate() {
        if index > 0 {
            output.write_all(b",")?;
        }
        output.write_all(b"{\"name\":")?;
        write_json_string(output, &column.name)?;
        output.write_all(b",\"type\":")?;
        write_json_string(output, &column.data_type.to_string())?;
        output.write_all(b"}")?;
    }
    output.write_all(b"],\"rows\":[")?;
    for (row_index, row) in result.rows.iter().enumerate() {
        if row_index > 0 {
            output.write_all(b",")?;
        }
        output.write_all(b"[")?;
        for (column_index, value) in row.iter().enumerate() {
            if column_index > 0 {
                output.write_all(b",")?;
            }
            write_json_value(output, value)?;
        }
        output.write_all(b"]")?;
    }
    output.write_all(b"]}")
}

fn write_json_value(output: &mut impl io::Write, value: &Value) -> io::Result<()> {
    match value {
        Value::Null(_) => output.write_all(b"null"),
        Value::Int64(value) => write!(output, "{value}"),
        Value::Float64(value) => {
            output.write_all(Value::Float64(*value).as_display_string().as_bytes())
        }
        Value::Bool(value) => write!(output, "{value}"),
        Value::String(value) => write_json_string(output, value),
    }
}

fn write_json_string(output: &mut impl io::Write, value: &str) -> io::Result<()> {
    output.write_all(b"\"")?;
    let mut unescaped_start = 0;
    for (index, character) in value.char_indices() {
        let escaped = match character {
            '"' => Some(r#"\""#),
            '\\' => Some(r"\\"),
            '\u{08}' => Some(r"\b"),
            '\u{0c}' => Some(r"\f"),
            '\n' => Some(r"\n"),
            '\r' => Some(r"\r"),
            '\t' => Some(r"\t"),
            character if character.is_control() => {
                output.write_all(value[unescaped_start..index].as_bytes())?;
                write!(output, "\\u{:04x}", character as u32)?;
                unescaped_start = index + character.len_utf8();
                continue;
            }
            _ => None,
        };

        if let Some(escaped) = escaped {
            output.write_all(value[unescaped_start..index].as_bytes())?;
            output.write_all(escaped.as_bytes())?;
            unescaped_start = index + character.len_utf8();
        }
    }
    output.write_all(value[unescaped_start..].as_bytes())?;
    output.write_all(b"\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::engine::ResultColumn;
    use crate::batch::value::DataType;

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
    fn streams_all_json_value_types_and_escaping() {
        let result = QueryResult {
            columns: vec![
                ResultColumn {
                    name: "null".to_owned(),
                    data_type: DataType::String,
                },
                ResultColumn {
                    name: "integer".to_owned(),
                    data_type: DataType::Int64,
                },
                ResultColumn {
                    name: "float".to_owned(),
                    data_type: DataType::Float64,
                },
                ResultColumn {
                    name: "boolean".to_owned(),
                    data_type: DataType::Bool,
                },
                ResultColumn {
                    name: "text\"\n".to_owned(),
                    data_type: DataType::String,
                },
            ],
            rows: vec![vec![
                Value::Null(DataType::String),
                Value::Int64(i64::MIN),
                Value::Float64(2.0),
                Value::Bool(false),
                Value::String("\"\\\u{08}\u{0c}\n\r\t\u{01}雪".to_owned()),
            ]],
        };
        let expected = r#"{"columns":[{"name":"null","type":"String"},{"name":"integer","type":"Int64"},{"name":"float","type":"Float64"},{"name":"boolean","type":"Bool"},{"name":"text\"\n","type":"String"}],"rows":[[null,-9223372036854775808,2.0,false,"\"\\\b\f\n\r\t\u0001雪"]]}"#;
        let mut output = Vec::new();

        write_json(&mut output, &result).expect("Vec accepts streamed JSON");

        assert_eq!(output, expected.as_bytes());
        assert_eq!(render(&result, OutputFormat::Json), expected);
    }

    #[test]
    fn renders_typed_nulls_in_csv_table_and_json_formats() {
        let result = QueryResult {
            columns: vec![ResultColumn {
                name: "total".to_owned(),
                data_type: DataType::Int64,
            }],
            rows: vec![vec![Value::Null(DataType::Int64)]],
        };

        assert_eq!(render(&result, OutputFormat::Csv), "total\nNULL\n");
        assert!(render(&result, OutputFormat::Table).contains("| NULL  |"));
        assert_eq!(
            render(&result, OutputFormat::Json),
            r#"{"columns":[{"name":"total","type":"Int64"}],"rows":[[null]]}"#
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
        let expected = r#"{"columns":[{"name":"id","type":"Int64"},{"name":"id","type":"String"}],"rows":[[1,"x"]]}"#;
        let mut output = Vec::new();

        write_json(&mut output, &result).expect("Vec accepts streamed JSON");

        assert_eq!(output, expected.as_bytes());
        assert_eq!(render(&result, OutputFormat::Json), expected);
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
