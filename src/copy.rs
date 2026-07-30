use std::fs::File;
use std::io::{BufRead, BufReader};
use std::str;

use csv_core::{ReadRecordResult, Reader};

use crate::error::{Error, Result};
use crate::storage::{ColumnDef, Table};
use crate::value::{DataType, Value};

const BATCH_ROWS: usize = 1_024;
const MAX_FIELD_BYTES: usize = 1024 * 1024;
const MAX_RECORD_BYTES: usize = 8 * 1024 * 1024;

pub(crate) fn copy_csv(table: &mut Table, path: &str, header: bool) -> Result<usize> {
    let checkpoint = table.checkpoint();
    match copy_csv_inner(table, path, header) {
        Ok(affected_rows) => Ok(affected_rows),
        Err(error) => {
            table.restore(&checkpoint);
            Err(error)
        }
    }
}

fn copy_csv_inner(table: &mut Table, path: &str, header: bool) -> Result<usize> {
    let file = File::open(path).map_err(|error| io_error(path, error))?;
    let mut input = BufReader::new(file);
    let mut parser = Reader::new();
    let schema = table.schema().to_vec();
    let mut record = vec![0; MAX_RECORD_BYTES + 1];
    let mut ends = vec![0; schema.len() + 1];
    let mut output_len = 0;
    let mut end_len = 0;
    let mut raw_record_bytes = 0;
    let mut record_number = 0;
    let mut affected_rows = 0;
    let mut batch = Vec::with_capacity(BATCH_ROWS);

    loop {
        let available = input.fill_buf().map_err(|error| io_error(path, error))?;
        let at_eof = available.is_empty();
        let (status, bytes_read, bytes_written, ends_written) =
            parser.read_record(available, &mut record[output_len..], &mut ends[end_len..]);
        input.consume(bytes_read);
        output_len += bytes_written;
        end_len += ends_written;
        raw_record_bytes += bytes_read;

        if raw_record_bytes > MAX_RECORD_BYTES || output_len > MAX_RECORD_BYTES {
            return Err(csv_error(
                path,
                record_number + 1,
                None,
                format!("record exceeds {MAX_RECORD_BYTES} byte limit"),
            ));
        }

        match status {
            ReadRecordResult::InputEmpty => {
                debug_assert!(!at_eof, "an empty input finalizes a CSV record");
            }
            ReadRecordResult::OutputFull => {
                return Err(csv_error(
                    path,
                    record_number + 1,
                    None,
                    format!("record exceeds {MAX_RECORD_BYTES} byte limit"),
                ));
            }
            ReadRecordResult::OutputEndsFull => {
                return Err(csv_error(
                    path,
                    record_number + 1,
                    None,
                    format!("record has more than {} fields", schema.len()),
                ));
            }
            ReadRecordResult::Record => {
                record_number += 1;
                if header && record_number == 1 {
                    validate_record_shape(path, record_number, end_len, schema.len())?;
                    validate_field_sizes(path, record_number, &ends[..end_len])?;
                } else {
                    let row = convert_record(
                        path,
                        record_number,
                        &record[..output_len],
                        &ends[..end_len],
                        &schema,
                    )?;
                    batch.push(row);
                    affected_rows += 1;
                    if batch.len() == BATCH_ROWS {
                        table.insert_batch(std::mem::take(&mut batch))?;
                        batch = Vec::with_capacity(BATCH_ROWS);
                    }
                }
                output_len = 0;
                end_len = 0;
                raw_record_bytes = 0;
            }
            ReadRecordResult::End => {
                if header && record_number == 0 {
                    return Err(csv_error(path, 1, None, "expected a header record"));
                }
                if !batch.is_empty() {
                    table.insert_batch(batch)?;
                }
                return Ok(affected_rows);
            }
        }
    }
}

fn convert_record(
    path: &str,
    record_number: usize,
    record: &[u8],
    ends: &[usize],
    schema: &[ColumnDef],
) -> Result<Vec<Value>> {
    validate_record_shape(path, record_number, ends.len(), schema.len())?;
    validate_field_sizes(path, record_number, ends)?;

    let mut row = Vec::with_capacity(schema.len());
    let mut start = 0;
    for (index, (end, column)) in ends.iter().zip(schema).enumerate() {
        let field_number = index + 1;
        let bytes = &record[start..*end];
        let value = convert_field(path, record_number, field_number, bytes, column)?;
        row.push(value);
        start = *end;
    }
    Ok(row)
}

fn validate_record_shape(
    path: &str,
    record_number: usize,
    actual: usize,
    expected: usize,
) -> Result<()> {
    if actual != expected {
        return Err(csv_error(
            path,
            record_number,
            None,
            format!("record has {actual} fields; expected {expected} for target table"),
        ));
    }
    Ok(())
}

fn validate_field_sizes(path: &str, record_number: usize, ends: &[usize]) -> Result<()> {
    let mut start = 0;
    for (index, end) in ends.iter().enumerate() {
        if end - start > MAX_FIELD_BYTES {
            return Err(csv_error(
                path,
                record_number,
                Some(index + 1),
                format!("field exceeds {MAX_FIELD_BYTES} byte limit"),
            ));
        }
        start = *end;
    }
    Ok(())
}

fn convert_field(
    path: &str,
    record_number: usize,
    field_number: usize,
    bytes: &[u8],
    column: &ColumnDef,
) -> Result<Value> {
    let text = str::from_utf8(bytes).map_err(|error| {
        csv_error(
            path,
            record_number,
            Some(field_number),
            format!("column '{}' is not valid UTF-8: {error}", column.name),
        )
    })?;

    let invalid = || {
        csv_error(
            path,
            record_number,
            Some(field_number),
            format!(
                "value {text:?} is not a valid {} for column '{}'",
                column.data_type, column.name
            ),
        )
    };
    match column.data_type {
        DataType::Int64 => text.parse::<i64>().map(Value::Int64).map_err(|_| invalid()),
        DataType::Float64 => {
            let value = text.parse::<f64>().map_err(|_| invalid())?;
            if !value.is_finite() {
                return Err(csv_error(
                    path,
                    record_number,
                    Some(field_number),
                    format!("column '{}' cannot store a non-finite Float64", column.name),
                ));
            }
            Ok(Value::Float64(value))
        }
        DataType::Bool if text.eq_ignore_ascii_case("true") => Ok(Value::Bool(true)),
        DataType::Bool if text.eq_ignore_ascii_case("false") => Ok(Value::Bool(false)),
        DataType::Bool => Err(invalid()),
        DataType::String => Ok(Value::String(text.to_owned())),
    }
}

fn csv_error(path: &str, record: usize, field: Option<usize>, message: impl Into<String>) -> Error {
    Error::Csv {
        path: path.to_owned(),
        record,
        field,
        message: message.into(),
    }
}

fn io_error(path: &str, error: std::io::Error) -> Error {
    Error::Io {
        path: path.to_owned(),
        message: error.to_string(),
    }
}
