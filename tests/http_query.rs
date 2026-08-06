use std::io::{self, Cursor, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};

use rusthouse::{
    HttpQueryError, HttpQueryLimits, SharedDatabase, TcpQueryError, TcpQuerySocketOption,
    accept_http_query, handle_http_query, handle_http_query_with_limits,
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

fn loopback_exchange(database: SharedDatabase, request: &[u8]) -> Vec<u8> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("loopback listener binds");
    let address = listener.local_addr().expect("listener has an address");
    let server = thread::spawn(move || {
        accept_http_query(
            &database,
            &listener,
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
    });

    let mut client = TcpStream::connect(address).expect("loopback client connects");
    client.write_all(request).expect("request is written");
    client
        .shutdown(Shutdown::Write)
        .expect("request half closes");
    let mut response = Vec::new();
    client
        .read_to_end(&mut response)
        .expect("response is read through connection close");
    server
        .join()
        .expect("server thread does not panic")
        .expect("TCP exchange succeeds");
    response
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

#[test]
fn tcp_query_accepts_one_loopback_select_and_closes_the_connection() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE readings (id Int64, label String); \
             INSERT INTO readings VALUES (2, 'two'), (1, 'one');",
        )
        .unwrap();

    let response = loopback_exchange(
        database,
        &request(b"SELECT id, label FROM readings ORDER BY id;"),
    );

    assert_response(
        &response,
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"},{"name":"label","type":"String"}],"rows":[[1,"one"],[2,"two"]]}"#,
    );
}

#[test]
fn tcp_query_returns_protocol_errors_over_loopback() {
    let response = loopback_exchange(
        SharedDatabase::default(),
        b"GET /query HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
    );

    assert_response(
        &response,
        "HTTP/1.1 405 Method Not Allowed",
        r#"{"error":"method must be POST"}"#,
    );
    assert!(
        std::str::from_utf8(&response)
            .unwrap()
            .contains("\r\nAllow: POST\r\n")
    );
}

#[test]
fn stalled_tcp_client_hits_the_read_timeout_and_is_closed() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("loopback listener binds");
    let address = listener.local_addr().expect("listener has an address");
    let database = SharedDatabase::default();
    let server = thread::spawn(move || {
        accept_http_query(
            &database,
            &listener,
            Duration::from_millis(100),
            Duration::from_secs(1),
        )
    });

    let mut client = TcpStream::connect(address).expect("loopback client connects");
    client
        .write_all(b"POST /query HTTP/1.1\r\nHost: localhost\r\n")
        .expect("incomplete request prefix is written");

    let error = server
        .join()
        .expect("server thread does not panic")
        .expect_err("stalled request times out");
    match error {
        TcpQueryError::Exchange(HttpQueryError::Read(error)) => assert!(matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        )),
        other => panic!("expected a typed HTTP read error, got {other:?}"),
    }

    client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut remaining = Vec::new();
    client
        .read_to_end(&mut remaining)
        .expect("server closes the timed-out connection");
    assert!(remaining.is_empty());
}

#[test]
fn slow_drip_cannot_extend_the_absolute_read_deadline() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("loopback listener binds");
    let address = listener.local_addr().expect("listener has an address");
    let database = SharedDatabase::default();
    let (finished_tx, finished_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let started = Instant::now();
        let result = accept_http_query(
            &database,
            &listener,
            Duration::from_millis(500),
            Duration::from_secs(1),
        );
        finished_tx.send((started.elapsed(), result)).unwrap();
    });

    let mut client = TcpStream::connect(address).expect("loopback client connects");
    for _ in 0..5 {
        client.write_all(b"P").expect("drip byte is written");
        thread::sleep(Duration::from_millis(40));
    }

    let keep_dripping = Arc::new(AtomicBool::new(true));
    let writer_flag = Arc::clone(&keep_dripping);
    let writer = thread::spawn(move || {
        let mut sent = 0;
        while writer_flag.load(Ordering::Relaxed) {
            if client.write_all(b"P").is_err() {
                break;
            }
            sent += 1;
            thread::sleep(Duration::from_millis(40));
        }
        sent
    });

    let finished = finished_rx.recv_timeout(Duration::from_millis(600));
    keep_dripping.store(false, Ordering::Relaxed);
    let drip_bytes = writer.join().expect("drip writer does not panic");
    server.join().expect("server thread does not panic");
    let (elapsed, result) = finished.expect("absolute deadline expires while bytes keep arriving");

    assert!(drip_bytes > 0);
    assert!(elapsed < Duration::from_millis(700));
    match result.expect_err("slow-drip request reaches its fixed deadline") {
        TcpQueryError::Exchange(HttpQueryError::Read(error)) => assert!(matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        )),
        other => panic!("expected a typed HTTP read error, got {other:?}"),
    }
}

#[test]
fn nonblocking_listener_waits_for_a_delayed_queued_request() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("loopback listener binds");
    listener.set_nonblocking(true).unwrap();
    let mut client = TcpStream::connect(listener.local_addr().unwrap())
        .expect("client connection is queued before accept");
    let complete_request = request(b"SELECT true AS ready;");
    client
        .write_all(&complete_request[..1])
        .expect("incomplete request prefix is queued");
    let database = SharedDatabase::default();
    let server = thread::spawn(move || {
        let accept_deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match accept_http_query(
                &database,
                &listener,
                Duration::from_secs(1),
                Duration::from_secs(1),
            ) {
                Err(TcpQueryError::Accept(error))
                    if error.kind() == io::ErrorKind::WouldBlock
                        && Instant::now() < accept_deadline =>
                {
                    thread::sleep(Duration::from_millis(1));
                }
                result => break result,
            }
        }
    });

    thread::sleep(Duration::from_millis(100));
    client
        .write_all(&complete_request[1..])
        .expect("delayed request remainder is written");
    client.shutdown(Shutdown::Write).unwrap();
    let mut response = Vec::new();
    client
        .read_to_end(&mut response)
        .expect("delayed request receives a response");
    server
        .join()
        .expect("server thread does not panic")
        .expect("accepted stream waits in blocking mode");

    assert_response(
        &response,
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"ready","type":"Bool"}],"rows":[[true]]}"#,
    );
}

#[test]
fn tcp_accept_and_socket_configuration_failures_remain_typed() {
    let database = SharedDatabase::default();
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("loopback listener binds");
    listener.set_nonblocking(true).unwrap();
    let error = accept_http_query(
        &database,
        &listener,
        Duration::from_secs(1),
        Duration::from_secs(1),
    )
    .expect_err("nonblocking accept has no client");
    assert!(matches!(error, TcpQueryError::Accept(_)));

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("loopback listener binds");
    let _client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let error = accept_http_query(&database, &listener, Duration::ZERO, Duration::from_secs(1))
        .expect_err("zero read timeout is rejected");
    assert!(matches!(
        error,
        TcpQueryError::SocketConfiguration {
            option: TcpQuerySocketOption::ReadTimeout,
            ..
        }
    ));

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("loopback listener binds");
    let _client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let error = accept_http_query(&database, &listener, Duration::from_secs(1), Duration::ZERO)
        .expect_err("zero write timeout is rejected");
    assert!(matches!(
        error,
        TcpQueryError::SocketConfiguration {
            option: TcpQuerySocketOption::WriteTimeout,
            ..
        }
    ));
}
