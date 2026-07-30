//! Bounded HTTP access to a shared [`Database`](crate::Database).
//!
//! `POST /query` accepts `application/sql` UTF-8 bodies and negotiates JSON or
//! CSV responses. `GET /health` reports process availability. Listeners are
//! restricted to loopback because query execution is intentionally unauthenticated.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::engine::{ExecutionBudget, ExecutionBudgetLimits};
use crate::format::{OutputFormat, render};
use crate::sql::{self, Statement};
use crate::{Database, Error, QueryResult, StatementResult, Value};

/// Maximum accepted SQL request body size (1 MiB).
pub const MAX_REQUEST_BODY_BYTES: usize = 1024 * 1024;
/// Maximum simultaneously accepted or queued connections.
pub const MAX_CONNECTIONS: usize = 128;
/// Fixed request worker count.
pub const WORKER_COUNT: usize = 8;
/// Total time allowed to queue, receive, execute, and answer one request.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum statements accepted in one HTTP batch.
pub const MAX_STATEMENTS_PER_BATCH: usize = 32;
/// Maximum encoded response size (4 MiB).
pub const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

const MAX_HEADER_BYTES: usize = 16 * 1024;
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(10);
const QUEUE_CAPACITY: usize = MAX_CONNECTIONS - WORKER_COUNT;
const MAX_SCHEMA_COLUMNS: usize = 1024;
const MAX_QUERY_ITEMS: usize = 256;
const MAX_GROUP_OR_ORDER_COLUMNS: usize = 64;
const MAX_INSERT_CELLS: usize = 100_000;
const MAX_SCANNED_ROWS: usize = 100_000;
const MAX_WORK_UNITS: usize = 1_000_000;
const MAX_INTERMEDIATE_GROUPS: usize = 10_000;
const MAX_RESULT_ROWS: usize = 10_000;
const MAX_RESULT_CELLS: usize = 100_000;
const MAX_MATERIALIZED_BYTES: usize = 2 * 1024 * 1024;
const MAX_RESULT_COLUMNS: usize = 256;

/// Serve HTTP requests until SIGINT or SIGTERM is received.
pub fn serve(address: &str) -> io::Result<()> {
    let listener = TcpListener::bind(address)?;
    let local_address = listener.local_addr()?;
    if !local_address.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "HTTP service only permits loopback listen addresses",
        ));
    }
    listener.set_nonblocking(true)?;

    let shutting_down = Arc::new(AtomicBool::new(false));
    let signal_flag = Arc::clone(&shutting_down);
    ctrlc::set_handler(move || signal_flag.store(true, Ordering::Release))
        .map_err(|error| io::Error::other(format!("could not install signal handler: {error}")))?;

    let state = Arc::new(RwLock::new(Database::new()));
    let active_connections = Arc::new(AtomicUsize::new(0));
    let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
    let workers = spawn_workers(receiver, &state)?;

    eprintln!("RustHouse HTTP server listening on http://{local_address}");
    accept_connections(&listener, &sender, &active_connections, &shutting_down)?;

    drop(sender);
    for worker in workers {
        worker
            .join()
            .map_err(|_| io::Error::other("HTTP worker panicked during shutdown"))?;
    }
    eprintln!("RustHouse HTTP server stopped");
    Ok(())
}

fn spawn_workers(
    receiver: Receiver<Connection>,
    state: &Arc<RwLock<Database>>,
) -> io::Result<Vec<thread::JoinHandle<()>>> {
    let receiver = Arc::new(Mutex::new(receiver));
    let mut workers = Vec::with_capacity(WORKER_COUNT);
    for index in 0..WORKER_COUNT {
        let receiver = Arc::clone(&receiver);
        let state = Arc::clone(state);
        match thread::Builder::new()
            .name(format!("rusthouse-http-{index}"))
            .spawn(move || worker_loop(&receiver, &state))
        {
            Ok(worker) => workers.push(worker),
            Err(error) => return Err(error),
        }
    }
    Ok(workers)
}

fn worker_loop(receiver: &Mutex<Receiver<Connection>>, state: &RwLock<Database>) {
    loop {
        let connection = match receiver.lock() {
            Ok(receiver) => receiver.recv(),
            Err(_) => return,
        };
        let Ok(mut connection) = connection else {
            return;
        };
        let deadline = connection.deadline;
        if let Err(error) = handle_connection(&mut connection.stream, state, deadline) {
            eprintln!("HTTP connection error: {error}");
        }
    }
}

