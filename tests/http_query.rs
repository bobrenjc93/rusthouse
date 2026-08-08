use std::io::{self, Cursor, Read, Write};
use std::sync::{Arc, RwLock, mpsc};
use std::thread;
use std::time::Duration;

use rusthouse::batch::csv::CsvIngestLimits;
use rusthouse::batch::engine::{Database, QueryResultLimits};
use rusthouse::batch::tsv::TsvIngestLimits;
use rusthouse::{
    HttpQueryError, HttpQueryLimits, SharedDatabase, handle_http_query,
    handle_http_query_with_bearer_token, handle_http_query_with_bearer_token_and_limits,
    handle_http_query_with_clickhouse_key, handle_http_query_with_clickhouse_key_and_limits,
    handle_http_query_with_limits,
};

fn request(sql: &[u8]) -> Vec<u8> {
    request_for_target("/query", sql)
}

fn request_for_target(target: &str, sql: &[u8]) -> Vec<u8> {
    request_for_target_with_headers(target, sql, "")
}

fn request_for_target_with_headers(target: &str, sql: &[u8], headers: &str) -> Vec<u8> {
    let mut request = format!(
        "POST {target} HTTP/1.1\r\nHost: localhost\r\n{headers}Content-Length: {}\r\n\r\n",
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
    request_with_authorization_for_target("/query", sql, authorization_headers)
}

fn request_with_authorization_for_target(
    target: &str,
    sql: &[u8],
    authorization_headers: &str,
) -> (Vec<u8>, u64) {
    let mut request = format!(
        "POST {target} HTTP/1.1\r\nHost: localhost\r\n{authorization_headers}Content-Length: {}\r\n\r\n",
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

fn clickhouse_key_exchange(database: &SharedDatabase, key: &str, request: &[u8]) -> Vec<u8> {
    let mut response = Vec::new();
    handle_http_query_with_clickhouse_key(database, key, Cursor::new(request), &mut response)
        .expect("ClickHouse-key-authenticated exchange succeeds");
    response
}

fn assert_response(response: &[u8], status: &str, expected_body: &str) {
    assert_response_with_content_type(
        response,
        status,
        "application/json",
        expected_body.as_bytes(),
    );
}

fn assert_response_with_content_type(
    response: &[u8],
    status: &str,
    content_type: &str,
    expected_body: &[u8],
) {
    let separator = b"\r\n\r\n";
    let split = response
        .windows(separator.len())
        .position(|window| window == separator)
        .expect("response has an empty header line");
    let headers = std::str::from_utf8(&response[..split]).expect("headers are UTF-8");
    let body = &response[split + separator.len()..];

    assert_eq!(headers.lines().next(), Some(status));
    assert!(headers.contains(&format!("\r\nContent-Type: {content_type}\r\n")));
    assert!(headers.contains("\r\nConnection: close"));
    assert!(headers.contains(&format!("\r\nContent-Length: {}\r\n", body.len())));
    assert_eq!(body, expected_body);
}

fn assert_response_header(response: &[u8], expected_header: &str) {
    let response = std::str::from_utf8(response).expect("response is UTF-8");
    let (headers, _) = response
        .split_once("\r\n\r\n")
        .expect("response has an empty header line");
    assert_eq!(
        headers
            .lines()
            .filter(|line| *line == expected_header)
            .count(),
        1,
        "expected exactly one {expected_header:?} response header"
    );
}

fn assert_clickhouse_key_response_is_not_cacheable(response: &[u8]) {
    assert_response_header(response, "Cache-Control: private, no-store");
}

fn assert_ok_health_response(response: &[u8]) {
    assert_response_with_content_type(
        response,
        "HTTP/1.1 200 OK",
        "text/plain; charset=utf-8",
        b"Ok.\n",
    );
}

fn metrics_body(
    tables: usize,
    columns: usize,
    retained_rows: usize,
    retained_value_bytes: usize,
) -> String {
    format!(
        "# HELP rusthouse_tables Number of tables retained by the database.\n\
         # TYPE rusthouse_tables gauge\n\
         rusthouse_tables {tables}\n\
         # HELP rusthouse_columns Number of columns retained by the database.\n\
         # TYPE rusthouse_columns gauge\n\
         rusthouse_columns {columns}\n\
         # HELP rusthouse_retained_rows Number of rows retained across all tables.\n\
         # TYPE rusthouse_retained_rows gauge\n\
         rusthouse_retained_rows {retained_rows}\n\
         # HELP rusthouse_retained_value_bytes Scalar payload bytes retained across all tables.\n\
         # TYPE rusthouse_retained_value_bytes gauge\n\
         rusthouse_retained_value_bytes {retained_value_bytes}\n"
    )
}

fn assert_ok_metrics_response(
    response: &[u8],
    tables: usize,
    columns: usize,
    retained_rows: usize,
    retained_value_bytes: usize,
) {
    assert_response_with_content_type(
        response,
        "HTTP/1.1 200 OK",
        "text/plain; version=0.0.4; charset=utf-8",
        metrics_body(tables, columns, retained_rows, retained_value_bytes).as_bytes(),
    );
}

#[test]
fn query_reports_the_configured_scan_limit_over_http() {
    let database = SharedDatabase::with_query_result_limits(QueryResultLimits {
        max_scan_rows: 2,
        ..QueryResultLimits::default()
    });
    database
        .execute(
            "CREATE TABLE values_table (value Int64); \
             INSERT INTO values_table VALUES (1), (2), (3);",
        )
        .unwrap();

    assert_response(
        &exchange(
            &database,
            &request(b"SELECT value FROM values_table WHERE value = 3 LIMIT 1;"),
        ),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"SELECT scanned rows requires at least 3, exceeding the limit of 2"}"#,
    );
}

#[test]
fn query_executes_unicode_contains_like_over_http() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE events (id Int64, label String); \
             INSERT INTO events VALUES (1, 'fresh snow 雪'), (2, 'snowman'), (3, 'Snow');",
        )
        .expect("setup");

    assert_response(
        &exchange(
            &database,
            &request(b"SELECT label FROM events WHERE label LIKE '%snow%' ORDER BY label;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"label","type":"String"}],"rows":[["fresh snow 雪"],["snowman"]]}"#,
    );
}

#[test]
fn query_executes_unicode_suffix_like_over_http() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE events (id Int64, label String); \
             INSERT INTO events VALUES (1, '東京'), (2, '西東京'), (3, '東京駅'), (4, 'Tokyo');",
        )
        .expect("setup");

    assert_response(
        &exchange(
            &database,
            &request(
                "SELECT label FROM events WHERE label LIKE '%東京' ORDER BY label;".as_bytes(),
            ),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"label","type":"String"}],"rows":[["東京"],["西東京"]]}"#,
    );
}

#[test]
fn query_executes_inclusive_between_over_http() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE readings (id Int64, score Float64); \
             INSERT INTO readings VALUES (1, 1.0), (2, 2.5), (3, 4.0), (4, 5.0);",
        )
        .expect("setup");

    assert_response(
        &exchange(
            &database,
            &request(b"SELECT id FROM readings WHERE score BETWEEN 2.5 AND 4 ORDER BY id;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"}],"rows":[[2],[3]]}"#,
    );
}

#[test]
fn query_executes_not_between_for_distinct_where_over_http() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE readings (id Int64, score Float64); \
             INSERT INTO readings VALUES \
             (1, 1.0), (2, 2.5), (3, 4.0), (4, 5.0), (4, 5.0);",
        )
        .expect("setup");

    assert_response(
        &exchange(
            &database,
            &request(
                b"SELECT DISTINCT id FROM readings \
                  WHERE score NOT BETWEEN 2.5 AND 4 ORDER BY id;",
            ),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"}],"rows":[[1],[4]]}"#,
    );
}

#[test]
fn query_pages_ordered_distinct_typed_in_results_over_http() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE readings (id Int64, label String); \
             INSERT INTO readings VALUES \
             (1, 'cold'), (2, 'warm'), (3, 'hot'), (4, 'warm');",
        )
        .expect("setup");

    assert_response(
        &exchange(
            &database,
            &request(
                b"SELECT DISTINCT label FROM readings WHERE id IN (2, 3, 4) \
                  ORDER BY label LIMIT 1 OFFSET 1;",
            ),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"label","type":"String"}],"rows":[["warm"]]}"#,
    );
}

#[test]
fn float64_to_bool_cast_is_visible_in_every_http_query_format() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE readings (value Float64); \
             INSERT INTO readings VALUES (-0.0), (-0.25), (0.25);",
        )
        .expect("setup");
    let sql = b"SELECT CAST(value AS Bool) AS enabled FROM readings ORDER BY enabled;";

    assert_response(
        &exchange(&database, &request(sql)),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"enabled","type":"Bool"}],"rows":[[false],[true],[true]]}"#,
    );

    let csv =
        request_for_target_with_headers("/query", sql, "X-ClickHouse-Format: CSVWithNames\r\n");
    assert_response_with_content_type(
        &exchange(&database, &csv),
        "HTTP/1.1 200 OK",
        "text/csv; charset=utf-8",
        b"enabled\nfalse\ntrue\ntrue\n",
    );

    let tsv = request_for_target_with_headers(
        "/query",
        sql,
        "X-ClickHouse-Format: TabSeparatedWithNames\r\n",
    );
    assert_response_with_content_type(
        &exchange(&database, &tsv),
        "HTTP/1.1 200 OK",
        "text/tab-separated-values; charset=utf-8",
        b"enabled\nfalse\ntrue\ntrue\n",
    );

    for (format, expected) in [
        (
            "JSONEachRow",
            "{\"enabled\":false}\n{\"enabled\":true}\n{\"enabled\":true}\n",
        ),
        ("JSONCompactEachRow", "[false]\n[true]\n[true]\n"),
    ] {
        let request = request_for_target_with_headers(
            "/query",
            sql,
            &format!("X-ClickHouse-Format: {format}\r\n"),
        );
        assert_response(&exchange(&database, &request), "HTTP/1.1 200 OK", expected);
    }
}

#[test]
fn bool_to_int64_cast_is_visible_in_every_http_query_format() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE flags (enabled Bool); \
             INSERT INTO flags VALUES (true), (false);",
        )
        .expect("setup");
    let sql = b"SELECT CAST(enabled AS Int64) AS enabled_i64 FROM flags ORDER BY enabled_i64;";

    assert_response(
        &exchange(&database, &request(sql)),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"enabled_i64","type":"Int64"}],"rows":[[0],[1]]}"#,
    );

    let csv =
        request_for_target_with_headers("/query", sql, "X-ClickHouse-Format: CSVWithNames\r\n");
    assert_response_with_content_type(
        &exchange(&database, &csv),
        "HTTP/1.1 200 OK",
        "text/csv; charset=utf-8",
        b"enabled_i64\n0\n1\n",
    );

    let tsv = request_for_target_with_headers(
        "/query",
        sql,
        "X-ClickHouse-Format: TabSeparatedWithNames\r\n",
    );
    assert_response_with_content_type(
        &exchange(&database, &tsv),
        "HTTP/1.1 200 OK",
        "text/tab-separated-values; charset=utf-8",
        b"enabled_i64\n0\n1\n",
    );

    for (format, expected) in [
        ("JSONEachRow", "{\"enabled_i64\":0}\n{\"enabled_i64\":1}\n"),
        ("JSONCompactEachRow", "[0]\n[1]\n"),
    ] {
        let request = request_for_target_with_headers(
            "/query",
            sql,
            &format!("X-ClickHouse-Format: {format}\r\n"),
        );
        assert_response(&exchange(&database, &request), "HTTP/1.1 200 OK", expected);
    }
}

#[test]
fn bool_to_string_cast_is_visible_in_every_http_query_format() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE flags (enabled Bool); \
             INSERT INTO flags VALUES (true), (false);",
        )
        .expect("setup");
    let sql = b"SELECT CAST(enabled AS String) AS text FROM flags ORDER BY text;";

    assert_response(
        &exchange(&database, &request(sql)),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"text","type":"String"}],"rows":[["false"],["true"]]}"#,
    );

    let csv =
        request_for_target_with_headers("/query", sql, "X-ClickHouse-Format: CSVWithNames\r\n");
    assert_response_with_content_type(
        &exchange(&database, &csv),
        "HTTP/1.1 200 OK",
        "text/csv; charset=utf-8",
        b"text\nfalse\ntrue\n",
    );

    let tsv = request_for_target_with_headers(
        "/query",
        sql,
        "X-ClickHouse-Format: TabSeparatedWithNames\r\n",
    );
    assert_response_with_content_type(
        &exchange(&database, &tsv),
        "HTTP/1.1 200 OK",
        "text/tab-separated-values; charset=utf-8",
        b"text\nfalse\ntrue\n",
    );

    for (format, expected) in [
        ("JSONEachRow", "{\"text\":\"false\"}\n{\"text\":\"true\"}\n"),
        ("JSONCompactEachRow", "[\"false\"]\n[\"true\"]\n"),
    ] {
        let request = request_for_target_with_headers(
            "/query",
            sql,
            &format!("X-ClickHouse-Format: {format}\r\n"),
        );
        assert_response(&exchange(&database, &request), "HTTP/1.1 200 OK", expected);
    }
}

