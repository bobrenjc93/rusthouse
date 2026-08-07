//! Transport-neutral handling for one bounded HTTP query, insert, health, or metrics exchange.

use std::error::Error as StdError;
use std::fmt;
use std::io::{self, Read, Write};

use crate::batch::format::{
    write_csv, write_json, write_json_compact_each_row, write_json_each_row_with_limit,
    write_json_string, write_tsv,
};
use crate::{DatabaseMetrics, SharedDatabase, SharedDatabaseError};

/// Default maximum size of the request line and headers, including the final
/// empty line.
pub const DEFAULT_MAX_HTTP_HEADER_BYTES: usize = 16 * 1024;

/// Default maximum number of request header fields.
pub const DEFAULT_MAX_HTTP_HEADER_COUNT: usize = 64;

/// Default maximum size of the decoded SQL request.
pub const DEFAULT_MAX_HTTP_SQL_BYTES: usize = 1024 * 1024;

/// Default maximum size of the complete HTTP response, including headers.
pub const DEFAULT_MAX_HTTP_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// Resource limits for a single [`handle_http_query`] exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpQueryLimits {
    /// Maximum request-line and header bytes, including the terminating CRLF.
    pub max_header_bytes: usize,
    /// Maximum number of request header fields.
    pub max_header_count: usize,
    /// Maximum SQL bytes in a POST body or decoded GET query parameter.
    pub max_sql_bytes: usize,
    /// Maximum bytes in the complete HTTP response, including its headers.
    pub max_response_bytes: usize,
}

impl Default for HttpQueryLimits {
    fn default() -> Self {
        Self {
            max_header_bytes: DEFAULT_MAX_HTTP_HEADER_BYTES,
            max_header_count: DEFAULT_MAX_HTTP_HEADER_COUNT,
            max_sql_bytes: DEFAULT_MAX_HTTP_SQL_BYTES,
            max_response_bytes: DEFAULT_MAX_HTTP_RESPONSE_BYTES,
        }
    }
}

/// A transport failure while handling one HTTP query, insert, health, or metrics exchange.
///
/// Request and query errors that can be represented on the wire are returned
/// as HTTP responses and are not Rust errors. No response is written for a
/// read failure. A write failure may leave a partial response in the caller's
/// writer, as permitted by [`Write::write_all`].
#[derive(Debug)]
pub enum HttpQueryError {
    /// Reading the request failed.
    Read(io::Error),
    /// Writing the prepared response failed.
    Write(io::Error),
    /// The configured response cap cannot hold the fixed limit-error response.
    ResponseLimitExceeded { bytes: usize, max_bytes: usize },
}

impl fmt::Display for HttpQueryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(error) => write!(formatter, "could not read HTTP request: {error}"),
            Self::Write(error) => write!(formatter, "could not write HTTP response: {error}"),
            Self::ResponseLimitExceeded { bytes, max_bytes } => write!(
                formatter,
                "HTTP response requires {bytes} bytes, exceeding the limit of {max_bytes} bytes"
            ),
        }
    }
}

impl StdError for HttpQueryError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Read(error) | Self::Write(error) => Some(error),
            Self::ResponseLimitExceeded { .. } => None,
        }
    }
}

/// Handles one strict, bounded HTTP/1.1 exchange.
///
/// `POST /` and `POST /query` require exactly one decimal `Content-Length` and
/// carry UTF-8 SQL in their body. `GET /?query=<percent-encoded SQL>` carries
/// the same SQL in its sole form-style query parameter: percent escapes are
/// decoded, `+` becomes a space, and no request body is accepted. All three
/// forms pass the SQL to [`SharedDatabase::try_query`], which accepts exactly
/// one read-only statement and makes one nonblocking read-lock attempt. Writer
/// contention returns `503 Service Unavailable`; lock poisoning remains a
/// `500 Internal Server Error`. A successful query response uses the same JSON
/// result shape as the batch JSON formatter unless exactly one
/// `X-ClickHouse-Format` header requests `CSVWithNames`,
/// `TabSeparatedWithNames`, `JSONEachRow`, or `JSONCompactEachRow`. CSV, TSV,
/// and row-oriented JSON responses use the corresponding batch writers;
/// positional JSON responses contain arrays separated by line feeds.
///
/// The bearer-authenticated handlers additionally accept exact `POST /insert`
/// requests with the same body framing and limits. They pass the SQL to
/// [`SharedDatabase::try_execute_insert_batch`], which atomically executes a
/// nonempty `INSERT`-only batch after one nonblocking write-lock attempt.
/// Success returns an empty `200 OK` response. The unauthenticated handlers do
/// not expose this route.
///
/// `GET /metrics` accepts no request body and returns three Prometheus gauges
/// for retained tables, columns, and rows. It takes a nonblocking, consistent
/// database metrics snapshot; lock contention and poisoning return `503`.
/// `GET /ping` accepts no request body and returns the ClickHouse-compatible
/// plain-text body `Ok.\n`. It does not access or acquire a lock on the
/// database. `GET /ready` also accepts no body and returns the same successful
/// response only when a database read lock is immediately available. All
/// targets require CRLF framing and one nonempty `Host` header. Transfer
/// encoding, including chunked bodies, and `Expect` are rejected.
///
/// The handler does not open, close, or otherwise manage a listener or stream.
/// Each call reads one header block, reads exactly the declared POST body when
/// applicable, and emits at most one response. It never consumes a subsequent
/// exchange from the input.
///
/// # Errors
///
/// Returns [`HttpQueryError`] only when request input fails, response output
/// fails, or the response cap is too small to represent the fixed limit error.
pub fn handle_http_query(
    database: &SharedDatabase,
    input: impl Read,
    output: impl Write,
) -> Result<(), HttpQueryError> {
    handle_http_query_with_limits(database, input, output, HttpQueryLimits::default())
}

