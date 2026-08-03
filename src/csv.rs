//! Streaming CSV output formats.

use crate::storage::{Column, Table};
use std::io::{self, Write};

/// Writes a table in ClickHouse's `CSVWithNames` shape.
///
/// The first record contains the table's field names. Headers and `String`
/// values are always double-quoted, with embedded double quotes doubled as
/// required by RFC 4180. Numeric and Boolean values are not quoted. Booleans
/// are written as `true` or `false`; finite floats use Rust's shortest
/// round-trippable display, signed zero is preserved, and non-finite floats
/// are written as `nan`, `inf`, or `-inf`.
///
/// Records end with `\n`, including the final record. An empty table therefore
/// emits only its header record. Output is written directly to `writer` and is
/// not buffered or flushed by this function. Any writer error is returned
/// immediately, and output already accepted by the writer is not rolled back.
///
/// # Examples
///
/// ```
/// use rusthouse::{DataType, Field, Table, Value, write_csv_with_names};
///
/// let mut table = Table::new(vec![
///     Field::new("id", DataType::Int64),
///     Field::new("label", DataType::String),
/// ])?;
/// table.insert_batch(vec![vec![Value::Int64(1), Value::from("north")]])?;
///
/// let mut csv = Vec::new();
/// write_csv_with_names(&table, &mut csv)?;
/// assert_eq!(csv, b"\"id\",\"label\"\n1,\"north\"\n");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn write_csv_with_names<W: Write + ?Sized>(table: &Table, writer: &mut W) -> io::Result<()> {
    write_header(table, writer)?;

    for row in 0..table.len() {
        for (index, column) in table.columns().iter().enumerate() {
            if index != 0 {
                writer.write_all(b",")?;
            }
            write_value(column, row, writer)?;
        }
        writer.write_all(b"\n")?;
    }

    Ok(())
}

fn write_header<W: Write + ?Sized>(table: &Table, writer: &mut W) -> io::Result<()> {
    for (index, field) in table.fields().iter().enumerate() {
        if index != 0 {
            writer.write_all(b",")?;
        }
        write_quoted(field.name(), writer)?;
    }
    writer.write_all(b"\n")
}

fn write_value<W: Write + ?Sized>(column: &Column, row: usize, writer: &mut W) -> io::Result<()> {
    match column {
        Column::Int64(values) => write!(writer, "{}", values[row]),
        Column::Float64(values) => write_float(values[row], writer),
        Column::Bool(values) => writer.write_all(if values[row] == 0 { b"false" } else { b"true" }),
        Column::String(values) => write_quoted(&values[row], writer),
    }
}

fn write_float<W: Write + ?Sized>(value: f64, writer: &mut W) -> io::Result<()> {
    if value.is_nan() {
        writer.write_all(b"nan")
    } else if value == f64::INFINITY {
        writer.write_all(b"inf")
    } else if value == f64::NEG_INFINITY {
        writer.write_all(b"-inf")
    } else {
        write!(writer, "{value}")
    }
}

fn write_quoted<W: Write + ?Sized>(value: &str, writer: &mut W) -> io::Result<()> {
    writer.write_all(b"\"")?;

    let mut remaining = value.as_bytes();
    while let Some(index) = remaining.iter().position(|byte| *byte == b'"') {
        writer.write_all(&remaining[..index])?;
        writer.write_all(b"\"\"")?;
        remaining = &remaining[index + 1..];
    }

    writer.write_all(remaining)?;
    writer.write_all(b"\"")
}
