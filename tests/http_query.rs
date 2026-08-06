use std::io::{self, Cursor, Read, Write};
use std::sync::{Arc, RwLock};
use std::thread;

use rusthouse::batch::engine::Database;
use rusthouse::{
    HttpQueryError, HttpQueryLimits, SharedDatabase, handle_http_query,
    handle_http_query_with_bearer_token, handle_http_query_with_bearer_token_and_limits,
    handle_http_query_with_limits,
};

fn request(sql: &[u8]) -> Vec<u8> {
    let mut request = format!(
        "POST /query HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n",
        sql.len()
    )
    .into_bytes();
    request.extend_from_slice(sql);
    request
}

fn exchange(database: &SharedDatabase, request: &[u8]) -> Vec<u8> {
    let mut response = Vec::new();
    handle_http_query(database, Cursor::new(request), &mut response).expect("exchange succeeds");
    response
}

fn request_with_authorization(sql: &[u8], authorization_headers: &str) -> (Vec<u8>, u64) {
    let mut request = format!(
        "POST /query HTTP/1.1\r\nHost: localhost\r\n{authorization_headers}Content-Length: {}\r\n\r\n",
        sql.len()
    )
    .into_bytes();
    let body_offset = request.len() as u64;
    request.extend_from_slice(sql);
    (request, body_offset)
}

fn authenticated_exchange(database: &SharedDatabase, token: &str, request: &[u8]) -> Vec<u8> {
    let mut response = Vec::new();
    handle_http_query_with_bearer_token(database, token, Cursor::new(request), &mut response)
        .expect("authenticated exchange succeeds");
    response
}

fn assert_response(response: &[u8], status: &str, expected_body: &str) {
    let separator = b"\r\n\r\n";
    let split = response
        .windows(separator.len())
        .position(|window| window == separator)
        .expect("response has an empty header line");
    let headers = std::str::from_utf8(&response[..split]).expect("headers are UTF-8");
    let body = &response[split + separator.len()..];

    assert_eq!(headers.lines().next(), Some(status));
    assert!(headers.contains("\r\nContent-Type: application/json\r\n"));
    assert!(headers.contains("\r\nConnection: close"));
    assert!(headers.contains(&format!("\r\nContent-Length: {}\r\n", body.len())));
    assert_eq!(body, expected_body.as_bytes());
}

#[test]
fn valid_query_returns_the_existing_json_result_shape() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE readings (id Int64, label String); \
             INSERT INTO readings VALUES (2, 'two'), (1, 'one');",
        )
        .unwrap();
    let sql = b"SELECT id, label FROM readings ORDER BY id;";

    let response = exchange(&database, &request(sql));

    assert_response(
        &response,
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"},{"name":"label","type":"String"}],"rows":[[1,"one"],[2,"two"]]}"#,
    );
}

#[test]
fn bearer_authenticated_query_returns_the_existing_json_result_shape() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE readings (id Int64, label String); \
             INSERT INTO readings VALUES (2, 'two'), (1, 'one');",
        )
        .unwrap();
    let (request, _) = request_with_authorization(
        b"SELECT id, label FROM readings ORDER BY id;",
        "Authorization: Bearer correct-token_42\r\n",
    );

    let response = authenticated_exchange(&database, "correct-token_42", &request);

    assert_response(
        &response,
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"},{"name":"label","type":"String"}],"rows":[[1,"one"],[2,"two"]]}"#,
    );
}

