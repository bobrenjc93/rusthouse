use std::fmt::Write as _;
use std::io::{self, Write};

use crate::engine::{QueryResult, ResultColumn, RowSink};
use crate::value::{Value, ValueRef};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Table,
    Csv,
    Json,
}

impl OutputFormat {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "table" => Some(Self::Table),
            "csv" => Some(Self::Csv),
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
        OutputFormat::Json => render_json(result),
    }
}

/// Streams CSV result sets to an I/O writer.
///
/// Multiple queries are separated by one blank line, matching [`render`].
#[derive(Debug)]
pub struct CsvSink<W> {
    output: W,
    query_count: usize,
    expected_columns: Option<usize>,
}

impl<W> CsvSink<W> {
    #[must_use]
    pub fn new(output: W) -> Self {
        Self {
            output,
            query_count: 0,
            expected_columns: None,
        }
    }

    #[must_use]
    pub fn get_ref(&self) -> &W {
        &self.output
    }

    pub fn get_mut(&mut self) -> &mut W {
        &mut self.output
    }

    #[must_use]
    pub fn into_inner(self) -> W {
        self.output
    }
}

impl<W: Write> RowSink for CsvSink<W> {
    type Error = io::Error;

    fn begin_query(&mut self, columns: &[ResultColumn]) -> io::Result<()> {
        if self.expected_columns.is_some() {
            return Err(invalid_sink_state("a CSV query is already active"));
        }
        if self.query_count > 0 {
            self.output.write_all(b"\n")?;
        }
        for (index, column) in columns.iter().enumerate() {
            if index > 0 {
                self.output.write_all(b",")?;
            }
            write_csv_field_io(&mut self.output, &column.name)?;
        }
        self.output.write_all(b"\n")?;
        self.expected_columns = Some(columns.len());
        self.query_count += 1;
        Ok(())
    }

    fn row<'a, I>(&mut self, values: I) -> io::Result<()>
    where
        I: ExactSizeIterator<Item = ValueRef<'a>>,
    {
        let Some(expected) = self.expected_columns else {
            return Err(invalid_sink_state("no CSV query is active"));
        };
        if values.len() != expected {
            return Err(invalid_row_width(expected, values.len()));
        }
        for (index, value) in values.enumerate() {
            if index > 0 {
                self.output.write_all(b",")?;
            }
            match value {
                ValueRef::String(value) => write_csv_field_io(&mut self.output, value)?,
                value => {
                    write_csv_field_io(&mut self.output, &value.as_display_string())?;
                }
            }
        }
        self.output.write_all(b"\n")
    }

    fn end_query(&mut self) -> io::Result<()> {
        if self.expected_columns.take().is_none() {
            return Err(invalid_sink_state("no CSV query is active"));
        }
        Ok(())
    }
}

/// Streams all JSON result sets into one top-level `results` document.
#[derive(Debug)]
pub struct JsonSink<W> {
    output: W,
    started: bool,
    finished: bool,
    query_count: usize,
    row_count: usize,
    expected_columns: Option<usize>,
}

impl<W> JsonSink<W> {
    #[must_use]
    pub fn new(output: W) -> Self {
        Self {
            output,
            started: false,
            finished: false,
            query_count: 0,
            row_count: 0,
            expected_columns: None,
        }
    }

    #[must_use]
    pub fn get_ref(&self) -> &W {
        &self.output
    }

    pub fn get_mut(&mut self) -> &mut W {
        &mut self.output
    }

    #[must_use]
    pub fn into_inner(self) -> W {
        self.output
    }
}

impl<W: Write> JsonSink<W> {
    /// Close the top-level JSON document. An empty execution produces an empty
    /// `results` array. Calling this method more than once is harmless.
    pub fn finish(&mut self) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        if self.expected_columns.is_some() {
            return Err(invalid_sink_state("cannot finish an active JSON query"));
        }
        self.ensure_started()?;
        self.output.write_all(b"]}\n")?;
        self.finished = true;
        Ok(())
    }

    fn ensure_started(&mut self) -> io::Result<()> {
        if !self.started {
            self.output.write_all(b"{\"results\":[")?;
            self.started = true;
        }
        Ok(())
    }
}

impl<W: Write> RowSink for JsonSink<W> {
    type Error = io::Error;

