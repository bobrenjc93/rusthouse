//! Bounded ingestion for one-column `JSONCompactEachRow` input.
//!
//! This intentionally small positional subset accepts one JSON array per
//! physical line. Every array contains exactly one JSON integer, or `null` for
//! a `Nullable(Int64)` target. The target must be an existing one-column
//! `Int64` or `Nullable(Int64)` table. Input is completely validated and
//! prepared before the caller commits any row.

use std::error::Error as StdError;
use std::fmt;
use std::str::Utf8Error;

use super::error::Error;
use super::storage::{PreparedInsertRows, Table};
use super::value::{DataType, Value};

/// Default maximum size of one `JSONCompactEachRow` ingestion operation.
pub const DEFAULT_MAX_JSON_COMPACT_EACH_ROW_BYTES: usize = 8 * 1024 * 1024;
/// Default maximum number of rows in one `JSONCompactEachRow` input.
pub const DEFAULT_MAX_JSON_COMPACT_EACH_ROW_ROWS: usize = 100_000;
/// Default maximum number of values in one `JSONCompactEachRow` input.
pub const DEFAULT_MAX_JSON_COMPACT_EACH_ROW_VALUES: usize = 100_000;

/// Resource bounds for one `JSONCompactEachRow` ingestion operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonCompactEachRowIngestLimits {
    /// Maximum complete input size, in bytes.
    pub max_bytes: usize,
    /// Maximum number of physical data rows.
    pub max_rows: usize,
    /// Maximum total number of positional values.
    pub max_values: usize,
}

impl JsonCompactEachRowIngestLimits {
    /// Creates explicit complete-input, row, and value bounds.
    #[must_use]
    pub const fn new(max_bytes: usize, max_rows: usize, max_values: usize) -> Self {
        Self {
            max_bytes,
            max_rows,
            max_values,
        }
    }
}

impl Default for JsonCompactEachRowIngestLimits {
    fn default() -> Self {
        Self::new(
            DEFAULT_MAX_JSON_COMPACT_EACH_ROW_BYTES,
            DEFAULT_MAX_JSON_COMPACT_EACH_ROW_ROWS,
            DEFAULT_MAX_JSON_COMPACT_EACH_ROW_VALUES,
        )
    }
}

/// A failure while validating or appending one-column `JSONCompactEachRow` input.
///
/// Line numbers and positional value indexes are one-based. JSON syntax
/// columns are one-based byte positions within the physical line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonCompactEachRowIngestError {
    /// The complete byte input exceeds its configured bound.
    ByteLimitExceeded { bytes: usize, max_bytes: usize },
    /// The input is not valid UTF-8.
    InvalidUtf8 { valid_up_to: usize },
    /// The target does not have exactly one physical column.
    UnsupportedColumnCount { actual: usize },
    /// The sole target column is not `Int64` or `Nullable(Int64)`.
    UnsupportedColumnType { column: String, actual: DataType },
    /// A physical row crosses the configured row bound.
    RowLimitExceeded {
        line: usize,
        rows: usize,
        max_rows: usize,
    },
    /// A physical row crosses the configured total-value bound.
    ValueLimitExceeded {
        line: usize,
        values: usize,
        max_values: usize,
    },
    /// A row is not a syntactically complete JSON array.
    InvalidJson { line: usize, column: usize },
    /// A row does not contain exactly one positional value.
    WrongColumnCount {
        line: usize,
        expected: usize,
        actual: usize,
    },
    /// The sole value is valid JSON but is not an integer or `null`.
    InvalidValue {
        line: usize,
        column: usize,
        expected: DataType,
    },
    /// A syntactically valid JSON integer is outside the `Int64` range.
    IntegerOverflow { line: usize, column: usize },
    /// JSON `null` was supplied for a non-nullable `Int64` column.
    NullNotAllowed { line: usize, column: usize },
    /// Table lookup, capacity preflight, WAL commit, or final append failed.
    Database(Error),
}

