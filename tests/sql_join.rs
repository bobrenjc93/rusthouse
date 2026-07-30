use rusthouse::{Database, Error, JoinLimits, QueryResult, StatementResult, Value};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    match database
        .execute(sql)
        .expect("SQL succeeds")
        .into_iter()
        .last()
        .expect("statement result")
    {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

#[test]
fn duplicate_keys_keep_left_then_right_input_order_when_left_is_hashed() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE left_rows (key Int64, name String);
             INSERT INTO left_rows VALUES (1, 'left one'), (1, 'left two');
             CREATE TABLE right_rows (key Int64, name String);
             INSERT INTO right_rows VALUES
                (1, 'right one'), (1, 'right two'), (1, 'right three');",
        )
        .expect("setup succeeds");

    let result = query(
        &mut database,
        "SELECT l.name AS left_name, r.name AS right_name
         FROM left_rows AS l
         INNER JOIN right_rows AS r ON l.key = r.key;",
    );

    assert_eq!(
        result.rows,
        vec![
            vec![
                Value::String("left one".into()),
                Value::String("right one".into())
            ],
            vec![
                Value::String("left one".into()),
                Value::String("right two".into())
            ],
            vec![
                Value::String("left one".into()),
                Value::String("right three".into())
            ],
            vec![
                Value::String("left two".into()),
                Value::String("right one".into())
            ],
            vec![
                Value::String("left two".into()),
                Value::String("right two".into())
            ],
            vec![
                Value::String("left two".into()),
                Value::String("right three".into())
            ],
        ]
    );
}

#[test]
fn duplicate_keys_keep_left_then_right_input_order_when_right_is_hashed() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE larger_left (key Int64, name String);
             INSERT INTO larger_left VALUES
                (1, 'left one'), (2, 'unmatched'), (1, 'left two');
             CREATE TABLE smaller_right (key Int64, name String);
             INSERT INTO smaller_right VALUES (1, 'right one'), (1, 'right two');",
        )
        .expect("setup succeeds");

    let result = query(
        &mut database,
        "SELECT l.name AS left_name, r.name AS right_name
         FROM larger_left AS l
         INNER JOIN smaller_right AS r ON l.key = r.key;",
    );

    assert_eq!(
        result.rows,
        vec![
            vec![
                Value::String("left one".into()),
                Value::String("right one".into()),
            ],
            vec![
                Value::String("left one".into()),
                Value::String("right two".into()),
            ],
            vec![
                Value::String("left two".into()),
                Value::String("right one".into()),
            ],
            vec![
                Value::String("left two".into()),
                Value::String("right two".into()),
            ],
        ]
    );
}

#[test]
fn composite_join_supports_filtering_grouping_aggregation_ordering_and_limit() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE customers (id Int64, region String);
             INSERT INTO customers VALUES (1, 'west'), (2, 'east'), (3, 'west');
             CREATE TABLE orders (customer_id Int64, region String, amount Int64, active Bool);
             INSERT INTO orders VALUES
                (1, 'west', 10, true),
                (1, 'west', 4, false),
                (2, 'east', 7, true),
                (3, 'wrong', 100, true),
                (3, 'west', 5, true);",
        )
        .expect("setup succeeds");

    let result = query(
        &mut database,
        "SELECT c.region, COUNT(*) AS orders, SUM(o.amount) AS total
         FROM customers c
         INNER JOIN orders o
           ON c.id = o.customer_id AND c.region = o.region
         WHERE o.active = true
         GROUP BY c.region
         ORDER BY total DESC
         LIMIT 1;",
    );

    assert_eq!(
        result.rows,
        vec![vec![
            Value::String("west".into()),
            Value::Int64(2),
            Value::Int64(15),
        ]]
    );
}

#[test]
fn self_join_uses_distinct_aliases_for_qualified_columns() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE employees (id Int64, manager_id Int64, name String);
             INSERT INTO employees VALUES
                (1, 1, 'chief'), (2, 1, 'Ada'),
                (3, 1, 'Linus'), (4, 2, 'Grace');",
        )
        .expect("setup succeeds");

    let result = query(
        &mut database,
        "SELECT employee.id, employee.name AS employee_name, manager.name AS manager_name
         FROM employees employee
         INNER JOIN employees AS manager ON employee.manager_id = manager.id
         ORDER BY employee.id;",
    );

    assert_eq!(
        result.rows,
        vec![
            vec![
                Value::Int64(1),
                Value::String("chief".into()),
                Value::String("chief".into()),
            ],
            vec![
                Value::Int64(2),
                Value::String("Ada".into()),
                Value::String("chief".into()),
            ],
            vec![
                Value::Int64(3),
                Value::String("Linus".into()),
                Value::String("chief".into()),
            ],
            vec![
                Value::Int64(4),
                Value::String("Grace".into()),
                Value::String("Ada".into()),
            ],
        ]
    );
}

