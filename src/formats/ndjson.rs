use super::{
    FormatError, FormatLimits, LimitKind, LimitedInput, empty_columns, finish_batch,
    ingest_batches, push_null,
};
use crate::storage::{Column, ColumnBatch, DataType, Schema, Table};
use std::io::{BufRead, Write};

/// Settings for schema-driven newline-delimited JSON decoding.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NdjsonOptions {
    pub limits: FormatLimits,
}

struct NdjsonStream<R> {
    input: LimitedInput<R>,
    max_record_bytes: usize,
}

impl<R: BufRead> NdjsonStream<R> {
    fn new(input: R, options: &NdjsonOptions) -> Self {
        Self {
            input: LimitedInput::new(input, options.limits.max_input_bytes),
            max_record_bytes: options.limits.max_record_bytes,
        }
    }

    fn read_record(&mut self, row: u64) -> Result<Option<Vec<u8>>, FormatError> {
        let mut bytes = Vec::new();
        loop {
            match self.input.read_byte()? {
                Some(b'\n') => break,
                Some(byte) => {
                    if bytes.len() == self.max_record_bytes {
                        return Err(FormatError::LimitExceeded {
                            kind: LimitKind::RecordBytes,
                            limit: self.max_record_bytes as u64,
                            row: Some(row),
                        });
                    }
                    bytes.push(byte);
                }
                None if bytes.is_empty() => return Ok(None),
                None => break,
            }
        }
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        if bytes.is_empty() {
            return Err(FormatError::JsonSyntax {
                row,
                message: "blank lines are not NDJSON records".to_owned(),
            });
        }
        Ok(Some(bytes))
    }
}

#[derive(Debug)]
enum JsonValue {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Complex,
}

impl JsonValue {
    fn description(&self) -> String {
        match self {
            Self::Null => "null".to_owned(),
            Self::Bool(value) => value.to_string(),
            Self::Number(value) | Self::String(value) => value.clone(),
            Self::Complex => "nested JSON value".to_owned(),
        }
    }
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    position: usize,
    limits: &'a FormatLimits,
    row: u64,
}

impl<'a> JsonParser<'a> {
    fn new(bytes: &'a [u8], limits: &'a FormatLimits, row: u64) -> Self {
        Self {
            bytes,
            position: 0,
            limits,
            row,
        }
    }

    fn parse_root_object(&mut self) -> Result<Vec<(String, JsonValue)>, FormatError> {
        self.skip_whitespace();
        if self.peek() != Some(b'{') {
            return Err(self.syntax("each NDJSON record must be a JSON object"));
        }
        self.check_depth(1)?;
        self.position += 1;
        self.skip_whitespace();
        let mut fields = Vec::new();
        if self.take(b'}') {
            self.finish_document()?;
            return Ok(fields);
        }
        loop {
            if fields.len() == self.limits.max_fields_per_row {
                return Err(FormatError::LimitExceeded {
                    kind: LimitKind::FieldsPerRow,
                    limit: self.limits.max_fields_per_row as u64,
                    row: Some(self.row),
                });
            }
            let name = self.parse_string()?;
            self.skip_whitespace();
            self.expect(b':', "expected ':' after object field name")?;
            self.skip_whitespace();
            let value_start = self.position;
            let value = self.parse_value(1)?;
            let value_bytes = self.position - value_start;
            if value_bytes > self.limits.max_field_bytes {
                return Err(FormatError::LimitExceeded {
                    kind: LimitKind::FieldBytes,
                    limit: self.limits.max_field_bytes as u64,
                    row: Some(self.row),
                });
            }
            fields.push((name, value));
            self.skip_whitespace();
            if self.take(b'}') {
                break;
            }
            self.expect(b',', "expected ',' or '}' after object field")?;
            self.skip_whitespace();
        }
        self.finish_document()?;
        Ok(fields)
    }

    fn finish_document(&mut self) -> Result<(), FormatError> {
        self.skip_whitespace();
        if self.position != self.bytes.len() {
            return Err(self.syntax("trailing bytes after JSON object"));
        }
        Ok(())
    }

    fn parse_value(&mut self, depth: usize) -> Result<JsonValue, FormatError> {
        self.skip_whitespace();
        match self.peek() {
            Some(b'"') => self.parse_string().map(JsonValue::String),
            Some(b't') => {
                self.parse_literal(b"true")?;
                Ok(JsonValue::Bool(true))
            }
            Some(b'f') => {
                self.parse_literal(b"false")?;
                Ok(JsonValue::Bool(false))
            }
            Some(b'n') => {
                self.parse_literal(b"null")?;
                Ok(JsonValue::Null)
            }
            Some(b'{') => {
                self.parse_nested_object(depth + 1)?;
                Ok(JsonValue::Complex)
            }
            Some(b'[') => {
                self.parse_array(depth + 1)?;
                Ok(JsonValue::Complex)
            }
            Some(b'-' | b'0'..=b'9') => self.parse_number().map(JsonValue::Number),
            Some(_) => Err(self.syntax("expected a JSON value")),
            None => Err(self.syntax("unexpected end of JSON value")),
        }
    }

