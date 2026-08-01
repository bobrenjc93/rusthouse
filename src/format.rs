use std::fmt::Write as _;

use crate::{QueryResult, Value};

/// A supported command-line result encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputFormat {
    Csv,
    Json,
}

impl OutputFormat {
    /// Parses the value accepted by `--format`.
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "csv" => Some(Self::Csv),
            "json" => Some(Self::Json),
            _ => None,
        }
    }
}

/// Encodes one query result. CSV has no header row; JSON is an array of
/// objects whose keys are the projected column names.
pub fn render(result: &QueryResult, format: OutputFormat) -> String {
    match format {
        OutputFormat::Csv => render_csv(result),
        OutputFormat::Json => render_json(result),
    }
}

fn render_csv(result: &QueryResult) -> String {
    let mut output = String::new();
    for row in &result.rows {
        for (index, value) in row.iter().enumerate() {
            if index != 0 {
                output.push(',');
            }
            match value {
                Value::Null => output.push_str("\\N"),
                Value::Int64(value) => write!(output, "{value}").expect("write to String"),
                Value::Float64(value) => write!(output, "{value}").expect("write to String"),
                Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
                Value::String(value) => csv_field(&mut output, value),
            }
        }
        output.push('\n');
    }
    output
}

fn csv_field(output: &mut String, value: &str) {
    if value.contains([',', '"', '\n', '\r']) || value == "\\N" {
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

fn render_json(result: &QueryResult) -> String {
    let mut output = String::from("[");
    for (row_index, row) in result.rows.iter().enumerate() {
        if row_index != 0 {
            output.push(',');
        }
        output.push('{');
        for (column_index, (name, value)) in result.columns.iter().zip(row).enumerate() {
            if column_index != 0 {
                output.push(',');
            }
            json_string(&mut output, name);
            output.push(':');
            json_value(&mut output, value);
        }
        output.push('}');
    }
    output.push_str("]\n");
    output
}

fn json_value(output: &mut String, value: &Value) {
    match value {
        Value::Null => output.push_str("null"),
        Value::Int64(value) => write!(output, "{value}").expect("write to String"),
        Value::Float64(value) => write!(output, "{value}").expect("write to String"),
        Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        Value::String(value) => json_string(output, value),
    }
}

fn json_string(output: &mut String, value: &str) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            '\u{08}' => output.push_str("\\b"),
            '\u{0c}' => output.push_str("\\f"),
            control if control <= '\u{1f}' => {
                write!(output, "\\u{:04x}", control as u32).expect("write to String");
            }
            other => output.push(other),
        }
    }
    output.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result() -> QueryResult {
        QueryResult {
            columns: vec!["name".to_owned(), "value".to_owned(), "valid".to_owned()],
            rows: vec![
                vec![
                    Value::String("a,\"b".to_owned()),
                    Value::Float64(1.5),
                    Value::Bool(true),
                ],
                vec![
                    Value::String("line\n2".to_owned()),
                    Value::Null,
                    Value::Bool(false),
                ],
            ],
        }
    }

    #[test]
    fn renders_rfc_style_csv_without_a_header() {
        assert_eq!(
            render(&result(), OutputFormat::Csv),
            "\"a,\"\"b\",1.5,true\n\"line\n2\",\\N,false\n"
        );
    }

    #[test]
    fn renders_typed_json_objects() {
        assert_eq!(
            render(&result(), OutputFormat::Json),
            "[{\"name\":\"a,\\\"b\",\"value\":1.5,\"valid\":true},{\"name\":\"line\\n2\",\"value\":null,\"valid\":false}]\n"
        );
    }
}
