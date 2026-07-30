use rusthouse::format::{OutputFormat, StreamingWriter};
use rusthouse::{Database, ResultColumn, RowSink, Value, ValueRef};

#[derive(Debug, Default)]
struct RecordingSink {
    events: Vec<String>,
    rows: Vec<Vec<Value>>,
}

impl RowSink for RecordingSink {
    fn command(&mut self, tag: &'static str, affected_rows: usize) -> rusthouse::Result<()> {
        self.events.push(format!("{tag}:{affected_rows}"));
        Ok(())
    }

    fn begin_query(&mut self, columns: &[ResultColumn]) -> rusthouse::Result<()> {
        self.events.push(format!(
            "begin:{}",
            columns
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>()
                .join(",")
        ));
        Ok(())
    }

    fn write_row(&mut self, row: &[ValueRef<'_>]) -> rusthouse::Result<()> {
        self.events.push("row".to_owned());
        self.rows
            .push(row.iter().copied().map(ValueRef::into_owned).collect());
        Ok(())
    }

    fn end_query(&mut self) -> rusthouse::Result<()> {
        self.events.push("end".to_owned());
        Ok(())
    }
}

#[test]
fn execute_with_sink_delivers_statement_order_and_borrowed_rows() {
    let mut database = Database::new();
    let mut sink = RecordingSink::default();

    database
        .execute_with_sink(
            "CREATE TABLE numbers (n Int64, label String);
             INSERT INTO numbers VALUES
                (1, 'one'), (2, 'two'), (3, 'three'), (4, 'four');
             SELECT label, n FROM numbers WHERE n >= 2 LIMIT 2;",
            &mut sink,
        )
        .expect("streaming execution succeeds");

    assert_eq!(
        sink.events,
        [
            "CREATE TABLE:0",
            "INSERT:4",
            "begin:label,n",
            "row",
            "row",
            "end",
        ]
    );
    assert_eq!(
        sink.rows,
        [
            vec![Value::String("two".to_owned()), Value::Int64(2)],
            vec![Value::String("three".to_owned()), Value::Int64(3)],
        ]
    );
}

#[test]
fn json_writer_streams_multiple_queries_as_one_document() {
    let mut database = Database::new();
    let mut writer = StreamingWriter::new(Vec::new(), OutputFormat::Json).expect("JSON writer");

    database
        .execute_with_sink(
            "CREATE TABLE valueset (n Int64);
             INSERT INTO valueset VALUES (2), (1);
             SELECT n FROM valueset ORDER BY n;
             SELECT COUNT(*) AS count FROM valueset;",
            &mut writer,
        )
        .expect("streaming execution succeeds");

    let output = writer.finish().expect("finish JSON");
    assert_eq!(
        String::from_utf8(output).expect("UTF-8 output"),
        "{\"results\":[{\"columns\":[{\"name\":\"n\",\"type\":\"Int64\"}],\"rows\":[[1],[2]]},{\"columns\":[{\"name\":\"count\",\"type\":\"Int64\"}],\"rows\":[[2]]}]}\n"
    );
}

#[test]
fn json_writer_emits_an_empty_results_document_for_command_only_batches() {
    let mut database = Database::new();
    let mut writer = StreamingWriter::new(Vec::new(), OutputFormat::Json).expect("JSON writer");
    database
        .execute_with_sink("CREATE TABLE empty (n Int64);", &mut writer)
        .expect("command succeeds");

    assert_eq!(writer.finish().expect("finish JSON"), b"{\"results\":[]}\n");
}
