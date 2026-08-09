use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rusthouse::{
    HttpConnectionError, HttpListenerError, HttpListenerLimits, HttpListenerReport, HttpQueryError,
    HttpQueryLimits, SharedDatabase, serve_http_read_only_with_limits,
};

const CLIENT_TIMEOUT: Duration = Duration::from_secs(2);

fn spawn_listener(
    database: &SharedDatabase,
    limits: HttpListenerLimits,
) -> (
    SocketAddr,
    JoinHandle<Result<HttpListenerReport, HttpListenerError>>,
) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback listener");
    let address = listener.local_addr().expect("listener address");
    let database = database.clone();
    let server =
        thread::spawn(move || serve_http_read_only_with_limits(&database, listener, limits));
    (address, server)
}

fn exchange(address: SocketAddr, request: &[u8]) -> Vec<u8> {
    let mut stream = TcpStream::connect(address).expect("connect to loopback listener");
    stream
        .set_read_timeout(Some(CLIENT_TIMEOUT))
        .expect("set client read timeout");
    stream.write_all(request).expect("write HTTP request");
    stream
        .shutdown(Shutdown::Write)
        .expect("finish HTTP request");

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .expect("read complete HTTP response");
    response
}

fn query_request(sql: &str) -> Vec<u8> {
    format!(
        "POST /query HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{sql}",
        sql.len()
    )
    .into_bytes()
}

fn response_text(response: &[u8]) -> &str {
    std::str::from_utf8(response).expect("HTTP response is UTF-8")
}

#[test]
fn serves_multiple_connections_and_isolates_a_malformed_client() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE readings (value Int64); \
             INSERT INTO readings VALUES (7), (11);",
        )
        .expect("setup shared database");
    let limits = HttpListenerLimits {
        max_connections: 4,
        connection_timeout: CLIENT_TIMEOUT,
        ..HttpListenerLimits::default()
    };
    let (address, server) = spawn_listener(&database, limits);

    let first = exchange(
        address,
        &query_request("SELECT value FROM readings ORDER BY value;"),
    );
    assert!(response_text(&first).starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(response_text(&first).contains(r#""rows":[[7],[11]]"#));

    let mutation = exchange(address, &query_request("INSERT INTO readings VALUES (13);"));
    assert!(response_text(&mutation).starts_with("HTTP/1.1 400 Bad Request\r\n"));

    let malformed = exchange(address, b"GET /ping HTTP/1.1\nHost: localhost\n\n");
    assert!(response_text(&malformed).starts_with("HTTP/1.1 400 Bad Request\r\n"));

    let third = exchange(
        address,
        &query_request("SELECT COUNT(*) AS rows FROM readings;"),
    );
    assert!(
        response_text(&third).starts_with("HTTP/1.1 200 OK\r\n"),
        "unexpected response: {}",
        response_text(&third)
    );
    assert!(response_text(&third).contains(r#""rows":[[2]]"#));

    let report = server
        .join()
        .expect("listener thread does not panic")
        .expect("listener completes");
    assert_eq!(report.accepted_connections, 4);
    assert_eq!(report.completed_connections, 4);
    assert!(report.connection_failures.is_empty());
}

#[test]
fn closes_each_connection_after_exactly_one_response() {
    let limits = HttpListenerLimits {
        max_connections: 1,
        connection_timeout: CLIENT_TIMEOUT,
        ..HttpListenerLimits::default()
    };
    let (address, server) = spawn_listener(&SharedDatabase::default(), limits);
    let pipelined = concat!(
        "GET /ping HTTP/1.1\r\nHost: localhost\r\n\r\n",
        "GET /ping HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );

    let response = exchange(address, pipelined.as_bytes());
    assert_eq!(
        response_text(&response).matches("HTTP/1.1 200 OK").count(),
        1
    );
    assert!(response_text(&response).contains("Connection: close\r\n"));

    let report = server
        .join()
        .expect("listener thread does not panic")
        .expect("listener completes at its finite connection limit");
    assert_eq!(report.accepted_connections, 1);
    assert_eq!(report.completed_connections, 1);
    assert!(report.connection_failures.is_empty());
}

#[test]
fn transport_failure_is_typed_and_does_not_stop_the_next_client() {
    let limits = HttpListenerLimits {
        max_connections: 2,
        connection_timeout: Duration::from_millis(100),
        ..HttpListenerLimits::default()
    };
    let (address, server) = spawn_listener(&SharedDatabase::default(), limits);

    let mut stalled = TcpStream::connect(address).expect("connect stalled client");
    stalled
        .set_read_timeout(Some(CLIENT_TIMEOUT))
        .expect("set stalled-client timeout");
    let mut response = Vec::new();
    stalled
        .read_to_end(&mut response)
        .expect("server closes timed-out connection");
    assert!(response.is_empty());

    let healthy = exchange(address, b"GET /ping HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(response_text(&healthy).starts_with("HTTP/1.1 200 OK\r\n"));

    let report = server
        .join()
        .expect("listener thread does not panic")
        .expect("listener continues after connection failure");
    assert_eq!(report.accepted_connections, 2);
    assert_eq!(report.completed_connections, 1);
    assert_eq!(report.connection_failures.len(), 1);
    assert_eq!(report.connection_failures[0].connection, 1);
    match &report.connection_failures[0].error {
        HttpConnectionError::Exchange(HttpQueryError::Read(error)) => assert!(matches!(
            error.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        )),
        error => panic!("expected a typed request-read timeout, found {error:?}"),
    }
}

#[test]
fn forwards_exchange_limits_and_rejects_an_unbounded_timeout() {
    let limits = HttpListenerLimits {
        max_connections: 1,
        connection_timeout: CLIENT_TIMEOUT,
        query_limits: HttpQueryLimits {
            max_header_bytes: 32,
            ..HttpQueryLimits::default()
        },
    };
    let (address, server) = spawn_listener(&SharedDatabase::default(), limits);

    let response = exchange(address, b"GET /ping HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(
        response_text(&response).starts_with("HTTP/1.1 431 Request Header Fields Too Large\r\n")
    );
    let report = server
        .join()
        .expect("listener thread does not panic")
        .expect("bounded rejection completes");
    assert_eq!(report.accepted_connections, 1);
    assert_eq!(report.completed_connections, 1);

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind validation listener");
    let error = serve_http_read_only_with_limits(
        &SharedDatabase::default(),
        listener,
        HttpListenerLimits {
            max_connections: 0,
            connection_timeout: Duration::ZERO,
            ..HttpListenerLimits::default()
        },
    )
    .expect_err("zero timeout is rejected before accepting");
    assert!(matches!(error, HttpListenerError::InvalidConnectionTimeout));
}

#[test]
fn zero_connection_limit_returns_without_accepting() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind finite listener");
    let report = serve_http_read_only_with_limits(
        &SharedDatabase::default(),
        listener,
        HttpListenerLimits {
            max_connections: 0,
            ..HttpListenerLimits::default()
        },
    )
    .expect("zero-connection listener completes immediately");

    assert_eq!(report.accepted_connections, 0);
    assert_eq!(report.completed_connections, 0);
    assert!(report.connection_failures.is_empty());
}
