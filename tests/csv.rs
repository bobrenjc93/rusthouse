use rusthouse::{
    Catalog, DataType, Field, SelectResult, Table, Value, write_csv_with_names,
    write_select_csv_with_names,
};
use std::io;

#[test]
fn empty_table_writes_only_its_quoted_header() {
    let table = Table::new(vec![
        Field::new("id", DataType::Int64),
        Field::new("label", DataType::String),
    ])
    .unwrap();

    assert_eq!(render(&table), b"\"id\",\"label\"\n");
}

#[test]
fn quotes_headers_and_strings_with_rfc_4180_escaping() {
    let mut table = Table::new(vec![
        Field::new("plain", DataType::String),
        Field::new("comma,name", DataType::String),
        Field::new("say \"hello\"", DataType::String),
        Field::new("multi\nline", DataType::String),
    ])
    .unwrap();
    table
        .insert_batch(vec![
            vec![
                Value::from(""),
                Value::from("west,east"),
                Value::from("a \"quoted\" value"),
                Value::from("first line\nsecond line"),
            ],
            vec![
                Value::from("carriage\rreturn"),
                Value::from("both\r\nlines"),
                Value::from("caf\u{e9}"),
                Value::from("tail"),
            ],
        ])
        .unwrap();

    assert_eq!(
        render(&table),
        concat!(
            "\"plain\",\"comma,name\",\"say \"\"hello\"\"\",\"multi\nline\"\n",
            "\"\",\"west,east\",\"a \"\"quoted\"\" value\",\"first line\nsecond line\"\n",
            "\"carriage\rreturn\",\"both\r\nlines\",\"caf\u{e9}\",\"tail\"\n",
        )
        .as_bytes()
    );
}

#[test]
fn renders_all_physical_types_and_numeric_boundaries_canonically() {
    let mut table = Table::new(vec![
        Field::new("integer", DataType::Int64),
        Field::new("float", DataType::Float64),
        Field::new("boolean", DataType::Bool),
        Field::new("text", DataType::String),
    ])
    .unwrap();
    table
        .insert_batch(vec![
            vec![
                Value::Int64(i64::MIN),
                Value::Float64(f64::MAX),
                Value::Bool(false),
                Value::from("minimum integer"),
            ],
            vec![
                Value::Int64(i64::MAX),
                Value::Float64(f64::MIN),
                Value::Bool(true),
                Value::from("maximum integer"),
            ],
            vec![
                Value::Int64(0),
                Value::Float64(f64::from_bits(1)),
                Value::Bool(false),
                Value::from("smallest subnormal"),
            ],
            vec![
                Value::Int64(-1),
                Value::Float64(-0.0),
                Value::Bool(true),
                Value::from("signed zero"),
            ],
            vec![
                Value::Int64(1),
                Value::Float64(f64::INFINITY),
                Value::Bool(true),
                Value::from("positive infinity"),
            ],
            vec![
                Value::Int64(1),
                Value::Float64(f64::NEG_INFINITY),
                Value::Bool(false),
                Value::from("negative infinity"),
            ],
            vec![
                Value::Int64(1),
                Value::Float64(f64::NAN),
                Value::Bool(true),
                Value::from("not a number"),
            ],
        ])
        .unwrap();

    let csv = String::from_utf8(render(&table)).unwrap();
    let mut records = csv.lines();
    assert_eq!(
        records.next(),
        Some("\"integer\",\"float\",\"boolean\",\"text\"")
    );
    let minimum_integer = format!("{},{},false,\"minimum integer\"", i64::MIN, f64::MAX);
    assert_eq!(records.next(), Some(minimum_integer.as_str()));
    let maximum_integer = format!("{},{},true,\"maximum integer\"", i64::MAX, f64::MIN);
    assert_eq!(records.next(), Some(maximum_integer.as_str()));
    let smallest_subnormal = format!("0,{},false,\"smallest subnormal\"", f64::from_bits(1));
    assert_eq!(records.next(), Some(smallest_subnormal.as_str()));
    assert_eq!(records.next(), Some("-1,-0,true,\"signed zero\""));
    assert_eq!(records.next(), Some("1,inf,true,\"positive infinity\""));
    assert_eq!(records.next(), Some("1,-inf,false,\"negative infinity\""));
    assert_eq!(records.next(), Some("1,nan,true,\"not a number\""));
    assert_eq!(records.next(), None);
}

