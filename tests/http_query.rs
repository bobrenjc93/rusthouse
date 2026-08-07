use std::io::{self, Cursor, Read, Write};
use std::sync::{Arc, RwLock, mpsc};
use std::thread;
use std::time::Duration;

use rusthouse::batch::engine::{Database, QueryResultLimits};
use rusthouse::{
    HttpQueryError, HttpQueryLimits, SharedDatabase, handle_http_query,
    handle_http_query_with_bearer_token, handle_http_query_with_bearer_token_and_limits,
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

fn assert_ok_health_response(response: &[u8]) {
    assert_response_with_content_type(
        response,
        "HTTP/1.1 200 OK",
        "text/plain; charset=utf-8",
        b"Ok.\n",
    );
}

fn metrics_body(tables: usize, columns: usize, retained_rows: usize) -> String {
    format!(
        "# HELP rusthouse_tables Number of tables retained by the database.\n\
         # TYPE rusthouse_tables gauge\n\
         rusthouse_tables {tables}\n\
         # HELP rusthouse_columns Number of columns retained by the database.\n\
         # TYPE rusthouse_columns gauge\n\
         rusthouse_columns {columns}\n\
         # HELP rusthouse_retained_rows Number of rows retained across all tables.\n\
         # TYPE rusthouse_retained_rows gauge\n\
         rusthouse_retained_rows {retained_rows}\n"
    )
}

fn assert_ok_metrics_response(
    response: &[u8],
    tables: usize,
    columns: usize,
    retained_rows: usize,
) {
    assert_response_with_content_type(
        response,
        "HTTP/1.1 200 OK",
        "text/plain; version=0.0.4; charset=utf-8",
        metrics_body(tables, columns, retained_rows).as_bytes(),
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

    assert_ok_metrics_response(&exchange(&database, REQUEST), 0, 0, 0);
    database
        .execute(
            "CREATE TABLE events (id Int64, label String); \
             CREATE TABLE flags (active Bool); \
             INSERT INTO events VALUES (1, 'one'), (2, 'two'); \
             INSERT INTO flags VALUES (true);",
        )
        .unwrap();
    assert_ok_metrics_response(&exchange(&database, REQUEST), 2, 3, 3);

    database
        .execute("TRUNCATE TABLE events; DROP TABLE flags;")
        .unwrap();
    assert_ok_metrics_response(&exchange(&database, REQUEST), 1, 2, 0);
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
        r#"{"error":"read-only query accepts only SELECT, SHOW TABLES, SHOW CREATE TABLE, DESCRIBE TABLE, or EXISTS TABLE; found DROP TABLE"}"#,
    );
    assert_response(
        &exchange(
            &database,
            b"GET /?query=DROP+TABLE+retained%3B HTTP/1.1\r\nHost: localhost\r\n\r\n",
        ),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"read-only query accepts only SELECT, SHOW TABLES, SHOW CREATE TABLE, DESCRIBE TABLE, or EXISTS TABLE; found DROP TABLE"}"#,
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