#[test]
fn ambiguous_columns_require_qualification() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE first (id Int64); INSERT INTO first VALUES (1);
             CREATE TABLE second (id Int64); INSERT INTO second VALUES (1);",
        )
        .expect("setup succeeds");

    let error = database
        .execute("SELECT id FROM first f INNER JOIN second s ON f.id = s.id;")
        .expect_err("unqualified id is ambiguous");
    assert!(matches!(
        error,
        Error::InvalidQuery(message)
            if message.contains("column reference 'id' is ambiguous")
    ));

    let result = query(
        &mut database,
        "SELECT f.id AS first_id, s.id AS second_id
         FROM first f INNER JOIN second s ON f.id = s.id;",
    );
    assert_eq!(result.rows, vec![vec![Value::Int64(1), Value::Int64(1)]]);
}

#[test]
fn join_keys_must_connect_the_tables_and_have_comparable_types() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE typed_left (id Int64, label String);
             CREATE TABLE typed_right (id Int64);",
        )
        .expect("setup succeeds");

    let same_side = database
        .execute(
            "SELECT l.id FROM typed_left l
             INNER JOIN typed_right r ON l.id = l.id;",
        )
        .expect_err("the join still requires a cross-table equality key");
    assert!(matches!(
        same_side,
        Error::InvalidQuery(message)
            if message.contains("requires at least one equality connecting the input tables")
    ));

    let mismatched = database
        .execute(
            "SELECT l.id FROM typed_left l
             INNER JOIN typed_right r ON l.label = r.id;",
        )
        .expect_err("join key types must be comparable");
    assert!(matches!(
        mismatched,
        Error::TypeMismatch {
            context,
            expected,
            actual,
        } if context == "INNER JOIN equality" && expected == "String" && actual == "Int64"
    ));
}

#[test]
fn same_side_equalities_are_residual_on_predicates() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE residual_left (id Int64, a Int64, b Int64, label String);
             INSERT INTO residual_left VALUES
                (1, 5, 5, 'matched'),
                (1, 5, 6, 'left rejected'),
                (2, 7, 7, 'right rejected');
             CREATE TABLE residual_right (id Int64, a Int64, b Int64, label String);
             INSERT INTO residual_right VALUES
                (1, 9, 9, 'kept'),
                (1, 9, 8, 'right rejected'),
                (2, 3, 4, 'also rejected');",
        )
        .expect("setup succeeds");

    let result = query(
        &mut database,
        "SELECT l.label AS left_label, r.label AS right_label
         FROM residual_left l LEFT JOIN residual_right r
           ON l.id = r.id AND l.a = l.b AND r.a = r.b;",
    );

    assert_eq!(
        result.rows,
        vec![
            vec![
                Value::String("matched".into()),
                Value::String("kept".into())
            ],
            vec![Value::String("left rejected".into()), Value::Null],
            vec![Value::String("right rejected".into()), Value::Null],
        ]
    );
}

#[test]
fn empty_inputs_produce_empty_joins_and_zero_global_counts() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE empty_rows (id Int64);
             CREATE TABLE populated (id Int64);
             INSERT INTO populated VALUES (1), (2);",
        )
        .expect("setup succeeds");

    for sql in [
        "SELECT COUNT(*) AS count FROM empty_rows e
         INNER JOIN populated p ON e.id = p.id;",
        "SELECT COUNT(*) AS count FROM populated p
         INNER JOIN empty_rows e ON p.id = e.id;",
    ] {
        assert_eq!(query(&mut database, sql).rows, vec![vec![Value::Int64(0)]]);
    }
}

#[test]
fn mixed_numeric_join_keys_are_exact_at_i64_and_f64_boundaries() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE integers (value Int64);
             INSERT INTO integers VALUES
                (-9223372036854775808),
                (9007199254740992),
                (9007199254740993),
                (9223372036854775807);
             CREATE TABLE floats (value Float64);
             INSERT INTO floats VALUES
                (-9223372036854775808.0),
                (9007199254740992.0),
                (9007199254740993.0),
                (9223372036854775808.0);",
        )
        .expect("setup succeeds");

    let result = query(
        &mut database,
        "SELECT i.value
         FROM integers i INNER JOIN floats f ON i.value = f.value;",
    );
    assert_eq!(
        result.rows,
        vec![
            vec![Value::Int64(i64::MIN)],
            vec![Value::Int64(9_007_199_254_740_992)],
            vec![Value::Int64(9_007_199_254_740_992)],
        ]
    );
}

