use rusthouse::{DataType, Database, Error, QueryResult, StatementResult, Value};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    match results.into_iter().last().expect("one result") {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

#[test]
fn grouped_having_accepts_group_columns_aggregates_and_unique_aliases() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE sales (region String, amount Int64);
             INSERT INTO sales VALUES
                ('west', 4), ('west', 7), ('west', 7),
                ('east', 5), ('east', 5), ('north', 20);",
        )
        .expect("setup succeeds");

    let result = query(
        &mut database,
        "SELECT region AS area, COUNT(*) AS orders, SUM(amount) AS total
         FROM sales
         WHERE amount >= 5
         GROUP BY region
         HAVING area != 'north'
            AND region != 'void'
            AND orders >= 2
            AND COUNT(DISTINCT amount) = 1
         ORDER BY total DESC;",
    );
    assert_eq!(
        result.rows,
        vec![
            vec![
                Value::String("west".to_owned()),
                Value::Int64(2),
                Value::Int64(14),
            ],
            vec![
                Value::String("east".to_owned()),
                Value::Int64(2),
                Value::Int64(10),
            ],
        ]
    );

    let unselected_aggregate = query(
        &mut database,
        "SELECT region FROM sales
         GROUP BY region
         HAVING COUNT(*) >= 2
         ORDER BY region;",
    );
    assert_eq!(
        unselected_aggregate.rows,
        vec![
            vec![Value::String("east".to_owned())],
            vec![Value::String("west".to_owned())],
        ]
    );

    let grouped_wildcard = query(
        &mut database,
        "SELECT * FROM sales
         GROUP BY region, amount
         HAVING COUNT(*) >= 2
         ORDER BY region, amount;",
    );
    assert_eq!(
        grouped_wildcard.rows,
        vec![
            vec![Value::String("east".to_owned()), Value::Int64(5)],
            vec![Value::String("west".to_owned()), Value::Int64(7)],
        ]
    );
}

#[test]
fn global_having_handles_empty_aggregate_inputs() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE empty_values (value Int64);")
        .expect("create succeeds");

    let admitted = query(
        &mut database,
        "SELECT COUNT(*) AS rows, COUNT(DISTINCT value) AS unique_values
         FROM empty_values
         HAVING rows = 0 AND COUNT(DISTINCT value) = 0;",
    );
    assert_eq!(admitted.rows, vec![vec![Value::Int64(0), Value::Int64(0)]]);

    let removed = query(
        &mut database,
        "SELECT COUNT(*) AS rows FROM empty_values HAVING rows > 0;",
    );
    assert!(removed.rows.is_empty());
}

#[test]
fn select_distinct_deduplicates_typed_tuples_before_order_and_limit() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE typed_values (i Int64, f Float64, b Bool, s String);
             INSERT INTO typed_values VALUES
                (4, 4.0, true, 'c'), (4, 4.0, true, 'c'),
                (3, 2.5, true, 'b'), (3, 2.5, true, 'b'),
                (2, 2.5, true, 'b'),
                (1, 0.0, false, 'a'), (1, -0.0, false, 'a');",
        )
        .expect("setup succeeds");

    let result = query(
        &mut database,
        "SELECT DISTINCT i, f, b, s
         FROM typed_values
         ORDER BY i DESC
         LIMIT 3;",
    );
    assert_eq!(
        result.rows,
        vec![
            vec![
                Value::Int64(4),
                Value::Float64(4.0),
                Value::Bool(true),
                Value::String("c".to_owned()),
            ],
            vec![
                Value::Int64(3),
                Value::Float64(2.5),
                Value::Bool(true),
                Value::String("b".to_owned()),
            ],
            vec![
                Value::Int64(2),
                Value::Float64(2.5),
                Value::Bool(true),
                Value::String("b".to_owned()),
            ],
        ]
    );

    let stable_without_order = query(&mut database, "SELECT DISTINCT s FROM typed_values;");
    assert_eq!(
        stable_without_order.rows,
        vec![
            vec![Value::String("c".to_owned())],
            vec![Value::String("b".to_owned())],
            vec![Value::String("a".to_owned())],
        ]
    );

    let empty = query(
        &mut database,
        "SELECT DISTINCT i, f, b, s FROM typed_values WHERE i < 0;",
    );
    assert!(empty.rows.is_empty());
}

