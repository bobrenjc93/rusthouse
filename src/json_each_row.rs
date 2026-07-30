use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};

use crate::error::{Error, Result};
use crate::storage::{ColumnDef, Table};
use crate::value::{DataType, Value};

const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;

pub(crate) fn read_file(path: &str, table_name: &str, schema: &[ColumnDef]) -> Result<Table> {
    let file = File::open(path).map_err(|error| Error::Io {
        context: format!("could not open COPY source '{path}'"),
        message: error.to_string(),
    })?;
    let mut reader = BufReader::new(file);
    let mut staged = Table::new(table_name.to_owned(), schema.to_vec())?;
    let schema_index = build_schema_index(schema);
    let mut record = 0;

    loop {
        let mut buffer = Vec::new();
        let bytes_read = (&mut reader)
            .take((MAX_RECORD_BYTES + 2) as u64)
            .read_until(b'\n', &mut buffer)
            .map_err(|error| Error::JsonEachRow {
                path: path.to_owned(),
                record: record + 1,
                message: format!("could not read record: {error}"),
            })?;
        if bytes_read == 0 {
            break;
        }
        record += 1;

        if buffer.last() == Some(&b'\n') {
            buffer.pop();
            if buffer.last() == Some(&b'\r') {
                buffer.pop();
            }
        }
        if buffer.len() > MAX_RECORD_BYTES {
            return Err(record_error(
                path,
                record,
                format!("record exceeds the {MAX_RECORD_BYTES}-byte limit"),
            ));
        }

        let text = std::str::from_utf8(&buffer)
            .map_err(|error| record_error(path, record, format!("record is not UTF-8: {error}")))?;
        if text.trim().is_empty() {
            continue;
        }
        let row = RecordParser::new(text, table_name, schema, &schema_index)
            .parse()
            .map_err(|message| record_error(path, record, message))?;
        staged
            .insert_row(row)
            .map_err(|error| record_error(path, record, error.to_string()))?;
    }

    Ok(staged)
}

fn build_schema_index(schema: &[ColumnDef]) -> HashMap<String, usize> {
    schema
        .iter()
        .enumerate()
        .map(|(index, field)| (field.name.to_ascii_lowercase(), index))
        .collect()
}

fn record_error(path: &str, record: usize, message: String) -> Error {
    Error::JsonEachRow {
        path: path.to_owned(),
        record,
        message,
    }
}

struct RecordParser<'a> {
    input: &'a str,
    position: usize,
    table_name: &'a str,
    schema: &'a [ColumnDef],
    schema_index: &'a HashMap<String, usize>,
}

impl<'a> RecordParser<'a> {
    fn new(
        input: &'a str,
        table_name: &'a str,
        schema: &'a [ColumnDef],
        schema_index: &'a HashMap<String, usize>,
    ) -> Self {
        Self {
            input,
            position: 0,
            table_name,
            schema,
            schema_index,
        }
    }

    fn parse(mut self) -> std::result::Result<Vec<Value>, String> {
        self.skip_whitespace();
        self.expect_byte(b'{', "expected a JSON object")?;
        self.skip_whitespace();

        let mut values = vec![None; self.schema.len()];
        if !self.eat_byte(b'}') {
            loop {
                self.skip_whitespace();
                if self.peek_byte() != Some(b'"') {
                    return self.error("expected a quoted object field name");
                }
                let name = self.parse_string()?;
                self.skip_whitespace();
                self.expect_byte(b':', "expected ':' after object field name")?;
                self.skip_whitespace();

                let Some(column) = self.schema_index.get(&name.to_ascii_lowercase()).copied()
                else {
                    return self.error(format!(
                        "unknown field '{name}' for table '{}'",
                        self.table_name
                    ));
                };
                if values[column].is_some() {
                    return self.error(format!("duplicate field '{name}'"));
                }
                values[column] = Some(self.parse_value(&self.schema[column])?);

                self.skip_whitespace();
                if self.eat_byte(b'}') {
                    break;
                }
                self.expect_byte(b',', "expected ',' or '}' after object field")?;
            }
        }

        self.skip_whitespace();
        if self.position != self.input.len() {
            return self.error("unexpected content after JSON object");
        }

        Ok(values
            .into_iter()
            .zip(self.schema)
            .map(|(value, field)| value.unwrap_or_else(|| default_value(field.data_type)))
            .collect())
    }