#[test]
fn hash_build_row_and_byte_limits_are_configurable_and_enforced() {
    let setup = "CREATE TABLE larger (id Int64);
                 INSERT INTO larger VALUES (1), (2), (3);
                 CREATE TABLE smaller (id Int64);
                 INSERT INTO smaller VALUES (1), (2);";

    let mut row_limited = Database::with_join_limits(JoinLimits {
        max_rows: 1,
        max_bytes: usize::MAX,
        max_candidate_pairs: usize::MAX,
    });
    row_limited.execute(setup).expect("setup succeeds");
    let error = row_limited
        .execute("SELECT larger.id FROM larger INNER JOIN smaller ON larger.id = smaller.id;")
        .expect_err("two-row smaller input exceeds row limit");
    assert!(matches!(
        error,
        Error::JoinLimitExceeded {
            resource: "rows",
            limit: 1,
            actual: 2,
        }
    ));

    let mut byte_limited = Database::with_join_limits(JoinLimits {
        max_rows: 10,
        max_bytes: 0,
        max_candidate_pairs: usize::MAX,
    });
    byte_limited.execute(setup).expect("setup succeeds");
    let error = byte_limited
        .execute("SELECT larger.id FROM larger INNER JOIN smaller ON larger.id = smaller.id;")
        .expect_err("nonempty hash table exceeds zero-byte limit");
    assert!(matches!(
        error,
        Error::JoinLimitExceeded {
            resource: "bytes",
            limit: 0,
            actual,
        } if actual > 0
    ));
}

#[test]
fn duplicate_key_fanout_is_bounded_before_output_allocation() {
    let mut database = Database::with_join_limits(JoinLimits {
        max_rows: 100,
        max_bytes: usize::MAX,
        max_candidate_pairs: usize::MAX,
    });
    database
        .execute(
            "CREATE TABLE fanout_left (key Int64);
             INSERT INTO fanout_left VALUES
                (1), (1), (1), (1), (1), (1),
                (1), (1), (1), (1), (1), (1);
             CREATE TABLE fanout_right (key Int64);
             INSERT INTO fanout_right VALUES
                (1), (1), (1), (1), (1), (1),
                (1), (1), (1), (1), (1), (1);",
        )
        .expect("setup succeeds within build limits");

    let error = database
        .execute(
            "SELECT l.key
             FROM fanout_left l INNER JOIN fanout_right r ON l.key = r.key
             LIMIT 0;",
        )
        .expect_err("12-by-12 fanout exceeds the output row bound");
    assert!(matches!(
        error,
        Error::JoinLimitExceeded {
            resource: "output rows",
            limit: 100,
            actual: 101,
        }
    ));
}

#[test]
fn fanout_row_limit_stops_at_first_excess_for_both_hash_sides() {
    for (left_count, right_count) in [(2, 3), (3, 2)] {
        let mut database = Database::with_join_limits(JoinLimits {
            max_rows: 4,
            max_bytes: usize::MAX,
            max_candidate_pairs: usize::MAX,
        });
        let left_values = vec!["(1)"; left_count].join(",");
        let right_values = vec!["(1)"; right_count].join(",");
        database
            .execute(&format!(
                "CREATE TABLE early_left (key Int64);
                 INSERT INTO early_left VALUES {left_values};
                 CREATE TABLE early_right (key Int64);
                 INSERT INTO early_right VALUES {right_values};"
            ))
            .expect("setup succeeds");

        let error = database
            .execute(
                "SELECT l.key FROM early_left l
                 INNER JOIN early_right r ON l.key = r.key;",
            )
            .expect_err("six-row fanout must stop at the fifth row");
        assert!(matches!(
            error,
            Error::JoinLimitExceeded {
                resource: "output rows",
                limit: 4,
                actual: 5,
            }
        ));
    }
}

