//! Bounded HTTP exchanges and sequential read-only TCP serving.

use std::borrow::Cow;
use std::error::Error as StdError;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::time::Duration;

use crate::batch::csv::{CsvIngestError, CsvIngestLimits};
use crate::batch::format::{
    write_csv, write_csv_rows, write_json, write_json_compact_each_row,
    write_json_each_row_with_limit, write_json_string, write_tsv, write_tsv_rows,
};
use crate::batch::shared_database::{DatabaseMetricsSnapshot, DatabaseMetricsWithTables};
use crate::batch::sql::{self, Statement};
use crate::batch::storage::validate_table_name;
use crate::batch::tsv::{TsvIngestError, TsvIngestLimits};
use crate::{SharedDatabase, SharedDatabaseError};

/// Default maximum size of the request line and headers, including the final
/// empty line.
pub const DEFAULT_MAX_HTTP_HEADER_BYTES: usize = 16 * 1024;

/// Default maximum number of request header fields.
pub const DEFAULT_MAX_HTTP_HEADER_COUNT: usize = 64;

/// Default maximum size of the decoded SQL request.
pub const DEFAULT_MAX_HTTP_SQL_BYTES: usize = 1024 * 1024;

/// Default maximum size of the complete HTTP response, including headers.
pub const DEFAULT_MAX_HTTP_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// Default number of sequential connections accepted by
/// [`serve_http_read_only`].
pub const DEFAULT_MAX_HTTP_CONNECTIONS: usize = 1024;

/// Default read and write timeout for each accepted TCP connection.
pub const DEFAULT_HTTP_CONNECTION_TIMEOUT: Duration = Duration::from_secs(30);

/// Resource limits for a single [`handle_http_query`] exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpQueryLimits {
    /// Maximum request-line and header bytes, including the terminating CRLF.
    pub max_header_bytes: usize,
    /// Maximum number of request header fields.
    pub max_header_count: usize,
    /// Maximum bytes in a POST body or decoded URL SQL query parameter.
    pub max_sql_bytes: usize,
    /// Byte, row, and value limits for one `POST /insert/<table>` `CSV` or
    /// `CSVWithNames` body, including parameterized insertion in either format.
    ///
    /// The HTTP body must independently fit within [`Self::max_sql_bytes`].
    pub csv_ingest_limits: CsvIngestLimits,
    /// Byte, row, and value limits for one `POST /insert/<table>`
    /// `TabSeparated` or `TabSeparatedWithNames` body, including parameterized
    /// `TabSeparated` insertion.
    ///
    /// The HTTP body must independently fit within [`Self::max_sql_bytes`].
    pub tsv_ingest_limits: TsvIngestLimits,
    /// Maximum bytes in the complete HTTP response, including its headers.
    pub max_response_bytes: usize,
}

impl Default for HttpQueryLimits {
    fn default() -> Self {
        Self {
            max_header_bytes: DEFAULT_MAX_HTTP_HEADER_BYTES,
            max_header_count: DEFAULT_MAX_HTTP_HEADER_COUNT,
            max_sql_bytes: DEFAULT_MAX_HTTP_SQL_BYTES,
            csv_ingest_limits: CsvIngestLimits::default(),
            tsv_ingest_limits: TsvIngestLimits::default(),
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

/// Resource and lifecycle limits for a bounded read-only HTTP listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpListenerLimits {
    /// Maximum number of sequential TCP connections to accept before the
    /// listener is dropped and the function returns.
    pub max_connections: usize,
    /// Read and write timeout installed on every accepted connection.
    ///
    /// A zero duration is rejected before the listener accepts a connection.
    pub connection_timeout: Duration,
    /// Request, ingestion, and response limits passed to the existing
    /// read-only HTTP exchange handler.
    pub query_limits: HttpQueryLimits,
}

impl HttpListenerLimits {
    /// Creates explicit connection-count, timeout, and exchange bounds.
    pub const fn new(
        max_connections: usize,
        connection_timeout: Duration,
        query_limits: HttpQueryLimits,
    ) -> Self {
        Self {
            max_connections,
            connection_timeout,
            query_limits,
        }
    }
}

impl Default for HttpListenerLimits {
    fn default() -> Self {
        Self::new(
            DEFAULT_MAX_HTTP_CONNECTIONS,
            DEFAULT_HTTP_CONNECTION_TIMEOUT,
            HttpQueryLimits::default(),
        )
    }
}

/// A connection-local failure observed by the bounded HTTP listener.
///
/// These failures are recorded in [`HttpListenerReport`] and do not stop the
/// listener from accepting its remaining configured connections.
#[derive(Debug)]
pub enum HttpConnectionError {
    /// Installing the configured read timeout failed.
    SetReadTimeout(io::Error),
    /// Installing the configured write timeout failed.
    SetWriteTimeout(io::Error),
    /// The existing read-only HTTP exchange handler reported a transport
    /// failure.
    Exchange(HttpQueryError),
    /// Finishing the response side of the connection after its exchange
    /// failed.
    Shutdown(io::Error),
}

impl fmt::Display for HttpConnectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SetReadTimeout(error) => {
                write!(
                    formatter,
                    "could not set HTTP connection read timeout: {error}"
                )
            }
            Self::SetWriteTimeout(error) => {
                write!(
                    formatter,
                    "could not set HTTP connection write timeout: {error}"
                )
            }
            Self::Exchange(error) => error.fmt(formatter),
            Self::Shutdown(error) => write!(formatter, "could not close HTTP connection: {error}"),
        }
    }
}

impl StdError for HttpConnectionError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::SetReadTimeout(error) | Self::SetWriteTimeout(error) | Self::Shutdown(error) => {
                Some(error)
            }
            Self::Exchange(error) => Some(error),
        }
    }
}

/// One failed accepted connection and its one-based acceptance position.
#[derive(Debug)]
pub struct HttpConnectionFailure {
    /// One-based position of the connection in this listener run.
    pub connection: usize,
    /// The connection-local failure.
    pub error: HttpConnectionError,
}

/// Completion summary for a bounded read-only HTTP listener run.
#[derive(Debug, Default)]
pub struct HttpListenerReport {
    /// Number of TCP connections accepted.
    pub accepted_connections: usize,
    /// Number of accepted connections whose exchange and close completed.
    pub completed_connections: usize,
    /// Connection-local failures, in acceptance order.
    ///
    /// This vector contains at most [`HttpListenerLimits::max_connections`]
    /// entries.
    pub connection_failures: Vec<HttpConnectionFailure>,
}

/// A listener-level failure that prevents further connections from being
/// served.
#[derive(Debug)]
pub enum HttpListenerError {
    /// The configured per-connection timeout was zero.
    InvalidConnectionTimeout,
    /// Switching the owned listener to blocking mode failed.
    SetBlocking(io::Error),
    /// Accepting the next connection failed.
    Accept {
        /// Number of connections accepted before the failure.
        accepted_connections: usize,
        /// The socket error returned by [`TcpListener::accept`].
        source: io::Error,
    },
}

impl fmt::Display for HttpListenerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConnectionTimeout => {
                write!(
                    formatter,
                    "HTTP connection timeout must be greater than zero"
                )
            }
            Self::SetBlocking(error) => {
                write!(formatter, "could not configure HTTP listener: {error}")
            }
            Self::Accept {
                accepted_connections,
                source,
            } => write!(
                formatter,
                "could not accept HTTP connection after {accepted_connections} accepted connections: {source}"
            ),
        }
    }
}

impl StdError for HttpListenerError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::InvalidConnectionTimeout => None,
            Self::SetBlocking(error) => Some(error),
            Self::Accept { source, .. } => Some(source),
        }
    }
}

/// Serves a bounded sequence of read-only HTTP connections with default
/// limits.
///
/// See [`serve_http_read_only_with_limits`] for listener lifecycle and failure
/// behavior. The listener is consumed so it is always dropped when the
/// connection budget is exhausted or a listener-level error is returned.
///
/// # Errors
///
/// Returns [`HttpListenerError`] when listener configuration or connection
/// acceptance fails. Connection-local failures are returned in the successful
/// [`HttpListenerReport`] after all configured connections have been accepted.
pub fn serve_http_read_only(
    database: &SharedDatabase,
    listener: TcpListener,
) -> Result<HttpListenerReport, HttpListenerError> {
    serve_http_read_only_with_limits(database, listener, HttpListenerLimits::default())
}

