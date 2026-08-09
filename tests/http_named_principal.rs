use std::io::{self, Cursor, Read};
use std::sync::{Arc, RwLock, mpsc};
use std::thread;
use std::time::Duration;

use rusthouse::batch::csv::CsvIngestLimits;
use rusthouse::batch::engine::Database;
use rusthouse::batch::tsv::TsvIngestLimits;
use rusthouse::{
    ClickHousePrincipal, ClickHousePrincipalRole, HttpQueryError, HttpQueryLimits,
    MAX_HTTP_NAMED_PRINCIPALS, SharedDatabase, handle_http_query_read_only_with_clickhouse_key,
    handle_http_query_read_only_with_clickhouse_principal,
    handle_http_query_read_only_with_clickhouse_principal_and_limits,
    handle_http_query_with_clickhouse_principal,
    handle_http_query_with_clickhouse_principal_and_limits,
    handle_http_query_with_clickhouse_principal_set,
};

const USER: &str = "reporting";
const KEY: &str = "read key:42";

fn request(target: &str, body: &[u8], headers: &str) -> (Vec<u8>, u64) {
    let mut request = format!(
        "POST {target} HTTP/1.1\r\nHost: localhost\r\n{headers}Content-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    let body_offset = request.len() as u64;
    request.extend_from_slice(body);
    (request, body_offset)
}

fn principal_headers() -> &'static str {
    "X-ClickHouse-User: reporting\r\nX-ClickHouse-Key: read key:42\r\n"
}

fn exchange(database: &SharedDatabase, request: &[u8]) -> Vec<u8> {
    let mut response = Vec::new();
    handle_http_query_read_only_with_clickhouse_principal(
        database,
        USER,
        KEY,
        Cursor::new(request),
        &mut response,
    )
    .expect("named-principal exchange succeeds");
    response
}

fn write_exchange(database: &SharedDatabase, request: &[u8]) -> Vec<u8> {
    let mut response = Vec::new();
    handle_http_query_with_clickhouse_principal(
        database,
        USER,
        KEY,
        Cursor::new(request),
        &mut response,
    )
    .expect("named-principal read-write exchange succeeds");
    response
}

fn assert_status(response: &[u8], expected: &str) {
    let response = std::str::from_utf8(response).expect("response is UTF-8");
    assert_eq!(response.lines().next(), Some(expected));
}

fn body(response: &[u8]) -> &[u8] {
    let body_offset = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("response has a header terminator")
        + 4;
    &response[body_offset..]
}

fn assert_private(response: &[u8]) {
    assert!(
        response
            .windows(b"Cache-Control: private, no-store\r\n".len())
            .any(|window| window == b"Cache-Control: private, no-store\r\n")
    );
}

#[test]
fn named_read_write_principal_wires_sql_csv_and_tsv_insertion_then_reads() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE events (id Int64, label String);")
        .unwrap();

    let (sql, _) = request(
        "/insert",
        b"INSERT INTO events VALUES (1, 'sql');",
        principal_headers(),
    );
    let sql_response = write_exchange(&database, &sql);
    assert_status(&sql_response, "HTTP/1.1 200 OK");
    assert_eq!(body(&sql_response), b"");
    assert_private(&sql_response);

    let (csv, _) = request("/insert/events", b"label,id\ncsv,2\n", principal_headers());
    let csv_response = write_exchange(&database, &csv);
    assert_status(&csv_response, "HTTP/1.1 200 OK");
    assert_eq!(body(&csv_response), b"");
    assert_private(&csv_response);

    let (tsv, _) = request(
        "/?query=INSERT+INTO+events+FORMAT+TabSeparated",
        b"3\ttsv\n",
        principal_headers(),
    );
    let tsv_response = write_exchange(&database, &tsv);
    assert_status(&tsv_response, "HTTP/1.1 200 OK");
    assert_eq!(body(&tsv_response), b"");
    assert_private(&tsv_response);

    let read = write_exchange(
        &database,
        b"GET /?query=SELECT+id%2C+label+FROM+events+ORDER+BY+id%3B HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-User: reporting\r\nX-ClickHouse-Key: read key:42\r\n\r\n",
    );
    assert_status(&read, "HTTP/1.1 200 OK");
    assert_eq!(
        body(&read),
        br#"{"columns":[{"name":"id","type":"Int64"},{"name":"label","type":"String"}],"rows":[[1,"sql"],[2,"csv"],[3,"tsv"]]}"#
    );
    assert_private(&read);
}