#[test]
fn int64_to_string_cast_is_visible_in_every_http_query_format() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE readings (value Int64); \
             INSERT INTO readings VALUES (2), (-10), (0);",
        )
        .expect("setup");
    let sql = b"SELECT CAST(value AS String) AS text FROM readings ORDER BY text;";

    assert_response(
        &exchange(&database, &request(sql)),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"text","type":"String"}],"rows":[["-10"],["0"],["2"]]}"#,
    );

    let csv =
        request_for_target_with_headers("/query", sql, "X-ClickHouse-Format: CSVWithNames\r\n");
    assert_response_with_content_type(
        &exchange(&database, &csv),
        "HTTP/1.1 200 OK",
        "text/csv; charset=utf-8",
        b"text\n-10\n0\n2\n",
    );

    let tsv = request_for_target_with_headers(
        "/query",
        sql,
        "X-ClickHouse-Format: TabSeparatedWithNames\r\n",
    );
    assert_response_with_content_type(
        &exchange(&database, &tsv),
        "HTTP/1.1 200 OK",
        "text/tab-separated-values; charset=utf-8",
        b"text\n-10\n0\n2\n",
    );

    for (format, expected) in [
        (
            "JSONEachRow",
            "{\"text\":\"-10\"}\n{\"text\":\"0\"}\n{\"text\":\"2\"}\n",
        ),
        ("JSONCompactEachRow", "[\"-10\"]\n[\"0\"]\n[\"2\"]\n"),
    ] {
        let request = request_for_target_with_headers(
            "/query",
            sql,
            &format!("X-ClickHouse-Format: {format}\r\n"),
        );
        assert_response(&exchange(&database, &request), "HTTP/1.1 200 OK", expected);
    }
}

#[test]
fn float64_to_string_cast_is_visible_in_every_http_query_format() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE readings (value Float64); \
             INSERT INTO readings VALUES (10.0), (-0.0), (1.25);",
        )
        .expect("setup");
    let sql = b"SELECT CAST(value AS String) AS text FROM readings ORDER BY text;";

    assert_response(
        &exchange(&database, &request(sql)),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"text","type":"String"}],"rows":[["-0"],["1.25"],["10"]]}"#,
    );

    let csv =
        request_for_target_with_headers("/query", sql, "X-ClickHouse-Format: CSVWithNames\r\n");
    assert_response_with_content_type(
        &exchange(&database, &csv),
        "HTTP/1.1 200 OK",
        "text/csv; charset=utf-8",
        b"text\n-0\n1.25\n10\n",
    );

    let tsv = request_for_target_with_headers(
        "/query",
        sql,
        "X-ClickHouse-Format: TabSeparatedWithNames\r\n",
    );
    assert_response_with_content_type(
        &exchange(&database, &tsv),
        "HTTP/1.1 200 OK",
        "text/tab-separated-values; charset=utf-8",
        b"text\n-0\n1.25\n10\n",
    );

    for (format, expected) in [
        (
            "JSONEachRow",
            "{\"text\":\"-0\"}\n{\"text\":\"1.25\"}\n{\"text\":\"10\"}\n",
        ),
        ("JSONCompactEachRow", "[\"-0\"]\n[\"1.25\"]\n[\"10\"]\n"),
    ] {
        let request = request_for_target_with_headers(
            "/query",
            sql,
            &format!("X-ClickHouse-Format: {format}\r\n"),
        );
        assert_response(&exchange(&database, &request), "HTTP/1.1 200 OK", expected);
    }
}

#[test]
fn ping_returns_the_clickhouse_health_response_without_content_length() {
    let database = SharedDatabase::default();

    assert_ok_health_response(&exchange(
        &database,
        b"GET /ping HTTP/1.1\r\nHost: localhost\r\n\r\n",
    ));
    assert_ok_health_response(&exchange(
        &database,
        b"GET /ping HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
    ));
}

#[test]
fn ping_succeeds_when_the_database_lock_is_unavailable() {
    let inner = Arc::new(RwLock::new(Database::new()));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let poisoner = thread::spawn(move || {
        let _guard = inner.write().unwrap();
        panic!("poison the database lock");
    });
    assert!(poisoner.join().is_err());

    assert_response(
        &exchange(&database, &request(b"SHOW TABLES;")),
        "HTTP/1.1 500 Internal Server Error",
        r#"{"error":"database is unavailable"}"#,
    );
    assert_ok_health_response(&exchange(
        &database,
        b"GET /ping HTTP/1.1\r\nHost: localhost\r\n\r\n",
    ));
}

#[test]
fn ready_returns_ok_when_a_read_lock_is_immediately_available() {
    let database = SharedDatabase::default();

    assert_ok_health_response(&exchange(
        &database,
        b"GET /ready HTTP/1.1\r\nHost: localhost\r\n\r\n",
    ));
    assert_ok_health_response(&exchange(
        &database,
        b"GET /ready HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
    ));
}

#[test]
fn ready_returns_the_same_503_for_writer_contention_and_poisoning() {
    const READY_REQUEST: &[u8] = b"GET /ready HTTP/1.1\r\nHost: localhost\r\n\r\n";

    let contended_inner = Arc::new(RwLock::new(Database::new()));
    let contended_database = SharedDatabase::from_arc(Arc::clone(&contended_inner));
    let mut writer = Some(contended_inner.write().unwrap());
    let (sender, receiver) = mpsc::channel();
    let worker_database = contended_database.clone();
    let worker = thread::spawn(move || {
        sender
            .send(exchange(&worker_database, READY_REQUEST))
            .unwrap();
    });
    let contended_response = match receiver.recv_timeout(Duration::from_secs(2)) {
        Ok(response) => response,
        Err(error) => {
            drop(writer.take());
            worker.join().unwrap();
            panic!("readiness check blocked behind a writer: {error}");
        }
    };
    assert_response(
        &contended_response,
        "HTTP/1.1 503 Service Unavailable",
        r#"{"error":"database is unavailable"}"#,
    );
    drop(writer.take());
    worker.join().unwrap();

    let poisoned_inner = Arc::new(RwLock::new(Database::new()));
    let poisoned_database = SharedDatabase::from_arc(Arc::clone(&poisoned_inner));
    let poisoner = thread::spawn(move || {
        let _guard = poisoned_inner.write().unwrap();
        panic!("poison the database lock");
    });
    assert!(poisoner.join().is_err());

    let poisoned_response = exchange(&poisoned_database, READY_REQUEST);
    assert_eq!(poisoned_response, contended_response);
}

#[test]
fn ready_requires_the_exact_get_target_and_an_empty_body() {
    let database = SharedDatabase::default();

    assert_response(
        &exchange(
            &database,
            b"GET /ready HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1\r\n\r\nx",
        ),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"GET /ready does not accept a request body"}"#,
    );

    let wrong_method = exchange(
        &database,
        b"POST /ready HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
    );
    assert_response(
        &wrong_method,
        "HTTP/1.1 405 Method Not Allowed",
        r#"{"error":"method must be GET for /ready"}"#,
    );
    assert!(
        std::str::from_utf8(&wrong_method)
            .unwrap()
            .contains("\r\nAllow: GET\r\n")
    );

    assert_response(
        &exchange(
            &database,
            b"GET /ready?details HTTP/1.1\r\nHost: localhost\r\n\r\n",
        ),
        "HTTP/1.1 404 Not Found",
        r#"{"error":"request target must be /ping, /ready, or /metrics"}"#,
    );
}

#[test]
fn bearer_authentication_protects_ready() {
    let database = SharedDatabase::default();
    let request = b"GET /ready HTTP/1.1\r\nHost: localhost\r\n\r\n";

    assert_response(
        &authenticated_exchange(&database, "correct-token", request),
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"bearer authentication required"}"#,
    );
    assert_ok_health_response(&authenticated_exchange(
        &database,
        "correct-token",
        b"GET /ready HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer correct-token\r\n\r\n",
    ));
}

#[test]
fn metrics_reports_state_changes_as_prometheus_gauges() {
    let database = SharedDatabase::default();
    const REQUEST: &[u8] = b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n";

    assert_ok_metrics_response(&exchange(&database, REQUEST), 0, 0, 0, 0);
    database
        .execute(
            "CREATE TABLE events (id Int64, score Float64, active Bool, label String); \
             CREATE TABLE flags (active Bool); \
             INSERT INTO events VALUES (1, 1.5, true, 'one'), (2, 2.5, false, 'two'); \
             INSERT INTO flags VALUES (true);",
        )
        .unwrap();
    assert_ok_metrics_response(&exchange(&database, REQUEST), 2, 5, 3, 41);

    database
        .execute("DELETE FROM events WHERE id = 2;")
        .unwrap();
    assert_ok_metrics_response(&exchange(&database, REQUEST), 2, 5, 2, 21);

    database
        .execute("TRUNCATE TABLE events; DROP TABLE flags;")
        .unwrap();
    assert_ok_metrics_response(&exchange(&database, REQUEST), 1, 4, 0, 0);
}

#[test]
fn metrics_returns_the_same_503_for_writer_contention_and_poisoning() {
    const REQUEST: &[u8] = b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n";

    let contended_inner = Arc::new(RwLock::new(Database::new()));
    let contended_database = SharedDatabase::from_arc(Arc::clone(&contended_inner));
    let mut writer = Some(contended_inner.write().unwrap());
    let (sender, receiver) = mpsc::channel();
    let worker_database = contended_database.clone();
    let worker = thread::spawn(move || sender.send(exchange(&worker_database, REQUEST)).unwrap());
    let contended_response = match receiver.recv_timeout(Duration::from_secs(2)) {
        Ok(response) => response,
        Err(error) => {
            drop(writer.take());
            worker.join().unwrap();
            panic!("metrics snapshot blocked behind a writer: {error}");
        }
    };
    assert_response(
        &contended_response,
        "HTTP/1.1 503 Service Unavailable",
        r#"{"error":"database is unavailable"}"#,
    );
    drop(writer.take());
    worker.join().unwrap();

    let poisoned_inner = Arc::new(RwLock::new(Database::new()));
    let poisoned_database = SharedDatabase::from_arc(Arc::clone(&poisoned_inner));
    let poisoner = thread::spawn(move || {
        let _guard = poisoned_inner.write().unwrap();
        panic!("poison the database lock");
    });
    assert!(poisoner.join().is_err());
    assert_eq!(exchange(&poisoned_database, REQUEST), contended_response);
}

#[test]
fn metrics_requires_exact_get_without_a_body_and_retains_bearer_authentication() {
    let database = SharedDatabase::default();
    let wrong_method = exchange(
        &database,
        b"POST /metrics HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
    );
    assert_response(
        &wrong_method,
        "HTTP/1.1 405 Method Not Allowed",
        r#"{"error":"method must be GET for /metrics"}"#,
    );
    assert!(
        std::str::from_utf8(&wrong_method)
            .unwrap()
            .contains("\r\nAllow: GET\r\n")
    );
    assert_response(
        &exchange(
            &database,
            b"GET /metrics?details HTTP/1.1\r\nHost: localhost\r\n\r\n",
        ),
        "HTTP/1.1 404 Not Found",
        r#"{"error":"request target must be /ping, /ready, or /metrics"}"#,
    );
    assert_response(
        &exchange(
            &database,
            b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1\r\n\r\nx",
        ),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"GET /metrics does not accept a request body"}"#,
    );

    let unauthenticated = b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n";
    assert_response(
        &authenticated_exchange(&database, "correct-token", unauthenticated),
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"bearer authentication required"}"#,
    );
    assert_ok_metrics_response(
        &authenticated_exchange(
            &database,
            "correct-token",
            b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer correct-token\r\n\r\n",
        ),
        0,
        0,
        0,
        0,
    );
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
fn root_query_returns_the_existing_json_result_shape() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE readings (id Int64, label String); \
             INSERT INTO readings VALUES (2, 'two'), (1, 'one');",
        )
        .unwrap();

    assert_response(
        &exchange(
            &database,
            &request_for_target("/", b"SELECT id, label FROM readings ORDER BY id;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"},{"name":"label","type":"String"}],"rows":[[1,"one"],[2,"two"]]}"#,
    );
}

#[test]
fn every_query_route_returns_503_without_waiting_for_a_writer() {
    let inner = Arc::new(RwLock::new(Database::new()));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let mut writer = Some(inner.write().unwrap());
    let requests = [
        request_for_target("/", b"SHOW TABLES;"),
        request_for_target_with_headers(
            "/query",
            b"SHOW TABLES;",
            "X-ClickHouse-Format: CSVWithNames\r\n",
        ),
        b"GET /?query=SHOW+TABLES%3B HTTP/1.1\r\nHost: localhost\r\n\r\n".to_vec(),
    ];
    let (sender, receiver) = mpsc::channel();
    let worker_database = database.clone();
    let worker = thread::spawn(move || {
        let responses = requests
            .iter()
            .map(|request| exchange(&worker_database, request))
            .collect::<Vec<_>>();
        sender.send(responses).unwrap();
    });

    let responses = match receiver.recv_timeout(Duration::from_secs(2)) {
        Ok(responses) => responses,
        Err(error) => {
            drop(writer.take());
            worker.join().unwrap();
            panic!("HTTP query admission blocked behind a writer: {error}");
        }
    };
    assert_eq!(writer.as_ref().unwrap().catalog().table_count(), 0);
    for response in &responses {
        assert_response(
            response,
            "HTTP/1.1 503 Service Unavailable",
            r#"{"error":"database is unavailable"}"#,
        );
    }
    assert!(responses.windows(2).all(|pair| pair[0] == pair[1]));

    drop(writer.take());
    worker.join().unwrap();
}

