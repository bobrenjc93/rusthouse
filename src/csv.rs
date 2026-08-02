//! Streaming CSV output for in-memory tables.

use std::io::{self, Write};

use crate::storage::{Table, ValueRef};

const NULL: &str = r"\N";

/// Writes a table as CSV without materializing rows or the complete output.
///
/// The first record contains column names and every subsequent record contains
/// one table row. Records end with `\n`. NULL is encoded as an unquoted `\N`,
/// booleans as `true` or `false`, and finite floats using Rust's shortest
/// round-trip decimal representation. A string equal to `\N` is quoted to
/// distinguish it from NULL.
///
/// Fields containing a comma, double quote, carriage return, or line feed are
/// enclosed in double quotes. Double quotes inside a quoted field are doubled.
/// Any error from `writer` is returned immediately.
///
/// ```
/// use rusthouse::{ColumnSchema, DataType, Schema, Table, Value, write_csv};
///
/// let schema = Schema::new(vec![
///     ColumnSchema::new("id", DataType::Int64, false),
///     ColumnSchema::new("label", DataType::String, false),
/// ])?;
/// let mut table = Table::new(schema);
/// table.insert_row(&[Value::Int64(1), Value::from("first")])?;
///
/// let mut output = Vec::new();
/// write_csv(&mut output, &table)?;
/// assert_eq!(output, b"id,label\n1,first\n");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn write_csv<W: Write + ?Sized>(writer: &mut W, table: &Table) -> io::Result<()> {
    for (index, column) in table.schema().columns().iter().enumerate() {
        write_separator(writer, index)?;
        write_text_field(writer, column.name())?;
    }
    writer.write_all(b"\n")?;

    for row in 0..table.len() {
        for (index, column) in table.columns().iter().enumerate() {
            write_separator(writer, index)?;
            let value = column
                .get(row)
                .expect("all table columns have the table row count");
            write_value(writer, value)?;
        }
        writer.write_all(b"\n")?;
    }

    Ok(())
}

fn write_separator<W: Write + ?Sized>(writer: &mut W, index: usize) -> io::Result<()> {
    if index > 0 {
        writer.write_all(b",")?;
    }
    Ok(())
}

fn write_value<W: Write + ?Sized>(writer: &mut W, value: ValueRef<'_>) -> io::Result<()> {
    match value {
        ValueRef::Null => writer.write_all(NULL.as_bytes()),
        ValueRef::Int64(value) => write!(writer, "{value}"),
        ValueRef::Float64(value) => write!(writer, "{value}"),
        ValueRef::Bool(value) => writer.write_all(if value { b"true" } else { b"false" }),
        ValueRef::String(value) => write_text_field(writer, value),
    }
}

fn write_text_field<W: Write + ?Sized>(writer: &mut W, field: &str) -> io::Result<()> {
    let bytes = field.as_bytes();
    let quoted = field == NULL
        || bytes
            .iter()
            .any(|byte| matches!(byte, b',' | b'"' | b'\r' | b'\n'));
    if !quoted {
        return writer.write_all(bytes);
    }

    writer.write_all(b"\"")?;
    let mut start = 0;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'"' {
            writer.write_all(&bytes[start..index])?;
            writer.write_all(b"\"\"")?;
            start = index + 1;
        }
    }
    writer.write_all(&bytes[start..])?;
    writer.write_all(b"\"")
}
