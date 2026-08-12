use std::io::{self, Cursor, Read, Write};
use std::num::NonZeroUsize;
use std::sync::{Arc, RwLock, mpsc};
use std::thread;
use std::time::Duration;

use rusthouse::batch::csv::CsvIngestLimits;
use rusthouse::batch::engine::{
    Database, ESTIMATED_GROUP_KEY_CELL_BYTES, LENGTH_UTF8_ORDERING_CACHE_ENTRY_BYTES,
    QueryResultLimits, ROW_NUMBER_ORDERING_STATE_ENTRY_BYTES, ResultColumn,
    STRING_TO_FLOAT64_ORDERING_CACHE_ENTRY_BYTES,
};
use rusthouse::batch::error::Error;
use rusthouse::batch::json_compact_each_row::JsonCompactEachRowIngestLimits;
use rusthouse::batch::tsv::TsvIngestLimits;
use rusthouse::batch::value::Value;
use rusthouse::{
    DEFAULT_GLOBAL_AGGREGATE_WORKER_CAP, HttpQueryError, HttpQueryLimits,
    Int64MinMaxIndexAdmission, Int64MinMaxIndexLimits, SharedDatabase, SharedDatabaseError,
    handle_http_query, handle_http_query_read_only_with_bearer_token,
    handle_http_query_read_only_with_bearer_token_and_limits,
    handle_http_query_read_only_with_clickhouse_key,
    handle_http_query_read_only_with_clickhouse_key_and_limits,
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

fn read_only_bearer_exchange(database: &SharedDatabase, token: &str, request: &[u8]) -> Vec<u8> {
    let mut response = Vec::new();
    handle_http_query_read_only_with_bearer_token(
        database,
        token,
        Cursor::new(request),
        &mut response,
    )
    .expect("read-only bearer-authenticated exchange succeeds");
    response
}

fn read_only_clickhouse_key_exchange(
    database: &SharedDatabase,
    key: &str,
    request: &[u8],
) -> Vec<u8> {
    let mut response = Vec::new();
    handle_http_query_read_only_with_clickhouse_key(
        database,
        key,
        Cursor::new(request),
        &mut response,
    )
    .expect("read-only ClickHouse-key-authenticated exchange succeeds");
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

fn aggregate_state_bytes_required(response: &[u8], limit: usize) -> usize {
    let response = std::str::from_utf8(response).expect("response is UTF-8");
    let (headers, body) = response
        .split_once("\r\n\r\n")
        .expect("response has an empty header line");
    assert_eq!(headers.lines().next(), Some("HTTP/1.1 400 Bad Request"));
    let prefix = r#"{"error":"SELECT aggregate state bytes requires at least "#;
    let suffix = format!(r#", exceeding the limit of {limit}"}}"#);
    body.strip_prefix(prefix)
        .and_then(|body| body.strip_suffix(&suffix))
        .expect("response reports the aggregate-state byte requirement")
        .parse()
        .expect("aggregate-state byte requirement is decimal")
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

#[derive(Clone, Copy)]
struct ExpectedMetrics<'a> {
    tables: usize,
    columns: usize,
    retained_rows: usize,
    retained_value_bytes: usize,
    global_aggregate_worker_cap: usize,
    index_pruning: (usize, usize),
    table_metrics: &'a [(&'a str, usize, usize)],
}

impl<'a> ExpectedMetrics<'a> {
    fn new(
        tables: usize,
        columns: usize,
        retained_rows: usize,
        retained_value_bytes: usize,
        table_metrics: &'a [(&'a str, usize, usize)],
    ) -> Self {
        Self {
            tables,
            columns,
            retained_rows,
            retained_value_bytes,
            global_aggregate_worker_cap: DEFAULT_GLOBAL_AGGREGATE_WORKER_CAP,
            index_pruning: (0, 0),
            table_metrics,
        }
    }
}

fn metrics_body(expected: ExpectedMetrics<'_>) -> String {
    let ExpectedMetrics {
        tables,
        columns,
        retained_rows,
        retained_value_bytes,
        global_aggregate_worker_cap,
        index_pruning: (index_scanned_blocks, index_pruned_blocks),
        table_metrics,
    } = expected;
    let mut body = format!(
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
         rusthouse_retained_value_bytes {retained_value_bytes}\n\
         # HELP rusthouse_global_aggregate_worker_cap Configured computation-lane cap for supported aggregate queries.\n\
         # TYPE rusthouse_global_aggregate_worker_cap gauge\n\
         rusthouse_global_aggregate_worker_cap {global_aggregate_worker_cap}\n\
         # HELP rusthouse_index_scanned_blocks Sparse-index blocks selected for exact evaluation by indexed query attempts.\n\
         # TYPE rusthouse_index_scanned_blocks counter\n\
         rusthouse_index_scanned_blocks {index_scanned_blocks}\n\
         # HELP rusthouse_index_pruned_blocks Sparse-index blocks rejected using metadata by indexed query attempts.\n\
         # TYPE rusthouse_index_pruned_blocks counter\n\
         rusthouse_index_pruned_blocks {index_pruned_blocks}\n\
         # HELP rusthouse_table_rows Number of rows retained by a table.\n\
         # TYPE rusthouse_table_rows gauge\n"
    );
    for (table, rows, _) in table_metrics {
        body.push_str(&format!(
            "rusthouse_table_rows{{table=\"{table}\"}} {rows}\n"
        ));
    }
    body.push_str(
        "# HELP rusthouse_table_retained_value_bytes Scalar payload bytes retained by a table.\n\
         # TYPE rusthouse_table_retained_value_bytes gauge\n",
    );
    for (table, _, retained_value_bytes) in table_metrics {
        body.push_str(&format!(
            "rusthouse_table_retained_value_bytes{{table=\"{table}\"}} {retained_value_bytes}\n"
        ));
    }
    body
}

fn assert_ok_metrics_response(
    response: &[u8],
    tables: usize,
    columns: usize,
    retained_rows: usize,
    retained_value_bytes: usize,
    table_metrics: &[(&str, usize, usize)],
) {
    assert_ok_metrics_response_with_expectation(
        response,
        ExpectedMetrics::new(
            tables,
            columns,
            retained_rows,
            retained_value_bytes,
            table_metrics,
        ),
    );
}

fn assert_ok_metrics_response_with_worker_cap(
    response: &[u8],
    tables: usize,
    columns: usize,
    retained_rows: usize,
    retained_value_bytes: usize,
    global_aggregate_worker_cap: usize,
    table_metrics: &[(&str, usize, usize)],
) {
    assert_ok_metrics_response_with_expectation(
        response,
        ExpectedMetrics {
            global_aggregate_worker_cap,
            ..ExpectedMetrics::new(
                tables,
                columns,
                retained_rows,
                retained_value_bytes,
                table_metrics,
            )
        },
    );
}

fn assert_ok_metrics_response_with_index_counters(
    response: &[u8],
    tables: usize,
    columns: usize,
    retained_rows: usize,
    retained_value_bytes: usize,
    index_pruning: (usize, usize),
    table_metrics: &[(&str, usize, usize)],
) {
    assert_ok_metrics_response_with_expectation(
        response,
        ExpectedMetrics {
            index_pruning,
            ..ExpectedMetrics::new(
                tables,
                columns,
                retained_rows,
                retained_value_bytes,
                table_metrics,
            )
        },
    );
}

fn assert_ok_metrics_response_with_expectation(response: &[u8], expected: ExpectedMetrics<'_>) {
    assert_response_with_content_type(
        response,
        "HTTP/1.1 200 OK",
        "text/plain; version=0.0.4; charset=utf-8",
        metrics_body(expected).as_bytes(),
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
fn query_executes_filtered_count_if_over_http() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE events (active Bool, included Bool); \
             INSERT INTO events VALUES \
                 (true, true), (false, true), (true, false), (true, true);",
        )
        .expect("setup");

    assert_response(
        &exchange(
            &database,
            &request(b"SELECT countIf(active) AS true_count FROM events WHERE included = true;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"true_count","type":"Int64"}],"rows":[[2]]}"#,
    );
}

#[test]
fn query_executes_to_string_projection_over_http() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE readings (id Int64, reading Float64, active Bool, label String); \
             INSERT INTO readings VALUES \
             (1, -0.0, false, 'first'), (2, 12.5, true, 'second');",
        )
        .expect("setup");

    assert_response(
        &exchange(
            &database,
            &request(
                b"SELECT toString(id) AS id_text, TOSTRING(reading) AS reading_text, \
                         ToStRiNg(active) AS active_text, tostring(label) AS label_text \
                  FROM readings WHERE id >= 2 ORDER BY toString(id) LIMIT 1;",
            ),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id_text","type":"String"},{"name":"reading_text","type":"String"},{"name":"active_text","type":"String"},{"name":"label_text","type":"String"}],"rows":[["2","12.5","true","second"]]}"#,
    );
}

#[test]
fn query_propagates_nullable_int64_through_to_string_over_http() {
    let mut inner = Database::new();
    inner
        .create_nullable_int64_table(
            "optional_readings",
            "value",
            vec![Some(2), None, Some(10), None, Some(-1)],
        )
        .expect("setup");
    let database = SharedDatabase::new(inner);

    assert_response(
        &exchange(
            &database,
            &request(
                b"SELECT toString(value) AS rendered FROM optional_readings \
                  ORDER BY toString(value) LIMIT 3 OFFSET 1;",
            ),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"rendered","type":"String"}],"rows":[[null],["-1"],["10"]]}"#,
    );
}

#[test]
fn query_executes_empty_count_over_http_and_preserves_its_name() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE events (kind String, included Bool); \
             INSERT INTO events VALUES ('a', true), ('a', true), ('b', true);",
        )
        .expect("setup");

    assert_response(
        &exchange(
            &database,
            &request(b"SELECT cOuNt() FROM events WHERE included = true;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"COUNT()","type":"Int64"}],"rows":[[3]]}"#,
    );

    assert_response(
        &exchange(
            &database,
            &request(
                b"SELECT COUNT() AS matches FROM events WHERE included = true \
                  HAVING matches = 3 ORDER BY matches DESC LIMIT 1 OFFSET 0;",
            ),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"matches","type":"Int64"}],"rows":[[3]]}"#,
    );
}

#[test]
fn query_executes_unicode_infix_not_like_over_http() {
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
                "SELECT label FROM events WHERE label NOT LIKE '%東京' ORDER BY label;".as_bytes(),
            ),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"label","type":"String"}],"rows":[["Tokyo"],["東京駅"]]}"#,
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
fn query_accepts_clickhouse_comma_limit_pagination_over_http() {
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
                  ORDER BY label LIMIT 1, 1;",
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
fn string_to_bool_cast_is_visible_in_every_http_query_format() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE flags (text String); \
             INSERT INTO flags VALUES ('TRUE'), ('false'), ('FaLsE'), ('tRuE');",
        )
        .expect("setup");
    let sql = b"SELECT CAST(text AS Bool) AS enabled FROM flags ORDER BY enabled;";

    assert_response(
        &exchange(&database, &request(sql)),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"enabled","type":"Bool"}],"rows":[[false],[false],[true],[true]]}"#,
    );

    let csv =
        request_for_target_with_headers("/query", sql, "X-ClickHouse-Format: CSVWithNames\r\n");
    assert_response_with_content_type(
        &exchange(&database, &csv),
        "HTTP/1.1 200 OK",
        "text/csv; charset=utf-8",
        b"enabled\nfalse\nfalse\ntrue\ntrue\n",
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
        b"enabled\nfalse\nfalse\ntrue\ntrue\n",
    );

    for (format, expected) in [
        (
            "JSONEachRow",
            "{\"enabled\":false}\n{\"enabled\":false}\n{\"enabled\":true}\n{\"enabled\":true}\n",
        ),
        ("JSONCompactEachRow", "[false]\n[false]\n[true]\n[true]\n"),
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
fn string_to_int64_cast_is_visible_in_every_http_query_format() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE readings (value String); \
             INSERT INTO readings VALUES ('2'), ('-10'), ('+0');",
        )
        .expect("setup");
    let sql = b"SELECT CAST(value AS Int64) AS converted FROM readings ORDER BY converted;";

    assert_response(
        &exchange(&database, &request(sql)),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"converted","type":"Int64"}],"rows":[[-10],[0],[2]]}"#,
    );

    let csv =
        request_for_target_with_headers("/query", sql, "X-ClickHouse-Format: CSVWithNames\r\n");
    assert_response_with_content_type(
        &exchange(&database, &csv),
        "HTTP/1.1 200 OK",
        "text/csv; charset=utf-8",
        b"converted\n-10\n0\n2\n",
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
        b"converted\n-10\n0\n2\n",
    );

    for (format, expected) in [
        (
            "JSONEachRow",
            "{\"converted\":-10}\n{\"converted\":0}\n{\"converted\":2}\n",
        ),
        ("JSONCompactEachRow", "[-10]\n[0]\n[2]\n"),
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
fn bool_to_float64_cast_is_visible_in_every_http_query_format() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE flags (enabled Bool); \
             INSERT INTO flags VALUES (true), (false);",
        )
        .expect("setup");
    let sql = b"SELECT CAST(enabled AS Float64) AS enabled_f64 FROM flags ORDER BY enabled_f64;";

    assert_response(
        &exchange(&database, &request(sql)),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"enabled_f64","type":"Float64"}],"rows":[[0.0],[1.0]]}"#,
    );

    let csv =
        request_for_target_with_headers("/query", sql, "X-ClickHouse-Format: CSVWithNames\r\n");
    assert_response_with_content_type(
        &exchange(&database, &csv),
        "HTTP/1.1 200 OK",
        "text/csv; charset=utf-8",
        b"enabled_f64\n0.0\n1.0\n",
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
        b"enabled_f64\n0.0\n1.0\n",
    );

    for (format, expected) in [
        (
            "JSONEachRow",
            "{\"enabled_f64\":0.0}\n{\"enabled_f64\":1.0}\n",
        ),
        ("JSONCompactEachRow", "[0.0]\n[1.0]\n"),
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

    assert_ok_metrics_response(&exchange(&database, REQUEST), 0, 0, 0, 0, &[]);
    database
        .execute(
            "CREATE TABLE zebra (id Int64, score Float64, active Bool, label String); \
             CREATE TABLE Alpha (active Bool);",
        )
        .unwrap();
    assert_ok_metrics_response(
        &exchange(&database, REQUEST),
        2,
        5,
        0,
        0,
        &[("Alpha", 0, 0), ("zebra", 0, 0)],
    );

    database
        .execute(
            "INSERT INTO zebra VALUES (1, 1.5, true, 'one'), (2, 2.5, false, 'two'); \
             INSERT INTO Alpha VALUES (true);",
        )
        .unwrap();
    assert_ok_metrics_response(
        &exchange(&database, REQUEST),
        2,
        5,
        3,
        41,
        &[("Alpha", 1, 1), ("zebra", 2, 40)],
    );

    database
        .execute("ALTER TABLE zebra UPDATE label = 'longer' WHERE id = 2;")
        .unwrap();
    assert_ok_metrics_response(
        &exchange(&database, REQUEST),
        2,
        5,
        3,
        44,
        &[("Alpha", 1, 1), ("zebra", 2, 43)],
    );

    database.execute("DELETE FROM zebra WHERE id = 1;").unwrap();
    assert_ok_metrics_response(
        &exchange(&database, REQUEST),
        2,
        5,
        2,
        24,
        &[("Alpha", 1, 1), ("zebra", 1, 23)],
    );

    database.execute("TRUNCATE TABLE zebra;").unwrap();
    assert_ok_metrics_response(
        &exchange(&database, REQUEST),
        2,
        5,
        1,
        1,
        &[("Alpha", 1, 1), ("zebra", 0, 0)],
    );

    database.execute("DROP TABLE Alpha;").unwrap();
    assert_ok_metrics_response(
        &exchange(&database, REQUEST),
        1,
        4,
        0,
        0,
        &[("zebra", 0, 0)],
    );
}

#[test]
fn metrics_reports_default_and_runtime_updated_worker_caps_without_request_overrides() {
    let database = SharedDatabase::default();
    const METRICS_REQUEST: &[u8] = b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n";

    assert_ok_metrics_response_with_worker_cap(
        &exchange(&database, METRICS_REQUEST),
        0,
        0,
        0,
        0,
        DEFAULT_GLOBAL_AGGREGATE_WORKER_CAP,
        &[],
    );
    assert_response(
        &exchange(
            &database,
            b"GET /?query=SELECT+1%3B&max_threads=1 HTTP/1.1\r\nHost: localhost\r\n\r\n",
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"1","type":"Int64"}],"rows":[[1]]}"#,
    );
    assert_ok_metrics_response_with_worker_cap(
        &exchange(&database, METRICS_REQUEST),
        0,
        0,
        0,
        0,
        DEFAULT_GLOBAL_AGGREGATE_WORKER_CAP,
        &[],
    );

    let updated_cap = NonZeroUsize::new(3).unwrap();
    assert_eq!(
        database
            .try_set_global_aggregate_worker_cap(updated_cap)
            .unwrap()
            .get(),
        DEFAULT_GLOBAL_AGGREGATE_WORKER_CAP
    );
    assert_ok_metrics_response_with_worker_cap(
        &exchange(&database, METRICS_REQUEST),
        0,
        0,
        0,
        0,
        updated_cap.get(),
        &[],
    );
}

#[test]
fn metrics_reports_index_work_from_failed_and_successful_queries() {
    let database = SharedDatabase::default();
    const REQUEST: &[u8] = b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n";

    database
        .execute(
            "CREATE TABLE events (id Int64, key Int64); \
             INSERT INTO events VALUES \
                 (0, 0), (1, 1), (2, 2), (3, 3), \
                 (4, 100), (5, 101), (6, 102), (7, 103), \
                 (8, 200), (9, 201), (10, 202), (11, 203);",
        )
        .unwrap();
    assert!(matches!(
        database
            .create_int64_min_max_index(
                "events",
                "key",
                Int64MinMaxIndexLimits::new(4, 3, usize::MAX),
            )
            .unwrap(),
        Int64MinMaxIndexAdmission::Created(_)
    ));

    assert_ok_metrics_response_with_index_counters(
        &exchange(&database, REQUEST),
        1,
        2,
        12,
        192,
        (0, 0),
        &[("events", 12, 192)],
    );
    assert!(matches!(
        database.query_with_result_limit("SELECT id FROM events WHERE key = 202", 0),
        Err(SharedDatabaseError::Sql(Error::ResultLimitExceeded {
            max_bytes: 0,
            ..
        }))
    ));
    assert_ok_metrics_response_with_index_counters(
        &exchange(&database, REQUEST),
        1,
        2,
        12,
        192,
        (1, 2),
        &[("events", 12, 192)],
    );
    database
        .query("SELECT id FROM events WHERE key = 202")
        .expect("indexed query succeeds");
    assert_ok_metrics_response_with_index_counters(
        &exchange(&database, REQUEST),
        1,
        2,
        12,
        192,
        (2, 4),
        &[("events", 12, 192)],
    );
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
        &[],
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
fn qualified_show_tables_is_read_only_and_rejects_invalid_qualifiers_over_http() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE zebra (id Int64); CREATE TABLE Alpha (id Int64);")
        .unwrap();
    let expected = r#"{"columns":[{"name":"name","type":"String"}],"rows":[["Alpha"],["zebra"]]}"#;

    for sql in [
        &b"SHOW TABLES FROM default;"[..],
        &b"sHoW TaBlEs In DeFaUlT"[..],
    ] {
        assert_response(
            &exchange(&database, &request(sql)),
            "HTTP/1.1 200 OK",
            expected,
        );
    }

    assert_response(
        &exchange(&database, &request(b"SHOW TABLES IN analytics;")),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"SQL error at byte 15: SHOW TABLES supports only the default database; found 'analytics'"}"#,
    );
    assert_response(
        &exchange(&database, &request(b"SHOW TABLES FROM default LIMIT 1;")),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"SQL error at byte 25: unexpected trailing input after SHOW TABLES"}"#,
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
        b"POST /?query=SHOW+TABLES%3B HTTP/1.1\r\nHost: localhost\r\n\r\n".to_vec(),
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
        b"POST /?query=SELECT+value+FROM+readings%3B HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n".to_vec(),
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
fn parameterized_get_and_post_combine_workload_limits_database_and_format_parameters() {
    let database = SharedDatabase::with_query_result_limits(QueryResultLimits {
        max_rows: 2,
        ..QueryResultLimits::default()
    });
    database
        .execute(
            "CREATE TABLE samples (value Int64); \
             INSERT INTO samples VALUES (1), (2), (3);",
        )
        .unwrap();

    let exact_requests: &[(&[u8], &str, &[u8])] = &[
        (
            b"GET /?database=default&max_result_bytes=%31%30%32%34&max_result_rows=%32&max_result_values=%32&max_rows_to_read=%33&max_rows_to_group_by=%33&max_threads=%31&default_format=CSVWithNames&query=SELECT+DISTINCT+value+FROM+samples+ORDER+BY+value+LIMIT+2%3B HTTP/1.1\r\nHost: localhost\r\n\r\n",
            "text/csv; charset=utf-8",
            b"value\n1\n2\n",
        ),
        (
            b"POST /?query=SELECT+DISTINCT+value+FROM+samples+ORDER+BY+value+LIMIT+2%3B&default_format=TabSeparatedWithNames&max_result_rows=2&max_result_values=2&max_rows_to_read=3&max_rows_to_group_by=3&max_threads=2&max_result_bytes=1024&database=default HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
            "text/tab-separated-values; charset=utf-8",
            b"value\n1\n2\n",
        ),
    ];
    for (request, content_type, body) in exact_requests {
        assert_response_with_content_type(
            &exchange(&database, request),
            "HTTP/1.1 200 OK",
            content_type,
            body,
        );
    }

    assert_response(
        &exchange(
            &database,
            b"POST /?query=SELECT+value+FROM+samples+ORDER+BY+value+LIMIT+2%3B&max_result_rows=1 HTTP/1.1\r\nHost: localhost\r\n\r\n",
        ),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"SELECT result rows requires at least 2, exceeding the limit of 1"}"#,
    );
    assert_response(
        &exchange(
            &database,
            b"GET /?max_result_rows=3&query=SELECT+value+FROM+samples+ORDER+BY+value%3B HTTP/1.1\r\nHost: localhost\r\n\r\n",
        ),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"SELECT result rows requires at least 3, exceeding the limit of 2"}"#,
    );
    assert_response(
        &exchange(
            &database,
            b"GET /?max_result_rows=0&query=SELECT+value+FROM+samples+ORDER+BY+value%3B HTTP/1.1\r\nHost: localhost\r\n\r\n",
        ),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"SELECT result rows requires at least 3, exceeding the limit of 2"}"#,
    );
}

#[test]
fn parameterized_max_threads_accepts_get_post_zero_and_numeric_boundary_without_mutating_settings()
{
    let database = SharedDatabase::with_global_aggregate_worker_cap(NonZeroUsize::new(4).unwrap());
    database
        .execute(
            "CREATE TABLE samples (value Int64); \
             INSERT INTO samples VALUES (1), (2), (3);",
        )
        .unwrap();
    let expected = r#"{"columns":[{"name":"SUM(value)","type":"Int64"}],"rows":[[6]]}"#;

    for request in [
        b"GET /?query=SELECT+SUM%28value%29+FROM+samples%3B&max_threads=1 HTTP/1.1\r\nHost: localhost\r\n\r\n".as_slice(),
        b"POST /?max_threads=2&query=SELECT+SUM%28value%29+FROM+samples%3B HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n".as_slice(),
        b"GET /?max_threads=0&query=SELECT+SUM%28value%29+FROM+samples%3B HTTP/1.1\r\nHost: localhost\r\n\r\n".as_slice(),
    ] {
        assert_response(
            &exchange(&database, request),
            "HTTP/1.1 200 OK",
            expected,
        );
    }

    let largest = format!(
        "POST /?query=SELECT+SUM%28value%29+FROM+samples%3B&max_threads={} HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
        usize::MAX,
    );
    assert_response(
        &exchange(&database, largest.as_bytes()),
        "HTTP/1.1 200 OK",
        expected,
    );

    let settings_before = exchange(&database, &request(b"SHOW SETTINGS"));
    let settings_with_request_cap = exchange(
        &database,
        b"GET /?max_threads=1&query=SHOW+SETTINGS HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert_eq!(settings_with_request_cap, settings_before);
    assert_eq!(
        database.global_aggregate_worker_cap().unwrap().get(),
        4,
        "the request cap must not mutate the configured setting"
    );
}

#[test]
fn parameterized_max_threads_rejects_empty_duplicate_malformed_and_overflowing_values() {
    let database = SharedDatabase::default();
    let overflow = (usize::MAX as u128 + 1).to_string();

    for method in ["GET", "POST"] {
        let cases = [
            (
                format!(
                    "{method} /?query=SELECT+1%3B&max_threads= HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"GET query parameters must have nonempty names and values"}"#
                    .replace("GET", method),
            ),
            (
                format!(
                    "{method} /?max_threads=1&query=SELECT+1%3B&max%5Fthreads=2 HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"duplicate max_threads parameter"}"#.to_owned(),
            ),
            (
                format!(
                    "{method} /?query=SELECT+1%3B&max_threads=1.0 HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"max_threads parameter must be a decimal integer"}"#.to_owned(),
            ),
            (
                format!(
                    "{method} /?max_threads={overflow}&query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"max_threads parameter is out of range"}"#.to_owned(),
            ),
        ];
        for (request, expected_body) in cases {
            assert_response(
                &exchange(&database, request.as_bytes()),
                "HTTP/1.1 400 Bad Request",
                &expected_body,
            );
        }
    }
}

#[test]
fn max_threads_composes_with_authentication_and_precedes_lock_admission() {
    let healthy_database = SharedDatabase::default();
    let valid_with_credentials = b"GET /?database=default&max_threads=1&max_result_rows=1&query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer correct-token\r\n\r\n";
    assert_response(
        &authenticated_exchange(&healthy_database, "correct-token", valid_with_credentials),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"1","type":"Int64"}],"rows":[[1]]}"#,
    );

    let poisoned_inner = Arc::new(RwLock::new(Database::new()));
    let poisoned_database = SharedDatabase::from_arc(Arc::clone(&poisoned_inner));
    let poisoner = thread::spawn(move || {
        let _guard = poisoned_inner.write().unwrap();
        panic!("poison the database lock");
    });
    assert!(poisoner.join().is_err());

    let invalid_without_credentials =
        b"GET /?query=SELECT+1%3B&max_threads=nope HTTP/1.1\r\nHost: localhost\r\n\r\n";
    assert_response(
        &authenticated_exchange(
            &poisoned_database,
            "correct-token",
            invalid_without_credentials,
        ),
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"bearer authentication required"}"#,
    );

    let invalid_with_credentials = b"GET /?query=SELECT+1%3B&max_threads=nope&max_result_rows=1 HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer correct-token\r\n\r\n";
    assert_response(
        &authenticated_exchange(
            &poisoned_database,
            "correct-token",
            invalid_with_credentials,
        ),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"max_threads parameter must be a decimal integer"}"#,
    );
}

