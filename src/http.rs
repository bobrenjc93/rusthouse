//! Bounded, engine-independent HTTP query service.

use std::{
    future::Future,
    io::{self, Write},
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
        mpsc::{SyncSender, TrySendError, sync_channel},
    },
    task::{Context, Poll},
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
use hyper::server::conn::http1;
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    service::TowerToHyperService,
};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore, oneshot},
    task::{JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;

use crate::query::{
    CancellationOutcome, QueryCancellation, QueryError, QueryErrorKind, QueryRequest, QueryResult,
    QueryService, QueryValue,
};

const FORCE_CANCELLATION_WAIT: Duration = Duration::from_millis(250);
const ACCEPT_BACKOFF_INITIAL: Duration = Duration::from_millis(10);
const ACCEPT_BACKOFF_MAX: Duration = Duration::from_secs(1);
const MAX_CONFIGURED_TIMEOUT: Duration = Duration::from_secs(365 * 24 * 60 * 60);

/// Resource limits and deadlines enforced by the HTTP frontend.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Maximum decoded request body size.
    pub max_request_bytes: usize,
    /// Maximum encoded successful response body size.
    pub max_response_bytes: usize,
    /// Maximum number of queries executing or encoding results at once.
    pub max_concurrent_queries: usize,
    /// Maximum number of query requests being read or processed at once.
    pub max_concurrent_requests: usize,
    /// Maximum number of accepted client connections.
    pub max_connections: usize,
    /// Maximum time allowed to receive one complete HTTP request header.
    pub header_read_timeout: Duration,
    /// Maximum time a connection may make no read or write progress.
    pub connection_idle_timeout: Duration,
    /// Maximum time allowed to read a complete query request body.
    pub request_body_timeout: Duration,
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
            max_concurrent_requests: 64,
            max_connections: 128,
            header_read_timeout: Duration::from_secs(10),
            connection_idle_timeout: Duration::from_secs(60),
            request_body_timeout: Duration::from_secs(10),
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
        validate_permit_count("max_concurrent_queries", self.max_concurrent_queries)?;
        validate_permit_count("max_concurrent_requests", self.max_concurrent_requests)?;
        validate_permit_count("max_connections", self.max_connections)?;
        validate_timeout("header_read_timeout", self.header_read_timeout)?;
        validate_timeout("connection_idle_timeout", self.connection_idle_timeout)?;
        validate_timeout("request_body_timeout", self.request_body_timeout)?;
        validate_timeout("query_timeout", self.query_timeout)?;
        validate_timeout("shutdown_timeout", self.shutdown_timeout)?;
        Ok(())
    }
}

fn validate_timeout(name: &str, timeout: Duration) -> Result<(), ServerError> {
    if timeout.is_zero() {
        return Err(ServerError::InvalidConfig(format!(
            "{name} must be greater than zero"
        )));
    }
    if timeout > MAX_CONFIGURED_TIMEOUT {
        return Err(ServerError::InvalidConfig(format!(
            "{name} must not exceed {} seconds",
            MAX_CONFIGURED_TIMEOUT.as_secs()
        )));
    }
    Ok(())
}

fn deadline_after(timeout: Duration) -> tokio::time::Instant {
    let now = tokio::time::Instant::now();
    now.checked_add(timeout).unwrap_or(now)
}

fn validate_permit_count(name: &str, count: usize) -> Result<(), ServerError> {
    if count == 0 {
        return Err(ServerError::InvalidConfig(format!(
            "{name} must be greater than zero"
        )));
    }
    if count > Semaphore::MAX_PERMITS {
        return Err(ServerError::InvalidConfig(format!(
            "{name} must not exceed {}",
            Semaphore::MAX_PERMITS
        )));
    }
    Ok(())
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
    /// The background server task was already consumed.
    NotRunning,
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidConfig(message) => write!(formatter, "invalid server config: {message}"),
            Self::Io(error) => write!(formatter, "HTTP server I/O error: {error}"),
            Self::Task(error) => write!(formatter, "HTTP server task failed: {error}"),
            Self::NotRunning => write!(formatter, "HTTP server is not running"),
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
    request_permits: Arc<Semaphore>,
    query_permits: Arc<Semaphore>,
    metrics_worker_permits: Arc<Semaphore>,
    metrics_sender: SyncSender<MetricsTask>,
    request_ids: AtomicU64,
    force_cancellation: CancellationToken,
    query_admission: Arc<QueryAdmission>,
}

struct MetricsTask {
    cancellation: QueryCancellation,
    response_limit: usize,
    result_sender: oneshot::Sender<Option<Result<Vec<u8>, ApiError>>>,
    _worker_permit: OwnedSemaphorePermit,
}