/// Serves a bounded sequence of read-only HTTP connections with explicit
/// limits.
///
/// Connections are accepted and handled sequentially against the same
/// [`SharedDatabase`]. Each accepted stream receives the configured read and
/// write timeout, is dispatched through [`handle_http_query_with_limits`], and
/// is explicitly closed after at most one response. A protocol failure that
/// can be represented as an HTTP response counts as a completed exchange. A
/// transport or socket failure is recorded in the returned report, its stream
/// is dropped, and the listener continues with the next connection.
///
/// The function returns after exactly [`HttpListenerLimits::max_connections`]
/// accepts; a zero connection limit returns immediately. It does not create
/// worker threads, terminate TLS, authenticate clients, or accept mutating SQL.
/// Callers should bind a loopback address or provide equivalent network access
/// control unless public, unauthenticated read access is intended.
///
/// # Errors
///
/// Returns [`HttpListenerError::InvalidConnectionTimeout`] before accepting a
/// connection when the timeout is zero. Listener configuration and accept
/// failures are also returned immediately. Failures after a connection has
/// been accepted remain connection-local and are collected in the successful
/// [`HttpListenerReport`].
pub fn serve_http_read_only_with_limits(
    database: &SharedDatabase,
    listener: TcpListener,
    limits: HttpListenerLimits,
) -> Result<HttpListenerReport, HttpListenerError> {
    if limits.connection_timeout.is_zero() {
        return Err(HttpListenerError::InvalidConnectionTimeout);
    }
    listener
        .set_nonblocking(false)
        .map_err(HttpListenerError::SetBlocking)?;

    let mut report = HttpListenerReport::default();
    while report.accepted_connections < limits.max_connections {
        let (stream, _) = listener
            .accept()
            .map_err(|source| HttpListenerError::Accept {
                accepted_connections: report.accepted_connections,
                source,
            })?;
        report.accepted_connections += 1;
        match serve_http_read_only_connection(database, stream, limits) {
            Ok(()) => report.completed_connections += 1,
            Err(error) => report.connection_failures.push(HttpConnectionFailure {
                connection: report.accepted_connections,
                error,
            }),
        }
    }

    Ok(report)
}

fn serve_http_read_only_connection(
    database: &SharedDatabase,
    stream: TcpStream,
    limits: HttpListenerLimits,
) -> Result<(), HttpConnectionError> {
    let exchange = (|| {
        stream
            .set_read_timeout(Some(limits.connection_timeout))
            .map_err(HttpConnectionError::SetReadTimeout)?;
        stream
            .set_write_timeout(Some(limits.connection_timeout))
            .map_err(HttpConnectionError::SetWriteTimeout)?;
        handle_http_query_with_limits(database, &stream, &stream, limits.query_limits)
            .map_err(HttpConnectionError::Exchange)
    })();
    let shutdown = stream
        .shutdown(Shutdown::Write)
        .map_err(HttpConnectionError::Shutdown);
    exchange.and(shutdown)
}

/// Handles one strict, bounded HTTP/1.1 exchange.
///
/// `POST /` and `POST /query` require exactly one decimal `Content-Length` and
/// carry UTF-8 SQL in their body. `GET /?query=<percent-encoded SQL>` and
/// `POST /?query=<percent-encoded SQL>` carry the same SQL in a required
/// form-style query parameter and optionally accept one `database=default`
/// parameter, one decimal `max_result_rows` parameter, one decimal
/// `max_result_bytes` parameter, and one `default_format` parameter in any
/// order. Nonzero result limits can tighten but never relax the database's
/// configured query-result limits; `max_result_bytes` also cannot relax the
/// default retained-result byte limit. Zero disables the corresponding
/// request-level limit while retaining the configured defaults. `default_format`
/// accepts `JSON`, `CSV`, `CSVWithNames`, `TabSeparated`,
/// `TabSeparatedWithNames`, `JSONEachRow`, or `JSONCompactEachRow`. Parameter
/// names and values are percent-decoded, and `+` becomes a space. An
/// insertion-capable authenticated `POST` additionally accepts a headerless CSV
/// body when the decoded SQL is exactly `INSERT INTO <table> FORMAT CSV`, a
/// `CSVWithNames` body when it ends in `FORMAT CSVWithNames`, or a headerless
/// TSV body when it ends in `FORMAT TabSeparated`; it routes the body through
/// the same bounded, atomic, nonblocking importer as `POST /insert/<table>`.
/// All other parameterized queries require an absent or zero `Content-Length`.
/// Empty, duplicate, unknown, and unsupported parameters, plus malformed or
/// overflowing result-limit values, are rejected after authentication and before
/// database lock admission. A
/// `default_format` parameter cannot be combined with an
/// `X-ClickHouse-Format` header. Exactly one read-only query on any query form
/// may instead end in a case-insensitive SQL `FORMAT CSVWithNames`, `FORMAT
/// TabSeparated`, or `FORMAT JSONEachRow` clause, with an optional trailing
/// semicolon. The clause selects the corresponding existing bounded writer and
/// cannot be combined with either HTTP format selector. Quoted strings and line
/// comments are not interpreted as format clauses. GET
/// requests and every request made through a
/// read-only handler pass the SQL to [`SharedDatabase::try_query`], which
/// accepts exactly one read-only statement and makes one nonblocking read-lock
/// attempt. POST requests made through the insertion-capable handlers without an
/// output-format selector additionally accept a nonempty `INSERT`-only batch
/// through [`SharedDatabase::try_execute_insert_batch`].
/// Mixed batches, other mutations, and INSERTs with an output-format selector
/// are rejected without mutation. Contention returns `503 Service Unavailable`;
/// lock poisoning remains a `500 Internal Server Error`.
/// A successful query response uses the same JSON result shape as the batch
/// JSON formatter unless one format selector requests a streaming format.
/// Parameterized-query `default_format` additionally accepts an explicit `JSON`; the
/// `X-ClickHouse-Format` header accepts `CSV`, `CSVWithNames`, `TabSeparated`,
/// `TabSeparatedWithNames`, `JSONEachRow`, or `JSONCompactEachRow`. CSV, TSV,
/// and row-oriented JSON responses use the corresponding batch writers;
/// headerless `CSV` and `TabSeparated` omit column names and emit no bytes for
/// an empty result, while positional JSON responses contain arrays separated
/// by line feeds.
/// Every query form also accepts at most one case-insensitive
/// `X-ClickHouse-Database` header whose value is exactly `default`, matching
/// RustHouse's single logical database. Empty, duplicate, and other values are
/// rejected after authentication and before a request body is read or the
/// database is accessed.
///
/// The insertion-capable bearer-authenticated handlers also accept exact
/// `POST /insert` requests with the same body framing and limits. Like
/// authenticated POST query-route INSERTs, they use
/// [`SharedDatabase::try_execute_insert_batch`], which
/// atomically executes a nonempty `INSERT`-only batch after one nonblocking
/// write-lock attempt. Exact
/// `POST /insert/<table>` requests treat the bounded body as `CSVWithNames` by
/// default. An exact `X-ClickHouse-Format: CSV` header selects headerless CSV
/// input in physical schema order. Exact `X-ClickHouse-Format: TabSeparated`
/// similarly selects headerless TSV in physical schema order, while
/// `TabSeparatedWithNames` selects named TSV; `CSVWithNames` may also be
/// selected explicitly. The corresponding independent ingestion limits and
/// nonblocking [`SharedDatabase`] importer are used. Success returns an empty
/// `200 OK` response. The unauthenticated handlers do not expose either route.
/// The insertion-capable `X-ClickHouse-Key`-authenticated handlers expose the
/// same route set. Both authenticated insert forms accept the same optional
/// `X-ClickHouse-Database: default` header as the query forms. The
/// [`handle_http_query_read_only_with_bearer_token`] and
/// [`handle_http_query_read_only_with_clickhouse_key`] families require the
/// same credentials but do not expose either explicit insertion route or
/// enable INSERT execution on standard query routes.
///
/// `GET /metrics` accepts no request body and returns four unlabeled Prometheus
/// gauges for database totals plus `rusthouse_table_rows` and
/// `rusthouse_table_retained_value_bytes` gauges per current table. It takes a
/// nonblocking, consistent database metrics snapshot; lock contention and
/// poisoning return `503`.
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
    handle_http_query_exchange(
        database,
        input,
        output,
        limits,
        Authentication::None,
        HttpAccess::ReadOnly,
    )
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
    handle_http_query_with_bearer_token_access_and_limits(
        database,
        expected_bearer_token,
        input,
        &mut output,
        limits,
        HttpAccess::ReadWrite,
    )
}

/// Handles one read-only HTTP exchange that requires a bearer token.
///
/// Authentication and resource limits are identical to
/// [`handle_http_query_with_bearer_token`], but this least-privilege variant
/// never exposes `POST /insert` or `POST /insert/<table>` and never enables
/// INSERT execution or parameterized query-plus-data ingestion on a standard
/// query route. Explicit insertion routes and parameterized data bodies are
/// rejected after authentication and before their body is read or the database
/// is accessed. The existing bearer-token handlers retain their insertion
/// behavior for backward compatibility.
///
/// # Errors
///
/// Returns [`HttpQueryError`] under the same conditions as
/// [`handle_http_query`].
pub fn handle_http_query_read_only_with_bearer_token(
    database: &SharedDatabase,
    expected_bearer_token: &str,
    input: impl Read,
    output: impl Write,
) -> Result<(), HttpQueryError> {
    handle_http_query_read_only_with_bearer_token_and_limits(
        database,
        expected_bearer_token,
        input,
        output,
        HttpQueryLimits::default(),
    )
}