/// Handles one HTTP query, health, or metrics exchange with explicit resource limits.
///
/// See [`handle_http_query`] for the accepted protocol and response behavior.
/// The response limit covers the status line, headers, empty line, and body.
/// Responses are completely prepared and size-checked before the first byte is
/// written.
///
/// # Errors
///
/// Returns [`HttpQueryError`] under the same conditions as
/// [`handle_http_query`].
pub fn handle_http_query_with_limits(
    database: &SharedDatabase,
    input: impl Read,
    output: impl Write,
    limits: HttpQueryLimits,
) -> Result<(), HttpQueryError> {
    handle_http_query_exchange(database, input, output, limits, None)
}

/// Handles one HTTP query, insert, health, or metrics exchange that requires a bearer token.
///
/// This is separate from [`handle_http_query`], which remains unauthenticated.
/// Every request, including `GET /ping`, `GET /ready`, and `GET /metrics`, is
/// authorized only when it has exactly one `Authorization` header whose value
/// is `Bearer`, one or more spaces, and a token matching
/// `expected_bearer_token`.
/// Authentication failures receive the same response before the SQL body is
/// read or the database is accessed. The configured token must be a nonempty
/// RFC token68 value; invalid configurations are rejected as a server error
/// without reading any input.
///
/// This function provides authentication only. The embedding application must
/// provide TLS to keep the bearer token and query contents confidential in
/// transit.
///
/// # Errors
///
/// Returns [`HttpQueryError`] under the same conditions as
/// [`handle_http_query`].
pub fn handle_http_query_with_bearer_token(
    database: &SharedDatabase,
    expected_bearer_token: &str,
    input: impl Read,
    output: impl Write,
) -> Result<(), HttpQueryError> {
    handle_http_query_with_bearer_token_and_limits(
        database,
        expected_bearer_token,
        input,
        output,
        HttpQueryLimits::default(),
    )
}

/// Handles one bearer-authenticated HTTP exchange with explicit resource limits.
///
/// See [`handle_http_query_with_bearer_token`] for authentication behavior and
/// [`handle_http_query_with_limits`] for resource-limit behavior.
///
/// # Errors
///
/// Returns [`HttpQueryError`] under the same conditions as
/// [`handle_http_query`].
pub fn handle_http_query_with_bearer_token_and_limits(
    database: &SharedDatabase,
    expected_bearer_token: &str,
    input: impl Read,
    mut output: impl Write,
    limits: HttpQueryLimits,
) -> Result<(), HttpQueryError> {
    if expected_bearer_token.is_empty() {
        return write_error_response(
            &mut output,
            Status::INTERNAL_SERVER_ERROR,
            &[],
            "configured bearer token must not be empty",
            limits.max_response_bytes,
        );
    }
    if !is_valid_bearer_token(expected_bearer_token.as_bytes()) {
        return write_error_response(
            &mut output,
            Status::INTERNAL_SERVER_ERROR,
            &[],
            "configured bearer token is not valid token68",
            limits.max_response_bytes,
        );
    }

    handle_http_query_exchange(
        database,
        input,
        output,
        limits,
        Some(expected_bearer_token.as_bytes()),
    )
}