impl fmt::Display for JsonCompactEachRowIngestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ByteLimitExceeded { bytes, max_bytes } => write!(
                formatter,
                "JSONCompactEachRow input is {bytes} bytes, exceeding the limit of {max_bytes} bytes"
            ),
            Self::InvalidUtf8 { valid_up_to } => write!(
                formatter,
                "JSONCompactEachRow input is not valid UTF-8 at byte {valid_up_to}"
            ),
            Self::UnsupportedColumnCount { actual } => write!(
                formatter,
                "JSONCompactEachRow ingestion requires exactly one target column; found {actual}"
            ),
            Self::UnsupportedColumnType { column, actual } => write!(
                formatter,
                "JSONCompactEachRow ingestion requires column '{column}' to be Int64 or Nullable(Int64); found {actual}"
            ),
            Self::RowLimitExceeded {
                line,
                rows,
                max_rows,
            } => write!(
                formatter,
                "JSONCompactEachRow record at line {line} raises the row count to {rows}, exceeding the limit of {max_rows}"
            ),
            Self::ValueLimitExceeded {
                line,
                values,
                max_values,
            } => write!(
                formatter,
                "JSONCompactEachRow record at line {line} raises the value count to {values}, exceeding the limit of {max_values}"
            ),
            Self::InvalidJson { line, column } => write!(
                formatter,
                "JSONCompactEachRow record at line {line} is not valid JSON at byte column {column}"
            ),
            Self::WrongColumnCount {
                line,
                expected,
                actual,
            } => write!(
                formatter,
                "JSONCompactEachRow record at line {line} has {actual} values; expected {expected}"
            ),
            Self::InvalidValue {
                line,
                column,
                expected,
            } => write!(
                formatter,
                "JSONCompactEachRow value at line {line}, position {column} is not a valid {expected}"
            ),
            Self::IntegerOverflow { line, column } => write!(
                formatter,
                "JSONCompactEachRow integer at line {line}, position {column} is outside the Int64 range"
            ),
            Self::NullNotAllowed { line, column } => write!(
                formatter,
                "JSONCompactEachRow null at line {line}, position {column} is not allowed for Int64"
            ),
            Self::Database(error) => write!(
                formatter,
                "could not ingest JSONCompactEachRow input: {error}"
            ),
        }
    }
}

impl StdError for JsonCompactEachRowIngestError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

impl From<Error> for JsonCompactEachRowIngestError {
    fn from(error: Error) -> Self {
        Self::Database(error)
    }
}

pub(crate) fn parse_rows(
    table: &Table,
    input: &[u8],
    limits: JsonCompactEachRowIngestLimits,
) -> Result<PreparedInsertRows, JsonCompactEachRowIngestError> {
    if input.len() > limits.max_bytes {
        return Err(JsonCompactEachRowIngestError::ByteLimitExceeded {
            bytes: input.len(),
            max_bytes: limits.max_bytes,
        });
    }
    let input = std::str::from_utf8(input).map_err(invalid_utf8)?;
    validate_target(table)?;

    let mut rows = Vec::new();
    let mut value_count = 0_usize;
    for (offset, raw_line) in input.split_inclusive('\n').enumerate() {
        let line = offset.saturating_add(1);
        let row_count = rows.len().saturating_add(1);
        if row_count > limits.max_rows {
            return Err(JsonCompactEachRowIngestError::RowLimitExceeded {
                line,
                rows: row_count,
                max_rows: limits.max_rows,
            });
        }

        let physical_line = raw_line.strip_suffix('\n').unwrap_or(raw_line);
        let parsed = parse_array(physical_line, line)?;
        let next_value_count = value_count.saturating_add(parsed.value_count);
        if next_value_count > limits.max_values {
            return Err(JsonCompactEachRowIngestError::ValueLimitExceeded {
                line,
                values: next_value_count,
                max_values: limits.max_values,
            });
        }
        if parsed.value_count != 1 {
            return Err(JsonCompactEachRowIngestError::WrongColumnCount {
                line,
                expected: 1,
                actual: parsed.value_count,
            });
        }

        let value = match parsed
            .first_value
            .expect("a validated one-value array retains its first value")
        {
            ParsedJsonValue::Number {
                start,
                end,
                is_integer: true,
            } => physical_line[start..end]
                .parse::<i64>()
                .map(Value::Int64)
                .map_err(|_| JsonCompactEachRowIngestError::IntegerOverflow { line, column: 1 })?,
            ParsedJsonValue::Null if table.column_is_nullable_int64(0) => {
                Value::Null(DataType::Int64)
            }
            ParsedJsonValue::Null => {
                return Err(JsonCompactEachRowIngestError::NullNotAllowed { line, column: 1 });
            }
            ParsedJsonValue::Number { .. } | ParsedJsonValue::Other => {
                return Err(JsonCompactEachRowIngestError::InvalidValue {
                    line,
                    column: 1,
                    expected: DataType::Int64,
                });
            }
        };
        rows.push(vec![value]);
        value_count = next_value_count;
    }

    table
        .prepare_projected_rows(vec![0], rows)
        .map_err(Into::into)
}