/// Handles one read-only bearer-authenticated HTTP exchange with explicit
/// resource limits.
///
/// See [`handle_http_query_read_only_with_bearer_token`] for access-control
/// behavior and [`handle_http_query_with_limits`] for resource-limit behavior.
///
/// # Errors
///
/// Returns [`HttpQueryError`] under the same conditions as
/// [`handle_http_query`].
pub fn handle_http_query_read_only_with_bearer_token_and_limits(
    database: &SharedDatabase,
    expected_bearer_token: &str,
    input: impl Read,
    mut output: impl Write,
    limits: HttpQueryLimits,
) -> Result<(), HttpQueryError> {
    handle_http_query_with_bearer_token_access_and_limits(
        database,
        expected_bearer_token,
        input,
        &mut output,
        limits,
        HttpAccess::ReadOnly,
    )
}

fn handle_http_query_with_bearer_token_access_and_limits(
    database: &SharedDatabase,
    expected_bearer_token: &str,
    input: impl Read,
    mut output: impl Write,
    limits: HttpQueryLimits,
    access: HttpAccess,
) -> Result<(), HttpQueryError> {
    if expected_bearer_token.is_empty() {
        return write_error_response(
            &mut output,
            Status::INTERNAL_SERVER_ERROR,
            &[],
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
        Authentication::Bearer(expected_bearer_token.as_bytes()),
        access,
    )
}

/// Handles one HTTP query, insert, health, or metrics exchange that requires
/// an `X-ClickHouse-Key` credential.
///
/// This is separate from [`handle_http_query`] and the bearer-authenticated
/// handlers. Every request, including `GET /ping`, `GET /ready`, and
/// `GET /metrics`, is authorized only when it has exactly one
/// case-insensitive `X-ClickHouse-Key` header whose value matches
/// `expected_clickhouse_key`. Header values are compared case-sensitively.
/// Missing, duplicate, empty, and incorrect credentials receive the same
/// `401 Unauthorized` response with an `X-ClickHouse-Key` authentication
/// challenge before the SQL body is read or the database is accessed. Every
/// response includes `Cache-Control: private, no-store` so authenticated GET
/// results cannot be reused by a shared cache.
///
/// The configured key must be a nonempty HTTP field value without leading or
/// trailing optional whitespace. Invalid configurations are rejected as a
/// server error without reading any input. This function provides
/// authentication only; the embedding application must provide TLS to keep
/// the key and query contents confidential in transit.
///
/// # Errors
///
/// Returns [`HttpQueryError`] under the same conditions as
/// [`handle_http_query`].
pub fn handle_http_query_with_clickhouse_key(
    database: &SharedDatabase,
    expected_clickhouse_key: &str,
    input: impl Read,
    output: impl Write,
) -> Result<(), HttpQueryError> {
    handle_http_query_with_clickhouse_key_and_limits(
        database,
        expected_clickhouse_key,
        input,
        output,
        HttpQueryLimits::default(),
    )
}

/// Handles one `X-ClickHouse-Key`-authenticated HTTP exchange with explicit
/// resource limits.
///
/// See [`handle_http_query_with_clickhouse_key`] for authentication behavior
/// and [`handle_http_query_with_limits`] for resource-limit behavior.
///
/// # Errors
///
/// Returns [`HttpQueryError`] under the same conditions as
/// [`handle_http_query`].
pub fn handle_http_query_with_clickhouse_key_and_limits(
    database: &SharedDatabase,
    expected_clickhouse_key: &str,
    input: impl Read,
    mut output: impl Write,
    limits: HttpQueryLimits,
) -> Result<(), HttpQueryError> {
    handle_http_query_with_clickhouse_key_access_and_limits(
        database,
        expected_clickhouse_key,
        input,
        &mut output,
        limits,
        HttpAccess::ReadWrite,
    )
}

/// Handles one read-only HTTP exchange that requires an `X-ClickHouse-Key`.
///
/// Authentication, cache-control headers, and resource limits are identical
/// to [`handle_http_query_with_clickhouse_key`], but this least-privilege
/// variant never exposes `POST /insert` or `POST /insert/<table>` and never
/// enables INSERT execution or parameterized query-plus-data ingestion on a
/// standard query route. Explicit insertion routes and parameterized data
/// bodies are rejected after authentication and before their body is read or
/// the database is accessed. The existing key handlers retain their insertion
/// behavior for backward compatibility.
///
/// # Errors
///
/// Returns [`HttpQueryError`] under the same conditions as
/// [`handle_http_query`].
pub fn handle_http_query_read_only_with_clickhouse_key(
    database: &SharedDatabase,
    expected_clickhouse_key: &str,
    input: impl Read,
    output: impl Write,
) -> Result<(), HttpQueryError> {
    handle_http_query_read_only_with_clickhouse_key_and_limits(
        database,
        expected_clickhouse_key,
        input,
        output,
        HttpQueryLimits::default(),
    )
}

/// Handles one read-only `X-ClickHouse-Key`-authenticated HTTP exchange with
/// explicit resource limits.
///
/// See [`handle_http_query_read_only_with_clickhouse_key`] for access-control
/// behavior and [`handle_http_query_with_limits`] for resource-limit behavior.
///
/// # Errors
///
/// Returns [`HttpQueryError`] under the same conditions as
/// [`handle_http_query`].
pub fn handle_http_query_read_only_with_clickhouse_key_and_limits(
    database: &SharedDatabase,
    expected_clickhouse_key: &str,
    input: impl Read,
    mut output: impl Write,
    limits: HttpQueryLimits,
) -> Result<(), HttpQueryError> {
    handle_http_query_with_clickhouse_key_access_and_limits(
        database,
        expected_clickhouse_key,
        input,
        &mut output,
        limits,
        HttpAccess::ReadOnly,
    )
}

fn handle_http_query_with_clickhouse_key_access_and_limits(
    database: &SharedDatabase,
    expected_clickhouse_key: &str,
    input: impl Read,
    mut output: impl Write,
    limits: HttpQueryLimits,
    access: HttpAccess,
) -> Result<(), HttpQueryError> {
    if expected_clickhouse_key.is_empty() {
        return write_error_response(
            &mut output,
            Status::INTERNAL_SERVER_ERROR,
            &[],
            CLICKHOUSE_KEY_RESPONSE_HEADERS,
            "configured ClickHouse key must not be empty",
            limits.max_response_bytes,
        );
    }
    if !is_valid_clickhouse_key(expected_clickhouse_key.as_bytes()) {
        return write_error_response(
            &mut output,
            Status::INTERNAL_SERVER_ERROR,
            &[],
            CLICKHOUSE_KEY_RESPONSE_HEADERS,
            "configured ClickHouse key is not a valid HTTP header value",
            limits.max_response_bytes,
        );
    }

    handle_http_query_exchange(
        database,
        input,
        output,
        limits,
        Authentication::ClickHouseKey(expected_clickhouse_key.as_bytes()),
        access,
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HttpAccess {
    ReadOnly,
    ReadWrite,
}

impl HttpAccess {
    const fn allows_insert(self) -> bool {
        matches!(self, Self::ReadWrite)
    }
}

#[derive(Clone, Copy)]
enum Authentication<'a> {
    None,
    Bearer(&'a [u8]),
    ClickHouseKey(&'a [u8]),
}

impl Authentication<'_> {
    const fn is_configured(self) -> bool {
        !matches!(self, Self::None)
    }

    const fn response_headers(self) -> &'static [&'static [u8]] {
        match self {
            Self::ClickHouseKey(_) => CLICKHOUSE_KEY_RESPONSE_HEADERS,
            Self::None | Self::Bearer(_) => &[],
        }
    }
}

fn handle_http_query_exchange(
    database: &SharedDatabase,
    mut input: impl Read,
    mut output: impl Write,
    limits: HttpQueryLimits,
    authentication: Authentication<'_>,
    access: HttpAccess,
) -> Result<(), HttpQueryError> {
    let response_headers = authentication.response_headers();
    let request = match read_request(&mut input, limits, authentication, access) {
        Ok(request) => request,
        Err(RequestReadError::Io(error)) => return Err(HttpQueryError::Read(error)),
        Err(RequestReadError::Protocol(failure)) => {
            return write_error_response(
                &mut output,
                failure.status,
                failure.extra_headers,
                response_headers,
                failure.message.as_ref(),
                limits.max_response_bytes,
            );
        }
    };

    let (sql, response_format, max_result_bytes, max_result_rows) = match request {
        HttpRequest::Ping => {
            return write_response(
                &mut output,
                Status::OK,
                &[],
                response_headers,
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
                response_headers,
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
                response_headers,
                "database is unavailable",
                limits.max_response_bytes,
            );
        }
        HttpRequest::Metrics => {
            let metrics = match database.metrics_snapshot_with_tables(
                |totals, table_name_bytes, row_count_bytes, retained_value_byte_count_bytes| {
                    let body_bytes = prometheus_metrics_body_len(
                        totals,
                        table_name_bytes,
                        row_count_bytes,
                        retained_value_byte_count_bytes,
                    );
                    response_len(
                        Status::OK,
                        &[],
                        response_headers,
                        CONTENT_TYPE_PROMETHEUS,
                        body_bytes,
                    ) <= limits.max_response_bytes
                },
            ) {
                DatabaseMetricsSnapshot::Available(metrics) => metrics,
                DatabaseMetricsSnapshot::ResponseLimitExceeded => {
                    return write_response_limit_error(
                        &mut output,
                        response_headers,
                        limits.max_response_bytes,
                    );
                }
                DatabaseMetricsSnapshot::Unavailable => {
                    return write_error_response(
                        &mut output,
                        Status::SERVICE_UNAVAILABLE,
                        &[],
                        response_headers,
                        "database is unavailable",
                        limits.max_response_bytes,
                    );
                }
            };
            let mut body = BoundedVec::new(limits.max_response_bytes);
            if write_prometheus_metrics(&mut body, metrics).is_err() {
                debug_assert!(body.limit_exceeded);
                return write_response_limit_error(
                    &mut output,
                    response_headers,
                    limits.max_response_bytes,
                );
            }
            return write_response(
                &mut output,
                Status::OK,
                &[],
                response_headers,
                CONTENT_TYPE_PROMETHEUS,
                body.bytes,
                limits.max_response_bytes,
            );
        }
        HttpRequest::Insert { sql } => {
            let success_response = match prepare_response(
                Status::OK,
                &[],
                response_headers,
                CONTENT_TYPE_TEXT,
                &[],
                limits.max_response_bytes,
            ) {
                Ok(response) => response,
                Err(_) => {
                    return write_response_limit_error(
                        &mut output,
                        response_headers,
                        limits.max_response_bytes,
                    );
                }
            };
            return match database.try_execute_insert_batch(&sql) {
                Ok(_) => output
                    .write_all(&success_response)
                    .map_err(HttpQueryError::Write),
                Err(SharedDatabaseError::DatabaseBusy) => write_error_response(
                    &mut output,
                    Status::SERVICE_UNAVAILABLE,
                    &[],
                    response_headers,
                    "database is unavailable",
                    limits.max_response_bytes,
                ),
                Err(SharedDatabaseError::LockPoisoned) => write_error_response(
                    &mut output,
                    Status::INTERNAL_SERVER_ERROR,
                    &[],
                    response_headers,
                    "database is unavailable",
                    limits.max_response_bytes,
                ),
                Err(error) => write_error_response(
                    &mut output,
                    Status::BAD_REQUEST,
                    &[],
                    response_headers,
                    &error.to_string(),
                    limits.max_response_bytes,
                ),
            };
        }
        HttpRequest::TableInsert {
            table,
            body,
            input_format,
        } => {
            let success_response = match prepare_response(
                Status::OK,
                &[],
                response_headers,
                CONTENT_TYPE_TEXT,
                &[],
                limits.max_response_bytes,
            ) {
                Ok(response) => response,
                Err(_) => {
                    return write_response_limit_error(
                        &mut output,
                        response_headers,
                        limits.max_response_bytes,
                    );
                }
            };
            let result = match input_format {
                TableInsertFormat::Csv => {
                    database.try_ingest_csv(&table, body, limits.csv_ingest_limits)
                }
                TableInsertFormat::CsvWithNames => {
                    database.try_ingest_csv_with_names(&table, body, limits.csv_ingest_limits)
                }
                TableInsertFormat::TabSeparated => {
                    database.try_ingest_tsv(&table, body, limits.tsv_ingest_limits)
                }
                TableInsertFormat::TabSeparatedWithNames => {
                    database.try_ingest_tsv_with_names(&table, body, limits.tsv_ingest_limits)
                }
            };
            return match result {
                Ok(_) => output
                    .write_all(&success_response)
                    .map_err(HttpQueryError::Write),
                Err(SharedDatabaseError::DatabaseBusy) => write_error_response(
                    &mut output,
                    Status::SERVICE_UNAVAILABLE,
                    &[],
                    response_headers,
                    "database is unavailable",
                    limits.max_response_bytes,
                ),
                Err(SharedDatabaseError::LockPoisoned) => write_error_response(
                    &mut output,
                    Status::INTERNAL_SERVER_ERROR,
                    &[],
                    response_headers,
                    "database is unavailable",
                    limits.max_response_bytes,
                ),
                Err(error) => write_error_response(
                    &mut output,
                    Status::BAD_REQUEST,
                    &[],
                    response_headers,
                    &error.to_string(),
                    limits.max_response_bytes,
                ),
            };
        }
        HttpRequest::Query {
            sql,
            response_format,
            max_result_bytes,
            max_result_rows,
        } => (sql, response_format, max_result_bytes, max_result_rows),
    };

    let query_result = match (max_result_bytes, max_result_rows) {
        (None, None) => database.try_query(&sql),
        (max_result_bytes, max_result_rows) => database.try_query_with_parameterized_result_limits(
            &sql,
            max_result_bytes.unwrap_or(0),
            max_result_rows.unwrap_or(0),
        ),
    };
    match query_result {
        Ok(result) => {
            let mut body = BoundedVec::new(limits.max_response_bytes);
            let (write_failed, content_type) = match response_format {
                QueryResponseFormat::Json => {
                    (write_json(&mut body, &result).is_err(), CONTENT_TYPE_JSON)
                }
                QueryResponseFormat::CsvWithNames => {
                    (write_csv(&mut body, &result).is_err(), CONTENT_TYPE_CSV)
                }
                QueryResponseFormat::Csv => (
                    write_csv_rows(&mut body, &result).is_err(),
                    CONTENT_TYPE_CSV,
                ),
                QueryResponseFormat::TabSeparatedWithNames => {
                    (write_tsv(&mut body, &result).is_err(), CONTENT_TYPE_TSV)
                }
                QueryResponseFormat::TabSeparated => (
                    write_tsv_rows(&mut body, &result).is_err(),
                    CONTENT_TYPE_TSV,
                ),
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
                return write_response_limit_error(
                    &mut output,
                    response_headers,
                    limits.max_response_bytes,
                );
            }
            write_response(
                &mut output,
                Status::OK,
                &[],
                response_headers,
                content_type,
                body.bytes,
                limits.max_response_bytes,
            )
        }
        Err(SharedDatabaseError::DatabaseBusy) => write_error_response(
            &mut output,
            Status::SERVICE_UNAVAILABLE,
            &[],
            response_headers,
            "database is unavailable",
            limits.max_response_bytes,
        ),
        Err(SharedDatabaseError::LockPoisoned) => write_error_response(
            &mut output,
            Status::INTERNAL_SERVER_ERROR,
            &[],
            response_headers,
            "database is unavailable",
            limits.max_response_bytes,
        ),
        Err(error) => write_error_response(
            &mut output,
            Status::BAD_REQUEST,
            &[],
            response_headers,
            &error.to_string(),
            limits.max_response_bytes,
        ),
    }
}

fn read_request(
    input: &mut impl Read,
    limits: HttpQueryLimits,
    authentication: Authentication<'_>,
    access: HttpAccess,
) -> Result<HttpRequest, RequestReadError> {
    let header = read_header_block(input, limits.max_header_bytes)?;
    let request = parse_headers(&header, limits.max_header_count, authentication, access)?;

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
            let output_format_selected = request.response_format.is_some();
            classify_standard_query_request(
                sql,
                request.response_format,
                None,
                None,
                access.allows_insert() && !output_format_selected,
            )
        }
        RequestKind::Insert => {
            let sql = read_sql_body(input, request.content_length, limits.max_sql_bytes)?;
            Ok(HttpRequest::Insert { sql })
        }
        RequestKind::TableInsert(table) => {
            let body = read_table_insert_body(
                input,
                request.content_length,
                request.table_insert_format,
                limits,
            )?;
            Ok(HttpRequest::TableInsert {
                table,
                body,
                input_format: request.table_insert_format,
            })
        }
        RequestKind::Query(QuerySource::UrlEncodedParameters {
            encoded_parameters,
            method,
        }) => {
            let decoded =
                decode_query_parameters(&encoded_parameters, method, limits.max_sql_bytes)?;
            let output_format_selected =
                request.response_format.is_some() || decoded.response_format.is_some();
            let response_format = match (request.response_format, decoded.response_format) {
                (Some(_), Some(_)) => {
                    return Err(RequestFailure::new(
                        Status::BAD_REQUEST,
                        "default_format parameter cannot be combined with X-ClickHouse-Format header",
                    )
                    .into());
                }
                (Some(format), None) | (None, Some(format)) => Some(format),
                (None, None) => None,
            };
            let sql = String::from_utf8(decoded.sql).map_err(|_| {
                RequestReadError::from(RequestFailure::new(
                    Status::BAD_REQUEST,
                    "SQL query is not valid UTF-8",
                ))
            })?;

            if matches!(method, ParameterizedQueryMethod::Post) && access.allows_insert() {
                if let Some((table, input_format)) = parse_parameterized_table_insert(&sql) {
                    if output_format_selected {
                        return Err(RequestFailure::new(
                            Status::BAD_REQUEST,
                            input_format.output_selector_rejection_message(),
                        )
                        .into());
                    }
                    let body = read_table_insert_body(
                        input,
                        request.content_length,
                        input_format,
                        limits,
                    )?;
                    return Ok(HttpRequest::TableInsert {
                        table,
                        body,
                        input_format,
                    });
                }
            }

            if request.content_length.unwrap_or(0) != 0 {
                return Err(RequestFailure::new(
                    Status::BAD_REQUEST,
                    method.body_rejection_message(),
                )
                .into());
            }

            classify_standard_query_request(
                sql,
                response_format,
                decoded.max_result_bytes,
                decoded.max_result_rows,
                access.allows_insert()
                    && matches!(method, ParameterizedQueryMethod::Post)
                    && !output_format_selected,
            )
        }
    }
}

