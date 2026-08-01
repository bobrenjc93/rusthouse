//! Bounded, engine-independent HTTP query service.

use std::{
    io::{self, Write},
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use tokio::{net::TcpListener, sync::Semaphore, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::query::{
    QueryCancellation, QueryError, QueryErrorKind, QueryRequest, QueryResult, QueryService,
    QueryValue,
};

const FORCE_CANCELLATION_WAIT: Duration = Duration::from_millis(250);

/// Resource limits and deadlines enforced by the HTTP frontend.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Maximum decoded request body size.
    pub max_request_bytes: usize,
    /// Maximum encoded successful response body size.
    pub max_response_bytes: usize,
    /// Maximum number of queries executing or encoding results at once.
    pub max_concurrent_queries: usize,
    /// Maximum engine execution time for one query.
    pub query_timeout: Duration,
    /// Time allowed for active requests to finish during shutdown.
    pub shutdown_timeout: Duration,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            max_request_bytes: 1024 * 1024,
            max_response_bytes: 16 * 1024 * 1024,
            max_concurrent_queries: 16,
            query_timeout: Duration::from_secs(30),
            shutdown_timeout: Duration::from_secs(10),
        }
    }
}

impl ServerConfig {
    /// Checks that every limit and timeout is nonzero.
    pub fn validate(&self) -> Result<(), ServerError> {
        if self.max_request_bytes == 0 {
            return Err(ServerError::InvalidConfig(
                "max_request_bytes must be greater than zero".into(),
            ));
        }
        if self.max_response_bytes == 0 {
            return Err(ServerError::InvalidConfig(
                "max_response_bytes must be greater than zero".into(),
            ));
        }
        if self.max_concurrent_queries == 0 {
            return Err(ServerError::InvalidConfig(
                "max_concurrent_queries must be greater than zero".into(),
            ));
        }
        if self.query_timeout.is_zero() {
            return Err(ServerError::InvalidConfig(
                "query_timeout must be greater than zero".into(),
            ));
        }
        if self.shutdown_timeout.is_zero() {
            return Err(ServerError::InvalidConfig(
                "shutdown_timeout must be greater than zero".into(),
            ));
        }
        Ok(())
    }
}

/// An error produced while configuring or running the HTTP server.
#[derive(Debug)]
pub enum ServerError {
    /// A configured limit or timeout was invalid.
    InvalidConfig(String),
    /// Binding or serving a socket failed.
    Io(io::Error),
    /// The background server task panicked or was cancelled unexpectedly.
    Task(tokio::task::JoinError),
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidConfig(message) => write!(formatter, "invalid server config: {message}"),
            Self::Io(error) => write!(formatter, "HTTP server I/O error: {error}"),
            Self::Task(error) => write!(formatter, "HTTP server task failed: {error}"),
        }
    }
}

impl std::error::Error for ServerError {}

impl From<io::Error> for ServerError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

struct HttpState {
    service: Arc<dyn QueryService>,
    config: ServerConfig,
    permits: Arc<Semaphore>,
    request_ids: AtomicU64,
    force_cancellation: CancellationToken,
}

impl HttpState {
    fn next_request_id(&self) -> u64 {
        self.request_ids.fetch_add(1, Ordering::Relaxed)
    }
}

/// Handle for a running HTTP server.
///
/// Call [`ServerHandle::shutdown`] to stop accepting connections and give active
/// requests the configured grace period. Dropping the handle stops it immediately.
pub struct ServerHandle {
    local_addr: SocketAddr,
    graceful_shutdown: CancellationToken,
    force_cancellation: CancellationToken,
    shutdown_timeout: Duration,
    task: Option<JoinHandle<io::Result<()>>>,
}

