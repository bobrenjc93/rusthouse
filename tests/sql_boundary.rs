use rusthouse::storage::{Column, ColumnDef, Table};
use rusthouse::{DataType, Database, Error, QueryResult, StatementResult, Value};

fn last_query(results: Vec<StatementResult>) -> QueryResult {
    match results.into_iter().last().expect("statement result") {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

fn execute_query(database: &mut Database, sql: &str) -> QueryResult {
    last_query(database.execute(sql).expect("SQL succeeds"))
}

#[test]
fn typed_projection_filter_order_and_limit_work_end_to_end() {
    let mut database = Database::new();
    let results = database
        .execute(
            "CREATE TABLE metrics (
                id Int64, score Float64, active Bool, label String
             );
             INSERT INTO metrics VALUES
                (1, 9.5, true, 'alpha'),
                (2, 7.25, false, 'beta'),
                (3, 1.0, true, 'gamma');
             SELECT id, label, score, active
             FROM metrics
             WHERE (active = true AND score >= 5) OR label = 'gamma'
             ORDER BY score DESC
             LIMIT 2;",
        )
        .expect("batch succeeds");

    assert_eq!(results.len(), 3);
    let result = last_query(results);
    assert_eq!(
        result
            .columns
            .iter()
            .map(|column| (&column.name, column.data_type))
            .collect::<Vec<_>>(),
        vec![
            (&"id".to_owned(), DataType::Int64),
            (&"label".to_owned(), DataType::String),
            (&"score".to_owned(), DataType::Float64),
            (&"active".to_owned(), DataType::Bool),
        ]
    );
    assert_eq!(
        result.rows,
        vec![
            vec![
                Value::Int64(1),
                Value::String("alpha".to_owned()),
                Value::Float64(9.5),
                Value::Bool(true),
            ],
            vec![
                Value::Int64(3),
                Value::String("gamma".to_owned()),
                Value::Float64(1.0),
                Value::Bool(true),
            ],
        ]
    );
}

#[test]
fn order_by_limit_preserves_input_order_for_ties_and_accepts_zero() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE ranked (id Int64, score Int64, label String);
             INSERT INTO ranked VALUES
                (1, 5, 'later'),
                (2, 9, 'first tie'),
                (3, 9, 'second tie'),
                (4, 1, 'last');",
        )
        .expect("setup succeeds");

    let top = execute_query(
        &mut database,
        "SELECT id, score, label FROM ranked ORDER BY score DESC LIMIT 2;",
    );
    assert_eq!(
        top.rows,
        vec![
            vec![
                Value::Int64(2),
                Value::Int64(9),
                Value::String("first tie".to_owned()),
            ],
            vec![
                Value::Int64(3),
                Value::Int64(9),
                Value::String("second tie".to_owned()),
            ],
        ]
    );

    let empty = execute_query(
        &mut database,
        "SELECT label, id FROM ranked ORDER BY label, id DESC LIMIT 0;",
    );
    assert!(empty.rows.is_empty());
}