#[test]
fn residual_on_candidate_limit_bounds_both_hash_sides() {
    for (left_count, right_count) in [(2, 3), (3, 2)] {
        let mut database = Database::with_join_limits(JoinLimits {
            max_rows: 10,
            max_bytes: usize::MAX,
            max_candidate_pairs: 4,
        });
        let left_values = vec!["(1)"; left_count].join(",");
        let right_values = vec!["(1)"; right_count].join(",");
        database
            .execute(&format!(
                "CREATE TABLE work_left (key Int64);
                 INSERT INTO work_left VALUES {left_values};
                 CREATE TABLE work_right (key Int64);
                 INSERT INTO work_right VALUES {right_values};"
            ))
            .expect("setup succeeds");

        let error = database
            .execute(
                "SELECT l.key FROM work_left l
                 INNER JOIN work_right r ON l.key = r.key AND 1 = 0;",
            )
            .expect_err("false residual must not bypass candidate work bounds");
        assert!(matches!(
            error,
            Error::JoinLimitExceeded {
                resource: "candidate pairs",
                limit: 4,
                actual: 5,
            }
        ));
    }
}

#[test]
fn join_fanout_working_bytes_are_bounded_separately_from_build_bytes() {
    let mut database = Database::with_join_limits(JoinLimits {
        max_rows: 100,
        max_bytes: 512,
        max_candidate_pairs: usize::MAX,
    });
    database
        .execute(
            "CREATE TABLE one_key (key Int64); INSERT INTO one_key VALUES (1);
             CREATE TABLE ten_keys (key Int64);
             INSERT INTO ten_keys VALUES
                (1), (1), (1), (1), (1), (1), (1), (1), (1), (1);",
        )
        .expect("setup succeeds");

    let error = database
        .execute("SELECT l.key FROM one_key l INNER JOIN ten_keys r ON l.key = r.key;")
        .expect_err("output working memory exceeds the byte bound");
    assert!(matches!(
        error,
        Error::JoinLimitExceeded {
            resource: "bytes",
            limit: 512,
            actual,
        } if actual > 512
    ));
}

#[test]
fn distinct_key_hash_table_allocations_count_toward_byte_limit() {
    let row_count = 1_024;
    let left_values = (0..row_count)
        .map(|value| format!("({value})"))
        .collect::<Vec<_>>()
        .join(",");
    let right_values = (row_count..row_count * 2)
        .map(|value| format!("({value})"))
        .collect::<Vec<_>>()
        .join(",");
    let mut database = Database::with_join_limits(JoinLimits {
        max_rows: row_count * 2,
        max_bytes: 64 * 1024,
        max_candidate_pairs: usize::MAX,
    });
    database
        .execute(&format!(
            "CREATE TABLE distinct_left (key Int64);
             INSERT INTO distinct_left VALUES {left_values};
             CREATE TABLE distinct_right (key Int64);
             INSERT INTO distinct_right VALUES {right_values};"
        ))
        .expect("setup succeeds");

    let error = database
        .execute(
            "SELECT l.key
             FROM distinct_left l INNER JOIN distinct_right r ON l.key = r.key;",
        )
        .expect_err("bucket and entry arrays exceed the byte limit before probing");
    assert!(matches!(
        error,
        Error::JoinLimitExceeded {
            resource: "bytes",
            limit: 65_536,
            actual,
        } if actual > 65_536
    ));
}

#[test]
fn boolean_named_aliases_are_qualified_columns_in_where() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE alias_left (id Int64); INSERT INTO alias_left VALUES (1), (2);
             CREATE TABLE alias_right (id Int64); INSERT INTO alias_right VALUES (1), (3);",
        )
        .expect("setup succeeds");

    let result = query(
        &mut database,
        "SELECT true.id AS left_id, false.id AS right_id
         FROM alias_left AS true
         INNER JOIN alias_right AS false ON true.id = false.id
         WHERE true.id = 1 AND false.id = 1;",
    );
    assert_eq!(result.rows, vec![vec![Value::Int64(1), Value::Int64(1)]]);
}