fn validate_target(table: &Table) -> Result<(), JsonCompactEachRowIngestError> {
    if table.schema().len() != 1 {
        return Err(JsonCompactEachRowIngestError::UnsupportedColumnCount {
            actual: table.schema().len(),
        });
    }
    let column = &table.schema()[0];
    if column.data_type != DataType::Int64 {
        return Err(JsonCompactEachRowIngestError::UnsupportedColumnType {
            column: column.name.clone(),
            actual: column.data_type,
        });
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParsedJsonValue {
    Null,
    Number {
        start: usize,
        end: usize,
        is_integer: bool,
    },
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ParsedJsonArray {
    value_count: usize,
    first_value: Option<ParsedJsonValue>,
}

fn parse_array(
    line_contents: &str,
    line: usize,
) -> Result<ParsedJsonArray, JsonCompactEachRowIngestError> {
    let mut lexer = JsonLexer::new(line_contents.as_bytes(), line);
    let Some(root) = lexer.next_token()? else {
        return Err(invalid_json(line, 0));
    };
    if root.kind != JsonTokenKind::LeftBracket {
        return Err(invalid_json(line, root.start));
    }

    let mut frames = vec![JsonFrame::Array(JsonArrayState::ValueOrEnd)];
    let mut parsed = ParsedJsonArray {
        value_count: 0,
        first_value: None,
    };
    while let Some(frame) = frames.last().copied() {
        let Some(token) = lexer.next_token()? else {
            return Err(invalid_json(line, line_contents.len()));
        };
        match frame {
            JsonFrame::Array(JsonArrayState::ValueOrEnd) => {
                if token.kind == JsonTokenKind::RightBracket {
                    close_json_frame(&mut frames);
                } else {
                    record_root_array_value(&frames, token, &mut parsed);
                    begin_json_value(token, &mut frames, line)?;
                }
            }
            JsonFrame::Array(JsonArrayState::Value) => {
                record_root_array_value(&frames, token, &mut parsed);
                begin_json_value(token, &mut frames, line)?;
            }
            JsonFrame::Array(JsonArrayState::CommaOrEnd) => match token.kind {
                JsonTokenKind::Comma => {
                    *frames.last_mut().expect("the copied frame still exists") =
                        JsonFrame::Array(JsonArrayState::Value);
                }
                JsonTokenKind::RightBracket => close_json_frame(&mut frames),
                _ => return Err(invalid_json(line, token.start)),
            },
            JsonFrame::Object(JsonObjectState::KeyOrEnd) => match token.kind {
                JsonTokenKind::String => {
                    *frames.last_mut().expect("the copied frame still exists") =
                        JsonFrame::Object(JsonObjectState::Colon);
                }
                JsonTokenKind::RightBrace => close_json_frame(&mut frames),
                _ => return Err(invalid_json(line, token.start)),
            },
            JsonFrame::Object(JsonObjectState::Key) => {
                if token.kind != JsonTokenKind::String {
                    return Err(invalid_json(line, token.start));
                }
                *frames.last_mut().expect("the copied frame still exists") =
                    JsonFrame::Object(JsonObjectState::Colon);
            }
            JsonFrame::Object(JsonObjectState::Colon) => {
                if token.kind != JsonTokenKind::Colon {
                    return Err(invalid_json(line, token.start));
                }
                *frames.last_mut().expect("the copied frame still exists") =
                    JsonFrame::Object(JsonObjectState::Value);
            }
            JsonFrame::Object(JsonObjectState::Value) => {
                begin_json_value(token, &mut frames, line)?;
            }
            JsonFrame::Object(JsonObjectState::CommaOrEnd) => match token.kind {
                JsonTokenKind::Comma => {
                    *frames.last_mut().expect("the copied frame still exists") =
                        JsonFrame::Object(JsonObjectState::Key);
                }
                JsonTokenKind::RightBrace => close_json_frame(&mut frames),
                _ => return Err(invalid_json(line, token.start)),
            },
        }
    }

    if let Some(trailing) = lexer.next_token()? {
        return Err(invalid_json(line, trailing.start));
    }
    Ok(parsed)
}

fn record_root_array_value(frames: &[JsonFrame], token: JsonToken, parsed: &mut ParsedJsonArray) {
    if frames.len() != 1 {
        return;
    }
    parsed.value_count = parsed.value_count.saturating_add(1);
    if parsed.first_value.is_none() {
        parsed.first_value = Some(match token.kind {
            JsonTokenKind::Null => ParsedJsonValue::Null,
            JsonTokenKind::Number { is_integer } => ParsedJsonValue::Number {
                start: token.start,
                end: token.end,
                is_integer,
            },
            _ => ParsedJsonValue::Other,
        });
    }
}

fn begin_json_value(
    token: JsonToken,
    frames: &mut Vec<JsonFrame>,
    line: usize,
) -> Result<(), JsonCompactEachRowIngestError> {
    match frames
        .last_mut()
        .expect("a JSON value always has a parent frame")
    {
        JsonFrame::Array(state) => *state = JsonArrayState::CommaOrEnd,
        JsonFrame::Object(state) => *state = JsonObjectState::CommaOrEnd,
    }
    match token.kind {
        JsonTokenKind::LeftBracket => {
            frames.push(JsonFrame::Array(JsonArrayState::ValueOrEnd));
        }
        JsonTokenKind::LeftBrace => {
            frames.push(JsonFrame::Object(JsonObjectState::KeyOrEnd));
        }
        JsonTokenKind::String
        | JsonTokenKind::Number { .. }
        | JsonTokenKind::True
        | JsonTokenKind::False
        | JsonTokenKind::Null => {}
        _ => return Err(invalid_json(line, token.start)),
    }
    Ok(())
}

fn close_json_frame(frames: &mut Vec<JsonFrame>) {
    frames.pop().expect("a closing token has a matching frame");
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JsonFrame {
    Array(JsonArrayState),
    Object(JsonObjectState),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JsonArrayState {
    ValueOrEnd,
    Value,
    CommaOrEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JsonObjectState {
    KeyOrEnd,
    Key,
    Colon,
    Value,
    CommaOrEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct JsonToken {
    kind: JsonTokenKind,
    start: usize,
    end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JsonTokenKind {
    LeftBracket,
    RightBracket,
    LeftBrace,
    RightBrace,
    Colon,
    Comma,
    String,
    Number { is_integer: bool },
    True,
    False,
    Null,
}

struct JsonLexer<'a> {
    bytes: &'a [u8],
    cursor: usize,
    line: usize,
}

impl<'a> JsonLexer<'a> {
    fn new(bytes: &'a [u8], line: usize) -> Self {
        Self {
            bytes,
            cursor: 0,
            line,
        }
    }

    fn next_token(&mut self) -> Result<Option<JsonToken>, JsonCompactEachRowIngestError> {
        self.cursor = skip_json_whitespace(self.bytes, self.cursor);
        let start = self.cursor;
        let Some(&byte) = self.bytes.get(start) else {
            return Ok(None);
        };
        let kind = match byte {
            b'[' => JsonTokenKind::LeftBracket,
            b']' => JsonTokenKind::RightBracket,
            b'{' => JsonTokenKind::LeftBrace,
            b'}' => JsonTokenKind::RightBrace,
            b':' => JsonTokenKind::Colon,
            b',' => JsonTokenKind::Comma,
            b'"' => return self.lex_string().map(Some),
            b'-' | b'0'..=b'9' => return self.lex_number().map(Some),
            b't' => return self.lex_keyword(b"true", JsonTokenKind::True).map(Some),
            b'f' => return self.lex_keyword(b"false", JsonTokenKind::False).map(Some),
            b'n' => return self.lex_keyword(b"null", JsonTokenKind::Null).map(Some),
            _ => return Err(invalid_json(self.line, start)),
        };
        self.cursor += 1;
        Ok(Some(JsonToken {
            kind,
            start,
            end: self.cursor,
        }))
    }

    fn lex_keyword(
        &mut self,
        keyword: &[u8],
        kind: JsonTokenKind,
    ) -> Result<JsonToken, JsonCompactEachRowIngestError> {
        let start = self.cursor;
        let end = start.saturating_add(keyword.len());
        if self.bytes.get(start..end) != Some(keyword) {
            return Err(invalid_json(self.line, start));
        }
        self.cursor = end;
        Ok(JsonToken { kind, start, end })
    }

    fn lex_string(&mut self) -> Result<JsonToken, JsonCompactEachRowIngestError> {
        let start = self.cursor;
        self.cursor += 1;
        while let Some(&byte) = self.bytes.get(self.cursor) {
            match byte {
                b'"' => {
                    self.cursor += 1;
                    return Ok(JsonToken {
                        kind: JsonTokenKind::String,
                        start,
                        end: self.cursor,
                    });
                }
                b'\\' => {
                    self.cursor += 1;
                    let Some(&escaped) = self.bytes.get(self.cursor) else {
                        return Err(invalid_json(self.line, self.cursor));
                    };
                    match escaped {
                        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => {
                            self.cursor += 1;
                        }
                        b'u' => {
                            self.cursor += 1;
                            let unicode_end = self.cursor.saturating_add(4);
                            let Some(hex) = self.bytes.get(self.cursor..unicode_end) else {
                                return Err(invalid_json(self.line, self.cursor));
                            };
                            if !hex.iter().all(u8::is_ascii_hexdigit) {
                                return Err(invalid_json(self.line, self.cursor));
                            }
                            self.cursor = unicode_end;
                        }
                        _ => return Err(invalid_json(self.line, self.cursor)),
                    }
                }
                0x00..=0x1f => return Err(invalid_json(self.line, self.cursor)),
                _ => self.cursor += 1,
            }
        }
        Err(invalid_json(self.line, self.cursor))
    }

    fn lex_number(&mut self) -> Result<JsonToken, JsonCompactEachRowIngestError> {
        let start = self.cursor;
        if self.bytes.get(self.cursor) == Some(&b'-') {
            self.cursor += 1;
        }

        match self.bytes.get(self.cursor) {
            Some(b'0') => {
                self.cursor += 1;
                if matches!(self.bytes.get(self.cursor), Some(b'0'..=b'9')) {
                    return Err(invalid_json(self.line, self.cursor));
                }
            }
            Some(b'1'..=b'9') => {
                self.cursor += 1;
                while matches!(self.bytes.get(self.cursor), Some(b'0'..=b'9')) {
                    self.cursor += 1;
                }
            }
            _ => return Err(invalid_json(self.line, self.cursor)),
        }

        let mut is_integer = true;
        if self.bytes.get(self.cursor) == Some(&b'.') {
            is_integer = false;
            self.cursor += 1;
            let fraction_start = self.cursor;
            while matches!(self.bytes.get(self.cursor), Some(b'0'..=b'9')) {
                self.cursor += 1;
            }
            if self.cursor == fraction_start {
                return Err(invalid_json(self.line, self.cursor));
            }
        }
        if matches!(self.bytes.get(self.cursor), Some(b'e' | b'E')) {
            is_integer = false;
            self.cursor += 1;
            if matches!(self.bytes.get(self.cursor), Some(b'+' | b'-')) {
                self.cursor += 1;
            }
            let exponent_start = self.cursor;
            while matches!(self.bytes.get(self.cursor), Some(b'0'..=b'9')) {
                self.cursor += 1;
            }
            if self.cursor == exponent_start {
                return Err(invalid_json(self.line, self.cursor));
            }
        }

        Ok(JsonToken {
            kind: JsonTokenKind::Number { is_integer },
            start,
            end: self.cursor,
        })
    }
}

fn skip_json_whitespace(bytes: &[u8], mut cursor: usize) -> usize {
    while matches!(bytes.get(cursor), Some(b' ' | b'\t' | b'\r')) {
        cursor += 1;
    }
    cursor
}

fn invalid_json(line: usize, zero_based_column: usize) -> JsonCompactEachRowIngestError {
    JsonCompactEachRowIngestError::InvalidJson {
        line,
        column: zero_based_column.saturating_add(1),
    }
}

fn invalid_utf8(error: Utf8Error) -> JsonCompactEachRowIngestError {
    JsonCompactEachRowIngestError::InvalidUtf8 {
        valid_up_to: error.valid_up_to(),
    }
}