    fn parse_value(&mut self, field: &ColumnDef) -> std::result::Result<Value, String> {
        if self.consume_keyword("null") {
            return self.error(format!(
                "field '{}' is null but column '{}.{}' is not nullable",
                field.name, self.table_name, field.name
            ));
        }

        match field.data_type {
            DataType::String => {
                if self.peek_byte() != Some(b'"') {
                    return self.type_error(field, "String");
                }
                self.parse_string().map(Value::String)
            }
            DataType::Bool => {
                if self.consume_keyword("true") {
                    Ok(Value::Bool(true))
                } else if self.consume_keyword("false") {
                    Ok(Value::Bool(false))
                } else {
                    self.type_error(field, "Bool")
                }
            }
            DataType::Int64 => {
                if !self
                    .peek_byte()
                    .is_some_and(|byte| byte == b'-' || byte.is_ascii_digit())
                {
                    return self.type_error(field, "Int64");
                }
                let number = self
                    .parse_number()
                    .map_err(|message| format!("field '{}': {message}", field.name))?;
                if number.contains(['.', 'e', 'E']) {
                    return self.type_error(field, "Int64");
                }
                number.parse::<i64>().map(Value::Int64).map_err(|_| {
                    self.message(format!(
                        "field '{}' contains an Int64 outside the supported range",
                        field.name
                    ))
                })
            }
            DataType::Float64 => {
                if !self
                    .peek_byte()
                    .is_some_and(|byte| byte == b'-' || byte.is_ascii_digit())
                {
                    return self.type_error(field, "Float64");
                }
                let number = self
                    .parse_number()
                    .map_err(|message| format!("field '{}': {message}", field.name))?;
                let value = number.parse::<f64>().map_err(|_| {
                    self.message(format!("field '{}' is not a valid Float64", field.name))
                })?;
                if !value.is_finite() {
                    return self.error(format!(
                        "field '{}' contains a non-finite Float64",
                        field.name
                    ));
                }
                Ok(Value::Float64(value))
            }
        }
    }

    fn type_error<T>(&self, field: &ColumnDef, expected: &str) -> std::result::Result<T, String> {
        self.error(format!(
            "field '{}' has the wrong JSON type; expected {expected}",
            field.name
        ))
    }

    fn parse_number(&mut self) -> std::result::Result<&'a str, String> {
        let start = self.position;
        self.eat_byte(b'-');

        match self.peek_byte() {
            Some(b'0') => {
                self.position += 1;
                if self.peek_byte().is_some_and(|byte| byte.is_ascii_digit()) {
                    return self.error("invalid JSON number with a leading zero");
                }
            }
            Some(b'1'..=b'9') => {
                self.position += 1;
                while self.peek_byte().is_some_and(|byte| byte.is_ascii_digit()) {
                    self.position += 1;
                }
            }
            _ => return self.error("expected a JSON number"),
        }

        if self.eat_byte(b'.') {
            if !self.peek_byte().is_some_and(|byte| byte.is_ascii_digit()) {
                return self.error("expected a digit after the decimal point");
            }
            while self.peek_byte().is_some_and(|byte| byte.is_ascii_digit()) {
                self.position += 1;
            }
        }
        if self
            .peek_byte()
            .is_some_and(|byte| matches!(byte, b'e' | b'E'))
        {
            self.position += 1;
            if self
                .peek_byte()
                .is_some_and(|byte| matches!(byte, b'+' | b'-'))
            {
                self.position += 1;
            }
            if !self.peek_byte().is_some_and(|byte| byte.is_ascii_digit()) {
                return self.error("expected an exponent digit");
            }
            while self.peek_byte().is_some_and(|byte| byte.is_ascii_digit()) {
                self.position += 1;
            }
        }

