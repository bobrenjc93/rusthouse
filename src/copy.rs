use std::collections::HashMap;
use std::io::{self, Read};

use crate::error::{Error, Result};
use crate::storage::{ColumnDef, Table};
use crate::value::{DataType, Value};

pub(crate) const MAX_CSV_FIELD_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_CSV_RECORD_BYTES: usize = 8 * 1024 * 1024;
const CSV_READ_BUFFER_BYTES: usize = 8 * 1024;
const COPY_BATCH_ROWS: usize = 1024;
const COPY_BATCH_BYTES: usize = 8 * 1024 * 1024;

pub(crate) fn copy_csv(
    table: &mut Table,
    reader: impl Read,
    path: &str,
    header: bool,
) -> Result<usize> {
    let original_row_count = table.row_count();
    let result = copy_csv_inner(table, reader, path, header);
    if result.is_err() {
        table.truncate(original_row_count);
    }
    result
}

fn copy_csv_inner(table: &mut Table, reader: impl Read, path: &str, header: bool) -> Result<usize> {
    let schema = table.schema().to_vec();
    let mut csv = CsvReader::new(reader, path, schema.len());
    let source_by_target = if header {
        let header = csv.next_record()?.ok_or_else(|| {
            copy_error(
                path,
                Some(1),
                None,
                "expected a header row, but the file is empty",
            )
        })?;
        resolve_header(path, &schema, header)?
    } else {
        (0..schema.len()).collect()
    };

    let mut batch = Vec::with_capacity(COPY_BATCH_ROWS);
    let mut batch_bytes = 0usize;
    let mut affected_rows = 0usize;
    while let Some(record) = csv.next_record()? {
        require_width(path, record.number, schema.len(), record.fields.len())?;
        if !batch.is_empty()
            && (batch.len() >= COPY_BATCH_ROWS
                || batch_bytes.saturating_add(record.decoded_bytes) > COPY_BATCH_BYTES)
        {
            affected_rows += flush_batch(table, &mut batch)?;
            batch_bytes = 0;
        }
        batch_bytes = batch_bytes.saturating_add(record.decoded_bytes);
        batch.push(convert_record(path, &schema, &source_by_target, record)?);
    }
    affected_rows += flush_batch(table, &mut batch)?;
    Ok(affected_rows)
}

fn flush_batch(table: &mut Table, batch: &mut Vec<Vec<Value>>) -> Result<usize> {
    let affected_rows = batch.len();
    table.insert_rows(std::mem::take(batch))?;
    batch.reserve(COPY_BATCH_ROWS);
    Ok(affected_rows)
}

fn resolve_header(path: &str, schema: &[ColumnDef], header: CsvRecord) -> Result<Vec<usize>> {
    require_width(path, header.number, schema.len(), header.fields.len())?;
    let targets = schema
        .iter()
        .enumerate()
        .map(|(index, column)| (column.name.to_ascii_lowercase(), index))
        .collect::<HashMap<_, _>>();
    let mut source_by_target = vec![usize::MAX; schema.len()];
    for (source, name) in header.fields.iter().enumerate() {
        let Some(&target) = targets.get(&name.to_ascii_lowercase()) else {
            return Err(copy_error(
                path,
                Some(header.number),
                Some(name.clone()),
                "header does not match any target column",
            ));
        };
        if source_by_target[target] != usize::MAX {
            return Err(copy_error(
                path,
                Some(header.number),
                Some(name.clone()),
                "header names the target column more than once",
            ));
        }
        source_by_target[target] = source;
    }

    if let Some((target, _)) = source_by_target
        .iter()
        .enumerate()
        .find(|(_, source)| **source == usize::MAX)
    {
        return Err(copy_error(
            path,
            Some(header.number),
            Some(schema[target].name.clone()),
            "target column is missing from the header",
        ));
    }
    Ok(source_by_target)
}

fn require_width(path: &str, row: usize, expected: usize, actual: usize) -> Result<()> {
    if actual == expected {
        return Ok(());
    }
    Err(copy_error(
        path,
        Some(row),
        None,
        format!("record has {actual} fields; expected {expected}"),
    ))
}

