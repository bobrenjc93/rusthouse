use super::{
    FormatError, FormatLimits, LimitKind, LimitedInput, empty_columns, finish_batch,
    ingest_batches, push_null,
};
use crate::storage::{Column, ColumnBatch, DataType, Schema, Table};
use std::io::{BufRead, Write};

/// Settings for schema-driven CSV decoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvOptions {
    pub limits: FormatLimits,
    pub delimiter: u8,
    pub has_header: bool,
    /// An exact, unquoted field matching this token is SQL `NULL`.
    pub null_token: String,
}

impl Default for CsvOptions {
    fn default() -> Self {
        Self {
            limits: FormatLimits::default(),
            delimiter: b',',
            has_header: true,
            null_token: "\\N".to_owned(),
        }
    }
}

impl CsvOptions {
    fn validate(&self, schema: &Schema) -> Result<(), FormatError> {
        self.limits.validate(schema)?;
        validate_csv_syntax_options(
            self.delimiter,
            &self.null_token,
            self.limits.max_field_bytes,
        )
    }
}

/// Settings for CSV serialization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvExportOptions {
    pub delimiter: u8,
    pub include_header: bool,
    pub null_token: String,
}

impl Default for CsvExportOptions {
    fn default() -> Self {
        Self {
            delimiter: b',',
            include_header: true,
            null_token: "\\N".to_owned(),
        }
    }
}

impl CsvExportOptions {
    fn validate(&self) -> Result<(), FormatError> {
        validate_csv_syntax_options(self.delimiter, &self.null_token, usize::MAX)
    }
}

fn validate_csv_syntax_options(
    delimiter: u8,
    null_token: &str,
    max_field_bytes: usize,
) -> Result<(), FormatError> {
    if delimiter == b'"' || delimiter == b'\r' || delimiter == b'\n' {
        return Err(FormatError::InvalidOption(
            "CSV delimiter cannot be a quote or line ending".to_owned(),
        ));
    }
    if null_token.is_empty() {
        return Err(FormatError::InvalidOption(
            "CSV null_token cannot be empty".to_owned(),
        ));
    }
    if null_token.len() > max_field_bytes {
        return Err(FormatError::InvalidOption(
            "CSV null_token exceeds max_field_bytes".to_owned(),
        ));
    }
    if null_token
        .as_bytes()
        .iter()
        .any(|byte| matches!(*byte, b'"' | b'\r' | b'\n') || *byte == delimiter)
    {
        return Err(FormatError::InvalidOption(
            "CSV null_token must be representable as one unquoted field".to_owned(),
        ));
    }
    Ok(())
}

#[derive(Debug)]
struct CsvField {
    bytes: Vec<u8>,
    quoted: bool,
}

struct CsvStream<R> {
    input: LimitedInput<R>,
    limits: FormatLimits,
    delimiter: u8,
    skip_lf: bool,
}

impl<R: BufRead> CsvStream<R> {
    fn new(input: R, options: &CsvOptions) -> Self {
        Self {
            input: LimitedInput::new(input, options.limits.max_input_bytes),
            limits: options.limits.clone(),
            delimiter: options.delimiter,
            skip_lf: false,
        }
    }

