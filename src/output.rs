use crate::QueryResult;
use std::io::{self, Write};

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
        .map(|column| column.value.to_output_string())
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
    let values: Vec<String> = result
        .columns
        .iter()
        .map(|column| column.value.to_output_string())
        .collect();
    let widths: Vec<usize> = result
        .columns
        .iter()
        .zip(&values)
        .map(|(column, value)| column.name.chars().count().max(value.chars().count()))
        .collect();

    write_table_row(
        result.columns.iter().map(|column| column.name.as_str()),
        &widths,
        writer,
    )?;
    for (index, width) in widths.iter().enumerate() {
        if index > 0 {
            writer.write_all(b"-+-")?;
        }
        writer.write_all("-".repeat(*width).as_bytes())?;
    }
    writer.write_all(b"\n")?;
    write_table_row(values.iter().map(String::as_str), &widths, writer)
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
            " ".repeat(width.saturating_sub(field.chars().count()))
                .as_bytes(),
        )?;
    }
    writer.write_all(b"\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Column, ScalarValue};

    #[test]
    fn csv_quotes_delimiters_quotes_and_line_breaks() {
        let results = vec![QueryResult {
            columns: vec![Column {
                name: "message,text".to_owned(),
                value: ScalarValue::String("say \"hello\"\nagain".to_owned()),
            }],
        }];
        let mut output = Vec::new();

        write_results(&results, OutputFormat::Csv, &mut output).unwrap();

        assert_eq!(
            String::from_utf8(output).unwrap(),
            "\"message,text\"\n\"say \"\"hello\"\"\nagain\"\n"
        );
    }
}