#[test]
fn named_principal_wires_reads_and_operational_routes() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE events (id Int64); INSERT INTO events VALUES (7);")
        .unwrap();

    let query = exchange(
        &database,
        b"GET /?query=SELECT+id+FROM+events%3B HTTP/1.1\r\nHost: localhost\r\nx-clickhouse-user: reporting\r\nX-CLICKHOUSE-KEY: read key:42\r\n\r\n",
    );
    assert_status(&query, "HTTP/1.1 200 OK");
    assert_eq!(
        body(&query),
        br#"{"columns":[{"name":"id","type":"Int64"}],"rows":[[7]]}"#
    );
    assert_private(&query);

    for target in ["/ping", "/ready"] {
        let request = format!(
            "GET {target} HTTP/1.1\r\nHost: localhost\r\n{}\r\n",
            principal_headers()
        );
        let response = exchange(&database, request.as_bytes());
        assert_status(&response, "HTTP/1.1 200 OK");
        assert_eq!(body(&response), b"Ok.\n");
        assert_private(&response);
    }

    let metrics = exchange(
        &database,
        b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-User: reporting\r\nX-ClickHouse-Key: read key:42\r\n\r\n",
    );
    assert_status(&metrics, "HTTP/1.1 200 OK");
    assert!(body(&metrics).starts_with(b"# HELP rusthouse_tables "));
    assert!(
        body(&metrics)
            .windows(b"rusthouse_tables 1\n".len())
            .any(|window| window == b"rusthouse_tables 1\n")
    );
    assert_private(&metrics);
}

#[test]
fn named_principal_credential_failures_are_identical_and_leave_bodies_unread() {
    let database = SharedDatabase::default();
    let rejected_headers = [
        ("missing both", ""),
        ("missing user", "X-ClickHouse-Key: read key:42\r\n"),
        ("missing key", "X-ClickHouse-User: reporting\r\n"),
        (
            "empty user",
            "X-ClickHouse-User:\r\nX-ClickHouse-Key: read key:42\r\n",
        ),
        (
            "empty key",
            "X-ClickHouse-User: reporting\r\nX-ClickHouse-Key:\r\n",
        ),
        (
            "wrong user",
            "X-ClickHouse-User: Reporting\r\nX-ClickHouse-Key: read key:42\r\n",
        ),
        (
            "wrong key",
            "X-ClickHouse-User: reporting\r\nX-ClickHouse-Key: wrong\r\n",
        ),
        (
            "duplicate user",
            "X-ClickHouse-User: reporting\r\nx-clickhouse-user: reporting\r\nX-ClickHouse-Key: read key:42\r\n",
        ),
        (
            "duplicate key",
            "X-ClickHouse-User: reporting\r\nX-ClickHouse-Key: read key:42\r\nx-clickhouse-key: read key:42\r\n",
        ),
    ];
    let mut expected_response = None;

    for (name, headers) in rejected_headers {
        let (request, body_offset) = request("/insert/events", b"id\n99\n", headers);
        let mut input = Cursor::new(request.clone());
        let mut response = Vec::new();
        handle_http_query_read_only_with_clickhouse_principal(
            &database,
            USER,
            KEY,
            &mut input,
            &mut response,
        )
        .unwrap_or_else(|error| panic!("{name} produces an HTTP response: {error}"));

        assert_eq!(
            input.position(),
            body_offset,
            "{name} leaves the body unread"
        );
        assert_status(&response, "HTTP/1.1 401 Unauthorized");
        assert_eq!(
            body(&response),
            br#"{"error":"X-ClickHouse-User and X-ClickHouse-Key authentication required"}"#
        );
        assert_private(&response);

        let mut write_input = Cursor::new(request);
        let mut write_response = Vec::new();
        handle_http_query_with_clickhouse_principal(
            &database,
            USER,
            KEY,
            &mut write_input,
            &mut write_response,
        )
        .unwrap_or_else(|error| panic!("{name} read-write rejection responds: {error}"));
        assert_eq!(
            write_input.position(),
            body_offset,
            "{name} read-write rejection leaves the body unread"
        );
        assert_eq!(
            write_response, response,
            "{name} rejection is identical for read-only and read-write principals"
        );

        if let Some(expected) = &expected_response {
            assert_eq!(&response, expected, "{name} must be indistinguishable");
        } else {
            expected_response = Some(response);
        }
    }
}