impl ServerHandle {
    /// Returns the bound address, including the selected port when port zero was used.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Stops accepting connections, waits for the grace period, then cancels work.
    pub async fn shutdown(mut self) -> Result<(), ServerError> {
        self.graceful_shutdown.cancel();
        let mut task = self.task.take().expect("server task is present");

        match tokio::time::timeout(self.shutdown_timeout, &mut task).await {
            Ok(result) => result.map_err(ServerError::Task)?.map_err(ServerError::Io),
            Err(_) => {
                self.force_cancellation.cancel();
                match tokio::time::timeout(FORCE_CANCELLATION_WAIT, &mut task).await {
                    Ok(result) => result.map_err(ServerError::Task)?.map_err(ServerError::Io),
                    Err(_) => {
                        task.abort();
                        let _ = task.await;
                        Ok(())
                    }
                }
            }
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.graceful_shutdown.cancel();
        self.force_cancellation.cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

/// Binds and starts an HTTP server on `address`.
///
/// Port zero can be used in tests; the selected address is available from the
/// returned handle. Hyper's HTTP/1.1 connection handling keeps connections alive
/// by default.
pub async fn spawn_http_server(
    address: SocketAddr,
    service: Arc<dyn QueryService>,
    config: ServerConfig,
) -> Result<ServerHandle, ServerError> {
    config.validate()?;
    let listener = TcpListener::bind(address).await?;
    spawn_on_listener(listener, service, config)
}

/// Starts an HTTP server using an already-bound Tokio listener.
pub fn spawn_on_listener(
    listener: TcpListener,
    service: Arc<dyn QueryService>,
    config: ServerConfig,
) -> Result<ServerHandle, ServerError> {
    config.validate()?;
    let local_addr = listener.local_addr()?;
    let graceful_shutdown = CancellationToken::new();
    let force_cancellation = CancellationToken::new();
    let state = Arc::new(HttpState {
        service,
        permits: Arc::new(Semaphore::new(config.max_concurrent_queries)),
        config: config.clone(),
        request_ids: AtomicU64::new(1),
        force_cancellation: force_cancellation.clone(),
    });

    let app = Router::new()
        .route("/query", post(query))
        .route("/health", get(readiness))
        .route("/health/live", get(liveness))
        .route("/health/ready", get(readiness))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(state);

    let shutdown_signal = graceful_shutdown.clone();
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal.cancelled_owned())
            .await
    });

    Ok(ServerHandle {
        local_addr,
        graceful_shutdown,
        force_cancellation,
        shutdown_timeout: config.shutdown_timeout,
        task: Some(task),
    })
}

async fn liveness(State(state): State<Arc<HttpState>>) -> Response {
    let request_id = state.next_request_id();
    json_response(
        StatusCode::OK,
        request_id,
        &HealthResponse {
            status: "ok",
            message: None,
        },
    )
}

async fn readiness(State(state): State<Arc<HttpState>>) -> Response {
    let request_id = state.next_request_id();
    let health = state.service.health();
    let status = if health.is_ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    json_response(
        status,
        request_id,
        &HealthResponse {
            status: if health.is_ready { "ok" } else { "not_ready" },
            message: health.message,
        },
    )
}

async fn query(State(state): State<Arc<HttpState>>, request: Request) -> Response {
    let request_id = state.next_request_id();
    match handle_query(&state, request_id, request).await {
        Ok(response) => response,
        Err(error) => error.into_response(request_id),
    }
}

async fn handle_query(
    state: &Arc<HttpState>,
    request_id: u64,
    request: Request,
) -> Result<Response, ApiError> {
    let format = ResponseFormat::negotiate(request.uri().query(), request.headers())?;
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    if let Some(length) = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        && length > state.config.max_request_bytes
    {
        return Err(ApiError::request_too_large(state.config.max_request_bytes));
    }

    let body = to_bytes(request.into_body(), state.config.max_request_bytes)
        .await
        .map_err(|_| ApiError::request_too_large(state.config.max_request_bytes))?;
    let sql = parse_query_body(&body, content_type.as_deref())?;

    let permit = state
        .permits
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::overloaded(state.config.max_concurrent_queries))?;

    let token = state.force_cancellation.child_token();
    let cancellation = QueryCancellation::new(token.clone());
    let mut cancel_on_drop = CancelOnDrop(Some(cancellation.clone()));
    let query_request = QueryRequest {
        sql,
        request_id,
        cancellation,
    };
    let execution = state.service.execute(query_request);

    let result = tokio::select! {
        result = execution => result.map_err(ApiError::from_query_error)?,
        () = tokio::time::sleep(state.config.query_timeout) => {
            token.cancel();
            return Err(ApiError::timeout(state.config.query_timeout));
        }
        () = state.force_cancellation.cancelled() => {
            token.cancel();
            return Err(ApiError::shutting_down());
        }
    };
    cancel_on_drop.0 = None;

    result.validate().map_err(ApiError::from_query_error)?;
    let bytes = serialize_result(&result, format, state.config.max_response_bytes)?;
    drop(permit);
    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(format.content_type()),
    );
    insert_common_headers(&mut response, request_id);
    Ok(response)
}

