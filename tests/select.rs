use rusthouse::{
    DataType, Database, DatabaseConfig, Error, ExecutionResult, InsertError, QueryResult, Value,
};

fn query(result: ExecutionResult) -> QueryResult {
    match result {
        ExecutionResult::Query(result) => result,
        other => panic!("expected query result, received {other:?}"),
    }
}

#[test]
fn selects_all_typed_rows_in_insertion_order() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE events (id Int64, score Float64, active Bool, label String)")
        .expect("create table");
    assert_eq!(
        database
            .execute(
                "INSERT INTO events VALUES \
                 (3, 1.5, true, 'third'), \
                 (1, -2.25, false, 'first'), \
                 (2, 0.0, true, 'second')",
            )
            .expect("insert rows"),
        ExecutionResult::InsertedRows(3)
    );

    let result = query(
        database
            .execute("SELECT * FROM EVENTS")
            .expect("select rows"),
    );
    assert_eq!(
        result
            .columns()
            .iter()
            .map(|column| (column.name(), column.data_type()))
            .collect::<Vec<_>>(),
        vec![
            ("id", DataType::Int64),
            ("score", DataType::Float64),
            ("active", DataType::Bool),
            ("label", DataType::String),
        ]
    );
    assert_eq!(
        result.rows(),
        [
            vec![
                Value::Int64(3),
                Value::Float64(1.5),
                Value::Bool(true),
                Value::String("third".to_owned()),
            ],
            vec![
                Value::Int64(1),
                Value::Float64(-2.25),
                Value::Bool(false),
                Value::String("first".to_owned()),
            ],
            vec![
                Value::Int64(2),
                Value::Float64(0.0),
                Value::Bool(true),
                Value::String("second".to_owned()),
            ],
        ]
    );
}

#[test]
fn explicit_projection_preserves_requested_column_order() {
    let mut database = Database::new();
    database
        .execute_batch(
            "CREATE TABLE events (id Int64, score Float64, active Bool, label String); \
             INSERT INTO events VALUES (7, 4.25, true, 'selected');",
        )
        .expect("setup batch");

    let result = query(
        database
            .execute("SELECT LABEL, id, score FROM Events")
            .expect("explicit projection"),
    );
    assert_eq!(
        result
            .columns()
            .iter()
            .map(|column| column.name())
            .collect::<Vec<_>>(),
        ["label", "id", "score"]
    );
    assert_eq!(
        result.rows(),
        [vec![
            Value::String("selected".to_owned()),
            Value::Int64(7),
            Value::Float64(4.25),
        ]]
    );
}

#[test]
fn reports_lookup_and_insert_failures_as_typed_errors() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE events (id Int64, active Bool)")
        .expect("create table");

    assert_eq!(
        database.execute("INSERT INTO events VALUES ('wrong', true)"),
        Err(Error::Insert(InsertError::TypeMismatch {
            row: 0,
            column: 0,
            column_name: "id".to_owned(),
            expected: DataType::Int64,
            actual: DataType::String,
        }))
    );
    assert_eq!(
        database.execute("SELECT missing FROM events"),
        Err(Error::ColumnNotFound {
            table: "events".to_owned(),
            column: "missing".to_owned(),
        })
    );
    assert_eq!(
        database.execute("SELECT * FROM absent"),
        Err(Error::TableNotFound {
            name: "absent".to_owned(),
        })
    );

    let result = query(
        database
            .execute("SELECT * FROM events")
            .expect("select table"),
    );
    assert!(result.is_empty(), "failed insert must be atomic");
}

#[test]
fn bounds_projection_width_and_materialized_result_cells() {
    let config = DatabaseConfig::new(4096, 2).with_result_limits(4, 8);
    let mut database = Database::with_config(config);
    database
        .execute_batch(
            "CREATE TABLE events (id Int64); \
             INSERT INTO events VALUES (1), (2);",
        )
        .expect("setup batch");

    let boundary = query(
        database
            .execute("SELECT id, id FROM events")
            .expect("four cells are allowed"),
    );
    assert_eq!(boundary.cell_count(), 4);

    database
        .execute("INSERT INTO events VALUES (3)")
        .expect("third row");
    assert_eq!(
        database.execute("SELECT id, id FROM events"),
        Err(Error::ResultTooLarge {
            actual: 6,
            maximum: 4,
        })
    );
    assert_eq!(
        database.execute("SELECT id, id, id FROM events"),
        Err(Error::TooManyProjectedColumns {
            actual: 3,
            maximum: 2,
        })
    );
}

#[test]
fn collecting_batches_are_cumulative_but_streaming_batches_do_not_retain() {
    let config = DatabaseConfig::new(4096, 2).with_result_limits(4, 2);
    let mut database = Database::with_config(config);
    database
        .execute_batch(
            "CREATE TABLE events (id Int64); \
             INSERT INTO events VALUES (1);",
        )
        .expect("setup batch");

    let collected = database
        .execute_batch("SELECT id FROM events; SELECT id FROM events")
        .expect("two retained cells are allowed");
    assert_eq!(collected.len(), 2);
    assert_eq!(
        database
            .execute_batch("SELECT id FROM events; SELECT id FROM events; SELECT id FROM events"),
        Err(Error::BatchResultTooLarge {
            actual: 3,
            maximum: 2,
        })
    );

    let streamed = database
        .execute_batch_iter("SELECT id FROM events; SELECT id FROM events; SELECT id FROM events")
        .expect("parse streaming batch");
    let mut streamed_count = 0;
    for result in streamed {
        result.expect("streaming results are individually bounded");
        streamed_count += 1;
    }
    assert_eq!(streamed_count, 3);
}

#[test]
fn collecting_batches_preflight_multiple_results_against_remaining_bytes() {
    let payload = "x".repeat(4096);
    let setup = format!(
        "CREATE TABLE events (label String); \
         INSERT INTO events VALUES ('{payload}');"
    );

    let mut probe = Database::with_config(DatabaseConfig::new(8192, 2));
    probe.execute_batch(&setup).expect("probe setup");
    let one_result_bytes = query(
        probe
            .execute("SELECT label FROM events")
            .expect("individually valid result"),
    )
    .materialized_bytes();

    let config = DatabaseConfig::new(8192, 2)
        .with_result_limits(4, 4)
        .with_result_byte_limits(one_result_bytes, one_result_bytes);
    let mut database = Database::with_config(config);
    database.execute_batch(&setup).expect("bounded setup");

    let error = database
        .execute_batch("SELECT label FROM events; SELECT label FROM events")
        .expect_err("the second result exceeds the remaining batch bytes");
    assert_eq!(
        error,
        Error::BatchResultBytesTooLarge {
            actual: one_result_bytes.saturating_mul(2),
            maximum: one_result_bytes,
        }
    );
}

#[test]
fn streaming_batch_size_hint_is_conservative_before_an_early_failure() {
    let mut database = Database::new();
    let mut results = database
        .execute_batch_iter(
            "SELECT * FROM missing; \
             CREATE TABLE later (id Int64); \
             SELECT * FROM later",
        )
        .expect("parse batch");

    assert_eq!(results.size_hint(), (0, Some(3)));
    assert_eq!(
        results.next(),
        Some(Err(Error::TableNotFound {
            name: "missing".to_owned(),
        }))
    );
    assert_eq!(results.size_hint(), (0, Some(0)));
    assert_eq!(results.next(), None);
    assert_eq!(results.next(), None, "iterator remains fused");
}