fn handle_http_query_exchange(
    database: &SharedDatabase,
    mut input: impl Read,
    mut output: impl Write,
    limits: HttpQueryLimits,
    expected_bearer_token: Option<&[u8]>,
) -> Result<(), HttpQueryError> {
    let request = match read_request(&mut input, limits, expected_bearer_token) {
        Ok(request) => request,
        Err(RequestReadError::Io(error)) => return Err(HttpQueryError::Read(error)),
        Err(RequestReadError::Protocol(failure)) => {
            return write_error_response(
                &mut output,
                failure.status,
                failure.extra_headers,
                failure.message,
                limits.max_response_bytes,
            );
        }
    };

    let (sql, response_format) = match request {
        HttpRequest::Ping => {
            return write_response(
                &mut output,
                Status::OK,
                &[],
                CONTENT_TYPE_TEXT,
                b"Ok.\n".to_vec(),
                limits.max_response_bytes,
            );
        }
        HttpRequest::Ready if database.is_read_lock_available() => {
            return write_response(
                &mut output,
                Status::OK,
                &[],
                CONTENT_TYPE_TEXT,
                b"Ok.\n".to_vec(),
                limits.max_response_bytes,
            );
        }
        HttpRequest::Ready => {
            return write_error_response(
                &mut output,
                Status::SERVICE_UNAVAILABLE,
                &[],
                "database is unavailable",
                limits.max_response_bytes,
            );
        }
        HttpRequest::Metrics => {
            let Some(metrics) = database.metrics_snapshot() else {
                return write_error_response(
                    &mut output,
                    Status::SERVICE_UNAVAILABLE,
                    &[],
                    "database is unavailable",
                    limits.max_response_bytes,
                );
            };
            let mut body = BoundedVec::new(limits.max_response_bytes);
            if write_prometheus_metrics(&mut body, metrics).is_err() {
                debug_assert!(body.limit_exceeded);
                return write_response_limit_error(&mut output, limits.max_response_bytes);
            }
            return write_response(
                &mut output,
                Status::OK,
                &[],
                CONTENT_TYPE_PROMETHEUS,
                body.bytes,
                limits.max_response_bytes,
            );
        }
        HttpRequest::Insert { sql } => {
            return match database.try_execute_insert_batch(&sql) {
                Ok(_) => write_response(
                    &mut output,
                    Status::OK,
                    &[],
                    CONTENT_TYPE_TEXT,
                    Vec::new(),
                    limits.max_response_bytes,
                ),
                Err(SharedDatabaseError::DatabaseBusy) => write_error_response(
                    &mut output,
                    Status::SERVICE_UNAVAILABLE,
                    &[],
                    "database is unavailable",
                    limits.max_response_bytes,
                ),
                Err(SharedDatabaseError::LockPoisoned) => write_error_response(
                    &mut output,
                    Status::INTERNAL_SERVER_ERROR,
                    &[],
                    "database is unavailable",
                    limits.max_response_bytes,
                ),
                Err(error) => write_error_response(
                    &mut output,
                    Status::BAD_REQUEST,
                    &[],
                    &error.to_string(),
                    limits.max_response_bytes,
                ),
            };
        }
        HttpRequest::Query {
            sql,
            response_format,
        } => (sql, response_format),
    };

    match database.try_query(&sql) {
        Ok(result) => {
            let mut body = BoundedVec::new(limits.max_response_bytes);
            let (write_failed, content_type) = match response_format {
                QueryResponseFormat::Json => {
                    (write_json(&mut body, &result).is_err(), CONTENT_TYPE_JSON)
                }
                QueryResponseFormat::CsvWithNames => {
                    (write_csv(&mut body, &result).is_err(), CONTENT_TYPE_CSV)
                }
                QueryResponseFormat::TabSeparatedWithNames => {
                    (write_tsv(&mut body, &result).is_err(), CONTENT_TYPE_TSV)
                }
                QueryResponseFormat::JsonEachRow => (
                    write_json_each_row_with_limit(&mut body, &result, limits.max_response_bytes)
                        .is_err(),
                    CONTENT_TYPE_JSON,
                ),
                QueryResponseFormat::JsonCompactEachRow => (
                    write_json_compact_each_row(&mut body, &result).is_err(),
                    CONTENT_TYPE_JSON,
                ),
            };
            if write_failed {
                debug_assert!(
                    body.limit_exceeded
                        || matches!(response_format, QueryResponseFormat::JsonEachRow)
                );
                return write_response_limit_error(&mut output, limits.max_response_bytes);
            }
            write_response(
                &mut output,
                Status::OK,
                &[],
                content_type,
                body.bytes,
                limits.max_response_bytes,
            )
        }
        Err(SharedDatabaseError::DatabaseBusy) => write_error_response(
            &mut output,
            Status::SERVICE_UNAVAILABLE,
            &[],
            "database is unavailable",
            limits.max_response_bytes,
        ),
        Err(SharedDatabaseError::LockPoisoned) => write_error_response(
            &mut output,
            Status::INTERNAL_SERVER_ERROR,
            &[],
            "database is unavailable",
            limits.max_response_bytes,
        ),
        Err(error) => write_error_response(
            &mut output,
            Status::BAD_REQUEST,
            &[],
            &error.to_string(),
            limits.max_response_bytes,
        ),
    }
}

