use std::borrow::Cow;
use std::error::Error as StdError;
use std::{fmt, io};

use crate::batch::engine::QueryResult;
use crate::batch::value::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Table,
    Csv,
    Tsv,
    Json,
}

impl OutputFormat {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "table" => Some(Self::Table),
            "csv" => Some(Self::Csv),
            "tsv" => Some(Self::Tsv),
            "json" => Some(Self::Json),
            _ => None,
        }
    }
}

#[must_use]
pub fn render(result: &QueryResult, format: OutputFormat) -> String {
    match format {
        OutputFormat::Table => render_table(result),
        OutputFormat::Csv => render_csv(result),
        OutputFormat::Tsv => render_tsv(result),
        OutputFormat::Json => render_json(result),
    }
}

fn render_table(result: &QueryResult) -> String {
    let mut output = Vec::new();
    write_table(&mut output, result, usize::MAX)
        .expect("rendering an in-memory query result to a Vec cannot fail");
    String::from_utf8(output).expect("table rendering preserves UTF-8")
}

/// A typed failure while sizing or streaming a human-readable table.
#[derive(Debug)]
pub enum TableWriteError {
    /// Table borders or cell padding would exceed the formatted-output bound.
    OutputLimitExceeded { bytes: usize, max_bytes: usize },
    /// Writing the already size-checked table failed.
    Write(io::Error),
}

impl fmt::Display for TableWriteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutputLimitExceeded { bytes, max_bytes } => write!(
                formatter,
                "table output requires at least {bytes} bytes, exceeding the limit of {max_bytes} bytes"
            ),
            Self::Write(error) => write!(formatter, "could not write table output: {error}"),
        }
    }
}

impl StdError for TableWriteError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Write(error) => Some(error),
            Self::OutputLimitExceeded { .. } => None,
        }
    }
}

/// Streams one human-readable result table after checking its exact byte size.
///
/// The bound includes borders and alignment padding. Sizing completes before
/// the first byte is written, so a rejected table never produces partial
/// output. Cell contents are revisited while streaming but are never retained
/// as a second formatted copy of the query result.
pub fn write_table(
    output: &mut impl io::Write,
    result: &QueryResult,
    max_output_bytes: usize,
) -> Result<(), TableWriteError> {
    write_table_with_affixes(output, result, max_output_bytes, b"", b"")
}

pub(crate) fn write_table_with_affixes(
    output: &mut impl io::Write,
    result: &QueryResult,
    max_output_bytes: usize,
    prefix: &[u8],
    suffix: &[u8],
) -> Result<(), TableWriteError> {
    let layout = TableLayout::new(result, prefix.len(), suffix.len(), max_output_bytes)?;

    output.write_all(prefix).map_err(TableWriteError::Write)?;
    write_table_border(output, &layout.widths).map_err(TableWriteError::Write)?;
    output.write_all(b"\n").map_err(TableWriteError::Write)?;
    write_table_header(output, result, &layout.widths).map_err(TableWriteError::Write)?;
    write_table_border(output, &layout.widths).map_err(TableWriteError::Write)?;
    output.write_all(b"\n").map_err(TableWriteError::Write)?;
    for row in &result.rows {
        write_table_values(output, row, &layout.widths).map_err(TableWriteError::Write)?;
    }
    write_table_border(output, &layout.widths).map_err(TableWriteError::Write)?;
    output.write_all(suffix).map_err(TableWriteError::Write)
}

struct TableLayout {
    widths: Vec<usize>,
}

impl TableLayout {
    fn new(
        result: &QueryResult,
        prefix_bytes: usize,
        suffix_bytes: usize,
        max_output_bytes: usize,
    ) -> Result<Self, TableWriteError> {
        let Some(widths) = table_widths(result) else {
            return Err(output_limit_overflow(max_output_bytes));
        };
        let Some(table_bytes) = table_rendered_bytes(result, &widths) else {
            return Err(output_limit_overflow(max_output_bytes));
        };
        let Some(output_bytes) = prefix_bytes
            .checked_add(table_bytes)
            .and_then(|bytes| bytes.checked_add(suffix_bytes))
        else {
            return Err(output_limit_overflow(max_output_bytes));
        };
        if output_bytes > max_output_bytes {
            return Err(TableWriteError::OutputLimitExceeded {
                bytes: output_bytes,
                max_bytes: max_output_bytes,
            });
        }
        Ok(Self { widths })
    }
}

