use std::convert::Infallible;
use std::io::{self, Write};

use rusthouse::format::{CsvWriter, JsonWriter};
use rusthouse::{
    Database, ROW_BATCH_SIZE, ResultColumn, RowBatch, RowBatchSink, StreamError, Value,
};

#[derive(Debug, Default)]
struct QueryStats {
    columns: Vec<ResultColumn>,
    batch_sizes: Vec<usize>,
    rows: usize,
    first_value: Option<Value>,
    last_value: Option<Value>,
}

#[derive(Debug, Default)]
struct MeasuringSink {
    queries: Vec<QueryStats>,
}

impl RowBatchSink for MeasuringSink {
    type Error = Infallible;

    fn start_query(&mut self, columns: &[ResultColumn]) -> std::result::Result<(), Self::Error> {
        self.queries.push(QueryStats {
            columns: columns.to_vec(),
            ..QueryStats::default()
        });
        Ok(())
    }

    fn write_batch(&mut self, batch: RowBatch<'_>) -> std::result::Result<(), Self::Error> {
        assert!(!batch.is_empty());
        assert!(batch.len() <= ROW_BATCH_SIZE);

        let query = self
            .queries
            .last_mut()
            .expect("query metadata precedes rows");
        query.batch_sizes.push(batch.len());
        query.rows += batch.len();
        if query.first_value.is_none() {
            query.first_value = batch.rows()[0].first().cloned();
        }
        query.last_value = batch.rows().last().and_then(|row| row.first()).cloned();
        Ok(())
    }
}

fn populated_database(row_count: usize) -> Database {
    let values = (0..row_count)
        .map(|value| format!("({value})"))
        .collect::<Vec<_>>()
        .join(",");
    let mut database = Database::new();
    database
        .execute(&format!(
            "CREATE TABLE numbers (id Int64); INSERT INTO numbers VALUES {values};"
        ))
        .expect("setup succeeds");
    database
}

#[test]
fn every_large_result_uses_fixed_size_batches() {
    let row_count = ROW_BATCH_SIZE * 3 + 17;
    let mut database = populated_database(row_count);
    let mut sink = MeasuringSink::default();

    database
        .execute_stream(
            "SELECT id FROM numbers WHERE id >= 0;
             SELECT id FROM numbers ORDER BY id DESC;
             SELECT id, COUNT(*) AS n FROM numbers GROUP BY id;",
            &mut sink,
        )
        .expect("streaming queries succeed");

    assert_eq!(sink.queries.len(), 3);
    for query in &sink.queries {
        assert!(!query.columns.is_empty());
        assert_eq!(query.rows, row_count);
        assert_eq!(
            query.batch_sizes,
            vec![ROW_BATCH_SIZE, ROW_BATCH_SIZE, ROW_BATCH_SIZE, 17]
        );
    }
    assert_eq!(sink.queries[0].first_value, Some(Value::Int64(0)));
    assert_eq!(
        sink.queries[0].last_value,
        Some(Value::Int64((row_count - 1) as i64))
    );
    assert_eq!(
        sink.queries[1].first_value,
        Some(Value::Int64((row_count - 1) as i64))
    );
    assert_eq!(sink.queries[1].last_value, Some(Value::Int64(0)));
}

#[test]
fn csv_and_json_writers_preserve_streamed_result_shape() {
    let mut database = populated_database(2);

    let mut csv = CsvWriter::new(Vec::new());
    database
        .execute_stream(
            "SELECT id FROM numbers ORDER BY id; SELECT id FROM numbers WHERE id = 1;",
            &mut csv,
        )
        .expect("CSV streaming succeeds");
    assert_eq!(
        String::from_utf8(csv.into_inner()).expect("UTF-8 CSV"),
        "id\n0\n1\n\nid\n1\n"
    );

    let mut json = JsonWriter::new(Vec::new());
    database
        .execute_stream(
            "SELECT id FROM numbers ORDER BY id; SELECT id FROM numbers WHERE id = 1;",
            &mut json,
        )
        .expect("JSON streaming succeeds");
    assert_eq!(
        String::from_utf8(json.into_inner()).expect("UTF-8 JSON"),
        "{\"results\":[{\"columns\":[{\"name\":\"id\",\"type\":\"Int64\"}],\"rows\":[[0],[1]]},{\"columns\":[{\"name\":\"id\",\"type\":\"Int64\"}],\"rows\":[[1]]}]}\n"
    );
}

#[derive(Debug)]
struct BrokenPipeWriter {
    remaining: usize,
}

impl Write for BrokenPipeWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "reader closed"));
        }
        let written = bytes.len().min(self.remaining);
        self.remaining -= written;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn streaming_writers_return_broken_pipe_errors() {
    let mut database = populated_database(ROW_BATCH_SIZE + 1);
    let mut json = JsonWriter::new(BrokenPipeWriter { remaining: 64 });

    let error = database
        .execute_stream("SELECT id FROM numbers;", &mut json)
        .expect_err("closed output stops streaming");

    assert!(matches!(
        error,
        StreamError::Sink(error) if error.kind() == io::ErrorKind::BrokenPipe
    ));
}

#[test]
fn database_errors_abort_json_as_a_valid_document() {
    let mut database = Database::new();
    let mut json = JsonWriter::new(Vec::new());

    let error = database
        .execute_stream("SELECT * FROM missing;", &mut json)
        .expect_err("missing table fails");

    assert!(matches!(error, StreamError::Database(_)));
    assert_eq!(
        String::from_utf8(json.into_inner()).expect("UTF-8 JSON"),
        "{\"results\":[]}\n"
    );

    database
        .execute("CREATE TABLE numbers (id Int64); INSERT INTO numbers VALUES (7);")
        .expect("setup succeeds");
    let mut json = JsonWriter::new(Vec::new());
    let error = database
        .execute_stream(
            "SELECT id FROM numbers; SELECT * FROM still_missing;",
            &mut json,
        )
        .expect_err("later missing table fails");

    assert!(matches!(error, StreamError::Database(_)));
    assert_eq!(
        String::from_utf8(json.into_inner()).expect("UTF-8 JSON"),
        "{\"results\":[{\"columns\":[{\"name\":\"id\",\"type\":\"Int64\"}],\"rows\":[[7]]}]}\n"
    );
}

#[derive(Debug, Default)]
struct FailingFinishSink {
    callbacks: Vec<&'static str>,
}

impl RowBatchSink for FailingFinishSink {
    type Error = &'static str;

    fn start(&mut self) -> std::result::Result<(), Self::Error> {
        self.callbacks.push("start");
        Ok(())
    }

    fn finish(&mut self) -> std::result::Result<(), Self::Error> {
        self.callbacks.push("finish");
        Err("finish failed")
    }

    fn abort(&mut self) -> std::result::Result<(), Self::Error> {
        self.callbacks.push("abort");
        Ok(())
    }
}

#[test]
fn finish_failures_run_abort_and_preserve_the_finish_error() {
    let mut database = Database::new();
    let mut sink = FailingFinishSink::default();

    let error = database
        .execute_stream("CREATE TABLE lifecycle (id Int64);", &mut sink)
        .expect_err("finish fails");

    assert!(matches!(error, StreamError::Sink("finish failed")));
    assert_eq!(sink.callbacks, ["start", "finish", "abort"]);
}