fn read_request(
    input: &mut impl Read,
    limits: HttpQueryLimits,
    expected_bearer_token: Option<&[u8]>,
) -> Result<HttpRequest, RequestReadError> {
    let header = read_header_block(input, limits.max_header_bytes)?;
    let request = parse_headers(&header, limits.max_header_count, expected_bearer_token)?;

    match request.kind {
        RequestKind::Ping => {
            if request.content_length.unwrap_or(0) != 0 {
                return Err(RequestFailure::new(
                    Status::BAD_REQUEST,
                    "GET /ping does not accept a request body",
                )
                .into());
            }
            Ok(HttpRequest::Ping)
        }
        RequestKind::Ready => {
            if request.content_length.unwrap_or(0) != 0 {
                return Err(RequestFailure::new(
                    Status::BAD_REQUEST,
                    "GET /ready does not accept a request body",
                )
                .into());
            }
            Ok(HttpRequest::Ready)
        }
        RequestKind::Metrics => {
            if request.content_length.unwrap_or(0) != 0 {
                return Err(RequestFailure::new(
                    Status::BAD_REQUEST,
                    "GET /metrics does not accept a request body",
                )
                .into());
            }
            Ok(HttpRequest::Metrics)
        }
        RequestKind::Query(QuerySource::Body) => {
            let sql = read_sql_body(input, request.content_length, limits.max_sql_bytes)?;
            Ok(HttpRequest::Query {
                sql,
                response_format: request.response_format,
            })
        }
        RequestKind::Insert => {
            let sql = read_sql_body(input, request.content_length, limits.max_sql_bytes)?;
            Ok(HttpRequest::Insert { sql })
        }
        RequestKind::Query(QuerySource::UrlEncoded(encoded_sql)) => {
            if request.content_length.unwrap_or(0) != 0 {
                return Err(RequestFailure::new(
                    Status::BAD_REQUEST,
                    "GET /?query= does not accept a request body",
                )
                .into());
            }

            let sql = decode_query_parameter(&encoded_sql, limits.max_sql_bytes)?;
            String::from_utf8(sql)
                .map(|sql| HttpRequest::Query {
                    sql,
                    response_format: request.response_format,
                })
                .map_err(|_| {
                    RequestFailure::new(Status::BAD_REQUEST, "SQL query is not valid UTF-8").into()
                })
        }
    }
}

fn read_sql_body(
    input: &mut impl Read,
    content_length: Option<usize>,
    max_sql_bytes: usize,
) -> Result<String, RequestReadError> {
    let Some(content_length) = content_length else {
        return Err(RequestFailure::new(
            Status::LENGTH_REQUIRED,
            "Content-Length header is required",
        )
        .into());
    };
    if content_length > max_sql_bytes {
        return Err(RequestFailure::new(
            Status::PAYLOAD_TOO_LARGE,
            "request body exceeds configured byte limit",
        )
        .into());
    }

    let mut body = vec![0; content_length];
    let mut read = 0;
    while read < body.len() {
        match input.read(&mut body[read..]) {
            Ok(0) => {
                return Err(RequestFailure::new(
                    Status::BAD_REQUEST,
                    "request body is shorter than Content-Length",
                )
                .into());
            }
            Ok(bytes) => read += bytes,
            Err(error) => return Err(RequestReadError::Io(error)),
        }
    }

    String::from_utf8(body)
        .map_err(|_| RequestFailure::new(Status::BAD_REQUEST, "SQL body is not valid UTF-8").into())
}

fn read_header_block(
    input: &mut impl Read,
    max_header_bytes: usize,
) -> Result<Vec<u8>, RequestReadError> {
    if max_header_bytes == 0 {
        return Err(RequestFailure::new(
            Status::REQUEST_HEADER_FIELDS_TOO_LARGE,
            "request headers exceed configured byte limit",
        )
        .into());
    }

    let mut header = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        match input.read(&mut byte) {
            Ok(0) => {
                return Err(RequestFailure::new(
                    Status::BAD_REQUEST,
                    "request headers are incomplete",
                )
                .into());
            }
            Ok(_) => {
                let previous = header.last().copied();
                if (byte[0] == b'\n' && previous != Some(b'\r'))
                    || (previous == Some(b'\r') && byte[0] != b'\n')
                {
                    return Err(RequestFailure::new(
                        Status::BAD_REQUEST,
                        "HTTP headers require CRLF framing",
                    )
                    .into());
                }
                header.push(byte[0]);
            }
            Err(error) => return Err(RequestReadError::Io(error)),
        }

        if header.ends_with(b"\r\n\r\n") {
            return Ok(header);
        }
        if header.len() >= max_header_bytes {
            return Err(RequestFailure::new(
                Status::REQUEST_HEADER_FIELDS_TOO_LARGE,
                "request headers exceed configured byte limit",
            )
            .into());
        }
    }
}

struct ParsedRequest {
    kind: RequestKind,
    content_length: Option<usize>,
    response_format: QueryResponseFormat,
}

enum HttpRequest {
    Query {
        sql: String,
        response_format: QueryResponseFormat,
    },
    Insert {
        sql: String,
    },
    Ping,
    Ready,
    Metrics,
}

#[derive(Clone, Copy)]
enum QueryResponseFormat {
    Json,
    CsvWithNames,
    TabSeparatedWithNames,
    JsonEachRow,
    JsonCompactEachRow,
}

enum RequestKind {
    Query(QuerySource),
    Insert,
    Ping,
    Ready,
    Metrics,
}

enum QuerySource {
    Body,
    UrlEncoded(Vec<u8>),
}