    fn begin_query(&mut self, columns: &[ResultColumn]) -> io::Result<()> {
        if self.finished {
            return Err(invalid_sink_state("the JSON document is finished"));
        }
        if self.expected_columns.is_some() {
            return Err(invalid_sink_state("a JSON query is already active"));
        }
        self.ensure_started()?;
        if self.query_count > 0 {
            self.output.write_all(b",")?;
        }
        self.output.write_all(b"{\"columns\":[")?;
        for (index, column) in columns.iter().enumerate() {
            if index > 0 {
                self.output.write_all(b",")?;
            }
            self.output.write_all(b"{\"name\":")?;
            write_json_string_io(&mut self.output, &column.name)?;
            self.output.write_all(b",\"type\":")?;
            write_json_string_io(&mut self.output, &column.data_type.to_string())?;
            self.output.write_all(b"}")?;
        }
        self.output.write_all(b"],\"rows\":[")?;
        self.expected_columns = Some(columns.len());
        self.query_count += 1;
        self.row_count = 0;
        Ok(())
    }

    fn row<'a, I>(&mut self, values: I) -> io::Result<()>
    where
        I: ExactSizeIterator<Item = ValueRef<'a>>,
    {
        let Some(expected) = self.expected_columns else {
            return Err(invalid_sink_state("no JSON query is active"));
        };
        if values.len() != expected {
            return Err(invalid_row_width(expected, values.len()));
        }
        if self.row_count > 0 {
            self.output.write_all(b",")?;
        }
        self.output.write_all(b"[")?;
        for (index, value) in values.enumerate() {
            if index > 0 {
                self.output.write_all(b",")?;
            }
            write_json_value_io(&mut self.output, value)?;
        }
        self.output.write_all(b"]")?;
        self.row_count += 1;
        Ok(())
    }

    fn end_query(&mut self) -> io::Result<()> {
        if self.expected_columns.take().is_none() {
            return Err(invalid_sink_state("no JSON query is active"));
        }
        self.output.write_all(b"]}")
    }
}

fn invalid_sink_state(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn invalid_row_width(expected: usize, actual: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("row has {actual} values; expected {expected}"),
    )
}

fn write_csv_field_io(output: &mut impl Write, value: &str) -> io::Result<()> {
    if value.contains([',', '"', '\n', '\r']) {
        output.write_all(b"\"")?;
        for (index, part) in value.split('"').enumerate() {
            if index > 0 {
                output.write_all(b"\"\"")?;
            }
            output.write_all(part.as_bytes())?;
        }
        output.write_all(b"\"")
    } else {
        output.write_all(value.as_bytes())
    }
}

fn write_json_value_io(output: &mut impl Write, value: ValueRef<'_>) -> io::Result<()> {
    match value {
        ValueRef::Int64(value) => write!(output, "{value}"),
        ValueRef::Float64(value) => {
            output.write_all(ValueRef::Float64(value).as_display_string().as_bytes())
        }
        ValueRef::Bool(value) => write!(output, "{value}"),
        ValueRef::String(value) => write_json_string_io(output, value),
    }
}

fn write_json_string_io(output: &mut impl Write, value: &str) -> io::Result<()> {
    output.write_all(b"\"")?;
    let mut encoded = [0; 4];
    for character in value.chars() {
        match character {
            '"' => output.write_all(b"\\\"")?,
            '\\' => output.write_all(b"\\\\")?,
            '\u{08}' => output.write_all(b"\\b")?,
            '\u{0c}' => output.write_all(b"\\f")?,
            '\n' => output.write_all(b"\\n")?,
            '\r' => output.write_all(b"\\r")?,
            '\t' => output.write_all(b"\\t")?,
            value if value.is_control() => write!(output, "\\u{:04x}", value as u32)?,
            value => output.write_all(value.encode_utf8(&mut encoded).as_bytes())?,
        }
    }
    output.write_all(b"\"")
}