#[test]
fn bearer_rejections_are_identical_and_do_not_consume_or_execute_the_body() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE retained (value Int64); INSERT INTO retained VALUES (7);")
        .unwrap();
    let rejected_headers = [
        ("missing", ""),
        (
            "duplicate",
            "Authorization: Bearer correct-token\r\nAuthorization: Bearer correct-token\r\n",
        ),
        ("wrong scheme", "Authorization: Basic correct-token\r\n"),
        ("missing token", "Authorization: Bearer\r\n"),
        (
            "embedded whitespace",
            "Authorization: Bearer two tokens\r\n",
        ),
        ("invalid padding", "Authorization: Bearer abc=def\r\n"),
        ("incorrect", "Authorization: Bearer incorrect-token\r\n"),
    ];
    let mut expected_response = None;

    for (name, headers) in rejected_headers {
        let (request, body_offset) = request_with_authorization(b"DROP TABLE retained;", headers);
        let mut input = Cursor::new(request);
        let mut response = Vec::new();

        handle_http_query_with_bearer_token(&database, "correct-token", &mut input, &mut response)
            .unwrap_or_else(|error| panic!("{name} credentials produce a response: {error}"));

        assert_eq!(
            input.position(),
            body_offset,
            "{name} credentials must not consume the SQL body"
        );
        assert_response(
            &response,
            "HTTP/1.1 401 Unauthorized",
            r#"{"error":"bearer authentication required"}"#,
        );
        assert!(
            std::str::from_utf8(&response)
                .unwrap()
                .contains("\r\nWWW-Authenticate: Bearer\r\n"),
            "{name} response advertises bearer authentication"
        );
        if let Some(expected_response) = &expected_response {
            assert_eq!(
                &response, expected_response,
                "credential failures must not disclose their rejection reason"
            );
        } else {
            expected_response = Some(response);
        }
    }

    assert_response(
        &exchange(&database, &request(b"SELECT value FROM retained;")),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"value","type":"Int64"}],"rows":[[7]]}"#,
    );
}

#[test]
fn bearer_rejection_does_not_lock_the_database() {
    let inner = Arc::new(RwLock::new(Database::new()));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let poisoner = thread::spawn(move || {
        let _guard = inner.write().unwrap();
        panic!("poison the database lock");
    });
    assert!(poisoner.join().is_err());
    let (request, _) =
        request_with_authorization(b"SELECT 1;", "Authorization: Bearer incorrect-token\r\n");

    let response = authenticated_exchange(&database, "correct-token", &request);

    assert_response(
        &response,
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"bearer authentication required"}"#,
    );
}

#[test]
fn empty_configured_bearer_token_is_rejected_before_input_or_database_access() {
    let database = SharedDatabase::default();
    let mut response = Vec::new();

    handle_http_query_with_bearer_token(&database, "", FailingReader, &mut response)
        .expect("invalid configuration produces a response without reading input");

    assert_response(
        &response,
        "HTTP/1.1 500 Internal Server Error",
        r#"{"error":"configured bearer token must not be empty"}"#,
    );
}

#[test]
fn bearer_rejection_respects_the_complete_response_cap() {
    let database = SharedDatabase::default();
    let (request, _) = request_with_authorization(b"SELECT 1;", "");
    let unrestricted = authenticated_exchange(&database, "correct-token", &request);
    let limits = HttpQueryLimits {
        max_response_bytes: unrestricted.len() - 1,
        ..HttpQueryLimits::default()
    };
    let mut response = Vec::new();

    handle_http_query_with_bearer_token_and_limits(
        &database,
        "correct-token",
        Cursor::new(&request),
        &mut response,
        limits,
    )
    .unwrap();

    assert!(response.len() <= limits.max_response_bytes);
    assert_response(
        &response,
        "HTTP/1.1 500 Internal Server Error",
        r#"{"error":"response exceeds configured byte limit"}"#,
    );

    let mut too_small_output = Vec::new();
    let error = handle_http_query_with_bearer_token_and_limits(
        &database,
        "correct-token",
        Cursor::new(&request),
        &mut too_small_output,
        HttpQueryLimits {
            max_response_bytes: 0,
            ..HttpQueryLimits::default()
        },
    )
    .expect_err("even the fixed limit response cannot fit");
    assert!(matches!(
        error,
        HttpQueryError::ResponseLimitExceeded { max_bytes: 0, .. }
    ));
    assert!(too_small_output.is_empty());
}

#[test]
fn request_headers_are_case_insensitive_but_framing_is_strict() {
    let database = SharedDatabase::default();
    let sql = b"SELECT true AS ready;";
    let mut mixed_case = format!(
        "POST /query HTTP/1.1\r\nhOsT: example.test\r\ncOnTeNt-LeNgTh:\t{} \r\n\r\n",
        sql.len()
    )
    .into_bytes();
    mixed_case.extend_from_slice(sql);

    assert_response(
        &exchange(&database, &mixed_case),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"ready","type":"Bool"}],"rows":[[true]]}"#,
    );

    let lf_only = format!(
        "POST /query HTTP/1.1\nHost: localhost\nContent-Length: {}\n\n{}",
        sql.len(),
        std::str::from_utf8(sql).unwrap()
    );
    assert_response(
        &exchange(&database, lf_only.as_bytes()),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"HTTP headers require CRLF framing"}"#,
    );
}