#[test]
fn returns_every_partial_writer_failure_without_buffering_the_table() {
    let mut table = Table::new(vec![
        Field::new("id", DataType::Int64),
        Field::new("message", DataType::String),
    ])
    .unwrap();
    table
        .insert_batch(vec![vec![Value::Int64(7), Value::from("some \"text\"")]])
        .unwrap();
    let expected = render(&table);

    for limit in 0..expected.len() {
        let mut writer = FailAfter::new(limit);
        let error = write_csv_with_names(&table, &mut writer).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other, "limit {limit}");
        assert_eq!(error.to_string(), "intentional writer failure");
        assert_eq!(writer.bytes, expected[..limit], "limit {limit}");
    }

    let mut complete = FailAfter::new(expected.len());
    write_csv_with_names(&table, &mut complete).unwrap();
    assert_eq!(complete.bytes, expected);
}

#[test]
fn select_results_stream_projected_columns_and_selected_rows() {
    let mut catalog = Catalog::new();
    catalog
        .execute_create("CREATE TABLE events (id Int64, active Bool, label String, score Float64)")
        .unwrap();
    catalog
        .execute_insert(
            "INSERT INTO events VALUES \
             (1, true, 'first', 1.5), \
             (2, false, 'second', 2.5), \
             (3, true, 'a \"quoted\" value', 3.5)",
        )
        .unwrap();

    let result = catalog
        .execute_select("SELECT label, id, label FROM events WHERE active = true")
        .unwrap();

    assert_eq!(
        render_select(&result),
        concat!(
            "\"label\",\"id\",\"label\"\n",
            "\"first\",1,\"first\"\n",
            "\"a \"\"quoted\"\" value\",3,\"a \"\"quoted\"\" value\"\n",
        )
        .as_bytes()
    );
}

#[test]
fn select_writer_returns_partial_failures_without_materializing_output() {
    let mut catalog = Catalog::new();
    catalog
        .execute_create("CREATE TABLE events (id Int64, label String)")
        .unwrap();
    catalog
        .execute_insert("INSERT INTO events VALUES (1, 'some \"text\"')")
        .unwrap();
    let result = catalog
        .execute_select("SELECT label, id FROM events")
        .unwrap();
    let expected = render_select(&result);

    for limit in 0..expected.len() {
        let mut writer = FailAfter::new(limit);
        let error = write_select_csv_with_names(&result, &mut writer).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other, "limit {limit}");
        assert_eq!(error.to_string(), "intentional writer failure");
        assert_eq!(writer.bytes, expected[..limit], "limit {limit}");
    }
}

fn render(table: &Table) -> Vec<u8> {
    let mut output = Vec::new();
    let writer: &mut dyn io::Write = &mut output;
    write_csv_with_names(table, writer).unwrap();
    output
}

fn render_select(result: &SelectResult<'_>) -> Vec<u8> {
    let mut output = Vec::new();
    let writer: &mut dyn io::Write = &mut output;
    write_select_csv_with_names(result, writer).unwrap();
    output
}

struct FailAfter {
    bytes: Vec<u8>,
    limit: usize,
}

impl FailAfter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }
}

impl io::Write for FailAfter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.bytes.len() == self.limit {
            return Err(io::Error::other("intentional writer failure"));
        }

        let accepted = bytes.len().min(self.limit - self.bytes.len());
        self.bytes.extend_from_slice(&bytes[..accepted]);
        Ok(accepted)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
