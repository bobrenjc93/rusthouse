use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use reqwest::{Client, StatusCode};
use rusthouse::{
    Database, EngineMetricsSnapshot, ObservabilitySnapshot, QueryError, QueryFuture, QueryRequest,
    QueryService, StatementResult, Value,
    http::{ServerConfig, spawn_http_server},
};
use serde_json::Value as JsonValue;
use tokio::sync::Semaphore;

struct BlockingMetricsService {
    started: Semaphore,
    release: AtomicBool,
}

impl BlockingMetricsService {
    fn new() -> Self {
        Self {
            started: Semaphore::new(0),
            release: AtomicBool::new(false),
        }
    }
}

impl QueryService for BlockingMetricsService {
    fn execute(&self, _request: QueryRequest) -> QueryFuture<'_> {
        Box::pin(async { Err(QueryError::unavailable("queries are not used in this test")) })
    }

    fn observability(&self) -> Option<ObservabilitySnapshot> {
        self.started.add_permits(1);
        while !self.release.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(1));
        }
        Some(empty_observability_snapshot())
    }
}

fn empty_observability_snapshot() -> ObservabilitySnapshot {
    ObservabilitySnapshot {
        active_queries: Vec::new(),
        engine_metrics: EngineMetricsSnapshot {
            active_queries: 0,
            tracked_active_queries: 0,
            queries_total: 0,
            queries_succeeded_total: 0,
            queries_failed_total: 0,
            queries_cancelled_total: 0,
            scanned_rows_total: 0,
            scanned_bytes_total: 0,
            peak_memory_bytes: 0,
            spill_bytes_total: 0,
            dropped_active_query_records_total: 0,
        },
    }
}

fn query_rows(database: &Database, sql: &str) -> Vec<Vec<Value>> {
    match database.execute(sql).unwrap() {
        StatementResult::Query(result) => result.rows,
        result => panic!("expected query result, got {result:?}"),
    }
}

#[test]
fn system_catalog_tables_are_queryable_and_read_only() {
    let database = Database::new();
    database
        .execute("CREATE TABLE events (id Int64, label String NULL)")
        .unwrap();
    database
        .execute("INSERT INTO events VALUES (1, 'one'), (2, NULL)")
        .unwrap();

    let tables = query_rows(
        &database,
        "SELECT name, column_count, row_count FROM system.tables WHERE name = 'events'",
    );
    assert_eq!(
        tables,
        vec![vec![
            Value::String("events".into()),
            Value::Int64(2),
            Value::Int64(2),
        ]]
    );

    let columns = query_rows(
        &database,
        "SELECT table, name, ordinal_position, data_type, nullable FROM system.columns WHERE table = 'events'",
    );
    assert_eq!(columns.len(), 2);
    assert_eq!(columns[0][1], Value::String("id".into()));
    assert_eq!(columns[1][1], Value::String("label".into()));
    assert_eq!(columns[1][4], Value::Bool(true));

    let segments = query_rows(
        &database,
        "SELECT table, row_count, logical_bytes FROM system.segments WHERE table = 'events'",
    );
    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0][0], Value::String("events".into()));
    assert_eq!(segments[0][1], Value::Int64(2));
    assert!(matches!(segments[0][2], Value::Int64(bytes) if bytes > 0));

    let error = database
        .execute("INSERT INTO system.engine_metrics VALUES ('fake', 1)")
        .unwrap_err();
    assert!(error.to_string().contains("system tables are read-only"));
}