fn parse_headers(
    header: &[u8],
    max_header_count: usize,
    expected_bearer_token: Option<&[u8]>,
) -> Result<ParsedRequest, RequestReadError> {
    let Some(without_terminator) = header.strip_suffix(b"\r\n\r\n") else {
        return Err(RequestFailure::new(Status::BAD_REQUEST, "malformed HTTP headers").into());
    };
    let mut lines = without_terminator.split(|byte| *byte == b'\n').peekable();
    let Some(raw_request_line) = lines.next() else {
        return Err(RequestFailure::new(Status::BAD_REQUEST, "missing request line").into());
    };
    let request_line = strict_header_line(raw_request_line, lines.peek().is_some())?;
    let kind = parse_request_line(request_line, expected_bearer_token.is_some())?;

    let mut content_length = None;
    let mut host_seen = false;
    let mut header_count = 0_usize;
    let mut transfer_encoding_seen = false;
    let mut expect_seen = false;
    let mut authorization = None;
    let mut duplicate_authorization = false;
    let mut clickhouse_format = None;
    let mut duplicate_clickhouse_format = false;
    while let Some(raw_line) = lines.next() {
        let line = strict_header_line(raw_line, lines.peek().is_some())?;
        header_count = header_count.saturating_add(1);
        if header_count > max_header_count {
            return Err(RequestFailure::new(
                Status::REQUEST_HEADER_FIELDS_TOO_LARGE,
                "request has too many header fields",
            )
            .into());
        }
        let Some(colon) = line.iter().position(|byte| *byte == b':') else {
            return Err(RequestFailure::new(Status::BAD_REQUEST, "malformed HTTP header").into());
        };
        let name = &line[..colon];
        let value = trim_optional_whitespace(&line[colon + 1..]);
        if name.is_empty()
            || !name.iter().copied().all(is_header_name_byte)
            || !value.iter().copied().all(is_header_value_byte)
        {
            return Err(RequestFailure::new(Status::BAD_REQUEST, "malformed HTTP header").into());
        }

        if name.eq_ignore_ascii_case(b"content-length") {
            if content_length.is_some() {
                return Err(RequestFailure::new(
                    Status::BAD_REQUEST,
                    "duplicate Content-Length header",
                )
                .into());
            }
            content_length = Some(parse_content_length(value)?);
        } else if name.eq_ignore_ascii_case(b"transfer-encoding") {
            transfer_encoding_seen = true;
        } else if name.eq_ignore_ascii_case(b"expect") {
            expect_seen = true;
        } else if name.eq_ignore_ascii_case(b"host") {
            if host_seen || value.is_empty() {
                return Err(RequestFailure::new(Status::BAD_REQUEST, "invalid Host header").into());
            }
            host_seen = true;
        } else if expected_bearer_token.is_some()
            && name.eq_ignore_ascii_case(b"authorization")
            && authorization.replace(value).is_some()
        {
            duplicate_authorization = true;
        } else if name.eq_ignore_ascii_case(b"x-clickhouse-format")
            && clickhouse_format.replace(value).is_some()
        {
            duplicate_clickhouse_format = true;
        }
    }

    if let Some(expected_bearer_token) = expected_bearer_token {
        let authorized = !duplicate_authorization
            && authorization
                .and_then(parse_bearer_token)
                .is_some_and(|provided| constant_work_eq(provided, expected_bearer_token));
        if !authorized {
            return Err(RequestFailure::with_headers(
                Status::UNAUTHORIZED,
                "bearer authentication required",
                &[b"WWW-Authenticate: Bearer\r\n"],
            )
            .into());
        }
    }

    if transfer_encoding_seen {
        return Err(
            RequestFailure::new(Status::BAD_REQUEST, "Transfer-Encoding is not supported").into(),
        );
    }
    if expect_seen {
        return Err(RequestFailure::new(
            Status::EXPECTATION_FAILED,
            "Expect header is not supported",
        )
        .into());
    }
    if !host_seen {
        return Err(RequestFailure::new(Status::BAD_REQUEST, "Host header is required").into());
    }
    let response_format = if matches!(&kind, RequestKind::Query(_)) {
        if duplicate_clickhouse_format {
            return Err(RequestFailure::new(
                Status::BAD_REQUEST,
                "duplicate X-ClickHouse-Format header",
            )
            .into());
        }
        match clickhouse_format {
            None => QueryResponseFormat::Json,
            Some(b"CSVWithNames") => QueryResponseFormat::CsvWithNames,
            Some(b"TabSeparatedWithNames") => QueryResponseFormat::TabSeparatedWithNames,
            Some(b"JSONEachRow") => QueryResponseFormat::JsonEachRow,
            Some(b"JSONCompactEachRow") => QueryResponseFormat::JsonCompactEachRow,
            Some(_) => {
                return Err(RequestFailure::new(
                    Status::BAD_REQUEST,
                    "unsupported X-ClickHouse-Format header",
                )
                .into());
            }
        }
    } else {
        QueryResponseFormat::Json
    };
    Ok(ParsedRequest {
        kind,
        content_length,
        response_format,
    })
}