#[test]
fn named_read_write_principal_enforces_explicit_http_csv_and_tsv_limits() {
    let database = SharedDatabase::default();
    database.execute("CREATE TABLE events (id Int64);").unwrap();

    let sql = b"INSERT INTO events VALUES (1);";
    let (sql_request, body_offset) = request("/insert", sql, principal_headers());
    let mut sql_input = Cursor::new(sql_request);
    let mut sql_response = Vec::new();
    handle_http_query_with_clickhouse_principal_and_limits(
        &database,
        USER,
        KEY,
        &mut sql_input,
        &mut sql_response,
        HttpQueryLimits {
            max_sql_bytes: sql.len() - 1,
            ..HttpQueryLimits::default()
        },
    )
    .unwrap();
    assert_eq!(sql_input.position(), body_offset);
    assert_status(&sql_response, "HTTP/1.1 413 Payload Too Large");
    assert_private(&sql_response);

    let csv = b"id\n1\n";
    let (csv_request, _) = request("/insert/events", csv, principal_headers());
    let mut csv_response = Vec::new();
    handle_http_query_with_clickhouse_principal_and_limits(
        &database,
        USER,
        KEY,
        Cursor::new(csv_request),
        &mut csv_response,
        HttpQueryLimits {
            csv_ingest_limits: CsvIngestLimits::new(csv.len() - 1, 10, 10),
            ..HttpQueryLimits::default()
        },
    )
    .unwrap();
    assert_status(&csv_response, "HTTP/1.1 400 Bad Request");
    assert!(body(&csv_response).starts_with(br#"{"error":"database CSV ingestion failed:"#));
    assert_private(&csv_response);

    let tsv = b"2\n";
    let (tsv_request, _) = request(
        "/insert/events",
        tsv,
        concat!(
            "X-ClickHouse-User: reporting\r\n",
            "X-ClickHouse-Key: read key:42\r\n",
            "X-ClickHouse-Format: TabSeparated\r\n",
        ),
    );
    let mut tsv_response = Vec::new();
    handle_http_query_with_clickhouse_principal_and_limits(
        &database,
        USER,
        KEY,
        Cursor::new(tsv_request),
        &mut tsv_response,
        HttpQueryLimits {
            tsv_ingest_limits: TsvIngestLimits::new(tsv.len() - 1, 10, 10),
            ..HttpQueryLimits::default()
        },
    )
    .unwrap();
    assert_status(&tsv_response, "HTTP/1.1 400 Bad Request");
    assert!(body(&tsv_response).starts_with(br#"{"error":"database TSV ingestion failed:"#));
    assert_private(&tsv_response);

    assert!(
        database
            .query("SELECT id FROM events;")
            .unwrap()
            .rows
            .is_empty()
    );
}

#[test]
fn named_read_write_principal_returns_503_without_waiting_on_a_reader() {
    let mut initial = Database::new();
    initial.execute("CREATE TABLE events (id Int64);").unwrap();
    let inner = Arc::new(RwLock::new(initial));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let mut reader = Some(inner.read().unwrap());
    let (request, _) = request(
        "/insert",
        b"INSERT INTO events VALUES (1);",
        principal_headers(),
    );
    let (sender, receiver) = mpsc::channel();
    let worker_database = database.clone();
    let worker = thread::spawn(move || {
        sender
            .send(write_exchange(&worker_database, &request))
            .unwrap();
    });

    let response = match receiver.recv_timeout(Duration::from_secs(2)) {
        Ok(response) => response,
        Err(error) => {
            drop(reader.take());
            worker.join().unwrap();
            panic!("named-principal insertion blocked behind a reader: {error}");
        }
    };
    assert_status(&response, "HTTP/1.1 503 Service Unavailable");
    assert_eq!(body(&response), br#"{"error":"database is unavailable"}"#);
    assert_private(&response);
    assert_eq!(
        reader
            .as_ref()
            .unwrap()
            .catalog()
            .table("events")
            .unwrap()
            .row_count(),
        0
    );
    drop(reader.take());
    worker.join().unwrap();
}

#[test]
fn named_principal_rejects_insertion_without_mutating_state() {
    let database = SharedDatabase::default();
    database.execute("CREATE TABLE events (id Int64);").unwrap();

    for target in ["/insert", "/insert/events"] {
        let (request, body_offset) = request(
            target,
            b"INSERT INTO events VALUES (99);",
            principal_headers(),
        );
        let mut input = Cursor::new(request);
        let mut response = Vec::new();
        handle_http_query_read_only_with_clickhouse_principal(
            &database,
            USER,
            KEY,
            &mut input,
            &mut response,
        )
        .unwrap();
        assert_eq!(input.position(), body_offset);
        assert_status(&response, "HTTP/1.1 404 Not Found");
        assert_private(&response);
    }

    let (insert, _) = request(
        "/query",
        b"INSERT INTO events VALUES (99);",
        principal_headers(),
    );
    let response = exchange(&database, &insert);
    assert_status(&response, "HTTP/1.1 400 Bad Request");
    assert!(body(&response).starts_with(br#"{"error":"read-only query"#));

    assert!(
        database
            .query("SELECT id FROM events;")
            .unwrap()
            .rows
            .is_empty()
    );
}

#[test]
fn named_principal_explicit_limits_are_enforced() {
    let database = SharedDatabase::default();
    let sql = b"SELECT 123 AS value;";
    let (request, body_offset) = request("/query", sql, principal_headers());
    let mut input = Cursor::new(request);
    let mut response = Vec::new();

    handle_http_query_read_only_with_clickhouse_principal_and_limits(
        &database,
        USER,
        KEY,
        &mut input,
        &mut response,
        HttpQueryLimits {
            max_sql_bytes: sql.len() - 1,
            ..HttpQueryLimits::default()
        },
    )
    .unwrap();

    assert_eq!(input.position(), body_offset);
    assert_status(&response, "HTTP/1.1 413 Payload Too Large");
    assert_private(&response);
}

#[test]
fn named_principal_configuration_is_validated_before_input() {
    let database = SharedDatabase::default();
    let cases = [
        ("", KEY, "configured ClickHouse user must not be empty"),
        (
            " leading",
            KEY,
            "configured ClickHouse user is not a valid HTTP header value",
        ),
        (USER, "", "configured ClickHouse key must not be empty"),
        (
            USER,
            "line\nbreak",
            "configured ClickHouse key is not a valid HTTP header value",
        ),
    ];

    for (user, key, message) in cases {
        let mut response = Vec::new();
        handle_http_query_read_only_with_clickhouse_principal(
            &database,
            user,
            key,
            FailingReader,
            &mut response,
        )
        .unwrap_or_else(|error| panic!("invalid configuration responds before input: {error}"));
        assert_status(&response, "HTTP/1.1 500 Internal Server Error");
        assert_eq!(
            body(&response),
            format!(r#"{{"error":"{message}"}}"#).as_bytes()
        );
        assert_private(&response);
    }
}

#[test]
fn principal_set_reader_rejects_writes_while_writer_ingests() {
    let database = SharedDatabase::default();
    database.execute("CREATE TABLE events (id Int64);").unwrap();
    let principals = [
        ClickHousePrincipal::new("reader", "reader-key", ClickHousePrincipalRole::ReadOnly),
        ClickHousePrincipal::new("writer", "writer-key", ClickHousePrincipalRole::ReadWrite),
    ];

    let (reader_request, reader_body_offset) = request(
        "/insert/events",
        b"id\n1\n",
        "X-ClickHouse-User: reader\r\nX-ClickHouse-Key: reader-key\r\n",
    );
    let mut reader_input = Cursor::new(reader_request);
    let mut reader_response = Vec::new();
    handle_http_query_with_clickhouse_principal_set(
        &database,
        &principals,
        &mut reader_input,
        &mut reader_response,
    )
    .unwrap();
    assert_eq!(reader_input.position(), reader_body_offset);
    assert_status(&reader_response, "HTTP/1.1 404 Not Found");
    assert_private(&reader_response);

    let (writer_request, _) = request(
        "/insert/events",
        b"id\n2\n",
        "X-ClickHouse-User: writer\r\nX-ClickHouse-Key: writer-key\r\n",
    );
    let mut writer_response = Vec::new();
    handle_http_query_with_clickhouse_principal_set(
        &database,
        &principals,
        Cursor::new(writer_request),
        &mut writer_response,
    )
    .unwrap();
    assert_status(&writer_response, "HTTP/1.1 200 OK");
    assert_eq!(body(&writer_response), b"");
    assert_private(&writer_response);

    let mut query_response = Vec::new();
    handle_http_query_with_clickhouse_principal_set(
        &database,
        &principals,
        Cursor::new(
            b"GET /?query=SELECT+id+FROM+events%3B HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-User: reader\r\nX-ClickHouse-Key: reader-key\r\n\r\n",
        ),
        &mut query_response,
    )
    .unwrap();
    assert_status(&query_response, "HTTP/1.1 200 OK");
    assert_eq!(
        body(&query_response),
        br#"{"columns":[{"name":"id","type":"Int64"}],"rows":[[2]]}"#
    );
}

#[test]
fn principal_set_credential_failures_are_indistinguishable() {
    let database = SharedDatabase::default();
    let principals = [
        ClickHousePrincipal::new("reader", "reader-key", ClickHousePrincipalRole::ReadOnly),
        ClickHousePrincipal::new("writer", "writer-key", ClickHousePrincipalRole::ReadWrite),
    ];
    let rejected_headers = [
        "",
        "X-ClickHouse-User: reader\r\n",
        "X-ClickHouse-Key: reader-key\r\n",
        "X-ClickHouse-User: reader\r\nX-ClickHouse-Key: writer-key\r\n",
        "X-ClickHouse-User: unknown\r\nX-ClickHouse-Key: unknown\r\n",
        "X-ClickHouse-User: reader\r\nx-clickhouse-user: reader\r\nX-ClickHouse-Key: reader-key\r\n",
        "X-ClickHouse-User: reader\r\nX-ClickHouse-Key: reader-key\r\nx-clickhouse-key: reader-key\r\n",
    ];
    let mut expected_response = None;

    for headers in rejected_headers {
        let (request, body_offset) = request("/insert/events", b"id\n9\n", headers);
        let mut input = Cursor::new(request);
        let mut response = Vec::new();
        handle_http_query_with_clickhouse_principal_set(
            &database,
            &principals,
            &mut input,
            &mut response,
        )
        .unwrap();

        assert_eq!(input.position(), body_offset);
        assert_status(&response, "HTTP/1.1 401 Unauthorized");
        assert_eq!(
            body(&response),
            br#"{"error":"X-ClickHouse-User and X-ClickHouse-Key authentication required"}"#
        );
        assert_private(&response);
        if let Some(expected) = &expected_response {
            assert_eq!(&response, expected);
        } else {
            expected_response = Some(response);
        }
    }
}

#[test]
fn principal_set_rejects_duplicate_configuration_before_input() {
    let database = SharedDatabase::default();
    let principals = [
        ClickHousePrincipal::new("same", "key", ClickHousePrincipalRole::ReadOnly),
        ClickHousePrincipal::new("same", "key", ClickHousePrincipalRole::ReadWrite),
    ];
    let mut response = Vec::new();

    handle_http_query_with_clickhouse_principal_set(
        &database,
        &principals,
        FailingReader,
        &mut response,
    )
    .unwrap();

    assert_status(&response, "HTTP/1.1 500 Internal Server Error");
    assert_eq!(
        body(&response),
        br#"{"error":"configured ClickHouse principal set contains a duplicate user/key pair"}"#
    );
    assert_private(&response);
}

#[test]
fn principal_set_enforces_its_count_limit_before_input() {
    let database = SharedDatabase::default();
    let users = (0..=MAX_HTTP_NAMED_PRINCIPALS)
        .map(|index| format!("user-{index}"))
        .collect::<Vec<_>>();
    let keys = (0..=MAX_HTTP_NAMED_PRINCIPALS)
        .map(|index| format!("key-{index}"))
        .collect::<Vec<_>>();
    let principals = users
        .iter()
        .zip(&keys)
        .map(|(user, key)| ClickHousePrincipal::new(user, key, ClickHousePrincipalRole::ReadOnly))
        .collect::<Vec<_>>();

    let mut at_limit_response = Vec::new();
    handle_http_query_with_clickhouse_principal_set(
        &database,
        &principals[..MAX_HTTP_NAMED_PRINCIPALS],
        Cursor::new(
            b"GET /ping HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-User: user-0\r\nX-ClickHouse-Key: key-0\r\n\r\n",
        ),
        &mut at_limit_response,
    )
    .unwrap();
    assert_status(&at_limit_response, "HTTP/1.1 200 OK");

    let mut over_limit_response = Vec::new();
    handle_http_query_with_clickhouse_principal_set(
        &database,
        &principals,
        FailingReader,
        &mut over_limit_response,
    )
    .unwrap();
    assert_status(&over_limit_response, "HTTP/1.1 500 Internal Server Error");
    assert_eq!(
        body(&over_limit_response),
        br#"{"error":"configured ClickHouse principal set exceeds the principal limit"}"#
    );
    assert_private(&over_limit_response);
}

#[test]
fn key_only_read_only_api_remains_independent_of_named_principals() {
    let database = SharedDatabase::default();
    let mut response = Vec::new();
    handle_http_query_read_only_with_clickhouse_key(
        &database,
        KEY,
        Cursor::new(
            b"GET /?query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: read key:42\r\n\r\n",
        ),
        &mut response,
    )
    .unwrap();
    assert_status(&response, "HTTP/1.1 200 OK");
}

struct FailingReader;

impl Read for FailingReader {
    fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("input must not be read"))
    }
}

#[test]
fn named_principal_response_limit_failure_remains_typed() {
    let database = SharedDatabase::default();
    let mut response = Vec::new();
    let error = handle_http_query_read_only_with_clickhouse_principal_and_limits(
        &database,
        USER,
        KEY,
        Cursor::new(b"GET /ping HTTP/1.1\r\nHost: localhost\r\n\r\n"),
        &mut response,
        HttpQueryLimits {
            max_response_bytes: 0,
            ..HttpQueryLimits::default()
        },
    )
    .expect_err("the fixed response-limit error cannot fit");
    assert!(matches!(
        error,
        HttpQueryError::ResponseLimitExceeded { max_bytes: 0, .. }
    ));
    assert!(response.is_empty());
}