#[test]
fn concurrent_parameterized_max_threads_requests_are_isolated_and_preserve_settings() {
    use std::sync::Barrier;

    let database = SharedDatabase::with_global_aggregate_worker_cap(NonZeroUsize::new(4).unwrap());
    database
        .execute(
            "CREATE TABLE samples (value Int64); \
             INSERT INTO samples VALUES (1), (2), (3);",
        )
        .unwrap();
    let expected = exchange(
        &database,
        b"GET /?query=SELECT+SUM%28value%29+FROM+samples%3B&max_threads=1 HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    let settings_before = exchange(&database, &request(b"SHOW SETTINGS"));
    let request_count = 8;
    let started = Arc::new(Barrier::new(request_count));
    let handles = (0..request_count)
        .map(|request_index| {
            let database = database.clone();
            let started = Arc::clone(&started);
            thread::spawn(move || {
                let max_threads = match request_index % 4 {
                    0 => "1".to_owned(),
                    1 => "2".to_owned(),
                    2 => "0".to_owned(),
                    _ => usize::MAX.to_string(),
                };
                let request = format!(
                    "GET /?max_threads={max_threads}&max_result_rows=1&query=SELECT+SUM%28value%29+FROM+samples%3B HTTP/1.1\r\nHost: localhost\r\n\r\n"
                );
                started.wait();
                exchange(&database, request.as_bytes())
            })
        })
        .collect::<Vec<_>>();

    for handle in handles {
        assert_eq!(handle.join().expect("request worker joins"), expected);
    }
    assert_eq!(
        exchange(&database, &request(b"SHOW SETTINGS")),
        settings_before
    );
    assert_eq!(database.global_aggregate_worker_cap().unwrap().get(), 4);
}

#[test]
fn parameterized_max_ordering_state_bytes_enforces_every_exact_cache_boundary() {
    let configured_max = 3 * STRING_TO_FLOAT64_ORDERING_CACHE_ENTRY_BYTES;
    let configured_limits = QueryResultLimits {
        max_ordering_state_bytes: configured_max,
        ..QueryResultLimits::default()
    };
    let database = SharedDatabase::with_query_result_limits(configured_limits);
    database
        .execute(
            "CREATE TABLE events (id Int64, rank_key Int64, keep Bool); \
             INSERT INTO events VALUES \
                 (1, 2, true), (2, 1, true), (3, 2, true), (4, 0, false); \
             CREATE TABLE labels (label String, keep Bool); \
             INSERT INTO labels VALUES \
                 ('é', true), ('Z', true), ('東京', true), ('discarded', false); \
             CREATE TABLE readings (id Int64, reading String, keep Bool); \
             INSERT INTO readings VALUES \
                 (1, '2', true), (2, '-0', true), (3, '2.00', true), \
                 (4, 'invalid', false);",
        )
        .unwrap();

    let cases = [
        (
            "SELECT+id%2C+ROW_NUMBER%28%29+OVER+%28ORDER+BY+rank_key+ASC%29+AS+n+FROM+events+WHERE+keep+%3D+true+LIMIT+3%3B",
            3 * ROW_NUMBER_ORDERING_STATE_ENTRY_BYTES,
            r#"{"columns":[{"name":"id","type":"Int64"},{"name":"n","type":"Int64"}],"rows":[[2,1],[1,2],[3,3]]}"#,
        ),
        (
            "SELECT+label%2C+lengthUTF8%28label%29+AS+scalars+FROM+labels+WHERE+keep+%3D+true+ORDER+BY+scalars+ASC+LIMIT+3%3B",
            3 * LENGTH_UTF8_ORDERING_CACHE_ENTRY_BYTES,
            r#"{"columns":[{"name":"label","type":"String"},{"name":"scalars","type":"Int64"}],"rows":[["é",1],["Z",1],["東京",2]]}"#,
        ),
        (
            "SELECT+id%2C+CAST%28reading+AS+Float64%29+AS+converted+FROM+readings+WHERE+keep+%3D+true+ORDER+BY+converted+ASC+LIMIT+3%3B",
            3 * STRING_TO_FLOAT64_ORDERING_CACHE_ENTRY_BYTES,
            r#"{"columns":[{"name":"id","type":"Int64"},{"name":"converted","type":"Float64"}],"rows":[[2,-0.0],[1,2.0],[3,2.0]]}"#,
        ),
    ];

    for method in ["GET", "POST"] {
        for (query, exact_bytes, expected_body) in cases {
            assert!(exact_bytes <= configured_max);
            let exact = format!(
                "{method} /?max_ordering_state_bytes={exact_bytes}&query={query} HTTP/1.1\r\nHost: localhost\r\n\r\n"
            );
            assert_response(
                &exchange(&database, exact.as_bytes()),
                "HTTP/1.1 200 OK",
                expected_body,
            );

            let one_byte_short = exact_bytes - 1;
            let exceeded = format!(
                "{method} /?query={query}&max_ordering_state_bytes={one_byte_short} HTTP/1.1\r\nHost: localhost\r\n\r\n"
            );
            assert_response(
                &exchange(&database, exceeded.as_bytes()),
                "HTTP/1.1 400 Bad Request",
                &format!(
                    r#"{{"error":"SELECT ordering state bytes requires at least {exact_bytes}, exceeding the limit of {one_byte_short}"}}"#
                ),
            );
        }
    }

    let zero_retains_configured_limit = format!(
        "POST /?query={}&max_ordering_state_bytes=0 HTTP/1.1\r\nHost: localhost\r\n\r\n",
        cases[2].0,
    );
    assert_response(
        &exchange(&database, zero_retains_configured_limit.as_bytes()),
        "HTTP/1.1 200 OK",
        cases[2].2,
    );

    let unfiltered_bytes = 4 * STRING_TO_FLOAT64_ORDERING_CACHE_ENTRY_BYTES;
    let cannot_relax_configured_limit = format!(
        "GET /?max_ordering_state_bytes={}&query=SELECT+CAST%28reading+AS+Float64%29+AS+converted+FROM+readings+ORDER+BY+converted+LIMIT+1%3B HTTP/1.1\r\nHost: localhost\r\n\r\n",
        usize::MAX,
    );
    assert_response(
        &exchange(&database, cannot_relax_configured_limit.as_bytes()),
        "HTTP/1.1 400 Bad Request",
        &format!(
            r#"{{"error":"SELECT ordering state bytes requires at least {unfiltered_bytes}, exceeding the limit of {configured_max}"}}"#
        ),
    );
    assert_eq!(database.query_result_limits().unwrap(), configured_limits);
}

#[test]
fn parameterized_max_ordering_state_bytes_rejects_invalid_values() {
    let database = SharedDatabase::default();
    let overflow = (usize::MAX as u128 + 1).to_string();

    for method in ["GET", "POST"] {
        let cases = [
            (
                format!(
                    "{method} /?query=SELECT+1%3B&max_ordering_state_bytes= HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"GET query parameters must have nonempty names and values"}"#
                    .replace("GET", method),
            ),
            (
                format!(
                    "{method} /?max_ordering_state_bytes=1&query=SELECT+1%3B&max%5Fordering%5Fstate%5Fbytes=2 HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"duplicate max_ordering_state_bytes parameter"}"#.to_owned(),
            ),
            (
                format!(
                    "{method} /?query=SELECT+1%3B&max_ordering_state_bytes=1.0 HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"max_ordering_state_bytes parameter must be a decimal integer"}"#
                    .to_owned(),
            ),
            (
                format!(
                    "{method} /?max_ordering_state_bytes={overflow}&query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"max_ordering_state_bytes parameter is out of range"}"#.to_owned(),
            ),
        ];
        for (request, expected_body) in cases {
            assert_response(
                &exchange(&database, request.as_bytes()),
                "HTTP/1.1 400 Bad Request",
                &expected_body,
            );
        }
    }
}

#[test]
fn concurrent_parameterized_max_ordering_state_requests_are_isolated() {
    use std::sync::Barrier;

    let exact_bytes = 3 * LENGTH_UTF8_ORDERING_CACHE_ENTRY_BYTES;
    let configured_limits = QueryResultLimits {
        max_ordering_state_bytes: exact_bytes,
        ..QueryResultLimits::default()
    };
    let database = SharedDatabase::with_query_result_limits(configured_limits);
    database
        .execute(
            "CREATE TABLE labels (label String); \
             INSERT INTO labels VALUES ('é'), ('Z'), ('東京');",
        )
        .unwrap();
    let query = "SELECT+label%2C+lengthUTF8%28label%29+AS+scalars+FROM+labels+ORDER+BY+scalars%3B";
    let success_request = format!(
        "GET /?query={query}&max_ordering_state_bytes={exact_bytes} HTTP/1.1\r\nHost: localhost\r\n\r\n"
    );
    let failure_request = format!(
        "GET /?query={query}&max_ordering_state_bytes={} HTTP/1.1\r\nHost: localhost\r\n\r\n",
        exact_bytes - 1,
    );
    let expected_success = exchange(&database, success_request.as_bytes());
    let expected_failure = exchange(&database, failure_request.as_bytes());

    let request_count = 12;
    let started = Arc::new(Barrier::new(request_count));
    let handles = (0..request_count)
        .map(|request_index| {
            let database = database.clone();
            let started = Arc::clone(&started);
            thread::spawn(move || {
                let requested_max = match request_index % 4 {
                    0 => exact_bytes.to_string(),
                    1 => (exact_bytes - 1).to_string(),
                    2 => "0".to_owned(),
                    _ => usize::MAX.to_string(),
                };
                let request = format!(
                    "GET /?max_ordering_state_bytes={requested_max}&query={query} HTTP/1.1\r\nHost: localhost\r\n\r\n"
                );
                started.wait();
                (request_index % 4, exchange(&database, request.as_bytes()))
            })
        })
        .collect::<Vec<_>>();

    for handle in handles {
        let (case, response) = handle.join().unwrap();
        if case == 1 {
            assert_eq!(response, expected_failure);
        } else {
            assert_eq!(response, expected_success);
        }
    }
    assert_eq!(database.query_result_limits().unwrap(), configured_limits);
}

#[test]
fn parameterized_max_aggregate_state_cells_enforces_global_and_grouped_boundaries() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE samples (g Int64, value Int64); \
             INSERT INTO samples VALUES (1, 10), (2, 20);",
        )
        .unwrap();
    let cases = [
        (
            "SELECT+COUNT%28%2A%29+AS+n%2C+SUM%28value%29+AS+total+FROM+samples%3B",
            r#"{"columns":[{"name":"n","type":"Int64"},{"name":"total","type":"Int64"}],"rows":[[2,30]]}"#,
        ),
        (
            "SELECT+g%2C+COUNT%28%2A%29+AS+n+FROM+samples+GROUP+BY+g+ORDER+BY+g%3B",
            r#"{"columns":[{"name":"g","type":"Int64"},{"name":"n","type":"Int64"}],"rows":[[1,1],[2,1]]}"#,
        ),
    ];

    for method in ["GET", "POST"] {
        for (query, expected_body) in cases {
            let exact = format!(
                "{method} /?max_aggregate_state_cells=2&query={query} HTTP/1.1\r\nHost: localhost\r\n\r\n"
            );
            assert_response(
                &exchange(&database, exact.as_bytes()),
                "HTTP/1.1 200 OK",
                expected_body,
            );

            let one_cell_short = format!(
                "{method} /?query={query}&max_aggregate_state_cells=1 HTTP/1.1\r\nHost: localhost\r\n\r\n"
            );
            assert_response(
                &exchange(&database, one_cell_short.as_bytes()),
                "HTTP/1.1 400 Bad Request",
                r#"{"error":"SELECT aggregate state cells requires at least 2, exceeding the limit of 1"}"#,
            );
        }
    }

    let configured_limits = QueryResultLimits {
        max_aggregate_state_cells: 1,
        ..QueryResultLimits::default()
    };
    let configured = SharedDatabase::with_query_result_limits(configured_limits);
    configured
        .execute(
            "CREATE TABLE samples (g Int64, value Int64); \
             INSERT INTO samples VALUES (1, 10), (2, 20);",
        )
        .unwrap();
    for (method, requested_max) in [
        ("GET", "0".to_owned()),
        ("POST", "2".to_owned()),
        ("GET", usize::MAX.to_string()),
    ] {
        let request = format!(
            "{method} /?query={}&max_aggregate_state_cells={requested_max} HTTP/1.1\r\nHost: localhost\r\n\r\n",
            cases[0].0,
        );
        assert_response(
            &exchange(&configured, request.as_bytes()),
            "HTTP/1.1 400 Bad Request",
            r#"{"error":"SELECT aggregate state cells requires at least 2, exceeding the limit of 1"}"#,
        );
    }
    assert_eq!(configured.query_result_limits().unwrap(), configured_limits);
}

#[test]
fn parameterized_max_aggregate_state_cells_rejects_invalid_values() {
    let database = SharedDatabase::default();
    let overflow = (usize::MAX as u128 + 1).to_string();

    for method in ["GET", "POST"] {
        let cases = [
            (
                format!(
                    "{method} /?query=SELECT+1%3B&max_aggregate_state_cells= HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"GET query parameters must have nonempty names and values"}"#
                    .replace("GET", method),
            ),
            (
                format!(
                    "{method} /?max_aggregate_state_cells=1&query=SELECT+1%3B&max%5Faggregate%5Fstate%5Fcells=2 HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"duplicate max_aggregate_state_cells parameter"}"#.to_owned(),
            ),
            (
                format!(
                    "{method} /?query=SELECT+1%3B&max_aggregate_state_cells=1.0 HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"max_aggregate_state_cells parameter must be a decimal integer"}"#
                    .to_owned(),
            ),
            (
                format!(
                    "{method} /?max_aggregate_state_cells={overflow}&query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"max_aggregate_state_cells parameter is out of range"}"#.to_owned(),
            ),
        ];
        for (request, expected_body) in cases {
            assert_response(
                &exchange(&database, request.as_bytes()),
                "HTTP/1.1 400 Bad Request",
                &expected_body,
            );
        }
    }
}

#[test]
fn concurrent_parameterized_max_aggregate_state_cell_requests_are_isolated() {
    use std::sync::Barrier;

    let configured_limits = QueryResultLimits {
        max_aggregate_state_cells: 2,
        ..QueryResultLimits::default()
    };
    let database = SharedDatabase::with_query_result_limits(configured_limits);
    database
        .execute(
            "CREATE TABLE samples (g Int64); \
             INSERT INTO samples VALUES (1), (2);",
        )
        .unwrap();
    let query = "SELECT+g%2C+COUNT%28%2A%29+AS+n+FROM+samples+GROUP+BY+g+ORDER+BY+g%3B";
    let success_request = format!(
        "GET /?query={query}&max_aggregate_state_cells=2 HTTP/1.1\r\nHost: localhost\r\n\r\n"
    );
    let failure_request = format!(
        "GET /?max_aggregate_state_cells=1&query={query} HTTP/1.1\r\nHost: localhost\r\n\r\n"
    );
    let expected_success = exchange(&database, success_request.as_bytes());
    let expected_failure = exchange(&database, failure_request.as_bytes());

    let request_count = 12;
    let started = Arc::new(Barrier::new(request_count));
    let handles = (0..request_count)
        .map(|request_index| {
            let database = database.clone();
            let started = Arc::clone(&started);
            thread::spawn(move || {
                let requested_max = match request_index % 4 {
                    0 => "2".to_owned(),
                    1 => "1".to_owned(),
                    2 => "0".to_owned(),
                    _ => usize::MAX.to_string(),
                };
                let request = format!(
                    "GET /?max_aggregate_state_cells={requested_max}&query={query} HTTP/1.1\r\nHost: localhost\r\n\r\n"
                );
                started.wait();
                (request_index % 4, exchange(&database, request.as_bytes()))
            })
        })
        .collect::<Vec<_>>();

    for handle in handles {
        let (case, response) = handle.join().unwrap();
        if case == 1 {
            assert_eq!(response, expected_failure);
        } else {
            assert_eq!(response, expected_success);
        }
    }
    assert_eq!(database.query_result_limits().unwrap(), configured_limits);
}

#[test]
fn parameterized_max_aggregate_state_bytes_enforces_fixed_and_dynamic_boundaries() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE samples (g Int64, value String); \
             INSERT INTO samples VALUES (1, 'abcd'), (2, 'wxyz');",
        )
        .unwrap();
    let fixed_query = "SELECT+g%2C+COUNT%28%2A%29+AS+n+FROM+samples+GROUP+BY+g%3B";
    let dynamic_query =
        "SELECT+g%2C+MIN%28value%29+AS+first%2C+MAX%28value%29+AS+last+FROM+samples+GROUP+BY+g%3B";
    let fixed_probe = b"GET /?query=SELECT+g%2C+COUNT%28%2A%29+AS+n+FROM+samples+GROUP+BY+g%3B&max_aggregate_state_bytes=1 HTTP/1.1\r\nHost: localhost\r\n\r\n";
    let fixed_bytes = aggregate_state_bytes_required(&exchange(&database, fixed_probe), 1);
    let dynamic_probe = b"GET /?max_aggregate_state_bytes=1&query=SELECT+g%2C+MIN%28value%29+AS+first%2C+MAX%28value%29+AS+last+FROM+samples+GROUP+BY+g%3B HTTP/1.1\r\nHost: localhost\r\n\r\n";
    let dynamic_fixed_bytes =
        aggregate_state_bytes_required(&exchange(&database, dynamic_probe), 1);
    let dynamic_bytes = dynamic_fixed_bytes + 16;

    for method in ["GET", "POST"] {
        let fixed_exact = format!(
            "{method} /?max_aggregate_state_bytes={fixed_bytes}&query={fixed_query} HTTP/1.1\r\nHost: localhost\r\n\r\n"
        );
        assert_response(
            &exchange(&database, fixed_exact.as_bytes()),
            "HTTP/1.1 200 OK",
            r#"{"columns":[{"name":"g","type":"Int64"},{"name":"n","type":"Int64"}],"rows":[[1,1],[2,1]]}"#,
        );

        let fixed_short = fixed_bytes - 1;
        let fixed_exceeded = format!(
            "{method} /?query={fixed_query}&max_aggregate_state_bytes={fixed_short} HTTP/1.1\r\nHost: localhost\r\n\r\n"
        );
        assert_response(
            &exchange(&database, fixed_exceeded.as_bytes()),
            "HTTP/1.1 400 Bad Request",
            &format!(
                r#"{{"error":"SELECT aggregate state bytes requires at least {fixed_bytes}, exceeding the limit of {fixed_short}"}}"#
            ),
        );

        let dynamic_exact = format!(
            "{method} /?query={dynamic_query}&max_aggregate_state_bytes={dynamic_bytes} HTTP/1.1\r\nHost: localhost\r\n\r\n"
        );
        assert_response(
            &exchange(&database, dynamic_exact.as_bytes()),
            "HTTP/1.1 200 OK",
            r#"{"columns":[{"name":"g","type":"Int64"},{"name":"first","type":"String"},{"name":"last","type":"String"}],"rows":[[1,"abcd","abcd"],[2,"wxyz","wxyz"]]}"#,
        );

        let dynamic_short = dynamic_bytes - 1;
        let dynamic_exceeded = format!(
            "{method} /?max_aggregate_state_bytes={dynamic_short}&query={dynamic_query} HTTP/1.1\r\nHost: localhost\r\n\r\n"
        );
        assert_response(
            &exchange(&database, dynamic_exceeded.as_bytes()),
            "HTTP/1.1 400 Bad Request",
            &format!(
                r#"{{"error":"SELECT aggregate state bytes requires at least {dynamic_bytes}, exceeding the limit of {dynamic_short}"}}"#
            ),
        );
    }

    let configured_limits = QueryResultLimits {
        max_aggregate_state_bytes: dynamic_bytes - 1,
        ..QueryResultLimits::default()
    };
    let configured = SharedDatabase::with_query_result_limits(configured_limits);
    configured
        .execute(
            "CREATE TABLE samples (g Int64, value String); \
             INSERT INTO samples VALUES (1, 'abcd'), (2, 'wxyz');",
        )
        .unwrap();
    for (method, requested_max) in [
        ("GET", "0".to_owned()),
        ("POST", dynamic_bytes.to_string()),
        ("GET", usize::MAX.to_string()),
    ] {
        let request = format!(
            "{method} /?query={dynamic_query}&max_aggregate_state_bytes={requested_max} HTTP/1.1\r\nHost: localhost\r\n\r\n"
        );
        assert_response(
            &exchange(&configured, request.as_bytes()),
            "HTTP/1.1 400 Bad Request",
            &format!(
                r#"{{"error":"SELECT aggregate state bytes requires at least {dynamic_bytes}, exceeding the limit of {}"}}"#,
                dynamic_bytes - 1,
            ),
        );
    }
    assert_eq!(configured.query_result_limits().unwrap(), configured_limits);
}

#[test]
fn parameterized_max_aggregate_state_bytes_rejects_invalid_values() {
    let database = SharedDatabase::default();
    let overflow = (usize::MAX as u128 + 1).to_string();

    for method in ["GET", "POST"] {
        let cases = [
            (
                format!(
                    "{method} /?query=SELECT+1%3B&max_aggregate_state_bytes= HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"GET query parameters must have nonempty names and values"}"#
                    .replace("GET", method),
            ),
            (
                format!(
                    "{method} /?max_aggregate_state_bytes=1&query=SELECT+1%3B&max%5Faggregate%5Fstate%5Fbytes=2 HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"duplicate max_aggregate_state_bytes parameter"}"#.to_owned(),
            ),
            (
                format!(
                    "{method} /?query=SELECT+1%3B&max_aggregate_state_bytes=1.0 HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"max_aggregate_state_bytes parameter must be a decimal integer"}"#
                    .to_owned(),
            ),
            (
                format!(
                    "{method} /?max_aggregate_state_bytes={overflow}&query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"max_aggregate_state_bytes parameter is out of range"}"#.to_owned(),
            ),
        ];
        for (request, expected_body) in cases {
            assert_response(
                &exchange(&database, request.as_bytes()),
                "HTTP/1.1 400 Bad Request",
                &expected_body,
            );
        }
    }
}

#[test]
fn concurrent_parameterized_max_aggregate_state_requests_are_isolated() {
    use std::sync::Barrier;

    let probe = SharedDatabase::default();
    probe
        .execute(
            "CREATE TABLE samples (g Int64, value String); \
             INSERT INTO samples VALUES (1, 'abcd'), (2, 'wxyz');",
        )
        .unwrap();
    let query =
        "SELECT+g%2C+MIN%28value%29+AS+first%2C+MAX%28value%29+AS+last+FROM+samples+GROUP+BY+g%3B";
    let probe_request = format!(
        "GET /?query={query}&max_aggregate_state_bytes=1 HTTP/1.1\r\nHost: localhost\r\n\r\n"
    );
    let exact_bytes =
        aggregate_state_bytes_required(&exchange(&probe, probe_request.as_bytes()), 1) + 16;

    let configured_limits = QueryResultLimits {
        max_aggregate_state_bytes: exact_bytes,
        ..QueryResultLimits::default()
    };
    let database = SharedDatabase::with_query_result_limits(configured_limits);
    database
        .execute(
            "CREATE TABLE samples (g Int64, value String); \
             INSERT INTO samples VALUES (1, 'abcd'), (2, 'wxyz');",
        )
        .unwrap();
    let success_request = format!(
        "GET /?query={query}&max_aggregate_state_bytes={exact_bytes} HTTP/1.1\r\nHost: localhost\r\n\r\n"
    );
    let failure_request = format!(
        "GET /?max_aggregate_state_bytes={}&query={query} HTTP/1.1\r\nHost: localhost\r\n\r\n",
        exact_bytes - 1,
    );
    let expected_success = exchange(&database, success_request.as_bytes());
    let expected_failure = exchange(&database, failure_request.as_bytes());

    let request_count = 12;
    let started = Arc::new(Barrier::new(request_count));
    let handles = (0..request_count)
        .map(|request_index| {
            let database = database.clone();
            let started = Arc::clone(&started);
            thread::spawn(move || {
                let requested_max = match request_index % 4 {
                    0 => exact_bytes.to_string(),
                    1 => (exact_bytes - 1).to_string(),
                    2 => "0".to_owned(),
                    _ => usize::MAX.to_string(),
                };
                let request = format!(
                    "GET /?max_aggregate_state_bytes={requested_max}&query={query} HTTP/1.1\r\nHost: localhost\r\n\r\n"
                );
                started.wait();
                (request_index % 4, exchange(&database, request.as_bytes()))
            })
        })
        .collect::<Vec<_>>();

    for handle in handles {
        let (case, response) = handle.join().unwrap();
        if case == 1 {
            assert_eq!(response, expected_failure);
        } else {
            assert_eq!(response, expected_success);
        }
    }
    assert_eq!(database.query_result_limits().unwrap(), configured_limits);
}

#[test]
fn parameterized_max_result_rows_accepts_zero_and_the_numeric_boundary() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE samples (value Int64); \
             INSERT INTO samples VALUES (1);",
        )
        .unwrap();

    assert_response(
        &exchange(
            &database,
            b"GET /?query=SELECT+value+FROM+samples%3B&max_result_rows=0 HTTP/1.1\r\nHost: localhost\r\n\r\n",
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"value","type":"Int64"}],"rows":[[1]]}"#,
    );
    assert_response(
        &exchange(
            &database,
            b"POST /?max_result_rows=0&query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\n\r\n",
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"1","type":"Int64"}],"rows":[[1]]}"#,
    );

    let largest = format!(
        "GET /?query=SELECT+1%3B&max_result_rows={} HTTP/1.1\r\nHost: localhost\r\n\r\n",
        usize::MAX,
    );
    assert_response(
        &exchange(&database, largest.as_bytes()),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"1","type":"Int64"}],"rows":[[1]]}"#,
    );
}