    fn read_record(&mut self, row: u64) -> Result<Option<Vec<CsvField>>, FormatError> {
        let mut fields = Vec::new();
        let mut field = Vec::new();
        let mut quoted = false;
        let mut at_field_start = true;
        let mut in_quotes = false;
        let mut quote_closed = false;
        let mut saw_record_byte = false;
        let mut record_bytes = 0_usize;

        let mut pending = None;
        if self.skip_lf {
            self.skip_lf = false;
            match self.input.read_byte()? {
                Some(b'\n') => {}
                other => pending = other,
            }
        }

        loop {
            let next = match pending.take() {
                Some(byte) => Some(byte),
                None => self.input.read_byte()?,
            };
            let Some(byte) = next else {
                if in_quotes {
                    return Err(FormatError::CsvSyntax {
                        row,
                        message: "unterminated quoted field".to_owned(),
                    });
                }
                if !saw_record_byte && fields.is_empty() && field.is_empty() {
                    return Ok(None);
                }
                self.finish_field(row, &mut fields, field, quoted)?;
                return Ok(Some(fields));
            };
            saw_record_byte = true;
            let is_record_ending = !in_quotes && matches!(byte, b'\r' | b'\n');
            if !is_record_ending {
                record_bytes = record_bytes.saturating_add(1);
                if record_bytes > self.limits.max_record_bytes {
                    return Err(FormatError::LimitExceeded {
                        kind: LimitKind::RecordBytes,
                        limit: self.limits.max_record_bytes as u64,
                        row: Some(row),
                    });
                }
            }

            if in_quotes {
                if byte == b'"' {
                    in_quotes = false;
                    quote_closed = true;
                } else {
                    self.push_field_byte(row, &mut field, byte)?;
                }
                continue;
            }

            if quote_closed {
                match byte {
                    b'"' => {
                        self.push_field_byte(row, &mut field, b'"')?;
                        in_quotes = true;
                        quote_closed = false;
                    }
                    byte if byte == self.delimiter => {
                        self.finish_field(row, &mut fields, field, quoted)?;
                        field = Vec::new();
                        quoted = false;
                        at_field_start = true;
                        quote_closed = false;
                    }
                    b'\n' => {
                        self.finish_field(row, &mut fields, field, quoted)?;
                        return Ok(Some(fields));
                    }
                    b'\r' => {
                        self.finish_field(row, &mut fields, field, quoted)?;
                        self.skip_lf = true;
                        return Ok(Some(fields));
                    }
                    _ => {
                        return Err(FormatError::CsvSyntax {
                            row,
                            message: "unexpected byte after closing quote".to_owned(),
                        });
                    }
                }
                continue;
            }

            match byte {
                byte if byte == self.delimiter => {
                    self.finish_field(row, &mut fields, field, quoted)?;
                    field = Vec::new();
                    quoted = false;
                    at_field_start = true;
                }
                b'\n' => {
                    self.finish_field(row, &mut fields, field, quoted)?;
                    return Ok(Some(fields));
                }
                b'\r' => {
                    self.finish_field(row, &mut fields, field, quoted)?;
                    self.skip_lf = true;
                    return Ok(Some(fields));
                }
                b'"' if at_field_start => {
                    quoted = true;
                    in_quotes = true;
                    at_field_start = false;
                }
                b'"' => {
                    return Err(FormatError::CsvSyntax {
                        row,
                        message: "quote in an unquoted field".to_owned(),
                    });
                }
                _ => {
                    self.push_field_byte(row, &mut field, byte)?;
                    at_field_start = false;
                }
            }
        }
    }

    fn push_field_byte(&self, row: u64, field: &mut Vec<u8>, byte: u8) -> Result<(), FormatError> {
        if field.len() == self.limits.max_field_bytes {
            return Err(FormatError::LimitExceeded {
                kind: LimitKind::FieldBytes,
                limit: self.limits.max_field_bytes as u64,
                row: Some(row),
            });
        }
        field.push(byte);
        Ok(())
    }

    fn finish_field(
        &self,
        row: u64,
        fields: &mut Vec<CsvField>,
        bytes: Vec<u8>,
        quoted: bool,
    ) -> Result<(), FormatError> {
        if fields.len() == self.limits.max_fields_per_row {
            return Err(FormatError::LimitExceeded {
                kind: LimitKind::FieldsPerRow,
                limit: self.limits.max_fields_per_row as u64,
                row: Some(row),
            });
        }
        fields.push(CsvField { bytes, quoted });
        Ok(())
    }
}

/// An iterator of typed CSV batches. It retains at most one record and one batch.
pub struct CsvBatchReader<R> {
    stream: CsvStream<R>,
    schema: Schema,
    options: CsvOptions,
    header_read: bool,
    rows_read: u64,
    finished: bool,
}