fn accept_connections(
    listener: &TcpListener,
    sender: &SyncSender<Connection>,
    active_connections: &Arc<AtomicUsize>,
    shutting_down: &AtomicBool,
) -> io::Result<()> {
    while !shutting_down.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_nonblocking(false)?;
                let previous = active_connections.fetch_add(1, Ordering::AcqRel);
                if previous >= MAX_CONNECTIONS {
                    active_connections.fetch_sub(1, Ordering::AcqRel);
                    reject_busy(&mut stream);
                    continue;
                }

                let connection = Connection {
                    stream,
                    active_connections: Arc::clone(active_connections),
                    deadline: Instant::now() + REQUEST_TIMEOUT,
                };
                match sender.try_send(connection) {
                    Ok(()) => {}
                    Err(TrySendError::Full(mut connection)) => {
                        reject_busy(&mut connection.stream);
                    }
                    Err(TrySendError::Disconnected(mut connection)) => {
                        reject_busy(&mut connection.stream);
                        return Err(io::Error::other("all HTTP workers stopped"));
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn reject_busy(stream: &mut TcpStream) {
    let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));
    let _ = write_response(
        stream,
        Response::error(
            503,
            "Service Unavailable",
            "server connection limit reached",
        ),
    );
}

struct Connection {
    stream: TcpStream,
    active_connections: Arc<AtomicUsize>,
    deadline: Instant,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.active_connections.fetch_sub(1, Ordering::AcqRel);
    }
}

fn handle_connection(
    stream: &mut TcpStream,
    state: &RwLock<Database>,
    deadline: Instant,
) -> io::Result<()> {
    stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
    stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;

    let response = if Instant::now() >= deadline {
        Response::error(
            408,
            "Request Timeout",
            "request was not received within 10 seconds",
        )
    } else {
        match read_request(stream, deadline) {
            Ok(request) => route(request, state, deadline),
            Err(response) => response,
        }
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    stream.set_write_timeout(Some(if remaining.is_zero() {
        Duration::from_secs(1)
    } else {
        remaining
    }))?;
    write_response(stream, response)
}

#[derive(Debug)]
struct Request {
    method: String,
    target: String,
    accept: Option<String>,
    content_type: Option<String>,
    origin: Option<String>,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream, deadline: Instant) -> Result<Request, Response> {
    let mut received = Vec::with_capacity(1024);
    let header_end = loop {
        if let Some(position) = find_header_end(&received) {
            break position;
        }
        if received.len() >= MAX_HEADER_BYTES {
            return Err(Response::error(
                431,
                "Request Header Fields Too Large",
                "request headers exceed 16384 bytes",
            ));
        }
        read_more(stream, &mut received, MAX_HEADER_BYTES, deadline)?;
    };

    let header = std::str::from_utf8(&received[..header_end])
        .map_err(|_| Response::error(400, "Bad Request", "request headers must be valid UTF-8"))?;
    let mut lines = header.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| Response::error(400, "Bad Request", "missing request line"))?;
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(Response::error(
            400,
            "Bad Request",
            "malformed request line",
        ));
    };
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(Response::error(
            505,
            "HTTP Version Not Supported",
            "only HTTP/1.0 and HTTP/1.1 are supported",
        ));
    }
    let method = method.to_owned();
    let target = target.to_owned();

    let mut content_length = None;
    let mut accept = None;
    let mut content_type = None;
    let mut origin = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return Err(Response::error(
                400,
                "Bad Request",
                "malformed request header",
            ));
        };
        if !valid_header_name(name) {
            return Err(Response::error(
                400,
                "Bad Request",
                "malformed request header name",
            ));
        }
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() {
                return Err(Response::error(
                    400,
                    "Bad Request",
                    "Content-Length may only be supplied once",
                ));
            }
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(Response::error(
                    400,
                    "Bad Request",
                    "invalid Content-Length header",
                ));
            }
            content_length = Some(value.parse::<usize>().map_err(|_| {
                Response::error(400, "Bad Request", "invalid Content-Length header")
            })?);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(Response::error(
                400,
                "Bad Request",
                "Transfer-Encoding is not supported",
            ));
        } else if name.eq_ignore_ascii_case("accept") {
            accept = Some(value.to_owned());
        } else if name.eq_ignore_ascii_case("content-type") {
            if content_type.is_some() {
                return Err(Response::error(
                    400,
                    "Bad Request",
                    "Content-Type may only be supplied once",
                ));
            }
            content_type = Some(value.to_owned());
        } else if name.eq_ignore_ascii_case("origin") {
            if origin.is_some() {
                return Err(Response::error(
                    400,
                    "Bad Request",
                    "Origin may only be supplied once",
                ));
            }
            origin = Some(value.to_owned());
        }
    }

    if method == "POST" && content_length.is_none() {
        return Err(Response::error(
            411,
            "Length Required",
            "POST requests require Content-Length",
        ));
    }
    let content_length = content_length.unwrap_or(0);
    if content_length > MAX_REQUEST_BODY_BYTES {
        return Err(Response::error(
            413,
            "Content Too Large",
            "request body exceeds 1048576 bytes",
        ));
    }

    let body_start = header_end + 4;
    let body_end = body_start + content_length;
    while received.len() < body_end {
        read_more(stream, &mut received, body_end, deadline)?;
    }
    received.truncate(body_end);

    Ok(Request {
        method,
        target,
        accept,
        content_type,
        origin,
        body: received.split_off(body_start),
    })
}