#[test]
fn every_query_route_admits_a_concurrent_reader() {
    let mut initial = Database::new();
    initial
        .execute(
            "CREATE TABLE readings (value Int64); \
             INSERT INTO readings VALUES (11);",
        )
        .unwrap();
    let inner = Arc::new(RwLock::new(initial));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let mut existing_reader = Some(inner.read().unwrap());
    let requests = [
        request_for_target("/", b"SELECT value FROM readings;"),
        request_for_target("/query", b"SELECT value FROM readings;"),
        b"GET /?query=SELECT+value+FROM+readings%3B HTTP/1.1\r\nHost: localhost\r\n\r\n".to_vec(),
    ];
    let (sender, receiver) = mpsc::channel();
    let worker_database = database.clone();
    let worker = thread::spawn(move || {
        let responses = requests
            .iter()
            .map(|request| exchange(&worker_database, request))
            .collect::<Vec<_>>();
        sender.send(responses).unwrap();
    });

    let responses = match receiver.recv_timeout(Duration::from_secs(2)) {
        Ok(responses) => responses,
        Err(error) => {
            drop(existing_reader.take());
            worker.join().unwrap();
            panic!("HTTP query blocked behind an existing reader: {error}");
        }
    };
    assert_eq!(
        existing_reader
            .as_ref()
            .unwrap()
            .catalog()
            .table("readings")
            .unwrap()
            .row_count(),
        1
    );
    for response in responses {
        assert_response(
            &response,
            "HTTP/1.1 200 OK",
            r#"{"columns":[{"name":"value","type":"Int64"}],"rows":[[11]]}"#,
        );
    }

    drop(existing_reader.take());
    worker.join().unwrap();
}

#[test]
fn url_encoded_get_query_decodes_percent_escapes_and_plus_on_the_exact_wire() {
    let database = SharedDatabase::default();
    let request =
        b"GET /?query=SELECT+%27snow+%E9%9B%AA%27+AS+label%3B HTTP/1.1\r\nHost: localhost\r\n\r\n";

    assert_eq!(
        exchange(&database, request),
        concat!(
            "HTTP/1.1 200 OK\r\n",
            "Content-Type: application/json\r\n",
            "Content-Length: 68\r\n",
            "Connection: close\r\n",
            "\r\n",
            r#"{"columns":[{"name":"label","type":"String"}],"rows":[["snow 雪"]]}"#,
        )
        .as_bytes(),
    );
}

#[test]
fn url_encoded_get_query_consumes_exactly_one_wire_exchange() {
    let database = SharedDatabase::default();
    const QUERY: &[u8] = b"GET /?query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\n\r\n";
    let mut wire = QUERY.to_vec();
    wire.extend_from_slice(b"GET /ping HTTP/1.1\r\nHost: localhost\r\n\r\n");
    let mut input = Cursor::new(wire);
    let mut response = Vec::new();

    handle_http_query(&database, &mut input, &mut response).unwrap();

    assert_eq!(input.position(), QUERY.len() as u64);
    assert_response(
        &response,
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"1","type":"Int64"}],"rows":[[1]]}"#,
    );
}

#[test]
fn url_encoded_get_query_retains_bearer_authentication_and_format_negotiation() {
    let database = SharedDatabase::default();
    let unauthorized = b"GET /?query=SELECT+%2B7+AS+value%3B HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Format: CSVWithNames\r\n\r\n";
    assert_response(
        &authenticated_exchange(&database, "correct-token", unauthorized),
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"bearer authentication required"}"#,
    );

    let authorized = b"GET /?query=SELECT+%2B7+AS+value%3B HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer correct-token\r\nX-ClickHouse-Format: CSVWithNames\r\n\r\n";
    assert_response_with_content_type(
        &authenticated_exchange(&database, "correct-token", authorized),
        "HTTP/1.1 200 OK",
        "text/csv; charset=utf-8",
        b"value\n7\n",
    );
}

#[test]
fn url_encoded_get_query_rejects_malformed_encoding_utf8_parameters_and_bodies() {
    let database = SharedDatabase::default();
    let cases: &[(&[u8], &str)] = &[
        (
            b"GET /?query=SELECT+1% HTTP/1.1\r\nHost: localhost\r\n\r\n",
            r#"{"error":"query parameter contains malformed percent encoding"}"#,
        ),
        (
            b"GET /?query=SELECT+1%2 HTTP/1.1\r\nHost: localhost\r\n\r\n",
            r#"{"error":"query parameter contains malformed percent encoding"}"#,
        ),
        (
            b"GET /?query=SELECT+1%GG HTTP/1.1\r\nHost: localhost\r\n\r\n",
            r#"{"error":"query parameter contains malformed percent encoding"}"#,
        ),
        (
            b"GET /?query=SELECT+%FF HTTP/1.1\r\nHost: localhost\r\n\r\n",
            r#"{"error":"SQL query is not valid UTF-8"}"#,
        ),
        (
            b"GET /?query=SELECT+1%3B&format=CSV HTTP/1.1\r\nHost: localhost\r\n\r\n",
            r#"{"error":"GET query target must contain exactly one query parameter"}"#,
        ),
        (
            b"GET /?query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1\r\n\r\nx",
            r#"{"error":"GET /?query= does not accept a request body"}"#,
        ),
    ];

    for (request, body) in cases {
        assert_response(
            &exchange(&database, request),
            "HTTP/1.1 400 Bad Request",
            body,
        );
    }
}

#[test]
fn url_encoded_get_query_retains_the_complete_response_limit() {
    let database = SharedDatabase::default();
    let request = format!(
        "GET /?query=SELECT+%27{}%27+AS+value%3B HTTP/1.1\r\nHost: localhost\r\n\r\n",
        "x".repeat(1_000),
    );
    let limits = HttpQueryLimits {
        max_response_bytes: 512,
        ..HttpQueryLimits::default()
    };
    let mut response = Vec::new();

    handle_http_query_with_limits(
        &database,
        Cursor::new(request.as_bytes()),
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
}

#[test]
fn every_query_form_streams_typed_json_each_row() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE typed_values (id Int64, score Float64, active Bool, label String); \
             INSERT INTO typed_values VALUES (-7, 2.0, false, 'quote\"\\line\nsnow 雪'), \
                                               (0, -1.25, true, '');",
        )
        .unwrap();
    let sql = b"SELECT id, score, active, label FROM typed_values ORDER BY id;";
    let requests = [
        request_for_target_with_headers(
            "/",
            sql,
            "X-ClickHouse-Format: JSONEachRow\r\n",
        ),
        request_for_target_with_headers(
            "/query",
            sql,
            "X-ClickHouse-Format: JSONEachRow\r\n",
        ),
        b"GET /?query=SELECT+id%2C+score%2C+active%2C+label+FROM+typed_values+ORDER+BY+id%3B HTTP/1.1\r\nHost: localhost\r\nx-cLiCkHoUsE-fOrMaT:\tJSONEachRow \r\n\r\n".to_vec(),
    ];
    let expected = concat!(
        "{\"id\":-7,\"score\":2.0,\"active\":false,\"label\":\"quote\\\"\\\\line\\nsnow 雪\"}\n",
        "{\"id\":0,\"score\":-1.25,\"active\":true,\"label\":\"\"}\n",
    );

    for request in requests {
        assert_response(&exchange(&database, &request), "HTTP/1.1 200 OK", expected);
    }
}

#[test]
fn json_each_row_handles_escaped_keys_nulls_and_empty_results() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE empty_values (id Int64, score Float64, active Bool, label String);")
        .unwrap();

    let escaped = request_for_target_with_headers(
        "/query",
        b"SELECT 'quote\"\\line\nsnow \xE9\x9B\xAA';",
        "X-ClickHouse-Format: JSONEachRow\r\n",
    );
    assert_response(
        &exchange(&database, &escaped),
        "HTTP/1.1 200 OK",
        "{\"'quote\\\"\\\\line\\nsnow 雪'\":\"quote\\\"\\\\line\\nsnow 雪\"}\n",
    );

    let nulls = request_for_target_with_headers(
        "/query",
        b"SELECT MIN(id) AS missing_id, MIN(score) AS missing_score, MIN(active) AS missing_active, MIN(label) AS missing_label FROM empty_values;",
        "X-ClickHouse-Format: JSONEachRow\r\n",
    );
    assert_response(
        &exchange(&database, &nulls),
        "HTTP/1.1 200 OK",
        "{\"missing_id\":null,\"missing_score\":null,\"missing_active\":null,\"missing_label\":null}\n",
    );

    let empty = request_for_target_with_headers(
        "/query",
        b"SELECT id, score, active, label FROM empty_values;",
        "X-ClickHouse-Format: JSONEachRow\r\n",
    );
    assert_response(&exchange(&database, &empty), "HTTP/1.1 200 OK", "");
}

#[test]
fn both_query_routes_stream_typed_json_compact_each_row_and_empty_results() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE typed_values (id Int64, score Float64, active Bool, label String); \
             INSERT INTO typed_values VALUES (-7, 2.0, false, 'ready'), \
                                               (0, -1.25, true, 'snow 雪'); \
             CREATE TABLE empty_values (id Int64, score Float64, active Bool, label String);",
        )
        .unwrap();

    for target in ["/", "/query"] {
        let typed_request = request_for_target_with_headers(
            target,
            b"SELECT id, score, active, label FROM typed_values ORDER BY id;",
            "x-cLiCkHoUsE-fOrMaT:\tJSONCompactEachRow \r\n",
        );
        assert_response(
            &exchange(&database, &typed_request),
            "HTTP/1.1 200 OK",
            "[-7,2.0,false,\"ready\"]\n[0,-1.25,true,\"snow 雪\"]\n",
        );

        let null_request = request_for_target_with_headers(
            target,
            b"SELECT MIN(id), MIN(score), MIN(active), MIN(label) FROM empty_values;",
            "X-ClickHouse-Format: JSONCompactEachRow\r\n",
        );
        assert_response(
            &exchange(&database, &null_request),
            "HTTP/1.1 200 OK",
            "[null,null,null,null]\n",
        );

        let empty_request = request_for_target_with_headers(
            target,
            b"SELECT id, score, active, label FROM empty_values;",
            "X-ClickHouse-Format: JSONCompactEachRow\r\n",
        );
        assert_response(&exchange(&database, &empty_request), "HTTP/1.1 200 OK", "");
    }
}

#[test]
fn both_query_routes_return_csv_with_names_for_all_value_types_and_empty_results() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE typed_values (integer Int64, score Float64, active Bool, label String); \
             INSERT INTO typed_values VALUES \
                 (-9223372036854775808, 2.0, false, 'comma, \"quote\"\ncarriage\rsnow 雪'), \
                 (7, -1.25, true, ''); \
             CREATE TABLE empty_values (integer Int64, score Float64, active Bool, label String);",
        )
        .unwrap();
    let expected = concat!(
        "integer,score,active,label\n",
        "-9223372036854775808,2.0,false,\"comma, \"\"quote\"\"\ncarriage\rsnow 雪\"\n",
        "7,-1.25,true,\n",
    );

    for target in ["/", "/query"] {
        let typed_request = request_for_target_with_headers(
            target,
            b"SELECT integer, score, active, label FROM typed_values ORDER BY integer;",
            "X-ClickHouse-Format: CSVWithNames\r\n",
        );
        assert_response_with_content_type(
            &exchange(&database, &typed_request),
            "HTTP/1.1 200 OK",
            "text/csv; charset=utf-8",
            expected.as_bytes(),
        );

        let null_request = request_for_target_with_headers(
            target,
            b"SELECT MIN(integer) AS missing_integer, MIN(score) AS missing_float, MIN(active) AS missing_boolean, MIN(label) AS missing_string FROM empty_values;",
            "X-ClickHouse-Format: CSVWithNames\r\n",
        );
        assert_response_with_content_type(
            &exchange(&database, &null_request),
            "HTTP/1.1 200 OK",
            "text/csv; charset=utf-8",
            b"missing_integer,missing_float,missing_boolean,missing_string\nNULL,NULL,NULL,NULL\n",
        );

        let empty_request = request_for_target_with_headers(
            target,
            b"SELECT integer, score, active, label FROM empty_values;",
            "X-ClickHouse-Format: CSVWithNames\r\n",
        );
        assert_response_with_content_type(
            &exchange(&database, &empty_request),
            "HTTP/1.1 200 OK",
            "text/csv; charset=utf-8",
            b"integer,score,active,label\n",
        );
    }
}