impl HttpState {
    fn next_request_id(&self) -> u64 {
        self.request_ids.fetch_add(1, Ordering::Relaxed)
    }
}

#[derive(Default)]
struct QueryAdmission {
    state: Mutex<bool>,
}

impl QueryAdmission {
    fn enter(&self) -> Option<MutexGuard<'_, bool>> {
        let guard = self.state.lock().unwrap_or_else(|error| error.into_inner());
        (!*guard).then_some(guard)
    }

    fn close(&self) {
        let mut closed = self.state.lock().unwrap_or_else(|error| error.into_inner());
        *closed = true;
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
    query_admission: Arc<QueryAdmission>,
    shutdown_timeout: Duration,
    task: Option<JoinHandle<io::Result<()>>>,
}

impl ServerHandle {
    /// Returns the bound address, including the selected port when port zero was used.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Waits for unexpected background server termination.
    ///
    /// This normally remains pending until shutdown is requested. It is
    /// cancellation-safe and can be raced against an operating-system signal.
    pub async fn wait(&mut self) -> Result<(), ServerError> {
        let result = match self.task.as_mut() {
            Some(task) => task.await,
            None => return Err(ServerError::NotRunning),
        };
        self.task.take();
        result.map_err(ServerError::Task)?.map_err(ServerError::Io)
    }

    /// Stops accepting connections, waits for the grace period, then cancels work.
    pub async fn shutdown(mut self) -> Result<(), ServerError> {
        self.graceful_shutdown.cancel();
        let Some(mut task) = self.task.take() else {
            return Err(ServerError::NotRunning);
        };

        match tokio::time::timeout(self.shutdown_timeout, &mut task).await {
            Ok(result) => result.map_err(ServerError::Task)?.map_err(ServerError::Io),
            Err(_) => {
                self.query_admission.close();
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
        self.query_admission.close();
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
    let query_admission = Arc::new(QueryAdmission::default());
    let (metrics_sender, metrics_receiver) = sync_channel(1);
    let metrics_service = Arc::clone(&service);
    let _ = std::thread::Builder::new()
        .name("rusthouse-metrics".to_owned())
        .spawn(move || metrics_worker(metrics_receiver, metrics_service))?;
    let state = Arc::new(HttpState {
        service,
        request_permits: Arc::new(Semaphore::new(config.max_concurrent_requests)),
        query_permits: Arc::new(Semaphore::new(config.max_concurrent_queries)),
        metrics_worker_permits: Arc::new(Semaphore::new(1)),
        metrics_sender,
        config: config.clone(),
        request_ids: AtomicU64::new(1),
        force_cancellation: force_cancellation.clone(),
        query_admission: query_admission.clone(),
    });

    let app = Router::new()
        .route("/query", post(query))
        .route("/metrics", get(metrics))
        .route("/health", get(readiness))
        .route("/health/live", get(liveness))
        .route("/health/ready", get(readiness))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(state);

    let task = tokio::spawn(serve_connections(
        listener,
        app,
        config.max_connections,
        config.header_read_timeout,
        config.connection_idle_timeout,
        graceful_shutdown.clone(),
        force_cancellation.clone(),
    ));

    Ok(ServerHandle {
        local_addr,
        graceful_shutdown,
        force_cancellation,
        query_admission,
        shutdown_timeout: config.shutdown_timeout,
        task: Some(task),
    })
}

fn metrics_worker(
    receiver: std::sync::mpsc::Receiver<MetricsTask>,
    service: Arc<dyn QueryService>,
) {
    while let Ok(task) = receiver.recv() {
        let result = service
            .observability_cancellable(&task.cancellation)
            .map(|snapshot| serialize_json_value(&snapshot, task.response_limit));
        let _ = task.result_sender.send(result);
    }
}

async fn serve_connections(
    listener: TcpListener,
    app: Router,
    max_connections: usize,
    header_read_timeout: Duration,
    connection_idle_timeout: Duration,
    graceful_shutdown: CancellationToken,
    force_cancellation: CancellationToken,
) -> io::Result<()> {
    let connection_permits = Arc::new(Semaphore::new(max_connections));
    let mut connections = JoinSet::new();
    let mut accept_backoff = ACCEPT_BACKOFF_INITIAL;

    loop {
        tokio::select! {
            biased;
            () = graceful_shutdown.cancelled() => break,
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                let _ = result;
            }
            permit = connection_permits.clone().acquire_owned() => {
                let permit = permit.expect("connection semaphore is never closed");
                let accepted = tokio::select! {
                    biased;
                    () = graceful_shutdown.cancelled() => {
                        drop(permit);
                        break;
                    }
                    accepted = listener.accept() => accepted,
                };
                let (stream, _) = match accepted {
                    Ok(accepted) => {
                        accept_backoff = ACCEPT_BACKOFF_INITIAL;
                        accepted
                    }
                    Err(error) => {
                        drop(permit);
                        if !is_recoverable_accept_error(&error) {
                            return Err(error);
                        }
                        eprintln!(
                            "RustHouse HTTP accept error: {error}; retrying in {} ms",
                            accept_backoff.as_millis()
                        );
                        tokio::select! {
                            biased;
                            () = graceful_shutdown.cancelled() => break,
                            () = tokio::time::sleep(accept_backoff) => {}
                        }
                        accept_backoff = next_accept_backoff(accept_backoff);
                        continue;
                    }
                };
                connections.spawn(serve_connection(
                    stream,
                    app.clone(),
                    permit,
                    header_read_timeout,
                    connection_idle_timeout,
                    graceful_shutdown.clone(),
                    force_cancellation.clone(),
                ));
            }
        }
    }

    while let Some(result) = connections.join_next().await {
        let _ = result;
    }
    Ok(())
}

fn next_accept_backoff(current: Duration) -> Duration {
    current.saturating_mul(2).min(ACCEPT_BACKOFF_MAX)
}

fn is_recoverable_accept_error(error: &io::Error) -> bool {
    !matches!(
        error.kind(),
        io::ErrorKind::InvalidInput
            | io::ErrorKind::InvalidData
            | io::ErrorKind::NotConnected
            | io::ErrorKind::Unsupported
    )
}

async fn serve_connection(
    stream: TcpStream,
    app: Router,
    _permit: OwnedSemaphorePermit,
    header_read_timeout: Duration,
    connection_idle_timeout: Duration,
    graceful_shutdown: CancellationToken,
    force_cancellation: CancellationToken,
) {
    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(header_read_timeout);
    let stream = IdleTimeoutStream::new(stream, connection_idle_timeout);
    let connection = builder.serve_connection(TokioIo::new(stream), TowerToHyperService::new(app));
    tokio::pin!(connection);

    tokio::select! {
        biased;
        () = force_cancellation.cancelled() => {}
        result = &mut connection => {
            let _ = result;
        }
        () = graceful_shutdown.cancelled() => {
            connection.as_mut().graceful_shutdown();
            tokio::select! {
                biased;
                () = force_cancellation.cancelled() => {}
                result = &mut connection => {
                    let _ = result;
                }
            }
        }
    }
}

struct IdleTimeoutStream {
    stream: TcpStream,
    timeout: Duration,
    deadline: Pin<Box<tokio::time::Sleep>>,
}

impl IdleTimeoutStream {
    fn new(stream: TcpStream, timeout: Duration) -> Self {
        Self {
            stream,
            timeout,
            deadline: Box::pin(tokio::time::sleep_until(deadline_after(timeout))),
        }
    }

    fn poll_timeout(&mut self, context: &mut Context<'_>) -> io::Result<()> {
        if self.deadline.as_mut().poll(context).is_ready() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "HTTP connection was idle for too long",
            ));
        }
        Ok(())
    }

    fn record_progress(&mut self) {
        self.deadline.as_mut().reset(deadline_after(self.timeout));
    }
}