fn parse_bearer_token(value: &[u8]) -> Option<&[u8]> {
    let scheme_length = b"Bearer".len();
    if value.len() <= scheme_length || !value[..scheme_length].eq_ignore_ascii_case(b"Bearer") {
        return None;
    }

    let separator_length = value[scheme_length..]
        .iter()
        .take_while(|byte| **byte == b' ')
        .count();
    if separator_length == 0 {
        return None;
    }
    let token = &value[scheme_length + separator_length..];
    is_valid_bearer_token(token).then_some(token)
}

fn is_valid_bearer_token(token: &[u8]) -> bool {
    let padding_start = token
        .iter()
        .position(|byte| *byte == b'=')
        .unwrap_or(token.len());
    let (unencoded, padding) = token.split_at(padding_start);
    !unencoded.is_empty()
        && unencoded.iter().copied().all(is_bearer_token_byte)
        && padding.iter().all(|byte| *byte == b'=')
}

fn is_bearer_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'+' | b'/')
}

fn constant_work_eq(left: &[u8], right: &[u8]) -> bool {
    let compared_bytes = left.len().max(right.len());
    let mut difference = left.len() ^ right.len();
    for index in 0..compared_bytes {
        let left_byte = left.get(index).copied().unwrap_or(0);
        let right_byte = right.get(index).copied().unwrap_or(0);
        difference |= usize::from(left_byte ^ right_byte);
    }
    difference == 0
}

fn parse_request_line(
    line: &[u8],
    authenticated_insert_enabled: bool,
) -> Result<RequestKind, RequestReadError> {
    let mut parts = line.split(|byte| *byte == b' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(RequestFailure::new(Status::BAD_REQUEST, "malformed request line").into());
    };
    if method.is_empty() || target.is_empty() || version.is_empty() {
        return Err(RequestFailure::new(Status::BAD_REQUEST, "malformed request line").into());
    }
    if version != b"HTTP/1.1" {
        return Err(RequestFailure::new(
            Status::HTTP_VERSION_NOT_SUPPORTED,
            "HTTP/1.1 is required",
        )
        .into());
    }
    const GET_QUERY_PREFIX: &[u8] = b"/?query=";

    match (method, target) {
        (b"POST", b"/" | b"/query") => Ok(RequestKind::Query(QuerySource::Body)),
        (b"POST", b"/insert") if authenticated_insert_enabled => Ok(RequestKind::Insert),
        (b"GET", target) if target.starts_with(GET_QUERY_PREFIX) => Ok(RequestKind::Query(
            QuerySource::UrlEncoded(target[GET_QUERY_PREFIX.len()..].to_vec()),
        )),
        (b"GET", b"/ping") => Ok(RequestKind::Ping),
        (b"GET", b"/ready") => Ok(RequestKind::Ready),
        (b"GET", b"/metrics") => Ok(RequestKind::Metrics),
        (_, b"/" | b"/query") => Err(RequestFailure::with_headers(
            Status::METHOD_NOT_ALLOWED,
            "method must be POST",
            &[b"Allow: POST\r\n"],
        )
        .into()),
        (_, target) if target.starts_with(GET_QUERY_PREFIX) => Err(RequestFailure::with_headers(
            Status::METHOD_NOT_ALLOWED,
            "method must be GET for /?query=",
            &[b"Allow: GET\r\n"],
        )
        .into()),
        (_, b"/ping") => Err(RequestFailure::with_headers(
            Status::METHOD_NOT_ALLOWED,
            "method must be GET for /ping",
            &[b"Allow: GET\r\n"],
        )
        .into()),
        (_, b"/ready") => Err(RequestFailure::with_headers(
            Status::METHOD_NOT_ALLOWED,
            "method must be GET for /ready",
            &[b"Allow: GET\r\n"],
        )
        .into()),
        (_, b"/metrics") => Err(RequestFailure::with_headers(
            Status::METHOD_NOT_ALLOWED,
            "method must be GET for /metrics",
            &[b"Allow: GET\r\n"],
        )
        .into()),
        (_, b"/insert") if authenticated_insert_enabled => Err(RequestFailure::with_headers(
            Status::METHOD_NOT_ALLOWED,
            "method must be POST for /insert",
            &[b"Allow: POST\r\n"],
        )
        .into()),
        (b"POST", _) => {
            Err(RequestFailure::new(Status::NOT_FOUND, "request target must be / or /query").into())
        }
        (b"GET", _) => Err(RequestFailure::new(
            Status::NOT_FOUND,
            "request target must be /ping, /ready, or /metrics",
        )
        .into()),
        _ => Err(RequestFailure::with_headers(
            Status::METHOD_NOT_ALLOWED,
            "method must be POST for / or /query or GET for /ping, /ready, or /metrics",
            &[b"Allow: GET, POST\r\n"],
        )
        .into()),
    }
}