fn read_more(
    stream: &mut TcpStream,
    buffer: &mut Vec<u8>,
    limit: usize,
    deadline: Instant,
) -> Result<(), Response> {
    let remaining = limit.saturating_sub(buffer.len());
    if remaining == 0 {
        return Err(Response::error(
            400,
            "Bad Request",
            "incomplete HTTP request",
        ));
    }
    let remaining_time = deadline.saturating_duration_since(Instant::now());
    if remaining_time.is_zero() {
        return Err(Response::error(
            408,
            "Request Timeout",
            "request was not received within 10 seconds",
        ));
    }
    if stream.set_read_timeout(Some(remaining_time)).is_err() {
        return Err(Response::error(
            500,
            "Internal Server Error",
            "could not apply request timeout",
        ));
    }
    let mut chunk = [0_u8; 8192];
    let read_limit = remaining.min(chunk.len());
    match stream.read(&mut chunk[..read_limit]) {
        Ok(0) => Err(Response::error(
            400,
            "Bad Request",
            "connection closed before the request was complete",
        )),
        Ok(count) => {
            buffer.extend_from_slice(&chunk[..count]);
            Ok(())
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ) =>
        {
            Err(Response::error(
                408,
                "Request Timeout",
                "request was not received within 10 seconds",
            ))
        }
        Err(error) => Err(Response::error(
            400,
            "Bad Request",
            &format!("could not read request: {error}"),
        )),
    }
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
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
        })
}

fn route(request: Request, state: &RwLock<Database>, deadline: Instant) -> Response {
    match (request.method.as_str(), request.target.as_str()) {
        ("GET", "/health") => Response::new(
            200,
            "OK",
            "application/json; charset=utf-8",
            b"{\"status\":\"ok\"}\n".to_vec(),
        ),
        ("POST", "/query") => execute_query(request, state, deadline),
        (_, "/health") => Response::error(405, "Method Not Allowed", "GET is required")
            .with_header("Allow", "GET"),
        (_, "/query") => Response::error(405, "Method Not Allowed", "POST is required")
            .with_header("Allow", "POST"),
        _ => Response::error(404, "Not Found", "route not found"),
    }
}

