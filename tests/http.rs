use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use rusthouse::server::MAX_REQUEST_BODY_BYTES;

struct TestServer {
    address: SocketAddr,
    child: Option<Child>,
}

impl TestServer {
    fn start() -> Self {
        let reservation = TcpListener::bind("127.0.0.1:0").expect("reserve test port");
        let address = reservation.local_addr().expect("reserved address");
        drop(reservation);

        let child = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
            .args(["serve", "--listen", &address.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn HTTP server");
        let mut server = Self {
            address,
            child: Some(child),
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(response) = try_request(address, "GET", "/health", None, b"")
                && response.status == 200
            {
                return server;
            }
            if let Some(status) = server
                .child
                .as_mut()
                .expect("child present")
                .try_wait()
                .expect("poll server")
            {
                panic!("HTTP server exited during startup with {status}");
            }
            assert!(Instant::now() < deadline, "HTTP server did not start");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn request(&self, method: &str, path: &str, accept: Option<&str>, body: &[u8]) -> HttpResponse {
        try_request(self.address, method, path, accept, body).expect("HTTP request succeeds")
    }

    fn raw_request(&self, request: &[u8]) -> HttpResponse {
        let mut stream = TcpStream::connect(self.address).expect("connect to HTTP server");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set test timeout");
        stream.write_all(request).expect("write raw HTTP request");
        read_response(stream).expect("read raw HTTP response")
    }

    #[cfg(unix)]
    fn shutdown(mut self) -> std::process::ExitStatus {
        let mut child = self.child.take().expect("child present");
        let signal = Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .expect("send SIGTERM");
        assert!(signal.success(), "kill command failed");

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().expect("poll server shutdown") {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "HTTP server did not shut down after SIGTERM"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    headers: String,
    body: String,
}

fn try_request(
    address: SocketAddr,
    method: &str,
    path: &str,
    accept: Option<&str>,
    body: &[u8],
) -> std::io::Result<HttpResponse> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(200))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    )?;
    if method == "POST" {
        stream.write_all(b"Content-Type: application/sql\r\n")?;
    }
    if let Some(accept) = accept {
        write!(stream, "Accept: {accept}\r\n")?;
    }
    stream.write_all(b"\r\n")?;
    stream.write_all(body)?;
    read_response(stream)
}

fn read_response(mut stream: TcpStream) -> std::io::Result<HttpResponse> {
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes)?;
    let response = String::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let (headers, body) = response.split_once("\r\n\r\n").ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "missing response headers")
    })?;
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse().ok())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid status"))?;
    Ok(HttpResponse {
        status,
        headers: headers.to_owned(),
        body: body.to_owned(),
    })
}

#[test]
fn state_crosses_requests_and_output_is_negotiated() {
    let server = TestServer::start();
    let response = server.request(
        "POST",
        "/query",
        Some("application/json"),
        b"CREATE TABLE events (id Int64, label String); \
          INSERT INTO events VALUES (2, 'second'), (1, 'first');",
    );
    assert_eq!(response.status, 200);
    assert_eq!(response.body, "{\"results\":[]}\n");

    let csv = server.request(
        "POST",
        "/query",
        Some("text/csv"),
        b"SELECT id, label FROM events ORDER BY id",
    );
    assert_eq!(csv.status, 200);
    assert!(csv.headers.contains("Content-Type: text/csv"));
    assert_eq!(csv.body, "id,label\n1,first\n2,second\n");

    let json = server.request(
        "POST",
        "/query",
        Some("text/csv;q=0.2, application/json;q=0.8"),
        b"SELECT COUNT(*) AS count FROM events",
    );
    assert_eq!(json.status, 200);
    assert!(json.headers.contains("Content-Type: application/json"));
    assert!(json.body.contains("\"rows\":[[2]]"));
}