fn decode_query_parameter(
    encoded: &[u8],
    max_sql_bytes: usize,
) -> Result<Vec<u8>, RequestReadError> {
    let mut decoded = Vec::with_capacity(encoded.len().min(max_sql_bytes));
    let mut index = 0_usize;
    while index < encoded.len() {
        let byte = match encoded[index] {
            b'&' => {
                return Err(RequestFailure::new(
                    Status::BAD_REQUEST,
                    "GET query target must contain exactly one query parameter",
                )
                .into());
            }
            b'+' => b' ',
            b'%' => {
                let Some(hex) = encoded.get(index + 1..index + 3) else {
                    return Err(RequestFailure::new(
                        Status::BAD_REQUEST,
                        "query parameter contains malformed percent encoding",
                    )
                    .into());
                };
                let Some(high) = decode_hex_digit(hex[0]) else {
                    return Err(RequestFailure::new(
                        Status::BAD_REQUEST,
                        "query parameter contains malformed percent encoding",
                    )
                    .into());
                };
                let Some(low) = decode_hex_digit(hex[1]) else {
                    return Err(RequestFailure::new(
                        Status::BAD_REQUEST,
                        "query parameter contains malformed percent encoding",
                    )
                    .into());
                };
                index += 2;
                (high << 4) | low
            }
            byte => byte,
        };

        if decoded.len() == max_sql_bytes {
            return Err(RequestFailure::new(
                Status::PAYLOAD_TOO_LARGE,
                "SQL query exceeds configured byte limit",
            )
            .into());
        }
        decoded.push(byte);
        index += 1;
    }
    Ok(decoded)
}

fn decode_hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn parse_content_length(value: &[u8]) -> Result<usize, RequestReadError> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return Err(
            RequestFailure::new(Status::BAD_REQUEST, "invalid Content-Length header").into(),
        );
    }
    let mut length = 0_usize;
    for byte in value {
        length = length
            .checked_mul(10)
            .and_then(|length| length.checked_add(usize::from(byte - b'0')))
            .ok_or_else(|| {
                RequestReadError::from(RequestFailure::new(
                    Status::PAYLOAD_TOO_LARGE,
                    "request body exceeds configured byte limit",
                ))
            })?;
    }
    Ok(length)
}

fn strict_header_line(line: &[u8], followed_by_line_feed: bool) -> Result<&[u8], RequestReadError> {
    if followed_by_line_feed {
        return line.strip_suffix(b"\r").ok_or_else(|| {
            RequestFailure::new(Status::BAD_REQUEST, "HTTP headers require CRLF framing").into()
        });
    }
    if line.contains(&b'\r') {
        return Err(RequestFailure::new(Status::BAD_REQUEST, "malformed HTTP header").into());
    }
    Ok(line)
}

fn trim_optional_whitespace(mut value: &[u8]) -> &[u8] {
    while matches!(value.first(), Some(b' ' | b'\t')) {
        value = &value[1..];
    }
    while matches!(value.last(), Some(b' ' | b'\t')) {
        value = &value[..value.len() - 1];
    }
    value
}

fn is_header_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn is_header_value_byte(byte: u8) -> bool {
    byte == b'\t' || (b' '..=b'~').contains(&byte)
}

#[derive(Clone, Copy)]
struct Status {
    code: u16,
    reason: &'static str,
}

impl Status {
    const OK: Self = Self::new(200, "OK");
    const BAD_REQUEST: Self = Self::new(400, "Bad Request");
    const UNAUTHORIZED: Self = Self::new(401, "Unauthorized");
    const NOT_FOUND: Self = Self::new(404, "Not Found");
    const METHOD_NOT_ALLOWED: Self = Self::new(405, "Method Not Allowed");
    const LENGTH_REQUIRED: Self = Self::new(411, "Length Required");
    const PAYLOAD_TOO_LARGE: Self = Self::new(413, "Payload Too Large");
    const EXPECTATION_FAILED: Self = Self::new(417, "Expectation Failed");
    const REQUEST_HEADER_FIELDS_TOO_LARGE: Self = Self::new(431, "Request Header Fields Too Large");
    const INTERNAL_SERVER_ERROR: Self = Self::new(500, "Internal Server Error");
    const SERVICE_UNAVAILABLE: Self = Self::new(503, "Service Unavailable");
    const HTTP_VERSION_NOT_SUPPORTED: Self = Self::new(505, "HTTP Version Not Supported");

    const fn new(code: u16, reason: &'static str) -> Self {
        Self { code, reason }
    }
}

struct RequestFailure {
    status: Status,
    message: &'static str,
    extra_headers: &'static [&'static [u8]],
}

impl RequestFailure {
    const fn new(status: Status, message: &'static str) -> Self {
        Self {
            status,
            message,
            extra_headers: &[],
        }
    }

    const fn with_headers(
        status: Status,
        message: &'static str,
        extra_headers: &'static [&'static [u8]],
    ) -> Self {
        Self {
            status,
            message,
            extra_headers,
        }
    }
}

enum RequestReadError {
    Io(io::Error),
    Protocol(RequestFailure),
}

impl From<RequestFailure> for RequestReadError {
    fn from(failure: RequestFailure) -> Self {
        Self::Protocol(failure)
    }
}