fn execute_query(request: Request, state: &RwLock<Database>, deadline: Instant) -> Response {
    if request.origin.is_some() {
        return Response::error(
            403,
            "Forbidden",
            "browser-originated query requests are not permitted",
        );
    }
    let media_type = request
        .content_type
        .as_deref()
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    if !media_type.is_some_and(|value| value.eq_ignore_ascii_case("application/sql")) {
        return Response::error(
            415,
            "Unsupported Media Type",
            "Content-Type must be application/sql",
        );
    }
    let format = match negotiate_format(request.accept.as_deref()) {
        Some(format) => format,
        None => {
            return Response::error(
                406,
                "Not Acceptable",
                "Accept must allow application/json or text/csv",
            );
        }
    };
    let sql = match String::from_utf8(request.body) {
        Ok(sql) if !sql.trim().is_empty() => sql,
        Ok(_) => return Response::error(400, "Bad Request", "request body must contain SQL"),
        Err(_) => {
            return Response::error(400, "Bad Request", "SQL request body must be valid UTF-8");
        }
    };
    let statements = match sql::parse(&sql) {
        Ok(statements) => statements,
        Err(error) => return Response::error(400, "Bad Request", &error.to_string()),
    };
    if Instant::now() >= deadline {
        return execution_error_response(Error::ExecutionTimeout);
    }
    if let Err(response) = validate_batch_limits(&statements) {
        return response;
    }
    let read_only = statements
        .iter()
        .all(|statement| matches!(statement, Statement::Select(_)));
    let mut budget = ExecutionBudget::new(ExecutionBudgetLimits {
        deadline,
        scanned_rows: MAX_SCANNED_ROWS,
        work_units: MAX_WORK_UNITS,
        groups: MAX_INTERMEDIATE_GROUPS,
        result_rows: MAX_RESULT_ROWS,
        result_cells: MAX_RESULT_CELLS,
        materialized_bytes: MAX_MATERIALIZED_BYTES,
        result_columns: MAX_RESULT_COLUMNS,
    });

    let execution = if read_only {
        let database = loop {
            match state.try_read() {
                Ok(database) => break database,
                Err(std::sync::TryLockError::WouldBlock) => {
                    if let Err(error) = budget.checkpoint() {
                        return execution_error_response(error);
                    }
                    thread::sleep(Duration::from_millis(1));
                }
                Err(std::sync::TryLockError::Poisoned(_)) => return Response::internal_error(),
            }
        };
        execute_batch(statements, format, &mut budget, |statement, budget| {
            database.execute_bounded_read_statement(statement, budget)
        })
    } else {
        let mut database = loop {
            match state.try_write() {
                Ok(database) => break database,
                Err(std::sync::TryLockError::WouldBlock) => {
                    if let Err(error) = budget.checkpoint() {
                        return execution_error_response(error);
                    }
                    thread::sleep(Duration::from_millis(1));
                }
                Err(std::sync::TryLockError::Poisoned(_)) => return Response::internal_error(),
            }
        };
        execute_batch(statements, format, &mut budget, |statement, budget| {
            database.execute_bounded_statement(statement, budget)
        })
    };
    let body = match execution {
        Ok(body) => body,
        Err(error) => return execution_error_response(error),
    };
    let content_type = match format {
        OutputFormat::Csv => "text/csv; charset=utf-8",
        OutputFormat::Json => "application/json; charset=utf-8",
        OutputFormat::Table => unreachable!("HTTP only negotiates JSON and CSV"),
    };
    Response::new(200, "OK", content_type, body)
}

fn validate_batch_limits(statements: &[Statement]) -> Result<(), Response> {
    if statements.len() > MAX_STATEMENTS_PER_BATCH {
        return Err(Response::error(
            422,
            "Unprocessable Content",
            "SQL batch contains more than 32 statements",
        ));
    }
    for statement in statements {
        match statement {
            Statement::CreateTable { columns, .. } if columns.len() > MAX_SCHEMA_COLUMNS => {
                return Err(Response::error(
                    422,
                    "Unprocessable Content",
                    "CREATE TABLE contains more than 1024 columns",
                ));
            }
            Statement::Insert { rows, .. } => {
                let cells = rows
                    .iter()
                    .try_fold(0_usize, |total, row| total.checked_add(row.len()).ok_or(()));
                if !matches!(cells, Ok(cells) if cells <= MAX_INSERT_CELLS) {
                    return Err(Response::error(
                        422,
                        "Unprocessable Content",
                        "INSERT contains more than 100000 values",
                    ));
                }
            }
            Statement::Select(select) => {
                if select.items.len() > MAX_QUERY_ITEMS {
                    return Err(Response::error(
                        422,
                        "Unprocessable Content",
                        "SELECT contains more than 256 output expressions",
                    ));
                }
                if select.group_by.len() > MAX_GROUP_OR_ORDER_COLUMNS
                    || select.order_by.len() > MAX_GROUP_OR_ORDER_COLUMNS
                {
                    return Err(Response::error(
                        422,
                        "Unprocessable Content",
                        "GROUP BY and ORDER BY are limited to 64 columns each",
                    ));
                }
            }
            Statement::CreateTable { .. } => {}
        }
    }
    Ok(())
}

