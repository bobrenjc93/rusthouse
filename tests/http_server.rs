use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use reqwest::{Client, StatusCode};
use rusthouse::{
    QueryCancellation, QueryError, QueryFuture, QueryRequest, QueryResult, QueryService,
    QueryValue, ServiceHealth,
    http::{ServerConfig, ServerHandle, spawn_http_server},
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{Barrier, Semaphore},
};
use tokio_util::sync::CancellationToken;

struct TestService {
    active: AtomicUsize,
    max_active: AtomicUsize,
    started: Arc<Semaphore>,
    release: CancellationToken,
    barrier: Option<Arc<Barrier>>,
    cancellations: Mutex<Vec<QueryCancellation>>,
    ready: bool,
}

impl TestService {
    fn new() -> Self {
        Self {
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
            started: Arc::new(Semaphore::new(0)),
            release: CancellationToken::new(),
            barrier: None,
            cancellations: Mutex::new(Vec::new()),
            ready: true,
        }
    }

    fn with_barrier(count: usize) -> Self {
        Self {
            barrier: Some(Arc::new(Barrier::new(count))),
            ..Self::new()
        }
    }

    async fn wait_for_starts(&self, count: usize) {
        for _ in 0..count {
            self.started.acquire().await.unwrap().forget();
        }
    }

    fn last_cancellation(&self) -> QueryCancellation {
        self.cancellations.lock().unwrap().last().unwrap().clone()
    }
}

struct ActiveQuery<'a>(&'a AtomicUsize);

impl Drop for ActiveQuery<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl QueryService for TestService {
    fn execute(&self, request: QueryRequest) -> QueryFuture<'_> {
        self.cancellations
            .lock()
            .unwrap()
            .push(request.cancellation.clone());

        Box::pin(async move {
            let current = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(current, Ordering::SeqCst);
            let _active = ActiveQuery(&self.active);
            self.started.add_permits(1);

            match request.sql.trim() {
                "hold" => {
                    tokio::select! {
                        () = self.release.cancelled() => rows_result(),
                        () = request.cancellation.cancelled() => {
                            Err(QueryError::unavailable("query was cancelled"))
                        }
                    }
                }
                "concurrent" => {
                    self.barrier.as_ref().unwrap().wait().await;
                    rows_result()
                }
                "large" => Ok(QueryResult::new(
                    vec!["value".into()],
                    vec![vec![QueryValue::String("x".repeat(1024))]],
                )),
                "bad sql" => Err(QueryError::invalid_query("test syntax error")),
                _ => rows_result(),
            }
        })
    }

    fn health(&self) -> ServiceHealth {
        if self.ready {
            ServiceHealth::ready()
        } else {
            ServiceHealth::not_ready("test service is unavailable")
        }
    }
}

fn rows_result() -> Result<QueryResult, QueryError> {
    Ok(QueryResult::new(
        vec![
            "id".into(),
            "name".into(),
            "enabled".into(),
            "missing".into(),
        ],
        vec![
            vec![
                QueryValue::Int64(1),
                QueryValue::String("alpha,beta".into()),
                QueryValue::Boolean(true),
                QueryValue::Null,
            ],
            vec![
                QueryValue::Int64(2),
                QueryValue::String("gamma".into()),
                QueryValue::Boolean(false),
                QueryValue::Null,
            ],
        ],
    ))
}

async fn start(service: Arc<TestService>, config: ServerConfig) -> (ServerHandle, String) {
    let handle = spawn_http_server("127.0.0.1:0".parse().unwrap(), service, config)
        .await
        .unwrap();
    let url = format!("http://{}", handle.local_addr());
    (handle, url)
}