fn output_limit_overflow(max_output_bytes: usize) -> TableWriteError {
    TableWriteError::OutputLimitExceeded {
        bytes: max_output_bytes.saturating_add(1),
        max_bytes: max_output_bytes,
    }
}

fn table_widths(result: &QueryResult) -> Option<Vec<usize>> {
    let mut widths = result
        .columns
        .iter()
        .map(|column| table_text_metrics(&column.name).map(|metrics| metrics.characters))
        .collect::<Option<Vec<_>>>()?;
    for row in &result.rows {
        for (column, width) in widths.iter_mut().enumerate() {
            let value = table_value_text(&row[column]);
            let metrics = table_text_metrics(&value)?;
            *width = (*width).max(metrics.characters);
        }
    }
    Some(widths)
}

fn table_rendered_bytes(result: &QueryResult, widths: &[usize]) -> Option<usize> {
    let border_bytes = widths.iter().try_fold(1_usize, |bytes, width| {
        bytes.checked_add(width.checked_add(3)?)
    })?;
    let header_bytes = table_row_bytes(
        result.columns.iter().map(|column| column.name.as_str()),
        widths,
    )?;
    let mut bytes = border_bytes
        .checked_add(1)?
        .checked_add(header_bytes)?
        .checked_add(border_bytes)?
        .checked_add(1)?;
    for row in &result.rows {
        bytes = bytes.checked_add(table_row_bytes(row.iter().map(table_value_text), widths)?)?;
    }
    bytes.checked_add(border_bytes)
}

fn table_row_bytes<T>(values: impl Iterator<Item = T>, widths: &[usize]) -> Option<usize>
where
    T: AsRef<str>,
{
    values
        .zip(widths)
        .try_fold(1_usize, |bytes, (value, width)| {
            let metrics = table_text_metrics(value.as_ref())?;
            let padding = width.checked_sub(metrics.characters)?.checked_add(1)?;
            bytes
                .checked_add(1)?
                .checked_add(metrics.bytes)?
                .checked_add(padding)?
                .checked_add(1)
        })?
        .checked_add(1)
}

#[derive(Clone, Copy)]
struct TableTextMetrics {
    characters: usize,
    bytes: usize,
}

fn table_text_metrics(value: &str) -> Option<TableTextMetrics> {
    let mut metrics = TableTextMetrics {
        characters: 0,
        bytes: 0,
    };
    for character in value.chars() {
        let (characters, bytes) = match character {
            '\\' | '\n' | '\r' | '\t' => (2, 2),
            value if value.is_control() => {
                let significant_hex_digits =
                    (u32::BITS - (value as u32).leading_zeros()).div_ceil(4) as usize;
                let escaped_bytes = significant_hex_digits.max(4).checked_add(4)?;
                (escaped_bytes, escaped_bytes)
            }
            value => (1, value.len_utf8()),
        };
        metrics.characters = metrics.characters.checked_add(characters)?;
        metrics.bytes = metrics.bytes.checked_add(bytes)?;
    }
    Some(metrics)
}

fn table_value_text(value: &Value) -> Cow<'_, str> {
    match value {
        Value::Null(_) => Cow::Borrowed("NULL"),
        Value::Bool(true) => Cow::Borrowed("true"),
        Value::Bool(false) => Cow::Borrowed("false"),
        Value::String(value) => Cow::Borrowed(value),
        Value::Int64(_) | Value::Float64(_) => Cow::Owned(value.as_display_string()),
    }
}

const TABLE_DASHES: [u8; 1024] = [b'-'; 1024];
const TABLE_SPACES: [u8; 1024] = [b' '; 1024];

fn write_table_border(output: &mut impl io::Write, widths: &[usize]) -> io::Result<()> {
    output.write_all(b"+")?;
    for width in widths {
        write_repeated(output, &TABLE_DASHES, width + 2)?;
        output.write_all(b"+")?;
    }
    Ok(())
}

