use rusthouse::{Database, Error, JoinLimits, QueryResult, StatementResult, Value};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    match results.into_iter().last().expect("one result") {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

#[test]
fn semi_and_anti_preserve_left_order_suppress_right_duplicates_and_obey_null_semantics() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE left_rows (id Nullable(Int64), label String, rank Int64);
             INSERT INTO left_rows VALUES
               (3, 'third', 30),
               (1, 'first', 10),
               (NULL, 'null key', 40),
               (2, 'second', 20),
               (1, 'duplicate left', 11);
             CREATE TABLE right_rows (id Nullable(Int64));
             INSERT INTO right_rows VALUES (1), (1), (3), (NULL);",
        )
        .expect("setup succeeds");

    let semi = query(
        &mut database,
        "SELECT * FROM left_rows l LEFT SEMI JOIN right_rows r ON l.id = r.id;",
    );
    assert_eq!(
        semi.columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>(),
        ["id", "label", "rank"]
    );
    assert_eq!(
        semi.rows,
        vec![
            vec![
                Value::Int64(3),
                Value::String("third".to_owned()),
                Value::Int64(30),
            ],
            vec![
                Value::Int64(1),
                Value::String("first".to_owned()),
                Value::Int64(10),
            ],
            vec![
                Value::Int64(1),
                Value::String("duplicate left".to_owned()),
                Value::Int64(11),
            ],
        ]
    );

    let anti = query(
        &mut database,
        "SELECT l.id, l.label FROM left_rows l
         LEFT ANTI JOIN right_rows r ON l.id = r.id;",
    );
    assert_eq!(
        anti.rows,
        vec![
            vec![Value::Null, Value::String("null key".to_owned())],
            vec![Value::Int64(2), Value::String("second".to_owned())],
        ]
    );
}

#[test]
fn filtering_joins_feed_where_grouping_ordering_and_limit() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE accounts (id Int64, region String, enabled Bool);
             INSERT INTO accounts VALUES
               (1, 'west', true), (2, 'east', true), (3, 'west', false),
               (4, 'west', true), (5, 'north', true);
             CREATE TABLE events (account_id Int64, region String, amount Int64);
             INSERT INTO events VALUES
               (1, 'west', 10), (1, 'west', 20), (2, 'east', 5),
               (3, 'west', 15), (4, 'east', 50), (5, 'north', 10);",
        )
        .expect("setup succeeds");

    let grouped = query(
        &mut database,
        "SELECT a.region, COUNT(*) AS accounts, SUM(a.id) AS id_total
         FROM accounts a LEFT SEMI JOIN events e
           ON a.id = e.account_id AND a.region = e.region AND e.amount >= 10
         WHERE a.enabled = true
         GROUP BY a.region
         ORDER BY accounts DESC, a.region ASC
         LIMIT 2;",
    );
    assert_eq!(
        grouped.rows,
        vec![
            vec![
                Value::String("north".to_owned()),
                Value::Int64(1),
                Value::Int64(5)
            ],
            vec![
                Value::String("west".to_owned()),
                Value::Int64(1),
                Value::Int64(1)
            ],
        ]
    );

    let excluded = query(
        &mut database,
        "SELECT a.id FROM accounts a LEFT ANTI JOIN events e
           ON a.id = e.account_id AND a.region = e.region AND e.amount >= 10
         WHERE a.enabled = true ORDER BY a.id DESC LIMIT 2;",
    );
    assert_eq!(
        excluded.rows,
        vec![vec![Value::Int64(4)], vec![Value::Int64(2)]]
    );
}

#[test]
fn empty_inputs_have_coherent_semi_anti_and_aggregate_results() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE empty_rows (id Int64);
             CREATE TABLE populated (id Int64);
             INSERT INTO populated VALUES (1), (2), (3);",
        )
        .expect("setup succeeds");

    let empty_left = query(
        &mut database,
        "SELECT COUNT(*) AS rows FROM empty_rows l
         LEFT ANTI JOIN populated r ON l.id = r.id;",
    );
    assert_eq!(empty_left.rows, vec![vec![Value::Int64(0)]]);

    let empty_right_semi = query(
        &mut database,
        "SELECT COUNT(*) AS rows FROM populated l
         LEFT SEMI JOIN empty_rows r ON l.id = r.id;",
    );
    assert_eq!(empty_right_semi.rows, vec![vec![Value::Int64(0)]]);

    let empty_right_anti = query(
        &mut database,
        "SELECT l.id FROM populated l LEFT ANTI JOIN empty_rows r ON l.id = r.id
         ORDER BY l.id DESC LIMIT 2;",
    );
    assert_eq!(
        empty_right_anti.rows,
        vec![vec![Value::Int64(3)], vec![Value::Int64(2)]]
    );
}