#[tokio::test]
async fn http_metrics_exposes_the_engine_snapshot() {
    let database = Database::new();
    let server = spawn_http_server(
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(database.clone()),
        ServerConfig::default(),
    )
    .await
    .unwrap();
    let url = format!("http://{}", server.local_addr());
    let client = Client::new();

    let response = client
        .post(format!("{url}/query"))
        .header("content-type", "application/sql")
        .body("SELECT * FROM system.tables")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let response = client.get(format!("{url}/metrics")).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let payload = response.json::<JsonValue>().await.unwrap();
    assert_eq!(payload["active_queries"], serde_json::json!([]));
    assert_eq!(payload["engine_metrics"]["active_queries"], 0);
    assert_eq!(payload["engine_metrics"]["queries_total"], 1);
    assert_eq!(payload["engine_metrics"]["queries_succeeded_total"], 1);

    let rows = query_rows(
        &database,
        "SELECT metric, value FROM system.engine_metrics WHERE metric = 'queries_total'",
    );
    assert_eq!(
        rows,
        vec![vec![Value::String("queries_total".into()), Value::Int64(1)]]
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn http_metrics_enforces_response_size_limit() {
    let config = ServerConfig {
        max_response_bytes: 64,
        ..ServerConfig::default()
    };
    let server = spawn_http_server(
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(Database::new()),
        config,
    )
    .await
    .unwrap();
    let response = Client::new()
        .get(format!("http://{}/metrics", server.local_addr()))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        response.json::<JsonValue>().await.unwrap()["error"]["code"],
        "response_too_large"
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn concurrent_metrics_scrapes_share_query_admission() {
    let service = Arc::new(BlockingMetricsService::new());
    let config = ServerConfig {
        max_concurrent_queries: 1,
        max_concurrent_requests: 2,
        ..ServerConfig::default()
    };
    let server = spawn_http_server("127.0.0.1:0".parse().unwrap(), service.clone(), config)
        .await
        .unwrap();
    let url = format!("http://{}/metrics", server.local_addr());
    let client = Client::new();
    let first_client = client.clone();
    let first_url = url.clone();
    let first = tokio::spawn(async move { first_client.get(first_url).send().await.unwrap() });
    service.started.acquire().await.unwrap().forget();

    let overloaded = client.get(url).send().await.unwrap();
    service.release.store(true, Ordering::Release);
    assert_eq!(overloaded.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        overloaded.json::<JsonValue>().await.unwrap()["error"]["code"],
        "overloaded"
    );
    assert_eq!(first.await.unwrap().status(), StatusCode::OK);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn stalled_metrics_times_out_without_retaining_the_query_slot() {
    let service = Arc::new(BlockingMetricsService::new());
    let config = ServerConfig {
        max_concurrent_queries: 1,
        query_timeout: Duration::from_millis(40),
        ..ServerConfig::default()
    };
    let server = spawn_http_server("127.0.0.1:0".parse().unwrap(), service.clone(), config)
        .await
        .unwrap();
    let base_url = format!("http://{}", server.local_addr());
    let client = Client::new();

    let metrics = client
        .get(format!("{base_url}/metrics"))
        .send()
        .await
        .unwrap();
    assert_eq!(metrics.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(
        metrics.json::<JsonValue>().await.unwrap()["error"]["code"],
        "metrics_timeout"
    );

    let query = client
        .post(format!("{base_url}/query"))
        .header("content-type", "application/sql")
        .body("SELECT * FROM anything")
        .send()
        .await
        .unwrap();
    assert_eq!(query.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        query.json::<JsonValue>().await.unwrap()["error"]["code"],
        "unavailable"
    );

    service.release.store(true, Ordering::Release);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn stalled_metrics_does_not_delay_forced_shutdown() {
    let service = Arc::new(BlockingMetricsService::new());
    let config = ServerConfig {
        query_timeout: Duration::from_secs(30),
        shutdown_timeout: Duration::from_millis(40),
        ..ServerConfig::default()
    };
    let server = spawn_http_server("127.0.0.1:0".parse().unwrap(), service.clone(), config)
        .await
        .unwrap();
    let url = format!("http://{}/metrics", server.local_addr());
    let request = tokio::spawn(async move { Client::new().get(url).send().await });
    service.started.acquire().await.unwrap().forget();

    tokio::time::timeout(Duration::from_secs(1), server.shutdown())
        .await
        .expect("shutdown remained blocked by metrics collection")
        .unwrap();
    service.release.store(true, Ordering::Release);
    let _ = tokio::time::timeout(Duration::from_secs(1), request).await;
}
