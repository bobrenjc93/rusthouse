use rusthouse::{Database, Error, QueryLimits, StatementResult, Value};

fn query(database: &Database, sql: &str) -> Vec<Vec<Value>> {
    match database.execute(sql).unwrap() {
        StatementResult::Query(result) => result.rows,
        result => panic!("expected query result, got {result:?}"),
    }
}

#[test]
fn inner_hash_join_skips_null_keys_and_preserves_duplicate_matches() {
    let database = Database::new();
    database
        .execute("CREATE TABLE left_rows (seq Int64, id Int64 NULL, label String)")
        .unwrap();
    database
        .execute("CREATE TABLE right_rows (id Int64 NULL, tag String)")
        .unwrap();
    database
        .execute("INSERT INTO left_rows VALUES (1, 1, 'one'), (2, NULL, 'null'), (3, 2, 'two')")
        .unwrap();
    database
        .execute("INSERT INTO right_rows VALUES (1, 'a'), (1, 'b'), (NULL, 'n')")
        .unwrap();

    let sql = "SELECT l.seq, l.label, r.tag FROM left_rows AS l \
               INNER JOIN right_rows AS r ON l.id = r.id ORDER BY l.seq, r.tag";
    assert_eq!(
        query(&database, sql),
        vec![
            vec![Value::Int64(1), Value::from("one"), Value::from("a")],
            vec![Value::Int64(1), Value::from("one"), Value::from("b")],
        ]
    );
    assert_eq!(query(&database, sql), query(&database, sql));
}

#[test]
fn left_join_null_extends_unmatched_rows_and_handles_an_empty_build() {
    let database = Database::new();
    database
        .execute("CREATE TABLE left_rows (seq Int64, id Int64 NULL)")
        .unwrap();
    database
        .execute("CREATE TABLE empty_right (id Int64, tag String)")
        .unwrap();
    database
        .execute("INSERT INTO left_rows VALUES (1, 7), (2, NULL)")
        .unwrap();

    let result = database
        .execute(
            "SELECT l.seq, r.tag FROM left_rows l LEFT JOIN empty_right r \
             ON l.id = r.id ORDER BY l.seq",
        )
        .unwrap()
        .into_result_set()
        .unwrap();
    assert!(result.columns[1].nullable);
    assert_eq!(
        result.rows,
        vec![
            vec![Value::Int64(1), Value::Null],
            vec![Value::Int64(2), Value::Null],
        ]
    );
}

#[test]
fn binding_rejects_ambiguous_and_invalid_qualified_columns_on_empty_inputs() {
    let database = Database::new();
    database.execute("CREATE TABLE first (id Int64)").unwrap();
    database.execute("CREATE TABLE second (id Int64)").unwrap();

    assert!(matches!(
        database.execute(
            "SELECT id FROM first f INNER JOIN second s ON f.id = s.id"
        ),
        Err(Error::AmbiguousColumn(column)) if column == "id"
    ));
    assert!(matches!(
        database.execute(
            "SELECT first.id FROM first f INNER JOIN second s ON f.id = s.id"
        ),
        Err(Error::ColumnNotFound(column)) if column == "first.id"
    ));
    assert!(matches!(
        database.execute("SELECT ROW_NUMBER() OVER (ORDER BY missing) AS rn FROM first"),
        Err(Error::ColumnNotFound(column)) if column == "missing"
    ));
    assert!(
        query(
            &database,
            "SELECT f.id, ROW_NUMBER() OVER (ORDER BY f.id) AS rn FROM first f"
        )
        .is_empty()
    );
}

