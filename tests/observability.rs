use std::sync::Arc;

use reqwest::{Client, StatusCode};
use rusthouse::{
    Database, StatementResult, Value,
    http::{ServerConfig, spawn_http_server},
};
use serde_json::Value as JsonValue;

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
