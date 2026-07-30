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
            DataType::Int64,
            DataType::Int64,
            DataType::Int64,
            DataType::Float64,
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
    assert_eq!(empty.rows, vec![vec![Value::Int64(0), Value::Float64(0.0)]]);

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
    for identifier in ["true", "FALSE"] {
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

#[test]
fn bounded_predicates_compose_with_grouping_ordering_and_limit() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE items (id Int64, category String, label String);
             INSERT INTO items VALUES
                (1, 'a', 'alpha'),
                (2, 'a', 'beta'),
                (3, 'b', 'gamma'),
                (4, 'b', 'delta'),
                (5, 'b', 'omega');",
        )
        .expect("setup succeeds");

    let result = execute_query(
        &mut database,
        "SELECT category, COUNT(*) AS rows, SUM(id) AS total
         FROM items
         WHERE id IN (1, 1, 2) OR id BETWEEN 4 AND 5 AND label NOT LIKE 'om%'
         GROUP BY category
         ORDER BY total DESC
         LIMIT 2;",
    );

    assert_eq!(
        result.rows,
        vec![
            vec![
                Value::String("b".to_owned()),
                Value::Int64(1),
                Value::Int64(4),
            ],
            vec![
                Value::String("a".to_owned()),
                Value::Int64(2),
                Value::Int64(3),
            ],
        ]
    );

    let grouped = execute_query(
        &mut database,
        "SELECT id FROM items
         WHERE (id IN (1, 2) OR id BETWEEN 4 AND 5) AND label LIKE '%a'
         ORDER BY id DESC;",
    );
    assert_eq!(
        grouped.rows,
        vec![
            vec![Value::Int64(5)],
            vec![Value::Int64(4)],
            vec![Value::Int64(2)],
            vec![Value::Int64(1)],
        ]
    );
}

#[test]
fn empty_and_duplicate_in_lists_have_bounded_set_semantics() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE numbers (id Int64);
             INSERT INTO numbers VALUES (1), (2), (3);",
        )
        .expect("setup succeeds");

    assert!(
        execute_query(&mut database, "SELECT id FROM numbers WHERE id IN ();")
            .rows
            .is_empty()
    );
    assert_eq!(
        execute_query(
            &mut database,
            "SELECT id FROM numbers WHERE id NOT IN () ORDER BY id;",
        )
        .rows,
        vec![
            vec![Value::Int64(1)],
            vec![Value::Int64(2)],
            vec![Value::Int64(3)],
        ]
    );
    assert_eq!(
        execute_query(
            &mut database,
            "SELECT id FROM numbers WHERE id IN (2, 2.0, 2, 3) ORDER BY id;",
        )
        .rows,
        vec![vec![Value::Int64(2)], vec![Value::Int64(3)]]
    );
}

#[test]
fn in_and_between_preserve_exact_mixed_numeric_boundaries() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE exact_numbers (id Int64);
             INSERT INTO exact_numbers VALUES
                (-9223372036854775808),
                (9007199254740992),
                (9007199254740993),
                (9223372036854775807);",
        )
        .expect("setup succeeds");

    let in_set = execute_query(
        &mut database,
        "SELECT id FROM exact_numbers
         WHERE id IN (
            -9223372036854775808.0,
            9007199254740992.0,
            9223372036854775808.0
         )
         ORDER BY id;",
    );
    assert_eq!(
        in_set.rows,
        vec![
            vec![Value::Int64(i64::MIN)],
            vec![Value::Int64(9_007_199_254_740_992)],
        ]
    );

    let singleton_range = execute_query(
        &mut database,
        "SELECT id FROM exact_numbers
         WHERE id BETWEEN 9007199254740992.0 AND 9007199254740992.0;",
    );
    assert_eq!(
        singleton_range.rows,
        vec![vec![Value::Int64(9_007_199_254_740_992)]]
    );

    let inclusive_range = execute_query(
        &mut database,
        "SELECT id FROM exact_numbers
         WHERE id NOT BETWEEN 9007199254740993 AND 9223372036854775807
         ORDER BY id;",
    );
    assert_eq!(
        inclusive_range.rows,
        vec![
            vec![Value::Int64(i64::MIN)],
            vec![Value::Int64(9_007_199_254_740_992)],
        ]
    );
}