#[test]
fn left_outer_join_preserves_duplicate_composite_and_null_keys() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE left_facts (
                account_id Nullable(Int64), region Nullable(String), label String
             );
             INSERT INTO left_facts VALUES
                (1, 'west', 'first'),
                (1, 'west', 'second'),
                (2, 'east', 'unmatched'),
                (NULL, 'west', 'null id'),
                (3, NULL, 'null region'),
                (4, 'north', 'extra unmatched');
             CREATE TABLE enrichments (
                account_id Nullable(Int64), region Nullable(String), detail String
             );
             INSERT INTO enrichments VALUES
                (1, 'west', 'alpha'),
                (1, 'west', 'beta'),
                (1, 'east', 'wrong region'),
                (NULL, 'west', 'null id right'),
                (3, NULL, 'null region right');",
        )
        .expect("setup succeeds");

    let result = query(
        &mut database,
        "SELECT facts.label AS left_label, info.detail AS right_detail
         FROM left_facts AS facts
         LEFT OUTER JOIN enrichments info
           ON facts.account_id = info.account_id AND facts.region = info.region;",
    );

    assert!(result.columns[1].nullable);
    assert_eq!(
        result.rows,
        vec![
            vec![Value::String("first".into()), Value::String("alpha".into())],
            vec![Value::String("first".into()), Value::String("beta".into())],
            vec![
                Value::String("second".into()),
                Value::String("alpha".into())
            ],
            vec![Value::String("second".into()), Value::String("beta".into())],
            vec![Value::String("unmatched".into()), Value::Null],
            vec![Value::String("null id".into()), Value::Null],
            vec![Value::String("null region".into()), Value::Null],
            vec![Value::String("extra unmatched".into()), Value::Null],
        ]
    );
}

#[test]
fn left_join_applies_on_before_extension_and_where_afterward() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE subjects (id Int64); INSERT INTO subjects VALUES (1), (2), (3);
             CREATE TABLE flags (id Int64, enabled Bool);
             INSERT INTO flags VALUES
                (1, false), (2, true), (4, false), (5, true);",
        )
        .expect("setup succeeds");

    let filtered_on = query(
        &mut database,
        "SELECT s.id, f.enabled
         FROM subjects s LEFT JOIN flags f
           ON s.id = f.id AND f.enabled = true
         ORDER BY s.id;",
    );
    assert_eq!(
        filtered_on.rows,
        vec![
            vec![Value::Int64(1), Value::Null],
            vec![Value::Int64(2), Value::Bool(true)],
            vec![Value::Int64(3), Value::Null],
        ]
    );

    let filtered_where = query(
        &mut database,
        "SELECT s.id, f.enabled
         FROM subjects s LEFT JOIN flags f ON s.id = f.id
         WHERE f.enabled = true
         ORDER BY s.id;",
    );
    assert_eq!(
        filtered_where.rows,
        vec![vec![Value::Int64(2), Value::Bool(true)]]
    );
}

#[test]
fn left_join_empty_inputs_aggregation_ordering_and_limit_are_coherent() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE dimensions (id Int64, name String);
             INSERT INTO dimensions VALUES (1, 'one'), (2, 'two'), (3, 'three');
             CREATE TABLE empty_facts (id Int64, amount Int64);",
        )
        .expect("setup succeeds");

    let empty_left = query(
        &mut database,
        "SELECT COUNT(*) AS rows
         FROM empty_facts f LEFT JOIN dimensions d ON f.id = d.id;",
    );
    assert_eq!(empty_left.rows, vec![vec![Value::Int64(0)]]);

    let grouped = query(
        &mut database,
        "SELECT d.name, COUNT(*) AS rows, COUNT(f.id) AS matches, SUM(f.amount) AS total
         FROM dimensions d LEFT JOIN empty_facts f ON d.id = f.id
         GROUP BY d.name
         ORDER BY d.name
         LIMIT 2;",
    );
    assert_eq!(
        grouped.rows,
        vec![
            vec![
                Value::String("one".into()),
                Value::Int64(1),
                Value::Int64(0),
                Value::Null,
            ],
            vec![
                Value::String("three".into()),
                Value::Int64(1),
                Value::Int64(0),
                Value::Null,
            ],
        ]
    );
    assert!(grouped.columns[3].nullable);
}

#[test]
fn left_join_unmatched_output_obeys_operator_bounds_before_sql_limit() {
    let mut database = Database::with_join_limits(JoinLimits {
        max_rows: 2,
        max_bytes: usize::MAX,
        max_candidate_pairs: usize::MAX,
    });
    database
        .execute(
            "CREATE TABLE bounded_left (id Int64);
             INSERT INTO bounded_left VALUES (1), (2), (3);
             CREATE TABLE bounded_right (id Int64);",
        )
        .expect("setup succeeds");

    let error = database
        .execute(
            "SELECT l.id FROM bounded_left l
             LEFT JOIN bounded_right r ON l.id = r.id LIMIT 1;",
        )
        .expect_err("null-extended operator output exceeds the bound before LIMIT");
    assert!(matches!(
        error,
        Error::JoinLimitExceeded {
            resource: "output rows",
            limit: 2,
            actual: 3,
        }
    ));
}