#[test]
fn both_query_routes_return_tab_separated_with_names_for_all_value_types_and_empty_results() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE typed_values (integer Int64, score Float64, active Bool, label String); \
             INSERT INTO typed_values VALUES \
                 (-9223372036854775808, 2.0, false, 'slash\\tab\tcarriage\rline\nnul\0backspace\u{08}formfeed\u{0c}apostrophe'' snow 雪'), \
                 (7, -1.25, true, ''); \
             CREATE TABLE empty_values (integer Int64, score Float64, active Bool, label String);",
        )
        .unwrap();
    let expected = concat!(
        "integer\tscore\tactive\tlabel\n",
        "-9223372036854775808\t2.0\tfalse\tslash\\\\tab\\tcarriage\\rline\\nnul\\0backspace\\bformfeed\\fapostrophe\\' snow 雪\n",
        "7\t-1.25\ttrue\t\n",
    );

    for target in ["/", "/query"] {
        let typed_request = request_for_target_with_headers(
            target,
            b"SELECT integer, score, active, label FROM typed_values ORDER BY integer;",
            "X-ClickHouse-Format: TabSeparatedWithNames\r\n",
        );
        assert_response_with_content_type(
            &exchange(&database, &typed_request),
            "HTTP/1.1 200 OK",
            "text/tab-separated-values; charset=utf-8",
            expected.as_bytes(),
        );

        let null_request = request_for_target_with_headers(
            target,
            b"SELECT MIN(integer) AS missing_integer, MIN(score) AS missing_float, MIN(active) AS missing_boolean, MIN(label) AS missing_string FROM empty_values;",
            "X-ClickHouse-Format: TabSeparatedWithNames\r\n",
        );
        assert_response_with_content_type(
            &exchange(&database, &null_request),
            "HTTP/1.1 200 OK",
            "text/tab-separated-values; charset=utf-8",
            b"missing_integer\tmissing_float\tmissing_boolean\tmissing_string\n\\N\t\\N\t\\N\t\\N\n",
        );

        let empty_request = request_for_target_with_headers(
            target,
            b"SELECT integer, score, active, label FROM empty_values;",
            "X-ClickHouse-Format: TabSeparatedWithNames\r\n",
        );
        assert_response_with_content_type(
            &exchange(&database, &empty_request),
            "HTTP/1.1 200 OK",
            "text/tab-separated-values; charset=utf-8",
            b"integer\tscore\tactive\tlabel\n",
        );
    }
}

#[test]
fn bearer_authenticated_get_query_honors_json_each_row() {
    let database = SharedDatabase::default();
    let unauthorized = b"GET /?query=SELECT+%2B7+AS+value%3B HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Format: JSONEachRow\r\n\r\n";
    assert_response(
        &authenticated_exchange(&database, "correct-token", unauthorized),
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"bearer authentication required"}"#,
    );

    let authorized = b"GET /?query=SELECT+%2B7+AS+value%3B HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer correct-token\r\nX-ClickHouse-Format: JSONEachRow\r\n\r\n";
    assert_response(
        &authenticated_exchange(&database, "correct-token", authorized),
        "HTTP/1.1 200 OK",
        "{\"value\":7}\n",
    );
}

#[test]
fn bearer_authenticated_queries_honor_json_compact_each_row() {
    let database = SharedDatabase::default();
    let sql = b"SELECT -7 AS integer;";
    let unauthorized = request_for_target_with_headers(
        "/query",
        sql,
        "X-ClickHouse-Format: JSONCompactEachRow\r\n",
    );

    assert_response(
        &authenticated_exchange(&database, "correct-token", &unauthorized),
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"bearer authentication required"}"#,
    );

    let (authorized, _) = request_with_authorization(
        sql,
        "Authorization: Bearer correct-token\r\n\
         X-ClickHouse-Format: JSONCompactEachRow\r\n",
    );
    assert_response(
        &authenticated_exchange(&database, "correct-token", &authorized),
        "HTTP/1.1 200 OK",
        "[-7]\n",
    );
}

#[test]
fn bearer_authenticated_queries_honor_csv_with_names() {
    let database = SharedDatabase::default();
    let sql = b"SELECT -7 AS integer;";
    let unauthorized =
        request_for_target_with_headers("/query", sql, "X-ClickHouse-Format: CSVWithNames\r\n");

    assert_response(
        &authenticated_exchange(&database, "correct-token", &unauthorized),
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"bearer authentication required"}"#,
    );

    let (authorized, _) = request_with_authorization(
        sql,
        "Authorization: Bearer correct-token\r\n\
         X-ClickHouse-Format: CSVWithNames\r\n",
    );
    assert_response_with_content_type(
        &authenticated_exchange(&database, "correct-token", &authorized),
        "HTTP/1.1 200 OK",
        "text/csv; charset=utf-8",
        b"integer\n-7\n",
    );
}

#[test]
fn bearer_authenticated_queries_honor_tab_separated_with_names_on_both_routes() {
    let database = SharedDatabase::default();
    let sql = b"SELECT -7 AS integer;";

    for target in ["/", "/query"] {
        let unauthorized = request_for_target_with_headers(
            target,
            sql,
            "X-ClickHouse-Format: TabSeparatedWithNames\r\n",
        );
        assert_response(
            &authenticated_exchange(&database, "correct-token", &unauthorized),
            "HTTP/1.1 401 Unauthorized",
            r#"{"error":"bearer authentication required"}"#,
        );

        let (authorized, _) = request_with_authorization_for_target(
            target,
            sql,
            "Authorization: Bearer correct-token\r\n\
             X-ClickHouse-Format: TabSeparatedWithNames\r\n",
        );
        assert_response_with_content_type(
            &authenticated_exchange(&database, "correct-token", &authorized),
            "HTTP/1.1 200 OK",
            "text/tab-separated-values; charset=utf-8",
            b"integer\n-7\n",
        );
    }
}

#[test]
fn query_forms_reject_duplicate_and_unsupported_clickhouse_formats() {
    let database = SharedDatabase::default();
    let duplicate_headers = [
        concat!(
            "X-ClickHouse-Format: JSONCompactEachRow\r\n",
            "X-ClickHouse-Format: JSONCompactEachRow\r\n",
        ),
        concat!(
            "X-ClickHouse-Format: JSONEachRow\r\n",
            "X-ClickHouse-Format: JSONEachRow\r\n",
        ),
        concat!(
            "X-ClickHouse-Format: JSONCompactEachRow\r\n",
            "x-clickhouse-format: JSONEachRow\r\n",
        ),
        concat!(
            "X-ClickHouse-Format: JSONEachRow\r\n",
            "X-ClickHouse-Format: JSONCompactEachRow\r\n",
        ),
        concat!(
            "X-ClickHouse-Format: CSVWithNames\r\n",
            "X-ClickHouse-Format: CSVWithNames\r\n",
        ),
        concat!(
            "X-ClickHouse-Format: CSVWithNames\r\n",
            "X-ClickHouse-Format: JSONCompactEachRow\r\n",
        ),
        concat!(
            "X-ClickHouse-Format: TabSeparatedWithNames\r\n",
            "X-ClickHouse-Format: TabSeparatedWithNames\r\n",
        ),
        concat!(
            "X-ClickHouse-Format: TabSeparatedWithNames\r\n",
            "X-ClickHouse-Format: JSONCompactEachRow\r\n",
        ),
    ];

    for target in ["/", "/query"] {
        for headers in duplicate_headers {
            assert_response(
                &exchange(
                    &database,
                    &request_for_target_with_headers(target, b"SELECT 1;", headers),
                ),
                "HTTP/1.1 400 Bad Request",
                r#"{"error":"duplicate X-ClickHouse-Format header"}"#,
            );
        }

        for unsupported in [
            "jsoneachrow",
            "JsonEachRow",
            "JSONEACHROW",
            "jsoncompacteachrow",
            "csvwithnames",
            "tabseparatedwithnames",
            "TabSeparated",
            "CSV",
            "",
        ] {
            let headers = format!("X-ClickHouse-Format: {unsupported}\r\n");
            assert_response(
                &exchange(
                    &database,
                    &request_for_target_with_headers(target, b"SELECT 1;", &headers),
                ),
                "HTTP/1.1 400 Bad Request",
                r#"{"error":"unsupported X-ClickHouse-Format header"}"#,
            );
        }
    }

    assert_response(
        &exchange(
            &database,
            b"GET /?query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Format: JSONEachRow\r\nx-clickhouse-format: JSONEachRow\r\n\r\n",
        ),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"duplicate X-ClickHouse-Format header"}"#,
    );
}

#[test]
fn json_each_row_honors_the_exact_complete_response_cap() {
    let database = SharedDatabase::default();
    let sql = format!("SELECT '{}' AS value;", "x".repeat(1_000));
    let request = request_for_target_with_headers(
        "/query",
        sql.as_bytes(),
        "X-ClickHouse-Format: JSONEachRow\r\n",
    );
    let expected_response = exchange(&database, &request);

    let mut exact_response = Vec::new();
    handle_http_query_with_limits(
        &database,
        Cursor::new(&request),
        &mut exact_response,
        HttpQueryLimits {
            max_response_bytes: expected_response.len(),
            ..HttpQueryLimits::default()
        },
    )
    .expect("the exact complete JSONEachRow response size is accepted");
    assert_eq!(exact_response, expected_response);

    let limits = HttpQueryLimits {
        max_response_bytes: expected_response.len() - 1,
        ..HttpQueryLimits::default()
    };
    let mut capped_response = Vec::new();
    handle_http_query_with_limits(
        &database,
        Cursor::new(&request),
        &mut capped_response,
        limits,
    )
    .expect("the fixed response-limit error fits");
    assert!(capped_response.len() <= limits.max_response_bytes);
    assert_response(
        &capped_response,
        "HTTP/1.1 500 Internal Server Error",
        r#"{"error":"response exceeds configured byte limit"}"#,
    );
}

#[test]
fn json_compact_each_row_honors_the_complete_response_cap() {
    let database = SharedDatabase::default();
    let sql = format!("SELECT '{}' AS value;", "x".repeat(1_000));
    let request = request_for_target_with_headers(
        "/query",
        sql.as_bytes(),
        "X-ClickHouse-Format: JSONCompactEachRow\r\n",
    );
    let expected_response = exchange(&database, &request);

    let mut exact_response = Vec::new();
    handle_http_query_with_limits(
        &database,
        Cursor::new(&request),
        &mut exact_response,
        HttpQueryLimits {
            max_response_bytes: expected_response.len(),
            ..HttpQueryLimits::default()
        },
    )
    .expect("the exact complete response size is accepted");
    assert_eq!(exact_response, expected_response);

    let limits = HttpQueryLimits {
        max_response_bytes: expected_response.len() - 1,
        ..HttpQueryLimits::default()
    };
    let mut capped_response = Vec::new();
    handle_http_query_with_limits(
        &database,
        Cursor::new(&request),
        &mut capped_response,
        limits,
    )
    .expect("the fixed response-limit error fits");
    assert!(capped_response.len() <= limits.max_response_bytes);
    assert_response(
        &capped_response,
        "HTTP/1.1 500 Internal Server Error",
        r#"{"error":"response exceeds configured byte limit"}"#,
    );
}

#[test]
fn csv_with_names_honors_the_complete_response_cap() {
    let database = SharedDatabase::default();
    let sql = format!("SELECT '{}' AS value;", "x".repeat(1_000));
    let request = request_for_target_with_headers(
        "/query",
        sql.as_bytes(),
        "X-ClickHouse-Format: CSVWithNames\r\n",
    );
    let expected_response = exchange(&database, &request);

    let mut exact_response = Vec::new();
    handle_http_query_with_limits(
        &database,
        Cursor::new(&request),
        &mut exact_response,
        HttpQueryLimits {
            max_response_bytes: expected_response.len(),
            ..HttpQueryLimits::default()
        },
    )
    .expect("the exact complete CSV response size is accepted");
    assert_eq!(exact_response, expected_response);

    let limits = HttpQueryLimits {
        max_response_bytes: expected_response.len() - 1,
        ..HttpQueryLimits::default()
    };
    let mut capped_response = Vec::new();
    handle_http_query_with_limits(
        &database,
        Cursor::new(&request),
        &mut capped_response,
        limits,
    )
    .expect("the fixed response-limit error fits");
    assert!(capped_response.len() <= limits.max_response_bytes);
    assert_response(
        &capped_response,
        "HTTP/1.1 500 Internal Server Error",
        r#"{"error":"response exceeds configured byte limit"}"#,
    );
}

#[test]
fn tab_separated_with_names_honors_the_exact_complete_response_cap() {
    let database = SharedDatabase::default();
    let sql = format!("SELECT '{}' AS value;", "x".repeat(1_000));
    let request = request_for_target_with_headers(
        "/query",
        sql.as_bytes(),
        "X-ClickHouse-Format: TabSeparatedWithNames\r\n",
    );
    let expected_response = exchange(&database, &request);

    let mut exact_response = Vec::new();
    handle_http_query_with_limits(
        &database,
        Cursor::new(&request),
        &mut exact_response,
        HttpQueryLimits {
            max_response_bytes: expected_response.len(),
            ..HttpQueryLimits::default()
        },
    )
    .expect("the exact complete TSV response size is accepted");
    assert_eq!(exact_response, expected_response);

    let limits = HttpQueryLimits {
        max_response_bytes: expected_response.len() - 1,
        ..HttpQueryLimits::default()
    };
    let mut capped_response = Vec::new();
    handle_http_query_with_limits(
        &database,
        Cursor::new(&request),
        &mut capped_response,
        limits,
    )
    .expect("the fixed response-limit error fits");
    assert!(capped_response.len() <= limits.max_response_bytes);
    assert_response(
        &capped_response,
        "HTTP/1.1 500 Internal Server Error",
        r#"{"error":"response exceeds configured byte limit"}"#,
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
fn bearer_authentication_protects_root_queries() {
    let database = SharedDatabase::default();
    let sql = b"SELECT true AS ready;";
    let (missing_credentials, body_offset) = request_with_authorization_for_target("/", sql, "");
    let mut input = Cursor::new(missing_credentials);
    let mut unauthorized = Vec::new();

    handle_http_query_with_bearer_token(&database, "correct-token", &mut input, &mut unauthorized)
        .expect("missing root credentials produce a response");

    assert_eq!(input.position(), body_offset);
    assert_response(
        &unauthorized,
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"bearer authentication required"}"#,
    );
    assert!(
        std::str::from_utf8(&unauthorized)
            .unwrap()
            .contains("\r\nWWW-Authenticate: Bearer\r\n")
    );

    let (authorized, _) =
        request_with_authorization_for_target("/", sql, "Authorization: Bearer correct-token\r\n");
    assert_response(
        &authenticated_exchange(&database, "correct-token", &authorized),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"ready","type":"Bool"}],"rows":[[true]]}"#,
    );
}