#[test]
fn like_supports_wildcards_negation_and_explicit_escaping() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE patterns (id Int64, label String);
             INSERT INTO patterns VALUES
                (1, '100% done'),
                (2, 'a_b'),
                (3, 'a!b'),
                (4, 'axb'),
                (5, 'plain');",
        )
        .expect("setup succeeds");

    let escaped = execute_query(
        &mut database,
        "SELECT id FROM patterns
         WHERE label LIKE '100!%%' ESCAPE '!'
            OR label LIKE 'a!_b' ESCAPE '!'
            OR label LIKE 'a!!b' ESCAPE '!'
         ORDER BY id;",
    );
    assert_eq!(
        escaped.rows,
        vec![
            vec![Value::Int64(1)],
            vec![Value::Int64(2)],
            vec![Value::Int64(3)],
        ]
    );

    let wildcard = execute_query(
        &mut database,
        "SELECT id FROM patterns WHERE label LIKE 'a_b' ORDER BY id;",
    );
    assert_eq!(
        wildcard.rows,
        vec![
            vec![Value::Int64(2)],
            vec![Value::Int64(3)],
            vec![Value::Int64(4)],
        ]
    );

    let negated = execute_query(
        &mut database,
        "SELECT id FROM patterns
         WHERE label NOT LIKE '%!%%' ESCAPE '!'
         ORDER BY id LIMIT 2;",
    );
    assert_eq!(
        negated.rows,
        vec![vec![Value::Int64(2)], vec![Value::Int64(3)]]
    );
}

#[test]
fn wildcard_matching_handles_long_adversarial_inputs_iteratively() {
    let value = "a".repeat(50_000);
    let pattern = format!("%{}b", "a".repeat(4_094));
    let mut database = Database::new();
    database
        .execute("CREATE TABLE long_patterns (label String);")
        .expect("create succeeds");
    database
        .execute(&format!("INSERT INTO long_patterns VALUES ('{value}');"))
        .expect("insert succeeds");

    let result = execute_query(
        &mut database,
        &format!("SELECT label FROM long_patterns WHERE label LIKE '{pattern}';"),
    );
    assert!(result.rows.is_empty());
}

#[test]
fn bounded_predicates_reject_invalid_types_and_escapes() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE typed_predicates (id Int64, label String);")
        .expect("create succeeds");

    for (sql, expected_context) in [
        (
            "SELECT id FROM typed_predicates WHERE id IN (1, 'two');",
            "WHERE IN list value 2",
        ),
        (
            "SELECT id FROM typed_predicates WHERE id BETWEEN 1 AND 'two';",
            "WHERE BETWEEN upper bound",
        ),
        (
            "SELECT id FROM typed_predicates WHERE id LIKE '%';",
            "WHERE LIKE operand",
        ),
    ] {
        let error = database.execute(sql).expect_err("types are incompatible");
        assert!(
            matches!(error, Error::TypeMismatch { context, .. } if context == expected_context)
        );
    }

    let dangling_escape = database
        .execute("SELECT id FROM typed_predicates WHERE label LIKE 'abc!' ESCAPE '!';")
        .expect_err("escape must quote a pattern character");
    assert!(matches!(
        dangling_escape,
        Error::InvalidQuery(message) if message.contains("ends with its escape character")
    ));

    for escape in ["''", "'ab'"] {
        let error = database
            .execute(&format!(
                "SELECT id FROM typed_predicates WHERE label LIKE '%' ESCAPE {escape};"
            ))
            .expect_err("escape is not exactly one character");
        assert!(matches!(
            error,
            Error::Sql { message, .. } if message.contains("ESCAPE must be exactly one character")
        ));
    }
}
