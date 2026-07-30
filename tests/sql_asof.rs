use rusthouse::{AsofJoinLimits, Database, Error, QueryResult, StatementResult, Value};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    match database
        .execute(sql)
        .expect("query succeeds")
        .into_iter()
        .last()
        .expect("statement result")
    {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

#[test]
fn backward_asof_uses_equality_keys_and_last_duplicate() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE trades (id Int64, symbol String, ts Int64);
             CREATE TABLE quotes (symbol String, ts Int64, price Int64, usable Bool);
             INSERT INTO trades VALUES
                (1, 'A', 1), (2, 'A', 5), (3, 'A', 10), (4, 'B', 4);
             INSERT INTO quotes VALUES
                ('A', 2, 20, true),
                ('A', 5, 50, false),
                ('A', 5, 51, true),
                ('A', 8, 80, true),
                ('B', 6, 60, true);",
        )
        .expect("setup succeeds");

    let result = query(
        &mut database,
        "SELECT t.id, q.ts AS quote_ts, q.price
         FROM trades AS t
         ASOF LEFT JOIN quotes AS q
           ON t.symbol = q.symbol AND t.ts >= q.ts
         ORDER BY t.id;",
    );
    assert_eq!(
        result.rows,
        vec![
            vec![Value::Int64(1), Value::Null, Value::Null],
            vec![Value::Int64(2), Value::Int64(5), Value::Int64(51)],
            vec![Value::Int64(3), Value::Int64(8), Value::Int64(80)],
            vec![Value::Int64(4), Value::Null, Value::Null],
        ]
    );

    let filtered = query(
        &mut database,
        "SELECT t.id, q.price
         FROM trades t ASOF LEFT JOIN quotes q
           ON t.symbol = q.symbol AND t.ts >= q.ts
         WHERE q.usable = true
         ORDER BY t.id LIMIT 2;",
    );
    assert_eq!(
        filtered.rows,
        vec![
            vec![Value::Int64(2), Value::Int64(51)],
            vec![Value::Int64(3), Value::Int64(80)],
        ]
    );
}

#[test]
fn strict_forward_asof_without_equality_keys_is_deterministic() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE probes (id Int64, ts Int64);
             CREATE TABLE schedule (ts Int64, label String);
             INSERT INTO probes VALUES (1, 5), (2, 7), (3, 10);
             INSERT INTO schedule VALUES
                (5, 'equal'), (8, 'first duplicate'), (8, 'last duplicate');",
        )
        .expect("setup succeeds");

    let result = query(
        &mut database,
        "SELECT p.id, s.ts, s.label
         FROM probes p ASOF LEFT JOIN schedule s ON p.ts < s.ts
         ORDER BY p.id;",
    );
    assert_eq!(
        result.rows,
        vec![
            vec![
                Value::Int64(1),
                Value::Int64(8),
                Value::String("last duplicate".to_owned()),
            ],
            vec![
                Value::Int64(2),
                Value::Int64(8),
                Value::String("last duplicate".to_owned()),
            ],
            vec![Value::Int64(3), Value::Null, Value::Null],
        ]
    );

    let reversed_operands = query(
        &mut database,
        "SELECT p.id, s.label
         FROM probes p ASOF LEFT JOIN schedule s ON s.ts > p.ts
         ORDER BY p.id LIMIT 1;",
    );
    assert_eq!(reversed_operands.rows[0][0], Value::Int64(1));
    assert_eq!(
        reversed_operands.rows[0][1],
        Value::String("last duplicate".to_owned())
    );
}

#[test]
fn asof_rows_filter_group_order_and_limit_coherently() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE readings (device String, ts Int64);
             CREATE TABLE status (device String, ts Int64, state String, weight Int64);
             INSERT INTO readings VALUES
                ('a', 2), ('a', 6), ('a', 9), ('b', 3), ('c', 4);
             INSERT INTO status VALUES
                ('a', 1, 'cold', 2), ('a', 5, 'hot', 7), ('b', 2, 'cold', 3);",
        )
        .expect("setup succeeds");

    let grouped = query(
        &mut database,
        "SELECT s.state, COUNT(*) AS rows, COUNT(s.weight) AS matched,
                SUM(s.weight) AS weight
         FROM readings r ASOF LEFT JOIN status s
           ON r.device = s.device AND r.ts >= s.ts
         GROUP BY s.state
         ORDER BY rows DESC
         LIMIT 3;",
    );
    assert_eq!(
        grouped.rows,
        vec![
            vec![
                Value::String("cold".to_owned()),
                Value::Int64(2),
                Value::Int64(2),
                Value::Int64(5),
            ],
            vec![
                Value::String("hot".to_owned()),
                Value::Int64(2),
                Value::Int64(2),
                Value::Int64(14),
            ],
            vec![
                Value::Null,
                Value::Int64(1),
                Value::Int64(0),
                Value::Int64(0)
            ],
        ]
    );
}