#[tokio::test]
async fn negotiates_formats_and_returns_structured_health_and_errors() {
    let service = Arc::new(TestService::new());
    let (server, url) = start(service, ServerConfig::default()).await;
    let client = Client::new();

    let health = client
        .get(format!("{url}/health/ready"))
        .send()
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);
    assert!(health.headers().contains_key("x-request-id"));
    assert_eq!(
        health.json::<Value>().await.unwrap(),
        json!({"status": "ok"})
    );

    let json_response = client
        .post(format!("{url}/query"))
        .json(&json!({"query": "rows"}))
        .send()
        .await
        .unwrap();
    assert_eq!(json_response.status(), StatusCode::OK);
    assert_eq!(
        json_response.json::<Value>().await.unwrap(),
        json!([
            {"id": 1, "name": "alpha,beta", "enabled": true, "missing": null},
            {"id": 2, "name": "gamma", "enabled": false, "missing": null}
        ])
    );

    let csv = client
        .post(format!("{url}/query?format=csv"))
        .header("content-type", "application/sql")
        .body("rows")
        .send()
        .await
        .unwrap();
    assert_eq!(csv.headers()["content-type"], "text/csv; charset=utf-8");
    assert_eq!(
        csv.text().await.unwrap(),
        "id,name,enabled,missing\n1,\"alpha,beta\",true,\n2,gamma,false,\n"
    );

    let ndjson = client
        .post(format!("{url}/query"))
        .header("accept", "application/x-ndjson")
        .body("rows")
        .send()
        .await
        .unwrap();
    assert_eq!(
        ndjson.text().await.unwrap(),
        concat!(
            "{\"id\":1,\"name\":\"alpha,beta\",\"enabled\":true,\"missing\":null}\n",
            "{\"id\":2,\"name\":\"gamma\",\"enabled\":false,\"missing\":null}\n"
        )
    );

    let error = client
        .post(format!("{url}/query"))
        .body("bad sql")
        .send()
        .await
        .unwrap();
    assert_eq!(error.status(), StatusCode::BAD_REQUEST);
    let request_id = error.headers()["x-request-id"].to_str().unwrap().to_owned();
    let body = error.json::<Value>().await.unwrap();
    assert_eq!(body["error"]["code"], "invalid_query");
    assert_eq!(body["error"]["message"], "test syntax error");
    assert_eq!(body["error"]["request_id"].to_string(), request_id);

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn serves_queries_concurrently() {
    const CLIENTS: usize = 8;
    let service = Arc::new(TestService::with_barrier(CLIENTS));
    let config = ServerConfig {
        max_concurrent_queries: CLIENTS,
        ..ServerConfig::default()
    };
    let (server, url) = start(service.clone(), config).await;
    let client = Client::new();

    let mut requests = Vec::new();
    for _ in 0..CLIENTS {
        let client = client.clone();
        let endpoint = format!("{url}/query");
        requests.push(tokio::spawn(async move {
            client
                .post(endpoint)
                .body("concurrent")
                .send()
                .await
                .unwrap()
        }));
    }
    for request in requests {
        assert_eq!(request.await.unwrap().status(), StatusCode::OK);
    }
    assert_eq!(service.max_active.load(Ordering::SeqCst), CLIENTS);

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn rejects_excess_work_without_queueing() {
    let service = Arc::new(TestService::new());
    let config = ServerConfig {
        max_concurrent_queries: 2,
        ..ServerConfig::default()
    };
    let (server, url) = start(service.clone(), config).await;
    let client = Client::new();

    let mut running = Vec::new();
    for _ in 0..2 {
        let client = client.clone();
        let endpoint = format!("{url}/query");
        running.push(tokio::spawn(async move {
            client.post(endpoint).body("hold").send().await.unwrap()
        }));
    }
    service.wait_for_starts(2).await;

    let rejected = client
        .post(format!("{url}/query"))
        .body("rows")
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(rejected.headers()["retry-after"], "1");
    assert_eq!(
        rejected.json::<Value>().await.unwrap()["error"]["code"],
        "overloaded"
    );

    service.release.cancel();
    for request in running {
        assert_eq!(request.await.unwrap().status(), StatusCode::OK);
    }
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn enforces_deadline_and_signals_cancellation() {
    let service = Arc::new(TestService::new());
    let config = ServerConfig {
        query_timeout: Duration::from_millis(40),
        ..ServerConfig::default()
    };
    let (server, url) = start(service.clone(), config).await;

    let response = Client::new()
        .post(format!("{url}/query"))
        .body("hold")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["code"],
        "query_timeout"
    );
    assert!(service.last_cancellation().is_cancelled());

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn enforces_request_and_encoded_response_limits() {
    let service = Arc::new(TestService::new());
    let config = ServerConfig {
        max_request_bytes: 8,
        max_response_bytes: 128,
        ..ServerConfig::default()
    };
    let (server, url) = start(service, config).await;
    let client = Client::new();

    let request_error = client
        .post(format!("{url}/query"))
        .body("a query that is too long")
        .send()
        .await
        .unwrap();
    assert_eq!(request_error.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        request_error.json::<Value>().await.unwrap()["error"]["code"],
        "request_too_large"
    );

    let response_error = client
        .post(format!("{url}/query"))
        .body("large")
        .send()
        .await
        .unwrap();
    assert_eq!(response_error.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        response_error.json::<Value>().await.unwrap()["error"]["code"],
        "response_too_large"
    );

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn keeps_http_1_connection_alive_for_multiple_queries() {
    let service = Arc::new(TestService::new());
    let (server, _) = start(service, ServerConfig::default()).await;
    let mut connection = TcpStream::connect(server.local_addr()).await.unwrap();
    let request = concat!(
        "POST /query HTTP/1.1\r\n",
        "Host: localhost\r\n",
        "Content-Type: text/plain\r\n",
        "Content-Length: 4\r\n",
        "\r\n",
        "rows"
    );

    connection.write_all(request.as_bytes()).await.unwrap();
    let first = read_http_response(&mut connection).await;
    assert!(first.starts_with("HTTP/1.1 200 OK\r\n"));

    connection.write_all(request.as_bytes()).await.unwrap();
    let second = read_http_response(&mut connection).await;
    assert!(second.starts_with("HTTP/1.1 200 OK\r\n"));

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn client_disconnect_cancels_in_flight_query() {
    let service = Arc::new(TestService::new());
    let (server, _) = start(service.clone(), ServerConfig::default()).await;
    let mut connection = TcpStream::connect(server.local_addr()).await.unwrap();
    connection
        .write_all(
            concat!(
                "POST /query HTTP/1.1\r\n",
                "Host: localhost\r\n",
                "Content-Length: 4\r\n",
                "\r\n",
                "hold"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    service.wait_for_starts(1).await;
    let cancellation = service.last_cancellation();
    drop(connection);

    tokio::time::timeout(Duration::from_secs(1), async {
        while !cancellation.is_cancelled() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("disconnect should cancel the handler");

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_is_bounded_and_cancels_work_after_grace_period() {
    let service = Arc::new(TestService::new());
    let config = ServerConfig {
        query_timeout: Duration::from_secs(30),
        shutdown_timeout: Duration::from_millis(40),
        ..ServerConfig::default()
    };
    let (server, url) = start(service.clone(), config).await;
    let request = tokio::spawn(async move {
        Client::new()
            .post(format!("{url}/query"))
            .body("hold")
            .send()
            .await
    });
    service.wait_for_starts(1).await;
    let cancellation = service.last_cancellation();

    let started = Instant::now();
    server.shutdown().await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(cancellation.is_cancelled());
    let _ = request.await;
}

async fn read_http_response(stream: &mut TcpStream) -> String {
    let mut response = Vec::new();
    while !response.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).await.unwrap();
        response.push(byte[0]);
    }
    let headers = String::from_utf8(response.clone()).unwrap();
    let content_length = headers
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length: ")
                .and_then(|value| value.parse::<usize>().ok())
        })
        .unwrap();
    let mut body = vec![0; content_length];
    stream.read_exact(&mut body).await.unwrap();
    response.extend_from_slice(&body);
    String::from_utf8(response).unwrap()
}