#[test]
fn cumulative_aggregate_windows_handle_partitions_nulls_ties_and_final_limit() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE readings (
                id Int64, cohort String, sequence Int64, amount Nullable(Int64)
             );
             INSERT INTO readings VALUES
                (1, 'a', 1, 10),
                (2, 'a', 2, NULL),
                (3, 'a', 2, 5),
                (4, 'a', 3, 7),
                (5, 'b', 1, NULL),
                (6, 'b', 2, 4);",
        )
        .expect("setup succeeds");

    let result = execute_query(
        &mut database,
        "SELECT id, cohort, amount,
                COUNT(*) OVER (
                    PARTITION BY cohort ORDER BY sequence
                    ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
                ) AS rows_seen,
                COUNT(amount) OVER (
                    PARTITION BY cohort ORDER BY sequence
                    ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
                ) AS values_seen,
                SUM(amount) OVER (
                    PARTITION BY cohort ORDER BY sequence
                    ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
                ) AS running_sum,
                MIN(amount) OVER (
                    PARTITION BY cohort ORDER BY sequence
                    ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
                ) AS running_min,
                MAX(amount) OVER (
                    PARTITION BY cohort ORDER BY sequence
                    ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
                ) AS running_max,
                AVG(amount) OVER (
                    PARTITION BY cohort ORDER BY sequence
                    ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
                ) AS running_avg
         FROM readings
         ORDER BY cohort, id
         LIMIT 5;",
    );

    assert_eq!(
        result
            .columns
            .iter()
            .map(|column| column.data_type)
            .collect::<Vec<_>>(),
        vec![
            DataType::Int64,
            DataType::String,
            DataType::NullableInt64,
            DataType::Int64,
            DataType::Int64,
            DataType::NullableInt64,
            DataType::NullableInt64,
            DataType::NullableInt64,
            DataType::NullableFloat64,
        ]
    );
    assert_eq!(
        result.rows,
        vec![
            vec![
                Value::Int64(1),
                Value::String("a".to_owned()),
                Value::Int64(10),
                Value::Int64(1),
                Value::Int64(1),
                Value::Int64(10),
                Value::Int64(10),
                Value::Int64(10),
                Value::Float64(10.0),
            ],
            vec![
                Value::Int64(2),
                Value::String("a".to_owned()),
                Value::Null,
                Value::Int64(2),
                Value::Int64(1),
                Value::Int64(10),
                Value::Int64(10),
                Value::Int64(10),
                Value::Float64(10.0),
            ],
            vec![
                Value::Int64(3),
                Value::String("a".to_owned()),
                Value::Int64(5),
                Value::Int64(3),
                Value::Int64(2),
                Value::Int64(15),
                Value::Int64(5),
                Value::Int64(10),
                Value::Float64(7.5),
            ],
            vec![
                Value::Int64(4),
                Value::String("a".to_owned()),
                Value::Int64(7),
                Value::Int64(4),
                Value::Int64(3),
                Value::Int64(22),
                Value::Int64(5),
                Value::Int64(10),
                Value::Float64(22.0 / 3.0),
            ],
            vec![
                Value::Int64(5),
                Value::String("b".to_owned()),
                Value::Null,
                Value::Int64(1),
                Value::Int64(0),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ],
        ]
    );
}

#[test]
fn fixed_preceding_windows_use_sliding_prefixes_and_extreme_queues() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (id Int64, cohort String, amount Nullable(Int64));
             INSERT INTO samples VALUES
                (1, 'x', 5), (2, 'x', NULL), (3, 'x', 3),
                (4, 'x', 8), (5, 'x', 2);",
        )
        .expect("setup succeeds");

    let result = execute_query(
        &mut database,
        "SELECT id,
                COUNT(amount) OVER (
                    PARTITION BY cohort ORDER BY id ROWS BETWEEN 2 PRECEDING AND CURRENT ROW
                ) AS count_value,
                SUM(amount) OVER (
                    PARTITION BY cohort ORDER BY id ROWS BETWEEN 2 PRECEDING AND CURRENT ROW
                ) AS sum_value,
                MIN(amount) OVER (
                    PARTITION BY cohort ORDER BY id ROWS BETWEEN 2 PRECEDING AND CURRENT ROW
                ) AS min_value,
                MAX(amount) OVER (
                    PARTITION BY cohort ORDER BY id ROWS BETWEEN 2 PRECEDING AND CURRENT ROW
                ) AS max_value,
                AVG(amount) OVER (
                    PARTITION BY cohort ORDER BY id ROWS BETWEEN 2 PRECEDING AND CURRENT ROW
                ) AS avg_value
         FROM samples ORDER BY id;",
    );

    assert_eq!(
        result.rows,
        vec![
            vec![
                Value::Int64(1),
                Value::Int64(1),
                Value::Int64(5),
                Value::Int64(5),
                Value::Int64(5),
                Value::Float64(5.0),
            ],
            vec![
                Value::Int64(2),
                Value::Int64(1),
                Value::Int64(5),
                Value::Int64(5),
                Value::Int64(5),
                Value::Float64(5.0),
            ],
            vec![
                Value::Int64(3),
                Value::Int64(2),
                Value::Int64(8),
                Value::Int64(3),
                Value::Int64(5),
                Value::Float64(4.0),
            ],
            vec![
                Value::Int64(4),
                Value::Int64(2),
                Value::Int64(11),
                Value::Int64(3),
                Value::Int64(8),
                Value::Float64(5.5),
            ],
            vec![
                Value::Int64(5),
                Value::Int64(3),
                Value::Int64(13),
                Value::Int64(2),
                Value::Int64(8),
                Value::Float64(13.0 / 3.0),
            ],
        ]
    );
}