#[test]
fn count_distinct_supports_every_type_globally_and_per_group() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (g String, i Int64, f Float64, b Bool, s String);
             INSERT INTO samples VALUES
                ('a', 1, 0.0, false, 'x'),
                ('a', 1, 0.0, false, 'x'),
                ('a', 2, -0.0, true, 'y'),
                ('b', 2, 2.5, true, 'y'),
                ('b', 2, 2.5, true, 'y'),
                ('b', 3, 3.5, true, 'z');",
        )
        .expect("setup succeeds");

    let global = query(
        &mut database,
        "SELECT COUNT(DISTINCT i), COUNT(DISTINCT f),
                COUNT(DISTINCT b), COUNT(DISTINCT s)
         FROM samples;",
    );
    assert_eq!(
        global
            .columns
            .iter()
            .map(|column| column.data_type)
            .collect::<Vec<_>>(),
        vec![DataType::Int64; 4]
    );
    assert_eq!(
        global.rows,
        vec![vec![
            Value::Int64(3),
            Value::Int64(3),
            Value::Int64(2),
            Value::Int64(3),
        ]]
    );

    let grouped = query(
        &mut database,
        "SELECT g, COUNT(DISTINCT i), COUNT(DISTINCT f),
                COUNT(DISTINCT b), COUNT(DISTINCT s)
         FROM samples
         GROUP BY g
         HAVING COUNT(DISTINCT i) = 2
         ORDER BY g;",
    );
    assert_eq!(
        grouped.rows,
        vec![
            vec![
                Value::String("a".to_owned()),
                Value::Int64(2),
                Value::Int64(1),
                Value::Int64(2),
                Value::Int64(2),
            ],
            vec![
                Value::String("b".to_owned()),
                Value::Int64(2),
                Value::Int64(2),
                Value::Int64(1),
                Value::Int64(2),
            ],
        ]
    );

    let filtered_empty = query(
        &mut database,
        "SELECT COUNT(DISTINCT i), COUNT(DISTINCT f),
                COUNT(DISTINCT b), COUNT(DISTINCT s)
         FROM samples WHERE i < 0;",
    );
    assert_eq!(filtered_empty.rows, vec![vec![Value::Int64(0); 4]]);
}

#[test]
fn distinct_applies_to_grouped_results_after_having() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE events (category String, id Int64);
             INSERT INTO events VALUES
                ('a', 1), ('a', 2), ('a', 3),
                ('b', 4), ('b', 5),
                ('c', 6), ('d', 7);",
        )
        .expect("setup succeeds");

    let result = query(
        &mut database,
        "SELECT DISTINCT COUNT(*) AS frequency
         FROM events
         GROUP BY category
         HAVING COUNT(*) >= 1
         ORDER BY frequency DESC
         LIMIT 3;",
    );
    assert_eq!(
        result.rows,
        vec![
            vec![Value::Int64(3)],
            vec![Value::Int64(2)],
            vec![Value::Int64(1)],
        ]
    );
}

#[test]
fn invalid_having_and_distinct_references_are_actionable() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE sales (region String, amount Int64);
             INSERT INTO sales VALUES ('west', 7);",
        )
        .expect("setup succeeds");

    let ungrouped = database
        .execute(
            "SELECT region, COUNT(*) FROM sales
             GROUP BY region HAVING amount > 1;",
        )
        .expect_err("ungrouped HAVING column is invalid");
    assert!(
        matches!(ungrouped, Error::InvalidQuery(message) if message.contains("must appear in GROUP BY"))
    );

    let unknown = database
        .execute(
            "SELECT region, COUNT(*) FROM sales
             GROUP BY region HAVING missing > 1;",
        )
        .expect_err("unknown HAVING name is invalid");
    assert!(matches!(
        unknown,
        Error::ColumnNotFound { column, .. } if column == "missing"
    ));

    for sql in [
        "SELECT region AS metric, COUNT(*) AS metric FROM sales
         GROUP BY region HAVING metric > 1",
        "SELECT region, COUNT(*) AS region FROM sales
         GROUP BY region HAVING region > 1",
    ] {
        let ambiguous = database
            .execute(sql)
            .expect_err("ambiguous HAVING name is invalid");
        assert!(matches!(ambiguous, Error::InvalidQuery(message) if message.contains("ambiguous")));
    }

    let without_grouping = database
        .execute("SELECT amount FROM sales HAVING 1 = 1")
        .expect_err("HAVING requires grouped data");
    assert!(
        matches!(without_grouping, Error::InvalidQuery(message) if message.contains("HAVING requires"))
    );

    let unsupported_distinct = database
        .execute("SELECT SUM(DISTINCT amount) FROM sales")
        .expect_err("only COUNT accepts a distinct argument");
    assert!(
        matches!(unsupported_distinct, Error::InvalidQuery(message) if message.contains("only supported for COUNT"))
    );

    let aggregate_in_where = database
        .execute("SELECT amount FROM sales WHERE COUNT(*) > 0")
        .expect_err("WHERE runs before aggregation");
    assert!(
        matches!(aggregate_in_where, Error::InvalidQuery(message) if message.contains("not allowed in WHERE"))
    );

    let type_mismatch = database
        .execute(
            "SELECT region, COUNT(*) FROM sales
             GROUP BY region HAVING COUNT(*) = 'one'",
        )
        .expect_err("HAVING comparisons are typed");
    assert!(matches!(
        type_mismatch,
        Error::TypeMismatch { context, .. } if context == "HAVING comparison"
    ));

    assert!(matches!(
        database.execute("SELECT COUNT(DISTINCT *) FROM sales"),
        Err(Error::Sql { .. })
    ));
}