fn write_table_header(
    output: &mut impl io::Write,
    result: &QueryResult,
    widths: &[usize],
) -> io::Result<()> {
    output.write_all(b"|")?;
    for (column, width) in result.columns.iter().zip(widths) {
        write_table_cell(output, &column.name, *width)?;
    }
    output.write_all(b"\n")
}

fn write_table_values(
    output: &mut impl io::Write,
    row: &[Value],
    widths: &[usize],
) -> io::Result<()> {
    output.write_all(b"|")?;
    for (value, width) in row.iter().zip(widths) {
        write_table_cell(output, &table_value_text(value), *width)?;
    }
    output.write_all(b"\n")
}

fn write_table_cell(output: &mut impl io::Write, value: &str, width: usize) -> io::Result<()> {
    let metrics = table_text_metrics(value).expect("validated table text metrics cannot overflow");
    output.write_all(b" ")?;
    write_escaped_table_text(output, value)?;
    write_repeated(
        output,
        &TABLE_SPACES,
        width
            .checked_sub(metrics.characters)
            .and_then(|padding| padding.checked_add(1))
            .expect("validated table padding cannot overflow"),
    )?;
    output.write_all(b"|")
}

fn write_repeated(output: &mut impl io::Write, chunk: &[u8], mut count: usize) -> io::Result<()> {
    while count >= chunk.len() {
        output.write_all(chunk)?;
        count -= chunk.len();
    }
    output.write_all(&chunk[..count])
}

fn write_escaped_table_text(output: &mut impl io::Write, value: &str) -> io::Result<()> {
    let mut unescaped_start = 0;
    for (index, character) in value.char_indices() {
        let escaped = match character {
            '\\' => Some("\\\\"),
            '\n' => Some("\\n"),
            '\r' => Some("\\r"),
            '\t' => Some("\\t"),
            character if character.is_control() => {
                output.write_all(value[unescaped_start..index].as_bytes())?;
                write!(output, "\\u{{{:04x}}}", character as u32)?;
                unescaped_start = index + character.len_utf8();
                continue;
            }
            _ => None,
        };
        if let Some(escaped) = escaped {
            output.write_all(value[unescaped_start..index].as_bytes())?;
            output.write_all(escaped.as_bytes())?;
            unescaped_start = index + character.len_utf8();
        }
    }
    output.write_all(value[unescaped_start..].as_bytes())
}

fn render_csv(result: &QueryResult) -> String {
    let mut output = Vec::new();
    write_csv(&mut output, result).expect("writing CSV to a Vec cannot fail");
    String::from_utf8(output).expect("CSV rendering preserves UTF-8")
}

/// Streams one CSVWithNames result without retaining a formatted output copy.
pub fn write_csv(output: &mut impl io::Write, result: &QueryResult) -> io::Result<()> {
    for (index, column) in result.columns.iter().enumerate() {
        if index > 0 {
            output.write_all(b",")?;
        }
        write_csv_field(output, &column.name)?;
    }
    output.write_all(b"\n")?;

    for row in &result.rows {
        for (index, value) in row.iter().enumerate() {
            if index > 0 {
                output.write_all(b",")?;
            }
            match value {
                Value::String(value) => write_csv_field(output, value)?,
                Value::Null(_) => output.write_all(b"NULL")?,
                value => write_csv_field(output, &value.as_display_string())?,
            }
        }
        output.write_all(b"\n")?;
    }
    Ok(())
}

fn write_csv_field(output: &mut impl io::Write, value: &str) -> io::Result<()> {
    if !value.contains([',', '"', '\n', '\r']) {
        return output.write_all(value.as_bytes());
    }

    output.write_all(b"\"")?;
    for (index, segment) in value.split('"').enumerate() {
        if index > 0 {
            output.write_all(b"\"\"")?;
        }
        output.write_all(segment.as_bytes())?;
    }
    output.write_all(b"\"")
}

fn render_tsv(result: &QueryResult) -> String {
    let mut output = Vec::new();
    write_tsv(&mut output, result).expect("writing TSV to a Vec cannot fail");
    String::from_utf8(output).expect("TSV rendering preserves UTF-8")
}

