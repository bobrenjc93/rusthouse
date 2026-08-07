use rusthouse::batch::engine::{Database, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::Statement;
use rusthouse::batch::value::Value;

fn query_rows(database: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    let results = database.execute(sql).expect("query succeeds");
    let StatementResult::Query(result) = results.into_iter().next().expect("one result") else {
        panic!("expected query result");
    };
    result.rows
}

#[test]
fn original_public_positional_insert_ast_shape_remains_executable() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE events (id Int64, label String);")
        .expect("create target");

    assert_eq!(
        database.execute_statement(Statement::Insert {
            table: "events".to_owned(),
            rows: vec![vec![Value::Int64(7), Value::String("seven".to_owned()),]],
        }),
        Ok(StatementResult::Command {
            tag: "INSERT",
            affected_rows: 1,
        })
    );
    assert_eq!(
        query_rows(&mut database, "SELECT id, label FROM events;"),
        [vec![Value::Int64(7), Value::String("seven".to_owned()),]]
    );
}

#[test]
fn complete_insert_columns_reorder_every_type_and_preserve_positional_insert() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE metrics (id Int64, score Float64, active Bool, label String); \
             INSERT INTO metrics (LABEL, ACTIVE, Score, ID) VALUES \
                 ('first', true, 1.5, 1), \
                 ('second', false, 2.5, 2); \
             INSERT INTO metrics VALUES (3, 3.5, true, 'third');",
        )
        .expect("named and positional INSERT forms succeed");

    assert_eq!(
        query_rows(
            &mut database,
            "SELECT id, score, active, label FROM metrics ORDER BY id;",
        ),
        [
            vec![
                Value::Int64(1),
                Value::Float64(1.5),
                Value::Bool(true),
                Value::String("first".to_owned()),
            ],
            vec![
                Value::Int64(2),
                Value::Float64(2.5),
                Value::Bool(false),
                Value::String("second".to_owned()),
            ],
            vec![
                Value::Int64(3),
                Value::Float64(3.5),
                Value::Bool(true),
                Value::String("third".to_owned()),
            ],
        ]
    );
}

#[test]
fn explicit_insert_columns_reject_duplicates_omissions_unknowns_and_bad_rows() {
    let cases = [
        (
            "INSERT INTO metrics (id, ID, active, label) VALUES (1, 2.5, true, 'x');",
            Error::DuplicateColumn("ID".to_owned()),
        ),
        (
            "INSERT INTO metrics (id, score, active) VALUES (1, 2.5, true);",
            Error::MissingInsertColumn {
                table: "metrics".to_owned(),
                column: "label".to_owned(),
            },
        ),
        (
            "INSERT INTO metrics (id, score, active, missing) VALUES (1, 2.5, true, 'x');",
            Error::ColumnNotFound {
                table: "metrics".to_owned(),
                column: "missing".to_owned(),
            },
        ),
        (
            "INSERT INTO metrics (label, active, score, id) VALUES ('x', true, 2.5);",
            Error::RowLength {
                table: "metrics".to_owned(),
                expected: 4,
                actual: 3,
            },
        ),
    ];

    for (insert, expected) in cases {
        let mut database = Database::new();
        database
            .execute("CREATE TABLE metrics (id Int64, score Float64, active Bool, label String);")
            .expect("create target");
        assert_eq!(database.execute(insert), Err(expected), "{insert}");
        assert_eq!(database.catalog().table("metrics").unwrap().row_count(), 0);
    }
}

#[test]
fn named_insert_batch_reorders_before_validation_and_rolls_back_every_target() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE events (id Int64, label String); \
             CREATE TABLE metrics (id Int64, score Float64, active Bool, label String);",
        )
        .expect("create targets");

    assert_eq!(
        database.execute_insert_batch(
            "INSERT INTO events (label, id) VALUES ('kept only on success', 1); \
             INSERT INTO metrics (label, active, score, id) \
                 VALUES ('bad', true, 2.5, 'not an integer');",
        ),
        Err(Error::TypeMismatch {
            context: "column 'metrics.id'".to_owned(),
            expected: "Int64".to_owned(),
            actual: "String".to_owned(),
        })
    );
    assert_eq!(database.catalog().table("events").unwrap().row_count(), 0);
    assert_eq!(database.catalog().table("metrics").unwrap().row_count(), 0);

    database
        .execute_insert_batch(
            "INSERT INTO events (LABEL, ID) VALUES ('one', 1), ('two', 2); \
             INSERT INTO metrics (active, label, id, score) \
                 VALUES (true, 'metric', 3, 3.5);",
        )
        .expect("valid named INSERT batch commits");
    assert_eq!(database.catalog().table("events").unwrap().row_count(), 2);
    assert_eq!(database.catalog().table("metrics").unwrap().row_count(), 1);
}