        Ok(&self.input[start..self.position])
    }

    fn parse_string(&mut self) -> std::result::Result<String, String> {
        self.expect_byte(b'"', "expected a JSON string")?;
        let mut output = String::new();
        loop {
            let Some(byte) = self.peek_byte() else {
                return self.error("unterminated JSON string");
            };
            match byte {
                b'"' => {
                    self.position += 1;
                    return Ok(output);
                }
                b'\\' => {
                    self.position += 1;
                    let escaped = self.peek_byte().ok_or_else(|| {
                        self.message("unterminated JSON string escape".to_owned())
                    })?;
                    self.position += 1;
                    match escaped {
                        b'"' => output.push('"'),
                        b'\\' => output.push('\\'),
                        b'/' => output.push('/'),
                        b'b' => output.push('\u{0008}'),
                        b'f' => output.push('\u{000c}'),
                        b'n' => output.push('\n'),
                        b'r' => output.push('\r'),
                        b't' => output.push('\t'),
                        b'u' => output.push(self.parse_unicode_escape()?),
                        _ => return self.error("invalid JSON string escape"),
                    }
                }
                0x00..=0x1f => return self.error("unescaped control character in JSON string"),
                _ => {
                    let character = self.input[self.position..]
                        .chars()
                        .next()
                        .expect("position is on a UTF-8 character boundary");
                    output.push(character);
                    self.position += character.len_utf8();
                }
            }
        }
    }

    fn parse_unicode_escape(&mut self) -> std::result::Result<char, String> {
        let first = self.parse_hex_quad()?;
        let codepoint = if (0xd800..=0xdbff).contains(&first) {
            if !self.input[self.position..].starts_with("\\u") {
                return self.error("high surrogate must be followed by a low surrogate");
            }
            self.position += 2;
            let second = self.parse_hex_quad()?;
            if !(0xdc00..=0xdfff).contains(&second) {
                return self.error("high surrogate must be followed by a low surrogate");
            }
            0x1_0000 + ((u32::from(first) - 0xd800) << 10) + (u32::from(second) - 0xdc00)
        } else if (0xdc00..=0xdfff).contains(&first) {
            return self.error("low surrogate is missing a leading high surrogate");
        } else {
            u32::from(first)
        };
        char::from_u32(codepoint).ok_or_else(|| self.message("invalid Unicode escape".to_owned()))
    }

    fn parse_hex_quad(&mut self) -> std::result::Result<u16, String> {
        if self.position + 4 > self.input.len() {
            return self.error("incomplete Unicode escape");
        }
        let digits = &self.input.as_bytes()[self.position..self.position + 4];
        if !digits.iter().all(u8::is_ascii_hexdigit) {
            return self.error("invalid Unicode escape");
        }
        self.position += 4;
        let text = std::str::from_utf8(digits).expect("hex digits are UTF-8");
        u16::from_str_radix(text, 16).map_err(|_| self.message("invalid Unicode escape".to_owned()))
    }

    fn consume_keyword(&mut self, keyword: &str) -> bool {
        if self.input[self.position..].starts_with(keyword) {
            self.position += keyword.len();
            true
        } else {
            false
        }
    }

    fn skip_whitespace(&mut self) {
        while self
            .peek_byte()
            .is_some_and(|byte| matches!(byte, b' ' | b'\n' | b'\r' | b'\t'))
        {
            self.position += 1;
        }
    }

    fn peek_byte(&self) -> Option<u8> {
        self.input.as_bytes().get(self.position).copied()
    }

    fn eat_byte(&mut self, expected: u8) -> bool {
        if self.peek_byte() == Some(expected) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn expect_byte(&mut self, expected: u8, message: &str) -> std::result::Result<(), String> {
        if self.eat_byte(expected) {
            Ok(())
        } else {
            self.error(message)
        }
    }

    fn error<T>(&self, message: impl Into<String>) -> std::result::Result<T, String> {
        Err(self.message(message.into()))
    }

    fn message(&self, message: String) -> String {
        format!("{message} at byte {}", self.position)
    }
}

fn default_value(data_type: DataType) -> Value {
    match data_type {
        DataType::Int64 => Value::Int64(0),
        DataType::Float64 => Value::Float64(0.0),
        DataType::Bool => Value::Bool(false),
        DataType::String => Value::String(String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> Vec<ColumnDef> {
        vec![
            ColumnDef {
                name: "id".to_owned(),
                data_type: DataType::Int64,
            },
            ColumnDef {
                name: "label".to_owned(),
                data_type: DataType::String,
            },
            ColumnDef {
                name: "score".to_owned(),
                data_type: DataType::Float64,
            },
            ColumnDef {
                name: "active".to_owned(),
                data_type: DataType::Bool,
            },
        ]
    }

    #[test]
    fn parses_reordered_fields_escapes_and_defaults() {
        let schema = schema();
        let schema_index = build_schema_index(&schema);
        let row = RecordParser::new(
            r#"{"label":"line\nquote: \" snowman: \u2603 face: \ud83d\ude00","id":7}"#,
            "events",
            &schema,
            &schema_index,
        )
        .parse()
        .expect("valid record");

        assert_eq!(
            row,
            vec![
                Value::Int64(7),
                Value::String("line\nquote: \" snowman: ☃ face: 😀".to_owned()),
                Value::Float64(0.0),
                Value::Bool(false),
            ]
        );
    }

    #[test]
    fn rejects_duplicate_unknown_null_and_malformed_fields() {
        let cases = [
            (r#"{"id":1,"ID":2}"#, "duplicate field"),
            (r#"{"missing":1}"#, "unknown field"),
            (r#"{"label":null}"#, "not nullable"),
            (r#"{"id":1,}"#, "quoted object field name"),
            (r#"{"label":"\ud800"}"#, "high surrogate"),
        ];

        let schema = schema();
        let schema_index = build_schema_index(&schema);
        for (record, expected) in cases {
            let error = RecordParser::new(record, "events", &schema, &schema_index)
                .parse()
                .expect_err("record should fail");
            assert!(error.contains(expected), "unexpected error: {error}");
        }
    }
}