impl AsyncRead for IdleTimeoutStream {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Err(error) = this.poll_timeout(context) {
            return Poll::Ready(Err(error));
        }
        let filled_before = buffer.filled().len();
        match Pin::new(&mut this.stream).poll_read(context, buffer) {
            Poll::Ready(Ok(())) => {
                if buffer.filled().len() > filled_before {
                    this.record_progress();
                }
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

impl AsyncWrite for IdleTimeoutStream {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if let Err(error) = this.poll_timeout(context) {
            return Poll::Ready(Err(error));
        }
        match Pin::new(&mut this.stream).poll_write(context, buffer) {
            Poll::Ready(Ok(written)) => {
                if written > 0 {
                    this.record_progress();
                }
                Poll::Ready(Ok(written))
            }
            other => other,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Err(error) = this.poll_timeout(context) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut this.stream).poll_flush(context)
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Err(error) = this.poll_timeout(context) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut this.stream).poll_shutdown(context)
    }
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

async fn metrics(State(state): State<Arc<HttpState>>) -> Response {
    let request_id = state.next_request_id();
    let _request_permit = match state.request_permits.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return ApiError::request_overloaded(state.config.max_concurrent_requests)
                .into_response(request_id);
        }
    };
    let query_permit = match state.query_permits.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return ApiError::overloaded(state.config.max_concurrent_queries)
                .into_response(request_id);
        }
    };
    let worker_permit = match state.metrics_worker_permits.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return ApiError::overloaded(1).into_response(request_id);
        }
    };
    let Some(admission) = state.query_admission.enter() else {
        return ApiError::shutting_down().into_response(request_id);
    };
    let response_limit = state.config.max_response_bytes;
    let token = state.force_cancellation.child_token();
    let cancellation = QueryCancellation::new(token);
    let mut cancel_on_drop = CancelOnDrop(Some(cancellation.clone()));
    let (sender, receiver) = oneshot::channel();
    let task = MetricsTask {
        cancellation,
        response_limit,
        result_sender: sender,
        _worker_permit: worker_permit,
    };
    let queued = state.metrics_sender.try_send(task);
    drop(admission);
    if let Err(error) = queued {
        return match error {
            TrySendError::Full(_) => ApiError::overloaded(1),
            TrySendError::Disconnected(_) => ApiError::internal("metrics worker is not running"),
        }
        .into_response(request_id);
    }
    let deadline = deadline_after(state.config.query_timeout);
    let result = tokio::select! {
        biased;
        () = state.force_cancellation.cancelled() => {
            let _ = cancel_on_drop.0.take().expect("metrics cancellation is present").cancel();
            return ApiError::shutting_down().into_response(request_id);
        }
        () = tokio::time::sleep_until(deadline) => {
            let _ = cancel_on_drop.0.take().expect("metrics cancellation is present").cancel();
            return ApiError::metrics_timeout(state.config.query_timeout).into_response(request_id);
        }
        result = receiver => match result {
            Ok(result) => result,
            Err(_) => {
                return ApiError::internal("metrics worker stopped without a result")
                    .into_response(request_id);
            }
        },
    };
    drop(query_permit);
    cancel_on_drop.0 = None;
    match result {
        Some(Ok(bytes)) => {
            let mut response = Response::new(Body::from(bytes));
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            insert_common_headers(&mut response, request_id);
            response
        }
        Some(Err(error)) => error.into_response(request_id),
        None => ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "observability_unavailable",
            "the query service does not expose engine observability",
        )
        .into_response(request_id),
    }
}