struct CancelOnDrop(Option<QueryCancellation>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(cancellation) = &self.0 {
            cancellation.cancel();
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonQuery {
    query: String,
}

fn parse_query_body(body: &[u8], content_type: Option<&str>) -> Result<String, ApiError> {
    let media_type = content_type
        .unwrap_or("text/plain")
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let query = match media_type.as_str() {
        "text/plain" | "application/sql" => std::str::from_utf8(body)
            .map_err(|_| ApiError::bad_request("query body must be valid UTF-8"))?
            .to_owned(),
        "application/json" => {
            serde_json::from_slice::<JsonQuery>(body)
                .map_err(|error| {
                    ApiError::bad_request(format!("invalid JSON query body: {error}"))
                })?
                .query
        }
        _ => return Err(ApiError::unsupported_media_type(&media_type)),
    };

    if query.trim().is_empty() {
        return Err(ApiError::bad_request("query must not be empty"));
    }
    Ok(query)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponseFormat {
    Json,
    Ndjson,
    Csv,
}

impl ResponseFormat {
    fn negotiate(query: Option<&str>, headers: &HeaderMap) -> Result<Self, ApiError> {
        if let Some(format) = query.and_then(query_format) {
            return Self::from_name(&format).ok_or_else(|| ApiError::not_acceptable(&format));
        }

        let Some(accept) = headers.get(header::ACCEPT) else {
            return Ok(Self::Json);
        };
        let accept = accept
            .to_str()
            .map_err(|_| ApiError::not_acceptable("non-UTF-8 Accept header"))?;
        let mut best: Option<(u16, usize, Self)> = None;
        for (index, item) in accept.split(',').enumerate() {
            let mut pieces = item.trim().split(';');
            let media_type = pieces.next().unwrap_or_default().trim();
            let quality = pieces
                .filter_map(|parameter| parameter.trim().strip_prefix("q="))
                .next()
                .map(parse_quality)
                .unwrap_or(Some(1000));
            let Some(quality) = quality else { continue };
            if quality == 0 {
                continue;
            }
            let format = match media_type {
                "application/json" | "application/*" | "*/*" => Some(Self::Json),
                "application/x-ndjson" | "application/ndjson" => Some(Self::Ndjson),
                "text/csv" | "text/*" => Some(Self::Csv),
                _ => None,
            };
            if let Some(format) = format
                && best.is_none_or(|(best_quality, best_index, _)| {
                    quality > best_quality || (quality == best_quality && index < best_index)
                })
            {
                best = Some((quality, index, format));
            }
        }
        best.map(|(_, _, format)| format)
            .ok_or_else(|| ApiError::not_acceptable(accept))
    }

    fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "json" => Some(Self::Json),
            "ndjson" => Some(Self::Ndjson),
            "csv" => Some(Self::Csv),
            _ => None,
        }
    }

    fn content_type(self) -> &'static str {
        match self {
            Self::Json => "application/json",
            Self::Ndjson => "application/x-ndjson",
            Self::Csv => "text/csv; charset=utf-8",
        }
    }
}

fn query_format(query: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        (key.eq_ignore_ascii_case("format")).then(|| value.to_owned())
    })
}

fn parse_quality(value: &str) -> Option<u16> {
    let parsed: f32 = value.parse().ok()?;
    (parsed.is_finite() && (0.0..=1.0).contains(&parsed)).then_some((parsed * 1000.0) as u16)
}