#[test]
fn parameterized_max_result_rows_rejects_empty_duplicate_malformed_and_overflowing_values() {
    let database = SharedDatabase::default();
    let overflow = (usize::MAX as u128 + 1).to_string();

    for method in ["GET", "POST"] {
        let cases = [
            (
                format!(
                    "{method} /?query=SELECT+1%3B&max_result_rows= HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"GET query parameters must have nonempty names and values"}"#
                    .replace("GET", method),
            ),
            (
                format!(
                    "{method} /?max_result_rows=1&query=SELECT+1%3B&max%5Fresult%5Frows=2 HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"duplicate max_result_rows parameter"}"#.to_owned(),
            ),
            (
                format!(
                    "{method} /?query=SELECT+1%3B&max_result_rows=1.0 HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"max_result_rows parameter must be a decimal integer"}"#.to_owned(),
            ),
            (
                format!(
                    "{method} /?max_result_rows={overflow}&query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"max_result_rows parameter is out of range"}"#.to_owned(),
            ),
        ];
        for (request, expected_body) in cases {
            assert_response(
                &exchange(&database, request.as_bytes()),
                "HTTP/1.1 400 Bad Request",
                &expected_body,
            );
        }
    }
}

#[test]
fn parameterized_max_result_values_enforces_exact_boundaries_before_materialization() {
    let configured_limits = QueryResultLimits {
        max_values: 4,
        ..QueryResultLimits::default()
    };
    let database = SharedDatabase::with_query_result_limits(configured_limits);
    database
        .execute(
            "CREATE TABLE samples (left_value Int64, right_value Int64); \
             INSERT INTO samples VALUES (1, 10), (2, 20); \
             CREATE TABLE invalid_values (value String); \
             INSERT INTO invalid_values VALUES ('bad'), ('worse');",
        )
        .unwrap();
    let expected = r#"{"columns":[{"name":"left_value","type":"Int64"},{"name":"right_value","type":"Int64"}],"rows":[[1,10],[2,20]]}"#;

    for method in ["GET", "POST"] {
        for requested_max in [4, 0] {
            let request = format!(
                "{method} /?query=SELECT+left_value%2C+right_value+FROM+samples%3B&max_result_values={requested_max} HTTP/1.1\r\nHost: localhost\r\n\r\n"
            );
            assert_response(
                &exchange(&database, request.as_bytes()),
                "HTTP/1.1 200 OK",
                expected,
            );
        }

        let below_boundary = format!(
            "{method} /?max_result_values=3&query=SELECT+left_value%2C+right_value+FROM+samples%3B HTTP/1.1\r\nHost: localhost\r\n\r\n"
        );
        assert_response(
            &exchange(&database, below_boundary.as_bytes()),
            "HTTP/1.1 400 Bad Request",
            r#"{"error":"SELECT result values requires at least 4, exceeding the limit of 3"}"#,
        );
    }

    assert_response(
        &exchange(
            &database,
            b"GET /?max_result_values=1&query=SELECT+CAST%28value+AS+Int64%29+FROM+invalid_values%3B HTTP/1.1\r\nHost: localhost\r\n\r\n",
        ),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"SELECT result values requires at least 2, exceeding the limit of 1"}"#,
    );
    assert_eq!(database.query_result_limits().unwrap(), configured_limits);
}

#[test]
fn parameterized_max_result_values_zero_and_larger_values_never_relax_the_configured_limit() {
    let configured_limits = QueryResultLimits {
        max_values: 3,
        ..QueryResultLimits::default()
    };
    let database = SharedDatabase::with_query_result_limits(configured_limits);
    database
        .execute(
            "CREATE TABLE samples (left_value Int64, right_value Int64); \
             INSERT INTO samples VALUES (1, 10), (2, 20);",
        )
        .unwrap();

    for (method, requested_max) in [
        ("GET", "0".to_owned()),
        ("POST", "4".to_owned()),
        ("GET", usize::MAX.to_string()),
    ] {
        let request = format!(
            "{method} /?max_result_values={requested_max}&query=SELECT+left_value%2C+right_value+FROM+samples%3B HTTP/1.1\r\nHost: localhost\r\n\r\n"
        );
        assert_response(
            &exchange(&database, request.as_bytes()),
            "HTTP/1.1 400 Bad Request",
            r#"{"error":"SELECT result values requires at least 4, exceeding the limit of 3"}"#,
        );
    }

    assert_eq!(database.query_result_limits().unwrap(), configured_limits);
}

#[test]
fn parameterized_max_result_values_rejects_empty_duplicate_malformed_and_overflowing_values() {
    let database = SharedDatabase::default();
    let overflow = (usize::MAX as u128 + 1).to_string();

    for method in ["GET", "POST"] {
        let cases = [
            (
                format!(
                    "{method} /?query=SELECT+1%3B&max_result_values= HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"GET query parameters must have nonempty names and values"}"#
                    .replace("GET", method),
            ),
            (
                format!(
                    "{method} /?max_result_values=1&query=SELECT+1%3B&max%5Fresult%5Fvalues=2 HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"duplicate max_result_values parameter"}"#.to_owned(),
            ),
            (
                format!(
                    "{method} /?query=SELECT+1%3B&max_result_values=1.0 HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"max_result_values parameter must be a decimal integer"}"#.to_owned(),
            ),
            (
                format!(
                    "{method} /?max_result_values={overflow}&query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"max_result_values parameter is out of range"}"#.to_owned(),
            ),
        ];
        for (request, expected_body) in cases {
            assert_response(
                &exchange(&database, request.as_bytes()),
                "HTTP/1.1 400 Bad Request",
                &expected_body,
            );
        }
    }
}

#[test]
fn max_result_values_validation_precedes_nonblocking_lock_admission() {
    let contended_inner = Arc::new(RwLock::new(Database::new()));
    let contended_database = SharedDatabase::from_arc(Arc::clone(&contended_inner));
    let mut writer = Some(contended_inner.write().unwrap());
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let invalid = exchange(
            &contended_database,
            b"GET /?query=SELECT+1%3B&max_result_values=-1 HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );
        let valid = exchange(
            &contended_database,
            b"POST /?max_result_values=1&query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );
        sender.send((invalid, valid)).unwrap();
    });
    let (invalid, valid) = match receiver.recv_timeout(Duration::from_secs(2)) {
        Ok(responses) => responses,
        Err(error) => {
            drop(writer.take());
            worker.join().unwrap();
            panic!("max_result_values admission blocked behind a writer: {error}");
        }
    };
    assert_response(
        &invalid,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"max_result_values parameter must be a decimal integer"}"#,
    );
    assert_response(
        &valid,
        "HTTP/1.1 503 Service Unavailable",
        r#"{"error":"database is unavailable"}"#,
    );
    drop(writer.take());
    worker.join().unwrap();
}

#[test]
fn parameterized_max_rows_to_read_enforces_exact_get_and_post_scan_boundaries() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE samples (value Int64); \
             INSERT INTO samples VALUES (1), (2), (3);",
        )
        .unwrap();

    for method in ["GET", "POST"] {
        let exact = format!(
            "{method} /?database=default&query=SELECT+value+FROM+samples+WHERE+value+%3D+1+LIMIT+1%3B&max_rows_to_read=3 HTTP/1.1\r\nHost: localhost\r\n\r\n"
        );
        assert_response(
            &exchange(&database, exact.as_bytes()),
            "HTTP/1.1 200 OK",
            r#"{"columns":[{"name":"value","type":"Int64"}],"rows":[[1]]}"#,
        );

        let exceeded = format!(
            "{method} /?max_rows_to_read=2&query=SELECT+value+FROM+samples+WHERE+value+%3D+1+LIMIT+1%3B HTTP/1.1\r\nHost: localhost\r\n\r\n"
        );
        assert_response(
            &exchange(&database, exceeded.as_bytes()),
            "HTTP/1.1 400 Bad Request",
            r#"{"error":"SELECT scanned rows requires at least 3, exceeding the limit of 2"}"#,
        );
    }
}

#[test]
fn parameterized_max_rows_to_read_zero_and_larger_values_never_relax_defaults() {
    let database = SharedDatabase::with_query_result_limits(QueryResultLimits {
        max_scan_rows: 2,
        ..QueryResultLimits::default()
    });
    database
        .execute(
            "CREATE TABLE samples (value Int64); \
             INSERT INTO samples VALUES (1), (2), (3);",
        )
        .unwrap();

    for (method, requested_max) in [
        ("GET", "0".to_owned()),
        ("POST", "3".to_owned()),
        ("GET", usize::MAX.to_string()),
    ] {
        let request = format!(
            "{method} /?query=SELECT+value+FROM+samples+LIMIT+1%3B&max_rows_to_read={requested_max} HTTP/1.1\r\nHost: localhost\r\n\r\n"
        );
        assert_response(
            &exchange(&database, request.as_bytes()),
            "HTTP/1.1 400 Bad Request",
            r#"{"error":"SELECT scanned rows requires at least 3, exceeding the limit of 2"}"#,
        );
    }

    let default_database = SharedDatabase::default();
    for (method, requested_max) in [("GET", "0".to_owned()), ("POST", usize::MAX.to_string())] {
        let request = format!(
            "{method} /?max_rows_to_read={requested_max}&query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\n\r\n"
        );
        assert_response(
            &exchange(&default_database, request.as_bytes()),
            "HTTP/1.1 200 OK",
            r#"{"columns":[{"name":"1","type":"Int64"}],"rows":[[1]]}"#,
        );
    }
}

#[test]
fn parameterized_max_rows_to_read_rejects_invalid_values_for_get_and_post() {
    let database = SharedDatabase::default();
    let overflow = (usize::MAX as u128 + 1).to_string();

    for method in ["GET", "POST"] {
        let cases = [
            (
                format!(
                    "{method} /?query=SELECT+1%3B&max_rows_to_read= HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"GET query parameters must have nonempty names and values"}"#
                    .replace("GET", method),
            ),
            (
                format!(
                    "{method} /?max_rows_to_read=1&query=SELECT+1%3B&max%5Frows%5Fto%5Fread=2 HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"duplicate max_rows_to_read parameter"}"#.to_owned(),
            ),
            (
                format!(
                    "{method} /?query=SELECT+1%3B&max_rows_to_read=1.0 HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"max_rows_to_read parameter must be a decimal integer"}"#.to_owned(),
            ),
            (
                format!(
                    "{method} /?max_rows_to_read={overflow}&query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"max_rows_to_read parameter is out of range"}"#.to_owned(),
            ),
        ];
        for (request, expected_body) in cases {
            assert_response(
                &exchange(&database, request.as_bytes()),
                "HTTP/1.1 400 Bad Request",
                &expected_body,
            );
        }
    }
}

#[test]
fn max_rows_to_read_validation_follows_authentication_and_precedes_lock_admission() {
    let poisoned_inner = Arc::new(RwLock::new(Database::new()));
    let poisoned_database = SharedDatabase::from_arc(Arc::clone(&poisoned_inner));
    let poisoner = thread::spawn(move || {
        let _guard = poisoned_inner.write().unwrap();
        panic!("poison the database lock");
    });
    assert!(poisoner.join().is_err());

    let invalid_without_credentials =
        b"GET /?query=SELECT+1%3B&max_rows_to_read=nope HTTP/1.1\r\nHost: localhost\r\n\r\n";
    assert_response(
        &authenticated_exchange(
            &poisoned_database,
            "correct-token",
            invalid_without_credentials,
        ),
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"bearer authentication required"}"#,
    );

    assert_response(
        &authenticated_exchange(
            &poisoned_database,
            "correct-token",
            b"GET /?query=SELECT+1%3B&max_rows_to_read=nope HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer correct-token\r\n\r\n",
        ),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"max_rows_to_read parameter must be a decimal integer"}"#,
    );

    let contended_inner = Arc::new(RwLock::new(Database::new()));
    let contended_database = SharedDatabase::from_arc(Arc::clone(&contended_inner));
    let mut writer = Some(contended_inner.write().unwrap());
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let invalid = exchange(
            &contended_database,
            b"GET /?query=SELECT+1%3B&max_rows_to_read=-1 HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );
        let valid = exchange(
            &contended_database,
            b"POST /?max_rows_to_read=1&query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );
        sender.send((invalid, valid)).unwrap();
    });
    let (invalid, valid) = match receiver.recv_timeout(Duration::from_secs(2)) {
        Ok(responses) => responses,
        Err(error) => {
            drop(writer.take());
            worker.join().unwrap();
            panic!("max_rows_to_read admission blocked behind a writer: {error}");
        }
    };
    assert_response(
        &invalid,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"max_rows_to_read parameter must be a decimal integer"}"#,
    );
    assert_response(
        &valid,
        "HTTP/1.1 503 Service Unavailable",
        r#"{"error":"database is unavailable"}"#,
    );
    drop(writer.take());
    worker.join().unwrap();
}

#[test]
fn parameterized_max_rows_to_group_by_enforces_exact_group_by_and_distinct_boundaries() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE samples (value Int64); \
             INSERT INTO samples VALUES (1), (2), (1), (3);",
        )
        .unwrap();

    for method in ["GET", "POST"] {
        let exact_group_by = format!(
            "{method} /?database=default&max_result_rows=3&max_result_bytes=4096&max_rows_to_read=4&max%5Frows%5Fto%5Fgroup%5Fby=%33&default_format=JSON&query=SELECT+value%2C+COUNT%28%2A%29+AS+n+FROM+samples+GROUP+BY+value+ORDER+BY+value%3B HTTP/1.1\r\nHost: localhost\r\n\r\n"
        );
        assert_response(
            &exchange(&database, exact_group_by.as_bytes()),
            "HTTP/1.1 200 OK",
            r#"{"columns":[{"name":"value","type":"Int64"},{"name":"n","type":"Int64"}],"rows":[[1,2],[2,1],[3,1]]}"#,
        );

        let exceeded_group_by = format!(
            "{method} /?query=SELECT+value%2C+COUNT%28%2A%29+AS+n+FROM+samples+GROUP+BY+value%3B&max_rows_to_group_by=2 HTTP/1.1\r\nHost: localhost\r\n\r\n"
        );
        assert_response(
            &exchange(&database, exceeded_group_by.as_bytes()),
            "HTTP/1.1 400 Bad Request",
            r#"{"error":"SELECT groups requires at least 3, exceeding the limit of 2"}"#,
        );

        let exact_distinct = format!(
            "{method} /?max_rows_to_group_by=3&query=SELECT+DISTINCT+value+FROM+samples+ORDER+BY+value%3B HTTP/1.1\r\nHost: localhost\r\n\r\n"
        );
        assert_response(
            &exchange(&database, exact_distinct.as_bytes()),
            "HTTP/1.1 200 OK",
            r#"{"columns":[{"name":"value","type":"Int64"}],"rows":[[1],[2],[3]]}"#,
        );

        let exceeded_distinct = format!(
            "{method} /?query=SELECT+DISTINCT+value+FROM+samples%3B&max_rows_to_group_by=2 HTTP/1.1\r\nHost: localhost\r\n\r\n"
        );
        assert_response(
            &exchange(&database, exceeded_distinct.as_bytes()),
            "HTTP/1.1 400 Bad Request",
            r#"{"error":"SELECT groups requires at least 3, exceeding the limit of 2"}"#,
        );
    }
}

#[test]
fn parameterized_group_cap_is_independent_of_group_by_having_and_distinct_limit() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE samples (value Int64); \
             INSERT INTO samples VALUES (1), (2), (1), (3);",
        )
        .unwrap();

    for (method, query) in [
        (
            "GET",
            "SELECT+value%2C+COUNT%28%2A%29+AS+n+FROM+samples+GROUP+BY+value+HAVING+n+%3E+100+LIMIT+0%3B",
        ),
        (
            "POST",
            "SELECT+DISTINCT+value+FROM+samples+ORDER+BY+value+LIMIT+0%3B",
        ),
    ] {
        let request = format!(
            "{method} /?max_rows_to_group_by=2&query={query} HTTP/1.1\r\nHost: localhost\r\n\r\n"
        );
        assert_response(
            &exchange(&database, request.as_bytes()),
            "HTTP/1.1 400 Bad Request",
            r#"{"error":"SELECT groups requires at least 3, exceeding the limit of 2"}"#,
        );
    }
}

#[test]
fn parameterized_max_rows_to_group_by_zero_and_larger_values_never_relax_defaults() {
    let database = SharedDatabase::with_query_result_limits(QueryResultLimits {
        max_groups: 2,
        ..QueryResultLimits::default()
    });
    database
        .execute(
            "CREATE TABLE samples (value Int64); \
             INSERT INTO samples VALUES (1), (2), (3);",
        )
        .unwrap();

    for (method, requested_max) in [
        ("GET", "0".to_owned()),
        ("POST", "3".to_owned()),
        ("GET", usize::MAX.to_string()),
    ] {
        let request = format!(
            "{method} /?query=SELECT+DISTINCT+value+FROM+samples%3B&max_rows_to_group_by={requested_max} HTTP/1.1\r\nHost: localhost\r\n\r\n"
        );
        assert_response(
            &exchange(&database, request.as_bytes()),
            "HTTP/1.1 400 Bad Request",
            r#"{"error":"SELECT groups requires at least 3, exceeding the limit of 2"}"#,
        );
    }
}

#[test]
fn parameterized_max_rows_to_group_by_rejects_invalid_values_for_get_and_post() {
    let database = SharedDatabase::default();
    let overflow = (usize::MAX as u128 + 1).to_string();

    for method in ["GET", "POST"] {
        let cases = [
            (
                format!(
                    "{method} /?query=SELECT+1%3B&max_rows_to_group_by= HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"GET query parameters must have nonempty names and values"}"#
                    .replace("GET", method),
            ),
            (
                format!(
                    "{method} /?max_rows_to_group_by=1&query=SELECT+1%3B&max%5Frows%5Fto%5Fgroup%5Fby=2 HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"duplicate max_rows_to_group_by parameter"}"#.to_owned(),
            ),
            (
                format!(
                    "{method} /?query=SELECT+1%3B&max_rows_to_group_by=1.0 HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"max_rows_to_group_by parameter must be a decimal integer"}"#
                    .to_owned(),
            ),
            (
                format!(
                    "{method} /?max_rows_to_group_by={overflow}&query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"max_rows_to_group_by parameter is out of range"}"#.to_owned(),
            ),
        ];
        for (request, expected_body) in cases {
            assert_response(
                &exchange(&database, request.as_bytes()),
                "HTTP/1.1 400 Bad Request",
                &expected_body,
            );
        }
    }
}

#[test]
fn parameterized_max_group_key_cells_enforces_group_by_distinct_and_union_boundaries() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE samples (value Int64); \
             CREATE TABLE left_rows (value Int64); \
             CREATE TABLE right_rows (value Int64); \
             INSERT INTO samples VALUES (1), (2), (1), (3); \
             INSERT INTO left_rows VALUES (1), (2); \
             INSERT INTO right_rows VALUES (2), (3);",
        )
        .unwrap();
    let cases = [
        (
            "SELECT+value%2C+COUNT%28%2A%29+AS+n+FROM+samples+GROUP+BY+value+ORDER+BY+value%3B",
            r#"{"columns":[{"name":"value","type":"Int64"},{"name":"n","type":"Int64"}],"rows":[[1,2],[2,1],[3,1]]}"#,
        ),
        (
            "SELECT+DISTINCT+value+FROM+samples+ORDER+BY+value%3B",
            r#"{"columns":[{"name":"value","type":"Int64"}],"rows":[[1],[2],[3]]}"#,
        ),
        (
            "SELECT+value+FROM+left_rows+UNION+DISTINCT+SELECT+value+FROM+right_rows%3B",
            r#"{"columns":[{"name":"value","type":"Int64"}],"rows":[[1],[2],[3]]}"#,
        ),
    ];

    for method in ["GET", "POST"] {
        for (query, expected_body) in cases {
            let exact = format!(
                "{method} /?max%5Fgroup%5Fkey%5Fcells=%33&query={query} HTTP/1.1\r\nHost: localhost\r\n\r\n"
            );
            assert_response(
                &exchange(&database, exact.as_bytes()),
                "HTTP/1.1 200 OK",
                expected_body,
            );

            let one_cell_short = format!(
                "{method} /?query={query}&max_group_key_cells=2 HTTP/1.1\r\nHost: localhost\r\n\r\n"
            );
            assert_response(
                &exchange(&database, one_cell_short.as_bytes()),
                "HTTP/1.1 400 Bad Request",
                r#"{"error":"SELECT group key cells requires at least 3, exceeding the limit of 2"}"#,
            );
        }
    }
}

#[test]
fn parameterized_max_group_key_cells_zero_and_larger_values_never_relax_defaults() {
    let configured_limits = QueryResultLimits {
        max_group_key_cells: 2,
        ..QueryResultLimits::default()
    };
    let database = SharedDatabase::with_query_result_limits(configured_limits);
    database
        .execute(
            "CREATE TABLE samples (value Int64); \
             INSERT INTO samples VALUES (1), (2), (3);",
        )
        .unwrap();

    for (method, requested_max) in [
        ("GET", "0".to_owned()),
        ("POST", "3".to_owned()),
        ("GET", usize::MAX.to_string()),
    ] {
        let request = format!(
            "{method} /?query=SELECT+DISTINCT+value+FROM+samples%3B&max_group_key_cells={requested_max} HTTP/1.1\r\nHost: localhost\r\n\r\n"
        );
        assert_response(
            &exchange(&database, request.as_bytes()),
            "HTTP/1.1 400 Bad Request",
            r#"{"error":"SELECT group key cells requires at least 3, exceeding the limit of 2"}"#,
        );
    }
    assert_eq!(database.query_result_limits().unwrap(), configured_limits);
}

#[test]
fn parameterized_max_group_key_cells_rejects_invalid_values_for_get_and_post() {
    let database = SharedDatabase::default();
    let overflow = (usize::MAX as u128 + 1).to_string();

    for method in ["GET", "POST"] {
        let cases = [
            (
                format!(
                    "{method} /?query=SELECT+1%3B&max_group_key_cells= HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"GET query parameters must have nonempty names and values"}"#
                    .replace("GET", method),
            ),
            (
                format!(
                    "{method} /?max_group_key_cells=1&query=SELECT+1%3B&max%5Fgroup%5Fkey%5Fcells=2 HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"duplicate max_group_key_cells parameter"}"#.to_owned(),
            ),
            (
                format!(
                    "{method} /?query=SELECT+1%3B&max_group_key_cells=1.0 HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"max_group_key_cells parameter must be a decimal integer"}"#.to_owned(),
            ),
            (
                format!(
                    "{method} /?max_group_key_cells={overflow}&query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"max_group_key_cells parameter is out of range"}"#.to_owned(),
            ),
        ];
        for (request, expected_body) in cases {
            assert_response(
                &exchange(&database, request.as_bytes()),
                "HTTP/1.1 400 Bad Request",
                &expected_body,
            );
        }
    }
}

#[test]
fn concurrent_parameterized_max_group_key_cell_requests_are_isolated() {
    use std::sync::Barrier;

    let configured_limits = QueryResultLimits {
        max_group_key_cells: 3,
        ..QueryResultLimits::default()
    };
    let database = SharedDatabase::with_query_result_limits(configured_limits);
    database
        .execute(
            "CREATE TABLE samples (value Int64); \
             INSERT INTO samples VALUES (1), (2), (3);",
        )
        .unwrap();
    let query = "SELECT+DISTINCT+value+FROM+samples+ORDER+BY+value%3B";
    let success_request =
        format!("GET /?query={query}&max_group_key_cells=3 HTTP/1.1\r\nHost: localhost\r\n\r\n");
    let failure_request =
        format!("GET /?max_group_key_cells=2&query={query} HTTP/1.1\r\nHost: localhost\r\n\r\n");
    let expected_success = exchange(&database, success_request.as_bytes());
    let expected_failure = exchange(&database, failure_request.as_bytes());

    let request_count = 12;
    let started = Arc::new(Barrier::new(request_count));
    let handles = (0..request_count)
        .map(|request_index| {
            let database = database.clone();
            let started = Arc::clone(&started);
            thread::spawn(move || {
                let requested_max = match request_index % 4 {
                    0 => "3".to_owned(),
                    1 => "2".to_owned(),
                    2 => "0".to_owned(),
                    _ => usize::MAX.to_string(),
                };
                let request = format!(
                    "GET /?max_group_key_cells={requested_max}&query={query} HTTP/1.1\r\nHost: localhost\r\n\r\n"
                );
                started.wait();
                (request_index % 4, exchange(&database, request.as_bytes()))
            })
        })
        .collect::<Vec<_>>();

    for handle in handles {
        let (case, response) = handle.join().unwrap();
        if case == 1 {
            assert_eq!(response, expected_failure);
        } else {
            assert_eq!(response, expected_success);
        }
    }
    assert_eq!(database.query_result_limits().unwrap(), configured_limits);
}

#[test]
fn parameterized_max_group_key_bytes_enforces_exact_utf8_and_composite_boundaries() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE labels (label String); \
             CREATE TABLE tuples (value Int64, label String, active Bool); \
             CREATE TABLE left_tuples (value Int64, label String, active Bool); \
             CREATE TABLE right_tuples (value Int64, label String, active Bool); \
             INSERT INTO labels VALUES ('é'), ('雪'), ('é'); \
             INSERT INTO tuples VALUES (1, 'é', true), (2, '雪', false), \
                 (1, 'é', true), (3, 'é', false); \
             INSERT INTO left_tuples VALUES (1, 'é', true), (2, '雪', false); \
             INSERT INTO right_tuples VALUES (2, '雪', false), (3, 'é', false);",
        )
        .unwrap();
    let string_key_bytes = 2 * ESTIMATED_GROUP_KEY_CELL_BYTES;
    let composite_key_bytes = 12 * ESTIMATED_GROUP_KEY_CELL_BYTES;
    let cases = [
        (
            "SELECT+label%2C+COUNT%28%2A%29+AS+n+FROM+labels+GROUP+BY+label+ORDER+BY+label%3B",
            string_key_bytes,
            r#"{"columns":[{"name":"label","type":"String"},{"name":"n","type":"Int64"}],"rows":[["é",2],["雪",1]]}"#,
        ),
        (
            "SELECT+DISTINCT+value%2C+label%2C+active+FROM+tuples+ORDER+BY+value%3B",
            composite_key_bytes,
            r#"{"columns":[{"name":"value","type":"Int64"},{"name":"label","type":"String"},{"name":"active","type":"Bool"}],"rows":[[1,"é",true],[2,"雪",false],[3,"é",false]]}"#,
        ),
        (
            "SELECT+value%2C+label%2C+active+FROM+left_tuples+UNION+DISTINCT+SELECT+value%2C+label%2C+active+FROM+right_tuples%3B",
            composite_key_bytes,
            r#"{"columns":[{"name":"value","type":"Int64"},{"name":"label","type":"String"},{"name":"active","type":"Bool"}],"rows":[[1,"é",true],[2,"雪",false],[3,"é",false]]}"#,
        ),
    ];

    for method in ["GET", "POST"] {
        for (query, exact_bytes, expected_body) in cases {
            let exact = format!(
                "{method} /?max%5Fgroup%5Fkey%5Fbytes={exact_bytes}&query={query} HTTP/1.1\r\nHost: localhost\r\n\r\n"
            );
            assert_response(
                &exchange(&database, exact.as_bytes()),
                "HTTP/1.1 200 OK",
                expected_body,
            );

            let one_byte_short = exact_bytes - 1;
            let exceeded = format!(
                "{method} /?query={query}&max_group_key_bytes={one_byte_short} HTTP/1.1\r\nHost: localhost\r\n\r\n"
            );
            assert_response(
                &exchange(&database, exceeded.as_bytes()),
                "HTTP/1.1 400 Bad Request",
                &format!(
                    r#"{{"error":"SELECT group key bytes requires at least {exact_bytes}, exceeding the limit of {one_byte_short}"}}"#
                ),
            );
        }
    }
}

#[test]
fn parameterized_max_group_key_bytes_zero_and_larger_values_never_relax_defaults() {
    let required_bytes = 3 * ESTIMATED_GROUP_KEY_CELL_BYTES;
    let configured_limits = QueryResultLimits {
        max_group_key_bytes: required_bytes - 1,
        ..QueryResultLimits::default()
    };
    let database = SharedDatabase::with_query_result_limits(configured_limits);
    database
        .execute(
            "CREATE TABLE samples (value Int64); \
             INSERT INTO samples VALUES (1), (2), (3);",
        )
        .unwrap();

    for (method, requested_max) in [
        ("GET", "0".to_owned()),
        ("POST", required_bytes.to_string()),
        ("GET", usize::MAX.to_string()),
    ] {
        let request = format!(
            "{method} /?query=SELECT+DISTINCT+value+FROM+samples%3B&max_group_key_bytes={requested_max} HTTP/1.1\r\nHost: localhost\r\n\r\n"
        );
        assert_response(
            &exchange(&database, request.as_bytes()),
            "HTTP/1.1 400 Bad Request",
            &format!(
                r#"{{"error":"SELECT group key bytes requires at least {required_bytes}, exceeding the limit of {}"}}"#,
                required_bytes - 1
            ),
        );
    }
    assert_eq!(database.query_result_limits().unwrap(), configured_limits);
}

#[test]
fn parameterized_max_group_key_bytes_rejects_invalid_values_for_get_and_post() {
    let database = SharedDatabase::default();
    let overflow = (usize::MAX as u128 + 1).to_string();

    for method in ["GET", "POST"] {
        let cases = [
            (
                format!(
                    "{method} /?query=SELECT+1%3B&max_group_key_bytes= HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"GET query parameters must have nonempty names and values"}"#
                    .replace("GET", method),
            ),
            (
                format!(
                    "{method} /?max_group_key_bytes=1&query=SELECT+1%3B&max%5Fgroup%5Fkey%5Fbytes=2 HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"duplicate max_group_key_bytes parameter"}"#.to_owned(),
            ),
            (
                format!(
                    "{method} /?query=SELECT+1%3B&max_group_key_bytes=1.0 HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"max_group_key_bytes parameter must be a decimal integer"}"#.to_owned(),
            ),
            (
                format!(
                    "{method} /?max_group_key_bytes={overflow}&query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"max_group_key_bytes parameter is out of range"}"#.to_owned(),
            ),
        ];
        for (request, expected_body) in cases {
            assert_response(
                &exchange(&database, request.as_bytes()),
                "HTTP/1.1 400 Bad Request",
                &expected_body,
            );
        }
    }
}

#[test]
fn concurrent_parameterized_max_group_key_byte_requests_are_isolated() {
    use std::sync::Barrier;

    let exact_bytes = 3 * ESTIMATED_GROUP_KEY_CELL_BYTES;
    let configured_limits = QueryResultLimits {
        max_group_key_bytes: exact_bytes,
        ..QueryResultLimits::default()
    };
    let database = SharedDatabase::with_query_result_limits(configured_limits);
    database
        .execute(
            "CREATE TABLE samples (value Int64); \
             INSERT INTO samples VALUES (1), (2), (3);",
        )
        .unwrap();
    let query = "SELECT+DISTINCT+value+FROM+samples+ORDER+BY+value%3B";
    let success_request = format!(
        "GET /?query={query}&max_group_key_bytes={exact_bytes} HTTP/1.1\r\nHost: localhost\r\n\r\n"
    );
    let failure_request = format!(
        "GET /?max_group_key_bytes={}&query={query} HTTP/1.1\r\nHost: localhost\r\n\r\n",
        exact_bytes - 1
    );
    let expected_success = exchange(&database, success_request.as_bytes());
    let expected_failure = exchange(&database, failure_request.as_bytes());

    let request_count = 12;
    let started = Arc::new(Barrier::new(request_count));
    let handles = (0..request_count)
        .map(|request_index| {
            let database = database.clone();
            let started = Arc::clone(&started);
            thread::spawn(move || {
                let requested_max = match request_index % 4 {
                    0 => exact_bytes.to_string(),
                    1 => (exact_bytes - 1).to_string(),
                    2 => "0".to_owned(),
                    _ => usize::MAX.to_string(),
                };
                let request = format!(
                    "GET /?max_group_key_bytes={requested_max}&query={query} HTTP/1.1\r\nHost: localhost\r\n\r\n"
                );
                started.wait();
                (request_index % 4, exchange(&database, request.as_bytes()))
            })
        })
        .collect::<Vec<_>>();

    for handle in handles {
        let (case, response) = handle.join().unwrap();
        if case == 1 {
            assert_eq!(response, expected_failure);
        } else {
            assert_eq!(response, expected_success);
        }
    }
    assert_eq!(database.query_result_limits().unwrap(), configured_limits);
}

#[test]
fn max_rows_to_group_by_validation_follows_authentication_and_precedes_lock_admission() {
    let poisoned_inner = Arc::new(RwLock::new(Database::new()));
    let poisoned_database = SharedDatabase::from_arc(Arc::clone(&poisoned_inner));
    let poisoner = thread::spawn(move || {
        let _guard = poisoned_inner.write().unwrap();
        panic!("poison the database lock");
    });
    assert!(poisoner.join().is_err());

    let invalid_without_credentials =
        b"GET /?query=SELECT+1%3B&max_rows_to_group_by=nope HTTP/1.1\r\nHost: localhost\r\n\r\n";
    assert_response(
        &authenticated_exchange(
            &poisoned_database,
            "correct-token",
            invalid_without_credentials,
        ),
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"bearer authentication required"}"#,
    );
    let missing_key = clickhouse_key_exchange(
        &poisoned_database,
        "correct-key",
        invalid_without_credentials,
    );
    assert_response(
        &missing_key,
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"X-ClickHouse-Key authentication required"}"#,
    );
    assert_clickhouse_key_response_is_not_cacheable(&missing_key);

    for authorized in [
        b"GET /?query=SELECT+1%3B&max_rows_to_group_by=nope HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer correct-token\r\n\r\n".as_slice(),
        b"POST /?max_rows_to_group_by=nope&query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer correct-token\r\n\r\n".as_slice(),
    ] {
        assert_response(
            &authenticated_exchange(&poisoned_database, "correct-token", authorized),
            "HTTP/1.1 400 Bad Request",
            r#"{"error":"max_rows_to_group_by parameter must be a decimal integer"}"#,
        );
    }

    let authorized_key = b"GET /?query=SELECT+1%3B&max_rows_to_group_by=nope HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: correct-key\r\n\r\n";
    let key_response = clickhouse_key_exchange(&poisoned_database, "correct-key", authorized_key);
    assert_response(
        &key_response,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"max_rows_to_group_by parameter must be a decimal integer"}"#,
    );
    assert_clickhouse_key_response_is_not_cacheable(&key_response);

    let contended_inner = Arc::new(RwLock::new(Database::new()));
    let contended_database = SharedDatabase::from_arc(Arc::clone(&contended_inner));
    let mut writer = Some(contended_inner.write().unwrap());
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let invalid = exchange(
            &contended_database,
            b"GET /?query=SELECT+1%3B&max_rows_to_group_by=-1 HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );
        let valid = exchange(
            &contended_database,
            b"POST /?max_rows_to_group_by=1&query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );
        sender.send((invalid, valid)).unwrap();
    });
    let (invalid, valid) = match receiver.recv_timeout(Duration::from_secs(2)) {
        Ok(responses) => responses,
        Err(error) => {
            drop(writer.take());
            worker.join().unwrap();
            panic!("max_rows_to_group_by admission blocked behind a writer: {error}");
        }
    };
    assert_response(
        &invalid,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"max_rows_to_group_by parameter must be a decimal integer"}"#,
    );
    assert_response(
        &valid,
        "HTTP/1.1 503 Service Unavailable",
        r#"{"error":"database is unavailable"}"#,
    );
    drop(writer.take());
    worker.join().unwrap();
}

fn single_string_result_bytes() -> usize {
    std::mem::size_of::<ResultColumn>()
        + "value".len()
        + std::mem::size_of::<Vec<Value>>()
        + std::mem::size_of::<Value>()
        + "x".len()
}

#[test]
fn parameterized_max_result_bytes_enforces_the_exact_get_and_post_boundary() {
    let database = SharedDatabase::default();
    let exact_bytes = single_string_result_bytes();
    let below_boundary = exact_bytes - 1;
    let expected = r#"{"columns":[{"name":"value","type":"String"}],"rows":[["x"]]}"#;

    for method in ["GET", "POST"] {
        let exact = format!(
            "{method} /?query=SELECT+%27x%27+AS+value%3B&max_result_bytes={exact_bytes} HTTP/1.1\r\nHost: localhost\r\n\r\n"
        );
        assert_response(
            &exchange(&database, exact.as_bytes()),
            "HTTP/1.1 200 OK",
            expected,
        );

        let exceeded = format!(
            "{method} /?max_result_bytes={below_boundary}&query=SELECT+%27x%27+AS+value%3B HTTP/1.1\r\nHost: localhost\r\n\r\n"
        );
        assert_response(
            &exchange(&database, exceeded.as_bytes()),
            "HTTP/1.1 400 Bad Request",
            &format!(
                r#"{{"error":"retained query results require at least {exact_bytes} bytes, exceeding the limit of {below_boundary} bytes"}}"#
            ),
        );
    }
}

#[test]
fn parameterized_max_result_bytes_zero_and_larger_values_never_relax_defaults() {
    let exact_bytes = single_string_result_bytes();
    let configured_max = exact_bytes - 1;
    let database = SharedDatabase::with_query_result_limits(QueryResultLimits {
        max_bytes: configured_max,
        ..QueryResultLimits::default()
    });

    for (method, requested_max) in [
        ("GET", "0".to_owned()),
        ("POST", exact_bytes.to_string()),
        ("GET", usize::MAX.to_string()),
    ] {
        let request = format!(
            "{method} /?max_result_bytes={requested_max}&query=SELECT+%27x%27+AS+value%3B HTTP/1.1\r\nHost: localhost\r\n\r\n"
        );
        assert_response(
            &exchange(&database, request.as_bytes()),
            "HTTP/1.1 400 Bad Request",
            &format!(
                r#"{{"error":"SELECT result bytes requires at least {exact_bytes}, exceeding the limit of {configured_max}"}}"#
            ),
        );
    }

    let default_database = SharedDatabase::default();
    for (method, requested_max) in [("GET", "0".to_owned()), ("POST", usize::MAX.to_string())] {
        let request = format!(
            "{method} /?query=SELECT+1%3B&max_result_bytes={requested_max} HTTP/1.1\r\nHost: localhost\r\n\r\n"
        );
        assert_response(
            &exchange(&default_database, request.as_bytes()),
            "HTTP/1.1 200 OK",
            r#"{"columns":[{"name":"1","type":"Int64"}],"rows":[[1]]}"#,
        );
    }
}

#[test]
fn parameterized_max_result_bytes_rejects_empty_duplicate_malformed_and_overflowing_values() {
    let database = SharedDatabase::default();
    let overflow = (usize::MAX as u128 + 1).to_string();

    for method in ["GET", "POST"] {
        let cases = [
            (
                format!(
                    "{method} /?query=SELECT+1%3B&max_result_bytes= HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"GET query parameters must have nonempty names and values"}"#
                    .replace("GET", method),
            ),
            (
                format!(
                    "{method} /?max_result_bytes=1&query=SELECT+1%3B&max%5Fresult%5Fbytes=2 HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"duplicate max_result_bytes parameter"}"#.to_owned(),
            ),
            (
                format!(
                    "{method} /?query=SELECT+1%3B&max_result_bytes=1.0 HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"max_result_bytes parameter must be a decimal integer"}"#.to_owned(),
            ),
            (
                format!(
                    "{method} /?max_result_bytes={overflow}&query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\n\r\n"
                ),
                r#"{"error":"max_result_bytes parameter is out of range"}"#.to_owned(),
            ),
        ];
        for (request, expected_body) in cases {
            assert_response(
                &exchange(&database, request.as_bytes()),
                "HTTP/1.1 400 Bad Request",
                &expected_body,
            );
        }
    }
}

#[test]
fn max_result_bytes_validation_follows_authentication_and_precedes_lock_admission() {
    let poisoned_inner = Arc::new(RwLock::new(Database::new()));
    let poisoned_database = SharedDatabase::from_arc(Arc::clone(&poisoned_inner));
    let poisoner = thread::spawn(move || {
        let _guard = poisoned_inner.write().unwrap();
        panic!("poison the database lock");
    });
    assert!(poisoner.join().is_err());

    let invalid_without_credentials =
        b"GET /?query=SELECT+1%3B&max_result_bytes=nope HTTP/1.1\r\nHost: localhost\r\n\r\n";
    assert_response(
        &authenticated_exchange(
            &poisoned_database,
            "correct-token",
            invalid_without_credentials,
        ),
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"bearer authentication required"}"#,
    );
    let missing_key = clickhouse_key_exchange(
        &poisoned_database,
        "correct-key",
        invalid_without_credentials,
    );
    assert_response(
        &missing_key,
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"X-ClickHouse-Key authentication required"}"#,
    );
    assert_clickhouse_key_response_is_not_cacheable(&missing_key);

    for authorized in [
        b"GET /?query=SELECT+1%3B&max_result_bytes=nope HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer correct-token\r\n\r\n".as_slice(),
        b"POST /?max_result_bytes=nope&query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer correct-token\r\n\r\n".as_slice(),
    ] {
        assert_response(
            &authenticated_exchange(&poisoned_database, "correct-token", authorized),
            "HTTP/1.1 400 Bad Request",
            r#"{"error":"max_result_bytes parameter must be a decimal integer"}"#,
        );
    }

    let contended_inner = Arc::new(RwLock::new(Database::new()));
    let contended_database = SharedDatabase::from_arc(Arc::clone(&contended_inner));
    let mut writer = Some(contended_inner.write().unwrap());
    let (sender, receiver) = mpsc::channel();
    let worker_database = contended_database.clone();
    let worker = thread::spawn(move || {
        let invalid = exchange(
            &worker_database,
            b"GET /?query=SELECT+1%3B&max_result_bytes=-1 HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );
        let valid = exchange(
            &worker_database,
            b"POST /?max_result_bytes=1&query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );
        sender.send((invalid, valid)).unwrap();
    });
    let (invalid, valid) = match receiver.recv_timeout(Duration::from_secs(2)) {
        Ok(responses) => responses,
        Err(error) => {
            drop(writer.take());
            worker.join().unwrap();
            panic!("max_result_bytes admission blocked behind a writer: {error}");
        }
    };
    assert_response(
        &invalid,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"max_result_bytes parameter must be a decimal integer"}"#,
    );
    assert_response(
        &valid,
        "HTTP/1.1 503 Service Unavailable",
        r#"{"error":"database is unavailable"}"#,
    );
    drop(writer.take());
    worker.join().unwrap();
}

#[test]
fn max_result_rows_validation_follows_authentication_and_precedes_lock_admission() {
    let poisoned_inner = Arc::new(RwLock::new(Database::new()));
    let poisoned_database = SharedDatabase::from_arc(Arc::clone(&poisoned_inner));
    let poisoner = thread::spawn(move || {
        let _guard = poisoned_inner.write().unwrap();
        panic!("poison the database lock");
    });
    assert!(poisoner.join().is_err());

    let invalid_without_credentials =
        b"GET /?query=SELECT+1%3B&max_result_rows=nope HTTP/1.1\r\nHost: localhost\r\n\r\n";
    assert_response(
        &authenticated_exchange(
            &poisoned_database,
            "correct-token",
            invalid_without_credentials,
        ),
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"bearer authentication required"}"#,
    );
    let missing_key = clickhouse_key_exchange(
        &poisoned_database,
        "correct-key",
        invalid_without_credentials,
    );
    assert_response(
        &missing_key,
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"X-ClickHouse-Key authentication required"}"#,
    );
    assert_clickhouse_key_response_is_not_cacheable(&missing_key);

    let authorized = b"GET /?query=SELECT+1%3B&max_result_rows=nope HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer correct-token\r\n\r\n";
    assert_response(
        &authenticated_exchange(&poisoned_database, "correct-token", authorized),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"max_result_rows parameter must be a decimal integer"}"#,
    );

    let contended_inner = Arc::new(RwLock::new(Database::new()));
    let contended_database = SharedDatabase::from_arc(Arc::clone(&contended_inner));
    let mut writer = Some(contended_inner.write().unwrap());
    let (sender, receiver) = mpsc::channel();
    let worker_database = contended_database.clone();
    let worker = thread::spawn(move || {
        let invalid = exchange(
            &worker_database,
            b"GET /?query=SELECT+1%3B&max_result_rows=-1 HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );
        let valid = exchange(
            &worker_database,
            b"POST /?max_result_rows=1&query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );
        sender.send((invalid, valid)).unwrap();
    });
    let (invalid, valid) = match receiver.recv_timeout(Duration::from_secs(2)) {
        Ok(responses) => responses,
        Err(error) => {
            drop(writer.take());
            worker.join().unwrap();
            panic!("max_result_rows admission blocked behind a writer: {error}");
        }
    };
    assert_response(
        &invalid,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"max_result_rows parameter must be a decimal integer"}"#,
    );
    assert_response(
        &valid,
        "HTTP/1.1 503 Service Unavailable",
        r#"{"error":"database is unavailable"}"#,
    );
    drop(writer.take());
    worker.join().unwrap();
}

#[test]
fn terminal_csv_with_names_format_wires_get_and_post_with_escaped_and_empty_results() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE samples (label String); \
             INSERT INTO samples VALUES ('comma, \"quoted\"\nline');",
        )
        .unwrap();

    assert_response_with_content_type(
        &exchange(
            &database,
            b"GET /?query=SELECT+label+FROM+samples+FORMAT+CSVWithNames%3B HTTP/1.1\r\nHost: localhost\r\n\r\n",
        ),
        "HTTP/1.1 200 OK",
        "text/csv; charset=utf-8",
        b"label\n\"comma, \"\"quoted\"\"\nline\"\n",
    );

    assert_response_with_content_type(
        &exchange(
            &database,
            &request(b"SELECT label FROM samples WHERE label = 'missing' FORMAT CSVWithNames;"),
        ),
        "HTTP/1.1 200 OK",
        "text/csv; charset=utf-8",
        b"label\n",
    );

    assert_response_with_content_type(
        &exchange(
            &database,
            b"POST /?query=SELECT+7+AS+value+format+csvwithnames HTTP/1.1\r\nHost: localhost\r\n\r\n",
        ),
        "HTTP/1.1 200 OK",
        "text/csv; charset=utf-8",
        b"value\n7\n",
    );
}

#[test]
fn terminal_csv_format_wires_get_and_post_with_typed_escaped_and_empty_results() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE typed_csv (id Int64, score Float64, active Bool, label String); \
             INSERT INTO typed_csv VALUES \
                 (-7, 2.0, false, 'comma, \"quoted\"\nline'), \
                 (0, -1.25, true, '');",
        )
        .unwrap();
    let expected = concat!(
        "-7,2.0,false,\"comma, \"\"quoted\"\"\nline\"\n",
        "0,-1.25,true,\n",
    );
    let requests = [
        request_for_target(
            "/",
            b"SELECT id, score, active, label FROM typed_csv ORDER BY id FORMAT CSV;",
        ),
        b"GET /?query=SELECT+id%2C+score%2C+active%2C+label+FROM+typed_csv+ORDER+BY+id+FoRmAt+CsV HTTP/1.1\r\nHost: localhost\r\n\r\n".to_vec(),
        b"POST /?query=SELECT+id%2C+score%2C+active%2C+label+FROM+typed_csv+ORDER+BY+id+format+csv%3B HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n".to_vec(),
    ];

    for request in requests {
        assert_response_with_content_type(
            &exchange(&database, &request),
            "HTTP/1.1 200 OK",
            "text/csv; charset=utf-8",
            expected.as_bytes(),
        );
    }

    assert_response_with_content_type(
        &exchange(
            &database,
            &request(b"SELECT id FROM typed_csv WHERE id = 99 FORMAT CSV;"),
        ),
        "HTTP/1.1 200 OK",
        "text/csv; charset=utf-8",
        b"",
    );
}

#[test]
fn terminal_json_format_wires_get_and_post_with_typed_and_empty_results() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE typed_json (id Int64, score Float64, active Bool, label String); \
             INSERT INTO typed_json VALUES (-7, 2.0, false, 'quote\"\\line\nsnow 雪'), \
                                           (0, -1.25, true, '');",
        )
        .unwrap();
    let expected = concat!(
        "{\"columns\":[{\"name\":\"id\",\"type\":\"Int64\"},",
        "{\"name\":\"score\",\"type\":\"Float64\"},",
        "{\"name\":\"active\",\"type\":\"Bool\"},",
        "{\"name\":\"label\",\"type\":\"String\"}],",
        "\"rows\":[[-7,2.0,false,\"quote\\\"\\\\line\\nsnow 雪\"],",
        "[0,-1.25,true,\"\"]]}"
    );

    let requests = [
        b"GET /?query=SELECT+id%2C+score%2C+active%2C+label+FROM+typed_json+ORDER+BY+id+FoRmAt+JsOn%3B HTTP/1.1\r\nHost: localhost\r\n\r\n".to_vec(),
        request_for_target(
            "/query",
            b"SELECT id, score, active, label FROM typed_json ORDER BY id format json",
        ),
    ];
    for request in requests {
        assert_response(&exchange(&database, &request), "HTTP/1.1 200 OK", expected);
    }

    assert_response(
        &exchange(
            &database,
            &request(b"SELECT id FROM typed_json WHERE id = 99 FORMAT JSON;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"}],"rows":[]}"#,
    );
}