async fn query(State(state): State<Arc<HttpState>>, request: Request) -> Response {
    let request_id = state.next_request_id();
    let request_permit = match state.request_permits.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return ApiError::request_overloaded(state.config.max_concurrent_requests)
                .into_response(request_id);
        }
    };
    let response = match handle_query(&state, request_id, request).await {
        Ok(response) => response,
        Err(error) => error.into_response(request_id),
    };
    drop(request_permit);
    response
}

async fn handle_query(
    state: &Arc<HttpState>,
    request_id: u64,
    request: Request,
) -> Result<Response, ApiError> {
    if request.headers().contains_key(header::ORIGIN) {
        return Err(ApiError::origin_not_allowed());
    }
    let format = ResponseFormat::negotiate(request.uri().query(), request.headers())?;
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|value| {
            value
                .to_str()
                .map(str::to_owned)
                .map_err(|_| ApiError::unsupported_media_type("non-UTF-8 Content-Type"))
        })
        .transpose()?;
    let body_format = QueryBodyFormat::from_content_type(content_type.as_deref())?;

    if let Some(length) = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        && length > state.config.max_request_bytes
    {
        return Err(ApiError::request_too_large(state.config.max_request_bytes));
    }

    let body = tokio::time::timeout(
        state.config.request_body_timeout,
        to_bytes(request.into_body(), state.config.max_request_bytes),
    )
    .await
    .map_err(|_| ApiError::request_timeout(state.config.request_body_timeout))?
    .map_err(|_| ApiError::request_too_large(state.config.max_request_bytes))?;
    let sql = parse_query_body(&body, body_format)?;

    let permit = state
        .query_permits
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::overloaded(state.config.max_concurrent_queries))?;

    let token = state.force_cancellation.child_token();
    let cancellation = QueryCancellation::new(token.clone());
    let mut cancel_on_drop = CancelOnDrop(Some(cancellation.clone()));
    let query_request = QueryRequest {
        sql,
        request_id,
        cancellation: cancellation.clone(),
        max_result_bytes: state.config.max_response_bytes,
    };
    let service = state.service.clone();
    let query_admission = state.query_admission.clone();
    let runtime = tokio::runtime::Handle::current();
    let deadline = deadline_after(state.config.query_timeout);
    let mut execution = tokio::task::spawn_blocking(move || {
        let Some(admission) = query_admission.enter() else {
            return EngineTaskOutput::AdmissionClosed(permit);
        };
        if query_request.cancellation.is_cancelled() {
            return EngineTaskOutput::Cancelled(permit);
        }
        let execution = service.execute(query_request);
        drop(admission);
        EngineTaskOutput::Finished(permit, runtime.block_on(execution))
    });

    let output = tokio::select! {
        biased;
        () = state.force_cancellation.cancelled() => {
            let cancellation_outcome = cancellation.cancel();
            execution.abort();
            return Err(match cancellation_outcome {
                CancellationOutcome::Cancelled => ApiError::shutting_down(),
                CancellationOutcome::PublicationInProgress => ApiError::query_outcome_unknown(),
            });
        }
        () = tokio::time::sleep_until(deadline) => {
            let cancellation_outcome = cancellation.cancel();
            execution.abort();
            return Err(match cancellation_outcome {
                CancellationOutcome::Cancelled => ApiError::timeout(state.config.query_timeout),
                CancellationOutcome::PublicationInProgress => ApiError::query_outcome_unknown(),
            });
        }
        output = &mut execution => output
            .map_err(|error| ApiError::blocking_task_failed("query execution", error))?,
    };
    let (permit, result) = match output {
        EngineTaskOutput::AdmissionClosed(permit) => {
            drop(permit);
            return Err(ApiError::shutting_down());
        }
        EngineTaskOutput::Cancelled(permit) => {
            drop(permit);
            return Err(ApiError::shutting_down());
        }
        EngineTaskOutput::Finished(permit, result) => (permit, result),
    };
    let result = result.map_err(ApiError::from_query_error)?;
    let mutation_published = cancellation.publication_started();

    let response_limit = state.config.max_response_bytes;
    let mut encoding = tokio::task::spawn_blocking(move || {
        let encoded = result
            .validate()
            .map_err(ApiError::from_query_error)
            .and_then(|()| serialize_result(&result, format, response_limit));
        (permit, encoded)
    });
    let (permit, bytes) = tokio::select! {
        biased;
        () = state.force_cancellation.cancelled() => {
            token.cancel();
            encoding.abort();
            if mutation_published {
                cancel_on_drop.0 = None;
                return Ok(published_mutation_response(request_id));
            }
            return Err(ApiError::shutting_down());
        }
        () = tokio::time::sleep_until(deadline) => {
            token.cancel();
            encoding.abort();
            if mutation_published {
                cancel_on_drop.0 = None;
                return Ok(published_mutation_response(request_id));
            }
            return Err(ApiError::timeout(state.config.query_timeout));
        }
        output = &mut encoding => match output {
            Ok(output) => output,
            Err(_) if mutation_published => {
                cancel_on_drop.0 = None;
                return Ok(published_mutation_response(request_id));
            }
            Err(error) => {
                return Err(ApiError::blocking_task_failed("result encoding", error));
            }
        },
    };
    drop(permit);
    let bytes = match bytes {
        Ok(bytes) => bytes,
        Err(_) if mutation_published => {
            cancel_on_drop.0 = None;
            return Ok(published_mutation_response(request_id));
        }
        Err(error) => return Err(error),
    };
    cancel_on_drop.0 = None;
    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(format.content_type()),
    );
    insert_common_headers(&mut response, request_id);
    Ok(response)
}

