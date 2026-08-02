use std::collections::HashSet;
use std::fmt::Write as _;

use crate::error::{Error, Result};
use crate::{QueryResult, Value};

#[cfg(not(test))]
const MAX_ENCODED_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
#[cfg(test)]
const MAX_ENCODED_OUTPUT_BYTES: usize = 1024 * 1024;

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

/// Encodes one query result. CSV starts with an escaped column-name row; JSON
/// is an array of objects whose keys are the projected column names.
pub fn render(result: &QueryResult, format: OutputFormat) -> Result<String> {
    let size = encoded_size(result, format)?;
    Ok(match format {
        OutputFormat::Csv => render_csv(result, size),
        OutputFormat::Json => render_json(result, size),
    })
}

fn render_csv(result: &QueryResult, capacity: usize) -> String {
    let mut output = String::with_capacity(capacity);
    for (index, name) in result.columns.iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        csv_field(&mut output, name);
    }
    output.push('\n');
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

fn render_json(result: &QueryResult, capacity: usize) -> String {
    let mut output = String::with_capacity(capacity);
    output.push('[');
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

fn encoded_size(result: &QueryResult, format: OutputFormat) -> Result<usize> {
    for row in &result.rows {
        if row.len() != result.columns.len() {
            return Err(Error::new(format!(
                "result row has {} values but {} columns were expected",
                row.len(),
                result.columns.len()
            )));
        }
    }
    let size = match format {
        OutputFormat::Csv => csv_size(result)?,
        OutputFormat::Json => json_size(result)?,
    };
    if size > MAX_ENCODED_OUTPUT_BYTES {
        Err(Error::new(format!(
            "encoded output limit exceeded (maximum {MAX_ENCODED_OUTPUT_BYTES} bytes)"
        )))
    } else {
        Ok(size)
    }
}

fn csv_size(result: &QueryResult) -> Result<usize> {
    let mut total = result.columns.len().saturating_sub(1) + 1;
    for name in &result.columns {
        total = checked_output_add(total, csv_text_size(name))?;
    }
    result.rows.iter().try_fold(total, |total, row| {
        let separators = row.len().saturating_sub(1) + 1;
        let total = checked_output_add(total, separators)?;
        row.iter().try_fold(total, |total, value| {
            checked_output_add(total, csv_value_size(value))
        })
    })
}

fn csv_value_size(value: &Value) -> usize {
    match value {
        Value::Null => 2,
        Value::Int64(value) => value.to_string().len(),
        Value::Float64(value) => value.to_string().len(),
        Value::Bool(value) => {
            if *value {
                4
            } else {
                5
            }
        }
        Value::String(value) => csv_text_size(value),
    }
}

fn csv_text_size(value: &str) -> usize {
    if value.contains([',', '"', '\n', '\r']) || value == "\\N" {
        2 + value.len() + value.bytes().filter(|byte| *byte == b'"').count()
    } else {
        value.len()
    }
}

fn json_size(result: &QueryResult) -> Result<usize> {
    let mut names = HashSet::new();
    let mut row_syntax = 2usize;
    for (index, name) in result.columns.iter().enumerate() {
        if !names.insert(name.as_str()) {
            return Err(Error::new(format!(
                "JSON output requires unique column names; duplicate '{name}'"
            )));
        }
        if index != 0 {
            row_syntax = checked_output_add(row_syntax, 1)?;
        }
        row_syntax = checked_output_add(row_syntax, json_string_size(name))?;
        row_syntax = checked_output_add(row_syntax, 1)?;
    }

    let mut total = 3usize;
    if !result.rows.is_empty() {
        total = checked_output_add(total, result.rows.len() - 1)?;
    }
    total = checked_output_add(
        total,
        row_syntax
            .checked_mul(result.rows.len())
            .ok_or_else(output_size_error)?,
    )?;
    for row in &result.rows {
        for value in row {
            total = checked_output_add(total, json_value_size(value)?)?;
        }
    }
    Ok(total)
}

fn json_value_size(value: &Value) -> Result<usize> {
    Ok(match value {
        Value::Null => 4,
        Value::Int64(value) => value.to_string().len(),
        Value::Float64(value) if value.is_finite() => value.to_string().len(),
        Value::Float64(_) => {
            return Err(Error::new(
                "JSON output cannot encode non-finite Float64 values",
            ));
        }
        Value::Bool(value) => {
            if *value {
                4
            } else {
                5
            }
        }
        Value::String(value) => json_string_size(value),
    })
}

fn json_string_size(value: &str) -> usize {
    2 + value
        .chars()
        .map(|character| match character {
            '"' | '\\' | '\n' | '\r' | '\t' | '\u{08}' | '\u{0c}' => 2,
            control if control <= '\u{1f}' => 6,
            other => other.len_utf8(),
        })
        .sum::<usize>()
}

fn checked_output_add(left: usize, right: usize) -> Result<usize> {
    left.checked_add(right).ok_or_else(output_size_error)
}

fn output_size_error() -> Error {
    Error::new("encoded output size overflow")
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
    fn renders_rfc_style_csv_with_a_header() {
        assert_eq!(
            render(&result(), OutputFormat::Csv).unwrap(),
            "name,value,valid\n\"a,\"\"b\",1.5,true\n\"line\n2\",\\N,false\n"
        );
    }

    #[test]
    fn escapes_csv_column_names() {
        let result = QueryResult {
            columns: vec!["a,b".to_owned(), "quote\"name".to_owned()],
            rows: Vec::new(),
        };
        assert_eq!(
            render(&result, OutputFormat::Csv).unwrap(),
            "\"a,b\",\"quote\"\"name\"\n"
        );
    }

    #[test]
    fn renders_typed_json_objects() {
        assert_eq!(
            render(&result(), OutputFormat::Json).unwrap(),
            "[{\"name\":\"a,\\\"b\",\"value\":1.5,\"valid\":true},{\"name\":\"line\\n2\",\"value\":null,\"valid\":false}]\n"
        );
    }

    #[test]
    fn rejects_duplicate_json_keys() {
        let result = QueryResult {
            columns: vec!["x".to_owned(), "x".to_owned()],
            rows: vec![vec![Value::Int64(1), Value::Int64(2)]],
        };
        let error = render(&result, OutputFormat::Json).unwrap_err();
        assert!(error.message().contains("duplicate 'x'"));
        assert!(render(&result, OutputFormat::Csv).is_ok());
    }

    #[test]
    fn accounts_for_repeated_json_column_names_before_allocating() {
        let result = QueryResult {
            columns: vec!["x".repeat(64 * 1024)],
            rows: vec![vec![Value::Int64(1)]; 17],
        };
        let error = render(&result, OutputFormat::Json).unwrap_err();
        assert!(error.message().contains("encoded output limit"));
    }

    #[test]
    fn rejects_non_finite_public_values_in_json() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let result = QueryResult {
                columns: vec!["value".to_owned()],
                rows: vec![vec![Value::Float64(value)]],
            };
            let error = render(&result, OutputFormat::Json).unwrap_err();
            assert!(error.message().contains("non-finite Float64"));
            assert!(render(&result, OutputFormat::Csv).is_ok());
        }
    }
}