fn parse_parameterized_table_insert(sql: &str) -> Option<(String, TableInsertFormat)> {
    let sql = sql.trim();
    let sql = sql.strip_suffix(';').unwrap_or(sql).trim_end();
    let mut tokens = sql.split_whitespace();
    let insert = tokens.next()?;
    let into = tokens.next()?;
    let table = tokens.next()?;
    let format = tokens.next()?;
    let input_format = tokens.next()?;
    if tokens.next().is_some()
        || !insert.eq_ignore_ascii_case("INSERT")
        || !into.eq_ignore_ascii_case("INTO")
        || !format.eq_ignore_ascii_case("FORMAT")
        || validate_table_name(table).is_err()
    {
        return None;
    }
    let input_format = if input_format.eq_ignore_ascii_case("CSV") {
        TableInsertFormat::Csv
    } else if input_format.eq_ignore_ascii_case("CSVWithNames") {
        TableInsertFormat::CsvWithNames
    } else if input_format.eq_ignore_ascii_case("TabSeparated") {
        TableInsertFormat::TabSeparated
    } else {
        return None;
    };
    Some((table.to_owned(), input_format))
}

fn classify_standard_query_request(
    mut sql: String,
    response_format: Option<QueryResponseFormat>,
    max_result_bytes: Option<usize>,
    max_result_rows: Option<usize>,
    insert_enabled: bool,
) -> Result<HttpRequest, RequestReadError> {
    let sql_format = take_terminal_query_format(&mut sql);
    if let (Some(sql_format), Some(_)) = (sql_format, response_format) {
        let message = match sql_format {
            QueryResponseFormat::CsvWithNames => {
                "FORMAT CSVWithNames clause cannot be combined with X-ClickHouse-Format header or default_format parameter"
            }
            QueryResponseFormat::JsonEachRow => {
                "FORMAT JSONEachRow clause cannot be combined with X-ClickHouse-Format header or default_format parameter"
            }
            QueryResponseFormat::TabSeparated => {
                "FORMAT TabSeparated clause cannot be combined with X-ClickHouse-Format header or default_format parameter"
            }
            _ => unreachable!("the SQL FORMAT scanner only returns supported terminal formats"),
        };
        return Err(RequestFailure::new(Status::BAD_REQUEST, message).into());
    };
    let response_format = sql_format
        .or(response_format)
        .unwrap_or(QueryResponseFormat::Json);

    // Route mixed batches through the insert-only executor too, so its
    // authoritative all-statement validation and transaction semantics decide
    // the error without risking an earlier partial INSERT.
    let contains_insert = insert_enabled
        && sql_format.is_none()
        && sql::parse(&sql).is_ok_and(|statements| {
            statements.iter().any(|statement| {
                matches!(
                    statement,
                    Statement::Insert { .. } | Statement::InsertWithColumns { .. }
                )
            })
        });
    if contains_insert {
        Ok(HttpRequest::Insert { sql })
    } else {
        Ok(HttpRequest::Query {
            sql,
            response_format,
            max_result_bytes,
            max_result_rows,
        })
    }
}