#[test]
fn ranking_and_aggregate_windows_share_deterministic_partition_ordering() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE scores (team Int64, id Int64, score Int64);
             INSERT INTO scores VALUES
                (1, 1, 90), (1, 2, 90), (1, 3, 70), (2, 4, 100);",
        )
        .expect("setup succeeds");

    let result = execute_query(
        &mut database,
        "SELECT team, id,
                ROW_NUMBER() OVER (PARTITION BY team ORDER BY score DESC) AS row_number,
                RANK() OVER (PARTITION BY team ORDER BY score DESC) AS rank,
                DENSE_RANK() OVER (PARTITION BY team ORDER BY score DESC) AS dense_rank,
                SUM(score) OVER (
                    PARTITION BY team ORDER BY score DESC
                    ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
                ) AS running_score
         FROM scores ORDER BY team, row_number;",
    );

    assert_eq!(
        result.rows,
        vec![
            vec![
                Value::Int64(1),
                Value::Int64(1),
                Value::Int64(1),
                Value::Int64(1),
                Value::Int64(1),
                Value::Int64(90),
            ],
            vec![
                Value::Int64(1),
                Value::Int64(2),
                Value::Int64(2),
                Value::Int64(1),
                Value::Int64(1),
                Value::Int64(180),
            ],
            vec![
                Value::Int64(1),
                Value::Int64(3),
                Value::Int64(3),
                Value::Int64(3),
                Value::Int64(2),
                Value::Int64(250),
            ],
            vec![
                Value::Int64(2),
                Value::Int64(4),
                Value::Int64(1),
                Value::Int64(1),
                Value::Int64(1),
                Value::Int64(100),
            ],
        ]
    );
}

#[test]
fn aggregate_windows_bound_frames_report_overflow_and_accept_empty_input() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE valueset (cohort Int64, id Int64, amount Int64, value Float64);
             INSERT INTO valueset VALUES
                (1, 1, 9223372036854775807, 1e308), (1, 2, 1, 1e308);",
        )
        .expect("setup succeeds");

    let empty = execute_query(
        &mut database,
        "SELECT SUM(amount) OVER (
             PARTITION BY cohort ORDER BY id
             ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
         ) AS total FROM valueset WHERE id < 0;",
    );
    assert!(empty.rows.is_empty());

    for bound in ["1000001", "99999999999999999999999999999999999999"] {
        let error = database
            .execute(&format!(
                "SELECT SUM(amount) OVER (
                    PARTITION BY cohort ORDER BY id
                    ROWS BETWEEN {bound} PRECEDING AND CURRENT ROW
                 ) FROM valueset;"
            ))
            .expect_err("frame bound is rejected");
        assert!(matches!(error, Error::Sql { .. }));
    }

    let integer_overflow = database
        .execute(
            "SELECT SUM(amount) OVER (
                PARTITION BY cohort ORDER BY id
                ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
             ) FROM valueset;",
        )
        .expect_err("running Int64 sum overflows");
    assert!(matches!(integer_overflow, Error::NumericOverflow(_)));

    let float_overflow = database
        .execute(
            "SELECT SUM(value) OVER (
                PARTITION BY cohort ORDER BY id
                ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
             ) FROM valueset;",
        )
        .expect_err("running Float64 prefix overflows");
    assert!(matches!(float_overflow, Error::NumericOverflow(_)));

    for sql in [
        "SELECT SUM(amount) OVER (PARTITION BY cohort ORDER BY id) FROM valueset;",
        "SELECT SUM(amount) OVER (
            PARTITION BY cohort ORDER BY id RANGE BETWEEN 1 PRECEDING AND CURRENT ROW
         ) FROM valueset;",
        "SELECT RANK() OVER (
            PARTITION BY cohort ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW
         ) FROM valueset;",
    ] {
        assert!(matches!(database.execute(sql), Err(Error::Sql { .. })));
    }
}