impl<R: BufRead> CsvBatchReader<R> {
    pub fn new(input: R, schema: &Schema, options: CsvOptions) -> Result<Self, FormatError> {
        options.validate(schema)?;
        Ok(Self {
            stream: CsvStream::new(input, &options),
            schema: schema.clone(),
            options,
            header_read: false,
            rows_read: 0,
            finished: false,
        })
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    fn read_header(&mut self) -> Result<(), FormatError> {
        self.header_read = true;
        if !self.options.has_header {
            return Ok(());
        }
        let Some(fields) = self.stream.read_record(1)? else {
            return Err(FormatError::HeaderMismatch {
                expected: self
                    .schema
                    .fields()
                    .iter()
                    .map(|field| field.name().to_owned())
                    .collect(),
                actual: Vec::new(),
            });
        };
        let mut actual = Vec::with_capacity(fields.len());
        for field in fields {
            let value = String::from_utf8(field.bytes).map_err(|_| FormatError::InvalidUtf8 {
                row: 1,
                column: "CSV header".to_owned(),
            })?;
            if value.len() > self.options.limits.max_string_bytes {
                return Err(FormatError::LimitExceeded {
                    kind: LimitKind::StringBytes,
                    limit: self.options.limits.max_string_bytes as u64,
                    row: Some(1),
                });
            }
            actual.push(value);
        }
        let expected: Vec<_> = self
            .schema
            .fields()
            .iter()
            .map(|field| field.name().to_owned())
            .collect();
        if actual != expected {
            return Err(FormatError::HeaderMismatch { expected, actual });
        }
        Ok(())
    }

    fn append_record(
        &self,
        columns: &mut [Column],
        fields: Vec<CsvField>,
        row: u64,
    ) -> Result<(), FormatError> {
        if fields.len() != self.schema.len() {
            return Err(FormatError::FieldCount {
                row,
                expected: self.schema.len(),
                actual: fields.len(),
            });
        }
        for ((column, schema_field), csv_field) in
            columns.iter_mut().zip(self.schema.fields()).zip(fields)
        {
            if !csv_field.quoted && csv_field.bytes == self.options.null_token.as_bytes() {
                push_null(column, schema_field.name(), schema_field.is_nullable(), row)?;
                continue;
            }
            let text =
                std::str::from_utf8(&csv_field.bytes).map_err(|_| FormatError::InvalidUtf8 {
                    row,
                    column: schema_field.name().to_owned(),
                })?;
            match column {
                Column::Int64(values) => values.push(Some(text.parse().map_err(|_| {
                    conversion_error(row, schema_field.name(), DataType::Int64, text)
                })?)),
                Column::Float64(values) => {
                    let value: f64 = text.parse().map_err(|_| {
                        conversion_error(row, schema_field.name(), DataType::Float64, text)
                    })?;
                    if !value.is_finite() {
                        return Err(conversion_error(
                            row,
                            schema_field.name(),
                            DataType::Float64,
                            text,
                        ));
                    }
                    values.push(Some(value));
                }
                Column::Bool(values) => values.push(Some(match text {
                    "true" => true,
                    "false" => false,
                    _ => {
                        return Err(conversion_error(
                            row,
                            schema_field.name(),
                            DataType::Bool,
                            text,
                        ));
                    }
                })),
                Column::String(values) => {
                    if text.len() > self.options.limits.max_string_bytes {
                        return Err(FormatError::LimitExceeded {
                            kind: LimitKind::StringBytes,
                            limit: self.options.limits.max_string_bytes as u64,
                            row: Some(row),
                        });
                    }
                    values.push(Some(text.to_owned()));
                }
            }
        }
        Ok(())
    }
}

impl<R: BufRead> Iterator for CsvBatchReader<R> {
    type Item = Result<ColumnBatch, FormatError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        if !self.header_read
            && let Err(error) = self.read_header()
        {
            self.finished = true;
            return Some(Err(error));
        }

        let mut columns = empty_columns(&self.schema);
        let mut batch_rows = 0_usize;
        while batch_rows < self.options.limits.batch_rows {
            let row = self.rows_read + 1;
            let fields = match self.stream.read_record(row) {
                Ok(Some(fields)) => fields,
                Ok(None) => {
                    self.finished = true;
                    break;
                }
                Err(error) => {
                    self.finished = true;
                    return Some(Err(error));
                }
            };
            if self.rows_read == self.options.limits.max_rows {
                self.finished = true;
                return Some(Err(FormatError::LimitExceeded {
                    kind: LimitKind::Rows,
                    limit: self.options.limits.max_rows,
                    row: Some(row),
                }));
            }
            if let Err(error) = self.append_record(&mut columns, fields, row) {
                self.finished = true;
                return Some(Err(error));
            }
            self.rows_read += 1;
            batch_rows += 1;
        }
        if batch_rows == 0 {
            None
        } else {
            Some(finish_batch(&self.schema, columns))
        }
    }
}

