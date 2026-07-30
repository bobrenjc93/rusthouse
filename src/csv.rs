use std::io::BufRead;

use crate::error::{Error, Result};
use crate::storage::{ColumnBatch, ColumnDef, Table};
use crate::value::{DataType, Value};

pub(crate) const BLOCK_ROWS: usize = 1_024;
const READ_BUFFER_BYTES: usize = 8 * 1_024;

pub(crate) fn insert<R: BufRead>(table: &mut Table, reader: R, with_names: bool) -> Result<usize> {
    let original_row_count = table.row_count();
    let result = insert_inner(table, reader, with_names);
    if result.is_err() {
        table.truncate(original_row_count);
    }
    result
}

fn insert_inner<R: BufRead>(table: &mut Table, reader: R, with_names: bool) -> Result<usize> {
    let schema = table.schema().to_vec();
    let table_name = table.name().to_owned();
    let mut records = RecordReader::new(reader);

    if with_names {
        let header = records.read_record()?.ok_or_else(|| Error::Csv {
            record: 1,
            column: None,
            message: "CSVWithNames input is missing its header row".to_owned(),
        })?;
        validate_header(&table_name, &schema, &header)?;
    }

    let mut imported = 0;
    let mut batch = ColumnBatch::new(&schema);
    while let Some(record) = records.read_record()? {
        let record_number = records.record_number();
        if record.len() != schema.len() {
            return Err(Error::RowLength {
                table: table_name.clone(),
                expected: schema.len(),
                actual: record.len(),
            });
        }

        let row = record
            .into_iter()
            .zip(&schema)
            .enumerate()
            .map(|(index, (field, definition))| {
                decode_value(field, definition, record_number, index + 1)
            })
            .collect::<Result<Vec<_>>>()?;
        batch.push_row(row);

        if batch.row_count() == BLOCK_ROWS {
            imported += batch.row_count();
            table.append_batch(batch);
            batch = ColumnBatch::new(&schema);
        }
    }

    imported += batch.row_count();
    table.append_batch(batch);
    Ok(imported)
}

fn validate_header(table: &str, schema: &[ColumnDef], header: &[String]) -> Result<()> {
    if header.len() != schema.len() {
        return Err(Error::RowLength {
            table: table.to_owned(),
            expected: schema.len(),
            actual: header.len(),
        });
    }

    for (index, (actual, expected)) in header.iter().zip(schema).enumerate() {
        if !actual.eq_ignore_ascii_case(&expected.name) {
            return Err(Error::Csv {
                record: 1,
                column: Some(index + 1),
                message: format!(
                    "header names column {:?}; expected {:?}",
                    actual, expected.name
                ),
            });
        }
    }
    Ok(())
}