fn convert_record(
    path: &str,
    schema: &[ColumnDef],
    source_by_target: &[usize],
    record: CsvRecord,
) -> Result<Vec<Value>> {
    let row_number = record.number;
    let mut fields = record.fields.into_iter().map(Some).collect::<Vec<_>>();
    schema
        .iter()
        .enumerate()
        .map(|(target, column)| {
            let value = fields[source_by_target[target]]
                .take()
                .expect("header mapping has one source per target column");
            convert_value(path, row_number, column, value)
        })
        .collect()
}

fn convert_value(path: &str, row: usize, column: &ColumnDef, value: String) -> Result<Value> {
    let invalid = |detail: String| {
        copy_error(
            path,
            Some(row),
            Some(column.name.clone()),
            format!("{detail}: {}", value_preview(&value)),
        )
    };

    match column.data_type {
        DataType::Int64 => value
            .parse::<i64>()
            .map(Value::Int64)
            .map_err(|_| invalid("invalid Int64 value".to_owned())),
        DataType::Float64 => {
            let number = value
                .parse::<f64>()
                .map_err(|_| invalid("invalid Float64 value".to_owned()))?;
            if !number.is_finite() {
                return Err(invalid("Float64 value must be finite".to_owned()));
            }
            Ok(Value::Float64(number))
        }
        DataType::Bool if value.eq_ignore_ascii_case("true") => Ok(Value::Bool(true)),
        DataType::Bool if value.eq_ignore_ascii_case("false") => Ok(Value::Bool(false)),
        DataType::Bool => Err(invalid(
            "invalid Bool value; expected true or false".to_owned(),
        )),
        DataType::String => Ok(Value::String(value)),
    }
}

fn value_preview(value: &str) -> String {
    const MAX_CHARS: usize = 80;
    let mut characters = value.chars();
    let preview = characters.by_ref().take(MAX_CHARS).collect::<String>();
    if characters.next().is_some() {
        format!("{preview:?}...")
    } else {
        format!("{preview:?}")
    }
}

fn copy_error(
    path: &str,
    row: Option<usize>,
    column: Option<String>,
    message: impl Into<String>,
) -> Error {
    Error::Copy {
        path: path.to_owned(),
        row,
        column,
        message: message.into(),
    }
}

#[derive(Debug)]
struct CsvRecord {
    number: usize,
    fields: Vec<String>,
    decoded_bytes: usize,
}

#[derive(Debug, Clone, Copy)]
enum FieldState {
    Start,
    Unquoted,
    Quoted,
    AfterQuote,
}

struct CsvReader<'a, R> {
    reader: R,
    path: &'a str,
    max_fields: usize,
    buffer: [u8; CSV_READ_BUFFER_BYTES],
    position: usize,
    length: usize,
    completed_records: usize,
}

impl<'a, R: Read> CsvReader<'a, R> {
    fn new(reader: R, path: &'a str, max_fields: usize) -> Self {
        Self {
            reader,
            path,
            max_fields,
            buffer: [0; CSV_READ_BUFFER_BYTES],
            position: 0,
            length: 0,
            completed_records: 0,
        }
    }