/// Streams one ClickHouse-style `TabSeparatedWithNames` result.
///
/// Column names and `String` values escape backslashes, tabs, carriage
/// returns, and line feeds. SQL `NULL` is emitted as `\N`.
pub fn write_tsv(output: &mut impl io::Write, result: &QueryResult) -> io::Result<()> {
    for (index, column) in result.columns.iter().enumerate() {
        if index > 0 {
            output.write_all(b"\t")?;
        }
        write_tsv_escaped(output, &column.name)?;
    }
    output.write_all(b"\n")?;

    for row in &result.rows {
        for (index, value) in row.iter().enumerate() {
            if index > 0 {
                output.write_all(b"\t")?;
            }
            match value {
                Value::String(value) => write_tsv_escaped(output, value)?,
                Value::Null(_) => output.write_all(b"\\N")?,
                value => output.write_all(value.as_display_string().as_bytes())?,
            }
        }
        output.write_all(b"\n")?;
    }
    Ok(())
}

fn write_tsv_escaped(output: &mut impl io::Write, value: &str) -> io::Result<()> {
    let mut unescaped_start = 0;
    for (index, character) in value.char_indices() {
        let escaped = match character {
            '\\' => Some(r"\\"),
            '\t' => Some(r"\t"),
            '\r' => Some(r"\r"),
            '\n' => Some(r"\n"),
            _ => None,
        };
        if let Some(escaped) = escaped {
            output.write_all(value[unescaped_start..index].as_bytes())?;
            output.write_all(escaped.as_bytes())?;
            unescaped_start = index + character.len_utf8();
        }
    }
    output.write_all(value[unescaped_start..].as_bytes())
}

fn render_json(result: &QueryResult) -> String {
    let mut output = Vec::new();
    write_json(&mut output, result).expect("writing JSON to a Vec cannot fail");
    String::from_utf8(output).expect("JSON rendering preserves UTF-8")
}

/// Streams one JSON result with explicit column metadata and positional rows.
///
/// Positional rows preserve every value even when output column names repeat.
pub fn write_json(output: &mut impl io::Write, result: &QueryResult) -> io::Result<()> {
    output.write_all(b"{\"columns\":[")?;
    for (index, column) in result.columns.iter().enumerate() {
        if index > 0 {
            output.write_all(b",")?;
        }
        output.write_all(b"{\"name\":")?;
        write_json_string(output, &column.name)?;
        output.write_all(b",\"type\":")?;
        write_json_string(output, &column.data_type.to_string())?;
        output.write_all(b"}")?;
    }
    output.write_all(b"],\"rows\":[")?;
    for (row_index, row) in result.rows.iter().enumerate() {
        if row_index > 0 {
            output.write_all(b",")?;
        }
        output.write_all(b"[")?;
        for (column_index, value) in row.iter().enumerate() {
            if column_index > 0 {
                output.write_all(b",")?;
            }
            write_json_value(output, value)?;
        }
        output.write_all(b"]")?;
    }
    output.write_all(b"]}")
}

fn write_json_value(output: &mut impl io::Write, value: &Value) -> io::Result<()> {
    match value {
        Value::Null(_) => output.write_all(b"null"),
        Value::Int64(value) => write!(output, "{value}"),
        Value::Float64(value) => {
            output.write_all(Value::Float64(*value).as_display_string().as_bytes())
        }
        Value::Bool(value) => write!(output, "{value}"),
        Value::String(value) => write_json_string(output, value),
    }
}

