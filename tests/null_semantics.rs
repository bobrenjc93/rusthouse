use rusthouse::{DataType, Database, Error, QueryResult, StatementResult, Value};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("SQL succeeds");
    match results.into_iter().last().expect("statement result") {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

#[test]
fn every_scalar_type_round_trips_null_and_rejects_null_for_required_columns() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE scalars (
                i Nullable(Int64), f Nullable(Float64),
                b Nullable(Bool), s Nullable(String)
             );
             INSERT INTO scalars VALUES
                (1, 1.5, true, 'one'),
                (NULL, NULL, NULL, NULL);",
        )
        .expect("nullable inserts succeed");

    let result = query(&mut database, "SELECT * FROM scalars ORDER BY i;");
    assert_eq!(
        result
            .columns
            .iter()
            .map(|column| column.data_type)
            .collect::<Vec<_>>(),
        vec![
            DataType::Int64.nullable().unwrap(),
            DataType::Float64.nullable().unwrap(),
            DataType::Bool.nullable().unwrap(),
            DataType::String.nullable().unwrap(),
        ]
    );
    assert_eq!(
        result.rows,
        vec![
            vec![
                Value::Int64(1),
                Value::Float64(1.5),
                Value::Bool(true),
                Value::String("one".to_owned()),
            ],
            vec![Value::Null, Value::Null, Value::Null, Value::Null],
        ]
    );

    database
        .execute("CREATE TABLE required (id Int64);")
        .expect("create succeeds");
    let error = database
        .execute("INSERT INTO required VALUES (1), (NULL), (2);")
        .expect_err("NULL is rejected for a non-nullable column");
    assert!(matches!(
        error,
        Error::TypeMismatch { expected, actual, .. }
            if expected == "Int64" && actual == "NULL"
    ));
    assert_eq!(
        query(&mut database, "SELECT COUNT(*) FROM required;").rows,
        vec![vec![Value::Int64(0)]]
    );
}

#[test]
fn predicates_follow_sql_three_valued_logic() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE truth (id Int64, n Nullable(Int64), flag Nullable(Bool));
             INSERT INTO truth VALUES
                (1, NULL, true),
                (2, 1, NULL),
                (3, NULL, false),
                (4, 1, true);",
        )
        .expect("setup succeeds");

    assert_eq!(
        query(
            &mut database,
            "SELECT id FROM truth WHERE n = 1 OR flag = true ORDER BY id;"
        )
        .rows,
        vec![
            vec![Value::Int64(1)],
            vec![Value::Int64(2)],
            vec![Value::Int64(4)],
        ]
    );
    assert!(
        query(
            &mut database,
            "SELECT id FROM truth WHERE n = 1 AND flag = false;"
        )
        .rows
        .is_empty()
    );
    assert!(
        query(&mut database, "SELECT id FROM truth WHERE n = NULL;")
            .rows
            .is_empty()
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT id FROM truth WHERE n IS NULL ORDER BY id;"
        )
        .rows,
        vec![vec![Value::Int64(1)], vec![Value::Int64(3)]]
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT id FROM truth WHERE n IS NOT NULL ORDER BY id;"
        )
        .rows,
        vec![vec![Value::Int64(2)], vec![Value::Int64(4)]]
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT COUNT(*) FROM truth WHERE NULL IS NULL AND NULL IS NOT NULL OR flag = true;"
        )
        .rows,
        vec![vec![Value::Int64(2)]]
    );
}

#[test]
fn grouping_ordering_and_aggregates_are_null_aware() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE readings (category Nullable(String), value Nullable(Int64));
             INSERT INTO readings VALUES
                ('b', 2), (NULL, NULL), ('a', 1), (NULL, NULL), ('a', NULL);",
        )
        .expect("setup succeeds");

    assert_eq!(
        query(
            &mut database,
            "SELECT value FROM readings ORDER BY value DESC;"
        )
        .rows,
        vec![
            vec![Value::Int64(2)],
            vec![Value::Int64(1)],
            vec![Value::Null],
            vec![Value::Null],
            vec![Value::Null],
        ]
    );

    let grouped = query(
        &mut database,
        "SELECT category, COUNT(*) AS rows, COUNT(value) AS present,
                SUM(value) AS total, MIN(value) AS low, MAX(value) AS high, AVG(value) AS mean
         FROM readings GROUP BY category ORDER BY category;",
    );
    assert_eq!(
        grouped.rows,
        vec![
            vec![
                Value::String("a".to_owned()),
                Value::Int64(2),
                Value::Int64(1),
                Value::Int64(1),
                Value::Int64(1),
                Value::Int64(1),
                Value::Float64(1.0),
            ],
            vec![
                Value::String("b".to_owned()),
                Value::Int64(1),
                Value::Int64(1),
                Value::Int64(2),
                Value::Int64(2),
                Value::Int64(2),
                Value::Float64(2.0),
            ],
            vec![
                Value::Null,
                Value::Int64(2),
                Value::Int64(0),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ],
        ]
    );

    let empty = query(
        &mut database,
        "SELECT COUNT(*) AS rows, COUNT(value) AS present, SUM(value), MIN(value), MAX(value), AVG(value)
         FROM readings WHERE value > 100;",
    );
    assert_eq!(
        empty.rows,
        vec![vec![
            Value::Int64(0),
            Value::Int64(0),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ]]
    );
}

#[test]
fn min_and_max_skip_null_for_bool_and_string() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE dimensions (flag Nullable(Bool), label Nullable(String));
             INSERT INTO dimensions VALUES (NULL, NULL), (true, 'z'), (false, 'a');",
        )
        .expect("setup succeeds");

    assert_eq!(
        query(
            &mut database,
            "SELECT MIN(flag), MAX(flag), MIN(label), MAX(label) FROM dimensions;"
        )
        .rows,
        vec![vec![
            Value::Bool(false),
            Value::Bool(true),
            Value::String("a".to_owned()),
            Value::String("z".to_owned()),
        ]]
    );
}
