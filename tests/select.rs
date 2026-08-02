use rusthouse::{DataType, Database, Error, ExecutionResult, InsertError, QueryResult, Value};

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