fn write_json_string(output: &mut impl io::Write, value: &str) -> io::Result<()> {
    output.write_all(b"\"")?;
    let mut unescaped_start = 0;
    for (index, character) in value.char_indices() {
        let escaped = match character {
            '"' => Some(r#"\""#),
            '\\' => Some(r"\\"),
            '\u{08}' => Some(r"\b"),
            '\u{0c}' => Some(r"\f"),
            '\n' => Some(r"\n"),
            '\r' => Some(r"\r"),
            '\t' => Some(r"\t"),
            character if character.is_control() => {
                output.write_all(value[unescaped_start..index].as_bytes())?;
                write!(output, "\\u{:04x}", character as u32)?;
                unescaped_start = index + character.len_utf8();
                continue;
            }
            _ => None,
        };

        if let Some(escaped) = escaped {
            output.write_all(value[unescaped_start..index].as_bytes())?;
            output.write_all(escaped.as_bytes())?;
            unescaped_start = index + character.len_utf8();
        }
    }
    output.write_all(value[unescaped_start..].as_bytes())?;
    output.write_all(b"\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::engine::ResultColumn;
    use crate::batch::value::DataType;

    fn result() -> QueryResult {
        QueryResult {
            columns: vec![
                ResultColumn {
                    name: "id".to_owned(),
                    data_type: DataType::Int64,
                },
                ResultColumn {
                    name: "note".to_owned(),
                    data_type: DataType::String,
                },
            ],
            rows: vec![vec![
                Value::Int64(1),
                Value::String("quote: \", comma".to_owned()),
            ]],
        }
    }

    #[test]
    fn renders_csv_escaping() {
        assert_eq!(
            render(&result(), OutputFormat::Csv),
            "id,note\n1,\"quote: \"\", comma\"\n"
        );
    }

    #[test]
    fn streams_all_tsv_value_types_nulls_and_escaping() {
        let result = QueryResult {
            columns: vec![
                ResultColumn {
                    name: "null\\name".to_owned(),
                    data_type: DataType::String,
                },
                ResultColumn {
                    name: "integer\tname".to_owned(),
                    data_type: DataType::Int64,
                },
                ResultColumn {
                    name: "float\rname".to_owned(),
                    data_type: DataType::Float64,
                },
                ResultColumn {
                    name: "boolean\nname".to_owned(),
                    data_type: DataType::Bool,
                },
                ResultColumn {
                    name: "text".to_owned(),
                    data_type: DataType::String,
                },
            ],
            rows: vec![vec![
                Value::Null(DataType::String),
                Value::Int64(i64::MIN),
                Value::Float64(2.0),
                Value::Bool(false),
                Value::String("slash\\tab\tcarriage\rline\n雪".to_owned()),
            ]],
        };
        let expected = concat!(
            "null\\\\name\tinteger\\tname\tfloat\\rname\tboolean\\nname\ttext\n",
            "\\N\t-9223372036854775808\t2.0\tfalse\tslash\\\\tab\\tcarriage\\rline\\n雪\n",
        );
        let mut output = Vec::new();

        write_tsv(&mut output, &result).expect("Vec accepts streamed TSV");

        assert_eq!(output, expected.as_bytes());
        assert_eq!(render(&result, OutputFormat::Tsv), expected);
    }

    #[test]
    fn tsv_empty_result_still_writes_escaped_header() {
        let result = QueryResult {
            columns: vec![ResultColumn {
                name: "empty\tcolumn".to_owned(),
                data_type: DataType::String,
            }],
            rows: Vec::new(),
        };
        let mut output = Vec::new();

        write_tsv(&mut output, &result).expect("Vec accepts streamed TSV");

        assert_eq!(output, b"empty\\tcolumn\n");
    }

    #[test]
    fn renders_json_with_schema_and_native_scalar_types() {
        assert_eq!(
            render(&result(), OutputFormat::Json),
            r#"{"columns":[{"name":"id","type":"Int64"},{"name":"note","type":"String"}],"rows":[[1,"quote: \", comma"]]}"#
        );
    }

    #[test]
    fn streams_all_json_value_types_and_escaping() {
        let result = QueryResult {
            columns: vec![
                ResultColumn {
                    name: "null".to_owned(),
                    data_type: DataType::String,
                },
                ResultColumn {
                    name: "integer".to_owned(),
                    data_type: DataType::Int64,
                },
                ResultColumn {
                    name: "float".to_owned(),
                    data_type: DataType::Float64,
                },
                ResultColumn {
                    name: "boolean".to_owned(),
                    data_type: DataType::Bool,
                },
                ResultColumn {
                    name: "text\"\n".to_owned(),
                    data_type: DataType::String,
                },
            ],
            rows: vec![vec![
                Value::Null(DataType::String),
                Value::Int64(i64::MIN),
                Value::Float64(2.0),
                Value::Bool(false),
                Value::String("\"\\\u{08}\u{0c}\n\r\t\u{01}雪".to_owned()),
            ]],
        };
        let expected = r#"{"columns":[{"name":"null","type":"String"},{"name":"integer","type":"Int64"},{"name":"float","type":"Float64"},{"name":"boolean","type":"Bool"},{"name":"text\"\n","type":"String"}],"rows":[[null,-9223372036854775808,2.0,false,"\"\\\b\f\n\r\t\u0001雪"]]}"#;
        let mut output = Vec::new();

        write_json(&mut output, &result).expect("Vec accepts streamed JSON");

        assert_eq!(output, expected.as_bytes());
        assert_eq!(render(&result, OutputFormat::Json), expected);
    }

    #[test]
    fn renders_typed_nulls_in_csv_table_and_json_formats() {
        let result = QueryResult {
            columns: vec![ResultColumn {
                name: "total".to_owned(),
                data_type: DataType::Int64,
            }],
            rows: vec![vec![Value::Null(DataType::Int64)]],
        };

        assert_eq!(render(&result, OutputFormat::Csv), "total\nNULL\n");
        assert!(render(&result, OutputFormat::Table).contains("| NULL  |"));
        assert_eq!(
            render(&result, OutputFormat::Json),
            r#"{"columns":[{"name":"total","type":"Int64"}],"rows":[[null]]}"#
        );
    }

    #[test]
    fn positional_json_preserves_duplicate_output_names() {
        let result = QueryResult {
            columns: vec![
                ResultColumn {
                    name: "id".to_owned(),
                    data_type: DataType::Int64,
                },
                ResultColumn {
                    name: "id".to_owned(),
                    data_type: DataType::String,
                },
            ],
            rows: vec![vec![Value::Int64(1), Value::String("x".to_owned())]],
        };
        let expected = r#"{"columns":[{"name":"id","type":"Int64"},{"name":"id","type":"String"}],"rows":[[1,"x"]]}"#;
        let mut output = Vec::new();

        write_json(&mut output, &result).expect("Vec accepts streamed JSON");

        assert_eq!(output, expected.as_bytes());
        assert_eq!(render(&result, OutputFormat::Json), expected);
    }

    #[test]
    fn renders_readable_table() {
        let rendered = render(&result(), OutputFormat::Table);
        assert!(rendered.contains("| id | note"));
        assert!(rendered.contains("| 1  | quote: \", comma"));
    }

    #[test]
    fn streams_a_table_at_the_exact_formatted_byte_limit() {
        let mut result = result();
        result.rows[0][1] = Value::String("snow: 雪\nslash: \\\u{1b}".to_owned());
        let expected = render(&result, OutputFormat::Table);
        let mut exact_output = Vec::new();

        write_table(&mut exact_output, &result, expected.len())
            .expect("the exact formatted byte limit is accepted");

        assert_eq!(exact_output, expected.as_bytes());

        let mut rejected_output = Vec::new();
        let error = write_table(&mut rejected_output, &result, expected.len() - 1)
            .expect_err("one byte below the formatted size is rejected");
        assert!(matches!(
            error,
            TableWriteError::OutputLimitExceeded { bytes, max_bytes }
                if bytes == expected.len() && max_bytes == expected.len() - 1
        ));
        assert!(rejected_output.is_empty());
    }

    #[test]
    fn table_output_escapes_terminal_control_characters() {
        let result = QueryResult {
            columns: vec![ResultColumn {
                name: "text".to_owned(),
                data_type: DataType::String,
            }],
            rows: vec![vec![Value::String(
                "\u{1b}[31mred\u{07}\u{00}\u{7f}".to_owned(),
            )]],
        };

        let rendered = render(&result, OutputFormat::Table);
        assert!(rendered.contains(r"\u{001b}[31mred\u{0007}\u{0000}\u{007f}"));
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains('\u{07}'));
        assert!(!rendered.contains('\u{00}'));
        assert!(!rendered.contains('\u{7f}'));
    }
}