fn serialize_result(
    result: &QueryResult,
    format: ResponseFormat,
    limit: usize,
) -> Result<Vec<u8>, ApiError> {
    let mut writer = LimitedWriter::new(limit);
    let serialization = match format {
        ResponseFormat::Json => write_json(result, &mut writer, false),
        ResponseFormat::Ndjson => write_json(result, &mut writer, true),
        ResponseFormat::Csv => write_csv(result, &mut writer),
    };
    if writer.exceeded {
        return Err(ApiError::response_too_large(limit));
    }
    serialization
        .map_err(|error| ApiError::internal(format!("could not encode result: {error}")))?;
    Ok(writer.bytes)
}

fn write_json(result: &QueryResult, writer: &mut impl Write, ndjson: bool) -> io::Result<()> {
    if !ndjson {
        writer.write_all(b"[")?;
    }
    for (row_index, row) in result.rows.iter().enumerate() {
        if ndjson {
            if row_index > 0 {
                writer.write_all(b"\n")?;
            }
        } else if row_index > 0 {
            writer.write_all(b",")?;
        }
        writer.write_all(b"{")?;
        for (column_index, (column, value)) in result.columns.iter().zip(row).enumerate() {
            if column_index > 0 {
                writer.write_all(b",")?;
            }
            serde_json::to_writer(&mut *writer, column).map_err(io::Error::other)?;
            writer.write_all(b":")?;
            write_json_value(writer, value)?;
        }
        writer.write_all(b"}")?;
    }
    if ndjson {
        if !result.rows.is_empty() {
            writer.write_all(b"\n")?;
        }
    } else {
        writer.write_all(b"]")?;
    }
    Ok(())
}

fn write_json_value(writer: &mut impl Write, value: &QueryValue) -> io::Result<()> {
    match value {
        QueryValue::Null => writer.write_all(b"null"),
        QueryValue::Boolean(value) => writer.write_all(if *value { b"true" } else { b"false" }),
        QueryValue::Int64(value) => write!(writer, "{value}"),
        QueryValue::Float64(value) if value.is_finite() => write!(writer, "{value}"),
        QueryValue::Float64(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "JSON cannot represent a non-finite float",
        )),
        QueryValue::String(value) => serde_json::to_writer(writer, value).map_err(io::Error::other),
    }
}

fn write_csv(result: &QueryResult, writer: &mut impl Write) -> io::Result<()> {
    write_csv_row(writer, result.columns.iter().map(String::as_str))?;
    for row in &result.rows {
        let fields = row.iter().map(csv_value).collect::<Vec<_>>();
        write_csv_row(writer, fields.iter().map(String::as_str))?;
    }
    Ok(())
}

fn csv_value(value: &QueryValue) -> String {
    match value {
        QueryValue::Null => String::new(),
        QueryValue::Boolean(value) => value.to_string(),
        QueryValue::Int64(value) => value.to_string(),
        QueryValue::Float64(value) => value.to_string(),
        QueryValue::String(value) => value.clone(),
    }
}

fn write_csv_row<'a>(
    writer: &mut impl Write,
    fields: impl Iterator<Item = &'a str>,
) -> io::Result<()> {
    for (index, field) in fields.enumerate() {
        if index > 0 {
            writer.write_all(b",")?;
        }
        if field
            .bytes()
            .any(|byte| matches!(byte, b',' | b'"' | b'\r' | b'\n'))
        {
            writer.write_all(b"\"")?;
            for (index, part) in field.split('"').enumerate() {
                if index > 0 {
                    writer.write_all(b"\"\"")?;
                }
                writer.write_all(part.as_bytes())?;
            }
            writer.write_all(b"\"")?;
        } else {
            writer.write_all(field.as_bytes())?;
        }
    }
    writer.write_all(b"\n")
}

struct LimitedWriter {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl LimitedWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(limit.min(8192)),
            limit,
            exceeded: false,
        }
    }
}