fn execute_batch(
    statements: Vec<Statement>,
    format: OutputFormat,
    budget: &mut ExecutionBudget,
    mut execute: impl FnMut(Statement, &mut ExecutionBudget) -> crate::Result<StatementResult>,
) -> crate::Result<Vec<u8>> {
    let mut output = match format {
        OutputFormat::Json => String::from("{\"results\":["),
        OutputFormat::Csv => String::new(),
        OutputFormat::Table => unreachable!("HTTP only negotiates JSON and CSV"),
    };
    let mut query_count = 0_usize;
    for statement in statements {
        budget.checkpoint()?;
        let result = execute(statement, budget)?;
        let StatementResult::Query(query) = result else {
            continue;
        };
        let separator = usize::from(query_count > 0);
        let closing = usize::from(format == OutputFormat::Json) * 3;
        let estimated = render_size_upper_bound(&query)?;
        let projected_size = output
            .len()
            .checked_add(separator)
            .and_then(|size| size.checked_add(estimated))
            .and_then(|size| size.checked_add(closing))
            .ok_or_else(|| Error::ResourceLimit("encoded response size overflow".to_owned()))?;
        if projected_size > MAX_RESPONSE_BYTES {
            return Err(Error::ResourceLimit(
                "encoded response exceeds 4194304 bytes".to_owned(),
            ));
        }
        if query_count > 0 {
            output.push(if format == OutputFormat::Json {
                ','
            } else {
                '\n'
            });
        }
        let rendered = render(&query, format);
        if output.len() + rendered.len() + closing > MAX_RESPONSE_BYTES {
            return Err(Error::ResourceLimit(
                "encoded response exceeds 4194304 bytes".to_owned(),
            ));
        }
        output.push_str(&rendered);
        query_count += 1;
        budget.checkpoint()?;
    }
    if format == OutputFormat::Json {
        output.push_str("]}\n");
    }
    Ok(output.into_bytes())
}

fn render_size_upper_bound(result: &QueryResult) -> crate::Result<usize> {
    let mut size = 128_usize;
    for column in &result.columns {
        size = size
            .checked_add(64)
            .and_then(|size| size.checked_add(column.name.len().checked_mul(6)?))
            .ok_or_else(|| Error::ResourceLimit("encoded response size overflow".to_owned()))?;
    }
    for row in &result.rows {
        size = size
            .checked_add(16)
            .ok_or_else(|| Error::ResourceLimit("encoded response size overflow".to_owned()))?;
        for value in row {
            let value_size = match value {
                Value::String(value) => value
                    .len()
                    .checked_mul(6)
                    .and_then(|size| size.checked_add(8)),
                Value::Int64(_) | Value::Float64(_) | Value::Bool(_) => Some(40),
            }
            .ok_or_else(|| Error::ResourceLimit("encoded response size overflow".to_owned()))?;
            size = size
                .checked_add(value_size)
                .ok_or_else(|| Error::ResourceLimit("encoded response size overflow".to_owned()))?;
        }
    }
    Ok(size)
}

fn execution_error_response(error: Error) -> Response {
    match error {
        Error::ExecutionTimeout => Response::error(408, "Request Timeout", &error.to_string()),
        Error::ResourceLimit(_) => {
            Response::error(422, "Unprocessable Content", &error.to_string())
        }
        _ => Response::error(400, "Bad Request", &error.to_string()),
    }
}