/// Removes one supported terminal, unquoted SQL `FORMAT` clause.
///
/// The SQL parser deliberately does not own transport output formats, so the
/// HTTP adapter recognizes these ClickHouse-compatible clauses before using
/// the unchanged read-only query API. This scanner follows the SQL lexer's
/// single-quote, doubled-quote, whitespace, and line-comment rules without
/// retaining tokens proportional to the bounded request size.
fn take_terminal_query_format(sql: &mut String) -> Option<QueryResponseFormat> {
    let (format_start, response_format) = terminal_query_format(sql)?;
    sql.truncate(format_start);
    Some(response_format)
}

fn terminal_query_format(sql: &str) -> Option<(usize, QueryResponseFormat)> {
    #[derive(Clone, Copy)]
    struct Token<'a> {
        text: &'a str,
        start: usize,
        has_previous: bool,
        separated_by_semicolon: bool,
    }

    fn record_token<'a>(
        token: Token<'a>,
        previous: &mut Option<Token<'a>>,
        last: &mut Option<Token<'a>>,
    ) {
        *previous = *last;
        *last = Some(token);
    }

    let mut index = 0_usize;
    let mut previous = None;
    let mut last = None;
    let mut semicolons_since_token = 0_usize;

    while index < sql.len() {
        let rest = &sql[index..];
        let character = rest
            .chars()
            .next()
            .expect("the byte index remains on a character boundary");

        if character.is_whitespace() {
            index += character.len_utf8();
            continue;
        }
        if rest.starts_with("--") {
            while index < sql.len() {
                let character = sql[index..]
                    .chars()
                    .next()
                    .expect("the byte index remains on a character boundary");
                index += character.len_utf8();
                if character == '\n' {
                    break;
                }
            }
            continue;
        }
        if character == ';' {
            semicolons_since_token = semicolons_since_token.saturating_add(1);
            index += 1;
            continue;
        }

        let start = index;
        if character == '\'' {
            index += 1;
            let mut terminated = false;
            while index < sql.len() {
                let character = sql[index..]
                    .chars()
                    .next()
                    .expect("the byte index remains on a character boundary");
                index += character.len_utf8();
                if character == '\'' {
                    if sql[index..].starts_with('\'') {
                        index += 1;
                    } else {
                        terminated = true;
                        break;
                    }
                }
            }
            if !terminated {
                return None;
            }
        } else if character.is_ascii_alphabetic() || character == '_' {
            index += 1;
            while index < sql.len() {
                let character = sql[index..]
                    .chars()
                    .next()
                    .expect("the byte index remains on a character boundary");
                if !character.is_ascii_alphanumeric() && character != '_' {
                    break;
                }
                index += character.len_utf8();
            }
        } else {
            index += character.len_utf8();
        }

        let token = Token {
            text: &sql[start..index],
            start,
            has_previous: last.is_some(),
            separated_by_semicolon: semicolons_since_token != 0,
        };
        record_token(token, &mut previous, &mut last);
        semicolons_since_token = 0;
    }

    let format = previous?;
    let format_name = last?;
    if !format.has_previous
        || format.separated_by_semicolon
        || format_name.separated_by_semicolon
        || semicolons_since_token > 1
        || !format.text.eq_ignore_ascii_case("FORMAT")
    {
        return None;
    }
    let response_format = if format_name.text.eq_ignore_ascii_case("CSVWithNames") {
        QueryResponseFormat::CsvWithNames
    } else if format_name.text.eq_ignore_ascii_case("TabSeparated") {
        QueryResponseFormat::TabSeparated
    } else if format_name.text.eq_ignore_ascii_case("JSONEachRow") {
        QueryResponseFormat::JsonEachRow
    } else {
        return None;
    };
    Some((format.start, response_format))
}

fn read_sql_body(
    input: &mut impl Read,
    content_length: Option<usize>,
    max_sql_bytes: usize,
) -> Result<String, RequestReadError> {
    let body = read_bounded_body(input, content_length, max_sql_bytes)?;
    String::from_utf8(body)
        .map_err(|_| RequestFailure::new(Status::BAD_REQUEST, "SQL body is not valid UTF-8").into())
}

fn read_bounded_body(
    input: &mut impl Read,
    content_length: Option<usize>,
    max_body_bytes: usize,
) -> Result<Vec<u8>, RequestReadError> {
    let content_length = bounded_body_length(content_length, max_body_bytes)?;
    read_body_with_length(input, content_length)
}

fn read_table_insert_body(
    input: &mut impl Read,
    content_length: Option<usize>,
    input_format: TableInsertFormat,
    limits: HttpQueryLimits,
) -> Result<Vec<u8>, RequestReadError> {
    match input_format {
        TableInsertFormat::Csv | TableInsertFormat::CsvWithNames => read_csv_body(
            input,
            content_length,
            limits.max_sql_bytes,
            limits.csv_ingest_limits,
        ),
        TableInsertFormat::TabSeparated | TableInsertFormat::TabSeparatedWithNames => {
            read_tsv_body(
                input,
                content_length,
                limits.max_sql_bytes,
                limits.tsv_ingest_limits,
            )
        }
    }
}

fn read_csv_body(
    input: &mut impl Read,
    content_length: Option<usize>,
    max_http_body_bytes: usize,
    csv_limits: CsvIngestLimits,
) -> Result<Vec<u8>, RequestReadError> {
    let content_length = bounded_body_length(content_length, max_http_body_bytes)?;
    if content_length > csv_limits.max_bytes {
        return Err(RequestFailure::owned(
            Status::BAD_REQUEST,
            SharedDatabaseError::CsvIngest(CsvIngestError::ByteLimitExceeded {
                bytes: content_length,
                max_bytes: csv_limits.max_bytes,
            })
            .to_string(),
        )
        .into());
    }
    read_body_with_length(input, content_length)
}