#[test]
fn grouped_top_k_retains_deterministic_multi_column_ordering() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE grouped (label String, active Bool, amount Int64);
             INSERT INTO grouped VALUES
                ('zeta', true, 4),
                ('alpha', false, 7),
                ('zeta', true, 3),
                ('beta', true, 7),
                ('alpha', true, 1);",
        )
        .expect("setup succeeds");

    let top = execute_query(
        &mut database,
        "SELECT label, active, COUNT(*) AS rows, SUM(amount) AS total
         FROM grouped
         GROUP BY label, active
         ORDER BY total DESC
         LIMIT 3;",
    );
    assert_eq!(
        top.rows,
        vec![
            vec![
                Value::String("alpha".to_owned()),
                Value::Bool(false),
                Value::Int64(1),
                Value::Int64(7),
            ],
            vec![
                Value::String("beta".to_owned()),
                Value::Bool(true),
                Value::Int64(1),
                Value::Int64(7),
            ],
            vec![
                Value::String("zeta".to_owned()),
                Value::Bool(true),
                Value::Int64(2),
                Value::Int64(7),
            ],
        ]
    );

    let all_groups = execute_query(
        &mut database,
        "SELECT label, active, MIN(amount) AS low
         FROM grouped GROUP BY label, active;",
    );
    assert_eq!(
        all_groups.rows,
        vec![
            vec![
                Value::String("alpha".to_owned()),
                Value::Bool(false),
                Value::Int64(7),
            ],
            vec![
                Value::String("alpha".to_owned()),
                Value::Bool(true),
                Value::Int64(1),
            ],
            vec![
                Value::String("beta".to_owned()),
                Value::Bool(true),
                Value::Int64(7),
            ],
            vec![
                Value::String("zeta".to_owned()),
                Value::Bool(true),
                Value::Int64(3),
            ],
        ]
    );

    let three_columns = execute_query(
        &mut database,
        "SELECT label, active, amount, COUNT(*) AS rows
         FROM grouped
         GROUP BY label, active, amount
         ORDER BY label, active, amount
         LIMIT 2;",
    );
    assert_eq!(
        three_columns.rows,
        vec![
            vec![
                Value::String("alpha".to_owned()),
                Value::Bool(false),
                Value::Int64(7),
                Value::Int64(1),
            ],
            vec![
                Value::String("alpha".to_owned()),
                Value::Bool(true),
                Value::Int64(1),
                Value::Int64(1),
            ],
        ]
    );
}

#[test]
fn every_aggregate_groups_and_uses_declared_result_types() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE sales (category String, amount Int64);
             INSERT INTO sales VALUES
                ('hardware', 10), ('books', 4), ('hardware', 20);",
        )
        .expect("setup succeeds");

    let result = execute_query(
        &mut database,
        "SELECT category,
                COUNT(*) AS rows,
                SUM(amount) AS total,
                MIN(amount) AS low,
                MAX(amount) AS high,
                AVG(amount) AS mean
         FROM sales
         GROUP BY category
         ORDER BY total DESC;",
    );

    assert_eq!(
        result
            .columns
            .iter()
            .map(|column| column.data_type)
            .collect::<Vec<_>>(),
        vec![
            DataType::String,
            DataType::Int64,
            DataType::NullableInt64,
            DataType::NullableInt64,
            DataType::NullableInt64,
            DataType::NullableFloat64,
        ]
    );
    assert_eq!(
        result.rows,
        vec![
            vec![
                Value::String("hardware".to_owned()),
                Value::Int64(2),
                Value::Int64(30),
                Value::Int64(10),
                Value::Int64(20),
                Value::Float64(15.0),
            ],
            vec![
                Value::String("books".to_owned()),
                Value::Int64(1),
                Value::Int64(4),
                Value::Int64(4),
                Value::Int64(4),
                Value::Float64(4.0),
            ],
        ]
    );
}