fn render_table(result: &QueryResult) -> String {
    let rendered_columns = result
        .columns
        .iter()
        .map(|column| escape_table_text(&column.name))
        .collect::<Vec<_>>();
    let rendered_rows = result
        .rows
        .iter()
        .map(|row| row.iter().map(table_value).collect::<Vec<_>>())
        .collect::<Vec<_>>();
    let widths = rendered_columns
        .iter()
        .enumerate()
        .map(|(column, name)| {
            rendered_rows
                .iter()
                .map(|row| row[column].chars().count())
                .max()
                .unwrap_or(0)
                .max(name.chars().count())
        })
        .collect::<Vec<_>>();

    let border = table_border(&widths);
    let mut output = String::new();
    output.push_str(&border);
    output.push('\n');
    table_row(
        &mut output,
        rendered_columns.iter().map(String::as_str),
        &widths,
    );
    output.push_str(&border);
    output.push('\n');
    for row in &rendered_rows {
        table_row(&mut output, row.iter().map(String::as_str), &widths);
    }
    output.push_str(&border);
    output
}

fn table_border(widths: &[usize]) -> String {
    let mut border = String::new();
    border.push('+');
    for width in widths {
        border.push_str(&"-".repeat(*width + 2));
        border.push('+');
    }
    border
}

fn table_row<'a>(output: &mut String, values: impl Iterator<Item = &'a str>, widths: &[usize]) {
    output.push('|');
    for (value, width) in values.zip(widths) {
        output.push(' ');
        output.push_str(value);
        output.push_str(&" ".repeat(width.saturating_sub(value.chars().count()) + 1));
        output.push('|');
    }
    output.push('\n');
}

fn table_value(value: &Value) -> String {
    escape_table_text(&value.as_display_string())
}

fn escape_table_text(value: &str) -> String {
    let mut output = String::new();
    for character in value.chars() {
        match character {
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            value if value.is_control() => {
                write!(output, "\\u{{{:04x}}}", value as u32)
                    .expect("writing to String cannot fail");
            }
            value => output.push(value),
        }
    }
    output
}

fn render_csv(result: &QueryResult) -> String {
    let mut output = String::new();
    write_csv_row(
        &mut output,
        result.columns.iter().map(|column| column.name.as_str()),
    );
    for row in &result.rows {
        let values = row.iter().map(Value::as_display_string).collect::<Vec<_>>();
        write_csv_row(&mut output, values.iter().map(String::as_str));
    }
    output
}

fn write_csv_row<'a>(output: &mut String, values: impl Iterator<Item = &'a str>) {
    for (index, value) in values.enumerate() {
        if index > 0 {
            output.push(',');
        }
        if value.contains([',', '"', '\n', '\r']) {
            output.push('"');
            output.push_str(&value.replace('"', "\"\""));
            output.push('"');
        } else {
            output.push_str(value);
        }
    }
    output.push('\n');
}

/// Render one result set with explicit column metadata and positional rows.
///
/// Positional rows preserve every value even when output column names repeat.
fn render_json(result: &QueryResult) -> String {
    let mut output = String::from("{\"columns\":[");
    for (index, column) in result.columns.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        output.push_str("{\"name\":");
        write_json_string(&mut output, &column.name);
        output.push_str(",\"type\":");
        write_json_string(&mut output, &column.data_type.to_string());
        output.push('}');
    }
    output.push_str("],\"rows\":[");
    for (row_index, row) in result.rows.iter().enumerate() {
        if row_index > 0 {
            output.push(',');
        }
        output.push('[');
        for (column_index, value) in row.iter().enumerate() {
            if column_index > 0 {
                output.push(',');
            }
            write_json_value(&mut output, value);
        }
        output.push(']');
    }
    output.push_str("]}");
    output
}

fn write_json_value(output: &mut String, value: &Value) {
    match value {
        Value::Int64(value) => write!(output, "{value}").expect("writing to String cannot fail"),
        Value::Float64(value) => output.push_str(&Value::Float64(*value).as_display_string()),
        Value::Bool(value) => write!(output, "{value}").expect("writing to String cannot fail"),
        Value::String(value) => write_json_string(output, value),
    }
}