#[test]
fn right_columns_are_scoped_to_the_on_clause() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE left_scope (id Int64); INSERT INTO left_scope VALUES (1);
             CREATE TABLE right_scope (id Int64); INSERT INTO right_scope VALUES (1);",
        )
        .expect("setup succeeds");

    for sql in [
        "SELECT r.id FROM left_scope l LEFT SEMI JOIN right_scope r ON l.id = r.id;",
        "SELECT l.id FROM left_scope l LEFT ANTI JOIN right_scope r ON l.id = r.id WHERE r.id = 1;",
    ] {
        let error = database
            .execute(sql)
            .expect_err("right relation is not an output relation");
        assert!(matches!(
            error,
            Error::InvalidQuery(message) if message.contains("unknown table name or alias 'r'")
        ));
    }
}

#[test]
fn filtering_joins_enforce_hash_output_candidate_and_byte_limits() {
    let setup =
        "CREATE TABLE bounded_left (id Int64); INSERT INTO bounded_left VALUES (1), (2), (3);
         CREATE TABLE bounded_right (id Int64, enabled Bool);
         INSERT INTO bounded_right VALUES (1, true), (1, true), (1, true);";

    let mut build_limited = Database::with_join_limits(JoinLimits {
        max_rows: 2,
        max_bytes: usize::MAX,
        max_candidate_pairs: usize::MAX,
    });
    build_limited.execute(setup).expect("setup succeeds");
    let error = build_limited
        .execute(
            "SELECT l.id FROM bounded_left l
             LEFT SEMI JOIN bounded_right r ON l.id = r.id;",
        )
        .expect_err("three right build rows exceed the limit");
    assert!(matches!(
        error,
        Error::JoinLimitExceeded {
            resource: "rows",
            limit: 2,
            actual: 3,
        }
    ));

    let mut output_limited = Database::with_join_limits(JoinLimits {
        max_rows: 2,
        max_bytes: usize::MAX,
        max_candidate_pairs: usize::MAX,
    });
    output_limited
        .execute(
            "CREATE TABLE output_left (id Int64); INSERT INTO output_left VALUES (1), (2), (3);
             CREATE TABLE output_right (id Int64);",
        )
        .expect("setup succeeds");
    let error = output_limited
        .execute(
            "SELECT l.id FROM output_left l
             LEFT ANTI JOIN output_right r ON l.id = r.id LIMIT 0;",
        )
        .expect_err("SQL LIMIT does not bypass the operator output bound");
    assert!(matches!(
        error,
        Error::JoinLimitExceeded {
            resource: "output rows",
            limit: 2,
            actual: 3,
        }
    ));

    let mut candidate_limited = Database::with_join_limits(JoinLimits {
        max_rows: 10,
        max_bytes: usize::MAX,
        max_candidate_pairs: 2,
    });
    candidate_limited.execute(setup).expect("setup succeeds");
    let error = candidate_limited
        .execute(
            "SELECT l.id FROM bounded_left l LEFT ANTI JOIN bounded_right r
             ON l.id = r.id AND r.enabled = false;",
        )
        .expect_err("failed residual matches still consume candidate work");
    assert!(matches!(
        error,
        Error::JoinLimitExceeded {
            resource: "candidate pairs",
            limit: 2,
            actual: 3,
        }
    ));

    let short_circuited = query(
        &mut candidate_limited,
        "SELECT l.id FROM bounded_left l LEFT SEMI JOIN bounded_right r
         ON l.id = r.id AND r.enabled = true;",
    );
    assert_eq!(short_circuited.rows, vec![vec![Value::Int64(1)]]);

    let mut byte_limited = Database::with_join_limits(JoinLimits {
        max_rows: 10,
        max_bytes: 0,
        max_candidate_pairs: 10,
    });
    byte_limited.execute(setup).expect("setup succeeds");
    let error = byte_limited
        .execute(
            "SELECT l.id FROM bounded_left l
             LEFT SEMI JOIN bounded_right r ON l.id = r.id;",
        )
        .expect_err("hash memory exceeds a zero-byte limit");
    assert!(matches!(
        error,
        Error::JoinLimitExceeded {
            resource: "bytes",
            limit: 0,
            actual,
        } if actual > 0
    ));
}