#[test]
fn global_aggregates_and_empty_count_are_supported() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE measurements (reading Float64);")
        .expect("create succeeds");

    let empty = execute_query(
        &mut database,
        "SELECT COUNT(*) AS count, SUM(reading) AS total FROM measurements;",
    );
    assert_eq!(
        empty
            .columns
            .iter()
            .map(|column| column.data_type)
            .collect::<Vec<_>>(),
        vec![DataType::Int64, DataType::NullableFloat64]
    );
    assert_eq!(empty.rows, vec![vec![Value::Int64(0), Value::Null]]);

    database
        .execute("INSERT INTO measurements VALUES (1.5), (2.5), (6.0);")
        .expect("insert succeeds");
    let result = execute_query(
        &mut database,
        "SELECT COUNT(reading) AS count,
                SUM(reading) AS total,
                MIN(reading) AS low,
                MAX(reading) AS high,
                AVG(reading) AS mean
         FROM measurements;",
    );
    assert_eq!(
        result.rows,
        vec![vec![
            Value::Int64(3),
            Value::Float64(10.0),
            Value::Float64(1.5),
            Value::Float64(6.0),
            Value::Float64(10.0 / 3.0),
        ]]
    );
}

#[test]
fn failed_multi_row_insert_is_atomic_and_actionable() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE events (id Int64, label String);")
        .expect("create succeeds");

    let error = database
        .execute("INSERT INTO events VALUES (1, 'valid'), (2, false);")
        .expect_err("second row has wrong type");
    assert!(matches!(
        error,
        Error::TypeMismatch {
            context,
            expected,
            actual,
        } if context == "column 'events.label'" && expected == "String" && actual == "Bool"
    ));

    let count = execute_query(&mut database, "SELECT COUNT(*) AS count FROM events;");
    assert_eq!(count.rows, vec![vec![Value::Int64(0)]]);

    let error = database
        .execute("INSERT INTO events VALUES (1);")
        .expect_err("row is too short");
    assert!(matches!(
        error,
        Error::RowLength {
            expected: 2,
            actual: 1,
            ..
        }
    ));
}

#[test]
fn invalid_grouping_and_aggregate_types_are_rejected() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE inventory (category String, label String, quantity Int64);
             INSERT INTO inventory VALUES ('tools', 'hammer', 2);",
        )
        .expect("setup succeeds");

    let grouping = database
        .execute("SELECT label, SUM(quantity) FROM inventory GROUP BY category;")
        .expect_err("label is not grouped");
    assert!(
        matches!(grouping, Error::InvalidQuery(message) if message.contains("must appear in GROUP BY"))
    );

    let aggregate = database
        .execute("SELECT SUM(label) FROM inventory;")
        .expect_err("strings cannot be summed");
    assert!(matches!(
        aggregate,
        Error::TypeMismatch {
            expected,
            actual,
            ..
        } if expected == "Int64 or Float64" && actual == "String"
    ));
}

#[test]
fn mixed_numeric_predicates_are_exact_at_f64_and_i64_boundaries() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE boundaries (id Int64);
             INSERT INTO boundaries VALUES
                (-9223372036854775808),
                (9007199254740992),
                (9007199254740993),
                (9223372036854775807);",
        )
        .expect("setup succeeds");

    let above_f64_precision = execute_query(
        &mut database,
        "SELECT id FROM boundaries
         WHERE id > 9007199254740992.0
         ORDER BY id;",
    );
    assert_eq!(
        above_f64_precision.rows,
        vec![
            vec![Value::Int64(9_007_199_254_740_993)],
            vec![Value::Int64(i64::MAX)],
        ]
    );

    let below_i64_upper_bound = execute_query(
        &mut database,
        "SELECT COUNT(*) AS count FROM boundaries
         WHERE id < 9223372036854775808.0;",
    );
    assert_eq!(below_i64_upper_bound.rows, vec![vec![Value::Int64(4)]]);
}