#[test]
fn bearer_authentication_also_protects_ping() {
    let inner = Arc::new(RwLock::new(Database::new()));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let poisoner = thread::spawn(move || {
        let _guard = inner.write().unwrap();
        panic!("poison the database lock");
    });
    assert!(poisoner.join().is_err());

    let unauthorized_request =
        b"GET /ping HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1\r\n\r\nx";
    let body_offset = unauthorized_request.len() as u64 - 1;
    let mut input = Cursor::new(unauthorized_request);
    let mut unauthorized = Vec::new();
    handle_http_query_with_bearer_token(&database, "correct-token", &mut input, &mut unauthorized)
        .expect("missing ping credentials produce a response");
    assert_eq!(
        input.position(),
        body_offset,
        "authentication failure must not consume a ping body"
    );
    assert_response(
        &unauthorized,
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"bearer authentication required"}"#,
    );
    assert!(
        std::str::from_utf8(&unauthorized)
            .unwrap()
            .contains("\r\nWWW-Authenticate: Bearer\r\n")
    );

    assert_ok_health_response(&authenticated_exchange(
        &database,
        "correct-token",
        b"GET /ping HTTP/1.1\r\nHost: localhost\r\naUtHoRiZaTiOn: bEaReR correct-token\r\n\r\n",
    ));
}

#[test]
fn bearer_scheme_accepts_one_or_more_spaces_before_the_token() {
    let database = SharedDatabase::default();

    for spaces in [" ", "  ", "    "] {
        let authorization = format!("Authorization: Bearer{spaces}correct-token\r\n");
        let (request, _) = request_with_authorization(b"SELECT true AS ready;", &authorization);

        assert_response(
            &authenticated_exchange(&database, "correct-token", &request),
            "HTTP/1.1 200 OK",
            r#"{"columns":[{"name":"ready","type":"Bool"}],"rows":[[true]]}"#,
        );
    }
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
fn malformed_configured_bearer_tokens_are_rejected_before_reading_input() {
    let database = SharedDatabase::default();
    let invalid_tokens = ["secret:42", "abc=def", "two words", " ", "\t", "tökén"];
    let mut expected_response = None;

    for token in invalid_tokens {
        let mut response = Vec::new();
        handle_http_query_with_bearer_token(&database, token, FailingReader, &mut response)
            .unwrap_or_else(|error| panic!("invalid configuration {token:?} responds: {error}"));

        assert_response(
            &response,
            "HTTP/1.1 500 Internal Server Error",
            r#"{"error":"configured bearer token is not valid token68"}"#,
        );
        if let Some(expected_response) = &expected_response {
            assert_eq!(&response, expected_response);
        } else {
            expected_response = Some(response);
        }
    }

    let expected_response = expected_response.expect("at least one invalid configuration");
    let limits = HttpQueryLimits {
        max_response_bytes: expected_response.len() - 1,
        ..HttpQueryLimits::default()
    };
    let mut capped_response = Vec::new();
    handle_http_query_with_bearer_token_and_limits(
        &database,
        "secret:42",
        FailingReader,
        &mut capped_response,
        limits,
    )
    .expect("the fixed response-limit error fits");
    assert!(capped_response.len() <= limits.max_response_bytes);
    assert_response(
        &capped_response,
        "HTTP/1.1 500 Internal Server Error",
        r#"{"error":"response exceeds configured byte limit"}"#,
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
fn clickhouse_key_authentication_wires_query_insert_and_operational_routes() {
    let database = SharedDatabase::default();
    database.execute("CREATE TABLE events (id Int64);").unwrap();
    let key = "correct key:42";
    let (insert, _) = request_with_authorization_for_target(
        "/insert",
        b"INSERT INTO events VALUES (7);",
        "x-cLiCkHoUsE-kEy: correct key:42\r\n",
    );

    let insert_response = clickhouse_key_exchange(&database, key, &insert);
    assert_response_with_content_type(
        &insert_response,
        "HTTP/1.1 200 OK",
        "text/plain; charset=utf-8",
        b"",
    );
    assert_clickhouse_key_response_is_not_cacheable(&insert_response);

    let query = request_for_target_with_headers(
        "/query",
        b"SELECT id FROM events;",
        "X-ClickHouse-Key: correct key:42\r\n",
    );
    let post_query_response = clickhouse_key_exchange(&database, key, &query);
    assert_response(
        &post_query_response,
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"}],"rows":[[7]]}"#,
    );
    assert_clickhouse_key_response_is_not_cacheable(&post_query_response);

    let get_query_response = clickhouse_key_exchange(
        &database,
        key,
        b"GET /?query=SELECT+id+FROM+events%3B HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: correct key:42\r\n\r\n",
    );
    assert_response(
        &get_query_response,
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"}],"rows":[[7]]}"#,
    );
    assert_clickhouse_key_response_is_not_cacheable(&get_query_response);

    let ping_response = clickhouse_key_exchange(
        &database,
        key,
        b"GET /ping HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: correct key:42\r\n\r\n",
    );
    assert_ok_health_response(&ping_response);
    assert_clickhouse_key_response_is_not_cacheable(&ping_response);

    let ready_response = clickhouse_key_exchange(
        &database,
        key,
        b"GET /ready HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: correct key:42\r\n\r\n",
    );
    assert_ok_health_response(&ready_response);
    assert_clickhouse_key_response_is_not_cacheable(&ready_response);

    let metrics_response = clickhouse_key_exchange(
        &database,
        key,
        b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: correct key:42\r\n\r\n",
    );
    assert_ok_metrics_response(&metrics_response, 1, 1, 1, 8);
    assert_clickhouse_key_response_is_not_cacheable(&metrics_response);
}

#[test]
fn clickhouse_key_rejections_are_identical_and_stop_before_the_body() {
    let database = SharedDatabase::default();
    database.execute("CREATE TABLE events (id Int64);").unwrap();
    let rejected_headers = [
        ("missing", ""),
        ("empty", "X-ClickHouse-Key:\r\n"),
        ("whitespace only", "X-ClickHouse-Key: \t \r\n"),
        ("incorrect", "X-ClickHouse-Key: incorrect\r\n"),
        ("wrong case", "X-ClickHouse-Key: Correct-Key\r\n"),
        ("bearer only", "Authorization: Bearer correct-key\r\n"),
        (
            "duplicate",
            "X-ClickHouse-Key: correct-key\r\nx-clickhouse-key: correct-key\r\n",
        ),
    ];
    let mut expected_response = None;

    for (name, headers) in rejected_headers {
        let (request, body_offset) = request_with_authorization_for_target(
            "/insert",
            b"INSERT INTO events VALUES (1);",
            headers,
        );
        let mut input = Cursor::new(request);
        let mut response = Vec::new();
        handle_http_query_with_clickhouse_key(&database, "correct-key", &mut input, &mut response)
            .unwrap_or_else(|error| panic!("{name} credentials produce a response: {error}"));

        assert_eq!(
            input.position(),
            body_offset,
            "{name} credentials must not consume the SQL body"
        );
        assert_response(
            &response,
            "HTTP/1.1 401 Unauthorized",
            r#"{"error":"X-ClickHouse-Key authentication required"}"#,
        );
        assert_response_header(&response, "WWW-Authenticate: X-ClickHouse-Key");
        assert_clickhouse_key_response_is_not_cacheable(&response);
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
        &exchange(
            &database,
            &request_for_target("/query", b"SELECT id FROM events;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"}],"rows":[]}"#,
    );

    let (key_only_request, _) = request_with_authorization(
        b"SELECT id FROM events;",
        "X-ClickHouse-Key: correct-key\r\n",
    );
    assert_response(
        &authenticated_exchange(&database, "correct-key", &key_only_request),
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"bearer authentication required"}"#,
    );
}

#[test]
fn clickhouse_key_authentication_precedes_database_lock_access() {
    let inner = Arc::new(RwLock::new(Database::new()));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let poisoner = thread::spawn(move || {
        let _guard = inner.write().unwrap();
        panic!("poison the database lock");
    });
    assert!(poisoner.join().is_err());

    assert_response(
        &clickhouse_key_exchange(
            &database,
            "correct-key",
            b"GET /ready HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: incorrect\r\n\r\n",
        ),
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"X-ClickHouse-Key authentication required"}"#,
    );
}