#[test]
fn terminal_json_each_row_format_wires_every_query_form_with_typed_and_escaped_rows() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE typed_values (id Int64, score Float64, active Bool, label String); \
             INSERT INTO typed_values VALUES (-7, 2.0, false, 'quote\"\\line\nsnow 雪'), \
                                               (0, -1.25, true, '');",
        )
        .unwrap();
    let sql = b"SELECT id, score, active, label FROM typed_values ORDER BY id FORMAT JSONEachRow;";
    let requests = [
        request_for_target("/", sql),
        request_for_target(
            "/query",
            b"SELECT id, score, active, label FROM typed_values ORDER BY id format jsoneachrow",
        ),
        b"GET /?query=SELECT+id%2C+score%2C+active%2C+label+FROM+typed_values+ORDER+BY+id+FORMAT+JSONEachRow%3B HTTP/1.1\r\nHost: localhost\r\n\r\n".to_vec(),
        b"POST /?query=SELECT+id%2C+score%2C+active%2C+label+FROM+typed_values+ORDER+BY+id+FORMAT+JSONEachRow HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n".to_vec(),
    ];
    let expected = concat!(
        "{\"id\":-7,\"score\":2.0,\"active\":false,\"label\":\"quote\\\"\\\\line\\nsnow 雪\"}\n",
        "{\"id\":0,\"score\":-1.25,\"active\":true,\"label\":\"\"}\n",
    );

    for request in requests {
        assert_response(&exchange(&database, &request), "HTTP/1.1 200 OK", expected);
    }

    assert_response(
        &exchange(
            &database,
            &request("SELECT 'quote\"\\line\nsnow 雪' FORMAT JSONEachRow;".as_bytes()),
        ),
        "HTTP/1.1 200 OK",
        "{\"'quote\\\"\\\\line\\nsnow 雪'\":\"quote\\\"\\\\line\\nsnow 雪\"}\n",
    );
    assert_response(
        &exchange(
            &database,
            &request(b"SELECT id FROM typed_values WHERE id = 99 FORMAT JSONEachRow;"),
        ),
        "HTTP/1.1 200 OK",
        "",
    );
}

#[test]
fn terminal_json_compact_each_row_format_wires_every_query_form_with_typed_escaped_and_empty_rows()
{
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE compact_values (id Int64, score Float64, active Bool, label String); \
             INSERT INTO compact_values VALUES (-7, 2.0, false, 'quote\"\\line\nsnow 雪'), \
                                                (0, -1.25, true, '');",
        )
        .unwrap();
    let requests = [
        request_for_target(
            "/",
            b"SELECT id, score, active, label FROM compact_values ORDER BY id FORMAT JSONCompactEachRow;",
        ),
        request_for_target(
            "/query",
            b"SELECT id, score, active, label FROM compact_values ORDER BY id format jsoncompacteachrow",
        ),
        b"GET /?query=SELECT+id%2C+score%2C+active%2C+label+FROM+compact_values+ORDER+BY+id+FoRmAt+JsOnCoMpAcTeAcHrOw%3B HTTP/1.1\r\nHost: localhost\r\n\r\n".to_vec(),
        b"POST /?query=SELECT+id%2C+score%2C+active%2C+label+FROM+compact_values+ORDER+BY+id+FORMAT+JSONCompactEachRow HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n".to_vec(),
    ];
    let expected = concat!(
        "[-7,2.0,false,\"quote\\\"\\\\line\\nsnow 雪\"]\n",
        "[0,-1.25,true,\"\"]\n",
    );

    for request in requests {
        assert_response(&exchange(&database, &request), "HTTP/1.1 200 OK", expected);
    }

    assert_response(
        &exchange(
            &database,
            &request(b"SELECT id FROM compact_values WHERE id = 99 FORMAT JSONCompactEachRow;"),
        ),
        "HTTP/1.1 200 OK",
        "",
    );
}

#[test]
fn terminal_tab_separated_format_wires_get_and_post_with_typed_escaped_rows() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE typed_tsv (id Int64, score Float64, active Bool, label String); \
             INSERT INTO typed_tsv VALUES \
                 (-7, 2.0, false, 'slash\\tab\tcarriage\rline\napostrophe'' 雪'), \
                 (0, -1.25, true, '');",
        )
        .unwrap();
    let sql = b"SELECT id, score, active, label FROM typed_tsv ORDER BY id FORMAT TabSeparated;";
    let requests = [
        request_for_target("/", sql),
        request_for_target(
            "/query",
            b"SELECT id, score, active, label FROM typed_tsv ORDER BY id format tabseparated",
        ),
        b"GET /?query=SELECT+id%2C+score%2C+active%2C+label+FROM+typed_tsv+ORDER+BY+id+FoRmAt+TaBsEpArAtEd%3B HTTP/1.1\r\nHost: localhost\r\n\r\n".to_vec(),
        b"POST /?query=SELECT+id%2C+score%2C+active%2C+label+FROM+typed_tsv+ORDER+BY+id+FORMAT+TabSeparated HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n".to_vec(),
    ];
    let expected = concat!(
        "-7\t2.0\tfalse\tslash\\\\tab\\tcarriage\\rline\\napostrophe\\' 雪\n",
        "0\t-1.25\ttrue\t\n",
    );

    for request in requests {
        assert_response_with_content_type(
            &exchange(&database, &request),
            "HTTP/1.1 200 OK",
            "text/tab-separated-values; charset=utf-8",
            expected.as_bytes(),
        );
    }

    assert_response_with_content_type(
        &exchange(
            &database,
            &request(b"SELECT id FROM typed_tsv WHERE id = 99 FORMAT TabSeparated;"),
        ),
        "HTTP/1.1 200 OK",
        "text/tab-separated-values; charset=utf-8",
        b"",
    );
}

#[test]
fn terminal_tab_separated_with_names_wires_typed_escaped_and_empty_results() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE named_tsv (id Int64, score Float64, active Bool, label String); \
             INSERT INTO named_tsv VALUES \
                 (-7, 2.0, false, 'slash\\tab\tcarriage\rline\napostrophe'' 雪'), \
                 (0, -1.25, true, '');",
        )
        .unwrap();
    let expected = concat!(
        "id\tscore\tactive\tlabel\n",
        "-7\t2.0\tfalse\tslash\\\\tab\\tcarriage\\rline\\napostrophe\\' 雪\n",
        "0\t-1.25\ttrue\t\n",
    );
    let requests = [
        request_for_target(
            "/query",
            b"SELECT id, score, active, label FROM named_tsv ORDER BY id FORMAT TabSeparatedWithNames;",
        ),
        b"GET /?query=SELECT+id%2C+score%2C+active%2C+label+FROM+named_tsv+ORDER+BY+id+FoRmAt+TaBsEpArAtEdWiThNaMeS HTTP/1.1\r\nHost: localhost\r\n\r\n".to_vec(),
        b"POST /?query=SELECT+id%2C+score%2C+active%2C+label+FROM+named_tsv+ORDER+BY+id+format+tabseparatedwithnames%3B HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n".to_vec(),
    ];

    for request in requests {
        assert_response_with_content_type(
            &exchange(&database, &request),
            "HTTP/1.1 200 OK",
            "text/tab-separated-values; charset=utf-8",
            expected.as_bytes(),
        );
    }

    assert_response_with_content_type(
        &exchange(
            &database,
            &request(b"SELECT id FROM named_tsv WHERE id = 99 FORMAT TabSeparatedWithNames;"),
        ),
        "HTTP/1.1 200 OK",
        "text/tab-separated-values; charset=utf-8",
        b"id\n",
    );
}

#[test]
fn terminal_json_format_preserves_quote_and_comment_scanning() {
    let database = SharedDatabase::default();

    assert_response(
        &exchange(&database, &request(b"SELECT 'FORMAT JSON' AS value;")),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"value","type":"String"}],"rows":[["FORMAT JSON"]]}"#,
    );
    assert_response(
        &exchange(&database, &request(b"SELECT 1 AS value; -- FORMAT JSON")),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"value","type":"Int64"}],"rows":[[1]]}"#,
    );
    assert_response(
        &exchange(
            &database,
            &request(b"SELECT 'escaped ''FORMAT JSON''' AS value;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"value","type":"String"}],"rows":[["escaped 'FORMAT JSON'"]]}"#,
    );
}

#[test]
fn terminal_json_format_rejects_selectors_after_authentication() {
    let database = SharedDatabase::default();
    let conflict_error = r#"{"error":"FORMAT JSON clause cannot be combined with X-ClickHouse-Format header or default_format parameter"}"#;

    assert_response(
        &exchange(
            &database,
            &request_for_target_with_headers(
                "/query",
                b"SELECT 1 FORMAT JSON;",
                "X-ClickHouse-Format: JSONEachRow\r\n",
            ),
        ),
        "HTTP/1.1 400 Bad Request",
        conflict_error,
    );
    assert_response(
        &exchange(
            &database,
            b"GET /?query=SELECT+1+FORMAT+JSON&default_format=JSON HTTP/1.1\r\nHost: localhost\r\n\r\n",
        ),
        "HTTP/1.1 400 Bad Request",
        conflict_error,
    );

    let unauthorized =
        b"GET /?query=SELECT+1+FORMAT+JSON&default_format=JSON HTTP/1.1\r\nHost: localhost\r\n\r\n";
    assert_response(
        &authenticated_exchange(&database, "correct-token", unauthorized),
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"bearer authentication required"}"#,
    );
    let missing_key_response = clickhouse_key_exchange(&database, "correct-key", unauthorized);
    assert_response(
        &missing_key_response,
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"X-ClickHouse-Key authentication required"}"#,
    );
    assert_clickhouse_key_response_is_not_cacheable(&missing_key_response);
}

#[test]
fn terminal_tab_separated_format_preserves_quote_and_comment_scanning() {
    let database = SharedDatabase::default();

    assert_response(
        &exchange(
            &database,
            &request(b"SELECT 'FORMAT TabSeparated' AS value;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"value","type":"String"}],"rows":[["FORMAT TabSeparated"]]}"#,
    );
    assert_response(
        &exchange(
            &database,
            &request(b"SELECT 1 AS value; -- FORMAT TabSeparated"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"value","type":"Int64"}],"rows":[[1]]}"#,
    );
    assert_response(
        &exchange(
            &database,
            &request(b"SELECT 'escaped ''FORMAT TabSeparated''' AS value;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"value","type":"String"}],"rows":[["escaped 'FORMAT TabSeparated'"]]}"#,
    );
}

#[test]
fn terminal_tab_separated_format_rejects_selectors_after_authentication() {
    let database = SharedDatabase::default();
    let conflict_error = r#"{"error":"FORMAT TabSeparated clause cannot be combined with X-ClickHouse-Format header or default_format parameter"}"#;

    assert_response(
        &exchange(
            &database,
            &request_for_target_with_headers(
                "/query",
                b"SELECT 1 FORMAT TabSeparated;",
                "X-ClickHouse-Format: TabSeparated\r\n",
            ),
        ),
        "HTTP/1.1 400 Bad Request",
        conflict_error,
    );
    assert_response(
        &exchange(
            &database,
            b"GET /?query=SELECT+1+FORMAT+TabSeparated&default_format=TabSeparated HTTP/1.1\r\nHost: localhost\r\n\r\n",
        ),
        "HTTP/1.1 400 Bad Request",
        conflict_error,
    );

    let unauthorized = b"GET /?query=SELECT+1+FORMAT+TabSeparated&default_format=TabSeparated HTTP/1.1\r\nHost: localhost\r\n\r\n";
    assert_response(
        &authenticated_exchange(&database, "correct-token", unauthorized),
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"bearer authentication required"}"#,
    );
    let missing_key_response = clickhouse_key_exchange(&database, "correct-key", unauthorized);
    assert_response(
        &missing_key_response,
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"X-ClickHouse-Key authentication required"}"#,
    );
    assert_clickhouse_key_response_is_not_cacheable(&missing_key_response);
}

#[test]
fn terminal_csv_and_named_tsv_preserve_quote_and_comment_scanning() {
    let database = SharedDatabase::default();

    for format in ["CSV", "TabSeparatedWithNames"] {
        let quoted = format!("SELECT 'FORMAT {format}' AS value;");
        assert_response(
            &exchange(&database, &request(quoted.as_bytes())),
            "HTTP/1.1 200 OK",
            &format!(
                r#"{{"columns":[{{"name":"value","type":"String"}}],"rows":[["FORMAT {format}"]]}}"#
            ),
        );

        let commented = format!("SELECT 1 AS value; -- FORMAT {format}");
        assert_response(
            &exchange(&database, &request(commented.as_bytes())),
            "HTTP/1.1 200 OK",
            r#"{"columns":[{"name":"value","type":"Int64"}],"rows":[[1]]}"#,
        );

        let escaped = format!("SELECT 'escaped ''FORMAT {format}''' AS value;");
        assert_response(
            &exchange(&database, &request(escaped.as_bytes())),
            "HTTP/1.1 200 OK",
            &format!(
                r#"{{"columns":[{{"name":"value","type":"String"}}],"rows":[["escaped 'FORMAT {format}'"]]}}"#
            ),
        );
    }
}

#[test]
fn terminal_csv_and_named_tsv_formats_authenticate_and_reject_selector_conflicts() {
    let database = SharedDatabase::default();

    for (format, content_type, expected) in [
        ("CSV", "text/csv; charset=utf-8", b"7\n".as_slice()),
        (
            "TabSeparatedWithNames",
            "text/tab-separated-values; charset=utf-8",
            b"value\n7\n".as_slice(),
        ),
    ] {
        let sql = format!("SELECT 7 AS value FORMAT {format};");
        let bearer = request_for_target_with_headers(
            "/query",
            sql.as_bytes(),
            "Authorization: Bearer correct-token\r\n",
        );
        assert_response_with_content_type(
            &authenticated_exchange(&database, "correct-token", &bearer),
            "HTTP/1.1 200 OK",
            content_type,
            expected,
        );

        let key_request = format!(
            "GET /?query=SELECT+7+AS+value+FORMAT+{format} HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: correct-key\r\n\r\n"
        );
        let key_response =
            clickhouse_key_exchange(&database, "correct-key", key_request.as_bytes());
        assert_response_with_content_type(&key_response, "HTTP/1.1 200 OK", content_type, expected);
        assert_clickhouse_key_response_is_not_cacheable(&key_response);

        let conflict_error = format!(
            r#"{{"error":"FORMAT {format} clause cannot be combined with X-ClickHouse-Format header or default_format parameter"}}"#
        );
        let header_conflict = request_for_target_with_headers(
            "/query",
            sql.as_bytes(),
            "X-ClickHouse-Format: JSONEachRow\r\n",
        );
        assert_response(
            &exchange(&database, &header_conflict),
            "HTTP/1.1 400 Bad Request",
            &conflict_error,
        );

        let parameter_conflict = format!(
            "GET /?query=SELECT+7+FORMAT+{format}&default_format={format} HTTP/1.1\r\nHost: localhost\r\n\r\n"
        );
        assert_response(
            &exchange(&database, parameter_conflict.as_bytes()),
            "HTTP/1.1 400 Bad Request",
            &conflict_error,
        );
        assert_response(
            &authenticated_exchange(&database, "correct-token", parameter_conflict.as_bytes()),
            "HTTP/1.1 401 Unauthorized",
            r#"{"error":"bearer authentication required"}"#,
        );
        let missing_key_response =
            clickhouse_key_exchange(&database, "correct-key", parameter_conflict.as_bytes());
        assert_response(
            &missing_key_response,
            "HTTP/1.1 401 Unauthorized",
            r#"{"error":"X-ClickHouse-Key authentication required"}"#,
        );
        assert_clickhouse_key_response_is_not_cacheable(&missing_key_response);
    }
}

#[test]
fn terminal_csv_and_named_tsv_formats_remain_read_only_and_response_bounded() {
    let database = SharedDatabase::default();
    database.execute("CREATE TABLE events (id Int64);").unwrap();

    for format in ["CSV", "TabSeparatedWithNames"] {
        let multiple = format!("SELECT 1; SELECT 2 FORMAT {format};");
        assert_response(
            &exchange(&database, &request(multiple.as_bytes())),
            "HTTP/1.1 400 Bad Request",
            r#"{"error":"read-only query requires exactly one statement; found 2"}"#,
        );

        let insert = format!("INSERT INTO events VALUES (1) FORMAT {format};");
        let authenticated_insert = request_for_target_with_headers(
            "/query",
            insert.as_bytes(),
            "Authorization: Bearer correct-token\r\n",
        );
        assert_response(
            &authenticated_exchange(&database, "correct-token", &authenticated_insert),
            "HTTP/1.1 400 Bad Request",
            r#"{"error":"read-only query accepts only SELECT, SHOW DATABASES, SHOW SETTINGS, SHOW FUNCTIONS, SHOW TABLES, SHOW CREATE TABLE, DESCRIBE TABLE, or EXISTS TABLE; found INSERT"}"#,
        );

        let oversized = format!("SELECT '{}' AS value FORMAT {format};", "x".repeat(1_000));
        let limits = HttpQueryLimits {
            max_response_bytes: 512,
            ..HttpQueryLimits::default()
        };
        let mut response = Vec::new();
        handle_http_query_with_limits(
            &database,
            Cursor::new(request(oversized.as_bytes())),
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

    assert_response(
        &exchange(&database, &request(b"SELECT id FROM events;")),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"}],"rows":[]}"#,
    );
}

#[test]
fn terminal_csv_with_names_format_works_with_both_authentication_modes() {
    let database = SharedDatabase::default();
    let bearer = request_for_target_with_headers(
        "/query",
        b"SELECT 7 AS value FORMAT CSVWithNames;",
        "Authorization: Bearer correct-token\r\n",
    );
    assert_response_with_content_type(
        &authenticated_exchange(&database, "correct-token", &bearer),
        "HTTP/1.1 200 OK",
        "text/csv; charset=utf-8",
        b"value\n7\n",
    );

    let key_response = clickhouse_key_exchange(
        &database,
        "correct-key",
        b"GET /?query=SELECT+8+AS+value+FORMAT+CSVWithNames HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: correct-key\r\n\r\n",
    );
    assert_response_with_content_type(
        &key_response,
        "HTTP/1.1 200 OK",
        "text/csv; charset=utf-8",
        b"value\n8\n",
    );
    assert_clickhouse_key_response_is_not_cacheable(&key_response);
}

#[test]
fn terminal_csv_with_names_format_rejects_metadata_conflicts_after_authentication() {
    let database = SharedDatabase::default();
    let conflict_error = r#"{"error":"FORMAT CSVWithNames clause cannot be combined with X-ClickHouse-Format header or default_format parameter"}"#;

    assert_response(
        &exchange(
            &database,
            &request_for_target_with_headers(
                "/query",
                b"SELECT 1 FORMAT CSVWithNames;",
                "X-ClickHouse-Format: CSVWithNames\r\n",
            ),
        ),
        "HTTP/1.1 400 Bad Request",
        conflict_error,
    );
    assert_response(
        &exchange(
            &database,
            b"GET /?query=SELECT+1+FORMAT+CSVWithNames&default_format=CSVWithNames HTTP/1.1\r\nHost: localhost\r\n\r\n",
        ),
        "HTTP/1.1 400 Bad Request",
        conflict_error,
    );

    let unauthorized = b"GET /?query=SELECT+1+FORMAT+CSVWithNames&default_format=CSVWithNames HTTP/1.1\r\nHost: localhost\r\n\r\n";
    assert_response(
        &authenticated_exchange(&database, "correct-token", unauthorized),
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"bearer authentication required"}"#,
    );
    let missing_key_response = clickhouse_key_exchange(&database, "correct-key", unauthorized);
    assert_response(
        &missing_key_response,
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"X-ClickHouse-Key authentication required"}"#,
    );
    assert_clickhouse_key_response_is_not_cacheable(&missing_key_response);
}

#[test]
fn terminal_csv_with_names_format_ignores_quoted_and_commented_text() {
    let database = SharedDatabase::default();

    assert_response(
        &exchange(
            &database,
            &request(b"SELECT 'FORMAT CSVWithNames' AS value;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"value","type":"String"}],"rows":[["FORMAT CSVWithNames"]]}"#,
    );
    assert_response(
        &exchange(
            &database,
            &request(b"SELECT 1 AS value; -- FORMAT CSVWithNames"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"value","type":"Int64"}],"rows":[[1]]}"#,
    );

    assert_response(
        &exchange(
            &database,
            b"GET /?query=SELECT+%27escaped+%27%27FORMAT+CSVWithNames%27%27%27+AS+value%3B HTTP/1.1\r\nHost: localhost\r\n\r\n",
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"value","type":"String"}],"rows":[["escaped 'FORMAT CSVWithNames'"]]}"#,
    );
}