#[test]
fn batch_failure_semantics_distinguish_parse_and_execution_errors() {
    let mut database = Database::new();

    let parse_error = database
        .execute(
            "CREATE TABLE not_applied (id Int64);
             SELECT id FORM not_applied;",
        )
        .expect_err("the complete batch is parsed first");
    assert!(matches!(parse_error, Error::Sql { .. }));
    assert!(matches!(
        database.catalog().table("not_applied"),
        Err(Error::TableNotFound(_))
    ));

    let execution_error = database
        .execute(
            "CREATE TABLE applied (id Int64);
             INSERT INTO applied VALUES (false);",
        )
        .expect_err("execution stops at the invalid insert");
    assert!(matches!(execution_error, Error::TypeMismatch { .. }));
    assert_eq!(
        database
            .catalog()
            .table("applied")
            .expect("earlier CREATE remains applied")
            .row_count(),
        0
    );
}

#[test]
fn avg_int64_accumulates_exactly_before_final_conversion() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE avg_samples (forward_order Bool, value Int64);
             INSERT INTO avg_samples VALUES
                (true, 9007199254740993),
                (true, 1),
                (true, -9007199254740993),
                (false, 9007199254740993),
                (false, -9007199254740993),
                (false, 1);",
        )
        .expect("setup succeeds");

    let result = execute_query(
        &mut database,
        "SELECT forward_order, AVG(value) AS mean
         FROM avg_samples
         GROUP BY forward_order
         ORDER BY forward_order;",
    );
    let one_third = 1.0 / 3.0;
    assert_eq!(
        result.rows,
        vec![
            vec![Value::Bool(false), Value::Float64(one_third)],
            vec![Value::Bool(true), Value::Float64(one_third)],
        ]
    );
}

#[test]
fn boolean_literals_cannot_be_ambiguous_column_names() {
    for identifier in ["true", "FALSE", "null"] {
        let mut database = Database::new();
        let error = database
            .execute(&format!(
                "CREATE TABLE reserved_names ({identifier} Bool, id Int64)"
            ))
            .expect_err("Boolean literal names are reserved");

        assert!(matches!(
            error,
            Error::ReservedIdentifier {
                identifier: rejected,
                context,
            } if rejected.eq_ignore_ascii_case(identifier) && context == "column name"
        ));
        assert!(matches!(
            database.catalog().table("reserved_names"),
            Err(Error::TableNotFound(_))
        ));
    }
}