#[test]
fn invalid_configured_clickhouse_keys_are_rejected_before_input() {
    let database = SharedDatabase::default();
    let cases = [
        ("", "configured ClickHouse key must not be empty"),
        (
            " leading",
            "configured ClickHouse key is not a valid HTTP header value",
        ),
        (
            "trailing ",
            "configured ClickHouse key is not a valid HTTP header value",
        ),
        (
            "line\nbreak",
            "configured ClickHouse key is not a valid HTTP header value",
        ),
        (
            "sëcret",
            "configured ClickHouse key is not a valid HTTP header value",
        ),
    ];

    for (key, message) in cases {
        let mut response = Vec::new();
        handle_http_query_with_clickhouse_key(&database, key, FailingReader, &mut response)
            .unwrap_or_else(|error| panic!("invalid configuration {key:?} responds: {error}"));
        assert_response(
            &response,
            "HTTP/1.1 500 Internal Server Error",
            &format!(r#"{{"error":"{message}"}}"#),
        );
    }
}

#[test]
fn clickhouse_key_rejection_respects_the_complete_response_cap() {
    let database = SharedDatabase::default();
    let request = request(b"SELECT 1;");
    let unrestricted = clickhouse_key_exchange(&database, "correct-key", &request);
    let mut exact_response = Vec::new();

    handle_http_query_with_clickhouse_key_and_limits(
        &database,
        "correct-key",
        Cursor::new(&request),
        &mut exact_response,
        HttpQueryLimits {
            max_response_bytes: unrestricted.len(),
            ..HttpQueryLimits::default()
        },
    )
    .unwrap();
    assert_eq!(exact_response, unrestricted);
    assert_response_header(&exact_response, "WWW-Authenticate: X-ClickHouse-Key");
    assert_clickhouse_key_response_is_not_cacheable(&exact_response);

    let mut too_small_output = Vec::new();
    handle_http_query_with_clickhouse_key_and_limits(
        &database,
        "correct-key",
        Cursor::new(&request),
        &mut too_small_output,
        HttpQueryLimits {
            max_response_bytes: unrestricted.len() - 1,
            ..HttpQueryLimits::default()
        },
    )
    .expect("the shorter fixed response-limit error fits");
    assert_response(
        &too_small_output,
        "HTTP/1.1 500 Internal Server Error",
        r#"{"error":"response exceeds configured byte limit"}"#,
    );
    assert_clickhouse_key_response_is_not_cacheable(&too_small_output);

    let mut zero_limit_output = Vec::new();
    let error = handle_http_query_with_clickhouse_key_and_limits(
        &database,
        "correct-key",
        Cursor::new(&request),
        &mut zero_limit_output,
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
    assert!(zero_limit_output.is_empty());
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
            b"POST / HTTP/1.1\r\nHost: localhost\r\n\r\n",
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

    let root_method = exchange(
        &database,
        b"GET / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
    );
    assert_response(
        &root_method,
        "HTTP/1.1 405 Method Not Allowed",
        r#"{"error":"method must be POST"}"#,
    );
    assert!(
        std::str::from_utf8(&root_method)
            .unwrap()
            .contains("\r\nAllow: POST\r\n")
    );

    assert_response(
        &exchange(
            &database,
            b"POST /?query HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
        ),
        "HTTP/1.1 404 Not Found",
        r#"{"error":"request target must be / or /query"}"#,
    );

    let ping_method = exchange(
        &database,
        b"POST /ping HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
    );
    assert_response(
        &ping_method,
        "HTTP/1.1 405 Method Not Allowed",
        r#"{"error":"method must be GET for /ping"}"#,
    );
    assert!(
        std::str::from_utf8(&ping_method)
            .unwrap()
            .contains("\r\nAllow: GET\r\n")
    );

    assert_response(
        &exchange(
            &database,
            b"POST /other HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
        ),
        "HTTP/1.1 404 Not Found",
        r#"{"error":"request target must be / or /query"}"#,
    );
    assert_response(
        &exchange(&database, b"GET /other HTTP/1.1\r\nHost: localhost\r\n\r\n"),
        "HTTP/1.1 404 Not Found",
        r#"{"error":"request target must be /ping, /ready, or /metrics"}"#,
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
fn ping_requires_host_http_1_1_and_an_empty_body() {
    let database = SharedDatabase::default();
    let cases: &[(&[u8], &str, &str)] = &[
        (
            b"GET /ping HTTP/1.1\r\n\r\n",
            "HTTP/1.1 400 Bad Request",
            r#"{"error":"Host header is required"}"#,
        ),
        (
            b"GET /ping HTTP/1.1\r\nHost: \r\n\r\n",
            "HTTP/1.1 400 Bad Request",
            r#"{"error":"invalid Host header"}"#,
        ),
        (
            b"GET /ping HTTP/1.0\r\nHost: localhost\r\n\r\n",
            "HTTP/1.1 505 HTTP Version Not Supported",
            r#"{"error":"HTTP/1.1 is required"}"#,
        ),
        (
            b"GET /ping HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1\r\n\r\nx",
            "HTTP/1.1 400 Bad Request",
            r#"{"error":"GET /ping does not accept a request body"}"#,
        ),
        (
            b"GET /ping HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
            "HTTP/1.1 400 Bad Request",
            r#"{"error":"Transfer-Encoding is not supported"}"#,
        ),
    ];

    for (request, status, body) in cases {
        assert_response(&exchange(&database, request), status, body);
    }
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

    response.clear();
    handle_http_query_with_limits(
        &database,
        Cursor::new(b"GET /?query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\n\r\n"),
        &mut response,
        HttpQueryLimits {
            max_sql_bytes: 8,
            ..HttpQueryLimits::default()
        },
    )
    .unwrap();
    assert_response(
        &response,
        "HTTP/1.1 413 Payload Too Large",
        r#"{"error":"SQL query exceeds configured byte limit"}"#,
    );
}

#[test]
fn oversized_root_query_is_rejected_before_its_body_is_read() {
    let database = SharedDatabase::default();
    let request = request_for_target("/", b"SELECT 1;");
    let body_offset = request.len() as u64 - b"SELECT 1;".len() as u64;
    let mut input = Cursor::new(request);
    let mut response = Vec::new();

    handle_http_query_with_limits(
        &database,
        &mut input,
        &mut response,
        HttpQueryLimits {
            max_sql_bytes: 4,
            ..HttpQueryLimits::default()
        },
    )
    .unwrap();

    assert_eq!(input.position(), body_offset);
    assert_response(
        &response,
        "HTTP/1.1 413 Payload Too Large",
        r#"{"error":"request body exceeds configured byte limit"}"#,
    );
}

#[test]
fn ping_honors_exact_header_and_complete_response_byte_limits() {
    let database = SharedDatabase::default();
    let request = b"GET /ping HTTP/1.1\r\nHost: localhost\r\n\r\n";
    let expected_response = exchange(&database, request);

    let mut response = Vec::new();
    handle_http_query_with_limits(
        &database,
        Cursor::new(request),
        &mut response,
        HttpQueryLimits {
            max_header_bytes: request.len(),
            max_header_count: 1,
            max_response_bytes: expected_response.len(),
            ..HttpQueryLimits::default()
        },
    )
    .unwrap();
    assert_eq!(response, expected_response);

    let mut header_overflow_response = Vec::new();
    handle_http_query_with_limits(
        &database,
        Cursor::new(request),
        &mut header_overflow_response,
        HttpQueryLimits {
            max_header_bytes: request.len() - 1,
            ..HttpQueryLimits::default()
        },
    )
    .unwrap();
    assert_response(
        &header_overflow_response,
        "HTTP/1.1 431 Request Header Fields Too Large",
        r#"{"error":"request headers exceed configured byte limit"}"#,
    );

    let mut response_overflow_output = Vec::new();
    let error = handle_http_query_with_limits(
        &database,
        Cursor::new(request),
        &mut response_overflow_output,
        HttpQueryLimits {
            max_response_bytes: expected_response.len() - 1,
            ..HttpQueryLimits::default()
        },
    )
    .expect_err("the cap cannot hold either the ping or fixed limit response");
    assert!(matches!(
        error,
        HttpQueryError::ResponseLimitExceeded {
            max_bytes,
            ..
        } if max_bytes == expected_response.len() - 1
    ));
    assert!(response_overflow_output.is_empty());
}

#[test]
fn ready_honors_exact_header_and_complete_response_byte_limits() {
    let database = SharedDatabase::default();
    let request = b"GET /ready HTTP/1.1\r\nHost: localhost\r\n\r\n";
    let expected_response = exchange(&database, request);

    let mut response = Vec::new();
    handle_http_query_with_limits(
        &database,
        Cursor::new(request),
        &mut response,
        HttpQueryLimits {
            max_header_bytes: request.len(),
            max_header_count: 1,
            max_response_bytes: expected_response.len(),
            ..HttpQueryLimits::default()
        },
    )
    .unwrap();
    assert_eq!(response, expected_response);

    let mut response_overflow_output = Vec::new();
    let error = handle_http_query_with_limits(
        &database,
        Cursor::new(request),
        &mut response_overflow_output,
        HttpQueryLimits {
            max_response_bytes: expected_response.len() - 1,
            ..HttpQueryLimits::default()
        },
    )
    .expect_err("the cap cannot hold either readiness or the fixed limit response");
    assert!(matches!(
        error,
        HttpQueryError::ResponseLimitExceeded {
            max_bytes,
            ..
        } if max_bytes == expected_response.len() - 1
    ));
    assert!(response_overflow_output.is_empty());
}

#[test]
fn metrics_honors_the_complete_response_byte_limit() {
    let database = SharedDatabase::default();
    let request = b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n";
    let expected_response = exchange(&database, request);

    let mut exact_response = Vec::new();
    handle_http_query_with_limits(
        &database,
        Cursor::new(request),
        &mut exact_response,
        HttpQueryLimits {
            max_response_bytes: expected_response.len(),
            ..HttpQueryLimits::default()
        },
    )
    .unwrap();
    assert_eq!(exact_response, expected_response);

    let mut capped_response = Vec::new();
    let limits = HttpQueryLimits {
        max_response_bytes: expected_response.len() - 1,
        ..HttpQueryLimits::default()
    };
    handle_http_query_with_limits(
        &database,
        Cursor::new(request),
        &mut capped_response,
        limits,
    )
    .expect("the bounded limit response fits");
    assert!(capped_response.len() <= limits.max_response_bytes);
    assert_response(
        &capped_response,
        "HTTP/1.1 500 Internal Server Error",
        r#"{"error":"response exceeds configured byte limit"}"#,
    );

    let mut too_small_output = Vec::new();
    let error = handle_http_query_with_limits(
        &database,
        Cursor::new(request),
        &mut too_small_output,
        HttpQueryLimits {
            max_response_bytes: 0,
            ..HttpQueryLimits::default()
        },
    )
    .expect_err("even the fixed response-limit error cannot fit");
    assert!(matches!(
        error,
        HttpQueryError::ResponseLimitExceeded { max_bytes: 0, .. }
    ));
    assert!(too_small_output.is_empty());
}

#[test]
fn query_routes_reject_mutating_and_multi_statement_sql_without_side_effects() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE retained (value Int64); INSERT INTO retained VALUES (7);")
        .unwrap();

    assert_response(
        &exchange(&database, &request_for_target("/", b"DROP TABLE retained;")),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"read-only query accepts only SELECT, SHOW DATABASES, SHOW TABLES, SHOW CREATE TABLE, DESCRIBE TABLE, or EXISTS TABLE; found DROP TABLE"}"#,
    );
    assert_response(
        &exchange(
            &database,
            b"GET /?query=DROP+TABLE+retained%3B HTTP/1.1\r\nHost: localhost\r\n\r\n",
        ),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"read-only query accepts only SELECT, SHOW DATABASES, SHOW TABLES, SHOW CREATE TABLE, DESCRIBE TABLE, or EXISTS TABLE; found DROP TABLE"}"#,
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
fn authenticated_insert_route_commits_a_batch_and_returns_an_empty_response() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE events (id Int64, label String, score Float64, active Bool); \
             CREATE TABLE readings (value Float64); \
             ALTER TABLE events RENAME COLUMN label TO name;",
        )
        .unwrap();
    let (request, _) = request_with_authorization_for_target(
        "/insert",
        b"INSERT INTO events (name, ID) VALUES ('one', 1), ('two', 2); \
          INSERT INTO readings (VALUE) VALUES (1.5); \
          INSERT INTO events (active, id, name) VALUES (true, 3, 'three');",
        "Authorization: Bearer correct-token\r\n",
    );

    assert_response_with_content_type(
        &authenticated_exchange(&database, "correct-token", &request),
        "HTTP/1.1 200 OK",
        "text/plain; charset=utf-8",
        b"",
    );
    assert_response(
        &exchange(
            &database,
            &request_for_target(
                "/query",
                b"SELECT id, name, score, active FROM events ORDER BY id;",
            ),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"},{"name":"name","type":"String"},{"name":"score","type":"Float64"},{"name":"active","type":"Bool"}],"rows":[[1,"one",0.0,false],[2,"two",0.0,false],[3,"three",0.0,true]]}"#,
    );
    assert_response(
        &exchange(
            &database,
            &request_for_target("/query", b"SELECT value FROM readings;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"value","type":"Float64"}],"rows":[[1.5]]}"#,
    );
}

#[test]
fn authenticated_insert_route_rolls_back_invalid_and_mixed_batches() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE events (id Int64); \
             CREATE TABLE readings (value Float64);",
        )
        .unwrap();
    let cases: &[(&[u8], &str)] = &[
        (
            b"INSERT INTO events VALUES (1); INSERT INTO readings VALUES ('wrong');",
            r#"{"error":"type mismatch for column 'readings.value': expected Float64, found String"}"#,
        ),
        (
            b"INSERT INTO events VALUES (2); SELECT id FROM events;",
            r#"{"error":"INSERT-only batch accepts only INSERT statements; found SELECT"}"#,
        ),
        (
            b"INSERT INTO events VALUES (3); ALTER TABLE events RENAME COLUMN id TO event_id;",
            r#"{"error":"INSERT-only batch accepts only INSERT statements; found ALTER TABLE"}"#,
        ),
        (
            b"INSERT INTO events (id) VALUES (4); INSERT INTO readings (missing) VALUES (1.5);",
            r#"{"error":"column 'missing' does not exist in table 'readings'"}"#,
        ),
    ];

    for (sql, expected_body) in cases {
        let (request, _) = request_with_authorization_for_target(
            "/insert",
            sql,
            "Authorization: Bearer correct-token\r\n",
        );
        assert_response(
            &authenticated_exchange(&database, "correct-token", &request),
            "HTTP/1.1 400 Bad Request",
            expected_body,
        );
    }

    assert_response(
        &exchange(
            &database,
            &request_for_target("/query", b"SELECT id FROM events;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"}],"rows":[]}"#,
    );
    assert_response(
        &exchange(
            &database,
            &request_for_target("/query", b"SELECT value FROM readings;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"value","type":"Float64"}],"rows":[]}"#,
    );
}

#[test]
fn insert_route_is_bearer_only_exact_and_does_not_make_query_routes_mutable() {
    let database = SharedDatabase::default();
    database.execute("CREATE TABLE events (id Int64);").unwrap();
    let sql = b"INSERT INTO events VALUES (1);";

    assert_response(
        &exchange(&database, &request_for_target("/insert", sql)),
        "HTTP/1.1 404 Not Found",
        r#"{"error":"request target must be / or /query"}"#,
    );

    let (missing_credentials, body_offset) =
        request_with_authorization_for_target("/insert", sql, "");
    let mut input = Cursor::new(missing_credentials);
    let mut response = Vec::new();
    handle_http_query_with_bearer_token(&database, "correct-token", &mut input, &mut response)
        .unwrap();
    assert_eq!(input.position(), body_offset);
    assert_response(
        &response,
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"bearer authentication required"}"#,
    );

    let (query_request, _) = request_with_authorization_for_target(
        "/query",
        sql,
        "Authorization: Bearer correct-token\r\n",
    );
    assert_response(
        &authenticated_exchange(&database, "correct-token", &query_request),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"read-only query accepts only SELECT, SHOW DATABASES, SHOW TABLES, SHOW CREATE TABLE, DESCRIBE TABLE, or EXISTS TABLE; found INSERT"}"#,
    );

    for target in ["/insert/", "/insert?async=1"] {
        let (inexact_request, _) = request_with_authorization_for_target(
            target,
            sql,
            "Authorization: Bearer correct-token\r\n",
        );
        assert_response(
            &authenticated_exchange(&database, "correct-token", &inexact_request),
            "HTTP/1.1 404 Not Found",
            r#"{"error":"request target must be / or /query"}"#,
        );
    }

    let wrong_method = authenticated_exchange(
        &database,
        "correct-token",
        b"GET /insert HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer correct-token\r\n\r\n",
    );
    assert_response(
        &wrong_method,
        "HTTP/1.1 405 Method Not Allowed",
        r#"{"error":"method must be POST for /insert"}"#,
    );
    assert!(
        std::str::from_utf8(&wrong_method)
            .unwrap()
            .contains("\r\nAllow: POST\r\n")
    );

    assert_response(
        &exchange(
            &database,
            &request_for_target("/query", b"SELECT id FROM events;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"}],"rows":[]}"#,
    );
}