    fn next_record(&mut self) -> Result<Option<CsvRecord>> {
        let row = self.completed_records + 1;
        let mut fields = Vec::with_capacity(self.max_fields);
        let mut field = Vec::new();
        let mut state = FieldState::Start;
        let mut record_bytes = 0usize;
        let mut decoded_bytes = 0usize;
        let mut saw_byte = false;

        loop {
            let Some(byte) = self.read_byte(row)? else {
                if !saw_byte {
                    return Ok(None);
                }
                if matches!(state, FieldState::Quoted) {
                    return Err(self.record_error(
                        row,
                        fields.len() + 1,
                        "unterminated quoted field",
                    ));
                }
                self.finish_field(row, &mut fields, field, &mut decoded_bytes)?;
                return self.finish_record(fields, decoded_bytes).map(Some);
            };
            saw_byte = true;
            self.count_record_byte(row, &mut record_bytes)?;

            match (state, byte) {
                (FieldState::Start, b'"') => state = FieldState::Quoted,
                (FieldState::Start, b',') => {
                    self.finish_field(row, &mut fields, field, &mut decoded_bytes)?;
                    field = Vec::new();
                }
                (FieldState::Start, b'\r')
                | (FieldState::Unquoted, b'\r')
                | (FieldState::AfterQuote, b'\r') => {
                    self.require_lf(row, fields.len() + 1, &mut record_bytes)?;
                    self.finish_field(row, &mut fields, field, &mut decoded_bytes)?;
                    return self.finish_record(fields, decoded_bytes).map(Some);
                }
                (FieldState::Start, b'\n')
                | (FieldState::Unquoted, b'\n')
                | (FieldState::AfterQuote, b'\n') => {
                    self.finish_field(row, &mut fields, field, &mut decoded_bytes)?;
                    return self.finish_record(fields, decoded_bytes).map(Some);
                }
                (FieldState::Start, byte) => {
                    self.push_byte(row, fields.len() + 1, &mut field, byte)?;
                    state = FieldState::Unquoted;
                }
                (FieldState::Unquoted, b',') => {
                    self.finish_field(row, &mut fields, field, &mut decoded_bytes)?;
                    field = Vec::new();
                    state = FieldState::Start;
                }
                (FieldState::Unquoted, b'"') => {
                    return Err(self.record_error(
                        row,
                        fields.len() + 1,
                        "quote is only valid at the start of a quoted field",
                    ));
                }
                (FieldState::Unquoted, byte) => {
                    self.push_byte(row, fields.len() + 1, &mut field, byte)?;
                }
                (FieldState::Quoted, b'"') => state = FieldState::AfterQuote,
                (FieldState::Quoted, byte) => {
                    self.push_byte(row, fields.len() + 1, &mut field, byte)?;
                }
                (FieldState::AfterQuote, b'"') => {
                    self.push_byte(row, fields.len() + 1, &mut field, b'"')?;
                    state = FieldState::Quoted;
                }
                (FieldState::AfterQuote, b',') => {
                    self.finish_field(row, &mut fields, field, &mut decoded_bytes)?;
                    field = Vec::new();
                    state = FieldState::Start;
                }
                (FieldState::AfterQuote, _) => {
                    return Err(self.record_error(
                        row,
                        fields.len() + 1,
                        "expected a comma or record ending after the closing quote",
                    ));
                }
            }
        }
    }

    fn read_byte(&mut self, row: usize) -> Result<Option<u8>> {
        while self.position == self.length {
            match self.reader.read(&mut self.buffer) {
                Ok(0) => return Ok(None),
                Ok(length) => {
                    self.position = 0;
                    self.length = length;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    return Err(copy_error(
                        self.path,
                        Some(row),
                        None,
                        format!("could not read file: {error}"),
                    ));
                }
            }
        }
        let byte = self.buffer[self.position];
        self.position += 1;
        Ok(Some(byte))
    }

    fn require_lf(&mut self, row: usize, field: usize, record_bytes: &mut usize) -> Result<()> {
        match self.read_byte(row)? {
            Some(b'\n') => self.count_record_byte(row, record_bytes),
            Some(_) => {
                Err(self.record_error(row, field, "bare carriage return outside quoted field"))
            }
            None => Err(self.record_error(row, field, "file ends after a bare carriage return")),
        }
    }

    fn count_record_byte(&self, row: usize, record_bytes: &mut usize) -> Result<()> {
        *record_bytes += 1;
        if *record_bytes > MAX_CSV_RECORD_BYTES {
            return Err(copy_error(
                self.path,
                Some(row),
                None,
                format!("record exceeds the {MAX_CSV_RECORD_BYTES}-byte limit"),
            ));
        }
        Ok(())
    }

    fn push_byte(
        &self,
        row: usize,
        field_number: usize,
        field: &mut Vec<u8>,
        byte: u8,
    ) -> Result<()> {
        if field.len() >= MAX_CSV_FIELD_BYTES {
            return Err(self.record_error(
                row,
                field_number,
                format!("field exceeds the {MAX_CSV_FIELD_BYTES}-byte limit"),
            ));
        }
        field.push(byte);
        Ok(())
    }

