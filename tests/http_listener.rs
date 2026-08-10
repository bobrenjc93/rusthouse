use std::error::Error as StdError;
use std::io::{Cursor, ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::num::NonZeroUsize;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rusthouse::batch::csv::CsvIngestLimits;
use rusthouse::batch::engine::Database;
use rusthouse::{
    HttpListenerError, HttpListenerLimits, HttpListenerReport, HttpQueryError, HttpQueryLimits,
    SharedDatabase, handle_http_query, handle_http_query_read_only_with_clickhouse_key,
    serve_http_read_only, serve_http_read_only_concurrently_with_clickhouse_key_and_limits,
    serve_http_read_only_with_clickhouse_key, serve_http_read_only_with_limits,
    serve_http_with_clickhouse_key, serve_http_with_clickhouse_key_and_limits,
};

const IO_TIMEOUT: Duration = Duration::from_secs(2);
const STALLED_CONNECTION_TIMEOUT: Duration = Duration::from_millis(100);
const STALLED_RESPONSE_BYTES: usize = 15 * 1024 * 1024;
const TRICKLE_INTERVAL: Duration = Duration::from_millis(20);
const TRICKLE_BYTES: usize = 20;

fn start_listener(
    database: SharedDatabase,
    limits: HttpListenerLimits,
) -> (
    SocketAddr,
    JoinHandle<Result<HttpListenerReport, HttpListenerError>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let address = listener.local_addr().expect("read loopback address");
    let worker =
        thread::spawn(move || serve_http_read_only_with_limits(&listener, &database, limits));
    (address, worker)
}

fn finish_request_stream(stream: &TcpStream) {
    match stream.shutdown(Shutdown::Write) {
        Ok(()) => {}
        // The listener may authenticate or reject a complete request and close
        // the connection before the client half-closes it. On Linux that valid
        // close race is reported as ENOTCONN.
        Err(error) if error.kind() == ErrorKind::NotConnected => {}
        Err(error) => panic!("finish request stream: {error}"),
    }
}

fn exchange(address: SocketAddr, request: &[u8]) -> Vec<u8> {
    let mut stream = TcpStream::connect(address).expect("connect to loopback listener");
    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .expect("set client read timeout");
    stream
        .set_write_timeout(Some(IO_TIMEOUT))
        .expect("set client write timeout");
    stream.write_all(request).expect("write complete request");
    finish_request_stream(&stream);

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .expect("listener closes the connection after its response");
    response
}

fn start_exchange(address: SocketAddr, request: Vec<u8>) -> (Receiver<Vec<u8>>, JoinHandle<()>) {
    let (request_sent_sender, request_sent_receiver) = mpsc::sync_channel(0);
    let (response_sender, response_receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut stream = TcpStream::connect(address).expect("connect to loopback listener");
        stream
            .set_read_timeout(Some(IO_TIMEOUT))
            .expect("set client read timeout");
        stream
            .set_write_timeout(Some(IO_TIMEOUT))
            .expect("set client write timeout");
        stream.write_all(&request).expect("write complete request");
        finish_request_stream(&stream);
        request_sent_sender
            .send(())
            .expect("report completed client request");

        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .expect("listener closes the connection after its response");
        response_sender
            .send(response)
            .expect("return loopback response");
    });
    request_sent_receiver
        .recv()
        .expect("client reports completed request");
    (response_receiver, worker)
}

fn post_query(sql: &str) -> Vec<u8> {
    format!(
        "POST /query HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{sql}",
        sql.len()
    )
    .into_bytes()
}

fn post_query_with_clickhouse_key(sql: &str, key: &str) -> Vec<u8> {
    format!(
        "POST /query HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: {key}\r\nContent-Length: {}\r\n\r\n{sql}",
        sql.len()
    )
    .into_bytes()
}