fn read_tsv_body(
    input: &mut impl Read,
    content_length: Option<usize>,
    max_http_body_bytes: usize,
    tsv_limits: TsvIngestLimits,
) -> Result<Vec<u8>, RequestReadError> {
    let content_length = bounded_body_length(content_length, max_http_body_bytes)?;
    if content_length > tsv_limits.max_bytes {
        return Err(RequestFailure::owned(
            Status::BAD_REQUEST,
            SharedDatabaseError::TsvIngest(TsvIngestError::ByteLimitExceeded {
                bytes: content_length,
                max_bytes: tsv_limits.max_bytes,
            })
            .to_string(),
        )
        .into());
    }
    read_body_with_length(input, content_length)
}

fn bounded_body_length(
    content_length: Option<usize>,
    max_body_bytes: usize,
) -> Result<usize, RequestReadError> {
    let Some(content_length) = content_length else {
        return Err(RequestFailure::new(
            Status::LENGTH_REQUIRED,
            "Content-Length header is required",
        )
        .into());
    };
    if content_length > max_body_bytes {
        return Err(RequestFailure::new(
            Status::PAYLOAD_TOO_LARGE,
            "request body exceeds configured byte limit",
        )
        .into());
    }
    Ok(content_length)
}

fn read_body_with_length(
    input: &mut impl Read,
    content_length: usize,
) -> Result<Vec<u8>, RequestReadError> {
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

    Ok(body)
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
    response_format: Option<QueryResponseFormat>,
    table_insert_format: TableInsertFormat,
}

enum HttpRequest {
    Query {
        sql: String,
        response_format: QueryResponseFormat,
        max_result_bytes: Option<usize>,
        max_result_rows: Option<usize>,
    },
    Insert {
        sql: String,
    },
    TableInsert {
        table: String,
        body: Vec<u8>,
        input_format: TableInsertFormat,
    },
    Ping,
    Ready,
    Metrics,
}

#[derive(Clone, Copy)]
enum QueryResponseFormat {
    Json,
    Csv,
    CsvWithNames,
    TabSeparated,
    TabSeparatedWithNames,
    JsonEachRow,
    JsonCompactEachRow,
}

enum RequestKind {
    Query(QuerySource),
    Insert,
    TableInsert(String),
    Ping,
    Ready,
    Metrics,
}

#[derive(Clone, Copy)]
enum TableInsertFormat {
    Csv,
    CsvWithNames,
    TabSeparated,
    TabSeparatedWithNames,
}

impl TableInsertFormat {
    const fn output_selector_rejection_message(self) -> &'static str {
        match self {
            Self::Csv => "CSV INSERT does not accept an output format selector",
            Self::CsvWithNames => "CSVWithNames INSERT does not accept an output format selector",
            Self::TabSeparated => "TabSeparated INSERT does not accept an output format selector",
            Self::TabSeparatedWithNames => {
                "parameterized INSERT does not accept an output format selector"
            }
        }
    }
}

enum QuerySource {
    Body,
    UrlEncodedParameters {
        encoded_parameters: Vec<u8>,
        method: ParameterizedQueryMethod,
    },
}

#[derive(Clone, Copy)]
enum ParameterizedQueryMethod {
    Get,
    Post,
}

impl ParameterizedQueryMethod {
    const fn body_rejection_message(self) -> &'static str {
        match self {
            Self::Get => "GET /?query= does not accept a request body",
            Self::Post => "POST /?query= does not accept a request body",
        }
    }

    const fn empty_parameter_message(self) -> &'static str {
        match self {
            Self::Get => "GET query parameters must have nonempty names and values",
            Self::Post => "POST query parameters must have nonempty names and values",
        }
    }

    const fn unknown_parameter_message(self) -> &'static str {
        match self {
            Self::Get => "GET query target contains an unknown parameter",
            Self::Post => "POST query target contains an unknown parameter",
        }
    }

    const fn missing_query_message(self) -> &'static str {
        match self {
            Self::Get => "GET query target must contain exactly one query parameter",
            Self::Post => "POST query target must contain exactly one query parameter",
        }
    }
}

struct DecodedQuery {
    sql: Vec<u8>,
    response_format: Option<QueryResponseFormat>,
    max_result_bytes: Option<usize>,
    max_result_rows: Option<usize>,
}