#[test]
fn date_and_datetime64_are_ordered_asof_keys() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE daily (id Int64, day Date);
             CREATE TABLE rates (day Date, rate Int64);
             INSERT INTO daily VALUES (1, '2026-01-01'), (2, '2026-01-05');
             INSERT INTO rates VALUES ('2025-12-31', 9), ('2026-01-03', 11);
             CREATE TABLE events (id Int64, happened DateTime64(3));
             CREATE TABLE versions (changed DateTime64(3), version Int64);
             INSERT INTO events VALUES
                (1, '2026-01-01T00:00:00.100Z'),
                (2, '2026-01-01T00:00:01.000Z');
             INSERT INTO versions VALUES
                ('2026-01-01T00:00:00.050Z', 1),
                ('2026-01-01T00:00:00.500Z', 2);",
        )
        .expect("temporal setup succeeds");

    let dates = query(
        &mut database,
        "SELECT d.id, r.day, r.rate
         FROM daily d ASOF LEFT JOIN rates r ON d.day >= r.day
         ORDER BY d.id;",
    );
    assert_eq!(dates.rows[0][1].as_display_string(), "2025-12-31");
    assert_eq!(dates.rows[1][1].as_display_string(), "2026-01-03");

    let timestamps = query(
        &mut database,
        "SELECT e.id, v.version
         FROM events e ASOF LEFT JOIN versions v ON e.happened >= v.changed
         WHERE e.happened >= DATETIME64(3) '2026-01-01T00:00:00.000Z'
         ORDER BY e.id;",
    );
    assert_eq!(
        timestamps.rows,
        vec![
            vec![Value::Int64(1), Value::Int64(1)],
            vec![Value::Int64(2), Value::Int64(2)],
        ]
    );
}

#[test]
fn invalid_asof_shapes_are_rejected() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE l (id Int64, ts Int64, label String);
             CREATE TABLE r (id Int64, ts Int64, label String);",
        )
        .expect("setup succeeds");

    for sql in [
        "SELECT l.id FROM l ASOF LEFT JOIN r ON l.id = r.id;",
        "SELECT l.id FROM l ASOF LEFT JOIN r ON l.ts >= r.ts AND l.id < r.id;",
        "SELECT l.id FROM l ASOF LEFT JOIN r ON l.label >= r.label;",
        "SELECT l.id FROM l ASOF LEFT JOIN r ON l.ts != r.ts;",
        "SELECT l.id FROM l ASOF LEFT JOIN r ON l.ts >= l.id;",
    ] {
        assert!(
            matches!(
                database.execute(sql),
                Err(Error::InvalidQuery(_)) | Err(Error::TypeMismatch { .. })
            ),
            "query should fail: {sql}"
        );
    }
}

#[test]
fn asof_row_memory_and_candidate_limits_fail_before_results() {
    let cases = [
        (
            AsofJoinLimits {
                max_rows: 1,
                max_bytes: usize::MAX,
                max_candidate_comparisons: usize::MAX,
            },
            "indexed rows",
        ),
        (
            AsofJoinLimits {
                max_rows: usize::MAX,
                max_bytes: 0,
                max_candidate_comparisons: usize::MAX,
            },
            "index bytes",
        ),
        (
            AsofJoinLimits {
                max_rows: usize::MAX,
                max_bytes: usize::MAX,
                max_candidate_comparisons: 0,
            },
            "candidate comparisons",
        ),
    ];

    for (limits, expected_resource) in cases {
        let mut database = Database::with_asof_join_limits(limits);
        database
            .execute(
                "CREATE TABLE l (ts Int64); INSERT INTO l VALUES (3);
                 CREATE TABLE r (ts Int64); INSERT INTO r VALUES (1), (2);",
            )
            .expect("setup succeeds");
        let error = database
            .execute("SELECT l.ts FROM l ASOF LEFT JOIN r ON l.ts >= r.ts LIMIT 0;")
            .expect_err("operator bounds apply before SQL LIMIT");
        assert!(matches!(
            error,
            Error::AsofJoinLimitExceeded { resource, .. } if resource == expected_resource
        ));
    }
}

#[test]
fn boolean_word_aliases_qualify_asof_conditions_and_predicates() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE probes (id Int64, key Int64, ts Int64);
             CREATE TABLE history (key Int64, ts Int64, value String);
             INSERT INTO probes VALUES (2, 1, 8), (1, 1, 5);
             INSERT INTO history VALUES (1, 3, 'old'), (1, 7, 'new');",
        )
        .expect("setup succeeds");

    let result = query(
        &mut database,
        "SELECT true.id, false.value
         FROM probes AS true
         ASOF LEFT JOIN history AS false
           ON true.key = false.key AND true.ts >= false.ts
         WHERE true.id = 1 AND false.value = 'old'
         ORDER BY true.id;",
    );
    assert_eq!(
        result.rows,
        vec![vec![Value::Int64(1), Value::String("old".to_owned()),]]
    );
}