#[test]
fn authenticated_insert_route_returns_503_without_waiting_for_a_reader() {
    let mut initial = Database::new();
    initial.execute("CREATE TABLE events (id Int64);").unwrap();
    let inner = Arc::new(RwLock::new(initial));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let mut reader = Some(inner.read().unwrap());
    let (request, _) = request_with_authorization_for_target(
        "/insert",
        b"INSERT INTO events VALUES (1);",
        "Authorization: Bearer correct-token\r\n",
    );
    let (sender, receiver) = mpsc::channel();
    let worker_database = database.clone();
    let worker = thread::spawn(move || {
        sender
            .send(authenticated_exchange(
                &worker_database,
                "correct-token",
                &request,
            ))
            .unwrap();
    });

    let response = match receiver.recv_timeout(Duration::from_secs(2)) {
        Ok(response) => response,
        Err(error) => {
            drop(reader.take());
            worker.join().unwrap();
            panic!("HTTP insert admission blocked behind a reader: {error}");
        }
    };
    assert_response(
        &response,
        "HTTP/1.1 503 Service Unavailable",
        r#"{"error":"database is unavailable"}"#,
    );
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
fn authenticated_insert_route_reports_lock_poisoning_as_500() {
    let mut initial = Database::new();
    initial.execute("CREATE TABLE events (id Int64);").unwrap();
    let inner = Arc::new(RwLock::new(initial));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let poisoner = thread::spawn(move || {
        let _guard = inner.write().unwrap();
        panic!("poison the database lock");
    });
    assert!(poisoner.join().is_err());
    let (request, _) = request_with_authorization_for_target(
        "/insert",
        b"INSERT INTO events VALUES (1);",
        "Authorization: Bearer correct-token\r\n",
    );

    assert_response(
        &authenticated_exchange(&database, "correct-token", &request),
        "HTTP/1.1 500 Internal Server Error",
        r#"{"error":"database is unavailable"}"#,
    );
}

#[test]
fn authenticated_insert_route_preserves_the_sql_body_limit() {
    let database = SharedDatabase::default();
    database.execute("CREATE TABLE events (id Int64);").unwrap();
    let sql = b"INSERT INTO events VALUES (1);";
    let (request, body_offset) = request_with_authorization_for_target(
        "/insert",
        sql,
        "Authorization: Bearer correct-token\r\n",
    );
    let mut input = Cursor::new(&request);
    let mut response = Vec::new();

    handle_http_query_with_bearer_token_and_limits(
        &database,
        "correct-token",
        &mut input,
        &mut response,
        HttpQueryLimits {
            max_sql_bytes: sql.len() - 1,
            ..HttpQueryLimits::default()
        },
    )
    .unwrap();
    assert_eq!(input.position(), body_offset);
    assert_response(
        &response,
        "HTTP/1.1 413 Payload Too Large",
        r#"{"error":"request body exceeds configured byte limit"}"#,
    );

    let mut exact_limit_response = Vec::new();
    handle_http_query_with_bearer_token_and_limits(
        &database,
        "correct-token",
        Cursor::new(request),
        &mut exact_limit_response,
        HttpQueryLimits {
            max_sql_bytes: sql.len(),
            ..HttpQueryLimits::default()
        },
    )
    .unwrap();
    assert_response_with_content_type(
        &exact_limit_response,
        "HTTP/1.1 200 OK",
        "text/plain; charset=utf-8",
        b"",
    );
}

#[test]
fn authenticated_insert_route_preflights_the_response_cap_before_commit() {
    let (request, _) = request_with_authorization_for_target(
        "/insert",
        b"INSERT INTO events VALUES (1);",
        "Authorization: Bearer correct-token\r\n",
    );
    let sizing_database = SharedDatabase::default();
    sizing_database
        .execute("CREATE TABLE events (id Int64);")
        .unwrap();
    let success_response = authenticated_exchange(&sizing_database, "correct-token", &request);

    let database = SharedDatabase::default();
    database.execute("CREATE TABLE events (id Int64);").unwrap();
    let max_response_bytes = success_response.len() - 1;
    let mut response = Vec::new();
    let error = handle_http_query_with_bearer_token_and_limits(
        &database,
        "correct-token",
        Cursor::new(request),
        &mut response,
        HttpQueryLimits {
            max_response_bytes,
            ..HttpQueryLimits::default()
        },
    )
    .expect_err("the fixed success response exceeds the cap");

    assert!(matches!(
        error,
        HttpQueryError::ResponseLimitExceeded { max_bytes, .. }
            if max_bytes == max_response_bytes
    ));
    assert!(response.is_empty());
    assert_response(
        &exchange(
            &database,
            &request_for_target("/query", b"SELECT id FROM events;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"}],"rows":[]}"#,
    );
}

#[test]
fn authenticated_csv_insert_ingests_all_physical_types_quoting_and_is_query_visible() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE typed_values (id Int64, score Float64, active Bool, label String);")
        .unwrap();
    let csv = b"label,active,score,id\r\n\
\"comma, quote \"\" and LF\nline\",true,1.5,-9223372036854775808\r\n\
\"CRLF\r\nline\",false,-0.125,9223372036854775807\r\n";
    let (request, _) = request_with_authorization_for_target(
        "/insert/typed_values",
        csv,
        "Authorization: Bearer correct-token\r\n",
    );

    assert_response_with_content_type(
        &authenticated_exchange(&database, "correct-token", &request),
        "HTTP/1.1 200 OK",
        "text/plain; charset=utf-8",
        b"",
    );
    assert_response(
        &exchange(
            &database,
            &request_for_target(
                "/query",
                b"SELECT id, score, active, label FROM typed_values ORDER BY id;",
            ),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"},{"name":"score","type":"Float64"},{"name":"active","type":"Bool"},{"name":"label","type":"String"}],"rows":[[-9223372036854775808,1.5,true,"comma, quote \" and LF\nline"],[9223372036854775807,-0.125,false,"CRLF\r\nline"]]}"#,
    );
}

#[test]
fn clickhouse_key_csv_insert_uses_the_same_authenticated_route() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE events (id Int64, label String);")
        .unwrap();
    let request = request_for_target_with_headers(
        "/insert/events",
        b"id,label\n7,key-authenticated\n",
        "x-cLiCkHoUsE-kEy: correct key:42\r\n",
    );

    let response = clickhouse_key_exchange(&database, "correct key:42", &request);
    assert_response_with_content_type(
        &response,
        "HTTP/1.1 200 OK",
        "text/plain; charset=utf-8",
        b"",
    );
    assert_clickhouse_key_response_is_not_cacheable(&response);
    assert_response(
        &exchange(
            &database,
            &request_for_target("/query", b"SELECT id, label FROM events;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"},{"name":"label","type":"String"}],"rows":[[7,"key-authenticated"]]}"#,
    );
}

#[test]
fn bearer_authenticated_tsv_insert_accepts_reordered_all_type_fields_and_escapes() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE typed_values (id Int64, score Float64, active Bool, label String);")
        .unwrap();
    let reordered_tsv = concat!(
        "label\tactive\tscore\tid\n",
        "slash\\\\tab\\tcarriage\\rline\\nnul\\0backspace\\bformfeed\\fapostrophe\\' snow 雪\ttrue\t1.5\t-9223372036854775808\n",
    )
    .as_bytes();
    let (request, _) = request_with_authorization_for_target(
        "/insert/typed_values",
        reordered_tsv,
        "Authorization: Bearer correct-token\r\n\
         X-ClickHouse-Format: TabSeparatedWithNames\r\n",
    );

    assert_response_with_content_type(
        &authenticated_exchange(&database, "correct-token", &request),
        "HTTP/1.1 200 OK",
        "text/plain; charset=utf-8",
        b"",
    );
    let query = request_for_target_with_headers(
        "/query",
        b"SELECT id, score, active, label FROM typed_values;",
        "X-ClickHouse-Format: TabSeparatedWithNames\r\n",
    );
    assert_response_with_content_type(
        &exchange(&database, &query),
        "HTTP/1.1 200 OK",
        "text/tab-separated-values; charset=utf-8",
        concat!(
            "id\tscore\tactive\tlabel\n",
            "-9223372036854775808\t1.5\ttrue\tslash\\\\tab\\tcarriage\\rline\\nnul\\0backspace\\bformfeed\\fapostrophe\\' snow 雪\n",
        )
        .as_bytes(),
    );
}

#[test]
fn clickhouse_key_authenticated_tsv_insert_uses_the_selected_importer() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE events (id Int64, label String);")
        .unwrap();
    let request = request_for_target_with_headers(
        "/insert/events",
        b"id\tlabel\r\n7\tkey-authenticated\r\n",
        "x-cLiCkHoUsE-kEy: correct key:42\r\n\
         X-ClickHouse-Format: TabSeparatedWithNames\r\n",
    );

    let response = clickhouse_key_exchange(&database, "correct key:42", &request);
    assert_response_with_content_type(
        &response,
        "HTTP/1.1 200 OK",
        "text/plain; charset=utf-8",
        b"",
    );
    assert_clickhouse_key_response_is_not_cacheable(&response);
    assert_response(
        &exchange(
            &database,
            &request_for_target("/query", b"SELECT id, label FROM events;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"},{"name":"label","type":"String"}],"rows":[[7,"key-authenticated"]]}"#,
    );
}

#[test]
fn table_insert_authentication_precedes_exact_format_validation_and_body_reads() {
    let database = SharedDatabase::default();
    database.execute("CREATE TABLE events (id Int64);").unwrap();
    let malformed_tsv = b"id\ninvalid\\escape\n";
    let invalid_format = "X-ClickHouse-Format: tabseparatedwithnames\r\n";

    let (bearer_request, bearer_body_offset) =
        request_with_authorization_for_target("/insert/events", malformed_tsv, invalid_format);
    let mut bearer_input = Cursor::new(bearer_request);
    let mut bearer_response = Vec::new();
    handle_http_query_with_bearer_token(
        &database,
        "correct-token",
        &mut bearer_input,
        &mut bearer_response,
    )
    .unwrap();
    assert_eq!(bearer_input.position(), bearer_body_offset);
    assert_response(
        &bearer_response,
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"bearer authentication required"}"#,
    );

    let (key_request, key_body_offset) =
        request_with_authorization_for_target("/insert/events", malformed_tsv, invalid_format);
    let mut key_input = Cursor::new(key_request);
    let mut key_response = Vec::new();
    handle_http_query_with_clickhouse_key(
        &database,
        "correct-key",
        &mut key_input,
        &mut key_response,
    )
    .unwrap();
    assert_eq!(key_input.position(), key_body_offset);
    assert_response(
        &key_response,
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"X-ClickHouse-Key authentication required"}"#,
    );

    let (authorized, authorized_body_offset) = request_with_authorization_for_target(
        "/insert/events",
        malformed_tsv,
        "Authorization: Bearer correct-token\r\n\
         X-ClickHouse-Format: tabseparatedwithnames\r\n",
    );
    let mut authorized_input = Cursor::new(authorized);
    let mut authorized_response = Vec::new();
    handle_http_query_with_bearer_token(
        &database,
        "correct-token",
        &mut authorized_input,
        &mut authorized_response,
    )
    .unwrap();
    assert_eq!(authorized_input.position(), authorized_body_offset);
    assert_response(
        &authorized_response,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"unsupported X-ClickHouse-Format header"}"#,
    );
}

#[test]
fn tsv_insert_reports_late_malformed_input_and_rolls_back_every_row() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE events (id Int64, label String); \
             INSERT INTO events VALUES (9, 'existing');",
        )
        .unwrap();
    let (request, _) = request_with_authorization_for_target(
        "/insert/events",
        b"id\tlabel\n1\tvalid\n2\tbad\\x\n",
        "Authorization: Bearer correct-token\r\n\
         X-ClickHouse-Format: TabSeparatedWithNames\r\n",
    );

    assert_response(
        &authenticated_exchange(&database, "correct-token", &request),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"database TSV ingestion failed: TSV field at line 3, column 2 contains an invalid backslash escape"}"#,
    );
    assert_response(
        &exchange(
            &database,
            &request_for_target("/query", b"SELECT id, label FROM events;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"},{"name":"label","type":"String"}],"rows":[[9,"existing"]]}"#,
    );
}