#[test]
fn window_rank_and_bounded_running_aggregates_are_deterministic() {
    let database = Database::new();
    database
        .execute("CREATE TABLE events (grp String, seq Int64, score Int64 NULL)")
        .unwrap();
    database
        .execute(
            "INSERT INTO events VALUES \
             ('a', 1, 10), ('a', 2, 10), ('a', 3, NULL), ('a', 4, 20), ('b', 1, 5)",
        )
        .unwrap();

    let sql = "SELECT grp, seq, \
               ROW_NUMBER() OVER (PARTITION BY grp ORDER BY score) AS rn, \
               RANK() OVER (PARTITION BY grp ORDER BY score) AS rnk, \
               SUM(score) OVER (PARTITION BY grp ORDER BY seq \
                 ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS running, \
               COUNT(score) OVER (PARTITION BY grp ORDER BY seq ROWS 1 PRECEDING) AS counted \
               FROM events ORDER BY grp, seq";
    let expected = vec![
        vec![
            Value::from("a"),
            Value::Int64(1),
            Value::Int64(1),
            Value::Int64(1),
            Value::Int64(10),
            Value::Int64(1),
        ],
        vec![
            Value::from("a"),
            Value::Int64(2),
            Value::Int64(2),
            Value::Int64(1),
            Value::Int64(20),
            Value::Int64(2),
        ],
        vec![
            Value::from("a"),
            Value::Int64(3),
            Value::Int64(4),
            Value::Int64(4),
            Value::Int64(10),
            Value::Int64(1),
        ],
        vec![
            Value::from("a"),
            Value::Int64(4),
            Value::Int64(3),
            Value::Int64(3),
            Value::Int64(20),
            Value::Int64(1),
        ],
        vec![
            Value::from("b"),
            Value::Int64(1),
            Value::Int64(1),
            Value::Int64(1),
            Value::Int64(5),
            Value::Int64(1),
        ],
    ];
    assert_eq!(query(&database, sql), expected);
    assert_eq!(query(&database, sql), expected);
}

#[test]
fn rows_following_frames_and_default_running_frames_have_sql_empty_values() {
    let database = Database::new();
    database
        .execute("CREATE TABLE numbers (seq Int64, value Int64 NULL)")
        .unwrap();
    database
        .execute("INSERT INTO numbers VALUES (1, 2), (2, NULL), (3, 4)")
        .unwrap();

    assert_eq!(
        query(
            &database,
            "SELECT seq, SUM(value) OVER (ORDER BY seq) AS running, \
             COUNT(*) OVER (ORDER BY seq ROWS BETWEEN 1 FOLLOWING AND 1 FOLLOWING) AS next_rows, \
             SUM(value) OVER (ORDER BY seq ROWS BETWEEN 1 FOLLOWING AND 1 FOLLOWING) AS next_sum \
             FROM numbers ORDER BY seq"
        ),
        vec![
            vec![
                Value::Int64(1),
                Value::Int64(2),
                Value::Int64(1),
                Value::Null,
            ],
            vec![
                Value::Int64(2),
                Value::Int64(2),
                Value::Int64(1),
                Value::Int64(4),
            ],
            vec![
                Value::Int64(3),
                Value::Int64(6),
                Value::Int64(0),
                Value::Null,
            ],
        ]
    );
}

#[test]
fn bounded_float_sum_does_not_subtract_rounded_prefixes() {
    let database = Database::new();
    database
        .execute("CREATE TABLE floats (seq Int64, value Float64)")
        .unwrap();
    database
        .execute("INSERT INTO floats VALUES (1, 1e16), (2, 1.0), (3, -1e16)")
        .unwrap();

    assert_eq!(
        query(
            &database,
            "SELECT seq, SUM(value) OVER (ORDER BY seq \
             ROWS BETWEEN CURRENT ROW AND CURRENT ROW) AS framed \
             FROM floats ORDER BY seq"
        ),
        vec![
            vec![Value::Int64(1), Value::Float64(1e16)],
            vec![Value::Int64(2), Value::Float64(1.0)],
            vec![Value::Int64(3), Value::Float64(-1e16)],
        ]
    );
}