    fn parse_nested_object(&mut self, depth: usize) -> Result<(), FormatError> {
        self.check_depth(depth)?;
        self.position += 1;
        self.skip_whitespace();
        if self.take(b'}') {
            return Ok(());
        }
        let mut fields = 0_usize;
        loop {
            if fields == self.limits.max_fields_per_row {
                return Err(FormatError::LimitExceeded {
                    kind: LimitKind::FieldsPerRow,
                    limit: self.limits.max_fields_per_row as u64,
                    row: Some(self.row),
                });
            }
            self.parse_string()?;
            self.skip_whitespace();
            self.expect(b':', "expected ':' after object field name")?;
            self.parse_value(depth)?;
            fields += 1;
            self.skip_whitespace();
            if self.take(b'}') {
                return Ok(());
            }
            self.expect(b',', "expected ',' or '}' after object field")?;
            self.skip_whitespace();
        }
    }

    fn parse_array(&mut self, depth: usize) -> Result<(), FormatError> {
        self.check_depth(depth)?;
        self.position += 1;
        self.skip_whitespace();
        if self.take(b']') {
            return Ok(());
        }
        loop {
            self.parse_value(depth)?;
            self.skip_whitespace();
            if self.take(b']') {
                return Ok(());
            }
            self.expect(b',', "expected ',' or ']' after array value")?;
            self.skip_whitespace();
        }
    }

    fn parse_string(&mut self) -> Result<String, FormatError> {
        self.expect(b'"', "expected a JSON string")?;
        let mut decoded = Vec::new();
        loop {
            let byte = self
                .next()
                .ok_or_else(|| self.syntax("unterminated JSON string"))?;
            match byte {
                b'"' => {
                    return String::from_utf8(decoded)
                        .map_err(|_| self.syntax("JSON string is not valid UTF-8"));
                }
                b'\\' => {
                    let escape = self
                        .next()
                        .ok_or_else(|| self.syntax("unterminated JSON escape"))?;
                    match escape {
                        b'"' | b'\\' | b'/' => self.push_decoded(&mut decoded, escape)?,
                        b'b' => self.push_decoded(&mut decoded, 0x08)?,
                        b'f' => self.push_decoded(&mut decoded, 0x0c)?,
                        b'n' => self.push_decoded(&mut decoded, b'\n')?,
                        b'r' => self.push_decoded(&mut decoded, b'\r')?,
                        b't' => self.push_decoded(&mut decoded, b'\t')?,
                        b'u' => {
                            let first = self.parse_hex_quad()?;
                            let scalar = if (0xd800..=0xdbff).contains(&first) {
                                if self.next() != Some(b'\\') || self.next() != Some(b'u') {
                                    return Err(self.syntax(
                                        "high UTF-16 surrogate must be followed by a low surrogate",
                                    ));
                                }
                                let second = self.parse_hex_quad()?;
                                if !(0xdc00..=0xdfff).contains(&second) {
                                    return Err(self.syntax("invalid low UTF-16 surrogate"));
                                }
                                0x10000
                                    + (((u32::from(first) - 0xd800) << 10)
                                        | (u32::from(second) - 0xdc00))
                            } else if (0xdc00..=0xdfff).contains(&first) {
                                return Err(self.syntax("unexpected low UTF-16 surrogate"));
                            } else {
                                u32::from(first)
                            };
                            let character = char::from_u32(scalar)
                                .ok_or_else(|| self.syntax("invalid Unicode scalar"))?;
                            let mut buffer = [0_u8; 4];
                            for byte in character.encode_utf8(&mut buffer).as_bytes() {
                                self.push_decoded(&mut decoded, *byte)?;
                            }
                        }
                        _ => return Err(self.syntax("invalid JSON string escape")),
                    }
                }
                0x00..=0x1f => {
                    return Err(self.syntax("unescaped control byte in JSON string"));
                }
                _ => self.push_decoded(&mut decoded, byte)?,
            }
        }
    }

    fn push_decoded(&self, decoded: &mut Vec<u8>, byte: u8) -> Result<(), FormatError> {
        if decoded.len() == self.limits.max_string_bytes {
            return Err(FormatError::LimitExceeded {
                kind: LimitKind::StringBytes,
                limit: self.limits.max_string_bytes as u64,
                row: Some(self.row),
            });
        }
        decoded.push(byte);
        Ok(())
    }