fn conversion_error(row: u64, column: &str, data_type: DataType, value: &str) -> FormatError {
    FormatError::Conversion {
        row,
        column: column.to_owned(),
        data_type,
        value: value.to_owned(),
    }
}

/// Validates and stages an entire CSV stream before appending any destination rows.
pub fn ingest_csv<R: BufRead>(
    input: R,
    destination: &mut Table,
    options: CsvOptions,
) -> Result<u64, FormatError> {
    let batches = CsvBatchReader::new(input, destination.schema(), options)?;
    ingest_batches(batches, destination)
}

/// Serializes a complete table as RFC 4180-style CSV with LF record endings.
pub fn export_csv<W: Write>(
    output: W,
    table: &Table,
    options: &CsvExportOptions,
) -> Result<(), FormatError> {
    write_csv_columns(
        output,
        table.schema(),
        table.columns(),
        table.rows(),
        options,
    )
}

/// Serializes one typed batch as CSV.
pub fn write_csv_batch<W: Write>(
    output: W,
    schema: &Schema,
    batch: &ColumnBatch,
    options: &CsvExportOptions,
) -> Result<(), FormatError> {
    ColumnBatch::validate(schema, batch.columns())?;
    write_csv_columns(output, schema, batch.columns(), batch.rows(), options)
}

fn write_csv_columns<W: Write>(
    mut output: W,
    schema: &Schema,
    columns: &[Column],
    rows: usize,
    options: &CsvExportOptions,
) -> Result<(), FormatError> {
    options.validate()?;
    if options.include_header {
        for (index, field) in schema.fields().iter().enumerate() {
            if index != 0 {
                output.write_all(&[options.delimiter])?;
            }
            write_csv_field(
                &mut output,
                field.name(),
                options.delimiter,
                &options.null_token,
            )?;
        }
        output.write_all(b"\n")?;
    }
    for row in 0..rows {
        for (index, (column, field)) in columns.iter().zip(schema.fields()).enumerate() {
            if index != 0 {
                output.write_all(&[options.delimiter])?;
            }
            match column {
                Column::Int64(values) => match values[row] {
                    Some(value) => write_csv_field(
                        &mut output,
                        &value.to_string(),
                        options.delimiter,
                        &options.null_token,
                    )?,
                    None => output.write_all(options.null_token.as_bytes())?,
                },
                Column::Float64(values) => match values[row] {
                    Some(value) if value.is_finite() => write_csv_field(
                        &mut output,
                        &value.to_string(),
                        options.delimiter,
                        &options.null_token,
                    )?,
                    Some(value) => {
                        return Err(conversion_error(
                            row as u64 + 1,
                            field.name(),
                            DataType::Float64,
                            &value.to_string(),
                        ));
                    }
                    None => output.write_all(options.null_token.as_bytes())?,
                },
                Column::Bool(values) => match values[row] {
                    Some(value) => write_csv_field(
                        &mut output,
                        if value { "true" } else { "false" },
                        options.delimiter,
                        &options.null_token,
                    )?,
                    None => output.write_all(options.null_token.as_bytes())?,
                },
                Column::String(values) => match &values[row] {
                    Some(value) => {
                        write_csv_field(&mut output, value, options.delimiter, &options.null_token)?
                    }
                    None => output.write_all(options.null_token.as_bytes())?,
                },
            }
        }
        output.write_all(b"\n")?;
    }
    Ok(())
}

fn write_csv_field<W: Write>(
    output: &mut W,
    value: &str,
    delimiter: u8,
    null_token: &str,
) -> Result<(), FormatError> {
    let quote = value == null_token
        || value
            .as_bytes()
            .iter()
            .any(|byte| matches!(*byte, b'"' | b'\r' | b'\n') || *byte == delimiter);
    if !quote {
        output.write_all(value.as_bytes())?;
        return Ok(());
    }
    output.write_all(b"\"")?;
    for (index, part) in value.split('"').enumerate() {
        if index != 0 {
            output.write_all(b"\"\"")?;
        }
        output.write_all(part.as_bytes())?;
    }
    output.write_all(b"\"")?;
    Ok(())
}