#[test]
fn float_window_sum_preserves_ordered_overflow_and_opposing_infinity() {
    let database = Database::new();
    database
        .execute("CREATE TABLE overflow_values (seq Int64, value Float64)")
        .unwrap();
    database
        .execute("INSERT INTO overflow_values VALUES (1, 1e308), (2, 1e308), (3, -1e309)")
        .unwrap();

    let rows = query(
        &database,
        "SELECT seq, SUM(value) OVER (ORDER BY seq) AS running, \
         SUM(value) OVER (ORDER BY seq ROWS BETWEEN 2 PRECEDING AND CURRENT ROW) AS bounded \
         FROM overflow_values ORDER BY seq",
    );
    assert_eq!(rows[0][1], Value::Float64(1e308));
    assert_eq!(rows[0][2], Value::Float64(1e308));
    assert_eq!(rows[1][1], Value::Float64(f64::INFINITY));
    assert_eq!(rows[1][2], Value::Float64(f64::INFINITY));
    assert!(matches!(&rows[2][1], Value::Float64(value) if value.is_nan()));
    assert!(matches!(&rows[2][2], Value::Float64(value) if value.is_nan()));
}

#[test]
fn rank_treats_signed_float_zeroes_as_peers() {
    let database = Database::new();
    database
        .execute("CREATE TABLE zeroes (seq Int64, value Float64)")
        .unwrap();
    database
        .execute("INSERT INTO zeroes VALUES (1, -0.0), (2, 0.0)")
        .unwrap();

    assert_eq!(
        query(
            &database,
            "SELECT seq, RANK() OVER (ORDER BY value) AS rnk FROM zeroes ORDER BY seq"
        ),
        vec![
            vec![Value::Int64(1), Value::Int64(1)],
            vec![Value::Int64(2), Value::Int64(1)],
        ]
    );
}

#[test]
fn join_build_partition_and_output_limits_fail_explicitly() {
    let limits = QueryLimits::new(1, usize::MAX, 2, usize::MAX, 1);
    let database = Database::with_query_limits(limits);
    database.execute("CREATE TABLE left_t (id Int64)").unwrap();
    database.execute("CREATE TABLE right_t (id Int64)").unwrap();
    database
        .execute("INSERT INTO left_t VALUES (1), (2), (3)")
        .unwrap();
    database
        .execute("INSERT INTO right_t VALUES (1), (1)")
        .unwrap();

    assert!(matches!(
        database.execute("SELECT l.id FROM left_t l JOIN right_t r ON l.id = r.id"),
        Err(Error::ExecutionRowLimitExceeded {
            operator: "hash join build",
            limit: 1,
            attempted: 2,
        })
    ));
    assert!(matches!(
        database.execute("SELECT ROW_NUMBER() OVER (ORDER BY id) AS rn FROM left_t"),
        Err(Error::ExecutionRowLimitExceeded {
            operator: "window partition",
            limit: 2,
            attempted: 3,
        })
    ));
    assert!(matches!(
        database.execute("SELECT id FROM left_t"),
        Err(Error::ExecutionRowLimitExceeded {
            operator: "query output",
            limit: 1,
            attempted: 3,
        })
    ));
    assert_eq!(
        query(&database, "SELECT id FROM left_t ORDER BY id LIMIT 1"),
        vec![vec![Value::Int64(1)]]
    );
}

#[test]
fn join_count_limit_rejects_schema_expansion_before_execution() {
    let database = Database::with_query_limits(QueryLimits::default().with_max_joins(1));
    database.execute("CREATE TABLE empty_t (id Int64)").unwrap();

    assert!(matches!(
        database.execute(
            "SELECT a.id FROM empty_t a \
             JOIN empty_t b ON a.id = b.id \
             JOIN empty_t c ON a.id = c.id"
        ),
        Err(Error::ExecutionRowLimitExceeded {
            operator: "query joins",
            limit: 1,
            attempted: 2,
        })
    ));
}

