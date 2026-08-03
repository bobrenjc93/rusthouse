//! Streaming CSV output formats.

use crate::SelectResult;
use crate::storage::{Column, Table, Value};
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

/// Writes a borrowed [`SelectResult`] in ClickHouse's `CSVWithNames` shape.
///
/// Projected fields are emitted in statement order, including duplicate
/// projections. Only selected rows are written, in result order. A scalar
/// aggregate list emits one field per aggregate and one data row, unless `LIMIT
/// 0` suppresses the data row. Grouped counts emit their owned key/count rows
/// in deterministic key order. Source columns and their values are otherwise
/// borrowed directly; this function does not build a result table or copy
/// selected values.
///
/// Encoding, record termination, and writer-error behavior match
/// [`write_csv_with_names`]. A result with no selected rows emits only its
/// projected header record.
///
/// # Examples
///
/// ```
/// use rusthouse::{Catalog, write_select_csv_with_names};
///
/// let mut catalog = Catalog::new();
/// catalog.execute_create("CREATE TABLE events (id Int64, label String)")?;
/// catalog.execute_insert("INSERT INTO events VALUES (1, 'north'), (2, 'south')")?;
/// let result = catalog.execute_select("SELECT label, id FROM events WHERE id = 2")?;
///
/// let mut csv = Vec::new();
/// write_select_csv_with_names(&result, &mut csv)?;
/// assert_eq!(csv, b"\"label\",\"id\"\n\"south\",2\n");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn write_select_csv_with_names<W: Write + ?Sized>(
    result: &SelectResult<'_>,
    writer: &mut W,
) -> io::Result<()> {
    for (index, field) in result.projected_fields().enumerate() {
        if index != 0 {
            writer.write_all(b",")?;
        }
        write_quoted(field.name(), writer)?;
    }
    writer.write_all(b"\n")?;

    if result.is_grouped() {
        for (key, count) in result.grouped_rows() {
            write_scalar_value(key, writer)?;
            writer.write_all(b",")?;
            write!(writer, "{count}")?;
            writer.write_all(b"\n")?;
        }
        return Ok(());
    }

    if result.is_scalar() {
        if !result.is_empty() {
            for (index, value) in result.scalar_values().enumerate() {
                if index != 0 {
                    writer.write_all(b",")?;
                }
                write_scalar_value(value, writer)?;
            }
            writer.write_all(b"\n")?;
        }
        return Ok(());
    }

    for row in result.selected_rows() {
        for (index, column) in result.projected_columns().enumerate() {
            if index != 0 {
                writer.write_all(b",")?;
            }
            write_value(column, row, writer)?;
        }
        writer.write_all(b"\n")?;
    }

    Ok(())
}

fn write_scalar_value<W: Write + ?Sized>(value: &Value, writer: &mut W) -> io::Result<()> {
    match value {
        Value::Int64(value) => write!(writer, "{value}"),
        Value::Float64(value) => write_float(*value, writer),
        Value::Bool(value) => writer.write_all(if *value { b"true" } else { b"false" }),
        Value::String(value) => write_quoted(value, writer),
    }
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