fn negotiate_format(accept: Option<&str>) -> Option<OutputFormat> {
    let Some(accept) = accept else {
        return Some(OutputFormat::Json);
    };
    let mut json_preference = None;
    let mut csv_preference = None;
    for item in accept.split(',') {
        let mut parts = item.trim().split(';');
        let media_type = parts.next()?.trim().to_ascii_lowercase();
        let mut quality = 1000;
        for parameter in parts {
            let parameter = parameter.trim();
            if let Some((name, value)) = parameter.split_once('=')
                && name.eq_ignore_ascii_case("q")
            {
                quality = parse_quality(value.trim()).unwrap_or(0);
            }
        }
        match media_type.as_str() {
            "application/json" => update_preference(&mut json_preference, 2, quality),
            "text/csv" => update_preference(&mut csv_preference, 2, quality),
            "application/*" => update_preference(&mut json_preference, 1, quality),
            "text/*" => update_preference(&mut csv_preference, 1, quality),
            "*/*" => {
                update_preference(&mut json_preference, 0, quality);
                update_preference(&mut csv_preference, 0, quality);
            }
            _ => {}
        }
    }
    let json_quality = json_preference.map_or(0, |(_, quality)| quality);
    let csv_quality = csv_preference.map_or(0, |(_, quality)| quality);
    match (json_quality, csv_quality) {
        (0, 0) => None,
        (json, csv) if csv > json => Some(OutputFormat::Csv),
        _ => Some(OutputFormat::Json),
    }
}

fn update_preference(preference: &mut Option<(u8, u16)>, specificity: u8, quality: u16) {
    if preference.is_none_or(|(current_specificity, _)| specificity > current_specificity) {
        *preference = Some((specificity, quality));
    }
}

fn parse_quality(value: &str) -> Option<u16> {
    let parsed = value.parse::<f32>().ok()?;
    if !(0.0..=1.0).contains(&parsed) {
        return None;
    }
    Some((parsed * 1000.0).round() as u16)
}

struct Response {
    status: u16,
    reason: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
    headers: Vec<(&'static str, &'static str)>,
}

impl Response {
    fn new(status: u16, reason: &'static str, content_type: &'static str, body: Vec<u8>) -> Self {
        Self {
            status,
            reason,
            content_type,
            body,
            headers: Vec::new(),
        }
    }

    fn error(status: u16, reason: &'static str, message: &str) -> Self {
        let mut body = String::from("{\"error\":");
        write_json_string(&mut body, message);
        body.push_str("}\n");
        Self::new(
            status,
            reason,
            "application/json; charset=utf-8",
            body.into_bytes(),
        )
    }

    fn internal_error() -> Self {
        Self::error(500, "Internal Server Error", "database lock is unavailable")
    }

    fn with_header(mut self, name: &'static str, value: &'static str) -> Self {
        self.headers.push((name, value));
        self
    }
}

fn write_response(stream: &mut TcpStream, response: Response) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n",
        response.status,
        response.reason,
        response.content_type,
        response.body.len()
    )?;
    for (name, value) in response.headers {
        write!(stream, "{name}: {value}\r\n")?;
    }
    stream.write_all(b"\r\n")?;
    stream.write_all(&response.body)
}

fn write_json_string(output: &mut String, value: &str) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            value if value.is_control() => {
                use std::fmt::Write as _;
                write!(output, "\\u{:04x}", value as u32).expect("writing to String cannot fail");
            }
            value => output.push(value),
        }
    }
    output.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiates_supported_response_formats() {
        assert_eq!(negotiate_format(None), Some(OutputFormat::Json));
        assert_eq!(negotiate_format(Some("text/csv")), Some(OutputFormat::Csv));
        assert_eq!(
            negotiate_format(Some("text/csv;q=0.4, application/json;q=0.9")),
            Some(OutputFormat::Json)
        );
        assert_eq!(negotiate_format(Some("text/html")), None);
        assert_eq!(negotiate_format(Some("application/json;q=0")), None);
        assert_eq!(
            negotiate_format(Some("application/json;q=0, */*;q=1")),
            Some(OutputFormat::Csv)
        );
    }

    #[test]
    fn escapes_error_messages_as_json() {
        let response = Response::error(400, "Bad Request", "bad \"SQL\"\nnext");
        assert_eq!(response.body, b"{\"error\":\"bad \\\"SQL\\\"\\nnext\"}\n");
    }

    #[test]
    fn rejects_execution_after_the_request_deadline() {
        let request = Request {
            method: "POST".to_owned(),
            target: "/query".to_owned(),
            accept: None,
            content_type: Some("application/sql".to_owned()),
            origin: None,
            body: b"CREATE TABLE late (id Int64)".to_vec(),
        };
        let response = execute_query(request, &RwLock::new(Database::new()), Instant::now());
        assert_eq!(response.status, 408);
        assert!(response.body.windows(8).any(|value| value == b"deadline"));
    }
}