    fn finish_field(
        &self,
        row: usize,
        fields: &mut Vec<String>,
        field: Vec<u8>,
        decoded_bytes: &mut usize,
    ) -> Result<()> {
        if fields.len() >= self.max_fields {
            return Err(copy_error(
                self.path,
                Some(row),
                None,
                format!("record has more than {} fields", self.max_fields),
            ));
        }
        let field_number = fields.len() + 1;
        let field = String::from_utf8(field)
            .map_err(|_| self.record_error(row, field_number, "field is not valid UTF-8"))?;
        *decoded_bytes = decoded_bytes.saturating_add(field.len());
        fields.push(field);
        Ok(())
    }

    fn finish_record(&mut self, fields: Vec<String>, decoded_bytes: usize) -> Result<CsvRecord> {
        self.completed_records = self.completed_records.checked_add(1).ok_or_else(|| {
            copy_error(
                self.path,
                None,
                None,
                "record count exceeds the platform limit",
            )
        })?;
        Ok(CsvRecord {
            number: self.completed_records,
            fields,
            decoded_bytes,
        })
    }

    fn record_error(&self, row: usize, field: usize, message: impl Into<String>) -> Error {
        copy_error(
            self.path,
            Some(row),
            None,
            format!("field {field}: {}", message.into()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::io::Cursor;
    use std::rc::Rc;

    fn table() -> Table {
        Table::new(
            "events".to_owned(),
            vec![
                ColumnDef {
                    name: "id".to_owned(),
                    data_type: DataType::Int64,
                },
                ColumnDef {
                    name: "note".to_owned(),
                    data_type: DataType::String,
                },
            ],
        )
        .expect("valid table")
    }

    #[test]
    fn parses_crlf_multiline_and_escaped_quotes() {
        let mut table = table();
        let csv = b"id,note\r\n1,plain\r\n2,\"line one\r\nline \"\"two\"\"\"\r\n";

        let rows = copy_csv(&mut table, Cursor::new(csv), "memory.csv", true).expect("valid CSV");

        assert_eq!(rows, 2);
        assert!(
            matches!(&table.columns()[0], crate::storage::Column::Int64(values) if values == &[1, 2])
        );
        assert!(
            matches!(&table.columns()[1], crate::storage::Column::String(values) if values == &["plain", "line one\r\nline \"two\""])
        );
    }

    struct BoundedReader {
        data: Cursor<Vec<u8>>,
        largest_request: Rc<Cell<usize>>,
    }

    impl Read for BoundedReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.largest_request
                .set(self.largest_request.get().max(buffer.len()));
            self.data.read(buffer)
        }
    }

    #[test]
    fn streams_large_inputs_with_bounded_reads_and_multiple_batches() {
        let mut data = Vec::new();
        for id in 0..5_000 {
            data.extend_from_slice(format!("{id},record {id}\n").as_bytes());
        }
        let largest_request = Rc::new(Cell::new(0));
        let reader = BoundedReader {
            data: Cursor::new(data),
            largest_request: Rc::clone(&largest_request),
        };
        let mut table = table();

        let rows = copy_csv(&mut table, reader, "large.csv", false).expect("valid CSV");

        assert_eq!(rows, 5_000);
        assert_eq!(table.row_count(), 5_000);
        assert!(largest_request.get() <= CSV_READ_BUFFER_BYTES);
    }

    struct FailsAfterData {
        data: Cursor<Vec<u8>>,
    }

    impl Read for FailsAfterData {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let read = self.data.read(buffer)?;
            if read == 0 {
                Err(io::Error::other("injected read failure"))
            } else {
                Ok(read)
            }
        }
    }

    #[test]
    fn rolls_back_batches_after_a_late_io_error() {
        let mut table = table();
        table
            .insert_row(vec![Value::Int64(-1), Value::String("existing".to_owned())])
            .expect("seed row");
        let mut data = Vec::new();
        for id in 0..1_500 {
            data.extend_from_slice(format!("{id},record {id}\n").as_bytes());
        }

        let error = copy_csv(
            &mut table,
            FailsAfterData {
                data: Cursor::new(data),
            },
            "broken.csv",
            false,
        )
        .expect_err("read failure");

        assert!(
            matches!(error, Error::Copy { message, .. } if message.contains("injected read failure"))
        );
        assert_eq!(table.row_count(), 1);
        assert!(
            matches!(&table.columns()[0], crate::storage::Column::Int64(values) if values == &[-1])
        );
    }
}
