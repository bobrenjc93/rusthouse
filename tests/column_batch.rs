use rusthouse::{Column, ColumnBatch, Database, Error, QueryResult, StatementResult, Value};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    match results.into_iter().next().expect("statement result") {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

fn metrics_batch(ids: Vec<i64>, scores: Vec<f64>, labels: &[&str]) -> ColumnBatch {
    ColumnBatch::new(vec![
        Column::Int64(ids),
        Column::Float64(scores),
        Column::String(labels.iter().map(|label| (*label).to_owned()).collect()),
    ])
}

#[test]
fn empty_batches_and_repeated_chunks_append_in_order() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE metrics (id Int64, score Float64, label String);")
        .expect("create table");

    assert_eq!(
        database
            .insert_batch("metrics", metrics_batch(vec![], vec![], &[]))
            .expect("empty batch is valid"),
        0
    );
    assert_eq!(
        database
            .insert_batch(
                "metrics",
                metrics_batch(vec![1, 2], vec![1.5, 2.5], &["one", "two"]),
            )
            .expect("first chunk"),
        2
    );
    assert_eq!(
        database
            .insert_batch("METRICS", metrics_batch(vec![3], vec![3.5], &["three"]))
            .expect("second chunk"),
        1
    );

    let result = query(&mut database, "SELECT * FROM metrics ORDER BY id;");
    assert_eq!(
        result.rows,
        vec![
            vec![
                Value::Int64(1),
                Value::Float64(1.5),
                Value::String("one".to_owned()),
            ],
            vec![
                Value::Int64(2),
                Value::Float64(2.5),
                Value::String("two".to_owned()),
            ],
            vec![
                Value::Int64(3),
                Value::Float64(3.5),
                Value::String("three".to_owned()),
            ],
        ]
    );
}

#[test]
fn invalid_batches_leave_all_existing_columns_unchanged() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE metrics (id Int64, score Float64, label String);")
        .expect("create table");
    database
        .insert_batch("metrics", metrics_batch(vec![1], vec![1.5], &["existing"]))
        .expect("seed row");

    let width = database
        .insert_batch("metrics", ColumnBatch::new(vec![Column::Int64(vec![2])]))
        .expect_err("batch is too narrow");
    assert!(matches!(
        width,
        Error::BatchWidth {
            expected: 3,
            actual: 1,
            ..
        }
    ));

    let column_type = database
        .insert_batch(
            "metrics",
            ColumnBatch::new(vec![
                Column::Bool(vec![true]),
                Column::Float64(vec![2.5]),
                Column::String(vec!["wrong type".to_owned()]),
            ]),
        )
        .expect_err("column type does not match schema");
    assert!(matches!(
        column_type,
        Error::TypeMismatch {
            expected,
            actual,
            ..
        } if expected == "Int64" && actual == "Bool"
    ));

    let lengths = database
        .insert_batch(
            "metrics",
            metrics_batch(vec![2, 3], vec![2.5], &["two", "three"]),
        )
        .expect_err("column lengths differ");
    assert!(matches!(
        lengths,
        Error::ColumnLength {
            column,
            expected: 2,
            actual: 1,
            ..
        } if column == "score"
    ));

    for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let float = database
            .insert_batch(
                "metrics",
                metrics_batch(vec![2], vec![invalid], &["not finite"]),
            )
            .expect_err("non-finite float");
        assert!(matches!(float, Error::InvalidQuery(message) if message.contains("non-finite")));
    }

    let result = query(&mut database, "SELECT * FROM metrics;");
    assert_eq!(
        result.rows,
        vec![vec![
            Value::Int64(1),
            Value::Float64(1.5),
            Value::String("existing".to_owned()),
        ]]
    );
    let table = database.catalog().table("metrics").expect("table exists");
    assert_eq!(table.row_count(), 1);
    assert!(table.columns().iter().all(|column| column.len() == 1));
}
