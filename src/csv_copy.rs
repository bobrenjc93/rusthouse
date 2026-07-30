use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::mem;

use crate::error::{Error, Result};
use crate::storage::Table;
use crate::value::{DataType, Value};

const COPY_BATCH_SIZE: usize = 1_024;

pub(crate) fn copy_from_path(
    table: &mut Table,
    requested_columns: Option<&[String]>,
    path: &str,
) -> Result<usize> {
    let columns = resolve_columns(table, requested_columns)?;
    let file = File::open(path).map_err(|error| copy_error(path, None, error.to_string()))?;
    let mut reader = CsvReader::new(BufReader::new(file));

    let header = read_record(&mut reader, path, 1)?
        .ok_or_else(|| copy_error(path, None, "CSV file is empty; expected a header row"))?;
    validate_header(path, &header, &columns)?;

    let mut batch = Vec::with_capacity(COPY_BATCH_SIZE);
    let mut inserted = 0usize;
    let mut record_number = 2usize;
    while let Some(fields) = read_record(&mut reader, path, record_number)? {
        batch.push(convert_record(
            path,
            record_number,
            fields,
            &columns,
            table.schema().len(),
        )?);
        if batch.len() == COPY_BATCH_SIZE {
            inserted += table.insert_batch(mem::take(&mut batch))?;
            batch = Vec::with_capacity(COPY_BATCH_SIZE);
        }
        record_number += 1;
    }
    inserted += table.insert_batch(batch)?;
    Ok(inserted)
}

#[derive(Debug)]
struct CopyColumn {
    input_name: String,
    table_index: usize,
    data_type: DataType,
}

fn resolve_columns(table: &Table, requested: Option<&[String]>) -> Result<Vec<CopyColumn>> {
    let names = requested.map(<[String]>::to_vec).unwrap_or_else(|| {
        table
            .schema()
            .iter()
            .map(|field| field.name.clone())
            .collect()
    });
    let mut seen = HashSet::with_capacity(names.len());
    let mut columns = Vec::with_capacity(names.len());
    for name in names {
        let table_index = table.column_index(&name)?;
        if !seen.insert(table_index) {
            return Err(Error::InvalidQuery(format!(
                "COPY column '{name}' is listed more than once"
            )));
        }
        columns.push(CopyColumn {
            input_name: name,
            table_index,
            data_type: table.schema()[table_index].data_type,
        });
    }
    if columns.len() != table.schema().len() {
        return Err(Error::InvalidQuery(format!(
            "COPY for table '{}' must name all {} columns because tables have no defaults or NULL; got {}",
            table.name(),
            table.schema().len(),
            columns.len()
        )));
    }
    Ok(columns)
}

fn validate_header(path: &str, header: &[String], columns: &[CopyColumn]) -> Result<()> {
    if header.len() != columns.len() {
        return Err(copy_error(
            path,
            Some(1),
            format!(
                "header has {} fields; expected {}",
                header.len(),
                columns.len()
            ),
        ));
    }
    for (position, (actual, expected)) in header.iter().zip(columns).enumerate() {
        if !actual.eq_ignore_ascii_case(&expected.input_name) {
            return Err(copy_error(
                path,
                Some(1),
                format!(
                    "header field {} is {actual:?}; expected {:?}",
                    position + 1,
                    expected.input_name
                ),
            ));
        }
    }
    Ok(())
}

fn convert_record(
    path: &str,
    record_number: usize,
    fields: Vec<String>,
    columns: &[CopyColumn],
    table_width: usize,
) -> Result<Vec<Value>> {
    if fields.len() != columns.len() {
        return Err(copy_error(
            path,
            Some(record_number),
            format!(
                "row has {} fields; expected {}",
                fields.len(),
                columns.len()
            ),
        ));
    }

    let mut row = (0..table_width)
        .map(|_| None)
        .collect::<Vec<Option<Value>>>();
    for (field, column) in fields.into_iter().zip(columns) {
        row[column.table_index] = Some(parse_value(path, record_number, column, field)?);
    }
    Ok(row
        .into_iter()
        .map(|value| value.expect("COPY column resolution covers the complete table"))
        .collect())
}

fn parse_value(
    path: &str,
    record_number: usize,
    column: &CopyColumn,
    field: String,
) -> Result<Value> {
    let invalid = || {
        copy_error(
            path,
            Some(record_number),
            format!(
                "field for column '{}' is not a valid {}: {}",
                column.input_name,
                column.data_type,
                display_field(&field)
            ),
        )
    };

    match column.data_type {
        DataType::Int64 => field
            .parse::<i64>()
            .map(Value::Int64)
            .map_err(|_| invalid()),
        DataType::Float64 => {
            let value = field.parse::<f64>().map_err(|_| invalid())?;
            if !value.is_finite() {
                return Err(invalid());
            }
            Ok(Value::Float64(value))
        }
        DataType::Bool => {
            if field.eq_ignore_ascii_case("true") {
                Ok(Value::Bool(true))
            } else if field.eq_ignore_ascii_case("false") {
                Ok(Value::Bool(false))
            } else {
                Err(invalid())
            }
        }
        DataType::String => Ok(Value::String(field)),
    }
}

fn display_field(field: &str) -> String {
    const MAX_CHARS: usize = 80;
    let mut characters = field.chars();
    let prefix = characters.by_ref().take(MAX_CHARS).collect::<String>();
    if characters.next().is_some() {
        format!("{prefix:?}...")
    } else {
        format!("{prefix:?}")
    }
}