struct PrefixThenWouldBlock {
    prefix: Cursor<Vec<u8>>,
}

impl PrefixThenWouldBlock {
    fn new(prefix: impl Into<Vec<u8>>) -> Self {
        Self {
            prefix: Cursor::new(prefix.into()),
        }
    }
}

impl Read for PrefixThenWouldBlock {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.prefix.position() < self.prefix.get_ref().len() as u64 {
            self.prefix.read(buffer)
        } else {
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "persistent connection has no more bytes yet",
            ))
        }
    }
}

#[test]
fn bare_lf_is_rejected_before_a_persistent_reader_needs_more_bytes() {
    let database = SharedDatabase::default();
    let input = PrefixThenWouldBlock::new(b"POST /query HTTP/1.1\n".to_vec());
    let mut response = Vec::new();

    handle_http_query(&database, input, &mut response).expect("bare LF produces a response");

    assert_response(
        &response,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"HTTP headers require CRLF framing"}"#,
    );
}

#[test]
fn expect_is_rejected_before_reading_a_body_from_a_persistent_reader() {
    let database = SharedDatabase::default();
    let input = PrefixThenWouldBlock::new(
        b"POST /query HTTP/1.1\r\nHost: localhost\r\nContent-Length: 9\r\nExpect: 100-continue\r\n\r\n"
            .to_vec(),
    );
    let mut response = Vec::new();

    handle_http_query(&database, input, &mut response)
        .expect("Expect produces a final response before the body is read");

    assert_response(
        &response,
        "HTTP/1.1 417 Expectation Failed",
        r#"{"error":"Expect header is not supported"}"#,
    );
}

#[test]
fn malformed_and_truncated_requests_have_deterministic_responses() {
    let database = SharedDatabase::default();
    let cases: &[(&[u8], &str, &str)] = &[
        (
            b"POST  /query HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 400 Bad Request",
            r#"{"error":"malformed request line"}"#,
        ),
        (
            b"POST /query HTTP/1.1\r\nHost: localhost\r\n\r\n",
            "HTTP/1.1 411 Length Required",
            r#"{"error":"Content-Length header is required"}"#,
        ),
        (
            b"POST /query HTTP/1.1\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 400 Bad Request",
            r#"{"error":"Host header is required"}"#,
        ),
        (
            b"POST /query HTTP/1.1\r\nHost: localhost\r\nContent-Length: nope\r\n\r\n",
            "HTTP/1.1 400 Bad Request",
            r#"{"error":"invalid Content-Length header"}"#,
        ),
        (
            b"POST /query HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\nx",
            "HTTP/1.1 400 Bad Request",
            r#"{"error":"duplicate Content-Length header"}"#,
        ),
        (
            b"POST /query HTTP/1.1\r\nHost: localhost\r\nContent-Length: 10\r\n\r\nSELECT 1",
            "HTTP/1.1 400 Bad Request",
            r#"{"error":"request body is shorter than Content-Length"}"#,
        ),
    ];

    for (request, status, body) in cases {
        assert_response(&exchange(&database, request), status, body);
    }

    let invalid_utf8 = request(&[0xff]);
    assert_response(
        &exchange(&database, &invalid_utf8),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"SQL body is not valid UTF-8"}"#,
    );
}

#[test]
fn method_target_version_and_chunked_encoding_are_rejected() {
    let database = SharedDatabase::default();
    let method = exchange(
        &database,
        b"GET /query HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
    );
    assert_response(
        &method,
        "HTTP/1.1 405 Method Not Allowed",
        r#"{"error":"method must be POST"}"#,
    );
    assert!(
        std::str::from_utf8(&method)
            .unwrap()
            .contains("\r\nAllow: POST\r\n")
    );

    assert_response(
        &exchange(
            &database,
            b"POST /other HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
        ),
        "HTTP/1.1 404 Not Found",
        r#"{"error":"request target must be /query"}"#,
    );
    assert_response(
        &exchange(
            &database,
            b"POST /query HTTP/1.0\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
        ),
        "HTTP/1.1 505 HTTP Version Not Supported",
        r#"{"error":"HTTP/1.1 is required"}"#,
    );
    assert_response(
        &exchange(
            &database,
            b"POST /query HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n8\r\nSELECT 1\r\n0\r\n\r\n",
        ),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"Transfer-Encoding is not supported"}"#,
    );
}

