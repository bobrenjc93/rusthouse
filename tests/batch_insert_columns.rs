use rusthouse::batch::engine::{Database, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::value::Value;

fn query_rows(database: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    let results = database.execute(sql).expect("query succeeds");
    let Some(StatementResult::Query(result)) = results.last() else {
        panic!("expected query result");
    };
    result.rows.clone()
}

#[test]
fn complete_case_insensitive_column_lists_reorder_all_types_and_preserve_positional_insert() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE metrics (id Int64, score Float64, active Bool, label String); \
             INSERT INTO metrics (LABEL, active, ID, ScOrE) VALUES \
                 ('first', true, 1, 2.5), ('second', false, 2, 4.0); \
             INSERT INTO metrics VALUES (3, 8.0, true, 'third');",
        )
        .expect("named and positional inserts succeed");

    assert_eq!(
        query_rows(
            &mut database,
            "SELECT id, score, active, label FROM metrics ORDER BY id",
        ),
        vec![
            vec![
                Value::Int64(1),
                Value::Float64(2.5),
                Value::Bool(true),
                Value::String("first".to_owned()),
            ],
            vec![
                Value::Int64(2),
                Value::Float64(4.0),
                Value::Bool(false),
                Value::String("second".to_owned()),
            ],
            vec![
                Value::Int64(3),
                Value::Float64(8.0),
                Value::Bool(true),
                Value::String("third".to_owned()),
            ],
        ]
    );
}

#[test]
fn duplicate_omitted_unknown_and_reordered_type_errors_do_not_insert_rows() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE metrics (id Int64, score Float64, active Bool, label String)")
        .expect("setup");

    let cases = [
        (
            "INSERT INTO metrics (id, ID, active, label) VALUES (1, 2, true, 'x')",
            Error::DuplicateColumn("ID".to_owned()),
        ),
        (
            "INSERT INTO metrics (id, score, active) VALUES (1, 2.0, true)",
            Error::MissingInsertColumn {
                table: "metrics".to_owned(),
                column: "label".to_owned(),
            },
        ),
        (
            "INSERT INTO metrics (id, score, active, unknown) VALUES (1, 2.0, true, 'x')",
            Error::ColumnNotFound {
                table: "metrics".to_owned(),
                column: "unknown".to_owned(),
            },
        ),
        (
            "INSERT INTO metrics (label, active, score, id) VALUES ('x', true, false, 1)",
            Error::TypeMismatch {
                context: "column 'metrics.score'".to_owned(),
                expected: "Float64".to_owned(),
                actual: "Bool".to_owned(),
            },
        ),
    ];

    for (sql, expected) in cases {
        assert_eq!(database.execute(sql).expect_err(sql), expected, "{sql}");
        assert_eq!(database.catalog().table("metrics").unwrap().row_count(), 0);
    }
}

#[test]
fn atomic_insert_batches_reorder_named_rows_and_roll_back_on_column_list_errors() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE events (id Int64, label String); \
             CREATE TABLE metrics (active Bool, score Float64);",
        )
        .expect("setup");

    database
        .execute_insert_batch(
            "INSERT INTO events (label, ID) VALUES ('one', 1), ('two', 2); \
             INSERT INTO metrics (SCORE, active) VALUES (2.5, true);",
        )
        .expect("complete named rows commit atomically");

    let error = database
        .execute_insert_batch(
            "INSERT INTO events (label, id) VALUES ('three', 3); \
             INSERT INTO metrics (active) VALUES (false);",
        )
        .expect_err("an omitted column rolls back the complete batch");
    assert_eq!(
        error,
        Error::MissingInsertColumn {
            table: "metrics".to_owned(),
            column: "score".to_owned(),
        }
    );

    assert_eq!(
        query_rows(&mut database, "SELECT id, label FROM events ORDER BY id"),
        vec![
            vec![Value::Int64(1), Value::String("one".to_owned())],
            vec![Value::Int64(2), Value::String("two".to_owned())],
        ]
    );
    assert_eq!(database.catalog().table("metrics").unwrap().row_count(), 1);
}