    fn parse_hex_quad(&mut self) -> Result<u16, FormatError> {
        let mut value = 0_u16;
        for _ in 0..4 {
            let digit = self
                .next()
                .and_then(|byte| (byte as char).to_digit(16))
                .ok_or_else(|| self.syntax("invalid \\u escape"))?;
            value = (value << 4) | digit as u16;
        }
        Ok(value)
    }

    fn parse_number(&mut self) -> Result<String, FormatError> {
        let start = self.position;
        self.take(b'-');
        match self.peek() {
            Some(b'0') => {
                self.position += 1;
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(self.syntax("leading zero in JSON number"));
                }
            }
            Some(b'1'..=b'9') => {
                self.position += 1;
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.position += 1;
                }
            }
            _ => return Err(self.syntax("invalid JSON number")),
        }
        if self.take(b'.') {
            let fraction_start = self.position;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.position += 1;
            }
            if self.position == fraction_start {
                return Err(self.syntax("JSON fraction requires a digit"));
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.position += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.position += 1;
            }
            let exponent_start = self.position;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.position += 1;
            }
            if self.position == exponent_start {
                return Err(self.syntax("JSON exponent requires a digit"));
            }
        }
        Ok(std::str::from_utf8(&self.bytes[start..self.position])
            .expect("JSON number grammar is ASCII")
            .to_owned())
    }

    fn parse_literal(&mut self, literal: &[u8]) -> Result<(), FormatError> {
        if self.bytes.get(self.position..self.position + literal.len()) != Some(literal) {
            return Err(self.syntax("invalid JSON literal"));
        }
        self.position += literal.len();
        Ok(())
    }

    fn check_depth(&self, depth: usize) -> Result<(), FormatError> {
        if depth > self.limits.max_nesting_depth {
            return Err(FormatError::LimitExceeded {
                kind: LimitKind::NestingDepth,
                limit: self.limits.max_nesting_depth as u64,
                row: Some(self.row),
            });
        }
        Ok(())
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.position += 1;
        }
    }

    fn expect(&mut self, byte: u8, message: &str) -> Result<(), FormatError> {
        if self.take(byte) {
            Ok(())
        } else {
            Err(self.syntax(message))
        }
    }

    fn take(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.position += 1;
        Some(byte)
    }

    fn syntax(&self, message: &str) -> FormatError {
        FormatError::JsonSyntax {
            row: self.row,
            message: format!("{message} at byte {}", self.position),
        }
    }
}

/// An iterator of typed NDJSON batches. It retains at most one line and one batch.
pub struct NdjsonBatchReader<R> {
    stream: NdjsonStream<R>,
    schema: Schema,
    options: NdjsonOptions,
    rows_read: u64,
    finished: bool,
}