#[test]
fn join_build_limits_preflight_before_the_right_scan() {
    let database = Database::new();
    database.execute("CREATE TABLE probe (id Int64)").unwrap();
    database
        .execute("CREATE TABLE large_build (id Int64, payload String)")
        .unwrap();
    database
        .execute(&format!(
            "INSERT INTO large_build VALUES (1, '{}')",
            "x".repeat(4 * 1024)
        ))
        .unwrap();
    let sql = "SELECT p.id FROM probe p JOIN large_build b ON p.id = b.id";
    let mut session = database.session();

    session.set_query_limits(
        QueryLimits::new(0, usize::MAX, usize::MAX, usize::MAX, usize::MAX).with_source_bytes(0),
    );
    assert!(matches!(
        session.execute(sql),
        Err(Error::ExecutionRowLimitExceeded {
            operator: "hash join build",
            limit: 0,
            attempted: 1,
        })
    ));

    session.set_query_limits(
        QueryLimits::new(usize::MAX, 0, usize::MAX, usize::MAX, usize::MAX).with_source_bytes(0),
    );
    assert!(matches!(
        session.execute(sql),
        Err(Error::MemoryLimitExceeded {
            operator: "hash join build",
            limit: 0,
            ..
        })
    ));
}

#[test]
fn duplicate_join_matches_are_bounded_before_result_materialization() {
    let database =
        Database::with_query_limits(QueryLimits::new(2, usize::MAX, usize::MAX, usize::MAX, 1));
    database.execute("CREATE TABLE left_t (id Int64)").unwrap();
    database.execute("CREATE TABLE right_t (id Int64)").unwrap();
    database.execute("INSERT INTO left_t VALUES (1)").unwrap();
    database
        .execute("INSERT INTO right_t VALUES (1), (1)")
        .unwrap();

    assert!(matches!(
        database.execute("SELECT l.id FROM left_t l JOIN right_t r ON l.id = r.id"),
        Err(Error::ExecutionRowLimitExceeded {
            operator: "hash join output",
            limit: 1,
            attempted: 2,
        })
    ));
}

#[test]
fn join_prunes_unprojected_payloads_and_bounds_projected_expansion() {
    let database = Database::with_query_limits(QueryLimits::new(
        usize::MAX,
        1_500,
        usize::MAX,
        usize::MAX,
        usize::MAX,
    ));
    database
        .execute("CREATE TABLE large_left (id Int64, payload String)")
        .unwrap();
    database
        .execute("CREATE TABLE duplicate_right (id Int64)")
        .unwrap();
    database
        .execute(&format!(
            "INSERT INTO large_left VALUES (1, '{}')",
            "x".repeat(800)
        ))
        .unwrap();
    database
        .execute("INSERT INTO duplicate_right VALUES (1), (1)")
        .unwrap();

    assert_eq!(
        query(
            &database,
            "SELECT l.id FROM large_left l JOIN duplicate_right r ON l.id = r.id"
        ),
        vec![vec![Value::Int64(1)], vec![Value::Int64(1)]]
    );
    assert!(matches!(
        database
            .execute("SELECT l.payload FROM large_left l JOIN duplicate_right r ON l.id = r.id"),
        Err(Error::MemoryLimitExceeded {
            operator: "hash join output",
            limit: 1_500,
            ..
        })
    ));
}

#[test]
fn source_scan_is_bounded_after_binding_and_skips_large_unused_strings() {
    let limits = QueryLimits::default().with_source_bytes(128);
    let database = Database::with_query_limits(limits);
    database
        .execute("CREATE TABLE payloads (id Int64, payload String)")
        .unwrap();
    database
        .execute(&format!(
            "INSERT INTO payloads VALUES (1, '{}')",
            "x".repeat(4 * 1024)
        ))
        .unwrap();

    assert_eq!(
        query(&database, "SELECT id FROM payloads WHERE id = 2"),
        Vec::<Vec<Value>>::new()
    );
    assert!(matches!(
        database.execute("SELECT missing FROM payloads"),
        Err(Error::ColumnNotFound(column)) if column == "missing"
    ));
    assert!(matches!(
        database.execute("SELECT payload FROM payloads"),
        Err(Error::MemoryLimitExceeded {
            operator: "table scan",
            limit: 128,
            ..
        })
    ));
}

