use std::io::{self, BufRead, Cursor, Read};

use rusthouse::{Database, Error, QueryResult, StatementResult, Value};

fn last_query(results: Vec<StatementResult>) -> QueryResult {
    match results.into_iter().last().expect("statement result") {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

fn query(database: &mut Database, sql: &str) -> QueryResult {
    last_query(database.execute(sql).expect("query succeeds"))
}

#[test]
fn streams_multiple_typed_blocks_and_remains_queryable() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE samples (id Int64, score Float64, active Bool, label String)")
        .expect("create table");

    let row_count = 2_051;
    let mut csv = String::new();
    for id in 0..row_count {
        csv.push_str(&format!(
            "{id},{}.5,{},row {id}\n",
            id,
            if id % 2 == 0 { "true" } else { "false" }
        ));
    }

    assert_eq!(
        database
            .insert_csv("samples", Cursor::new(csv), false)
            .expect("stream import"),
        row_count
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT COUNT(*) AS rows, SUM(id) AS total FROM samples"
        )
        .rows,
        vec![vec![
            Value::Int64(row_count as i64),
            Value::Int64(2_102_275)
        ]]
    );
}

#[test]
fn csv_with_names_decodes_quoting_unicode_and_numeric_boundaries() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE boundaries (id Int64, score Float64, active Bool, label String)")
        .expect("create table");

    let csv = concat!(
        "ID,Score,Active,Label\r\n",
        "-9223372036854775808,-1.25,True,\"東京, \"\"north\"\"\"\r\n",
        "9223372036854775807,1e308,0,\"line one\r\nline two\"\r\n"
    );
    assert_eq!(
        database
            .insert_csv("boundaries", Cursor::new(csv), true)
            .expect("typed import"),
        2
    );

    assert_eq!(
        query(
            &mut database,
            "SELECT id, score, active, label FROM boundaries ORDER BY id"
        )
        .rows,
        vec![
            vec![
                Value::Int64(i64::MIN),
                Value::Float64(-1.25),
                Value::Bool(true),
                Value::String("東京, \"north\"".to_owned()),
            ],
            vec![
                Value::Int64(i64::MAX),
                Value::Float64(1e308),
                Value::Bool(false),
                Value::String("line one\r\nline two".to_owned()),
            ],
        ]
    );

    let error = database
        .insert_csv("boundaries", Cursor::new("0,1e999,true,overflow\n"), false)
        .expect_err("non-finite Float64");
    assert!(matches!(
        error,
        Error::Csv {
            column: Some(2),
            ..
        }
    ));
    assert_eq!(
        database.catalog().table("boundaries").unwrap().row_count(),
        2
    );
}

#[test]
fn late_type_error_rolls_back_every_completed_block() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE events (id Int64, label String); \
             INSERT INTO events VALUES (-1, 'existing')",
        )
        .expect("setup");

    let mut csv = String::new();
    for id in 0..1_100 {
        csv.push_str(&format!("{id},valid\n"));
    }
    csv.push_str("9223372036854775808,invalid\n");

    let error = database
        .insert_csv("events", Cursor::new(csv), false)
        .expect_err("late conversion error");
    assert!(matches!(
        error,
        Error::Csv {
            record: 1_101,
            column: Some(1),
            ..
        }
    ));
    assert_eq!(
        query(&mut database, "SELECT id, label FROM events").rows,
        vec![vec![Value::Int64(-1), Value::String("existing".to_owned())]]
    );
}

#[test]
fn malformed_rows_headers_and_quotes_leave_the_table_unchanged() {
    let cases = [
        (true, "wrong,label\n1,ok\n"),
        (false, "1,too,many\n"),
        (false, "1,\"unterminated"),
        (false, "1e999,label\n"),
    ];

    for (with_names, csv) in cases {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE items (id Int64, label String); \
                 INSERT INTO items VALUES (7, 'kept')",
            )
            .expect("setup");

        database
            .insert_csv("items", Cursor::new(csv), with_names)
            .expect_err("malformed CSV");
        assert_eq!(database.catalog().table("items").unwrap().row_count(), 1);
    }
}

#[test]
fn late_io_error_rolls_back_completed_blocks() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE numbers (n Int64)")
        .expect("create table");
    let csv = (0..1_100)
        .map(|number| format!("{number}\n"))
        .collect::<String>();
    let fail_at = csv.len() - 10;

    let error = database
        .insert_csv(
            "numbers",
            FailingReader::new(csv.into_bytes(), fail_at),
            false,
        )
        .expect_err("read failure");
    assert!(matches!(error, Error::Csv { .. }));
    assert_eq!(database.catalog().table("numbers").unwrap().row_count(), 0);
}

#[test]
fn format_statement_imports_then_queries_in_the_same_batch() {
    let mut database = Database::new();
    let results = database
        .execute_with_input(
            "CREATE TABLE names (id Int64, name String); \
             INSERT INTO names FORMAT CSVWithNames; \
             SELECT id, name FROM names ORDER BY id DESC",
            Cursor::new("id,name\n1,Ada\n2,Grace\n"),
        )
        .expect("reader-backed batch");

    assert!(matches!(
        &results[1],
        StatementResult::Command {
            tag: "INSERT",
            affected_rows: 2
        }
    ));
    assert_eq!(
        last_query(results).rows,
        vec![
            vec![Value::Int64(2), Value::String("Grace".to_owned())],
            vec![Value::Int64(1), Value::String("Ada".to_owned())]
        ]
    );
}

struct FailingReader {
    data: Vec<u8>,
    position: usize,
    fail_at: usize,
}

impl FailingReader {
    fn new(data: Vec<u8>, fail_at: usize) -> Self {
        Self {
            data,
            position: 0,
            fail_at,
        }
    }
}

impl Read for FailingReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.position >= self.fail_at {
            return Err(io::Error::other("synthetic read failure"));
        }
        let available = (self.fail_at - self.position).min(self.data.len() - self.position);
        let len = available.min(buffer.len()).min(64);
        buffer[..len].copy_from_slice(&self.data[self.position..self.position + len]);
        self.position += len;
        Ok(len)
    }
}

impl BufRead for FailingReader {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.position >= self.fail_at {
            return Err(io::Error::other("synthetic read failure"));
        }
        let end = self.fail_at.min(self.data.len());
        Ok(&self.data[self.position..end])
    }

    fn consume(&mut self, amount: usize) {
        self.position = (self.position + amount).min(self.data.len());
    }
}