#[test]
fn terminal_json_each_row_format_preserves_quote_and_comment_scanning() {
    let database = SharedDatabase::default();

    assert_response(
        &exchange(
            &database,
            &request(b"SELECT 'FORMAT JSONEachRow' AS value;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"value","type":"String"}],"rows":[["FORMAT JSONEachRow"]]}"#,
    );
    assert_response(
        &exchange(
            &database,
            &request(b"SELECT 1 AS value; -- FORMAT JSONEachRow"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"value","type":"Int64"}],"rows":[[1]]}"#,
    );
    assert_response(
        &exchange(
            &database,
            &request(b"SELECT 'escaped ''FORMAT JSONEachRow''' AS value;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"value","type":"String"}],"rows":[["escaped 'FORMAT JSONEachRow'"]]}"#,
    );
}

#[test]
fn terminal_json_each_row_format_rejects_selectors_after_authentication() {
    let database = SharedDatabase::default();
    let conflict_error = r#"{"error":"FORMAT JSONEachRow clause cannot be combined with X-ClickHouse-Format header or default_format parameter"}"#;

    assert_response(
        &exchange(
            &database,
            &request_for_target_with_headers(
                "/query",
                b"SELECT 1 FORMAT JSONEachRow;",
                "X-ClickHouse-Format: CSVWithNames\r\n",
            ),
        ),
        "HTTP/1.1 400 Bad Request",
        conflict_error,
    );
    assert_response(
        &exchange(
            &database,
            b"GET /?query=SELECT+1+FORMAT+JSONEachRow&default_format=JSONEachRow HTTP/1.1\r\nHost: localhost\r\n\r\n",
        ),
        "HTTP/1.1 400 Bad Request",
        conflict_error,
    );

    let unauthorized = b"GET /?query=SELECT+1+FORMAT+JSONEachRow&default_format=JSONEachRow HTTP/1.1\r\nHost: localhost\r\n\r\n";
    assert_response(
        &authenticated_exchange(&database, "correct-token", unauthorized),
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"bearer authentication required"}"#,
    );
    let missing_key_response = clickhouse_key_exchange(&database, "correct-key", unauthorized);
    assert_response(
        &missing_key_response,
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"X-ClickHouse-Key authentication required"}"#,
    );
    assert_clickhouse_key_response_is_not_cacheable(&missing_key_response);
}

#[test]
fn terminal_json_compact_each_row_format_preserves_quote_and_comment_scanning() {
    let database = SharedDatabase::default();

    assert_response(
        &exchange(
            &database,
            &request(b"SELECT 'FORMAT JSONCompactEachRow' AS value;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"value","type":"String"}],"rows":[["FORMAT JSONCompactEachRow"]]}"#,
    );
    assert_response(
        &exchange(
            &database,
            &request(b"SELECT 1 AS value; -- FORMAT JSONCompactEachRow"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"value","type":"Int64"}],"rows":[[1]]}"#,
    );
    assert_response(
        &exchange(
            &database,
            &request(b"SELECT 'escaped ''FORMAT JSONCompactEachRow''' AS value;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"value","type":"String"}],"rows":[["escaped 'FORMAT JSONCompactEachRow'"]]}"#,
    );
}

#[test]
fn terminal_json_compact_each_row_format_rejects_selectors_after_authentication() {
    let database = SharedDatabase::default();
    let conflict_error = r#"{"error":"FORMAT JSONCompactEachRow clause cannot be combined with X-ClickHouse-Format header or default_format parameter"}"#;

    assert_response(
        &exchange(
            &database,
            &request_for_target_with_headers(
                "/query",
                b"SELECT 1 FORMAT JSONCompactEachRow;",
                "X-ClickHouse-Format: JSONCompactEachRow\r\n",
            ),
        ),
        "HTTP/1.1 400 Bad Request",
        conflict_error,
    );
    assert_response(
        &exchange(
            &database,
            b"GET /?query=SELECT+1+FORMAT+JSONCompactEachRow&default_format=JSONCompactEachRow HTTP/1.1\r\nHost: localhost\r\n\r\n",
        ),
        "HTTP/1.1 400 Bad Request",
        conflict_error,
    );

    let unauthorized = b"GET /?query=SELECT+1+FORMAT+JSONCompactEachRow&default_format=JSONCompactEachRow HTTP/1.1\r\nHost: localhost\r\n\r\n";
    assert_response(
        &authenticated_exchange(&database, "correct-token", unauthorized),
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"bearer authentication required"}"#,
    );
    let missing_key_response = clickhouse_key_exchange(&database, "correct-key", unauthorized);
    assert_response(
        &missing_key_response,
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"X-ClickHouse-Key authentication required"}"#,
    );
    assert_clickhouse_key_response_is_not_cacheable(&missing_key_response);
}

#[test]
fn terminal_csv_with_names_format_preserves_single_read_only_query_and_response_limits() {
    let database = SharedDatabase::default();
    database.execute("CREATE TABLE events (id Int64);").unwrap();

    assert_response(
        &exchange(
            &database,
            &request(b"SELECT 1; SELECT 2 FORMAT CSVWithNames;"),
        ),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"read-only query requires exactly one statement; found 2"}"#,
    );
    let authenticated_insert = request_for_target_with_headers(
        "/query",
        b"INSERT INTO events VALUES (1) FORMAT CSVWithNames;",
        "Authorization: Bearer correct-token\r\n",
    );
    assert_response(
        &authenticated_exchange(&database, "correct-token", &authenticated_insert),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"read-only query accepts only SELECT, SHOW DATABASES, SHOW SETTINGS, SHOW FUNCTIONS, SHOW TABLES, SHOW CREATE TABLE, DESCRIBE TABLE, or EXISTS TABLE; found INSERT"}"#,
    );
    assert_response(
        &exchange(&database, &request(b"SELECT id FROM events;")),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"}],"rows":[]}"#,
    );

    let sql = format!(
        "SELECT '{}' AS value FORMAT CSVWithNames;",
        "x".repeat(1_000)
    );
    let limits = HttpQueryLimits {
        max_response_bytes: 512,
        ..HttpQueryLimits::default()
    };
    let mut response = Vec::new();
    handle_http_query_with_limits(
        &database,
        Cursor::new(request(sql.as_bytes())),
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
fn terminal_json_format_retains_the_complete_response_limit() {
    let database = SharedDatabase::default();
    let sql = format!("SELECT '{}' AS value FORMAT JSON;", "x".repeat(1_000));
    let limits = HttpQueryLimits {
        max_response_bytes: 512,
        ..HttpQueryLimits::default()
    };
    let mut response = Vec::new();

    handle_http_query_with_limits(
        &database,
        Cursor::new(request(sql.as_bytes())),
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
fn terminal_json_each_row_format_retains_the_complete_response_limit() {
    let database = SharedDatabase::default();
    let sql = format!(
        "SELECT '{}' AS value FORMAT JSONEachRow;",
        "x".repeat(1_000)
    );
    let limits = HttpQueryLimits {
        max_response_bytes: 512,
        ..HttpQueryLimits::default()
    };
    let mut response = Vec::new();

    handle_http_query_with_limits(
        &database,
        Cursor::new(request(sql.as_bytes())),
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
fn terminal_json_compact_each_row_format_retains_the_complete_response_limit() {
    let database = SharedDatabase::default();
    let sql = format!(
        "SELECT '{}' AS value FORMAT JSONCompactEachRow;",
        "x".repeat(1_000)
    );
    let limits = HttpQueryLimits {
        max_response_bytes: 512,
        ..HttpQueryLimits::default()
    };
    let mut response = Vec::new();

    handle_http_query_with_limits(
        &database,
        Cursor::new(request(sql.as_bytes())),
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
fn terminal_tab_separated_format_retains_the_complete_response_limit() {
    let database = SharedDatabase::default();
    let sql = format!(
        "SELECT '{}' AS value FORMAT TabSeparated;",
        "x".repeat(1_000)
    );
    let limits = HttpQueryLimits {
        max_response_bytes: 512,
        ..HttpQueryLimits::default()
    };
    let mut response = Vec::new();

    handle_http_query_with_limits(
        &database,
        Cursor::new(request(sql.as_bytes())),
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
fn url_encoded_post_query_accepts_absent_or_zero_length_and_every_default_format() {
    let database = SharedDatabase::default();
    let cases: &[(&[u8], &str, &[u8])] = &[
        (
            b"POST /?default_format=JSON&query=SELECT+7+AS+value%3B&database=default HTTP/1.1\r\nHost: localhost\r\n\r\n",
            "application/json",
            br#"{"columns":[{"name":"value","type":"Int64"}],"rows":[[7]]}"#,
        ),
        (
            b"POST /?query=SELECT+7+AS+value%3B&default_format=CSV HTTP/1.1\r\nHost: localhost\r\n\r\n",
            "text/csv; charset=utf-8",
            b"7\n",
        ),
        (
            b"POST /?query=SELECT+7+AS+value%3B&default_format=CSVWithNames HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
            "text/csv; charset=utf-8",
            b"value\n7\n",
        ),
        (
            b"POST /?query=SELECT+7+AS+value%3B&default_format=TabSeparated HTTP/1.1\r\nHost: localhost\r\n\r\n",
            "text/tab-separated-values; charset=utf-8",
            b"7\n",
        ),
        (
            b"POST /?database=default&default_format=TabSeparatedWithNames&query=SELECT+7+AS+value%3B HTTP/1.1\r\nHost: localhost\r\n\r\n",
            "text/tab-separated-values; charset=utf-8",
            b"value\n7\n",
        ),
        (
            b"POST /?default_format=JSONEachRow&query=SELECT+7+AS+value%3B HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
            "application/json",
            b"{\"value\":7}\n",
        ),
        (
            b"POST /?query=SELECT+7+AS+value%3B&default_format=JSONCompactEachRow HTTP/1.1\r\nHost: localhost\r\n\r\n",
            "application/json",
            b"[7]\n",
        ),
    ];

    for (request, content_type, body) in cases {
        assert_response_with_content_type(
            &exchange(&database, request),
            "HTTP/1.1 200 OK",
            content_type,
            body,
        );
    }
}

#[test]
fn url_encoded_post_query_reuses_authentication_database_and_header_format() {
    let database = SharedDatabase::default();
    let missing_bearer = b"POST /?query=SELECT+7%3B&database=analytics HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1\r\n\r\nx";
    assert_response(
        &authenticated_exchange(&database, "correct-token", missing_bearer),
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"bearer authentication required"}"#,
    );

    let bearer = b"POST /?database=default&query=SELECT+7+AS+value%3B HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer correct-token\r\nX-ClickHouse-Database: default\r\nX-ClickHouse-Format: JSONEachRow\r\nContent-Length: 0\r\n\r\n";
    assert_response(
        &authenticated_exchange(&database, "correct-token", bearer),
        "HTTP/1.1 200 OK",
        "{\"value\":7}\n",
    );

    let key_response = clickhouse_key_exchange(
        &database,
        "correct-key",
        b"POST /?query=SELECT+7+AS+value%3B&default_format=CSVWithNames HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: correct-key\r\n\r\n",
    );
    assert_response_with_content_type(
        &key_response,
        "HTTP/1.1 200 OK",
        "text/csv; charset=utf-8",
        b"value\n7\n",
    );
    assert_clickhouse_key_response_is_not_cacheable(&key_response);

    let tab_separated = clickhouse_key_exchange(
        &database,
        "correct-key",
        b"POST /?query=SELECT+7+AS+value%3B&default_format=TabSeparated HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: correct-key\r\n\r\n",
    );
    assert_response_with_content_type(
        &tab_separated,
        "HTTP/1.1 200 OK",
        "text/tab-separated-values; charset=utf-8",
        b"7\n",
    );
    assert_clickhouse_key_response_is_not_cacheable(&tab_separated);
}

#[test]
fn url_encoded_post_query_rejects_bodies_conflicts_and_invalid_parameters() {
    let database = SharedDatabase::default();
    let body_request =
        b"POST /?query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1\r\n\r\nx";
    let mut input = Cursor::new(body_request);
    let mut response = Vec::new();
    handle_http_query(&database, &mut input, &mut response).unwrap();
    assert_eq!(input.position(), (body_request.len() - 1) as u64);
    assert_response(
        &response,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"POST /?query= does not accept a request body"}"#,
    );

    let errors: &[(&[u8], &str)] = &[
        (
            b"POST /?query=SELECT+1%3B&default_format=JSONEachRow HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Format: CSVWithNames\r\n\r\n",
            r#"{"error":"default_format parameter cannot be combined with X-ClickHouse-Format header"}"#,
        ),
        (
            b"POST /?query=SELECT+1%3B&default_format=CSV HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Format: CSV\r\n\r\n",
            r#"{"error":"default_format parameter cannot be combined with X-ClickHouse-Format header"}"#,
        ),
        (
            b"POST /?query=SELECT+1%3B&default_format=TabSeparated HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Format: TabSeparated\r\n\r\n",
            r#"{"error":"default_format parameter cannot be combined with X-ClickHouse-Format header"}"#,
        ),
        (
            b"POST /?database=analytics&query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\n\r\n",
            r#"{"error":"database query parameter must be default"}"#,
        ),
        (
            b"POST /?query=SELECT+1%3B&default_format=XML HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
            r#"{"error":"unsupported default_format parameter"}"#,
        ),
        (
            b"POST /?query=SELECT+1%3B&default_format=csv HTTP/1.1\r\nHost: localhost\r\n\r\n",
            r#"{"error":"unsupported default_format parameter"}"#,
        ),
        (
            b"POST /?query=SELECT+1%3B&default_format=tabseparated HTTP/1.1\r\nHost: localhost\r\n\r\n",
            r#"{"error":"unsupported default_format parameter"}"#,
        ),
        (
            b"POST /?query=SELECT+1%3B&format=JSON HTTP/1.1\r\nHost: localhost\r\n\r\n",
            r#"{"error":"POST query target contains an unknown parameter"}"#,
        ),
        (
            b"POST /?database=default HTTP/1.1\r\nHost: localhost\r\n\r\n",
            r#"{"error":"POST query target must contain exactly one query parameter"}"#,
        ),
        (
            b"POST /?query= HTTP/1.1\r\nHost: localhost\r\n\r\n",
            r#"{"error":"POST query parameters must have nonempty names and values"}"#,
        ),
    ];
    for (request, expected_body) in errors {
        assert_response(
            &exchange(&database, request),
            "HTTP/1.1 400 Bad Request",
            expected_body,
        );
    }

    let mut limited_response = Vec::new();
    handle_http_query_with_limits(
        &database,
        Cursor::new(b"POST /?query=SELECT+10%3B HTTP/1.1\r\nHost: localhost\r\n\r\n".as_slice()),
        &mut limited_response,
        HttpQueryLimits {
            max_sql_bytes: b"SELECT 10;".len() - 1,
            ..HttpQueryLimits::default()
        },
    )
    .unwrap();
    assert_response(
        &limited_response,
        "HTTP/1.1 413 Payload Too Large",
        r#"{"error":"SQL query exceeds configured byte limit"}"#,
    );
}

#[test]
fn get_query_accepts_percent_decoded_default_database_in_either_order() {
    let database = SharedDatabase::default();
    let expected = r#"{"columns":[{"name":"value","type":"Int64"}],"rows":[[7]]}"#;
    let requests: &[&[u8]] = &[
        b"GET /?query=SELECT+7+AS+value%3B&database=default HTTP/1.1\r\nHost: localhost\r\n\r\n",
        b"GET /?data%62ase=def%61ult&%71uery=SELECT+7+AS+value%3B HTTP/1.1\r\nHost: localhost\r\n\r\n",
    ];

    for request in requests {
        assert_response(&exchange(&database, request), "HTTP/1.1 200 OK", expected);
    }
}

#[test]
fn get_default_format_selects_every_writer_with_encoded_parameters_in_any_order() {
    let database = SharedDatabase::default();
    let cases: &[(&[u8], &str, &[u8])] = &[
        (
            b"GET /?default_format=JSON&query=SELECT+7+AS+value%3B&database=default HTTP/1.1\r\nHost: localhost\r\n\r\n",
            "application/json",
            br#"{"columns":[{"name":"value","type":"Int64"}],"rows":[[7]]}"#,
        ),
        (
            b"GET /?query=SELECT+7+AS+value%3B&default_format=CSV&database=default HTTP/1.1\r\nHost: localhost\r\n\r\n",
            "text/csv; charset=utf-8",
            b"7\n",
        ),
        (
            b"GET /?query=SELECT+7+AS+value%3B&default_format=CSVWithNames&database=default HTTP/1.1\r\nHost: localhost\r\n\r\n",
            "text/csv; charset=utf-8",
            b"value\n7\n",
        ),
        (
            b"GET /?query=SELECT+7+AS+value%3B&default_format=TabSeparated&database=default HTTP/1.1\r\nHost: localhost\r\n\r\n",
            "text/tab-separated-values; charset=utf-8",
            b"7\n",
        ),
        (
            b"GET /?database=default&query=SELECT+7+AS+value%3B&default_format=TabSeparatedWithNames HTTP/1.1\r\nHost: localhost\r\n\r\n",
            "text/tab-separated-values; charset=utf-8",
            b"value\n7\n",
        ),
        (
            b"GET /?default%5Fformat=JSON%45achRow&database=def%61ult&%71uery=SELECT+7+AS+value%3B HTTP/1.1\r\nHost: localhost\r\n\r\n",
            "application/json",
            b"{\"value\":7}\n",
        ),
        (
            b"GET /?%71uery=SELECT+7+AS+value%3B&database=default&%64efault_format=JSONCompact%45achRow HTTP/1.1\r\nHost: localhost\r\n\r\n",
            "application/json",
            b"[7]\n",
        ),
    ];

    for (request, content_type, body) in cases {
        assert_response_with_content_type(
            &exchange(&database, request),
            "HTTP/1.1 200 OK",
            content_type,
            body,
        );
    }
}

#[test]
fn get_default_database_parameter_executes_length_utf8_projection() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE samples (label String); \
             INSERT INTO samples VALUES ('é'), ('東京'), ('👨‍👩‍👧‍👦');",
        )
        .unwrap();
    let request = b"GET /?database=default&query=SELECT+lengthUTF8%28label%29+AS+characters+FROM+samples+ORDER+BY+characters%3B HTTP/1.1\r\nHost: localhost\r\n\r\n";

    assert_response(
        &exchange(&database, request),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"characters","type":"Int64"}],"rows":[[1],[2],[7]]}"#,
    );
}

#[test]
fn get_database_parameter_coexists_with_headers_and_both_authentication_modes() {
    let database = SharedDatabase::default();
    let bearer_request = b"GET /?query=SELECT+7+AS+value%3B&database=default HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer correct-token\r\nX-ClickHouse-Database: default\r\nX-ClickHouse-Format: CSVWithNames\r\n\r\n";
    assert_response_with_content_type(
        &authenticated_exchange(&database, "correct-token", bearer_request),
        "HTTP/1.1 200 OK",
        "text/csv; charset=utf-8",
        b"value\n7\n",
    );

    let key_response = clickhouse_key_exchange(
        &database,
        "correct-key",
        b"GET /?database=default&query=SELECT+7+AS+value%3B HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: correct-key\r\nX-ClickHouse-Database: default\r\n\r\n",
    );
    assert_response(
        &key_response,
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"value","type":"Int64"}],"rows":[[7]]}"#,
    );
    assert_clickhouse_key_response_is_not_cacheable(&key_response);
}

#[test]
fn get_parameter_validation_follows_authentication_and_precedes_database_access() {
    let inner = Arc::new(RwLock::new(Database::new()));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let poisoner = thread::spawn(move || {
        let _guard = inner.write().unwrap();
        panic!("poison the database lock");
    });
    assert!(poisoner.join().is_err());

    let missing_bearer =
        b"GET /?query=SELECT+1%3B&database=analytics HTTP/1.1\r\nHost: localhost\r\n\r\n";
    assert_response(
        &authenticated_exchange(&database, "correct-token", missing_bearer),
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"bearer authentication required"}"#,
    );
    let missing_key = b"GET /?unknown=value&query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\n\r\n";
    let missing_key_response = clickhouse_key_exchange(&database, "correct-key", missing_key);
    assert_response(
        &missing_key_response,
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"X-ClickHouse-Key authentication required"}"#,
    );
    assert_clickhouse_key_response_is_not_cacheable(&missing_key_response);

    let invalid_parameters: &[(&[u8], &str)] = &[
        (
            b"GET /?query=SELECT+1%3B&database= HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer correct-token\r\n\r\n",
            r#"{"error":"GET query parameters must have nonempty names and values"}"#,
        ),
        (
            b"GET /?query=SELECT+1%3B&&database=default HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer correct-token\r\n\r\n",
            r#"{"error":"GET query parameters must have nonempty names and values"}"#,
        ),
        (
            b"GET /?query=SELECT+1%3B&database=default&data%62ase=default HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer correct-token\r\n\r\n",
            r#"{"error":"duplicate database parameter"}"#,
        ),
        (
            b"GET /?query=SELECT+1%3B&%71uery=SELECT+2%3B HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer correct-token\r\n\r\n",
            r#"{"error":"duplicate query parameter"}"#,
        ),
        (
            b"GET /?query=SELECT+1%3B&format=CSV HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer correct-token\r\n\r\n",
            r#"{"error":"GET query target contains an unknown parameter"}"#,
        ),
        (
            b"GET /?database=analytics&query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer correct-token\r\n\r\n",
            r#"{"error":"database query parameter must be default"}"#,
        ),
        (
            b"GET /?database=default HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer correct-token\r\n\r\n",
            r#"{"error":"GET query target must contain exactly one query parameter"}"#,
        ),
        (
            b"GET /?query=&database=default HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer correct-token\r\n\r\n",
            r#"{"error":"GET query parameters must have nonempty names and values"}"#,
        ),
    ];

    for (request, expected_body) in invalid_parameters {
        assert_response(
            &authenticated_exchange(&database, "correct-token", request),
            "HTTP/1.1 400 Bad Request",
            expected_body,
        );
    }
}

#[test]
fn get_default_format_rejection_follows_authentication_and_precedes_database_access() {
    let inner = Arc::new(RwLock::new(Database::new()));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let poisoner = thread::spawn(move || {
        let _guard = inner.write().unwrap();
        panic!("poison the database lock");
    });
    assert!(poisoner.join().is_err());

    let unsupported_without_bearer =
        b"GET /?query=SELECT+1%3B&default_format=XML HTTP/1.1\r\nHost: localhost\r\n\r\n";
    assert_response(
        &authenticated_exchange(&database, "correct-token", unsupported_without_bearer),
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"bearer authentication required"}"#,
    );

    let conflicting_without_key = b"GET /?query=SELECT+1%3B&default_format=JSON HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Format: CSVWithNames\r\n\r\n";
    let missing_key_response =
        clickhouse_key_exchange(&database, "correct-key", conflicting_without_key);
    assert_response(
        &missing_key_response,
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"X-ClickHouse-Key authentication required"}"#,
    );
    assert_clickhouse_key_response_is_not_cacheable(&missing_key_response);

    let invalid_bearer_requests: &[(&[u8], &str)] = &[
        (
            b"GET /?query=SELECT+1%3B&default_format=JSON&default%5Fformat=JSONEachRow HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer correct-token\r\n\r\n",
            r#"{"error":"duplicate default_format parameter"}"#,
        ),
        (
            b"GET /?default_format=%58ML&query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer correct-token\r\n\r\n",
            r#"{"error":"unsupported default_format parameter"}"#,
        ),
        (
            b"GET /?default_format=JSON&query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer correct-token\r\nX-ClickHouse-Format: JSONEachRow\r\n\r\n",
            r#"{"error":"default_format parameter cannot be combined with X-ClickHouse-Format header"}"#,
        ),
    ];

    for (request, expected_body) in invalid_bearer_requests {
        assert_response(
            &authenticated_exchange(&database, "correct-token", request),
            "HTTP/1.1 400 Bad Request",
            expected_body,
        );
    }

    let duplicate_with_key = b"GET /?default_format=JSONEachRow&query=SELECT+1%3B&%64efault_format=JSON HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: correct-key\r\n\r\n";
    let duplicate_key_response =
        clickhouse_key_exchange(&database, "correct-key", duplicate_with_key);
    assert_response(
        &duplicate_key_response,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"duplicate default_format parameter"}"#,
    );
    assert_clickhouse_key_response_is_not_cacheable(&duplicate_key_response);
}

