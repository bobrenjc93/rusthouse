use std::io::{self, Write};

use crate::QueryResult;

/// Writes a query result as RFC 4180-style CSV using `\n` record endings.
///
/// Headers and values containing commas, quotes, carriage returns, or newlines
/// are quoted, and embedded quotes are doubled.
pub fn write_csv<W: Write>(result: &QueryResult, mut writer: W) -> io::Result<()> {
    write_record(
        &mut writer,
        result.columns.iter().map(|column| column.name.as_str()),
    )?;
    for row in &result.rows {
        let values: Vec<String> = row.iter().map(|value| value.display_value()).collect();
        write_record(&mut writer, values.iter().map(String::as_str))?;
    }
    Ok(())
}

fn write_record<'a, W, I>(writer: &mut W, fields: I) -> io::Result<()>
where
    W: Write,
    I: IntoIterator<Item = &'a str>,
{
    for (index, field) in fields.into_iter().enumerate() {
        if index > 0 {
            writer.write_all(b",")?;
        }
        write_field(writer, field)?;
    }
    writer.write_all(b"\n")
}

fn write_field(writer: &mut impl Write, field: &str) -> io::Result<()> {
    if !field
        .bytes()
        .any(|byte| matches!(byte, b',' | b'"' | b'\r' | b'\n'))
    {
        return writer.write_all(field.as_bytes());
    }

    writer.write_all(b"\"")?;
    let mut remainder = field;
    while let Some(index) = remainder.find('"') {
        writer.write_all(&remainder.as_bytes()[..index])?;
        writer.write_all(b"\"\"")?;
        remainder = &remainder[index + 1..];
    }
    writer.write_all(remainder.as_bytes())?;
    writer.write_all(b"\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execute;

    #[test]
    fn escapes_headers_and_values() {
        let result = execute("SELECT 'a,\"b\"\n' AS \"heading, \"\"one\"\"\"").unwrap();
        let mut output = Vec::new();
        write_csv(&result, &mut output).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "\"heading, \"\"one\"\"\"\n\"a,\"\"b\"\"\n\"\n"
        );
    }
}