fn post_target_with_clickhouse_key(target: &str, body: &[u8], key: &str) -> Vec<u8> {
    let mut request = format!(
        "POST {target} HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: {key}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(body);
    request
}

fn body(response: &[u8]) -> &[u8] {
    let body_offset = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("response has a header terminator")
        + 4;
    &response[body_offset..]
}

#[test]
fn sequential_connections_share_one_read_only_database_and_stop_at_the_limit() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE readings (value Int64); INSERT INTO readings VALUES (7), (11);")
        .unwrap();
    let (address, worker) = start_listener(
        database.clone(),
        HttpListenerLimits::new(3, HttpQueryLimits::default()),
    );

    let first = exchange(
        address,
        &post_query("SELECT value FROM readings ORDER BY value;"),
    );
    assert!(first.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert_eq!(
        body(&first),
        br#"{"columns":[{"name":"value","type":"Int64"}],"rows":[[7],[11]]}"#
    );
    assert!(
        first
            .windows(19)
            .any(|window| window == b"Connection: close\r\n")
    );

    let mutation = exchange(address, &post_query("INSERT INTO readings VALUES (99);"));
    assert!(mutation.starts_with(b"HTTP/1.1 400 Bad Request\r\n"));
    assert!(String::from_utf8_lossy(body(&mutation)).contains("read-only query"));

    let final_query = exchange(address, &post_query("SELECT COUNT(*) FROM readings;"));
    assert!(final_query.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert_eq!(
        body(&final_query),
        br#"{"columns":[{"name":"COUNT(*)","type":"Int64"}],"rows":[[2]]}"#
    );

    let report = worker
        .join()
        .expect("listener thread did not panic")
        .expect("listener completed its connection budget");
    assert_eq!(report.accepted_connections, 3);
    assert_eq!(report.successful_exchanges, 3);
    assert!(report.connection_failures.is_empty());

    let rows = database
        .query("SELECT value FROM readings ORDER BY value;")
        .unwrap();
    assert_eq!(rows.rows.len(), 2);
}

#[test]
fn authenticated_listener_isolates_rejected_keys_and_preserves_read_only_state() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE readings (value Int64); INSERT INTO readings VALUES (7), (11);")
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let address = listener.local_addr().expect("read loopback address");
    let served_database = database.clone();
    let worker = thread::spawn(move || {
        serve_http_read_only_with_clickhouse_key(&listener, &served_database, "correct-key", 5)
    });

    let missing = exchange(
        address,
        &post_query("SELECT value FROM readings ORDER BY value;"),
    );
    assert!(missing.starts_with(b"HTTP/1.1 401 Unauthorized\r\n"));
    assert!(
        missing
            .windows(b"WWW-Authenticate: X-ClickHouse-Key\r\n".len())
            .any(|window| window == b"WWW-Authenticate: X-ClickHouse-Key\r\n")
    );

    let incorrect = exchange(
        address,
        &post_query_with_clickhouse_key(
            "SELECT value FROM readings ORDER BY value;",
            "incorrect-key",
        ),
    );
    assert_eq!(incorrect, missing);

    let correct = exchange(
        address,
        &post_query_with_clickhouse_key(
            "SELECT value FROM readings ORDER BY value;",
            "correct-key",
        ),
    );
    assert!(correct.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert_eq!(
        body(&correct),
        br#"{"columns":[{"name":"value","type":"Int64"}],"rows":[[7],[11]]}"#
    );
    assert!(
        correct
            .windows(b"Cache-Control: private, no-store\r\n".len())
            .any(|window| window == b"Cache-Control: private, no-store\r\n")
    );

    let settings = exchange(
        address,
        &post_query_with_clickhouse_key("SELECT name, value FROM system.settings;", "correct-key"),
    );
    assert!(settings.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert!(
        body(&settings)
            .windows(b"query_result_limits.max_rows".len())
            .any(|window| window == b"query_result_limits.max_rows")
    );

    let mutation = exchange(
        address,
        &post_query_with_clickhouse_key("INSERT INTO readings VALUES (99);", "correct-key"),
    );
    assert!(mutation.starts_with(b"HTTP/1.1 400 Bad Request\r\n"));
    assert!(String::from_utf8_lossy(body(&mutation)).contains("read-only query"));

    let report = worker
        .join()
        .expect("listener thread did not panic")
        .expect("listener completed its connection budget");
    assert_eq!(report.accepted_connections, 5);
    assert_eq!(report.successful_exchanges, 5);
    assert!(report.connection_failures.is_empty());

    let rows = database
        .query("SELECT value FROM readings ORDER BY value;")
        .unwrap();
    assert_eq!(rows.rows.len(), 2);
}

#[test]
fn authenticated_read_write_listener_commits_for_later_reads_and_rolls_back_failures() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE readings (value Int64);")
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let address = listener.local_addr().expect("read loopback address");
    let served_database = database.clone();
    let worker = thread::spawn(move || {
        serve_http_with_clickhouse_key(&listener, &served_database, "correct-key", 4)
    });

    let rejected = exchange(
        address,
        b"POST /insert HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: wrong-key\r\nX-ClickHouse-Database: other\r\nContent-Length: 0\r\n\r\n",
    );
    assert!(rejected.starts_with(b"HTTP/1.1 401 Unauthorized\r\n"));
    assert!(
        rejected
            .windows(b"WWW-Authenticate: X-ClickHouse-Key\r\n".len())
            .any(|window| window == b"WWW-Authenticate: X-ClickHouse-Key\r\n")
    );

    let inserted = exchange(
        address,
        &post_target_with_clickhouse_key(
            "/insert",
            b"INSERT INTO readings VALUES (7), (11);",
            "correct-key",
        ),
    );
    assert!(inserted.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert!(body(&inserted).is_empty());

    let rolled_back = exchange(
        address,
        &post_target_with_clickhouse_key(
            "/insert",
            b"INSERT INTO readings VALUES (13); INSERT INTO readings VALUES ('wrong');",
            "correct-key",
        ),
    );
    assert!(rolled_back.starts_with(b"HTTP/1.1 400 Bad Request\r\n"));

    let later_read = exchange(
        address,
        &post_query_with_clickhouse_key(
            "SELECT value FROM readings ORDER BY value;",
            "correct-key",
        ),
    );
    assert!(later_read.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert_eq!(
        body(&later_read),
        br#"{"columns":[{"name":"value","type":"Int64"}],"rows":[[7],[11]]}"#
    );

    let report = worker.join().unwrap().unwrap();
    assert_eq!(report.accepted_connections, 4);
    assert_eq!(report.successful_exchanges, 4);
    assert!(report.connection_failures.is_empty());
}

#[test]
fn authenticated_read_write_listener_limits_ingestion_and_never_waits_for_admission() {
    let mut initial = Database::new();
    initial
        .execute("CREATE TABLE events (id Int64, label String);")
        .unwrap();
    let inner = Arc::new(RwLock::new(initial));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let reader = inner.read().unwrap();
    let limits = HttpListenerLimits::new(
        4,
        HttpQueryLimits {
            csv_ingest_limits: CsvIngestLimits::new(1_024, 1, 2),
            ..HttpQueryLimits::default()
        },
    );
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let address = listener.local_addr().expect("read loopback address");
    let served_database = database.clone();
    let worker = thread::spawn(move || {
        serve_http_with_clickhouse_key_and_limits(
            &listener,
            &served_database,
            "correct-key",
            limits,
        )
    });

    let contended = exchange(
        address,
        &post_target_with_clickhouse_key(
            "/insert",
            b"INSERT INTO events VALUES (9, 'blocked');",
            "correct-key",
        ),
    );
    assert!(contended.starts_with(b"HTTP/1.1 503 Service Unavailable\r\n"));
    assert_eq!(body(&contended), br#"{"error":"database is unavailable"}"#);
    assert_eq!(reader.catalog().table("events").unwrap().row_count(), 0);
    drop(reader);

    let over_limit = exchange(
        address,
        &post_target_with_clickhouse_key(
            "/insert/events",
            b"id,label\n1,one\n2,two\n",
            "correct-key",
        ),
    );
    assert!(over_limit.starts_with(b"HTTP/1.1 400 Bad Request\r\n"));
    assert!(String::from_utf8_lossy(body(&over_limit)).contains("exceeding the limit of 1"));

    let inserted = exchange(
        address,
        &post_target_with_clickhouse_key("/insert/events", b"id,label\n1,one\n", "correct-key"),
    );
    assert!(inserted.starts_with(b"HTTP/1.1 200 OK\r\n"));

    let later_read = exchange(
        address,
        &post_query_with_clickhouse_key("SELECT id, label FROM events ORDER BY id;", "correct-key"),
    );
    assert_eq!(
        body(&later_read),
        br#"{"columns":[{"name":"id","type":"Int64"},{"name":"label","type":"String"}],"rows":[[1,"one"]]}"#
    );

    let report = worker.join().unwrap().unwrap();
    assert_eq!(report.accepted_connections, 4);
    assert_eq!(report.successful_exchanges, 4);
    assert!(report.connection_failures.is_empty());
}

#[test]
fn authenticated_read_write_listener_explicit_limits_keep_deadline_failures_connection_local() {
    let limits = HttpListenerLimits {
        read_timeout: STALLED_CONNECTION_TIMEOUT,
        write_timeout: IO_TIMEOUT,
        query_limits: HttpQueryLimits {
            max_sql_bytes: 8,
            ..HttpQueryLimits::default()
        },
        ..HttpListenerLimits::new(3, HttpQueryLimits::default())
    };
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let address = listener.local_addr().expect("read loopback address");
    let worker = thread::spawn(move || {
        serve_http_with_clickhouse_key_and_limits(
            &listener,
            &SharedDatabase::default(),
            "correct-key",
            limits,
        )
    });

    let mut stalled = TcpStream::connect(address).expect("connect stalled client");
    stalled
        .set_write_timeout(Some(IO_TIMEOUT))
        .expect("set stalled client write timeout");
    stalled
        .write_all(b"GET /ping HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: correct-key")
        .expect("write incomplete authenticated request");

    let limited = exchange(
        address,
        &post_query_with_clickhouse_key("SELECT 123;", "correct-key"),
    );
    assert!(limited.starts_with(b"HTTP/1.1 413 Payload Too Large\r\n"));

    let healthy = exchange(
        address,
        b"GET /ping HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: correct-key\r\n\r\n",
    );
    assert!(healthy.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert_eq!(body(&healthy), b"Ok.\n");

    let report = worker.join().unwrap().unwrap();
    assert_eq!(report.accepted_connections, 3);
    assert_eq!(report.successful_exchanges, 2);
    assert_eq!(report.connection_failures.len(), 1);
    assert_eq!(report.connection_failures[0].connection, 1);
    assert!(matches!(
        &report.connection_failures[0].source,
        HttpQueryError::Read(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            )
    ));
}

#[test]
fn capped_authenticated_concurrency_serves_fast_client_while_first_client_is_stalled() {
    let limits = HttpListenerLimits {
        read_timeout: IO_TIMEOUT,
        write_timeout: IO_TIMEOUT,
        ..HttpListenerLimits::new(2, HttpQueryLimits::default())
    };
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let address = listener.local_addr().expect("read loopback address");
    let worker = thread::spawn(move || {
        serve_http_read_only_concurrently_with_clickhouse_key_and_limits(
            &listener,
            &SharedDatabase::default(),
            "correct-key",
            limits,
            NonZeroUsize::new(2).unwrap(),
        )
    });

    let mut stalled = TcpStream::connect(address).expect("connect stalled client");
    stalled
        .set_read_timeout(Some(IO_TIMEOUT))
        .expect("set stalled client read timeout");
    stalled
        .set_write_timeout(Some(IO_TIMEOUT))
        .expect("set stalled client write timeout");
    stalled
        .write_all(b"GET /ping HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: correct-key")
        .expect("write incomplete authenticated request");

    let pipelined = b"GET /ping HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: correct-key\r\n\r\nGET /ping HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: correct-key\r\n\r\n";
    let (fast_response, fast_client) = start_exchange(address, pipelined.to_vec());
    let fast_response = fast_response
        .recv_timeout(Duration::from_secs(1))
        .expect("fast client must finish while the first client remains stalled");
    assert!(fast_response.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert_eq!(body(&fast_response), b"Ok.\n");
    assert_eq!(
        fast_response
            .windows(b"HTTP/1.1".len())
            .filter(|window| *window == b"HTTP/1.1")
            .count(),
        1
    );
    fast_client.join().expect("fast client did not panic");

    stalled
        .write_all(b"\r\n\r\n")
        .expect("complete stalled request");
    finish_request_stream(&stalled);
    let mut stalled_response = Vec::new();
    stalled
        .read_to_end(&mut stalled_response)
        .expect("read completed stalled response");
    assert_eq!(body(&stalled_response), b"Ok.\n");

    let report = worker.join().unwrap().unwrap();
    assert_eq!(report.accepted_connections, 2);
    assert_eq!(report.successful_exchanges, 2);
    assert!(report.connection_failures.is_empty());
}

#[test]
fn capped_authenticated_concurrency_with_cap_one_waits_before_accepting_the_next_client() {
    let limits = HttpListenerLimits {
        read_timeout: IO_TIMEOUT,
        write_timeout: IO_TIMEOUT,
        ..HttpListenerLimits::new(2, HttpQueryLimits::default())
    };
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let address = listener.local_addr().expect("read loopback address");
    let worker = thread::spawn(move || {
        serve_http_read_only_concurrently_with_clickhouse_key_and_limits(
            &listener,
            &SharedDatabase::default(),
            "correct-key",
            limits,
            NonZeroUsize::new(1).unwrap(),
        )
    });

    let mut stalled = TcpStream::connect(address).expect("connect stalled client");
    stalled
        .set_read_timeout(Some(IO_TIMEOUT))
        .expect("set stalled client read timeout");
    stalled
        .set_write_timeout(Some(IO_TIMEOUT))
        .expect("set stalled client write timeout");
    stalled
        .write_all(b"GET /ping HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: correct-key")
        .expect("write incomplete authenticated request");

    let (fast_response, fast_client) = start_exchange(
        address,
        b"GET /ping HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: correct-key\r\n\r\n".to_vec(),
    );
    assert!(matches!(
        fast_response.recv_timeout(Duration::from_millis(200)),
        Err(RecvTimeoutError::Timeout)
    ));

    stalled
        .write_all(b"\r\n\r\n")
        .expect("complete stalled request");
    finish_request_stream(&stalled);
    let mut stalled_response = Vec::new();
    stalled
        .read_to_end(&mut stalled_response)
        .expect("read completed stalled response");
    assert_eq!(body(&stalled_response), b"Ok.\n");

    let fast_response = fast_response
        .recv_timeout(IO_TIMEOUT)
        .expect("fast client proceeds after cap-one capacity is released");
    assert_eq!(body(&fast_response), b"Ok.\n");
    fast_client.join().expect("fast client did not panic");

    let report = worker.join().unwrap().unwrap();
    assert_eq!(report.accepted_connections, 2);
    assert_eq!(report.successful_exchanges, 2);
    assert!(report.connection_failures.is_empty());
}

#[test]
fn concurrent_transport_failures_are_isolated_and_reported_in_acceptance_order() {
    let database = SharedDatabase::default();
    let ping_request =
        b"GET /ping HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: correct-key\r\n\r\n";
    let mut expected_ping = Vec::new();
    handle_http_query_read_only_with_clickhouse_key(
        &database,
        "correct-key",
        Cursor::new(ping_request),
        &mut expected_ping,
    )
    .unwrap();
    let limits = HttpListenerLimits {
        read_timeout: STALLED_CONNECTION_TIMEOUT,
        write_timeout: IO_TIMEOUT,
        query_limits: HttpQueryLimits {
            max_response_bytes: expected_ping.len(),
            ..HttpQueryLimits::default()
        },
        ..HttpListenerLimits::new(3, HttpQueryLimits::default())
    };
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let address = listener.local_addr().expect("read loopback address");
    let worker = thread::spawn(move || {
        serve_http_read_only_concurrently_with_clickhouse_key_and_limits(
            &listener,
            &database,
            "correct-key",
            limits,
            NonZeroUsize::new(2).unwrap(),
        )
    });

    let mut stalled = TcpStream::connect(address).expect("connect stalled client");
    stalled
        .set_write_timeout(Some(IO_TIMEOUT))
        .expect("set stalled client write timeout");
    stalled
        .write_all(b"GET /ping HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: correct-key")
        .expect("write incomplete authenticated request");

    let oversized = post_query_with_clickhouse_key(
        &format!("SELECT '{}' AS value;", "x".repeat(1_000)),
        "correct-key",
    );
    assert!(exchange(address, &oversized).is_empty());
    assert_eq!(exchange(address, ping_request), expected_ping);

    let report = worker.join().unwrap().unwrap();
    assert_eq!(report.accepted_connections, 3);
    assert_eq!(report.successful_exchanges, 1);
    assert_eq!(report.connection_failures.len(), 2);
    assert_eq!(report.connection_failures[0].connection, 1);
    assert!(matches!(
        &report.connection_failures[0].source,
        HttpQueryError::Read(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            )
    ));
    assert_eq!(report.connection_failures[1].connection, 2);
    assert!(matches!(
        report.connection_failures[1].source,
        HttpQueryError::ResponseLimitExceeded { max_bytes, .. }
            if max_bytes == expected_ping.len()
    ));
}

#[test]
fn sequential_connections_observe_updated_cached_system_metrics() {
    let database = SharedDatabase::default();
    let (address, worker) = start_listener(
        database.clone(),
        HttpListenerLimits::new(2, HttpQueryLimits::default()),
    );
    let request = post_query("SELECT metric, value FROM system.metrics;");

    let empty = exchange(address, &request);
    assert!(empty.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert_eq!(
        body(&empty),
        br#"{"columns":[{"name":"metric","type":"String"},{"name":"value","type":"Int64"}],"rows":[["rusthouse_tables",0],["rusthouse_columns",0],["rusthouse_retained_rows",0],["rusthouse_retained_value_bytes",0],["rusthouse_index_scanned_blocks",0],["rusthouse_index_pruned_blocks",0]]}"#
    );

    database
        .execute(
            "CREATE TABLE readings (value Int64, label String); \
             INSERT INTO readings VALUES (7, 'x'), (11, 'é');",
        )
        .unwrap();

    let populated = exchange(address, &request);
    assert!(populated.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert_eq!(
        body(&populated),
        br#"{"columns":[{"name":"metric","type":"String"},{"name":"value","type":"Int64"}],"rows":[["rusthouse_tables",1],["rusthouse_columns",2],["rusthouse_retained_rows",2],["rusthouse_retained_value_bytes",19],["rusthouse_index_scanned_blocks",0],["rusthouse_index_pruned_blocks",0]]}"#
    );

    let report = worker.join().unwrap().unwrap();
    assert_eq!(report.accepted_connections, 2);
    assert_eq!(report.successful_exchanges, 2);
    assert!(report.connection_failures.is_empty());
}

#[test]
fn one_connection_receives_only_one_response_even_when_requests_are_pipelined() {
    let database = SharedDatabase::default();
    let (address, worker) = start_listener(
        database,
        HttpListenerLimits::new(1, HttpQueryLimits::default()),
    );
    let requests = b"GET /ping HTTP/1.1\r\nHost: localhost\r\n\r\nGET /ping HTTP/1.1\r\nHost: localhost\r\n\r\n";

    let response = exchange(address, requests);
    assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert_eq!(body(&response), b"Ok.\n");
    assert_eq!(
        response
            .windows(b"HTTP/1.1".len())
            .filter(|window| *window == b"HTTP/1.1")
            .count(),
        1
    );

    let report = worker.join().unwrap().unwrap();
    assert_eq!(report.accepted_connections, 1);
    assert_eq!(report.successful_exchanges, 1);
}

#[test]
fn malformed_client_is_closed_without_preventing_the_next_request() {
    let database = SharedDatabase::default();
    let (address, worker) = start_listener(
        database,
        HttpListenerLimits::new(2, HttpQueryLimits::default()),
    );

    let malformed = exchange(address, b"GET /ping HTTP/1.1\nHost: localhost\n\n");
    assert!(malformed.starts_with(b"HTTP/1.1 400 Bad Request\r\n"));

    let healthy = exchange(address, b"GET /ping HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(healthy.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert_eq!(body(&healthy), b"Ok.\n");

    let report = worker.join().unwrap().unwrap();
    assert_eq!(report.accepted_connections, 2);
    assert_eq!(report.successful_exchanges, 2);
    assert!(report.connection_failures.is_empty());
}

#[test]
fn stalled_request_times_out_without_preventing_the_next_connection() {
    let limits = HttpListenerLimits {
        read_timeout: STALLED_CONNECTION_TIMEOUT,
        write_timeout: IO_TIMEOUT,
        ..HttpListenerLimits::new(2, HttpQueryLimits::default())
    };
    let (address, worker) = start_listener(SharedDatabase::default(), limits);

    let mut stalled = TcpStream::connect(address).expect("connect stalled client");
    stalled
        .set_write_timeout(Some(IO_TIMEOUT))
        .expect("set stalled client write timeout");
    stalled
        .write_all(b"GET /ping HTTP/1.1\r\nHost: localhost")
        .expect("write incomplete request without closing it");

    let healthy = exchange(address, b"GET /ping HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(healthy.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert_eq!(body(&healthy), b"Ok.\n");

    let report = worker.join().unwrap().unwrap();
    assert_eq!(report.accepted_connections, 2);
    assert_eq!(report.successful_exchanges, 1);
    assert_eq!(report.connection_failures.len(), 1);
    assert_eq!(report.connection_failures[0].connection, 1);
    assert!(matches!(
        &report.connection_failures[0].source,
        HttpQueryError::Read(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            )
    ));
}

#[test]
fn trickling_request_cannot_renew_the_absolute_read_deadline() {
    let limits = HttpListenerLimits {
        read_timeout: STALLED_CONNECTION_TIMEOUT,
        write_timeout: IO_TIMEOUT,
        ..HttpListenerLimits::new(2, HttpQueryLimits::default())
    };
    let (address, worker) = start_listener(SharedDatabase::default(), limits);

    let mut trickling = TcpStream::connect(address).expect("connect trickling client");
    trickling
        .set_write_timeout(Some(IO_TIMEOUT))
        .expect("set trickling client write timeout");
    trickling
        .write_all(b"GET /ping HTTP/1.1\r\nHost: localhost\r\nX-Slow: ")
        .expect("write incomplete request prefix");
    let trickler = thread::spawn(move || {
        for _ in 0..TRICKLE_BYTES {
            thread::sleep(TRICKLE_INTERVAL);
            if trickling.write_all(b"a").is_err() {
                return;
            }
        }
        let _ = trickling.shutdown(Shutdown::Write);
    });

    let healthy = exchange(address, b"GET /ping HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(healthy.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert_eq!(body(&healthy), b"Ok.\n");

    let report = worker.join().unwrap().unwrap();
    trickler.join().expect("trickling client did not panic");
    assert_eq!(report.accepted_connections, 2);
    assert_eq!(report.successful_exchanges, 1);
    assert_eq!(report.connection_failures.len(), 1);
    assert_eq!(report.connection_failures[0].connection, 1);
    assert!(matches!(
        &report.connection_failures[0].source,
        HttpQueryError::Read(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            )
    ));
}

#[test]
fn stalled_response_times_out_without_preventing_the_next_connection() {
    let database = SharedDatabase::default();
    database
        .execute(&format!(
            "CREATE TABLE payloads (value String); INSERT INTO payloads VALUES ('{}');",
            "x".repeat(STALLED_RESPONSE_BYTES)
        ))
        .expect("create a response larger than the loopback socket buffers");
    let limits = HttpListenerLimits {
        read_timeout: IO_TIMEOUT,
        write_timeout: STALLED_CONNECTION_TIMEOUT,
        ..HttpListenerLimits::new(2, HttpQueryLimits::default())
    };
    let (address, worker) = start_listener(database, limits);

    let mut stalled = TcpStream::connect(address).expect("connect non-reading client");
    stalled
        .set_write_timeout(Some(IO_TIMEOUT))
        .expect("set non-reading client write timeout");
    stalled
        .write_all(&post_query("SELECT value FROM payloads;"))
        .expect("write request for large response");
    finish_request_stream(&stalled);

    let healthy = exchange(address, b"GET /ping HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(healthy.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert_eq!(body(&healthy), b"Ok.\n");

    let report = worker.join().unwrap().unwrap();
    assert_eq!(report.accepted_connections, 2);
    assert_eq!(report.successful_exchanges, 1);
    assert_eq!(report.connection_failures.len(), 1);
    assert_eq!(report.connection_failures[0].connection, 1);
    assert!(matches!(
        &report.connection_failures[0].source,
        HttpQueryError::Write(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            )
    ));
}

#[test]
fn connection_local_limit_failure_is_typed_and_the_listener_continues() {
    let database = SharedDatabase::default();
    let ping_request = b"GET /ping HTTP/1.1\r\nHost: localhost\r\n\r\n";
    let mut expected_ping = Vec::new();
    handle_http_query(&database, Cursor::new(ping_request), &mut expected_ping).unwrap();
    let limits = HttpQueryLimits {
        max_response_bytes: expected_ping.len(),
        ..HttpQueryLimits::default()
    };
    let (address, worker) = start_listener(database, HttpListenerLimits::new(2, limits));

    let oversized = post_query(&format!("SELECT '{}' AS value;", "x".repeat(1_000)));
    assert!(exchange(address, &oversized).is_empty());
    assert_eq!(exchange(address, ping_request), expected_ping);

    let report = worker.join().unwrap().unwrap();
    assert_eq!(report.accepted_connections, 2);
    assert_eq!(report.successful_exchanges, 1);
    assert_eq!(report.connection_failures.len(), 1);
    assert_eq!(report.connection_failures[0].connection, 1);
    assert!(matches!(
        report.connection_failures[0].source,
        HttpQueryError::ResponseLimitExceeded { max_bytes, .. }
            if max_bytes == expected_ping.len()
    ));
}

#[test]
fn zero_connection_limit_returns_without_accepting() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let report = serve_http_read_only(&listener, &SharedDatabase::default(), 0).unwrap();

    assert_eq!(report.accepted_connections, 0);
    assert_eq!(report.successful_exchanges, 0);
    assert!(report.connection_failures.is_empty());
}

#[test]
fn listener_accept_failures_return_a_typed_error_and_partial_report() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();

    let error = serve_http_read_only(&listener, &SharedDatabase::default(), 1)
        .expect_err("a nonblocking listener with no client cannot accept");
    match &error {
        HttpListenerError::Accept { report, source } => {
            assert_eq!(source.kind(), std::io::ErrorKind::WouldBlock);
            assert_eq!(report.accepted_connections, 0);
            assert_eq!(report.successful_exchanges, 0);
            assert!(report.connection_failures.is_empty());
        }
    }
    assert!(error.source().is_some());
}