#[test]
fn get_database_and_default_format_do_not_count_toward_the_decoded_sql_limit() {
    let database = SharedDatabase::default();
    let request = b"GET /?database=def%61ult&default_format=JSON&query=SELECT+7%3B HTTP/1.1\r\nHost: localhost\r\n\r\n";
    let mut response = Vec::new();
    handle_http_query_with_limits(
        &database,
        Cursor::new(request),
        &mut response,
        HttpQueryLimits {
            max_sql_bytes: b"SELECT 7;".len(),
            ..HttpQueryLimits::default()
        },
    )
    .unwrap();
    assert_response(
        &response,
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"7","type":"Int64"}],"rows":[[7]]}"#,
    );

    response.clear();
    handle_http_query_with_limits(
        &database,
        Cursor::new(request),
        &mut response,
        HttpQueryLimits {
            max_sql_bytes: b"SELECT 7;".len() - 1,
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
fn default_database_header_wires_every_query_form_and_both_authentication_modes() {
    let database = SharedDatabase::default();
    let expected = r#"{"columns":[{"name":"value","type":"Int64"}],"rows":[[7]]}"#;

    let root_post = request_for_target_with_headers(
        "/",
        b"SELECT 7 AS value;",
        "x-cLiCkHoUsE-dAtAbAsE:\tdefault \r\n",
    );
    assert_response(
        &exchange(&database, &root_post),
        "HTTP/1.1 200 OK",
        expected,
    );

    let (bearer_post, _) = request_with_authorization_for_target(
        "/query",
        b"SELECT 7 AS value;",
        "Authorization: Bearer correct-token\r\n\
         X-ClickHouse-Database: default\r\n",
    );
    assert_response(
        &authenticated_exchange(&database, "correct-token", &bearer_post),
        "HTTP/1.1 200 OK",
        expected,
    );

    assert_response(
        &exchange(
            &database,
            b"GET /?query=SELECT+7+AS+value%3B HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Database: default\r\n\r\n",
        ),
        "HTTP/1.1 200 OK",
        expected,
    );

    let key_response = clickhouse_key_exchange(
        &database,
        "correct-key",
        b"GET /?query=SELECT+7+AS+value%3B HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: correct-key\r\nx-clickhouse-database: default\r\n\r\n",
    );
    assert_response(&key_response, "HTTP/1.1 200 OK", expected);
    assert_clickhouse_key_response_is_not_cacheable(&key_response);
}

#[test]
fn database_header_rejects_empty_duplicate_and_non_default_query_values() {
    let database = SharedDatabase::default();
    let cases: &[(&[u8], &str)] = &[
        (
            b"GET /?query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Database:\r\n\r\n",
            r#"{"error":"X-ClickHouse-Database header must be default"}"#,
        ),
        (
            b"POST /query HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Database: analytics\r\nContent-Length: 9\r\n\r\nSELECT 1;",
            r#"{"error":"X-ClickHouse-Database header must be default"}"#,
        ),
        (
            b"POST / HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Database: DEFAULT\r\nContent-Length: 9\r\n\r\nSELECT 1;",
            r#"{"error":"X-ClickHouse-Database header must be default"}"#,
        ),
        (
            b"GET /?query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Database: default\r\nx-clickhouse-database: default\r\n\r\n",
            r#"{"error":"duplicate X-ClickHouse-Database header"}"#,
        ),
    ];

    for (request, expected_body) in cases {
        assert_response(
            &exchange(&database, request),
            "HTTP/1.1 400 Bad Request",
            expected_body,
        );
    }
}

#[test]
fn database_header_validation_follows_both_authentication_modes() {
    let database = SharedDatabase::default();
    let invalid_database = "X-ClickHouse-Database: analytics\r\n";

    let (bearer_request, bearer_body_offset) =
        request_with_authorization_for_target("/query", b"SELECT 1;", invalid_database);
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

    let (key_request, key_body_offset) = request_with_authorization_for_target(
        "/insert",
        b"INSERT INTO events VALUES (1);",
        "X-ClickHouse-Key: incorrect\r\n\
         X-ClickHouse-Database: default\r\n\
         x-clickhouse-database: default\r\n",
    );
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
    assert_clickhouse_key_response_is_not_cacheable(&key_response);
}

#[test]
fn database_header_rejection_precedes_format_body_and_database_access() {
    let inner = Arc::new(RwLock::new(Database::new()));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let poisoner = thread::spawn(move || {
        let _guard = inner.write().unwrap();
        panic!("poison the database lock");
    });
    assert!(poisoner.join().is_err());

    let (request, body_offset) = request_with_authorization_for_target(
        "/insert/events",
        b"id\n1\n",
        "Authorization: Bearer correct-token\r\n\
         X-ClickHouse-Database: analytics\r\n\
         X-ClickHouse-Format: unsupported\r\n",
    );
    let mut input = Cursor::new(request);
    let mut response = Vec::new();
    handle_http_query_with_bearer_token(&database, "correct-token", &mut input, &mut response)
        .unwrap();

    assert_eq!(input.position(), body_offset);
    assert_response(
        &response,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"X-ClickHouse-Database header must be default"}"#,
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
            r#"{"error":"GET query target contains an unknown parameter"}"#,
        ),
        (
            b"GET /?query=SELECT+1%3B&database=def%GGault HTTP/1.1\r\nHost: localhost\r\n\r\n",
            r#"{"error":"query parameter contains malformed percent encoding"}"#,
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
fn get_default_format_retains_the_complete_response_limit() {
    let database = SharedDatabase::default();
    let request = format!(
        "GET /?query=SELECT+%27{}%27+AS+value%3B&default_format=JSONCompactEachRow HTTP/1.1\r\nHost: localhost\r\n\r\n",
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
fn both_query_routes_return_both_csv_formats_for_all_value_types_and_empty_results() {
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
    let expected_rows = concat!(
        "-9223372036854775808,2.0,false,\"comma, \"\"quote\"\"\ncarriage\rsnow 雪\"\n",
        "7,-1.25,true,\n",
    );

    for target in ["/", "/query"] {
        for (format, expected, expected_null, expected_empty) in [
            (
                "CSV",
                expected_rows.to_owned(),
                "NULL,NULL,NULL,NULL\n".to_owned(),
                String::new(),
            ),
            (
                "CSVWithNames",
                format!("integer,score,active,label\n{expected_rows}"),
                "missing_integer,missing_float,missing_boolean,missing_string\nNULL,NULL,NULL,NULL\n"
                    .to_owned(),
                "integer,score,active,label\n".to_owned(),
            ),
        ] {
            let headers = format!("X-ClickHouse-Format: {format}\r\n");
            let typed_request = request_for_target_with_headers(
                target,
                b"SELECT integer, score, active, label FROM typed_values ORDER BY integer;",
                &headers,
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
                &headers,
            );
            assert_response_with_content_type(
                &exchange(&database, &null_request),
                "HTTP/1.1 200 OK",
                "text/csv; charset=utf-8",
                expected_null.as_bytes(),
            );

            let empty_request = request_for_target_with_headers(
                target,
                b"SELECT integer, score, active, label FROM empty_values;",
                &headers,
            );
            assert_response_with_content_type(
                &exchange(&database, &empty_request),
                "HTTP/1.1 200 OK",
                "text/csv; charset=utf-8",
                expected_empty.as_bytes(),
            );
        }
    }
}

#[test]
fn both_query_routes_return_both_tab_separated_formats_for_typed_escaped_and_empty_results() {
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
    let expected_rows = concat!(
        "-9223372036854775808\t2.0\tfalse\tslash\\\\tab\\tcarriage\\rline\\nnul\\0backspace\\bformfeed\\fapostrophe\\' snow 雪\n",
        "7\t-1.25\ttrue\t\n",
    );

    for target in ["/", "/query"] {
        for (format, expected, expected_null, expected_empty) in [
            (
                "TabSeparated",
                expected_rows.to_owned(),
                "\\N\t\\N\t\\N\t\\N\n".to_owned(),
                String::new(),
            ),
            (
                "TabSeparatedWithNames",
                format!("integer\tscore\tactive\tlabel\n{expected_rows}"),
                "missing_integer\tmissing_float\tmissing_boolean\tmissing_string\n\\N\t\\N\t\\N\t\\N\n"
                    .to_owned(),
                "integer\tscore\tactive\tlabel\n".to_owned(),
            ),
        ] {
            let headers = format!("X-ClickHouse-Format: {format}\r\n");
            let typed_request = request_for_target_with_headers(
                target,
                b"SELECT integer, score, active, label FROM typed_values ORDER BY integer;",
                &headers,
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
                &headers,
            );
            assert_response_with_content_type(
                &exchange(&database, &null_request),
                "HTTP/1.1 200 OK",
                "text/tab-separated-values; charset=utf-8",
                expected_null.as_bytes(),
            );

            let empty_request = request_for_target_with_headers(
                target,
                b"SELECT integer, score, active, label FROM empty_values;",
                &headers,
            );
            assert_response_with_content_type(
                &exchange(&database, &empty_request),
                "HTTP/1.1 200 OK",
                "text/tab-separated-values; charset=utf-8",
                expected_empty.as_bytes(),
            );
        }
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
fn bearer_authenticated_queries_honor_both_tab_separated_formats_on_both_routes() {
    let database = SharedDatabase::default();
    let sql = b"SELECT -7 AS integer;";

    for (format, expected) in [
        ("TabSeparated", b"-7\n".as_slice()),
        ("TabSeparatedWithNames", b"integer\n-7\n".as_slice()),
    ] {
        for target in ["/", "/query"] {
            let format_header = format!("X-ClickHouse-Format: {format}\r\n");
            let unauthorized = request_for_target_with_headers(target, sql, &format_header);
            assert_response(
                &authenticated_exchange(&database, "correct-token", &unauthorized),
                "HTTP/1.1 401 Unauthorized",
                r#"{"error":"bearer authentication required"}"#,
            );

            let authorization_headers =
                format!("Authorization: Bearer correct-token\r\nX-ClickHouse-Format: {format}\r\n");
            let (authorized, _) =
                request_with_authorization_for_target(target, sql, &authorization_headers);
            assert_response_with_content_type(
                &authenticated_exchange(&database, "correct-token", &authorized),
                "HTTP/1.1 200 OK",
                "text/tab-separated-values; charset=utf-8",
                expected,
            );
        }
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
        concat!(
            "X-ClickHouse-Format: TabSeparated\r\n",
            "X-ClickHouse-Format: TabSeparated\r\n",
        ),
        concat!(
            "X-ClickHouse-Format: TabSeparated\r\n",
            "X-ClickHouse-Format: TabSeparatedWithNames\r\n",
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
            "csv",
            "Csv",
            "tabseparated",
            "Tabseparated",
            "tabseparatedwithnames",
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
fn headerless_csv_honors_the_exact_complete_response_cap() {
    let database = SharedDatabase::default();
    let sql = format!("SELECT '{}' AS value;", "x".repeat(1_000));
    let request =
        request_for_target_with_headers("/query", sql.as_bytes(), "X-ClickHouse-Format: CSV\r\n");
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
    .expect("the exact complete headerless CSV response size is accepted");
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
fn headerless_tab_separated_honors_the_exact_complete_response_cap() {
    let database = SharedDatabase::default();
    let sql = format!("SELECT '{}' AS value;", "x".repeat(1_000));
    let request = request_for_target_with_headers(
        "/query",
        sql.as_bytes(),
        "X-ClickHouse-Format: TabSeparated\r\n",
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
    .expect("the exact complete headerless TSV response size is accepted");
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
    assert_ok_metrics_response(&metrics_response, 1, 1, 1, 8, &[("events", 1, 8)]);
    assert_clickhouse_key_response_is_not_cacheable(&metrics_response);
}

#[test]
fn authenticated_read_only_modes_wire_queries_and_operational_routes() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE events (id Int64); INSERT INTO events VALUES (7);")
        .unwrap();

    let bearer_query = request_for_target_with_headers(
        "/query",
        b"SELECT id FROM events;",
        "Authorization: Bearer read-token\r\n",
    );
    assert_response(
        &read_only_bearer_exchange(&database, "read-token", &bearer_query),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"}],"rows":[[7]]}"#,
    );

    let key_query = read_only_clickhouse_key_exchange(
        &database,
        "read-key",
        b"GET /?query=SELECT+id+FROM+events%3B HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: read-key\r\n\r\n",
    );
    assert_response(
        &key_query,
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"}],"rows":[[7]]}"#,
    );
    assert_clickhouse_key_response_is_not_cacheable(&key_query);

    for target in ["/ping", "/ready"] {
        let bearer_request = format!(
            "GET {target} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer read-token\r\n\r\n"
        );
        assert_ok_health_response(&read_only_bearer_exchange(
            &database,
            "read-token",
            bearer_request.as_bytes(),
        ));

        let key_request = format!(
            "GET {target} HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: read-key\r\n\r\n"
        );
        let response =
            read_only_clickhouse_key_exchange(&database, "read-key", key_request.as_bytes());
        assert_ok_health_response(&response);
        assert_clickhouse_key_response_is_not_cacheable(&response);
    }

    assert_ok_metrics_response(
        &read_only_bearer_exchange(
            &database,
            "read-token",
            b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer read-token\r\n\r\n",
        ),
        1,
        1,
        1,
        8,
        &[("events", 1, 8)],
    );
    let key_metrics = read_only_clickhouse_key_exchange(
        &database,
        "read-key",
        b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: read-key\r\n\r\n",
    );
    assert_ok_metrics_response(&key_metrics, 1, 1, 1, 8, &[("events", 1, 8)]);
    assert_clickhouse_key_response_is_not_cacheable(&key_metrics);
}

#[test]
fn authenticated_read_only_modes_reject_every_insert_surface_without_locking() {
    let inner = Arc::new(RwLock::new(Database::new()));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let poisoner = thread::spawn(move || {
        let _guard = inner.write().unwrap();
        panic!("poison the database lock");
    });
    assert!(poisoner.join().is_err());

    for (target, body) in [
        ("/insert", &b"INSERT INTO events VALUES (1);"[..]),
        ("/insert/events", &b"id\n1\n"[..]),
    ] {
        let (missing_bearer, body_offset) = request_with_authorization_for_target(target, body, "");
        let mut input = Cursor::new(missing_bearer);
        let mut response = Vec::new();
        handle_http_query_read_only_with_bearer_token(
            &database,
            "read-token",
            &mut input,
            &mut response,
        )
        .unwrap();
        assert_eq!(input.position(), body_offset);
        assert_response(
            &response,
            "HTTP/1.1 401 Unauthorized",
            r#"{"error":"bearer authentication required"}"#,
        );

        let (authorized_bearer, body_offset) = request_with_authorization_for_target(
            target,
            body,
            "Authorization: Bearer read-token\r\n",
        );
        let mut input = Cursor::new(authorized_bearer);
        response.clear();
        handle_http_query_read_only_with_bearer_token(
            &database,
            "read-token",
            &mut input,
            &mut response,
        )
        .unwrap();
        assert_eq!(input.position(), body_offset);
        assert_response(
            &response,
            "HTTP/1.1 404 Not Found",
            r#"{"error":"request target must be / or /query"}"#,
        );

        let (missing_key, body_offset) = request_with_authorization_for_target(target, body, "");
        let mut input = Cursor::new(missing_key);
        response.clear();
        handle_http_query_read_only_with_clickhouse_key(
            &database,
            "read-key",
            &mut input,
            &mut response,
        )
        .unwrap();
        assert_eq!(input.position(), body_offset);
        assert_response(
            &response,
            "HTTP/1.1 401 Unauthorized",
            r#"{"error":"X-ClickHouse-Key authentication required"}"#,
        );
        assert_clickhouse_key_response_is_not_cacheable(&response);

        let (authorized_key, body_offset) =
            request_with_authorization_for_target(target, body, "X-ClickHouse-Key: read-key\r\n");
        let mut input = Cursor::new(authorized_key);
        response.clear();
        handle_http_query_read_only_with_clickhouse_key(
            &database,
            "read-key",
            &mut input,
            &mut response,
        )
        .unwrap();
        assert_eq!(input.position(), body_offset);
        assert_response(
            &response,
            "HTTP/1.1 404 Not Found",
            r#"{"error":"request target must be / or /query"}"#,
        );
        assert_clickhouse_key_response_is_not_cacheable(&response);
    }

    let bearer_standard_insert = request_for_target_with_headers(
        "/query",
        b"INSERT INTO events VALUES (1);",
        "Authorization: Bearer read-token\r\n",
    );
    assert_response(
        &read_only_bearer_exchange(&database, "read-token", &bearer_standard_insert),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"read-only query accepts only SELECT, SHOW DATABASES, SHOW SETTINGS, SHOW FUNCTIONS, SHOW TABLES, SHOW CREATE TABLE, DESCRIBE TABLE, or EXISTS TABLE; found INSERT"}"#,
    );

    let key_parameterized_insert = read_only_clickhouse_key_exchange(
        &database,
        "read-key",
        b"POST /?query=INSERT+INTO+events+VALUES+%281%29%3B HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: read-key\r\n\r\n",
    );
    assert_response(
        &key_parameterized_insert,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"read-only query accepts only SELECT, SHOW DATABASES, SHOW SETTINGS, SHOW FUNCTIONS, SHOW TABLES, SHOW CREATE TABLE, DESCRIBE TABLE, or EXISTS TABLE; found INSERT"}"#,
    );
    assert_clickhouse_key_response_is_not_cacheable(&key_parameterized_insert);
}

#[test]
fn authenticated_read_only_modes_reject_string_alter_updates_without_mutation() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE events (id Int64, label String, category String); \
             INSERT INTO events VALUES (1, 'original', 'queued');",
        )
        .unwrap();
    let update = b"ALTER TABLE events UPDATE label = 'changed' WHERE category = 'queued';";
    let read_only_error = r#"{"error":"read-only query accepts only SELECT, SHOW DATABASES, SHOW SETTINGS, SHOW FUNCTIONS, SHOW TABLES, SHOW CREATE TABLE, DESCRIBE TABLE, or EXISTS TABLE; found ALTER TABLE"}"#;

    let bearer_request =
        request_for_target_with_headers("/query", update, "Authorization: Bearer read-token\r\n");
    assert_response(
        &read_only_bearer_exchange(&database, "read-token", &bearer_request),
        "HTTP/1.1 400 Bad Request",
        read_only_error,
    );

    let key_request =
        request_for_target_with_headers("/query", update, "X-ClickHouse-Key: read-key\r\n");
    let key_response = read_only_clickhouse_key_exchange(&database, "read-key", &key_request);
    assert_response(&key_response, "HTTP/1.1 400 Bad Request", read_only_error);
    assert_clickhouse_key_response_is_not_cacheable(&key_response);

    assert_response(
        &exchange(
            &database,
            &request(b"SELECT id, label, category FROM events;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"},{"name":"label","type":"String"},{"name":"category","type":"String"}],"rows":[[1,"original","queued"]]}"#,
    );
}

#[test]
fn authenticated_read_only_modes_preserve_complete_response_limits() {
    let database = SharedDatabase::default();
    let sql = format!("SELECT '{}' AS value;", "x".repeat(256));
    let bearer_request = request_for_target_with_headers(
        "/query",
        sql.as_bytes(),
        "Authorization: Bearer read-token\r\n",
    );
    let unrestricted = read_only_bearer_exchange(&database, "read-token", &bearer_request);
    let mut exact = Vec::new();
    handle_http_query_read_only_with_bearer_token_and_limits(
        &database,
        "read-token",
        Cursor::new(&bearer_request),
        &mut exact,
        HttpQueryLimits {
            max_response_bytes: unrestricted.len(),
            ..HttpQueryLimits::default()
        },
    )
    .unwrap();
    assert_eq!(exact, unrestricted);

    let mut capped = Vec::new();
    handle_http_query_read_only_with_bearer_token_and_limits(
        &database,
        "read-token",
        Cursor::new(&bearer_request),
        &mut capped,
        HttpQueryLimits {
            max_response_bytes: unrestricted.len() - 1,
            ..HttpQueryLimits::default()
        },
    )
    .expect("the fixed response-limit error fits");
    assert_response(
        &capped,
        "HTTP/1.1 500 Internal Server Error",
        r#"{"error":"response exceeds configured byte limit"}"#,
    );

    let key_request = b"GET /?query=SELECT+7+AS+value%3B HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: read-key\r\n\r\n";
    let mut no_output = Vec::new();
    let error = handle_http_query_read_only_with_clickhouse_key_and_limits(
        &database,
        "read-key",
        Cursor::new(key_request),
        &mut no_output,
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
    assert!(no_output.is_empty());
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
            b"PUT /?query=SELECT+1%3B HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
        ),
        "HTTP/1.1 405 Method Not Allowed",
        r#"{"error":"method must be GET or POST for /?query="}"#,
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
fn metrics_preflights_the_complete_response_limit_before_materializing_samples() {
    let worker_cap = NonZeroUsize::new(usize::MAX).unwrap();
    let database = SharedDatabase::with_global_aggregate_worker_cap(worker_cap);
    database
        .execute(
            "CREATE TABLE Observed (id Int64); \
             INSERT INTO Observed VALUES \
                 (1), (2), (3), (4), (5), (6), \
                 (7), (8), (9), (10), (11), (12);",
        )
        .unwrap();
    assert!(matches!(
        database
            .create_int64_min_max_index(
                "Observed",
                "id",
                Int64MinMaxIndexLimits::new(4, 3, usize::MAX),
            )
            .unwrap(),
        Int64MinMaxIndexAdmission::Created(_)
    ));
    database
        .query("SELECT id FROM Observed WHERE id = 12")
        .expect("indexed query succeeds");
    let request = b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n";
    let expected_response = exchange(&database, request);
    assert_ok_metrics_response_with_expectation(
        &expected_response,
        ExpectedMetrics {
            tables: 1,
            columns: 1,
            retained_rows: 12,
            retained_value_bytes: 96,
            global_aggregate_worker_cap: worker_cap.get(),
            index_pruning: (1, 2),
            table_metrics: &[("Observed", 12, 96)],
        },
    );

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
        r#"{"error":"read-only query accepts only SELECT, SHOW DATABASES, SHOW SETTINGS, SHOW FUNCTIONS, SHOW TABLES, SHOW CREATE TABLE, DESCRIBE TABLE, or EXISTS TABLE; found DROP TABLE"}"#,
    );
    assert_response(
        &exchange(
            &database,
            b"GET /?query=DROP+TABLE+retained%3B HTTP/1.1\r\nHost: localhost\r\n\r\n",
        ),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"read-only query accepts only SELECT, SHOW DATABASES, SHOW SETTINGS, SHOW FUNCTIONS, SHOW TABLES, SHOW CREATE TABLE, DESCRIBE TABLE, or EXISTS TABLE; found DROP TABLE"}"#,
    );
    assert_response(
        &exchange(
            &database,
            b"POST /?query=DROP+TABLE+retained%3B HTTP/1.1\r\nHost: localhost\r\n\r\n",
        ),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"read-only query accepts only SELECT, SHOW DATABASES, SHOW SETTINGS, SHOW FUNCTIONS, SHOW TABLES, SHOW CREATE TABLE, DESCRIBE TABLE, or EXISTS TABLE; found DROP TABLE"}"#,
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
fn default_database_header_wires_both_authenticated_insert_routes() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE events (id Int64, label String);")
        .unwrap();

    let (sql_insert, _) = request_with_authorization_for_target(
        "/insert",
        b"INSERT INTO events VALUES (1, 'sql');",
        "Authorization: Bearer correct-token\r\n\
         x-clickhouse-database: default\r\n",
    );
    assert_response_with_content_type(
        &authenticated_exchange(&database, "correct-token", &sql_insert),
        "HTTP/1.1 200 OK",
        "text/plain; charset=utf-8",
        b"",
    );

    let csv_insert = request_for_target_with_headers(
        "/insert/events",
        b"id,label\n2,csv\n",
        "X-ClickHouse-Key: correct-key\r\n\
         X-ClickHouse-Database: default\r\n",
    );
    let csv_response = clickhouse_key_exchange(&database, "correct-key", &csv_insert);
    assert_response_with_content_type(
        &csv_response,
        "HTTP/1.1 200 OK",
        "text/plain; charset=utf-8",
        b"",
    );
    assert_clickhouse_key_response_is_not_cacheable(&csv_response);

    assert_response(
        &exchange(
            &database,
            &request_for_target("/query", b"SELECT id, label FROM events ORDER BY id;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"},{"name":"label","type":"String"}],"rows":[[1,"sql"],[2,"csv"]]}"#,
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
fn http_insert_and_query_round_trip_sql_created_nullable_int64_values() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE Readings (measurement Nullable(Int64));")
        .unwrap();
    let (insert, _) = request_with_authorization_for_target(
        "/insert",
        b"INSERT INTO readings VALUES (7), (NULL), (-2);",
        "Authorization: Bearer correct-token\r\n",
    );

    assert_response_with_content_type(
        &authenticated_exchange(&database, "correct-token", &insert),
        "HTTP/1.1 200 OK",
        "text/plain; charset=utf-8",
        b"",
    );
    assert_response(
        &exchange(
            &database,
            &request_for_target(
                "/query",
                b"SELECT measurement FROM readings ORDER BY measurement ASC;",
            ),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"measurement","type":"Int64"}],"rows":[[null],[-2],[7]]}"#,
    );
    assert_response(
        &exchange(
            &database,
            &request_for_target(
                "/query",
                b"SELECT measurement FROM readings WHERE measurement iS nUlL;",
            ),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"measurement","type":"Int64"}],"rows":[[null]]}"#,
    );
    assert_response(
        &exchange(
            &database,
            &request_for_target(
                "/query",
                b"SELECT measurement FROM readings \
                  WHERE measurement IS NOT NULL ORDER BY measurement;",
            ),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"measurement","type":"Int64"}],"rows":[[-2],[7]]}"#,
    );
    assert_response(
        &exchange(
            &database,
            &request_for_target("/query", b"SHOW CREATE TABLE readings;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"statement","type":"String"}],"rows":[["CREATE TABLE Readings (measurement Nullable(Int64))"]]}"#,
    );

    let malformed = exchange(
        &database,
        &request_for_target(
            "/query",
            b"SELECT measurement FROM readings WHERE measurement IS NOT;",
        ),
    );
    let malformed = std::str::from_utf8(&malformed).expect("HTTP response is UTF-8");
    assert!(malformed.starts_with("HTTP/1.1 400 Bad Request\r\n"));
    assert!(malformed.contains("expected keyword NULL"));
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
fn insert_route_is_bearer_only_exact_and_standard_query_writes_require_authentication() {
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
    assert_response_with_content_type(
        &authenticated_exchange(&database, "correct-token", &query_request),
        "HTTP/1.1 200 OK",
        "text/plain; charset=utf-8",
        b"",
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
        r#"{"columns":[{"name":"id","type":"Int64"}],"rows":[[1]]}"#,
    );
}

#[test]
fn authenticated_standard_post_routes_insert_with_both_credential_modes() {
    let database = SharedDatabase::default();
    database.execute("CREATE TABLE events (id Int64);").unwrap();

    for (target, sql) in [
        (
            "/",
            &b"INSERT INTO events VALUES (1); INSERT INTO events VALUES (2);"[..],
        ),
        ("/query", &b"INSERT INTO events VALUES (3);"[..]),
    ] {
        let (request, _) = request_with_authorization_for_target(
            target,
            sql,
            "Authorization: Bearer correct-token\r\n",
        );
        assert_response_with_content_type(
            &authenticated_exchange(&database, "correct-token", &request),
            "HTTP/1.1 200 OK",
            "text/plain; charset=utf-8",
            b"",
        );
    }

    let key_response = clickhouse_key_exchange(
        &database,
        "correct-key",
        b"POST /?database=default&query=INSERT+INTO+events+VALUES+%284%29%3B HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: correct-key\r\nX-ClickHouse-Database: default\r\n\r\n",
    );
    assert_response_with_content_type(
        &key_response,
        "HTTP/1.1 200 OK",
        "text/plain; charset=utf-8",
        b"",
    );
    assert_clickhouse_key_response_is_not_cacheable(&key_response);

    assert_response(
        &exchange(
            &database,
            &request_for_target("/query", b"SELECT id FROM events ORDER BY id;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"}],"rows":[[1],[2],[3],[4]]}"#,
    );
}

#[test]
fn parameterized_csv_with_names_insert_works_with_both_credential_modes() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE events (id Int64, label String);")
        .unwrap();

    let bearer_request = request_for_target_with_headers(
        "/?query=INSERT+INTO+events+FORMAT+CSVWithNames",
        b"label,id\nbearer,1\n",
        "Authorization: Bearer correct-token\r\n",
    );
    assert_response_with_content_type(
        &authenticated_exchange(&database, "correct-token", &bearer_request),
        "HTTP/1.1 200 OK",
        "text/plain; charset=utf-8",
        b"",
    );

    let key_request = request_for_target_with_headers(
        "/?database=default&query=insert+into+events+format+csvwithnames%3B",
        b"id,label\n2,key\n",
        "X-ClickHouse-Key: correct-key\r\nX-ClickHouse-Database: default\r\n",
    );
    let key_response = clickhouse_key_exchange(&database, "correct-key", &key_request);
    assert_response_with_content_type(
        &key_response,
        "HTTP/1.1 200 OK",
        "text/plain; charset=utf-8",
        b"",
    );
    assert_clickhouse_key_response_is_not_cacheable(&key_response);

    assert_response(
        &exchange(
            &database,
            &request_for_target("/query", b"SELECT id, label FROM events ORDER BY id;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"},{"name":"label","type":"String"}],"rows":[[1,"bearer"],[2,"key"]]}"#,
    );
}