fn decode_value(
    field: String,
    definition: &ColumnDef,
    record: usize,
    column: usize,
) -> Result<Value> {
    let invalid = |expected: DataType| Error::Csv {
        record,
        column: Some(column),
        message: format!(
            "value {field:?} is not a valid {expected} for column {:?}",
            definition.name
        ),
    };

    match definition.data_type {
        DataType::Int64 => field
            .parse::<i64>()
            .map(Value::Int64)
            .map_err(|_| invalid(DataType::Int64)),
        DataType::Float64 => {
            let value = field
                .parse::<f64>()
                .map_err(|_| invalid(DataType::Float64))?;
            if !value.is_finite() {
                return Err(invalid(DataType::Float64));
            }
            Ok(Value::Float64(value))
        }
        DataType::Bool if field == "1" || field.eq_ignore_ascii_case("true") => {
            Ok(Value::Bool(true))
        }
        DataType::Bool if field == "0" || field.eq_ignore_ascii_case("false") => {
            Ok(Value::Bool(false))
        }
        DataType::Bool => Err(invalid(DataType::Bool)),
        DataType::String => Ok(Value::String(field)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldState {
    Start,
    Unquoted,
    Quoted,
    AfterQuote,
}

/// A strict RFC 4180 record reader with a small fixed I/O buffer.
struct RecordReader<R> {
    reader: R,
    buffer: [u8; READ_BUFFER_BYTES],
    buffer_position: usize,
    buffer_len: usize,
    record_number: usize,
}

impl<R: BufRead> RecordReader<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            buffer: [0; READ_BUFFER_BYTES],
            buffer_position: 0,
            buffer_len: 0,
            record_number: 0,
        }
    }

    fn record_number(&self) -> usize {
        self.record_number
    }

    fn read_record(&mut self) -> Result<Option<Vec<String>>> {
        let next_record = self.record_number + 1;
        let mut fields = Vec::new();
        let mut field = Vec::new();
        let mut state = FieldState::Start;
        let mut started = false;

        loop {
            let Some(byte) = self.read_byte().map_err(|error| Error::Csv {
                record: next_record,
                column: Some(fields.len() + 1),
                message: format!("could not read input: {error}"),
            })?
            else {
                if !started {
                    return Ok(None);
                }
                if state == FieldState::Quoted {
                    return self.error(fields.len() + 1, "unterminated quoted field");
                }
                fields.push(self.finish_field(field, fields.len() + 1)?);
                self.record_number = next_record;
                return Ok(Some(fields));
            };
            started = true;

            match (state, byte) {
                (FieldState::Start, b',') => {
                    fields.push(String::new());
                }
                (FieldState::Start, b'"') => state = FieldState::Quoted,
                (FieldState::Start, b'\n') => {
                    fields.push(String::new());
                    self.record_number = next_record;
                    return Ok(Some(fields));
                }
                (FieldState::Start, b'\r') => {
                    self.expect_line_feed(fields.len() + 1)?;
                    fields.push(String::new());
                    self.record_number = next_record;
                    return Ok(Some(fields));
                }
                (FieldState::Start, byte) => {
                    field.push(byte);
                    state = FieldState::Unquoted;
                }
                (FieldState::Unquoted, b',') => {
                    fields.push(self.finish_field(field, fields.len() + 1)?);
                    field = Vec::new();
                    state = FieldState::Start;
                }
                (FieldState::Unquoted, b'\n') => {
                    fields.push(self.finish_field(field, fields.len() + 1)?);
                    self.record_number = next_record;
                    return Ok(Some(fields));
                }
                (FieldState::Unquoted, b'\r') => {
                    self.expect_line_feed(fields.len() + 1)?;
                    fields.push(self.finish_field(field, fields.len() + 1)?);
                    self.record_number = next_record;
                    return Ok(Some(fields));
                }
                (FieldState::Unquoted, b'"') => {
                    return self.error(fields.len() + 1, "quote in an unquoted field");
                }
                (FieldState::Unquoted, byte) => field.push(byte),
                (FieldState::Quoted, b'"') => state = FieldState::AfterQuote,
                (FieldState::Quoted, byte) => field.push(byte),
                (FieldState::AfterQuote, b'"') => {
                    field.push(b'"');
                    state = FieldState::Quoted;
                }
                (FieldState::AfterQuote, b',') => {
                    fields.push(self.finish_field(field, fields.len() + 1)?);
                    field = Vec::new();
                    state = FieldState::Start;
                }
                (FieldState::AfterQuote, b'\n') => {
                    fields.push(self.finish_field(field, fields.len() + 1)?);
                    self.record_number = next_record;
                    return Ok(Some(fields));
                }
                (FieldState::AfterQuote, b'\r') => {
                    self.expect_line_feed(fields.len() + 1)?;
                    fields.push(self.finish_field(field, fields.len() + 1)?);
                    self.record_number = next_record;
                    return Ok(Some(fields));
                }
                (FieldState::AfterQuote, _) => {
                    return self.error(
                        fields.len() + 1,
                        "expected a comma or record ending after a closing quote",
                    );
                }
            }
        }
    }

    fn finish_field(&self, field: Vec<u8>, column: usize) -> Result<String> {
        String::from_utf8(field).map_err(|_| Error::Csv {
            record: self.record_number + 1,
            column: Some(column),
            message: "field is not valid UTF-8".to_owned(),
        })
    }

    fn expect_line_feed(&mut self, column: usize) -> Result<()> {
        match self.read_byte() {
            Ok(Some(b'\n')) => Ok(()),
            Ok(_) => self.error(column, "carriage return must be followed by a line feed"),
            Err(error) => Err(Error::Csv {
                record: self.record_number + 1,
                column: Some(column),
                message: format!("could not read input: {error}"),
            }),
        }
    }

    fn error<T>(&self, column: usize, message: impl Into<String>) -> Result<T> {
        Err(Error::Csv {
            record: self.record_number + 1,
            column: Some(column),
            message: message.into(),
        })
    }

    fn read_byte(&mut self) -> std::io::Result<Option<u8>> {
        if self.buffer_position == self.buffer_len {
            self.buffer_len = self.reader.read(&mut self.buffer)?;
            self.buffer_position = 0;
            if self.buffer_len == 0 {
                return Ok(None);
            }
        }

        let byte = self.buffer[self.buffer_position];
        self.buffer_position += 1;
        Ok(Some(byte))
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufReader, Cursor};

    use super::*;

    #[test]
    fn reads_quotes_commas_newlines_and_utf8_across_small_records() {
        let input = "plain,\"comma, quote \"\" and\r\nnewline\"\r\n\"東京\",last\r\n";
        let mut reader = RecordReader::new(Cursor::new(input));

        assert_eq!(
            reader.read_record().expect("first record"),
            Some(vec![
                "plain".to_owned(),
                "comma, quote \" and\r\nnewline".to_owned()
            ])
        );
        assert_eq!(
            reader.read_record().expect("second record"),
            Some(vec!["東京".to_owned(), "last".to_owned()])
        );
        assert_eq!(reader.read_record().expect("EOF"), None);
    }

    #[test]
    fn rejects_malformed_quoting_and_utf8() {
        for input in [b"a\"b,c\n".as_slice(), b"\"unterminated".as_slice()] {
            let mut reader = RecordReader::new(BufReader::new(input));
            assert!(matches!(reader.read_record(), Err(Error::Csv { .. })));
        }

        let mut reader = RecordReader::new(BufReader::new(&b"a,\xff\n"[..]));
        assert!(matches!(reader.read_record(), Err(Error::Csv { .. })));
    }
}