#[test]
fn nullable_columns_follow_sql_null_semantics_end_to_end() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (
                id Int64,
                category Nullable(String),
                amount Nullable(Int64),
                enabled Nullable(Bool)
             );
             INSERT INTO samples VALUES
                (1, NULL, NULL, NULL),
                (2, NULL, 5, true),
                (3, 'a', NULL, false),
                (4, 'a', 7, true),
                (5, 'b', 3, NULL),
                (6, 'c', NULL, false);",
        )
        .expect("nullable setup succeeds");

    let projection = execute_query(
        &mut database,
        "SELECT id, category, amount FROM samples
         WHERE amount = 5 OR category IS NULL
         ORDER BY amount;",
    );
    assert_eq!(
        projection
            .columns
            .iter()
            .map(|column| column.data_type)
            .collect::<Vec<_>>(),
        vec![
            DataType::Int64,
            DataType::NullableString,
            DataType::NullableInt64,
        ]
    );
    assert_eq!(
        projection.rows,
        vec![
            vec![Value::Int64(2), Value::Null, Value::Int64(5)],
            vec![Value::Int64(1), Value::Null, Value::Null],
        ]
    );

    let descending = execute_query(
        &mut database,
        "SELECT id, amount FROM samples ORDER BY amount DESC LIMIT 4;",
    );
    assert_eq!(
        descending.rows,
        vec![
            vec![Value::Int64(1), Value::Null],
            vec![Value::Int64(3), Value::Null],
            vec![Value::Int64(6), Value::Null],
            vec![Value::Int64(4), Value::Int64(7)],
        ]
    );

    let unknown_comparison = execute_query(
        &mut database,
        "SELECT id FROM samples
         WHERE amount = NULL OR enabled = true
         ORDER BY id;",
    );
    assert_eq!(
        unknown_comparison.rows,
        vec![vec![Value::Int64(2)], vec![Value::Int64(4)]]
    );

    let non_null = execute_query(
        &mut database,
        "SELECT id FROM samples WHERE amount IS NOT NULL ORDER BY id;",
    );
    assert_eq!(
        non_null.rows,
        vec![
            vec![Value::Int64(2)],
            vec![Value::Int64(4)],
            vec![Value::Int64(5)],
        ]
    );

    let grouped = execute_query(
        &mut database,
        "SELECT category,
                COUNT(*) AS rows,
                COUNT(amount) AS present,
                SUM(amount) AS total,
                MIN(amount) AS low,
                MAX(amount) AS high,
                AVG(amount) AS mean
         FROM samples
         GROUP BY category
         ORDER BY category;",
    );
    assert_eq!(
        grouped.rows,
        vec![
            vec![
                Value::String("a".to_owned()),
                Value::Int64(2),
                Value::Int64(1),
                Value::Int64(7),
                Value::Int64(7),
                Value::Int64(7),
                Value::Float64(7.0),
            ],
            vec![
                Value::String("b".to_owned()),
                Value::Int64(1),
                Value::Int64(1),
                Value::Int64(3),
                Value::Int64(3),
                Value::Int64(3),
                Value::Float64(3.0),
            ],
            vec![
                Value::String("c".to_owned()),
                Value::Int64(1),
                Value::Int64(0),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ],
            vec![
                Value::Null,
                Value::Int64(2),
                Value::Int64(1),
                Value::Int64(5),
                Value::Int64(5),
                Value::Int64(5),
                Value::Float64(5.0),
            ],
        ]
    );

    let empty = execute_query(
        &mut database,
        "SELECT SUM(amount) AS total, MIN(amount) AS low,
                MAX(amount) AS high, AVG(amount) AS mean
         FROM samples WHERE id < 0;",
    );
    assert_eq!(
        empty.rows,
        vec![vec![Value::Null, Value::Null, Value::Null, Value::Null]]
    );
}

#[test]
fn null_is_rejected_by_non_nullable_columns_without_partial_insert() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE required (id Int64);")
        .expect("create succeeds");

    let error = database
        .execute("INSERT INTO required VALUES (1), (NULL);")
        .expect_err("NULL is invalid for Int64");
    assert!(matches!(
        error,
        Error::TypeMismatch {
            expected,
            actual,
            ..
        } if expected == "Int64" && actual == "NULL"
    ));
    let count = execute_query(&mut database, "SELECT COUNT(*) FROM required;");
    assert_eq!(count.rows, vec![vec![Value::Int64(0)]]);
}

#[test]
fn public_schema_construction_rejects_the_null_literal_type() {
    let error = Table::new(
        "invalid".to_owned(),
        vec![ColumnDef {
            name: "value".to_owned(),
            data_type: DataType::Null,
        }],
    )
    .expect_err("the NULL literal type is not a physical column type");

    assert!(matches!(
        error,
        Error::InvalidQuery(message)
            if message == "column 'invalid.value' cannot use NULL as a data type; use Nullable(T)"
    ));
}

#[test]
fn public_column_construction_rejects_the_null_literal_type() {
    let error = Column::new(DataType::Null)
        .expect_err("the NULL literal type is not a physical column type");

    assert!(matches!(
        error,
        Error::InvalidQuery(message)
            if message == "NULL is a literal type, not a physical column type; use Nullable(T)"
    ));
}

#[test]
fn creates_a_fifty_thousand_column_schema() {
    let column_count = 50_000;
    let definitions = (0..column_count)
        .map(|index| format!("c{index} Int64"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut database = Database::new();

    database
        .execute(&format!("CREATE TABLE wide ({definitions})"))
        .expect("wide schema should validate in linear time");

    assert_eq!(
        database
            .catalog()
            .table("wide")
            .expect("wide table exists")
            .schema()
            .len(),
        column_count
    );
}