#[test]
fn parameterized_csv_with_names_insert_rejects_other_access_and_shapes_before_body() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE events (id Int64, label String);")
        .unwrap();
    let csv = b"id,label\n1,rejected\n";

    let (missing_credentials, body_offset) = request_with_authorization_for_target(
        "/?query=INSERT+INTO+events+FORMAT+CSVWithNames",
        csv,
        "",
    );
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

    let (selected_format, body_offset) = request_with_authorization_for_target(
        "/?query=INSERT+INTO+events+FORMAT+CSVWithNames",
        csv,
        "Authorization: Bearer correct-token\r\nX-ClickHouse-Format: CSVWithNames\r\n",
    );
    let mut input = Cursor::new(selected_format);
    response.clear();
    handle_http_query_with_bearer_token(&database, "correct-token", &mut input, &mut response)
        .unwrap();
    assert_eq!(input.position(), body_offset);
    assert_response(
        &response,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"CSVWithNames INSERT does not accept an output format selector"}"#,
    );

    let default_format = request_for_target_with_headers(
        "/?query=INSERT+INTO+events+FORMAT+CSVWithNames&default_format=JSON",
        csv,
        "X-ClickHouse-Key: correct-key\r\n",
    );
    let key_response = clickhouse_key_exchange(&database, "correct-key", &default_format);
    assert_response(
        &key_response,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"CSVWithNames INSERT does not accept an output format selector"}"#,
    );
    assert_clickhouse_key_response_is_not_cacheable(&key_response);

    let extra_sql = request_for_target_with_headers(
        "/?query=INSERT+INTO+events+FORMAT+CSVWithNames%3B+SELECT+1%3B",
        csv,
        "Authorization: Bearer correct-token\r\n",
    );
    assert_response(
        &authenticated_exchange(&database, "correct-token", &extra_sql),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"POST /?query= does not accept a request body"}"#,
    );

    let get_request = format!(
        "GET /?query=INSERT+INTO+events+FORMAT+CSVWithNames HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer correct-token\r\nContent-Length: {}\r\n\r\n",
        csv.len(),
    );
    let mut get_request = get_request.into_bytes();
    get_request.extend_from_slice(csv);
    assert_response(
        &authenticated_exchange(&database, "correct-token", &get_request),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"GET /?query= does not accept a request body"}"#,
    );

    let read_only_request = request_for_target_with_headers(
        "/?query=INSERT+INTO+events+FORMAT+CSVWithNames",
        csv,
        "Authorization: Bearer read-token\r\n",
    );
    assert_response(
        &read_only_bearer_exchange(&database, "read-token", &read_only_request),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"POST /?query= does not accept a request body"}"#,
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
fn parameterized_csv_with_names_insert_preserves_limits_and_late_rollback() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE events (id Int64, label String); \
             INSERT INTO events VALUES (9, 'existing');",
        )
        .unwrap();

    let late_error = request_for_target_with_headers(
        "/?query=INSERT+INTO+events+FORMAT+CSVWithNames",
        b"id,label\n1,valid\nwrong,late\n",
        "X-ClickHouse-Key: correct-key\r\n",
    );
    let late_response = clickhouse_key_exchange(&database, "correct-key", &late_error);
    assert_response(
        &late_response,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"database CSV ingestion failed: CSV field at line 3, column 1 is not a valid Int64"}"#,
    );
    assert_clickhouse_key_response_is_not_cacheable(&late_response);

    let oversized_csv = format!("id,label\n2,{}\n", "x".repeat(128)).into_bytes();
    let (oversized_request, body_offset) = request_with_authorization_for_target(
        "/?query=INSERT+INTO+events+FORMAT+CSVWithNames",
        &oversized_csv,
        "Authorization: Bearer correct-token\r\n",
    );
    let mut input = Cursor::new(&oversized_request);
    let mut response = Vec::new();
    handle_http_query_with_bearer_token_and_limits(
        &database,
        "correct-token",
        &mut input,
        &mut response,
        HttpQueryLimits {
            max_sql_bytes: oversized_csv.len() - 1,
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

    let mut input = Cursor::new(&oversized_request);
    response.clear();
    handle_http_query_with_bearer_token_and_limits(
        &database,
        "correct-token",
        &mut input,
        &mut response,
        HttpQueryLimits {
            csv_ingest_limits: CsvIngestLimits::new(oversized_csv.len() - 1, 10, 20),
            ..HttpQueryLimits::default()
        },
    )
    .unwrap();
    assert_eq!(input.position(), body_offset);
    assert_response(
        &response,
        "HTTP/1.1 400 Bad Request",
        &format!(
            r#"{{"error":"database CSV ingestion failed: CSV input is {} bytes, exceeding the limit of {} bytes"}}"#,
            oversized_csv.len(),
            oversized_csv.len() - 1,
        ),
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
fn parameterized_csv_with_names_insert_returns_503_without_waiting_for_a_reader() {
    let mut initial = Database::new();
    initial.execute("CREATE TABLE events (id Int64);").unwrap();
    let inner = Arc::new(RwLock::new(initial));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let mut reader = Some(inner.read().unwrap());
    let request = request_for_target_with_headers(
        "/?query=INSERT+INTO+events+FORMAT+CSVWithNames",
        b"id\n1\n",
        "X-ClickHouse-Key: correct-key\r\n",
    );
    let (sender, receiver) = mpsc::channel();
    let worker_database = database.clone();
    let worker = thread::spawn(move || {
        sender
            .send(clickhouse_key_exchange(
                &worker_database,
                "correct-key",
                &request,
            ))
            .unwrap();
    });

    let response = match receiver.recv_timeout(Duration::from_secs(2)) {
        Ok(response) => response,
        Err(error) => {
            drop(reader.take());
            worker.join().unwrap();
            panic!("parameterized HTTP CSV admission blocked behind a reader: {error}");
        }
    };
    assert_response(
        &response,
        "HTTP/1.1 503 Service Unavailable",
        r#"{"error":"database is unavailable"}"#,
    );
    assert_clickhouse_key_response_is_not_cacheable(&response);
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
fn parameterized_headerless_csv_insert_ingests_all_types_quoting_and_empty_input() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE typed_values (id Int64, score Float64, active Bool, label String);")
        .unwrap();

    let bearer_csv = concat!(
        "-9223372036854775808,2.5,true,\"comma, \"\"quoted\"\"\n",
        "next\"\r\n",
    )
    .as_bytes();
    let bearer_request = request_for_target_with_headers(
        "/?query=INSERT+INTO+typed_values+FORMAT+CSV",
        bearer_csv,
        "Authorization: Bearer correct-token\r\nX-ClickHouse-Database: default\r\n",
    );
    assert_response_with_content_type(
        &authenticated_exchange(&database, "correct-token", &bearer_request),
        "HTTP/1.1 200 OK",
        "text/plain; charset=utf-8",
        b"",
    );

    let key_request = request_for_target_with_headers(
        "/?database=default&query=insert+into+typed_values+format+csv%3B",
        b"7,-3e2,false,plain\n",
        "X-ClickHouse-Key: correct-key\r\n",
    );
    let key_response = clickhouse_key_exchange(&database, "correct-key", &key_request);
    assert_response_with_content_type(
        &key_response,
        "HTTP/1.1 200 OK",
        "text/plain; charset=utf-8",
        b"",
    );
    assert_clickhouse_key_response_is_not_cacheable(&key_response);

    let empty_request = request_for_target_with_headers(
        "/?query=INSERT+INTO+typed_values+FORMAT+CSV",
        b"",
        "X-ClickHouse-Key: correct-key\r\n",
    );
    let empty_response = clickhouse_key_exchange(&database, "correct-key", &empty_request);
    assert_response_with_content_type(
        &empty_response,
        "HTTP/1.1 200 OK",
        "text/plain; charset=utf-8",
        b"",
    );
    assert_clickhouse_key_response_is_not_cacheable(&empty_response);

    assert_response(
        &exchange(
            &database,
            &request_for_target(
                "/query",
                b"SELECT id, score, active, label FROM typed_values ORDER BY id;",
            ),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"},{"name":"score","type":"Float64"},{"name":"active","type":"Bool"},{"name":"label","type":"String"}],"rows":[[-9223372036854775808,2.5,true,"comma, \"quoted\"\nnext"],[7,-300.0,false,"plain"]]}"#,
    );
}

#[test]
fn parameterized_headerless_csv_insert_preserves_auth_access_selectors_and_body_rules() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE events (id Int64, label String);")
        .unwrap();
    let csv = b"1,rejected\n";

    let (missing_credentials, body_offset) =
        request_with_authorization_for_target("/?query=INSERT+INTO+events+FORMAT+CSV", csv, "");
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

    let (selected_format, body_offset) = request_with_authorization_for_target(
        "/?query=INSERT+INTO+events+FORMAT+CSV",
        csv,
        "Authorization: Bearer correct-token\r\nX-ClickHouse-Format: CSV\r\n",
    );
    let mut input = Cursor::new(selected_format);
    response.clear();
    handle_http_query_with_bearer_token(&database, "correct-token", &mut input, &mut response)
        .unwrap();
    assert_eq!(input.position(), body_offset);
    assert_response(
        &response,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"CSV INSERT does not accept an output format selector"}"#,
    );

    let (default_format, body_offset) = request_with_authorization_for_target(
        "/?query=INSERT+INTO+events+FORMAT+CSV&default_format=JSON",
        csv,
        "Authorization: Bearer correct-token\r\n",
    );
    let mut input = Cursor::new(default_format);
    response.clear();
    handle_http_query_with_bearer_token(&database, "correct-token", &mut input, &mut response)
        .unwrap();
    assert_eq!(input.position(), body_offset);
    assert_response(
        &response,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"CSV INSERT does not accept an output format selector"}"#,
    );

    let (read_only_request, body_offset) = request_with_authorization_for_target(
        "/?query=INSERT+INTO+events+FORMAT+CSV",
        csv,
        "Authorization: Bearer read-token\r\n",
    );
    let mut input = Cursor::new(read_only_request);
    response.clear();
    handle_http_query_read_only_with_bearer_token(
        &database,
        "read-token",
        &mut input,
        &mut response,
    )
    .unwrap();
    assert_eq!(input.position(), body_offset);
    assert_response(
        &response,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"POST /?query= does not accept a request body"}"#,
    );

    let (extra_sql, body_offset) = request_with_authorization_for_target(
        "/?query=INSERT+INTO+events+FORMAT+CSV%3B+SELECT+1%3B",
        csv,
        "Authorization: Bearer correct-token\r\n",
    );
    let mut input = Cursor::new(extra_sql);
    response.clear();
    handle_http_query_with_bearer_token(&database, "correct-token", &mut input, &mut response)
        .unwrap();
    assert_eq!(input.position(), body_offset);
    assert_response(
        &response,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"POST /?query= does not accept a request body"}"#,
    );

    response.clear();
    handle_http_query_with_bearer_token(
        &database,
        "correct-token",
        Cursor::new(
            b"POST /?query=INSERT+INTO+events+FORMAT+CSV HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer correct-token\r\n\r\n",
        ),
        &mut response,
    )
    .unwrap();
    assert_response(
        &response,
        "HTTP/1.1 411 Length Required",
        r#"{"error":"Content-Length header is required"}"#,
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
fn parameterized_headerless_csv_insert_preserves_limits_and_atomic_rollback() {
    let database = SharedDatabase::with_max_rows_per_table(2);
    database
        .execute(
            "CREATE TABLE events (id Int64, label String); \
             INSERT INTO events VALUES (9, 'existing');",
        )
        .unwrap();

    let late_error = request_for_target_with_headers(
        "/?query=INSERT+INTO+events+FORMAT+CSV",
        b"1,valid\nwrong,late\n",
        "X-ClickHouse-Key: correct-key\r\n",
    );
    let late_response = clickhouse_key_exchange(&database, "correct-key", &late_error);
    assert_response(
        &late_response,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"database CSV ingestion failed: CSV field at line 2, column 1 is not a valid Int64"}"#,
    );
    assert_clickhouse_key_response_is_not_cacheable(&late_response);

    let oversized_csv = format!("2,{}\n", "x".repeat(128)).into_bytes();
    let (oversized_request, body_offset) = request_with_authorization_for_target(
        "/?query=INSERT+INTO+events+FORMAT+CSV",
        &oversized_csv,
        "Authorization: Bearer correct-token\r\n",
    );
    let mut input = Cursor::new(&oversized_request);
    let mut response = Vec::new();
    handle_http_query_with_bearer_token_and_limits(
        &database,
        "correct-token",
        &mut input,
        &mut response,
        HttpQueryLimits {
            max_sql_bytes: oversized_csv.len() - 1,
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

    let mut input = Cursor::new(&oversized_request);
    response.clear();
    handle_http_query_with_bearer_token_and_limits(
        &database,
        "correct-token",
        &mut input,
        &mut response,
        HttpQueryLimits {
            csv_ingest_limits: CsvIngestLimits::new(oversized_csv.len() - 1, 10, 20),
            ..HttpQueryLimits::default()
        },
    )
    .unwrap();
    assert_eq!(input.position(), body_offset);
    assert_response(
        &response,
        "HTTP/1.1 400 Bad Request",
        &format!(
            r#"{{"error":"database CSV ingestion failed: CSV input is {} bytes, exceeding the limit of {} bytes"}}"#,
            oversized_csv.len(),
            oversized_csv.len() - 1,
        ),
    );

    let limited_csv = b"1,one\n2,two\n";
    let limited_request = request_for_target_with_headers(
        "/?query=INSERT+INTO+events+FORMAT+CSV",
        limited_csv,
        "Authorization: Bearer correct-token\r\n",
    );
    for (csv_ingest_limits, expected_body) in [
        (
            CsvIngestLimits::new(limited_csv.len(), 1, 4),
            r#"{"error":"database CSV ingestion failed: CSV record at line 2 raises the row count to 2, exceeding the limit of 1"}"#,
        ),
        (
            CsvIngestLimits::new(limited_csv.len(), 2, 3),
            r#"{"error":"database CSV ingestion failed: CSV record at line 2 raises the value count to 4, exceeding the limit of 3"}"#,
        ),
    ] {
        response.clear();
        handle_http_query_with_bearer_token_and_limits(
            &database,
            "correct-token",
            Cursor::new(&limited_request),
            &mut response,
            HttpQueryLimits {
                csv_ingest_limits,
                ..HttpQueryLimits::default()
            },
        )
        .unwrap();
        assert_response(&response, "HTTP/1.1 400 Bad Request", expected_body);
    }

    assert_response(
        &authenticated_exchange(&database, "correct-token", &limited_request),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"database CSV ingestion failed: could not ingest CSV input: table rows requires at least 3, exceeding the limit of 2"}"#,
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
fn parameterized_headerless_csv_insert_returns_503_without_waiting_for_a_reader() {
    let mut initial = Database::new();
    initial.execute("CREATE TABLE events (id Int64);").unwrap();
    let inner = Arc::new(RwLock::new(initial));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let mut reader = Some(inner.read().unwrap());
    let request = request_for_target_with_headers(
        "/?query=INSERT+INTO+events+FORMAT+CSV",
        b"1\n",
        "X-ClickHouse-Key: correct-key\r\n",
    );
    let (sender, receiver) = mpsc::channel();
    let worker_database = database.clone();
    let worker = thread::spawn(move || {
        sender
            .send(clickhouse_key_exchange(
                &worker_database,
                "correct-key",
                &request,
            ))
            .unwrap();
    });

    let response = match receiver.recv_timeout(Duration::from_secs(2)) {
        Ok(response) => response,
        Err(error) => {
            drop(reader.take());
            worker.join().unwrap();
            panic!("parameterized HTTP headerless CSV admission blocked behind a reader: {error}");
        }
    };
    assert_response(
        &response,
        "HTTP/1.1 503 Service Unavailable",
        r#"{"error":"database is unavailable"}"#,
    );
    assert_clickhouse_key_response_is_not_cacheable(&response);
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
fn parameterized_tab_separated_insert_works_with_both_credential_modes() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE events (id Int64, label String, active Bool);")
        .unwrap();

    let bearer_request = request_for_target_with_headers(
        "/?query=INSERT+INTO+events+FORMAT+TabSeparated",
        b"1\tbearer\\tlabel\ttrue\n",
        "Authorization: Bearer correct-token\r\n",
    );
    assert_response_with_content_type(
        &authenticated_exchange(&database, "correct-token", &bearer_request),
        "HTTP/1.1 200 OK",
        "text/plain; charset=utf-8",
        b"",
    );

    let key_request = request_for_target_with_headers(
        "/?database=default&query=insert+into+events+format+tabseparated%3B",
        b"2\tkey\tfalse\n",
        "X-ClickHouse-Key: correct-key\r\nX-ClickHouse-Database: default\r\n",
    );
    let key_response = clickhouse_key_exchange(&database, "correct-key", &key_request);
    assert_response_with_content_type(
        &key_response,
        "HTTP/1.1 200 OK",
        "text/plain; charset=utf-8",
        b"",
    );
    assert_clickhouse_key_response_is_not_cacheable(&key_response);

    assert_response(
        &exchange(
            &database,
            &request_for_target(
                "/query",
                b"SELECT id, label, active FROM events ORDER BY id;",
            ),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"},{"name":"label","type":"String"},{"name":"active","type":"Bool"}],"rows":[[1,"bearer\tlabel",true],[2,"key",false]]}"#,
    );
}

#[test]
fn parameterized_tab_separated_insert_rejects_access_and_invalid_shapes_before_body() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE events (id Int64, label String);")
        .unwrap();
    let tsv = b"1\trejected\n";

    let (missing_credentials, body_offset) = request_with_authorization_for_target(
        "/?query=INSERT+INTO+events+FORMAT+TabSeparated",
        tsv,
        "",
    );
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

    let (selected_format, body_offset) = request_with_authorization_for_target(
        "/?query=INSERT+INTO+events+FORMAT+TabSeparated",
        tsv,
        "Authorization: Bearer correct-token\r\nX-ClickHouse-Format: TabSeparated\r\n",
    );
    let mut input = Cursor::new(selected_format);
    response.clear();
    handle_http_query_with_bearer_token(&database, "correct-token", &mut input, &mut response)
        .unwrap();
    assert_eq!(input.position(), body_offset);
    assert_response(
        &response,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"TabSeparated INSERT does not accept an output format selector"}"#,
    );

    let (read_only_request, body_offset) = request_with_authorization_for_target(
        "/?query=INSERT+INTO+events+FORMAT+TabSeparated",
        tsv,
        "Authorization: Bearer read-token\r\n",
    );
    let mut input = Cursor::new(read_only_request);
    response.clear();
    handle_http_query_read_only_with_bearer_token(
        &database,
        "read-token",
        &mut input,
        &mut response,
    )
    .unwrap();
    assert_eq!(input.position(), body_offset);
    assert_response(
        &response,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"POST /?query= does not accept a request body"}"#,
    );

    let (extra_sql, body_offset) = request_with_authorization_for_target(
        "/?query=INSERT+INTO+events+FORMAT+TabSeparated%3B+SELECT+1%3B",
        tsv,
        "Authorization: Bearer correct-token\r\n",
    );
    let mut input = Cursor::new(extra_sql);
    response.clear();
    handle_http_query_with_bearer_token(&database, "correct-token", &mut input, &mut response)
        .unwrap();
    assert_eq!(input.position(), body_offset);
    assert_response(
        &response,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"POST /?query= does not accept a request body"}"#,
    );

    response.clear();
    handle_http_query_with_bearer_token(
        &database,
        "correct-token",
        Cursor::new(
            b"POST /?query=INSERT+INTO+events+FORMAT+TabSeparated HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer correct-token\r\n\r\n",
        ),
        &mut response,
    )
    .unwrap();
    assert_response(
        &response,
        "HTTP/1.1 411 Length Required",
        r#"{"error":"Content-Length header is required"}"#,
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
fn parameterized_tab_separated_insert_preserves_limits_and_atomic_rollback() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE events (id Int64, label String); \
             INSERT INTO events VALUES (9, 'existing');",
        )
        .unwrap();

    let late_error = request_for_target_with_headers(
        "/?query=INSERT+INTO+events+FORMAT+TabSeparated",
        b"1\tvalid\nwrong\tlate\n",
        "X-ClickHouse-Key: correct-key\r\n",
    );
    let late_response = clickhouse_key_exchange(&database, "correct-key", &late_error);
    assert_response(
        &late_response,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"database TSV ingestion failed: TSV field at line 2, column 1 is not a valid Int64"}"#,
    );
    assert_clickhouse_key_response_is_not_cacheable(&late_response);

    let oversized_tsv = format!("2\t{}\n", "x".repeat(128)).into_bytes();
    let (oversized_request, body_offset) = request_with_authorization_for_target(
        "/?query=INSERT+INTO+events+FORMAT+TabSeparated",
        &oversized_tsv,
        "Authorization: Bearer correct-token\r\n",
    );
    let mut input = Cursor::new(&oversized_request);
    let mut response = Vec::new();
    handle_http_query_with_bearer_token_and_limits(
        &database,
        "correct-token",
        &mut input,
        &mut response,
        HttpQueryLimits {
            max_sql_bytes: oversized_tsv.len() - 1,
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

    let mut input = Cursor::new(&oversized_request);
    response.clear();
    handle_http_query_with_bearer_token_and_limits(
        &database,
        "correct-token",
        &mut input,
        &mut response,
        HttpQueryLimits {
            csv_ingest_limits: CsvIngestLimits::new(0, 0, 0),
            tsv_ingest_limits: TsvIngestLimits::new(oversized_tsv.len() - 1, 10, 20),
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
            oversized_tsv.len(),
            oversized_tsv.len() - 1,
        ),
    );

    let row_limited_tsv = b"3\tthree\n4\tfour\n";
    let row_limited_request = request_for_target_with_headers(
        "/?query=INSERT+INTO+events+FORMAT+TabSeparated",
        row_limited_tsv,
        "Authorization: Bearer correct-token\r\n",
    );
    response.clear();
    handle_http_query_with_bearer_token_and_limits(
        &database,
        "correct-token",
        Cursor::new(row_limited_request),
        &mut response,
        HttpQueryLimits {
            tsv_ingest_limits: TsvIngestLimits::new(row_limited_tsv.len(), 1, 4),
            ..HttpQueryLimits::default()
        },
    )
    .unwrap();
    assert_response(
        &response,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"database TSV ingestion failed: TSV record at line 2 raises the row count to 2, exceeding the limit of 1"}"#,
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
fn parameterized_tab_separated_insert_returns_503_without_waiting_for_a_reader() {
    let mut initial = Database::new();
    initial.execute("CREATE TABLE events (id Int64);").unwrap();
    let inner = Arc::new(RwLock::new(initial));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let mut reader = Some(inner.read().unwrap());
    let request = request_for_target_with_headers(
        "/?query=INSERT+INTO+events+FORMAT+TabSeparated",
        b"1\n",
        "X-ClickHouse-Key: correct-key\r\n",
    );
    let (sender, receiver) = mpsc::channel();
    let worker_database = database.clone();
    let worker = thread::spawn(move || {
        sender
            .send(clickhouse_key_exchange(
                &worker_database,
                "correct-key",
                &request,
            ))
            .unwrap();
    });

    let response = match receiver.recv_timeout(Duration::from_secs(2)) {
        Ok(response) => response,
        Err(error) => {
            drop(reader.take());
            worker.join().unwrap();
            panic!("parameterized HTTP TSV admission blocked behind a reader: {error}");
        }
    };
    assert_response(
        &response,
        "HTTP/1.1 503 Service Unavailable",
        r#"{"error":"database is unavailable"}"#,
    );
    assert_clickhouse_key_response_is_not_cacheable(&response);
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
fn parameterized_tab_separated_with_names_wires_reordered_subsets_and_all_types() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE typed_values (id Int64, score Float64, active Bool, label String);")
        .unwrap();

    let bearer_request = request_for_target_with_headers(
        "/?query=INSERT+INTO+typed_values+FORMAT+TabSeparatedWithNames",
        b"label\tid\r\nbearer\\tlabel \xE9\x9B\xAA\t7\r\n",
        "Authorization: Bearer correct-token\r\n",
    );
    assert_response_with_content_type(
        &authenticated_exchange(&database, "correct-token", &bearer_request),
        "HTTP/1.1 200 OK",
        "text/plain; charset=utf-8",
        b"",
    );

    let key_request = request_for_target_with_headers(
        "/?database=default&query=insert+into+typed_values+format+tabseparatedwithnames%3B",
        b"active\tscore\ntrue\t-0.125\n",
        "X-ClickHouse-Key: correct-key\r\nX-ClickHouse-Database: default\r\n",
    );
    let key_response = clickhouse_key_exchange(&database, "correct-key", &key_request);
    assert_response_with_content_type(
        &key_response,
        "HTTP/1.1 200 OK",
        "text/plain; charset=utf-8",
        b"",
    );
    assert_clickhouse_key_response_is_not_cacheable(&key_response);

    assert_response(
        &exchange(
            &database,
            &request_for_target(
                "/query",
                b"SELECT id, score, active, label FROM typed_values ORDER BY id;",
            ),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"},{"name":"score","type":"Float64"},{"name":"active","type":"Bool"},{"name":"label","type":"String"}],"rows":[[0,-0.125,true,""],[7,0.0,false,"bearer\tlabel 雪"]]}"#,
    );
}

#[test]
fn parameterized_tab_separated_with_names_preserves_auth_access_and_selectors() {
    let database = SharedDatabase::default();
    database.execute("CREATE TABLE events (id Int64);").unwrap();
    let tsv = b"id\n1\n";
    let target = "/?query=INSERT+INTO+events+FORMAT+TabSeparatedWithNames";

    let (missing_credentials, body_offset) = request_with_authorization_for_target(target, tsv, "");
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

    let (selected_format, body_offset) = request_with_authorization_for_target(
        target,
        tsv,
        "Authorization: Bearer correct-token\r\n\
         X-ClickHouse-Format: TabSeparatedWithNames\r\n",
    );
    let mut input = Cursor::new(selected_format);
    response.clear();
    handle_http_query_with_bearer_token(&database, "correct-token", &mut input, &mut response)
        .unwrap();
    assert_eq!(input.position(), body_offset);
    assert_response(
        &response,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"TabSeparatedWithNames INSERT does not accept an output format selector"}"#,
    );

    let default_format = request_for_target_with_headers(
        "/?query=INSERT+INTO+events+FORMAT+TabSeparatedWithNames&default_format=JSON",
        tsv,
        "X-ClickHouse-Key: correct-key\r\n",
    );
    let key_response = clickhouse_key_exchange(&database, "correct-key", &default_format);
    assert_response(
        &key_response,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"TabSeparatedWithNames INSERT does not accept an output format selector"}"#,
    );
    assert_clickhouse_key_response_is_not_cacheable(&key_response);

    let (read_only_request, body_offset) =
        request_with_authorization_for_target(target, tsv, "Authorization: Bearer read-token\r\n");
    let mut input = Cursor::new(read_only_request);
    response.clear();
    handle_http_query_read_only_with_bearer_token(
        &database,
        "read-token",
        &mut input,
        &mut response,
    )
    .unwrap();
    assert_eq!(input.position(), body_offset);
    assert_response(
        &response,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"POST /?query= does not accept a request body"}"#,
    );

    assert!(
        database
            .query("SELECT id FROM events;")
            .unwrap()
            .rows
            .is_empty()
    );
}

#[test]
fn parameterized_tab_separated_with_names_preserves_exact_body_and_tsv_limits() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE exact_events (id Int64, label String); \
             CREATE TABLE http_limited (id Int64, label String); \
             CREATE TABLE tsv_limited (id Int64, label String); \
             CREATE TABLE row_limited (id Int64, label String); \
             CREATE TABLE value_limited (id Int64, label String);",
        )
        .unwrap();
    let label = "x".repeat(64);
    let tsv = format!("label\tid\n{label}\t1\n").into_bytes();

    let exact_request = request_for_target_with_headers(
        "/?query=INSERT+INTO+exact_events+FORMAT+TabSeparatedWithNames",
        &tsv,
        "Authorization: Bearer correct-token\r\n",
    );
    let mut response = Vec::new();
    handle_http_query_with_bearer_token_and_limits(
        &database,
        "correct-token",
        Cursor::new(exact_request),
        &mut response,
        HttpQueryLimits {
            max_sql_bytes: tsv.len(),
            csv_ingest_limits: CsvIngestLimits::new(0, 0, 0),
            tsv_ingest_limits: TsvIngestLimits::new(tsv.len(), 1, 2),
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

    let (http_limited, body_offset) = request_with_authorization_for_target(
        "/?query=INSERT+INTO+http_limited+FORMAT+TabSeparatedWithNames",
        &tsv,
        "Authorization: Bearer correct-token\r\n",
    );
    let mut input = Cursor::new(http_limited);
    response.clear();
    handle_http_query_with_bearer_token_and_limits(
        &database,
        "correct-token",
        &mut input,
        &mut response,
        HttpQueryLimits {
            max_sql_bytes: tsv.len() - 1,
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

    let (tsv_limited, body_offset) = request_with_authorization_for_target(
        "/?query=INSERT+INTO+tsv_limited+FORMAT+TabSeparatedWithNames",
        &tsv,
        "Authorization: Bearer correct-token\r\n",
    );
    let mut input = Cursor::new(tsv_limited);
    response.clear();
    handle_http_query_with_bearer_token_and_limits(
        &database,
        "correct-token",
        &mut input,
        &mut response,
        HttpQueryLimits {
            tsv_ingest_limits: TsvIngestLimits::new(tsv.len() - 1, 1, 2),
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

    for (table, tsv_ingest_limits, expected_body) in [
        (
            "row_limited",
            TsvIngestLimits::new(tsv.len(), 0, 2),
            r#"{"error":"database TSV ingestion failed: TSV record at line 2 raises the row count to 1, exceeding the limit of 0"}"#,
        ),
        (
            "value_limited",
            TsvIngestLimits::new(tsv.len(), 1, 1),
            r#"{"error":"database TSV ingestion failed: TSV record at line 2 raises the value count to 2, exceeding the limit of 1"}"#,
        ),
    ] {
        let request = request_for_target_with_headers(
            &format!("/?query=INSERT+INTO+{table}+FORMAT+TabSeparatedWithNames"),
            &tsv,
            "Authorization: Bearer correct-token\r\n",
        );
        response.clear();
        handle_http_query_with_bearer_token_and_limits(
            &database,
            "correct-token",
            Cursor::new(request),
            &mut response,
            HttpQueryLimits {
                tsv_ingest_limits,
                ..HttpQueryLimits::default()
            },
        )
        .unwrap();
        assert_response(&response, "HTTP/1.1 400 Bad Request", expected_body);
    }

    assert_eq!(
        database
            .query("SELECT id, label FROM exact_events;")
            .unwrap()
            .rows,
        vec![vec![Value::Int64(1), Value::String(label)]],
    );
    for table in [
        "http_limited",
        "tsv_limited",
        "row_limited",
        "value_limited",
    ] {
        assert!(
            database
                .query(&format!("SELECT id FROM {table};"))
                .unwrap()
                .rows
                .is_empty()
        );
    }
}

#[test]
fn parameterized_tab_separated_with_names_rejects_empty_and_rolls_back_malformed_body() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE events (id Int64, label String); \
             INSERT INTO events VALUES (9, 'existing');",
        )
        .unwrap();
    let target = "/?query=INSERT+INTO+events+FORMAT+TabSeparatedWithNames";

    let empty =
        request_for_target_with_headers(target, b"", "Authorization: Bearer correct-token\r\n");
    assert_response(
        &authenticated_exchange(&database, "correct-token", &empty),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"database TSV ingestion failed: missing TabSeparatedWithNames header at line 1"}"#,
    );

    let malformed = request_for_target_with_headers(
        target,
        b"label\tid\nvalid\t1\nbad\\x\t2\n",
        "X-ClickHouse-Key: correct-key\r\n",
    );
    let malformed_response = clickhouse_key_exchange(&database, "correct-key", &malformed);
    assert_response(
        &malformed_response,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"database TSV ingestion failed: TSV field at line 3, column 1 contains an invalid backslash escape"}"#,
    );
    assert_clickhouse_key_response_is_not_cacheable(&malformed_response);

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
fn parameterized_tab_separated_with_names_returns_503_without_waiting_for_a_reader() {
    let mut initial = Database::new();
    initial.execute("CREATE TABLE events (id Int64);").unwrap();
    let inner = Arc::new(RwLock::new(initial));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let mut reader = Some(inner.read().unwrap());
    let request = request_for_target_with_headers(
        "/?query=INSERT+INTO+events+FORMAT+TabSeparatedWithNames",
        b"id\n1\n",
        "X-ClickHouse-Key: correct-key\r\n",
    );
    let (sender, receiver) = mpsc::channel();
    let worker_database = database.clone();
    let worker = thread::spawn(move || {
        sender
            .send(clickhouse_key_exchange(
                &worker_database,
                "correct-key",
                &request,
            ))
            .unwrap();
    });

    let response = match receiver.recv_timeout(Duration::from_secs(2)) {
        Ok(response) => response,
        Err(error) => {
            drop(reader.take());
            worker.join().unwrap();
            panic!("parameterized HTTP named TSV admission blocked behind a reader: {error}");
        }
    };
    assert_response(
        &response,
        "HTTP/1.1 503 Service Unavailable",
        r#"{"error":"database is unavailable"}"#,
    );
    assert_clickhouse_key_response_is_not_cacheable(&response);
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
fn get_and_unauthenticated_standard_query_routes_remain_read_only() {
    let database = SharedDatabase::default();
    database.execute("CREATE TABLE events (id Int64);").unwrap();
    let sql = b"INSERT INTO events VALUES (1);";
    let read_only_error = r#"{"error":"read-only query accepts only SELECT, SHOW DATABASES, SHOW SETTINGS, SHOW FUNCTIONS, SHOW TABLES, SHOW CREATE TABLE, DESCRIBE TABLE, or EXISTS TABLE; found INSERT"}"#;

    for request in [
        request_for_target("/", sql),
        request_for_target("/query", sql),
        b"POST /?query=INSERT+INTO+events+VALUES+%281%29%3B HTTP/1.1\r\nHost: localhost\r\n\r\n"
            .to_vec(),
    ] {
        assert_response(
            &exchange(&database, &request),
            "HTTP/1.1 400 Bad Request",
            read_only_error,
        );
    }

    assert_response(
        &authenticated_exchange(
            &database,
            "correct-token",
            b"GET /?query=INSERT+INTO+events+VALUES+%281%29%3B HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer correct-token\r\n\r\n",
        ),
        "HTTP/1.1 400 Bad Request",
        read_only_error,
    );

    let (missing_credentials, body_offset) =
        request_with_authorization_for_target("/query", sql, "");
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

    let (invalid_database, body_offset) = request_with_authorization_for_target(
        "/",
        sql,
        "Authorization: Bearer correct-token\r\nX-ClickHouse-Database: analytics\r\n",
    );
    let mut input = Cursor::new(invalid_database);
    response.clear();
    handle_http_query_with_bearer_token(&database, "correct-token", &mut input, &mut response)
        .unwrap();
    assert_eq!(input.position(), body_offset);
    assert_response(
        &response,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"X-ClickHouse-Database header must be default"}"#,
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
fn standard_query_inserts_reject_mixed_batches_formats_and_late_failures_atomically() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE events (id Int64); \
             CREATE TABLE readings (value Float64); \
             CREATE TABLE protected (id Int64); \
             INSERT INTO protected VALUES (9);",
        )
        .unwrap();

    let (mixed, _) = request_with_authorization_for_target(
        "/query",
        b"INSERT INTO events VALUES (1); SELECT id FROM events;",
        "Authorization: Bearer correct-token\r\n",
    );
    assert_response(
        &authenticated_exchange(&database, "correct-token", &mixed),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"INSERT-only batch accepts only INSERT statements; found SELECT"}"#,
    );

    let late_failure = request_for_target_with_headers(
        "/",
        b"INSERT INTO events VALUES (2); INSERT INTO readings VALUES ('wrong');",
        "X-ClickHouse-Key: correct-key\r\n",
    );
    let late_failure_response = clickhouse_key_exchange(&database, "correct-key", &late_failure);
    assert_response(
        &late_failure_response,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"type mismatch for column 'readings.value': expected Float64, found String"}"#,
    );
    assert_clickhouse_key_response_is_not_cacheable(&late_failure_response);

    let (formatted, _) = request_with_authorization_for_target(
        "/query",
        b"INSERT INTO events VALUES (3);",
        "Authorization: Bearer correct-token\r\nX-ClickHouse-Format: CSVWithNames\r\n",
    );
    assert_response(
        &authenticated_exchange(&database, "correct-token", &formatted),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"read-only query accepts only SELECT, SHOW DATABASES, SHOW SETTINGS, SHOW FUNCTIONS, SHOW TABLES, SHOW CREATE TABLE, DESCRIBE TABLE, or EXISTS TABLE; found INSERT"}"#,
    );

    let default_formatted = clickhouse_key_exchange(
        &database,
        "correct-key",
        b"POST /?query=INSERT+INTO+events+VALUES+%284%29%3B&default_format=JSON HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: correct-key\r\n\r\n",
    );
    assert_response(
        &default_formatted,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"read-only query accepts only SELECT, SHOW DATABASES, SHOW SETTINGS, SHOW FUNCTIONS, SHOW TABLES, SHOW CREATE TABLE, DESCRIBE TABLE, or EXISTS TABLE; found INSERT"}"#,
    );
    assert_clickhouse_key_response_is_not_cacheable(&default_formatted);

    let (non_insert, _) = request_with_authorization_for_target(
        "/",
        b"DELETE FROM protected WHERE id = 9;",
        "Authorization: Bearer correct-token\r\n",
    );
    assert_response(
        &authenticated_exchange(&database, "correct-token", &non_insert),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"read-only query accepts only SELECT, SHOW DATABASES, SHOW SETTINGS, SHOW FUNCTIONS, SHOW TABLES, SHOW CREATE TABLE, DESCRIBE TABLE, or EXISTS TABLE; found DELETE"}"#,
    );

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
            &request_for_target("/query", b"SELECT id FROM protected;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"}],"rows":[[9]]}"#,
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
fn standard_query_inserts_preserve_sql_body_capacity_and_response_limits() {
    let sql = b"INSERT INTO events VALUES (1);";
    let body_limited_database = SharedDatabase::default();
    body_limited_database
        .execute("CREATE TABLE events (id Int64);")
        .unwrap();
    let (oversized_body, body_offset) = request_with_authorization_for_target(
        "/query",
        sql,
        "Authorization: Bearer correct-token\r\n",
    );
    let mut input = Cursor::new(&oversized_body);
    let mut response = Vec::new();
    handle_http_query_with_bearer_token_and_limits(
        &body_limited_database,
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

    let mut url_response = Vec::new();
    handle_http_query_with_clickhouse_key_and_limits(
        &body_limited_database,
        "correct-key",
        Cursor::new(
            b"POST /?query=INSERT+INTO+events+VALUES+%281%29%3B HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: correct-key\r\n\r\n",
        ),
        &mut url_response,
        HttpQueryLimits {
            max_sql_bytes: sql.len() - 1,
            ..HttpQueryLimits::default()
        },
    )
    .unwrap();
    assert_response(
        &url_response,
        "HTTP/1.1 413 Payload Too Large",
        r#"{"error":"SQL query exceeds configured byte limit"}"#,
    );
    assert_clickhouse_key_response_is_not_cacheable(&url_response);

    let capacity_database = SharedDatabase::with_max_rows_per_table(1);
    capacity_database
        .execute("CREATE TABLE events (id Int64);")
        .unwrap();
    let (capacity_request, _) = request_with_authorization_for_target(
        "/",
        b"INSERT INTO events VALUES (1); INSERT INTO events VALUES (2);",
        "Authorization: Bearer correct-token\r\n",
    );
    assert_response(
        &authenticated_exchange(&capacity_database, "correct-token", &capacity_request),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"table rows requires at least 2, exceeding the limit of 1"}"#,
    );
    assert_response(
        &exchange(
            &capacity_database,
            &request_for_target("/query", b"SELECT id FROM events;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"}],"rows":[]}"#,
    );

    let (response_limited_request, _) =
        request_with_authorization_for_target("/", sql, "Authorization: Bearer correct-token\r\n");
    let sizing_database = SharedDatabase::default();
    sizing_database
        .execute("CREATE TABLE events (id Int64);")
        .unwrap();
    let success_response =
        authenticated_exchange(&sizing_database, "correct-token", &response_limited_request);
    let response_limited_database = SharedDatabase::default();
    response_limited_database
        .execute("CREATE TABLE events (id Int64);")
        .unwrap();
    let max_response_bytes = success_response.len() - 1;
    let mut limited_output = Vec::new();
    let error = handle_http_query_with_bearer_token_and_limits(
        &response_limited_database,
        "correct-token",
        Cursor::new(response_limited_request),
        &mut limited_output,
        HttpQueryLimits {
            max_response_bytes,
            ..HttpQueryLimits::default()
        },
    )
    .expect_err("the insert success response exceeds the configured cap");
    assert!(matches!(
        error,
        HttpQueryError::ResponseLimitExceeded { max_bytes, .. }
            if max_bytes == max_response_bytes
    ));
    assert!(limited_output.is_empty());
    assert_response(
        &exchange(
            &response_limited_database,
            &request_for_target("/query", b"SELECT id FROM events;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"}],"rows":[]}"#,
    );
}