#[test]
fn concurrent_readers_return_consistent_results() {
    let server = TestServer::start();
    let values = (0..2_000)
        .map(|value| format!("({value})"))
        .collect::<Vec<_>>()
        .join(",");
    let setup = format!("CREATE TABLE numbers (n Int64); INSERT INTO numbers VALUES {values}");
    assert_eq!(
        server
            .request("POST", "/query", None, setup.as_bytes())
            .status,
        200
    );

    let address = server.address;
    let barrier = Arc::new(Barrier::new(17));
    let readers = (0..16)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                try_request(
                    address,
                    "POST",
                    "/query",
                    Some("application/json"),
                    b"SELECT COUNT(*) AS count, SUM(n) AS total FROM numbers",
                )
                .expect("concurrent read")
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    for reader in readers {
        let response = reader.join().expect("reader thread");
        assert_eq!(response.status, 200);
        assert!(response.body.contains("\"rows\":[[2000,1999000]]"));
    }
}

#[test]
fn concurrent_mutations_are_serialized_without_lost_rows() {
    let server = TestServer::start();
    assert_eq!(
        server
            .request("POST", "/query", None, b"CREATE TABLE writes (id Int64)",)
            .status,
        200
    );

    let address = server.address;
    let barrier = Arc::new(Barrier::new(33));
    let writers = (0..32)
        .map(|value| {
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let sql = format!("INSERT INTO writes VALUES ({value})");
                barrier.wait();
                try_request(address, "POST", "/query", None, sql.as_bytes())
                    .expect("concurrent mutation")
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    for writer in writers {
        assert_eq!(writer.join().expect("writer thread").status, 200);
    }

    let result = server.request(
        "POST",
        "/query",
        None,
        b"SELECT COUNT(*) AS count, SUM(id) AS total FROM writes",
    );
    assert_eq!(result.status, 200);
    assert!(result.body.contains("\"rows\":[[32,496]]"));
}

#[test]
fn malformed_and_oversized_requests_are_bounded_errors() {
    let server = TestServer::start();

    let malformed_http = server.raw_request(b"POST /query HTTP/1.1\r\nBroken\r\n\r\n");
    assert_eq!(malformed_http.status, 400);
    assert!(malformed_http.body.contains("malformed request header"));

    let malformed_sql = server.request("POST", "/query", None, b"SELECT FROM");
    assert_eq!(malformed_sql.status, 400);
    assert!(malformed_sql.body.contains("SQL error"));

    let unacceptable = server.request("POST", "/query", Some("text/html"), b"SELECT 1");
    assert_eq!(unacceptable.status, 406);

    let dense_tokens = ";".repeat(70_000);
    let dense = server.request("POST", "/query", None, dense_tokens.as_bytes());
    assert_eq!(dense.status, 422);
    assert!(dense.body.contains("more than 65536 tokens"));

    let declared_size = MAX_REQUEST_BODY_BYTES + 1;
    let oversized = server.raw_request(
        format!(
            "POST /query HTTP/1.1\r\nHost: localhost\r\nContent-Length: {declared_size}\r\n\r\n"
        )
        .as_bytes(),
    );
    assert_eq!(oversized.status, 413);
    assert!(oversized.body.contains("1048576 bytes"));

    assert_eq!(server.request("GET", "/health", None, b"").status, 200);
}

#[test]
fn fragmented_requests_and_large_responses_complete() {
    let server = TestServer::start();
    let sql = b"CREATE TABLE fragments (value String)";
    let headers = format!(
        "POST /query HTTP/1.1\r\nHost: {}\r\nContent-Type: application/sql\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        server.address,
        sql.len()
    );
    let mut stream = TcpStream::connect(server.address).expect("connect fragmented request");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set fragmented request timeout");
    stream
        .write_all([headers.as_bytes(), &sql[..12]].concat().as_slice())
        .expect("write first request fragment");
    thread::sleep(Duration::from_millis(200));
    stream
        .write_all(&sql[12..])
        .expect("write second request fragment");
    let response = read_response(stream).expect("read fragmented request response");
    assert_eq!(response.status, 200);

    let value = "x".repeat(512 * 1024);
    let insert = format!("INSERT INTO fragments VALUES ('{value}')");
    assert_eq!(
        server
            .request("POST", "/query", None, insert.as_bytes())
            .status,
        200
    );
    let response = server.request(
        "POST",
        "/query",
        Some("application/json"),
        b"SELECT value FROM fragments",
    );
    assert_eq!(response.status, 200);
    assert!(response.body.contains(&value));
    let declared_length = response
        .headers
        .lines()
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .and_then(|value| value.parse::<usize>().ok())
        .expect("response content length");
    assert_eq!(declared_length, response.body.len());
}

#[test]
fn execution_and_aggregate_results_are_limited() {
    let server = TestServer::start();
    let too_many_statements = vec!["SELECT value FROM blobs"; 33].join(";");
    let response = server.request("POST", "/query", None, too_many_statements.as_bytes());
    assert_eq!(response.status, 422);
    assert!(response.body.contains("more than 32 statements"));

    let value = "z".repeat(600 * 1024);
    let setup = format!("CREATE TABLE blobs (value String); INSERT INTO blobs VALUES ('{value}')");
    assert_eq!(
        server
            .request("POST", "/query", None, setup.as_bytes())
            .status,
        200
    );
    let amplified = ["SELECT value FROM blobs"; 4].join(";");
    let response = server.request("POST", "/query", None, amplified.as_bytes());
    assert_eq!(response.status, 422);
    assert!(response.body.contains("resource limit exceeded"));

    drop(server);
    let server = TestServer::start();
    let values = std::iter::repeat_n("(1)", 10_001)
        .collect::<Vec<_>>()
        .join(",");
    let setup = format!("CREATE TABLE rows (n Int64); INSERT INTO rows VALUES {values}");
    assert_eq!(
        server
            .request("POST", "/query", None, setup.as_bytes())
            .status,
        200
    );
    let response = server.request("POST", "/query", None, b"SELECT n FROM rows");
    assert_eq!(response.status, 422);
    assert!(response.body.contains("more than 10000 result rows"));

    let more_values = std::iter::repeat_n("(2)", 9_999)
        .collect::<Vec<_>>()
        .join(",");
    let insert = format!("INSERT INTO rows VALUES {more_values}");
    for _ in 0..9 {
        assert_eq!(
            server
                .request("POST", "/query", None, insert.as_bytes())
                .status,
            200
        );
    }
    assert_eq!(
        server
            .request(
                "POST",
                "/query",
                None,
                b"INSERT INTO rows VALUES (2),(2),(2),(2),(2),(2),(2),(2)",
            )
            .status,
        200
    );
    let rejected = server.request("POST", "/query", None, b"INSERT INTO rows VALUES (3)");
    assert_eq!(rejected.status, 422);
    assert!(rejected.body.contains("retains more than 100000 rows"));

    let response = server.request(
        "POST",
        "/query",
        None,
        b"SELECT COUNT(*) AS count FROM rows",
    );
    assert_eq!(response.status, 200);
    assert!(response.body.contains("\"rows\":[[100000]]"));
    assert_eq!(server.request("GET", "/health", None, b"").status, 200);
}

#[test]
fn query_endpoint_rejects_browser_simple_requests() {
    let server = TestServer::start();
    let body = b"CREATE TABLE forbidden (id Int64)";
    let missing_type = server.raw_request(
        format!(
            "POST /query HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            String::from_utf8_lossy(body)
        )
        .as_bytes(),
    );
    assert_eq!(missing_type.status, 415);

    let origin = server.raw_request(
        format!(
            "POST /query HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/sql\r\nOrigin: https://example.com\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            String::from_utf8_lossy(body)
        )
        .as_bytes(),
    );
    assert_eq!(origin.status, 403);
    assert!(origin.body.contains("browser-originated"));
}

#[test]
fn non_loopback_listener_is_rejected() {
    let output = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args(["serve", "--listen", "0.0.0.0:0"])
        .output()
        .expect("run server with wildcard address");
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .expect("UTF-8 stderr")
            .contains("only permits loopback listen addresses")
    );
}

#[cfg(unix)]
#[test]
fn sigterm_gracefully_stops_the_server() {
    let server = TestServer::start();
    assert_eq!(server.request("GET", "/health", None, b"").status, 200);
    let status = server.shutdown();
    assert!(status.success(), "server exited with {status}");
}
