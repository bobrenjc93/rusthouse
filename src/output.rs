use crate::QueryResult;
use std::io::{self, Write};
use unicode_width::UnicodeWidthStr;

/// Supported result encodings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputFormat {
    #[default]
    Table,
    Csv,
}

/// Writes all query results, separated by one blank line.
pub fn write_results<W: Write>(
    results: &[QueryResult],
    format: OutputFormat,
    mut writer: W,
) -> io::Result<()> {
    for (index, result) in results.iter().enumerate() {
        if index > 0 {
            writer.write_all(b"\n")?;
        }
        match format {
            OutputFormat::Table => write_table(result, &mut writer)?,
            OutputFormat::Csv => write_csv(result, &mut writer)?,
        }
    }
    Ok(())
}

fn write_csv<W: Write>(result: &QueryResult, writer: &mut W) -> io::Result<()> {
    write_csv_row(
        result.columns.iter().map(|column| column.name.as_str()),
        writer,
    )?;
    let values: Vec<String> = result
        .columns
        .iter()
        .map(|column| column.value.to_string())
        .collect();
    write_csv_row(values.iter().map(String::as_str), writer)
}

fn write_csv_row<'a, W, I>(fields: I, writer: &mut W) -> io::Result<()>
where
    W: Write,
    I: IntoIterator<Item = &'a str>,
{
    for (index, field) in fields.into_iter().enumerate() {
        if index > 0 {
            writer.write_all(b",")?;
        }
        write_csv_field(field, writer)?;
    }
    writer.write_all(b"\n")
}

fn write_csv_field<W: Write>(field: &str, writer: &mut W) -> io::Result<()> {
    if field.contains([',', '"', '\n', '\r']) {
        writer.write_all(b"\"")?;
        for (index, part) in field.split('"').enumerate() {
            if index > 0 {
                writer.write_all(b"\"\"")?;
            }
            writer.write_all(part.as_bytes())?;
        }
        writer.write_all(b"\"")
    } else {
        writer.write_all(field.as_bytes())
    }
}

fn write_table<W: Write>(result: &QueryResult, writer: &mut W) -> io::Result<()> {
    let names: Vec<String> = result
        .columns
        .iter()
        .map(|column| escape_table_field(&column.name))
        .collect();
    let values: Vec<String> = result
        .columns
        .iter()
        .map(|column| escape_table_field(&column.value.to_string()))
        .collect();
    let widths: Vec<usize> = names
        .iter()
        .zip(&values)
        .map(|(name, value)| display_width(name).max(display_width(value)))
        .collect();

    write_table_row(names.iter().map(String::as_str), &widths, writer)?;
    for (index, width) in widths.iter().enumerate() {
        if index > 0 {
            writer.write_all(b"-+-")?;
        }
        writer.write_all("-".repeat(*width).as_bytes())?;
    }
    writer.write_all(b"\n")?;
    write_table_row(values.iter().map(String::as_str), &widths, writer)
}

fn escape_table_field(field: &str) -> String {
    let mut escaped = String::with_capacity(field.len());
    for character in field.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            value if value.is_control() => escaped.extend(value.escape_unicode()),
            value => escaped.push(value),
        }
    }
    escaped
}

fn write_table_row<'a, W, I>(fields: I, widths: &[usize], writer: &mut W) -> io::Result<()>
where
    W: Write,
    I: IntoIterator<Item = &'a str>,
{
    for (index, (field, width)) in fields.into_iter().zip(widths).enumerate() {
        if index > 0 {
            writer.write_all(b" | ")?;
        }
        writer.write_all(field.as_bytes())?;
        writer.write_all(
            " ".repeat(width.saturating_sub(display_width(field)))
                .as_bytes(),
        )?;
    }
    writer.write_all(b"\n")
}

fn display_width(value: &str) -> usize {
    UnicodeWidthStr::width(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ResultColumn, Value};

    #[test]
    fn csv_quotes_delimiters_quotes_and_line_breaks() {
        let results = vec![QueryResult {
            columns: vec![ResultColumn {
                name: "message,text".to_owned(),
                value: Value::String("say \"hello\"\nagain".to_owned()),
            }],
        }];
        let mut output = Vec::new();

        write_results(&results, OutputFormat::Csv, &mut output).unwrap();

        assert_eq!(
            String::from_utf8(output).unwrap(),
            "\"message,text\"\n\"say \"\"hello\"\"\nagain\"\n"
        );
    }

    #[test]
    fn table_escapes_control_characters_in_names_and_values() {
        let results = vec![QueryResult {
            columns: vec![ResultColumn {
                name: "line\nname".to_owned(),
                value: Value::String("first\nsecond".to_owned()),
            }],
        }];
        let mut output = Vec::new();

        write_results(&results, OutputFormat::Table, &mut output).unwrap();

        assert_eq!(
            String::from_utf8(output).unwrap(),
            "line\\nname   \n-------------\nfirst\\nsecond\n"
        );
    }

    #[test]
    fn table_aligns_wide_and_combining_characters() {
        let results = vec![QueryResult {
            columns: vec![
                ResultColumn {
                    name: "表".to_owned(),
                    value: Value::String("x".to_owned()),
                },
                ResultColumn {
                    name: "e\u{301}".to_owned(),
                    value: Value::String("xx".to_owned()),
                },
            ],
        }];
        let mut output = Vec::new();

        write_results(&results, OutputFormat::Table, &mut output).unwrap();

        assert_eq!(
            String::from_utf8(output).unwrap(),
            "表 | e\u{301} \n---+---\nx  | xx\n"
        );
    }
}