impl<R: BufRead> NdjsonBatchReader<R> {
    pub fn new(input: R, schema: &Schema, options: NdjsonOptions) -> Result<Self, FormatError> {
        options.limits.validate(schema)?;
        Ok(Self {
            stream: NdjsonStream::new(input, &options),
            schema: schema.clone(),
            options,
            rows_read: 0,
            finished: false,
        })
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    fn append_record(
        &self,
        columns: &mut [Column],
        bytes: &[u8],
        row: u64,
    ) -> Result<(), FormatError> {
        let fields = JsonParser::new(bytes, &self.options.limits, row).parse_root_object()?;
        let mut values: Vec<Option<JsonValue>> = (0..self.schema.len()).map(|_| None).collect();
        for (name, value) in fields {
            let Some(index) = self.schema.index_of(&name) else {
                return Err(FormatError::UnknownField { row, field: name });
            };
            if values[index].is_some() {
                return Err(FormatError::DuplicateField { row, field: name });
            }
            values[index] = Some(value);
        }

        for (index, (column, field)) in columns.iter_mut().zip(self.schema.fields()).enumerate() {
            let Some(value) = values[index].take() else {
                return Err(FormatError::MissingField {
                    row,
                    field: field.name().to_owned(),
                });
            };
            match (column, value) {
                (column, JsonValue::Null) => {
                    push_null(column, field.name(), field.is_nullable(), row)?;
                }
                (Column::Int64(target), JsonValue::Number(value)) => {
                    target.push(Some(value.parse().map_err(|_| {
                        conversion_error(row, field.name(), DataType::Int64, value)
                    })?));
                }
                (Column::Float64(target), JsonValue::Number(value)) => {
                    let parsed: f64 = value.parse().map_err(|_| {
                        conversion_error(row, field.name(), DataType::Float64, value.clone())
                    })?;
                    if !parsed.is_finite() {
                        return Err(conversion_error(
                            row,
                            field.name(),
                            DataType::Float64,
                            value,
                        ));
                    }
                    target.push(Some(parsed));
                }
                (Column::Bool(target), JsonValue::Bool(value)) => target.push(Some(value)),
                (Column::String(target), JsonValue::String(value)) => target.push(Some(value)),
                (column, value) => {
                    return Err(conversion_error(
                        row,
                        field.name(),
                        column.data_type(),
                        value.description(),
                    ));
                }
            }
        }
        Ok(())
    }
}

impl<R: BufRead> Iterator for NdjsonBatchReader<R> {
    type Item = Result<ColumnBatch, FormatError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        let mut columns = empty_columns(&self.schema, self.options.limits.batch_rows);
        let mut batch_rows = 0_usize;
        while batch_rows < self.options.limits.batch_rows {
            let row = self.rows_read + 1;
            let record = match self.stream.read_record(row) {
                Ok(Some(record)) => record,
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
            if let Err(error) = self.append_record(&mut columns, &record, row) {
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

fn conversion_error(row: u64, column: &str, data_type: DataType, value: String) -> FormatError {
    FormatError::Conversion {
        row,
        column: column.to_owned(),
        data_type,
        value,
    }
}

/// Validates and stages an entire NDJSON stream before appending destination rows.
pub fn ingest_ndjson<R: BufRead>(
    input: R,
    destination: &mut Table,
    options: NdjsonOptions,
) -> Result<u64, FormatError> {
    let batches = NdjsonBatchReader::new(input, destination.schema(), options)?;
    ingest_batches(batches, destination)
}

/// Serializes a complete table as one compact JSON object per line.
pub fn export_ndjson<W: Write>(output: W, table: &Table) -> Result<(), FormatError> {
    write_ndjson_columns(output, table.schema(), table.columns(), table.rows())
}

/// Serializes one typed batch as compact NDJSON.
pub fn write_ndjson_batch<W: Write>(
    output: W,
    schema: &Schema,
    batch: &ColumnBatch,
) -> Result<(), FormatError> {
    ColumnBatch::validate(schema, batch.columns())?;
    write_ndjson_columns(output, schema, batch.columns(), batch.rows())
}

fn write_ndjson_columns<W: Write>(
    mut output: W,
    schema: &Schema,
    columns: &[Column],
    rows: usize,
) -> Result<(), FormatError> {
    for row in 0..rows {
        output.write_all(b"{")?;
        for (index, (column, field)) in columns.iter().zip(schema.fields()).enumerate() {
            if index != 0 {
                output.write_all(b",")?;
            }
            write_json_string(&mut output, field.name())?;
            output.write_all(b":")?;
            match column {
                Column::Int64(values) => match values[row] {
                    Some(value) => write!(output, "{value}")?,
                    None => output.write_all(b"null")?,
                },
                Column::Float64(values) => match values[row] {
                    Some(value) if value.is_finite() => write!(output, "{value}")?,
                    Some(value) => {
                        return Err(conversion_error(
                            row as u64 + 1,
                            field.name(),
                            DataType::Float64,
                            value.to_string(),
                        ));
                    }
                    None => output.write_all(b"null")?,
                },
                Column::Bool(values) => match values[row] {
                    Some(value) => output.write_all(if value { b"true" } else { b"false" })?,
                    None => output.write_all(b"null")?,
                },
                Column::String(values) => match &values[row] {
                    Some(value) => write_json_string(&mut output, value)?,
                    None => output.write_all(b"null")?,
                },
            }
        }
        output.write_all(b"}\n")?;
    }
    Ok(())
}

fn write_json_string<W: Write>(output: &mut W, value: &str) -> Result<(), FormatError> {
    output.write_all(b"\"")?;
    for character in value.chars() {
        match character {
            '"' => output.write_all(b"\\\"")?,
            '\\' => output.write_all(b"\\\\")?,
            '\u{08}' => output.write_all(b"\\b")?,
            '\u{0c}' => output.write_all(b"\\f")?,
            '\n' => output.write_all(b"\\n")?,
            '\r' => output.write_all(b"\\r")?,
            '\t' => output.write_all(b"\\t")?,
            '\u{00}'..='\u{1f}' => write!(output, "\\u{:04x}", character as u32)?,
            _ => {
                let mut encoded = [0_u8; 4];
                output.write_all(character.encode_utf8(&mut encoded).as_bytes())?;
            }
        }
    }
    output.write_all(b"\"")?;
    Ok(())
}