fn read_record<R: BufRead>(
    reader: &mut CsvReader<R>,
    path: &str,
    record_number: usize,
) -> Result<Option<Vec<String>>> {
    reader.read_record().map_err(|error| match error {
        CsvReadError::Io(error) => copy_error(path, Some(record_number), error.to_string()),
        CsvReadError::Malformed(message) => copy_error(path, Some(record_number), message),
    })
}

fn copy_error(path: &str, record: Option<usize>, message: impl Into<String>) -> Error {
    Error::Copy {
        path: path.to_owned(),
        record,
        message: message.into(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldState {
    Start,
    Unquoted,
    Quoted,
    QuoteClosed,
}

struct CsvReader<R> {
    input: R,
    line: Vec<u8>,
}

impl<R: BufRead> CsvReader<R> {
    fn new(input: R) -> Self {
        Self {
            input,
            line: Vec::new(),
        }
    }

    fn read_record(&mut self) -> std::result::Result<Option<Vec<String>>, CsvReadError> {
        let mut fields = Vec::new();
        let mut field = Vec::new();
        let mut state = FieldState::Start;
        let mut saw_input = false;

        loop {
            self.line.clear();
            let bytes_read = self
                .input
                .read_until(b'\n', &mut self.line)
                .map_err(CsvReadError::Io)?;
            if bytes_read == 0 {
                if !saw_input {
                    return Ok(None);
                }
                if state == FieldState::Quoted {
                    return Err(CsvReadError::Malformed(
                        "unterminated quoted field".to_owned(),
                    ));
                }
                finish_field(&mut fields, &mut field)?;
                return Ok(Some(fields));
            }
            saw_input = true;

            let mut position = 0;
            while position < self.line.len() {
                let byte = self.line[position];
                let crlf = byte == b'\r'
                    && self.line.get(position + 1) == Some(&b'\n')
                    && state != FieldState::Quoted;
                if crlf {
                    position += 1;
                    continue;
                }

                match (state, byte) {
                    (FieldState::Start, b'"') => state = FieldState::Quoted,
                    (FieldState::Start, b',') => finish_field(&mut fields, &mut field)?,
                    (FieldState::Start, b'\n') => {
                        finish_field(&mut fields, &mut field)?;
                        return Ok(Some(fields));
                    }
                    (FieldState::Start, byte) => {
                        field.push(byte);
                        state = FieldState::Unquoted;
                    }
                    (FieldState::Unquoted, b'"') => {
                        return Err(CsvReadError::Malformed(
                            "unexpected quote in unquoted field".to_owned(),
                        ));
                    }
                    (FieldState::Unquoted, b',') => {
                        finish_field(&mut fields, &mut field)?;
                        state = FieldState::Start;
                    }
                    (FieldState::Unquoted, b'\n') => {
                        finish_field(&mut fields, &mut field)?;
                        return Ok(Some(fields));
                    }
                    (FieldState::Unquoted, byte) => field.push(byte),
                    (FieldState::Quoted, b'"') => state = FieldState::QuoteClosed,
                    (FieldState::Quoted, byte) => field.push(byte),
                    (FieldState::QuoteClosed, b'"') => {
                        field.push(b'"');
                        state = FieldState::Quoted;
                    }
                    (FieldState::QuoteClosed, b',') => {
                        finish_field(&mut fields, &mut field)?;
                        state = FieldState::Start;
                    }
                    (FieldState::QuoteClosed, b'\n') => {
                        finish_field(&mut fields, &mut field)?;
                        return Ok(Some(fields));
                    }
                    (FieldState::QuoteClosed, _) => {
                        return Err(CsvReadError::Malformed(
                            "unexpected character after closing quote".to_owned(),
                        ));
                    }
                }
                position += 1;
            }
        }
    }
}

fn finish_field(
    fields: &mut Vec<String>,
    field: &mut Vec<u8>,
) -> std::result::Result<(), CsvReadError> {
    let bytes = mem::take(field);
    let value = String::from_utf8(bytes)
        .map_err(|_| CsvReadError::Malformed("field is not valid UTF-8".to_owned()))?;
    fields.push(value);
    Ok(())
}

#[derive(Debug)]
enum CsvReadError {
    Io(io::Error),
    Malformed(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn records(input: &[u8]) -> std::result::Result<Vec<Vec<String>>, CsvReadError> {
        let mut reader = CsvReader::new(Cursor::new(input));
        let mut records = Vec::new();
        while let Some(record) = reader.read_record()? {
            records.push(record);
        }
        Ok(records)
    }

    #[test]
    fn reads_quotes_commas_newlines_empty_fields_and_crlf() {
        assert_eq!(
            records(b"a,b,c\r\n1,\"hello, \"\"world\"\"\",\"line 1\r\nline 2\"\r\n2,,tail")
                .expect("valid CSV"),
            vec![
                vec!["a", "b", "c"],
                vec!["1", "hello, \"world\"", "line 1\r\nline 2"],
                vec!["2", "", "tail"],
            ]
        );
    }

    #[test]
    fn rejects_malformed_quotes() {
        let error = records(b"header\n\"not closed").expect_err("invalid CSV");
        assert!(
            matches!(error, CsvReadError::Malformed(message) if message.contains("unterminated"))
        );
    }
}