#[test]
fn authenticated_standard_query_insert_returns_503_without_waiting_for_a_reader() {
    let mut initial = Database::new();
    initial.execute("CREATE TABLE events (id Int64);").unwrap();
    let inner = Arc::new(RwLock::new(initial));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let mut reader = Some(inner.read().unwrap());
    let (request, _) = request_with_authorization_for_target(
        "/query",
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
            panic!("standard HTTP insert admission blocked behind a reader: {error}");
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
fn authenticated_csv_routes_ingest_nullable_int64_writer_tokens() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE readings (value Nullable(Int64));")
        .unwrap();

    let named_csv = b"value\n-9223372036854775808\nNULL\n";
    let (named_request, _) = request_with_authorization_for_target(
        "/insert/readings",
        named_csv,
        "Authorization: Bearer correct-token\r\n",
    );
    assert_response_with_content_type(
        &authenticated_exchange(&database, "correct-token", &named_request),
        "HTTP/1.1 200 OK",
        "text/plain; charset=utf-8",
        b"",
    );

    let headerless_csv = b"9223372036854775807\nNULL\n";
    let headerless_request = request_for_target_with_headers(
        "/?query=INSERT+INTO+readings+FORMAT+CSV",
        headerless_csv,
        "X-ClickHouse-Key: correct-key\r\n",
    );
    assert_response_with_content_type(
        &clickhouse_key_exchange(&database, "correct-key", &headerless_request),
        "HTTP/1.1 200 OK",
        "text/plain; charset=utf-8",
        b"",
    );

    assert_response(
        &exchange(
            &database,
            &request_for_target("/query", b"SELECT value FROM readings;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"value","type":"Int64"}],"rows":[[-9223372036854775808],[null],[9223372036854775807],[null]]}"#,
    );
}

#[test]
fn authenticated_json_compact_each_row_insert_ingests_nullable_int64_rows() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE readings (value Nullable(Int64));")
        .unwrap();
    let input = b"[null]\n[-7]\r\n[9223372036854775807]\n";
    let (request, _) = request_with_authorization_for_target(
        "/insert/readings",
        input,
        "Authorization: Bearer correct-token\r\n\
         X-ClickHouse-Database: default\r\n\
         X-ClickHouse-Format: JSONCompactEachRow\r\n",
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
            &request_for_target("/query", b"SELECT value FROM readings;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"value","type":"Int64"}],"rows":[[null],[-7],[9223372036854775807]]}"#,
    );
}

#[test]
fn json_compact_each_row_insert_rejects_late_malformed_input_atomically() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE readings (value Int64); \
             INSERT INTO readings VALUES (9);",
        )
        .unwrap();
    let input = b"[1]\n[late]\n";
    let (request, _) = request_with_authorization_for_target(
        "/insert/readings",
        input,
        "Authorization: Bearer correct-token\r\n\
         X-ClickHouse-Format: JSONCompactEachRow\r\n",
    );

    assert_response(
        &authenticated_exchange(&database, "correct-token", &request),
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"database JSONCompactEachRow ingestion failed: JSONCompactEachRow record at line 2 is not valid JSON at byte column 2"}"#,
    );
    assert_response(
        &exchange(
            &database,
            &request_for_target("/query", b"SELECT value FROM readings;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"value","type":"Int64"}],"rows":[[9]]}"#,
    );
}

#[test]
fn json_compact_each_row_insert_preserves_http_and_format_limits() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE readings (value Int64);")
        .unwrap();
    let input = b"[1]\n[2]\n";
    let (request, body_offset) = request_with_authorization_for_target(
        "/insert/readings",
        input,
        "Authorization: Bearer correct-token\r\n\
         X-ClickHouse-Format: JSONCompactEachRow\r\n",
    );

    let mut http_limited_input = Cursor::new(&request);
    let mut response = Vec::new();
    handle_http_query_with_bearer_token_and_limits(
        &database,
        "correct-token",
        &mut http_limited_input,
        &mut response,
        HttpQueryLimits {
            max_sql_bytes: input.len() - 1,
            ..HttpQueryLimits::default()
        },
    )
    .unwrap();
    assert_eq!(http_limited_input.position(), body_offset);
    assert_response(
        &response,
        "HTTP/1.1 413 Payload Too Large",
        r#"{"error":"request body exceeds configured byte limit"}"#,
    );

    let mut format_limited_input = Cursor::new(&request);
    response.clear();
    handle_http_query_with_bearer_token_and_limits(
        &database,
        "correct-token",
        &mut format_limited_input,
        &mut response,
        HttpQueryLimits {
            json_compact_each_row_ingest_limits: JsonCompactEachRowIngestLimits::new(
                input.len() - 1,
                2,
                2,
            ),
            ..HttpQueryLimits::default()
        },
    )
    .unwrap();
    assert_eq!(format_limited_input.position(), body_offset);
    assert_response(
        &response,
        "HTTP/1.1 400 Bad Request",
        &format!(
            r#"{{"error":"database JSONCompactEachRow ingestion failed: JSONCompactEachRow input is {} bytes, exceeding the limit of {} bytes"}}"#,
            input.len(),
            input.len() - 1,
        ),
    );

    assert_response(
        &exchange(
            &database,
            &request_for_target("/query", b"SELECT value FROM readings;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"value","type":"Int64"}],"rows":[]}"#,
    );
}

#[test]
fn json_compact_each_row_insert_returns_503_without_waiting_for_a_reader() {
    let mut initial = Database::new();
    initial
        .execute("CREATE TABLE readings (value Int64);")
        .unwrap();
    let inner = Arc::new(RwLock::new(initial));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let mut reader = Some(inner.read().unwrap());
    let (request, _) = request_with_authorization_for_target(
        "/insert/readings",
        b"[1]\n",
        "Authorization: Bearer correct-token\r\n\
         X-ClickHouse-Format: JSONCompactEachRow\r\n",
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
            panic!("HTTP JSONCompactEachRow admission blocked behind a reader: {error}");
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
            .table("readings")
            .unwrap()
            .row_count(),
        0,
    );
    drop(reader.take());
    worker.join().unwrap();
}

#[test]
fn authenticated_headerless_csv_insert_ingests_all_physical_types_in_schema_order() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE typed_values (id Int64, score Float64, active Bool, label String);")
        .unwrap();
    let csv = concat!(
        "-9223372036854775808,2.5,true,\"comma, \"\"quoted\"\"\nnext\"\r\n",
        "7,-3e2,false,plain\n",
    )
    .as_bytes();
    let (request, _) = request_with_authorization_for_target(
        "/insert/typed_values",
        csv,
        "Authorization: Bearer correct-token\r\n\
         X-ClickHouse-Database: default\r\n\
         X-ClickHouse-Format: CSV\r\n",
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
        r#"{"columns":[{"name":"id","type":"Int64"},{"name":"score","type":"Float64"},{"name":"active","type":"Bool"},{"name":"label","type":"String"}],"rows":[[-9223372036854775808,2.5,true,"comma, \"quoted\"\nnext"],[7,-300.0,false,"plain"]]}"#,
    );
}

#[test]
fn authenticated_headerless_tsv_insert_ingests_all_physical_types_and_escapes() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE typed_values (id Int64, score Float64, active Bool, label String);")
        .unwrap();
    let tsv = concat!(
        "-9223372036854775808\t2.5\ttrue\tslash\\\\tab\\tcarriage\\rline\\nnul\\0backspace\\bformfeed\\fapostrophe\\' snow 雪\r\n",
        "7\t-3e2\tfalse\tplain\n",
    )
    .as_bytes();
    let (request, _) = request_with_authorization_for_target(
        "/insert/typed_values",
        tsv,
        "Authorization: Bearer correct-token\r\n\
         X-ClickHouse-Database: default\r\n\
         X-ClickHouse-Format: TabSeparated\r\n",
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
        r#"{"columns":[{"name":"id","type":"Int64"},{"name":"score","type":"Float64"},{"name":"active","type":"Bool"},{"name":"label","type":"String"}],"rows":[[-9223372036854775808,2.5,true,"slash\\tab\tcarriage\rline\nnul\u0000backspace\bformfeed\fapostrophe' snow 雪"],[7,-300.0,false,"plain"]]}"#,
    );
}

#[test]
fn authenticated_tsv_routes_ingest_nullable_int64_null_tokens() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE direct_rows (value Nullable(Int64)); \
             CREATE TABLE direct_named (value Nullable(Int64)); \
             CREATE TABLE query_rows (value Nullable(Int64)); \
             CREATE TABLE query_named (value Nullable(Int64));",
        )
        .unwrap();

    for (target, body, headers) in [
        (
            "/insert/direct_rows",
            b"\\N\n7\n".as_slice(),
            "Authorization: Bearer correct-token\r\n\
             X-ClickHouse-Format: TabSeparated\r\n",
        ),
        (
            "/insert/direct_named",
            b"value\n\\N\n8\n".as_slice(),
            "Authorization: Bearer correct-token\r\n\
             X-ClickHouse-Format: TabSeparatedWithNames\r\n",
        ),
        (
            "/?query=INSERT+INTO+query_rows+FORMAT+TabSeparated",
            b"\\N\n9\n".as_slice(),
            "Authorization: Bearer correct-token\r\n",
        ),
        (
            "/?query=INSERT+INTO+query_named+FORMAT+TabSeparatedWithNames",
            b"value\n\\N\n10\n".as_slice(),
            "Authorization: Bearer correct-token\r\n",
        ),
    ] {
        let request = request_for_target_with_headers(target, body, headers);
        assert_response_with_content_type(
            &authenticated_exchange(&database, "correct-token", &request),
            "HTTP/1.1 200 OK",
            "text/plain; charset=utf-8",
            b"",
        );
    }

    for (table, present) in [
        ("direct_rows", 7),
        ("direct_named", 8),
        ("query_rows", 9),
        ("query_named", 10),
    ] {
        assert_eq!(
            database
                .query(&format!("SELECT value FROM {table};"))
                .unwrap()
                .rows,
            [
                vec![Value::Null(rusthouse::batch::value::DataType::Int64)],
                vec![Value::Int64(present)],
            ],
        );
    }
}

#[test]
fn headerless_csv_and_tsv_empty_input_are_no_ops_and_named_csv_remains_the_default() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE events (id Int64, label String); \
             INSERT INTO events VALUES (9, 'existing');",
        )
        .unwrap();
    for format in ["CSV", "TabSeparated"] {
        let empty = request_for_target_with_headers(
            "/insert/events",
            b"",
            &format!("X-ClickHouse-Key: correct key:42\r\nX-ClickHouse-Format: {format}\r\n"),
        );

        let response = clickhouse_key_exchange(&database, "correct key:42", &empty);
        assert_response_with_content_type(
            &response,
            "HTTP/1.1 200 OK",
            "text/plain; charset=utf-8",
            b"",
        );
        assert_clickhouse_key_response_is_not_cacheable(&response);
    }

    let named = request_for_target_with_headers(
        "/insert/events",
        b"label,id\nnamed-default,1\n",
        "X-ClickHouse-Key: correct key:42\r\n",
    );
    assert_response_with_content_type(
        &clickhouse_key_exchange(&database, "correct key:42", &named),
        "HTTP/1.1 200 OK",
        "text/plain; charset=utf-8",
        b"",
    );
    assert_response(
        &exchange(
            &database,
            &request_for_target("/query", b"SELECT id, label FROM events ORDER BY id;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"},{"name":"label","type":"String"}],"rows":[[1,"named-default"],[9,"existing"]]}"#,
    );
}

#[test]
fn authenticated_csv_insert_accepts_subsets_and_fills_every_typed_default() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE typed_values (id Int64, score Float64, active Bool, label String);")
        .unwrap();

    for csv in [
        b"label,id\n\"HTTP, \"\"quoted\"\"\",\"7\"\n".as_slice(),
        b"active,score\n\"true\",\"-0.125\"\n".as_slice(),
    ] {
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
    }

    assert_response(
        &exchange(
            &database,
            &request_for_target(
                "/query",
                b"SELECT id, score, active, label FROM typed_values ORDER BY id;",
            ),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"},{"name":"score","type":"Float64"},{"name":"active","type":"Bool"},{"name":"label","type":"String"}],"rows":[[0,-0.125,true,""],[7,0.0,false,"HTTP, \"quoted\""]]}"#,
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
fn bearer_authenticated_tsv_insert_accepts_subsets_and_fills_every_typed_default() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE typed_values (id Int64, score Float64, active Bool, label String);")
        .unwrap();

    for tsv in [
        concat!(
            "label\tid\n",
            "slash\\\\tab\\tcarriage\\rline\\nnul\\0backspace\\bformfeed\\fapostrophe\\' snow 雪\t7\n",
        )
        .as_bytes(),
        b"active\tscore\ntrue\t-0.125\n".as_slice(),
    ] {
        let (request, _) = request_with_authorization_for_target(
            "/insert/typed_values",
            tsv,
            "Authorization: Bearer correct-token\r\n\
             X-ClickHouse-Format: TabSeparatedWithNames\r\n",
        );
        assert_response_with_content_type(
            &authenticated_exchange(&database, "correct-token", &request),
            "HTTP/1.1 200 OK",
            "text/plain; charset=utf-8",
            b"",
        );
    }
    let query = request_for_target_with_headers(
        "/query",
        b"SELECT id, score, active, label FROM typed_values ORDER BY id;",
        "X-ClickHouse-Format: TabSeparatedWithNames\r\n",
    );
    assert_response_with_content_type(
        &exchange(&database, &query),
        "HTTP/1.1 200 OK",
        "text/tab-separated-values; charset=utf-8",
        concat!(
            "id\tscore\tactive\tlabel\n",
            "0\t-0.125\ttrue\t\n",
            "7\t0.0\tfalse\tslash\\\\tab\\tcarriage\\rline\\nnul\\0backspace\\bformfeed\\fapostrophe\\' snow 雪\n",
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
    let invalid_format = "X-ClickHouse-Format: tabseparated\r\n";

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
         X-ClickHouse-Format: tabseparated\r\n",
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
fn table_insert_rejects_nonexact_and_duplicate_formats_before_body_reads() {
    let database = SharedDatabase::default();
    database.execute("CREATE TABLE events (id Int64);").unwrap();

    for format_headers in [
        "X-ClickHouse-Format: csv\r\n",
        "X-ClickHouse-Format: Csv\r\n",
        "X-ClickHouse-Format: CSVWithnames\r\n",
        "X-ClickHouse-Format: tabseparated\r\n",
        "X-ClickHouse-Format: Tabseparated\r\n",
        "X-ClickHouse-Format: TabSeparatedWithnames\r\n",
        "X-ClickHouse-Format: jsoncompacteachrow\r\n",
        "X-ClickHouse-Format: JsonCompactEachRow\r\n",
    ] {
        let headers = format!("Authorization: Bearer correct-token\r\n{format_headers}");
        let (request, body_offset) =
            request_with_authorization_for_target("/insert/events", b"1\n", &headers);
        let mut input = Cursor::new(request);
        let mut response = Vec::new();
        handle_http_query_with_bearer_token(&database, "correct-token", &mut input, &mut response)
            .unwrap();

        assert_eq!(input.position(), body_offset);
        assert_response(
            &response,
            "HTTP/1.1 400 Bad Request",
            r#"{"error":"unsupported X-ClickHouse-Format header"}"#,
        );
    }

    let (duplicate, body_offset) = request_with_authorization_for_target(
        "/insert/events",
        b"1\n",
        "Authorization: Bearer correct-token\r\n\
         X-ClickHouse-Format: TabSeparated\r\n\
         x-clickhouse-format: TabSeparated\r\n",
    );
    let mut input = Cursor::new(duplicate);
    let mut response = Vec::new();
    handle_http_query_with_bearer_token(&database, "correct-token", &mut input, &mut response)
        .unwrap();

    assert_eq!(input.position(), body_offset);
    assert_response(
        &response,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"duplicate X-ClickHouse-Format header"}"#,
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
fn headerless_tsv_insert_reports_late_errors_and_rolls_back_every_row() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE events (id Int64, label String); \
             INSERT INTO events VALUES (9, 'existing');",
        )
        .unwrap();
    let cases: &[(&[u8], &str)] = &[
        (
            b"1\tvalid\n2\tbad\\x\n",
            r#"{"error":"database TSV ingestion failed: TSV field at line 2, column 2 contains an invalid backslash escape"}"#,
        ),
        (
            b"1\tvalid\nwrong\tlate\n",
            r#"{"error":"database TSV ingestion failed: TSV field at line 2, column 1 is not a valid Int64"}"#,
        ),
    ];

    for (tsv, expected_body) in cases {
        let (request, _) = request_with_authorization_for_target(
            "/insert/events",
            tsv,
            "Authorization: Bearer correct-token\r\n\
             X-ClickHouse-Format: TabSeparated\r\n",
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
            &request_for_target("/query", b"SELECT id, label FROM events;"),
        ),
        "HTTP/1.1 200 OK",
        r#"{"columns":[{"name":"id","type":"Int64"},{"name":"label","type":"String"}],"rows":[[9,"existing"]]}"#,
    );
}

#[test]
fn headerless_tsv_insert_preserves_exact_http_tsv_and_independent_csv_limits() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE tsv_events (id Int64, label String); \
             CREATE TABLE csv_events (id Int64, label String); \
             CREATE TABLE bounded_events (id Int64, label String);",
        )
        .unwrap();
    let tsv = b"1\tone\n2\ttwo\n";
    let (tsv_request, _) = request_with_authorization_for_target(
        "/insert/tsv_events",
        tsv,
        "Authorization: Bearer correct-token\r\n\
         X-ClickHouse-Format: TabSeparated\r\n",
    );
    let mut response = Vec::new();
    handle_http_query_with_bearer_token_and_limits(
        &database,
        "correct-token",
        Cursor::new(&tsv_request),
        &mut response,
        HttpQueryLimits {
            max_sql_bytes: tsv.len(),
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
         X-ClickHouse-Format: TabSeparated\r\n",
    );
    let mut input = Cursor::new(&bounded_request);
    let mut response = Vec::new();
    handle_http_query_with_bearer_token_and_limits(
        &database,
        "correct-token",
        &mut input,
        &mut response,
        HttpQueryLimits {
            max_sql_bytes: tsv.len() - 1,
            tsv_ingest_limits: TsvIngestLimits::new(tsv.len(), 2, 4),
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
            r#"{"error":"database TSV ingestion failed: TSV record at line 2 raises the row count to 2, exceeding the limit of 1"}"#,
        ),
        (
            TsvIngestLimits::new(tsv.len(), 2, 3),
            r#"{"error":"database TSV ingestion failed: TSV record at line 2 raises the value count to 4, exceeding the limit of 3"}"#,
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
fn headerless_tsv_insert_returns_503_without_waiting_for_a_reader() {
    let mut initial = Database::new();
    initial.execute("CREATE TABLE events (id Int64);").unwrap();
    let inner = Arc::new(RwLock::new(initial));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let mut reader = Some(inner.read().unwrap());
    let (request, _) = request_with_authorization_for_target(
        "/insert/events",
        b"1\n",
        "Authorization: Bearer correct-token\r\n\
         X-ClickHouse-Format: TabSeparated\r\n",
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
fn headerless_csv_insert_rolls_back_late_format_and_capacity_failures() {
    let database = SharedDatabase::with_max_rows_per_table(2);
    database
        .execute(
            "CREATE TABLE metrics (id Int64, score Float64, active Bool, label String); \
             INSERT INTO metrics VALUES (9, 9.0, true, 'existing');",
        )
        .unwrap();
    let cases: &[(&[u8], &str)] = &[
        (
            b"1,1.5,true,valid\n2,NaN,false,late\n",
            r#"{"error":"database CSV ingestion failed: CSV field at line 2, column 2 is not a valid Float64"}"#,
        ),
        (
            b"1,1.5,true,one\n2,2.5,false,two\n",
            r#"{"error":"database CSV ingestion failed: could not ingest CSV input: table rows requires at least 3, exceeding the limit of 2"}"#,
        ),
    ];

    for (csv, expected_body) in cases {
        let (request, _) = request_with_authorization_for_target(
            "/insert/metrics",
            csv,
            "Authorization: Bearer correct-token\r\n\
             X-ClickHouse-Format: CSV\r\n",
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
fn csv_table_insert_formats_return_503_without_waiting_for_a_reader() {
    let mut initial = Database::new();
    initial.execute("CREATE TABLE events (id Int64);").unwrap();
    let inner = Arc::new(RwLock::new(initial));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let mut reader = Some(inner.read().unwrap());
    for (csv, format_header) in [
        (b"id\n1\n".as_slice(), ""),
        (b"1\n".as_slice(), "X-ClickHouse-Format: CSV\r\n"),
    ] {
        let headers = format!("Authorization: Bearer correct-token\r\n{format_header}");
        let (request, _) = request_with_authorization_for_target("/insert/events", csv, &headers);
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
        worker.join().unwrap();
    }
    drop(reader.take());
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

#[test]
fn parameterized_readonly_tightens_key_requests_without_granting_access() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE events (id Int64); INSERT INTO events VALUES (7);")
        .unwrap();

    for request in [
        b"GET /?query=SELECT+id+FROM+events%3B&readonly=1 HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: correct-key\r\n\r\n".as_slice(),
        b"POST /?readonly=%30%31&query=SELECT+id+FROM+events%3B HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: correct-key\r\nContent-Length: 0\r\n\r\n".as_slice(),
    ] {
        let response = clickhouse_key_exchange(&database, "correct-key", request);
        assert_response(
            &response,
            "HTTP/1.1 200 OK",
            r#"{"columns":[{"name":"id","type":"Int64"}],"rows":[[7]]}"#,
        );
        assert_clickhouse_key_response_is_not_cacheable(&response);
    }

    let readonly_sql_insert = clickhouse_key_exchange(
        &database,
        "correct-key",
        b"POST /?query=INSERT+INTO+events+VALUES+%288%29%3B&readonly=1 HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: correct-key\r\nContent-Length: 0\r\n\r\n",
    );
    assert_response(
        &readonly_sql_insert,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"read-only query accepts only SELECT, SHOW DATABASES, SHOW SETTINGS, SHOW FUNCTIONS, SHOW TABLES, SHOW CREATE TABLE, DESCRIBE TABLE, or EXISTS TABLE; found INSERT"}"#,
    );

    let formatted_body = b"9\n";
    let (formatted_request, body_offset) = request_with_authorization_for_target(
        "/?query=INSERT+INTO+events+FORMAT+TabSeparated&readonly=1",
        formatted_body,
        "X-ClickHouse-Key: correct-key\r\n",
    );
    let mut formatted_input = Cursor::new(formatted_request);
    let mut formatted_response = Vec::new();
    handle_http_query_with_clickhouse_key(
        &database,
        "correct-key",
        &mut formatted_input,
        &mut formatted_response,
    )
    .unwrap();
    assert_eq!(formatted_input.position(), body_offset);
    assert_response(
        &formatted_response,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"POST /?query= does not accept a request body"}"#,
    );

    let retained_write_access = clickhouse_key_exchange(
        &database,
        "correct-key",
        b"POST /?readonly=0&query=INSERT+INTO+events+VALUES+%2810%29%3B HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: correct-key\r\nContent-Length: 0\r\n\r\n",
    );
    assert_response_with_content_type(
        &retained_write_access,
        "HTTP/1.1 200 OK",
        "text/plain; charset=utf-8",
        b"",
    );

    let read_only_handler_is_not_upgraded = read_only_clickhouse_key_exchange(
        &database,
        "correct-key",
        b"POST /?query=INSERT+INTO+events+VALUES+%2811%29%3B&readonly=0 HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: correct-key\r\nContent-Length: 0\r\n\r\n",
    );
    assert_response(
        &read_only_handler_is_not_upgraded,
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"read-only query accepts only SELECT, SHOW DATABASES, SHOW SETTINGS, SHOW FUNCTIONS, SHOW TABLES, SHOW CREATE TABLE, DESCRIBE TABLE, or EXISTS TABLE; found INSERT"}"#,
    );

    let rows = database
        .query("SELECT id FROM events ORDER BY id;")
        .unwrap();
    assert_eq!(
        rows.rows,
        vec![vec![Value::Int64(7)], vec![Value::Int64(10)]]
    );
}

#[test]
fn readonly_validation_follows_key_authentication_and_precedes_admission_and_mutation() {
    let mut initial = Database::new();
    initial.execute("CREATE TABLE events (id Int64);").unwrap();
    let inner = Arc::new(RwLock::new(initial));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));

    let unauthenticated = clickhouse_key_exchange(
        &database,
        "correct-key",
        b"GET /?query=SELECT+1%3B&readonly=nope HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert_response(
        &unauthenticated,
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":"X-ClickHouse-Key authentication required"}"#,
    );

    let writer = inner.write().unwrap();
    let cases = [
        (
            "readonly=0&read%6Fnly=1",
            r#"{"error":"duplicate readonly parameter"}"#,
        ),
        (
            "readonly=1.0",
            r#"{"error":"readonly parameter must be a decimal integer"}"#,
        ),
        (
            "readonly=2",
            r#"{"error":"readonly parameter must be 0 or 1"}"#,
        ),
    ];
    for method in ["GET", "POST"] {
        for (setting, expected_body) in cases {
            let content_length = if method == "POST" {
                "Content-Length: 0\r\n"
            } else {
                ""
            };
            let request = format!(
                "{method} /?query=INSERT+INTO+events+VALUES+%281%29%3B&{setting} HTTP/1.1\r\nHost: localhost\r\nX-ClickHouse-Key: correct-key\r\n{content_length}\r\n"
            );
            let response = clickhouse_key_exchange(&database, "correct-key", request.as_bytes());
            assert_response(&response, "HTTP/1.1 400 Bad Request", expected_body);
            assert_clickhouse_key_response_is_not_cacheable(&response);
        }
    }
    drop(writer);

    assert!(
        database
            .query("SELECT id FROM events;")
            .unwrap()
            .rows
            .is_empty()
    );
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