#[test]
fn join_and_window_byte_state_limits_are_enforced() {
    let database =
        Database::with_query_limits(QueryLimits::new(usize::MAX, 0, usize::MAX, 0, usize::MAX));
    database.execute("CREATE TABLE left_t (id Int64)").unwrap();
    database.execute("CREATE TABLE right_t (id Int64)").unwrap();
    database.execute("INSERT INTO left_t VALUES (1)").unwrap();
    database.execute("INSERT INTO right_t VALUES (1)").unwrap();

    assert!(matches!(
        database.execute("SELECT l.id FROM left_t l JOIN right_t r ON l.id = r.id"),
        Err(Error::MemoryLimitExceeded {
            operator: "hash join build",
            limit: 0,
            ..
        })
    ));
    assert!(matches!(
        database.execute("SELECT ROW_NUMBER() OVER (ORDER BY id) AS rn FROM left_t"),
        Err(Error::MemoryLimitExceeded {
            operator: "window partition",
            limit: 0,
            ..
        })
    ));
}

#[test]
fn hash_build_limit_accounts_unique_key_buckets_and_index_vectors() {
    let database = Database::new();
    database.execute("CREATE TABLE probe (id Int64)").unwrap();
    database.execute("CREATE TABLE build (id Int64)").unwrap();
    database.execute("INSERT INTO probe VALUES (1)").unwrap();
    database
        .execute(
            "INSERT INTO build VALUES (1), (2), (3), (4), (5), \
             (6), (7), (8), (9), (10)",
        )
        .unwrap();

    let mut session = database.session();
    session.set_query_limits(QueryLimits::new(
        usize::MAX,
        2_000,
        usize::MAX,
        usize::MAX,
        usize::MAX,
    ));
    assert!(matches!(
        session.execute("SELECT p.id FROM probe p JOIN build b ON p.id = b.id"),
        Err(Error::MemoryLimitExceeded {
            operator: "hash join build",
            limit: 2_000,
            ..
        })
    ));

    session.set_query_limits(QueryLimits::new(
        usize::MAX,
        10_000,
        usize::MAX,
        usize::MAX,
        usize::MAX,
    ));
    assert!(matches!(
        session.execute("SELECT p.id FROM probe p JOIN build b ON p.id = b.id"),
        Ok(StatementResult::Query(result)) if result.rows == vec![vec![Value::Int64(1)]]
    ));
}

#[test]
fn window_limit_counts_cumulative_partitions_outputs_and_prefix_state() {
    let database = Database::with_query_limits(QueryLimits::new(
        usize::MAX,
        usize::MAX,
        usize::MAX,
        1_000,
        usize::MAX,
    ));
    database
        .execute("CREATE TABLE many_partitions (grp Int64, value Float64)")
        .unwrap();
    database
        .execute(
            "INSERT INTO many_partitions VALUES \
             (1, 1.0), (2, 2.0), (3, 3.0), (4, 4.0), (5, 5.0), (6, 6.0), (7, 7.0), (8, 8.0)",
        )
        .unwrap();

    assert!(matches!(
        database.execute(
            "SELECT ROW_NUMBER() OVER (PARTITION BY grp ORDER BY value) AS rn \
             FROM many_partitions"
        ),
        Err(Error::MemoryLimitExceeded {
            operator: "window partition",
            limit: 1_000,
            ..
        })
    ));

    let prefix_database = Database::with_query_limits(QueryLimits::new(
        usize::MAX,
        usize::MAX,
        usize::MAX,
        650,
        usize::MAX,
    ));
    prefix_database
        .execute("CREATE TABLE narrow (value Float64)")
        .unwrap();
    prefix_database
        .execute(
            "INSERT INTO narrow VALUES (1.0), (2.0), (3.0), (4.0), (5.0), \
             (6.0), (7.0), (8.0), (9.0), (10.0)",
        )
        .unwrap();
    assert_eq!(
        query(
            &prefix_database,
            "SELECT ROW_NUMBER() OVER (ORDER BY value) AS rn FROM narrow"
        )
        .len(),
        10
    );
    assert!(matches!(
        prefix_database
            .execute("SELECT COUNT(*) OVER (ORDER BY value) AS running_count FROM narrow"),
        Err(Error::MemoryLimitExceeded {
            operator: "window partition",
            limit: 650,
            ..
        })
    ));
}