impl Write for LimitedWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.len() > self.limit.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(io::Error::other("response limit exceeded"));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    retry_after: Option<&'static str>,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            retry_after: None,
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }

    fn request_too_large(limit: usize) -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            format!("request body exceeds the {limit} byte limit"),
        )
    }

    fn response_too_large(limit: usize) -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "response_too_large",
            format!("encoded response exceeds the {limit} byte limit"),
        )
    }

    fn unsupported_media_type(media_type: &str) -> Self {
        Self::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            format!("unsupported request content type: {media_type}"),
        )
    }

    fn not_acceptable(value: &str) -> Self {
        Self::new(
            StatusCode::NOT_ACCEPTABLE,
            "not_acceptable",
            format!("no supported response format matches: {value}"),
        )
    }

    fn overloaded(limit: usize) -> Self {
        let mut error = Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "overloaded",
            format!("all {limit} query execution slots are busy"),
        );
        error.retry_after = Some("1");
        error
    }

    fn timeout(duration: Duration) -> Self {
        Self::new(
            StatusCode::GATEWAY_TIMEOUT,
            "query_timeout",
            format!("query exceeded its {} ms deadline", duration.as_millis()),
        )
    }

    fn shutting_down() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "shutting_down",
            "server is shutting down",
        )
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
    }

    fn from_query_error(error: QueryError) -> Self {
        let status = match error.kind {
            QueryErrorKind::InvalidQuery => StatusCode::BAD_REQUEST,
            QueryErrorKind::NotFound => StatusCode::NOT_FOUND,
            QueryErrorKind::Conflict => StatusCode::CONFLICT,
            QueryErrorKind::ResourceLimit => StatusCode::TOO_MANY_REQUESTS,
            QueryErrorKind::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            QueryErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self::new(status, error.kind.code(), error.message)
    }

    fn into_response(self, request_id: u64) -> Response {
        let retry_after = self.retry_after;
        let mut response = json_response(
            self.status,
            request_id,
            &ErrorEnvelope {
                error: ErrorBody {
                    code: self.code,
                    message: self.message,
                    request_id,
                },
            },
        );
        if let Some(value) = retry_after {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static(value));
        }
        response
    }
}

#[derive(Serialize)]
struct ErrorEnvelope<'a> {
    error: ErrorBody<'a>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    code: &'a str,
    message: String,
    request_id: u64,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

fn json_response<T: Serialize>(status: StatusCode, request_id: u64, value: &T) -> Response {
    let mut response = (status, Json(value)).into_response();
    insert_common_headers(&mut response, request_id);
    response
}

fn insert_common_headers(response: &mut Response, request_id: u64) {
    response.headers_mut().insert(
        "x-request-id",
        HeaderValue::from_str(&request_id.to_string())
            .expect("numeric request ID is a header value"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
}

async fn not_found(State(state): State<Arc<HttpState>>) -> Response {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "route not found")
        .into_response(state.next_request_id())
}

async fn method_not_allowed(State(state): State<Arc<HttpState>>) -> Response {
    ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "method not allowed for this route",
    )
    .into_response(state.next_request_id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_quotes_special_characters() {
        let result = QueryResult::new(
            vec!["a".into(), "b".into()],
            vec![vec![
                QueryValue::String("one,two".into()),
                QueryValue::String("say \"hi\"".into()),
            ]],
        );
        let bytes = serialize_result(&result, ResponseFormat::Csv, 1024).unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "a,b\n\"one,two\",\"say \"\"hi\"\"\"\n"
        );
    }

    #[test]
    fn negotiation_honors_quality() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("text/csv;q=0.5, application/x-ndjson;q=0.9"),
        );
        assert_eq!(
            ResponseFormat::negotiate(None, &headers).unwrap(),
            ResponseFormat::Ndjson
        );
    }

    #[test]
    fn writer_never_exceeds_limit() {
        let result = QueryResult::new(
            vec!["value".into()],
            vec![vec![QueryValue::String("too large".into())]],
        );
        let error = serialize_result(&result, ResponseFormat::Json, 4).unwrap_err();
        assert_eq!(error.code, "response_too_large");
    }
}
