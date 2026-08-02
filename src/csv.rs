//! Streaming CSV export for in-memory tables.

use std::io::{self, Write};

use crate::{Column, Table};

/// Writes a table as RFC 4180 CSV without materializing row-oriented data.
///
/// The schema header is written first, followed by rows in insertion order.
/// Records use CRLF line endings, and fields containing commas, quotes, CR, or
/// LF are quoted with embedded quotes doubled.
///
/// # Errors
///
/// Returns the first error reported by `writer`.
pub fn write_csv<W: Write + ?Sized>(table: &Table, writer: &mut W) -> io::Result<()> {
    for (index, column) in table.schema().columns().iter().enumerate() {
        write_separator(index, writer)?;
        write_field(column.name(), writer)?;
    }
    writer.write_all(b"\r\n")?;

    for row_index in 0..table.row_count() {
        for (column_index, column) in table.columns().iter().enumerate() {
            write_separator(column_index, writer)?;
            match column {
                Column::Int64(values) => write!(writer, "{}", values[row_index])?,
                Column::Float64(values) => write!(writer, "{}", values[row_index])?,
                Column::Bool(values) => write!(writer, "{}", values[row_index])?,
                Column::String(values) => write_field(&values[row_index], writer)?,
            }
        }
        writer.write_all(b"\r\n")?;
    }

    Ok(())
}

fn write_separator<W: Write + ?Sized>(field_index: usize, writer: &mut W) -> io::Result<()> {
    if field_index > 0 {
        writer.write_all(b",")?;
    }
    Ok(())
}

fn write_field<W: Write + ?Sized>(field: &str, writer: &mut W) -> io::Result<()> {
    if !field
        .as_bytes()
        .iter()
        .any(|byte| matches!(byte, b',' | b'"' | b'\r' | b'\n'))
    {
        return writer.write_all(field.as_bytes());
    }

    writer.write_all(b"\"")?;
    let mut chunk_start = 0;
    for (index, byte) in field.bytes().enumerate() {
        if byte == b'"' {
            writer.write_all(&field.as_bytes()[chunk_start..index])?;
            writer.write_all(b"\"\"")?;
            chunk_start = index + 1;
        }
    }
    writer.write_all(&field.as_bytes()[chunk_start..])?;
    writer.write_all(b"\"")
}