const RESPONSE_LIMIT_MESSAGE: &str = "response exceeds configured byte limit";
const CONTENT_TYPE_CSV: &[u8] = b"text/csv; charset=utf-8";
const CONTENT_TYPE_JSON: &[u8] = b"application/json";
const CONTENT_TYPE_TEXT: &[u8] = b"text/plain; charset=utf-8";
const CONTENT_TYPE_TSV: &[u8] = b"text/tab-separated-values; charset=utf-8";
const CONTENT_TYPE_PROMETHEUS: &[u8] = b"text/plain; version=0.0.4; charset=utf-8";

fn write_prometheus_metrics(output: &mut impl Write, metrics: DatabaseMetrics) -> io::Result<()> {
    writeln!(
        output,
        "# HELP rusthouse_tables Number of tables retained by the database.\n\
         # TYPE rusthouse_tables gauge\n\
         rusthouse_tables {}\n\
         # HELP rusthouse_columns Number of columns retained by the database.\n\
         # TYPE rusthouse_columns gauge\n\
         rusthouse_columns {}\n\
         # HELP rusthouse_retained_rows Number of rows retained across all tables.\n\
         # TYPE rusthouse_retained_rows gauge\n\
         rusthouse_retained_rows {}",
        metrics.table_count, metrics.column_count, metrics.retained_row_count,
    )
}

fn write_error_response(
    output: &mut impl Write,
    status: Status,
    extra_headers: &[&[u8]],
    message: &str,
    max_response_bytes: usize,
) -> Result<(), HttpQueryError> {
    let mut body = Vec::new();
    body.extend_from_slice(b"{\"error\":");
    write_json_string(&mut body, message).expect("writing JSON to a Vec cannot fail");
    body.extend_from_slice(b"}");
    write_response(
        output,
        status,
        extra_headers,
        CONTENT_TYPE_JSON,
        body,
        max_response_bytes,
    )
}

fn write_response(
    output: &mut impl Write,
    status: Status,
    extra_headers: &[&[u8]],
    content_type: &[u8],
    body: Vec<u8>,
    max_response_bytes: usize,
) -> Result<(), HttpQueryError> {
    match prepare_response(
        status,
        extra_headers,
        content_type,
        &body,
        max_response_bytes,
    ) {
        Ok(response) => output.write_all(&response).map_err(HttpQueryError::Write),
        Err(_) => write_response_limit_error(output, max_response_bytes),
    }
}

fn write_response_limit_error(
    output: &mut impl Write,
    max_response_bytes: usize,
) -> Result<(), HttpQueryError> {
    let mut body = Vec::new();
    body.extend_from_slice(b"{\"error\":");
    write_json_string(&mut body, RESPONSE_LIMIT_MESSAGE)
        .expect("writing JSON to a Vec cannot fail");
    body.extend_from_slice(b"}");
    let response = prepare_response(
        Status::INTERNAL_SERVER_ERROR,
        &[],
        CONTENT_TYPE_JSON,
        &body,
        max_response_bytes,
    )
    .map_err(|bytes| HttpQueryError::ResponseLimitExceeded {
        bytes,
        max_bytes: max_response_bytes,
    })?;
    output.write_all(&response).map_err(HttpQueryError::Write)
}

fn prepare_response(
    status: Status,
    extra_headers: &[&[u8]],
    content_type: &[u8],
    body: &[u8],
    max_response_bytes: usize,
) -> Result<Vec<u8>, usize> {
    let mut header = Vec::new();
    write!(header, "HTTP/1.1 {} {}\r\n", status.code, status.reason)
        .expect("writing HTTP headers to a Vec cannot fail");
    header.extend_from_slice(b"Content-Type: ");
    header.extend_from_slice(content_type);
    header.extend_from_slice(b"\r\n");
    write!(header, "Content-Length: {}\r\n", body.len())
        .expect("writing HTTP headers to a Vec cannot fail");
    header.extend_from_slice(b"Connection: close\r\n");
    for extra_header in extra_headers {
        header.extend_from_slice(extra_header);
    }
    header.extend_from_slice(b"\r\n");

    let response_bytes = header.len().checked_add(body.len()).unwrap_or(usize::MAX);
    if response_bytes > max_response_bytes {
        return Err(response_bytes);
    }
    header.extend_from_slice(body);
    Ok(header)
}

struct BoundedVec {
    bytes: Vec<u8>,
    max_bytes: usize,
    limit_exceeded: bool,
}

impl BoundedVec {
    fn new(max_bytes: usize) -> Self {
        Self {
            bytes: Vec::new(),
            max_bytes,
            limit_exceeded: false,
        }
    }
}

impl Write for BoundedVec {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let Some(bytes) = self.bytes.len().checked_add(buffer.len()) else {
            self.limit_exceeded = true;
            return Err(io::Error::other("response byte limit exceeded"));
        };
        if bytes > self.max_bytes {
            self.limit_exceeded = true;
            return Err(io::Error::other("response byte limit exceeded"));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