#[test]
fn table_insert_uses_independent_exact_csv_and_tsv_limits() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE tsv_events (id Int64, label String); \
             CREATE TABLE csv_events (id Int64, label String); \
             CREATE TABLE bounded_events (id Int64, label String);",
        )
        .unwrap();
    let tsv = b"id\tlabel\n1\tone\n2\ttwo\n";
    let (tsv_request, _) = request_with_authorization_for_target(
        "/insert/tsv_events",
        tsv,
        "Authorization: Bearer correct-token\r\n\
         X-ClickHouse-Format: TabSeparatedWithNames\r\n",
    );
    let mut response = Vec::new();
    handle_http_query_with_bearer_token_and_limits(
        &database,
        "correct-token",
        Cursor::new(&tsv_request),
        &mut response,
        HttpQueryLimits {
            csv_ingest_limits: CsvIngestLimits::new(0, 0, 0),
            tsv_ingest_limits: TsvIngestLimits::new(tsv.len(), 2, 4),
            ..HttpQueryLimits::default()
        },
    )
    .unwrap();
    assert_response_with_content_type(
        &response,
        "HTTP/1.1 200 OK",
        "text/plain; charset=utf-8",
        b"",
    );

    let csv = b"id,label\n3,three\n";
    let (csv_request, _) = request_with_authorization_for_target(
        "/insert/csv_events",
        csv,
        "Authorization: Bearer correct-token\r\n",
    );
    let mut response = Vec::new();
    handle_http_query_with_bearer_token_and_limits(
        &database,
        "correct-token",
        Cursor::new(csv_request),
        &mut response,
        HttpQueryLimits {
            csv_ingest_limits: CsvIngestLimits::new(csv.len(), 1, 2),
            tsv_ingest_limits: TsvIngestLimits::new(0, 0, 0),
            ..HttpQueryLimits::default()
        },
    )
    .unwrap();
    assert_response_with_content_type(
        &response,
        "HTTP/1.1 200 OK",
        "text/plain; charset=utf-8",
        b"",
    );

    let (bounded_request, body_offset) = request_with_authorization_for_target(
        "/insert/bounded_events",
        tsv,
        "Authorization: Bearer correct-token\r\n\
         X-ClickHouse-Format: TabSeparatedWithNames\r\n",
    );
    let mut input = Cursor::new(&bounded_request);
    let mut response = Vec::new();
    handle_http_query_with_bearer_token_and_limits(
        &database,
        "correct-token",
        &mut input,
        &mut response,
        HttpQueryLimits {
            tsv_ingest_limits: TsvIngestLimits::new(tsv.len() - 1, 2, 4),
            ..HttpQueryLimits::default()
        },
    )
    .unwrap();
    assert_eq!(input.position(), body_offset);
    assert_response(
        &response,
        "HTTP/1.1 400 Bad Request",
        &format!(
            r#"{{"error":"database TSV ingestion failed: TSV input is {} bytes, exceeding the limit of {} bytes"}}"#,
            tsv.len(),
            tsv.len() - 1,
        ),
    );

    let limit_cases = [
        (
            TsvIngestLimits::new(tsv.len(), 1, 4),
            r#"{"error":"database TSV ingestion failed: TSV record at line 3 raises the row count to 2, exceeding the limit of 1"}"#,
        ),
        (
            TsvIngestLimits::new(tsv.len(), 2, 3),
            r#"{"error":"database TSV ingestion failed: TSV record at line 3 raises the value count to 4, exceeding the limit of 3"}"#,
        ),
    ];
    for (tsv_ingest_limits, expected_body) in limit_cases {
        let mut response = Vec::new();
        handle_http_query_with_bearer_token_and_limits(
            &database,
            "correct-token",
            Cursor::new(&bounded_request),
            &mut response,
            HttpQueryLimits {
                tsv_ingest_limits,
                ..HttpQueryLimits::default()
            },
        )
        .unwrap();
        assert_response(&response, "HTTP/1.1 400 Bad Request", expected_body);
    }

    assert_response(
        &exchange(
            &database,
            &request_for_target("/query", b"SELECT id, label FROM tsv_events ORDER BY id;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"},{"name":"label","type":"String"}],"rows":[[1,"one"],[2,"two"]]}"#,
    );
    assert_response(
        &exchange(
            &database,
            &request_for_target("/query", b"SELECT id, label FROM csv_events;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"},{"name":"label","type":"String"}],"rows":[[3,"three"]]}"#,
    );
    assert_response(
        &exchange(
            &database,
            &request_for_target("/query", b"SELECT id FROM bounded_events;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"}],"rows":[]}"#,
    );
}

#[test]
fn tsv_insert_returns_503_without_waiting_for_a_reader() {
    let mut initial = Database::new();
    initial.execute("CREATE TABLE events (id Int64);").unwrap();
    let inner = Arc::new(RwLock::new(initial));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let mut reader = Some(inner.read().unwrap());
    let (request, _) = request_with_authorization_for_target(
        "/insert/events",
        b"id\n1\n",
        "Authorization: Bearer correct-token\r\n\
         X-ClickHouse-Format: TabSeparatedWithNames\r\n",
    );
    let (sender, receiver) = mpsc::channel();
    let worker_database = database.clone();
    let worker = thread::spawn(move || {
        sender
            .send(authenticated_exchange(
                &worker_database,
                "correct-token",
                &request,
            ))
            .unwrap();
    });

    let response = match receiver.recv_timeout(Duration::from_secs(2)) {
        Ok(response) => response,
        Err(error) => {
            drop(reader.take());
            worker.join().unwrap();
            panic!("HTTP TSV admission blocked behind a reader: {error}");
        }
    };
    assert_response(
        &response,
        "HTTP/1.1 503 Service Unavailable",
        r#"{"error":"database is unavailable"}"#,
    );
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
fn csv_insert_reports_typed_malformed_errors_and_rolls_back_late_rows() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE metrics (id Int64, score Float64, active Bool, label String); \
             INSERT INTO metrics VALUES (9, 9.0, true, 'existing');",
        )
        .unwrap();
    let cases: &[(&[u8], &str)] = &[
        (
            b"id,score,active,label\n1,1.5,true,valid\n2,2.5,false,\"unclosed\n",
            r#"{"error":"database CSV ingestion failed: CSV field at line 3, column 4 has malformed quoting"}"#,
        ),
        (
            b"id,score,active,label\n1,1.5,true,valid\n2,NaN,false,late\n",
            r#"{"error":"database CSV ingestion failed: CSV field at line 3, column 2 is not a valid Float64"}"#,
        ),
    ];

    for (csv, expected_body) in cases {
        let (request, _) = request_with_authorization_for_target(
            "/insert/metrics",
            csv,
            "Authorization: Bearer correct-token\r\n",
        );
        assert_response(
            &authenticated_exchange(&database, "correct-token", &request),
            "HTTP/1.1 400 Bad Request",
            expected_body,
        );
    }

    assert_response(
        &exchange(
            &database,
            &request_for_target("/query", b"SELECT id, label FROM metrics;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"},{"name":"label","type":"String"}],"rows":[[9,"existing"]]}"#,
    );
}

#[test]
fn csv_insert_is_authenticated_and_requires_an_exact_table_target() {
    let database = SharedDatabase::default();
    database.execute("CREATE TABLE events (id Int64);").unwrap();
    let csv = b"id\n1\n";

    assert_response(
        &exchange(&database, &request_for_target("/insert/events", csv)),
        "HTTP/1.1 404 Not Found",
        r#"{"error":"request target must be / or /query"}"#,
    );

    let (missing_credentials, body_offset) =
        request_with_authorization_for_target("/insert/events", csv, "");
    let mut input = Cursor::new(missing_credentials);
    let mut response = Vec::new();
    handle_http_query_with_bearer_token(&database, "correct-token", &mut input, &mut response)
        .unwrap();
    assert_eq!(input.position(), body_offset);
    assert_response(
        &response,
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"bearer authentication required"}"#,
    );

    for target in ["/insert/events/", "/insert/events?async=1", "/insert/"] {
        let (request, _) = request_with_authorization_for_target(
            target,
            csv,
            "Authorization: Bearer correct-token\r\n",
        );
        assert_response(
            &authenticated_exchange(&database, "correct-token", &request),
            "HTTP/1.1 404 Not Found",
            r#"{"error":"request target must be / or /query"}"#,
        );
    }

    let wrong_method = authenticated_exchange(
        &database,
        "correct-token",
        b"GET /insert/events HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer correct-token\r\n\r\n",
    );
    assert_response(
        &wrong_method,
        "HTTP/1.1 405 Method Not Allowed",
        r#"{"error":"method must be POST for /insert/<table>"}"#,
    );
    assert_response_header(&wrong_method, "Allow: POST");
}

#[test]
fn csv_insert_preserves_http_and_csv_ingest_limits_without_partial_rows() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE events (id Int64, label String);")
        .unwrap();
    let csv = b"id,label\n1,one\n2,two\n";
    let (request, body_offset) = request_with_authorization_for_target(
        "/insert/events",
        csv,
        "Authorization: Bearer correct-token\r\n",
    );

    let mut input = Cursor::new(&request);
    let mut response = Vec::new();
    handle_http_query_with_bearer_token_and_limits(
        &database,
        "correct-token",
        &mut input,
        &mut response,
        HttpQueryLimits {
            max_sql_bytes: csv.len() - 1,
            ..HttpQueryLimits::default()
        },
    )
    .unwrap();
    assert_eq!(input.position(), body_offset);
    assert_response(
        &response,
        "HTTP/1.1 413 Payload Too Large",
        r#"{"error":"request body exceeds configured byte limit"}"#,
    );

    let mut input = Cursor::new(&request);
    let mut response = Vec::new();
    handle_http_query_with_bearer_token_and_limits(
        &database,
        "correct-token",
        &mut input,
        &mut response,
        HttpQueryLimits {
            csv_ingest_limits: CsvIngestLimits::new(csv.len() - 1, 2, 4),
            ..HttpQueryLimits::default()
        },
    )
    .unwrap();
    assert_eq!(
        input.position(),
        body_offset,
        "the CSV byte cap must be checked before reading the body"
    );
    assert_response(
        &response,
        "HTTP/1.1 400 Bad Request",
        &format!(
            r#"{{"error":"database CSV ingestion failed: CSV input is {} bytes, exceeding the limit of {} bytes"}}"#,
            csv.len(),
            csv.len() - 1,
        ),
    );

    let csv_limit_cases = [
        (
            CsvIngestLimits::new(csv.len(), 1, 4),
            r#"{"error":"database CSV ingestion failed: CSV record at line 3 raises the row count to 2, exceeding the limit of 1"}"#.to_owned(),
        ),
        (
            CsvIngestLimits::new(csv.len(), 2, 3),
            r#"{"error":"database CSV ingestion failed: CSV record at line 3 raises the value count to 4, exceeding the limit of 3"}"#.to_owned(),
        ),
    ];

    for (csv_ingest_limits, expected_body) in csv_limit_cases {
        let mut response = Vec::new();
        handle_http_query_with_bearer_token_and_limits(
            &database,
            "correct-token",
            Cursor::new(&request),
            &mut response,
            HttpQueryLimits {
                csv_ingest_limits,
                ..HttpQueryLimits::default()
            },
        )
        .unwrap();
        assert_response(&response, "HTTP/1.1 400 Bad Request", &expected_body);
    }

    assert_response(
        &exchange(
            &database,
            &request_for_target("/query", b"SELECT id FROM events;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"}],"rows":[]}"#,
    );
}

#[test]
fn csv_insert_returns_503_without_waiting_for_a_reader() {
    let mut initial = Database::new();
    initial.execute("CREATE TABLE events (id Int64);").unwrap();
    let inner = Arc::new(RwLock::new(initial));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let mut reader = Some(inner.read().unwrap());
    let (request, _) = request_with_authorization_for_target(
        "/insert/events",
        b"id\n1\n",
        "Authorization: Bearer correct-token\r\n",
    );
    let (sender, receiver) = mpsc::channel();
    let worker_database = database.clone();
    let worker = thread::spawn(move || {
        sender
            .send(authenticated_exchange(
                &worker_database,
                "correct-token",
                &request,
            ))
            .unwrap();
    });

    let response = match receiver.recv_timeout(Duration::from_secs(2)) {
        Ok(response) => response,
        Err(error) => {
            drop(reader.take());
            worker.join().unwrap();
            panic!("HTTP CSV admission blocked behind a reader: {error}");
        }
    };
    assert_response(
        &response,
        "HTTP/1.1 503 Service Unavailable",
        r#"{"error":"database is unavailable"}"#,
    );
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
fn csv_insert_preflights_the_response_cap_before_commit() {
    let (request, _) = request_with_authorization_for_target(
        "/insert/events",
        b"id\n1\n",
        "Authorization: Bearer correct-token\r\n",
    );
    let sizing_database = SharedDatabase::default();
    sizing_database
        .execute("CREATE TABLE events (id Int64);")
        .unwrap();
    let success_response = authenticated_exchange(&sizing_database, "correct-token", &request);

    let database = SharedDatabase::default();
    database.execute("CREATE TABLE events (id Int64);").unwrap();
    let max_response_bytes = success_response.len() - 1;
    let mut response = Vec::new();
    let error = handle_http_query_with_bearer_token_and_limits(
        &database,
        "correct-token",
        Cursor::new(request),
        &mut response,
        HttpQueryLimits {
            max_response_bytes,
            ..HttpQueryLimits::default()
        },
    )
    .expect_err("the fixed success response exceeds the cap");

    assert!(matches!(
        error,
        HttpQueryError::ResponseLimitExceeded { max_bytes, .. }
            if max_bytes == max_response_bytes
    ));
    assert!(response.is_empty());
    assert_response(
        &exchange(
            &database,
            &request_for_target("/query", b"SELECT id FROM events;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"}],"rows":[]}"#,
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
        Cursor::new(request_for_target("/", large_sql.as_bytes())),
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
        Cursor::new(request_for_target("/", b"SELECT 1;")),
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