fn parse_headers(
    header: &[u8],
    max_header_count: usize,
    authentication: Authentication<'_>,
    access: HttpAccess,
) -> Result<ParsedRequest, RequestReadError> {
    let Some(without_terminator) = header.strip_suffix(b"\r\n\r\n") else {
        return Err(RequestFailure::new(Status::BAD_REQUEST, "malformed HTTP headers").into());
    };
    let mut lines = without_terminator.split(|byte| *byte == b'\n').peekable();
    let Some(raw_request_line) = lines.next() else {
        return Err(RequestFailure::new(Status::BAD_REQUEST, "missing request line").into());
    };
    let request_line = strict_header_line(raw_request_line, lines.peek().is_some())?;
    // Authenticated read-only handlers still recognize explicit insertion
    // targets long enough to authenticate them. This preserves credential
    // precedence without exposing the routes or consuming their bodies.
    let kind = parse_request_line(request_line, authentication.is_configured())?;

    let mut content_length = None;
    let mut host_seen = false;
    let mut header_count = 0_usize;
    let mut transfer_encoding_seen = false;
    let mut expect_seen = false;
    let mut authorization = None;
    let mut duplicate_authorization = false;
    let mut clickhouse_key = None;
    let mut duplicate_clickhouse_key = false;
    let mut clickhouse_format = None;
    let mut duplicate_clickhouse_format = false;
    let mut clickhouse_database = None;
    let mut duplicate_clickhouse_database = false;
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
        } else if matches!(authentication, Authentication::Bearer(_))
            && name.eq_ignore_ascii_case(b"authorization")
            && authorization.replace(value).is_some()
        {
            duplicate_authorization = true;
        } else if matches!(authentication, Authentication::ClickHouseKey(_))
            && name.eq_ignore_ascii_case(b"x-clickhouse-key")
            && clickhouse_key.replace(value).is_some()
        {
            duplicate_clickhouse_key = true;
        } else if name.eq_ignore_ascii_case(b"x-clickhouse-format")
            && clickhouse_format.replace(value).is_some()
        {
            duplicate_clickhouse_format = true;
        } else if name.eq_ignore_ascii_case(b"x-clickhouse-database")
            && clickhouse_database.replace(value).is_some()
        {
            duplicate_clickhouse_database = true;
        }
    }

    match authentication {
        Authentication::None => {}
        Authentication::Bearer(expected_bearer_token) => {
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
        Authentication::ClickHouseKey(expected_clickhouse_key) => {
            let key_matches = clickhouse_key.is_some_and(|provided| {
                !provided.is_empty() && constant_work_eq(provided, expected_clickhouse_key)
            });
            if duplicate_clickhouse_key || !key_matches {
                return Err(RequestFailure::with_headers(
                    Status::UNAUTHORIZED,
                    "X-ClickHouse-Key authentication required",
                    &[b"WWW-Authenticate: X-ClickHouse-Key\r\n"],
                )
                .into());
            }
        }
    }

    if !access.allows_insert() && matches!(&kind, RequestKind::Insert | RequestKind::TableInsert(_))
    {
        return Err(
            RequestFailure::new(Status::NOT_FOUND, "request target must be / or /query").into(),
        );
    }

    if matches!(
        &kind,
        RequestKind::Query(_) | RequestKind::Insert | RequestKind::TableInsert(_)
    ) {
        if duplicate_clickhouse_database {
            return Err(RequestFailure::new(
                Status::BAD_REQUEST,
                "duplicate X-ClickHouse-Database header",
            )
            .into());
        }
        if clickhouse_database.is_some_and(|database| database != b"default") {
            return Err(RequestFailure::new(
                Status::BAD_REQUEST,
                "X-ClickHouse-Database header must be default",
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
            None => None,
            Some(b"CSV") => Some(QueryResponseFormat::Csv),
            Some(b"CSVWithNames") => Some(QueryResponseFormat::CsvWithNames),
            Some(b"TabSeparated") => Some(QueryResponseFormat::TabSeparated),
            Some(b"TabSeparatedWithNames") => Some(QueryResponseFormat::TabSeparatedWithNames),
            Some(b"JSONEachRow") => Some(QueryResponseFormat::JsonEachRow),
            Some(b"JSONCompactEachRow") => Some(QueryResponseFormat::JsonCompactEachRow),
            Some(_) => {
                return Err(RequestFailure::new(
                    Status::BAD_REQUEST,
                    "unsupported X-ClickHouse-Format header",
                )
                .into());
            }
        }
    } else {
        None
    };
    let table_insert_format = if matches!(&kind, RequestKind::TableInsert(_)) {
        if duplicate_clickhouse_format {
            return Err(RequestFailure::new(
                Status::BAD_REQUEST,
                "duplicate X-ClickHouse-Format header",
            )
            .into());
        }
        match clickhouse_format {
            None | Some(b"CSVWithNames") => TableInsertFormat::CsvWithNames,
            Some(b"CSV") => TableInsertFormat::Csv,
            Some(b"TabSeparated") => TableInsertFormat::TabSeparated,
            Some(b"TabSeparatedWithNames") => TableInsertFormat::TabSeparatedWithNames,
            Some(_) => {
                return Err(RequestFailure::new(
                    Status::BAD_REQUEST,
                    "unsupported X-ClickHouse-Format header",
                )
                .into());
            }
        }
    } else {
        TableInsertFormat::CsvWithNames
    };
    Ok(ParsedRequest {
        kind,
        content_length,
        response_format,
        table_insert_format,
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

fn is_valid_clickhouse_key(key: &[u8]) -> bool {
    !key.is_empty()
        && trim_optional_whitespace(key) == key
        && key.iter().copied().all(is_header_value_byte)
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
    const QUERY_PARAMETERS_PREFIX: &[u8] = b"/?";

    match (method, target) {
        (b"POST", b"/" | b"/query") => Ok(RequestKind::Query(QuerySource::Body)),
        (b"POST", b"/insert") if authenticated_insert_enabled => Ok(RequestKind::Insert),
        (b"POST", target) if authenticated_insert_enabled && target.starts_with(b"/insert/") => {
            parse_table_insert_target(target)
                .map(|table| RequestKind::TableInsert(table.to_owned()))
                .ok_or_else(|| {
                    RequestReadError::from(RequestFailure::new(
                        Status::NOT_FOUND,
                        "request target must be / or /query",
                    ))
                })
        }
        (b"GET", target) if target.starts_with(QUERY_PARAMETERS_PREFIX) => {
            Ok(RequestKind::Query(QuerySource::UrlEncodedParameters {
                encoded_parameters: target[QUERY_PARAMETERS_PREFIX.len()..].to_vec(),
                method: ParameterizedQueryMethod::Get,
            }))
        }
        (b"POST", target) if target.starts_with(QUERY_PARAMETERS_PREFIX) => {
            Ok(RequestKind::Query(QuerySource::UrlEncodedParameters {
                encoded_parameters: target[QUERY_PARAMETERS_PREFIX.len()..].to_vec(),
                method: ParameterizedQueryMethod::Post,
            }))
        }
        (b"GET", b"/ping") => Ok(RequestKind::Ping),
        (b"GET", b"/ready") => Ok(RequestKind::Ready),
        (b"GET", b"/metrics") => Ok(RequestKind::Metrics),
        (_, b"/" | b"/query") => Err(RequestFailure::with_headers(
            Status::METHOD_NOT_ALLOWED,
            "method must be POST",
            &[b"Allow: POST\r\n"],
        )
        .into()),
        (_, target)
            if target.starts_with(b"/?query=")
                || target.starts_with(b"/?database=")
                || target.starts_with(b"/?max_result_bytes=")
                || target.starts_with(b"/?max_result_rows=")
                || target.starts_with(b"/?default_format=") =>
        {
            Err(RequestFailure::with_headers(
                Status::METHOD_NOT_ALLOWED,
                "method must be GET or POST for /?query=",
                &[b"Allow: GET, POST\r\n"],
            )
            .into())
        }
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
        (_, target)
            if authenticated_insert_enabled && parse_table_insert_target(target).is_some() =>
        {
            Err(RequestFailure::with_headers(
                Status::METHOD_NOT_ALLOWED,
                "method must be POST for /insert/<table>",
                &[b"Allow: POST\r\n"],
            )
            .into())
        }
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

fn parse_table_insert_target(target: &[u8]) -> Option<&str> {
    let table = target.strip_prefix(b"/insert/")?;
    let table = std::str::from_utf8(table).ok()?;
    validate_table_name(table).is_ok().then_some(table)
}

fn decode_query_parameters(
    encoded_parameters: &[u8],
    method: ParameterizedQueryMethod,
    max_sql_bytes: usize,
) -> Result<DecodedQuery, RequestReadError> {
    let mut query = None;
    let mut database_seen = false;
    let mut response_format = None;
    let mut max_result_bytes = None;
    let mut max_result_rows = None;

    for encoded_parameter in encoded_parameters.split(|byte| *byte == b'&') {
        let Some(equals) = encoded_parameter.iter().position(|byte| *byte == b'=') else {
            return Err(
                RequestFailure::new(Status::BAD_REQUEST, method.empty_parameter_message()).into(),
            );
        };
        let encoded_name = &encoded_parameter[..equals];
        let encoded_value = &encoded_parameter[equals + 1..];
        if encoded_name.is_empty() || encoded_value.is_empty() {
            return Err(
                RequestFailure::new(Status::BAD_REQUEST, method.empty_parameter_message()).into(),
            );
        }

        let name = decode_form_component(encoded_name, None)?;
        match name.as_slice() {
            b"query" => {
                if query.is_some() {
                    return Err(RequestFailure::new(
                        Status::BAD_REQUEST,
                        "duplicate query parameter",
                    )
                    .into());
                }
                query = Some(decode_form_component(encoded_value, Some(max_sql_bytes))?);
            }
            b"database" => {
                if database_seen {
                    return Err(RequestFailure::new(
                        Status::BAD_REQUEST,
                        "duplicate database parameter",
                    )
                    .into());
                }
                database_seen = true;
                if decode_form_component(encoded_value, None)? != b"default" {
                    return Err(RequestFailure::new(
                        Status::BAD_REQUEST,
                        "database query parameter must be default",
                    )
                    .into());
                }
            }
            b"default_format" => {
                if response_format.is_some() {
                    return Err(RequestFailure::new(
                        Status::BAD_REQUEST,
                        "duplicate default_format parameter",
                    )
                    .into());
                }
                let value = decode_form_component(encoded_value, None)?;
                response_format = Some(match value.as_slice() {
                    b"JSON" => QueryResponseFormat::Json,
                    b"CSV" => QueryResponseFormat::Csv,
                    b"CSVWithNames" => QueryResponseFormat::CsvWithNames,
                    b"TabSeparated" => QueryResponseFormat::TabSeparated,
                    b"TabSeparatedWithNames" => QueryResponseFormat::TabSeparatedWithNames,
                    b"JSONEachRow" => QueryResponseFormat::JsonEachRow,
                    b"JSONCompactEachRow" => QueryResponseFormat::JsonCompactEachRow,
                    _ => {
                        return Err(RequestFailure::new(
                            Status::BAD_REQUEST,
                            "unsupported default_format parameter",
                        )
                        .into());
                    }
                });
            }
            b"max_result_rows" => {
                if max_result_rows.is_some() {
                    return Err(RequestFailure::new(
                        Status::BAD_REQUEST,
                        "duplicate max_result_rows parameter",
                    )
                    .into());
                }
                let value = decode_form_component(encoded_value, None)?;
                max_result_rows = Some(parse_decimal_max_result_rows(&value)?);
            }
            b"max_result_bytes" => {
                if max_result_bytes.is_some() {
                    return Err(RequestFailure::new(
                        Status::BAD_REQUEST,
                        "duplicate max_result_bytes parameter",
                    )
                    .into());
                }
                let value = decode_form_component(encoded_value, None)?;
                max_result_bytes = Some(parse_decimal_max_result_bytes(&value)?);
            }
            _ => {
                return Err(RequestFailure::new(
                    Status::BAD_REQUEST,
                    method.unknown_parameter_message(),
                )
                .into());
            }
        }
    }

    let sql = query.ok_or_else(|| {
        RequestReadError::from(RequestFailure::new(
            Status::BAD_REQUEST,
            method.missing_query_message(),
        ))
    })?;
    Ok(DecodedQuery {
        sql,
        response_format,
        max_result_bytes,
        max_result_rows,
    })
}

fn parse_decimal_max_result_bytes(value: &[u8]) -> Result<usize, RequestReadError> {
    parse_decimal_result_limit(
        value,
        "max_result_bytes parameter must be a decimal integer",
        "max_result_bytes parameter is out of range",
    )
}

fn parse_decimal_max_result_rows(value: &[u8]) -> Result<usize, RequestReadError> {
    parse_decimal_result_limit(
        value,
        "max_result_rows parameter must be a decimal integer",
        "max_result_rows parameter is out of range",
    )
}

fn parse_decimal_result_limit(
    value: &[u8],
    malformed_message: &'static str,
    overflow_message: &'static str,
) -> Result<usize, RequestReadError> {
    let mut parsed = 0_usize;
    for byte in value {
        if !byte.is_ascii_digit() {
            return Err(RequestFailure::new(Status::BAD_REQUEST, malformed_message).into());
        }
        parsed = parsed
            .checked_mul(10)
            .and_then(|parsed| parsed.checked_add(usize::from(byte - b'0')))
            .ok_or_else(|| {
                RequestReadError::from(RequestFailure::new(Status::BAD_REQUEST, overflow_message))
            })?;
    }
    Ok(parsed)
}

fn decode_form_component(
    encoded: &[u8],
    max_decoded_bytes: Option<usize>,
) -> Result<Vec<u8>, RequestReadError> {
    let capacity = max_decoded_bytes.map_or(encoded.len(), |limit| encoded.len().min(limit));
    let mut decoded = Vec::with_capacity(capacity);
    let mut index = 0_usize;
    while index < encoded.len() {
        let byte = match encoded[index] {
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

        if max_decoded_bytes.is_some_and(|limit| decoded.len() == limit) {
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
    message: Cow<'static, str>,
    extra_headers: &'static [&'static [u8]],
}

impl RequestFailure {
    const fn new(status: Status, message: &'static str) -> Self {
        Self {
            status,
            message: Cow::Borrowed(message),
            extra_headers: &[],
        }
    }

    fn owned(status: Status, message: String) -> Self {
        Self {
            status,
            message: Cow::Owned(message),
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
            message: Cow::Borrowed(message),
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
const CLICKHOUSE_KEY_RESPONSE_HEADERS: &[&[u8]] = &[b"Cache-Control: private, no-store\r\n"];
const CONTENT_TYPE_CSV: &[u8] = b"text/csv; charset=utf-8";
const CONTENT_TYPE_JSON: &[u8] = b"application/json";
const CONTENT_TYPE_TEXT: &[u8] = b"text/plain; charset=utf-8";
const CONTENT_TYPE_TSV: &[u8] = b"text/tab-separated-values; charset=utf-8";
const CONTENT_TYPE_PROMETHEUS: &[u8] = b"text/plain; version=0.0.4; charset=utf-8";
const TABLES_METRIC_PREFIX: &str = concat!(
    "# HELP rusthouse_tables Number of tables retained by the database.\n",
    "# TYPE rusthouse_tables gauge\n",
    "rusthouse_tables ",
);
const COLUMNS_METRIC_PREFIX: &str = concat!(
    "# HELP rusthouse_columns Number of columns retained by the database.\n",
    "# TYPE rusthouse_columns gauge\n",
    "rusthouse_columns ",
);
const RETAINED_ROWS_METRIC_PREFIX: &str = concat!(
    "# HELP rusthouse_retained_rows Number of rows retained across all tables.\n",
    "# TYPE rusthouse_retained_rows gauge\n",
    "rusthouse_retained_rows ",
);
const RETAINED_VALUE_BYTES_METRIC_PREFIX: &str = concat!(
    "# HELP rusthouse_retained_value_bytes Scalar payload bytes retained across all tables.\n",
    "# TYPE rusthouse_retained_value_bytes gauge\n",
    "rusthouse_retained_value_bytes ",
);
const TABLE_ROWS_METRIC_HEADER: &str = concat!(
    "# HELP rusthouse_table_rows Number of rows retained by a table.\n",
    "# TYPE rusthouse_table_rows gauge\n",
);
const TABLE_ROW_METRIC_PREFIX: &str = "rusthouse_table_rows{table=\"";
const TABLE_ROW_METRIC_SEPARATOR: &str = "\"} ";
const TABLE_RETAINED_VALUE_BYTES_METRIC_HEADER: &str = concat!(
    "# HELP rusthouse_table_retained_value_bytes Scalar payload bytes retained by a table.\n",
    "# TYPE rusthouse_table_retained_value_bytes gauge\n",
);
const TABLE_RETAINED_VALUE_BYTES_METRIC_PREFIX: &str =
    "rusthouse_table_retained_value_bytes{table=\"";

fn prometheus_metrics_body_len(
    totals: crate::DatabaseMetrics,
    table_name_bytes: usize,
    row_count_bytes: usize,
    retained_value_byte_count_bytes: usize,
) -> usize {
    let fixed_bytes = TABLES_METRIC_PREFIX
        .len()
        .saturating_add(COLUMNS_METRIC_PREFIX.len())
        .saturating_add(RETAINED_ROWS_METRIC_PREFIX.len())
        .saturating_add(RETAINED_VALUE_BYTES_METRIC_PREFIX.len())
        .saturating_add(TABLE_ROWS_METRIC_HEADER.len())
        .saturating_add(TABLE_RETAINED_VALUE_BYTES_METRIC_HEADER.len())
        .saturating_add(4)
        .saturating_add(usize_decimal_len(totals.table_count))
        .saturating_add(usize_decimal_len(totals.column_count))
        .saturating_add(usize_decimal_len(totals.retained_row_count))
        .saturating_add(usize_decimal_len(totals.retained_value_bytes));
    let per_table_fixed_bytes = TABLE_ROW_METRIC_PREFIX
        .len()
        .saturating_add(TABLE_ROW_METRIC_SEPARATOR.len())
        .saturating_add(1)
        .saturating_add(TABLE_RETAINED_VALUE_BYTES_METRIC_PREFIX.len())
        .saturating_add(TABLE_ROW_METRIC_SEPARATOR.len())
        .saturating_add(1);
    fixed_bytes
        .saturating_add(totals.table_count.saturating_mul(per_table_fixed_bytes))
        .saturating_add(table_name_bytes.saturating_mul(2))
        .saturating_add(row_count_bytes)
        .saturating_add(retained_value_byte_count_bytes)
}

fn write_prometheus_metrics(
    output: &mut impl Write,
    metrics: DatabaseMetricsWithTables,
) -> io::Result<()> {
    let DatabaseMetricsWithTables { totals, tables } = metrics;
    output.write_all(TABLES_METRIC_PREFIX.as_bytes())?;
    writeln!(output, "{}", totals.table_count)?;
    output.write_all(COLUMNS_METRIC_PREFIX.as_bytes())?;
    writeln!(output, "{}", totals.column_count)?;
    output.write_all(RETAINED_ROWS_METRIC_PREFIX.as_bytes())?;
    writeln!(output, "{}", totals.retained_row_count)?;
    output.write_all(RETAINED_VALUE_BYTES_METRIC_PREFIX.as_bytes())?;
    writeln!(output, "{}", totals.retained_value_bytes)?;
    output.write_all(TABLE_ROWS_METRIC_HEADER.as_bytes())?;
    for (table, row_count, _) in &tables {
        output.write_all(TABLE_ROW_METRIC_PREFIX.as_bytes())?;
        output.write_all(table.as_bytes())?;
        output.write_all(TABLE_ROW_METRIC_SEPARATOR.as_bytes())?;
        writeln!(output, "{row_count}")?;
    }
    output.write_all(TABLE_RETAINED_VALUE_BYTES_METRIC_HEADER.as_bytes())?;
    for (table, _, retained_value_bytes) in tables {
        output.write_all(TABLE_RETAINED_VALUE_BYTES_METRIC_PREFIX.as_bytes())?;
        output.write_all(table.as_bytes())?;
        output.write_all(TABLE_ROW_METRIC_SEPARATOR.as_bytes())?;
        writeln!(output, "{retained_value_bytes}")?;
    }
    Ok(())
}

fn write_error_response(
    output: &mut impl Write,
    status: Status,
    extra_headers: &[&[u8]],
    response_headers: &[&[u8]],
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
        response_headers,
        CONTENT_TYPE_JSON,
        body,
        max_response_bytes,
    )
}

fn write_response(
    output: &mut impl Write,
    status: Status,
    extra_headers: &[&[u8]],
    response_headers: &[&[u8]],
    content_type: &[u8],
    body: Vec<u8>,
    max_response_bytes: usize,
) -> Result<(), HttpQueryError> {
    match prepare_response(
        status,
        extra_headers,
        response_headers,
        content_type,
        &body,
        max_response_bytes,
    ) {
        Ok(response) => output.write_all(&response).map_err(HttpQueryError::Write),
        Err(_) => write_response_limit_error(output, response_headers, max_response_bytes),
    }
}

fn write_response_limit_error(
    output: &mut impl Write,
    response_headers: &[&[u8]],
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
        response_headers,
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
    response_headers: &[&[u8]],
    content_type: &[u8],
    body: &[u8],
    max_response_bytes: usize,
) -> Result<Vec<u8>, usize> {
    let response_bytes = response_len(
        status,
        extra_headers,
        response_headers,
        content_type,
        body.len(),
    );
    if response_bytes > max_response_bytes {
        return Err(response_bytes);
    }

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
    for response_header in response_headers {
        header.extend_from_slice(response_header);
    }
    header.extend_from_slice(b"\r\n");

    debug_assert_eq!(header.len().saturating_add(body.len()), response_bytes);
    header.extend_from_slice(body);
    Ok(header)
}

fn response_len(
    status: Status,
    extra_headers: &[&[u8]],
    response_headers: &[&[u8]],
    content_type: &[u8],
    body_len: usize,
) -> usize {
    let extra_header_bytes = extra_headers
        .iter()
        .chain(response_headers)
        .map(|header| header.len())
        .fold(0_usize, usize::saturating_add);
    b"HTTP/1.1 "
        .len()
        .saturating_add(usize_decimal_len(usize::from(status.code)))
        .saturating_add(1)
        .saturating_add(status.reason.len())
        .saturating_add(b"\r\nContent-Type: ".len())
        .saturating_add(content_type.len())
        .saturating_add(b"\r\nContent-Length: ".len())
        .saturating_add(usize_decimal_len(body_len))
        .saturating_add(b"\r\nConnection: close\r\n".len())
        .saturating_add(extra_header_bytes)
        .saturating_add(b"\r\n".len())
        .saturating_add(body_len)
}

fn usize_decimal_len(mut value: usize) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
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
