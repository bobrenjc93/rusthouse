use std::io::{self, Cursor, Read};

use rusthouse::{
    HttpQueryError, HttpQueryLimits, SharedDatabase,
    handle_http_query_read_only_with_clickhouse_key,
    handle_http_query_read_only_with_clickhouse_principal,
    handle_http_query_read_only_with_clickhouse_principal_and_limits,
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
        let mut input = Cursor::new(request);
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
        if let Some(expected) = &expected_response {
            assert_eq!(&response, expected, "{name} must be indistinguishable");
        } else {
            expected_response = Some(response);
        }
    }
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