fn write_json_string(output: &mut String, value: &str) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{08}' => output.push_str("\\b"),
            '\u{0c}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            value if value.is_control() => {
                write!(output, "\\u{:04x}", value as u32).expect("writing to String cannot fail");
            }
            value => output.push(value),
        }
    }
    output.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::ResultColumn;
    use crate::value::DataType;

    fn feed_result<S: RowSink>(sink: &mut S, result: &QueryResult)
    where
        S::Error: std::fmt::Debug,
    {
        sink.begin_query(&result.columns).expect("begin query");
        for row in &result.rows {
            sink.row(row.iter().map(Value::as_ref)).expect("write row");
        }
        sink.end_query().expect("end query");
    }

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
    fn renders_json_with_schema_and_native_scalar_types() {
        assert_eq!(
            render(&result(), OutputFormat::Json),
            r#"{"columns":[{"name":"id","type":"Int64"},{"name":"note","type":"String"}],"rows":[[1,"quote: \", comma"]]}"#
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

        assert_eq!(
            render(&result, OutputFormat::Json),
            r#"{"columns":[{"name":"id","type":"Int64"},{"name":"id","type":"String"}],"rows":[[1,"x"]]}"#
        );
    }

    #[test]
    fn renders_readable_table() {
        let rendered = render(&result(), OutputFormat::Table);
        assert!(rendered.contains("| id | note"));
        assert!(rendered.contains("| 1  | quote: \", comma"));
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

    #[test]
    fn streaming_csv_is_byte_equivalent_to_collected_rendering() {
        let result = QueryResult {
            columns: vec![
                ResultColumn {
                    name: "integer".to_owned(),
                    data_type: DataType::Int64,
                },
                ResultColumn {
                    name: "float".to_owned(),
                    data_type: DataType::Float64,
                },
                ResultColumn {
                    name: "enabled".to_owned(),
                    data_type: DataType::Bool,
                },
                ResultColumn {
                    name: "note".to_owned(),
                    data_type: DataType::String,
                },
            ],
            rows: vec![vec![
                Value::Int64(-7),
                Value::Float64(2.0),
                Value::Bool(true),
                Value::String("quote: \"; comma, newline\nend".to_owned()),
            ]],
        };
        let expected = render(&result, OutputFormat::Csv);
        let mut sink = CsvSink::new(Vec::new());
        feed_result(&mut sink, &result);

        assert_eq!(sink.into_inner(), expected.as_bytes());
    }

    #[test]
    fn streaming_json_is_byte_equivalent_to_collected_rendering() {
        let result = QueryResult {
            columns: vec![
                ResultColumn {
                    name: "value".to_owned(),
                    data_type: DataType::String,
                },
                ResultColumn {
                    name: "value".to_owned(),
                    data_type: DataType::Float64,
                },
            ],
            rows: vec![vec![
                Value::String("snowman: \u{2603}; controls: \u{0008}\u{000c}\n".to_owned()),
                Value::Float64(3.0),
            ]],
        };
        let expected = format!(
            "{{\"results\":[{}]}}\n",
            render(&result, OutputFormat::Json)
        );
        let mut sink = JsonSink::new(Vec::new());
        feed_result(&mut sink, &result);
        sink.finish().expect("finish document");

        assert_eq!(sink.into_inner(), expected.as_bytes());
    }

    #[test]
    fn streaming_formats_preserve_multiple_query_framing() {
        let first = result();
        let second = QueryResult {
            columns: vec![ResultColumn {
                name: "empty".to_owned(),
                data_type: DataType::Bool,
            }],
            rows: Vec::new(),
        };

        let mut csv = CsvSink::new(Vec::new());
        feed_result(&mut csv, &first);
        feed_result(&mut csv, &second);
        assert_eq!(
            csv.into_inner(),
            format!(
                "{}\n{}",
                render(&first, OutputFormat::Csv),
                render(&second, OutputFormat::Csv)
            )
            .as_bytes()
        );

        let mut json = JsonSink::new(Vec::new());
        feed_result(&mut json, &first);
        feed_result(&mut json, &second);
        json.finish().expect("finish document");
        assert_eq!(
            json.into_inner(),
            format!(
                "{{\"results\":[{},{}]}}\n",
                render(&first, OutputFormat::Json),
                render(&second, OutputFormat::Json)
            )
            .as_bytes()
        );
    }

    #[test]
    fn empty_json_execution_is_a_complete_document() {
        let mut sink = JsonSink::new(Vec::new());
        sink.finish().expect("finish empty document");
        sink.finish().expect("finish is idempotent");
        assert_eq!(sink.into_inner(), b"{\"results\":[]}\n");
    }
}