#[test]
fn request_header_count_header_bytes_and_sql_body_are_bounded() {
    let database = SharedDatabase::default();

    let mut response = Vec::new();
    let limits = HttpQueryLimits {
        max_header_bytes: 32,
        ..HttpQueryLimits::default()
    };
    handle_http_query_with_limits(
        &database,
        Cursor::new(request(b"SELECT 1;")),
        &mut response,
        limits,
    )
    .unwrap();
    assert_response(
        &response,
        "HTTP/1.1 431 Request Header Fields Too Large",
        r#"{"error":"request headers exceed configured byte limit"}"#,
    );

    response.clear();
    let limits = HttpQueryLimits {
        max_header_count: 1,
        ..HttpQueryLimits::default()
    };
    handle_http_query_with_limits(
        &database,
        Cursor::new(request(b"SELECT 1;")),
        &mut response,
        limits,
    )
    .unwrap();
    assert_response(
        &response,
        "HTTP/1.1 431 Request Header Fields Too Large",
        r#"{"error":"request has too many header fields"}"#,
    );

    response.clear();
    let limits = HttpQueryLimits {
        max_sql_bytes: 4,
        ..HttpQueryLimits::default()
    };
    handle_http_query_with_limits(
        &database,
        Cursor::new(request(b"SELECT 1;")),
        &mut response,
        limits,
    )
    .unwrap();
    assert_response(
        &response,
        "HTTP/1.1 413 Payload Too Large",
        r#"{"error":"request body exceeds configured byte limit"}"#,
    );
}

#[test]
fn mutating_and_multi_statement_sql_are_rejected_without_side_effects() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE retained (value Int64); INSERT INTO retained VALUES (7);")
        .unwrap();

    assert_response(
        &exchange(&database, &request(b"DROP TABLE retained;")),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"read-only query accepts only SELECT, SHOW TABLES, SHOW CREATE TABLE, or DESCRIBE TABLE; found DROP TABLE"}"#,
    );
    assert_response(
        &exchange(&database, &request(b"SHOW TABLES; SHOW TABLES;")),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"read-only query requires exactly one statement; found 2"}"#,
    );
    assert_response(
        &exchange(&database, &request(b"SELECT value FROM retained;")),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"value","type":"Int64"}],"rows":[[7]]}"#,
    );
}

#[test]
fn complete_response_is_capped_before_any_bytes_are_written() {
    let database = SharedDatabase::default();
    let large_sql = format!("SELECT '{}' AS value;", "x".repeat(1_000));
    let limits = HttpQueryLimits {
        max_response_bytes: 512,
        ..HttpQueryLimits::default()
    };
    let mut response = Vec::new();

    handle_http_query_with_limits(
        &database,
        Cursor::new(request(large_sql.as_bytes())),
        &mut response,
        limits,
    )
    .unwrap();

    assert!(response.len() <= limits.max_response_bytes);
    assert_response(
        &response,
        "HTTP/1.1 500 Internal Server Error",
        r#"{"error":"response exceeds configured byte limit"}"#,
    );

    let mut too_small_output = Vec::new();
    let error = handle_http_query_with_limits(
        &database,
        Cursor::new(request(b"SELECT 1;")),
        &mut too_small_output,
        HttpQueryLimits {
            max_response_bytes: 0,
            ..HttpQueryLimits::default()
        },
    )
    .expect_err("even the fixed error response cannot fit");
    assert!(matches!(
        error,
        HttpQueryError::ResponseLimitExceeded { max_bytes: 0, .. }
    ));
    assert!(too_small_output.is_empty());
}

struct FailingReader;

impl Read for FailingReader {
    fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("intentional read failure"))
    }
}

struct FailingWriter;

impl Write for FailingWriter {
    fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("intentional write failure"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn read_and_write_failures_remain_typed() {
    let database = SharedDatabase::default();
    let mut no_output = Vec::new();
    let read_error = handle_http_query(&database, FailingReader, &mut no_output)
        .expect_err("reader failure is returned");
    assert!(matches!(read_error, HttpQueryError::Read(_)));
    assert!(no_output.is_empty());

    let write_error =
        handle_http_query(&database, Cursor::new(request(b"SELECT 1;")), FailingWriter)
            .expect_err("writer failure is returned");
    assert!(matches!(write_error, HttpQueryError::Write(_)));
}