fn published_mutation_response(request_id: u64) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::NO_CONTENT;
    insert_common_headers(&mut response, request_id);
    response
}

enum EngineTaskOutput {
    AdmissionClosed(OwnedSemaphorePermit),
    Cancelled(OwnedSemaphorePermit),
    Finished(OwnedSemaphorePermit, Result<QueryResult, QueryError>),
}

struct CancelOnDrop(Option<QueryCancellation>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(cancellation) = &self.0 {
            let _ = cancellation.cancel();
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonQuery {
    query: String,
}

#[derive(Debug, Clone, Copy)]
enum QueryBodyFormat {
    Sql,
    Json,
}

impl QueryBodyFormat {
    fn from_content_type(content_type: Option<&str>) -> Result<Self, ApiError> {
        let media_type = content_type
            .ok_or_else(ApiError::content_type_required)?
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        match media_type.as_str() {
            "application/sql" => Ok(Self::Sql),
            "application/json" => Ok(Self::Json),
            _ => Err(ApiError::unsupported_media_type(&media_type)),
        }
    }
}

fn parse_query_body(body: &[u8], format: QueryBodyFormat) -> Result<String, ApiError> {
    let query = match format {
        QueryBodyFormat::Sql => std::str::from_utf8(body)
            .map_err(|_| ApiError::bad_request("query body must be valid UTF-8"))?
            .to_owned(),
        QueryBodyFormat::Json => {
            serde_json::from_slice::<JsonQuery>(body)
                .map_err(|error| {
                    ApiError::bad_request(format!("invalid JSON query body: {error}"))
                })?
                .query
        }
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

        if !headers.contains_key(header::ACCEPT) {
            return Ok(Self::Json);
        }
        let (ranges, raw_accept) = parse_accept_ranges(headers)?;
        [Self::Json, Self::Ndjson, Self::Csv]
            .into_iter()
            .filter_map(|format| {
                format
                    .accepted_quality(&ranges)
                    .filter(|quality| *quality > 0)
                    .map(|quality| (quality, format))
            })
            .max_by_key(|(quality, format)| (*quality, format.preference()))
            .map(|(_, format)| format)
            .ok_or_else(|| ApiError::not_acceptable(&raw_accept))
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

    fn media_type(self) -> (&'static str, &'static str) {
        match self {
            Self::Json => ("application", "json"),
            Self::Ndjson => ("application", "x-ndjson"),
            Self::Csv => ("text", "csv"),
        }
    }

    fn accepted_quality(self, ranges: &[MediaRange]) -> Option<u16> {
        let (type_name, subtype) = self.media_type();
        ranges
            .iter()
            .filter_map(|range| {
                range
                    .specificity(type_name, subtype)
                    .map(|specificity| (specificity, range.quality, usize::MAX - range.order))
            })
            .max_by_key(|&(specificity, quality, reverse_order)| {
                (specificity, quality, reverse_order)
            })
            .map(|(_, quality, _)| quality)
    }

    fn preference(self) -> u8 {
        match self {
            Self::Json => 3,
            Self::Ndjson => 2,
            Self::Csv => 1,
        }
    }
}

#[derive(Debug)]
struct MediaRange {
    type_name: String,
    subtype: String,
    quality: u16,
    order: usize,
}

impl MediaRange {
    fn specificity(&self, type_name: &str, subtype: &str) -> Option<u8> {
        if self.type_name == "*" && self.subtype == "*" {
            return Some(0);
        }
        if self.type_name != type_name {
            return None;
        }
        if self.subtype == "*" {
            return Some(1);
        }
        let requested_subtype = if type_name == "application" && self.subtype == "ndjson" {
            "x-ndjson"
        } else {
            &self.subtype
        };
        (requested_subtype == subtype).then_some(2)
    }
}

fn parse_accept_ranges(headers: &HeaderMap) -> Result<(Vec<MediaRange>, String), ApiError> {
    let mut ranges = Vec::new();
    let mut raw_values = Vec::new();
    let mut order = 0;
    for value in headers.get_all(header::ACCEPT) {
        let value = value
            .to_str()
            .map_err(|_| ApiError::not_acceptable("non-UTF-8 Accept header"))?;
        raw_values.push(value);
        for item in value.split(',') {
            if let Some(range) = parse_media_range(item, order) {
                ranges.push(range);
            }
            order += 1;
        }
    }
    Ok((ranges, raw_values.join(", ")))
}

fn parse_media_range(item: &str, order: usize) -> Option<MediaRange> {
    let mut pieces = item.trim().split(';');
    let (type_name, subtype) = pieces.next()?.trim().split_once('/')?;
    let type_name = type_name.trim().to_ascii_lowercase();
    let subtype = subtype.trim().to_ascii_lowercase();
    if type_name.is_empty()
        || subtype.is_empty()
        || (type_name == "*" && subtype != "*")
        || subtype.contains('*') && subtype != "*"
    {
        return None;
    }

    let mut quality = 1000;
    for parameter in pieces {
        let Some((name, value)) = parameter.split_once('=') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("q") {
            quality = parse_quality(value.trim())?;
        }
    }
    Some(MediaRange {
        type_name,
        subtype,
        quality,
        order,
    })
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

fn serialize_json_value(value: &impl Serialize, limit: usize) -> Result<Vec<u8>, ApiError> {
    let mut writer = LimitedWriter::new(limit);
    let serialization = serde_json::to_writer(&mut writer, value);
    if writer.exceeded {
        return Err(ApiError::response_too_large(limit));
    }
    serialization
        .map_err(|error| ApiError::internal(format!("could not encode metrics: {error}")))?;
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
        QueryValue::Float64(value) if value.is_nan() => writer.write_all(b"\"NaN\""),
        QueryValue::Float64(value) if value.is_sign_positive() => writer.write_all(b"\"Infinity\""),
        QueryValue::Float64(_) => writer.write_all(b"\"-Infinity\""),
        QueryValue::String(value) => serde_json::to_writer(writer, value).map_err(io::Error::other),
    }
}

fn write_csv(result: &QueryResult, writer: &mut impl Write) -> io::Result<()> {
    for (index, column) in result.columns.iter().enumerate() {
        write_csv_separator(writer, index)?;
        write_csv_field(writer, column)?;
    }
    writer.write_all(b"\n")?;

    for row in &result.rows {
        for (index, value) in row.iter().enumerate() {
            write_csv_separator(writer, index)?;
            write_csv_value(writer, value)?;
        }
        writer.write_all(b"\n")?;
    }
    Ok(())
}

fn write_csv_value(writer: &mut impl Write, value: &QueryValue) -> io::Result<()> {
    match value {
        QueryValue::Null => Ok(()),
        QueryValue::Boolean(value) => write!(writer, "{value}"),
        QueryValue::Int64(value) => write!(writer, "{value}"),
        QueryValue::Float64(value) => write!(writer, "{value}"),
        QueryValue::String(value) => write_csv_field(writer, value),
    }
}

fn write_csv_separator(writer: &mut impl Write, index: usize) -> io::Result<()> {
    if index > 0 {
        writer.write_all(b",")?;
    }
    Ok(())
}

fn write_csv_field(writer: &mut impl Write, field: &str) -> io::Result<()> {
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
    Ok(())
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
    close_connection: bool,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            retry_after: None,
            close_connection: false,
        }
    }

    fn close_connection(mut self) -> Self {
        self.close_connection = true;
        self
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
        .close_connection()
    }

    fn request_overloaded(limit: usize) -> Self {
        let mut error = Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "request_overloaded",
            format!("all {limit} HTTP query request slots are busy"),
        )
        .close_connection();
        error.retry_after = Some("1");
        error
    }

    fn request_timeout(duration: Duration) -> Self {
        Self::new(
            StatusCode::REQUEST_TIMEOUT,
            "request_timeout",
            format!(
                "query request body was not received within {} ms",
                duration.as_millis()
            ),
        )
        .close_connection()
    }

    fn origin_not_allowed() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "origin_not_allowed",
            "browser-originated query requests are not allowed",
        )
        .close_connection()
    }

    fn content_type_required() -> Self {
        Self::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "content_type_required",
            "query requests require Content-Type application/sql or application/json",
        )
        .close_connection()
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
        .close_connection()
    }

    fn not_acceptable(value: &str) -> Self {
        Self::new(
            StatusCode::NOT_ACCEPTABLE,
            "not_acceptable",
            format!("no supported response format matches: {value}"),
        )
        .close_connection()
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

    fn metrics_timeout(duration: Duration) -> Self {
        Self::new(
            StatusCode::GATEWAY_TIMEOUT,
            "metrics_timeout",
            format!(
                "metrics collection exceeded its {} ms deadline",
                duration.as_millis()
            ),
        )
    }

    fn query_outcome_unknown() -> Self {
        Self::new(
            StatusCode::GATEWAY_TIMEOUT,
            "query_outcome_unknown",
            "the request timed out after mutation publication began; its commit outcome is unknown",
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

    fn blocking_task_failed(stage: &str, error: tokio::task::JoinError) -> Self {
        Self::internal(format!("{stage} worker failed: {error}"))
    }

    fn from_query_error(error: QueryError) -> Self {
        let status = match error.kind {
            QueryErrorKind::InvalidQuery => StatusCode::BAD_REQUEST,
            QueryErrorKind::NotFound => StatusCode::NOT_FOUND,
            QueryErrorKind::Conflict => StatusCode::CONFLICT,
            QueryErrorKind::ResourceLimit => StatusCode::TOO_MANY_REQUESTS,
            QueryErrorKind::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            QueryErrorKind::PublishedUncertain => StatusCode::ACCEPTED,
            QueryErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self::new(status, error.kind.code(), error.message)
    }

    fn into_response(self, request_id: u64) -> Response {
        let retry_after = self.retry_after;
        let close_connection = self.close_connection;
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
        if close_connection {
            response
                .headers_mut()
                .insert(header::CONNECTION, HeaderValue::from_static("close"));
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
    fn json_uses_stable_strings_for_non_finite_floats() {
        let result = QueryResult::new(
            vec!["nan".into(), "positive".into(), "negative".into()],
            vec![vec![
                QueryValue::Float64(f64::NAN),
                QueryValue::Float64(f64::INFINITY),
                QueryValue::Float64(f64::NEG_INFINITY),
            ]],
        );
        let bytes = serialize_result(&result, ResponseFormat::Json, 1024).unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            r#"[{"nan":"NaN","positive":"Infinity","negative":"-Infinity"}]"#
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
    fn exact_rejection_takes_precedence_over_wildcard() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("application/json;q=0, */*;q=1"),
        );

        assert_eq!(
            ResponseFormat::negotiate(None, &headers).unwrap(),
            ResponseFormat::Ndjson
        );
    }

    #[test]
    fn negotiation_is_case_insensitive() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("APPLICATION/JSON;Q=0.8, TEXT/CSV;Q=0.5"),
        );

        assert_eq!(
            ResponseFormat::negotiate(None, &headers).unwrap(),
            ResponseFormat::Json
        );
    }

    #[test]
    fn negotiation_rejects_formats_with_zero_quality() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, HeaderValue::from_static("*/*;q=0"));

        let error = ResponseFormat::negotiate(None, &headers).unwrap_err();
        assert_eq!(error.status, StatusCode::NOT_ACCEPTABLE);
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

    #[test]
    fn oversized_csv_is_stopped_by_limited_writer() {
        let result = QueryResult::new(
            vec!["value".into()],
            vec![vec![QueryValue::String(
                "large,\"field\"".repeat(128 * 1024),
            )]],
        );

        let error = serialize_result(&result, ResponseFormat::Csv, 64).unwrap_err();
        assert_eq!(error.code, "response_too_large");
    }

    #[test]
    fn rejects_extreme_timeout_durations() {
        for field in [
            "header_read_timeout",
            "connection_idle_timeout",
            "request_body_timeout",
            "query_timeout",
            "shutdown_timeout",
        ] {
            let mut config = ServerConfig::default();
            match field {
                "header_read_timeout" => config.header_read_timeout = Duration::MAX,
                "connection_idle_timeout" => config.connection_idle_timeout = Duration::MAX,
                "request_body_timeout" => config.request_body_timeout = Duration::MAX,
                "query_timeout" => config.query_timeout = Duration::MAX,
                "shutdown_timeout" => config.shutdown_timeout = Duration::MAX,
                _ => unreachable!(),
            }

            let error = config.validate().unwrap_err();
            assert!(error.to_string().contains(field));
            assert!(error.to_string().contains("must not exceed"));
        }

        let boundary = ServerConfig {
            header_read_timeout: MAX_CONFIGURED_TIMEOUT,
            connection_idle_timeout: MAX_CONFIGURED_TIMEOUT,
            request_body_timeout: MAX_CONFIGURED_TIMEOUT,
            query_timeout: MAX_CONFIGURED_TIMEOUT,
            shutdown_timeout: MAX_CONFIGURED_TIMEOUT,
            ..ServerConfig::default()
        };
        boundary.validate().unwrap();
    }

    #[test]
    fn rejects_semaphore_counts_above_tokio_limit() {
        let too_many = Semaphore::MAX_PERMITS + 1;
        for field in [
            "max_concurrent_queries",
            "max_concurrent_requests",
            "max_connections",
        ] {
            let mut config = ServerConfig::default();
            match field {
                "max_concurrent_queries" => config.max_concurrent_queries = too_many,
                "max_concurrent_requests" => config.max_concurrent_requests = too_many,
                "max_connections" => config.max_connections = too_many,
                _ => unreachable!(),
            }
            let error = config.validate().unwrap_err();
            assert!(error.to_string().contains(field));
            assert!(error.to_string().contains("must not exceed"));
        }

        let boundary = ServerConfig {
            max_concurrent_queries: Semaphore::MAX_PERMITS,
            max_concurrent_requests: Semaphore::MAX_PERMITS,
            max_connections: Semaphore::MAX_PERMITS,
            ..ServerConfig::default()
        };
        boundary.validate().unwrap();
    }

    #[test]
    fn accept_error_backoff_is_capped() {
        assert_eq!(
            next_accept_backoff(ACCEPT_BACKOFF_INITIAL),
            Duration::from_millis(20)
        );
        assert_eq!(
            next_accept_backoff(Duration::from_millis(800)),
            ACCEPT_BACKOFF_MAX
        );
        assert_eq!(next_accept_backoff(ACCEPT_BACKOFF_MAX), ACCEPT_BACKOFF_MAX);
        assert!(is_recoverable_accept_error(&io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "transient"
        )));
        assert!(!is_recoverable_accept_error(&io::Error::new(
            io::ErrorKind::NotConnected,
            "fatal"
        )));
    }

    #[tokio::test]
    async fn server_task_failure_is_observable() {
        let task = tokio::spawn(async { Err(io::Error::other("accept loop failed")) });
        let mut server = ServerHandle {
            local_addr: "127.0.0.1:1".parse().unwrap(),
            graceful_shutdown: CancellationToken::new(),
            force_cancellation: CancellationToken::new(),
            query_admission: Arc::new(QueryAdmission::default()),
            shutdown_timeout: Duration::from_secs(1),
            task: Some(task),
        };

        let error = server.wait().await.unwrap_err();
        assert!(matches!(error, ServerError::Io(_)));
        assert!(error.to_string().contains("accept loop failed"));
        assert!(matches!(server.wait().await, Err(ServerError::NotRunning)));
    }
}
