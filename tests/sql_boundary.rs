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
fn update_and_delete_report_zero_and_all_matches() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE lifecycle (id Int64, enabled Bool);
             INSERT INTO lifecycle VALUES
                (1, false), (2, false), (3, false), (4, false);",
        )
        .expect("setup succeeds");

    assert_eq!(
        database
            .execute("UPDATE lifecycle SET enabled = true WHERE id < 0;")
            .expect("zero-row update succeeds"),
        vec![StatementResult::Command {
            tag: "UPDATE",
            affected_rows: 0,
        }]
    );
    assert_eq!(
        database
            .execute("DELETE FROM lifecycle WHERE id < 0;")
            .expect("zero-row delete succeeds"),
        vec![StatementResult::Command {
            tag: "DELETE",
            affected_rows: 0,
        }]
    );
    assert_eq!(
        database
            .execute("UPDATE lifecycle SET enabled = true WHERE id >= 1;")
            .expect("all-row update succeeds"),
        vec![StatementResult::Command {
            tag: "UPDATE",
            affected_rows: 4,
        }]
    );
    assert_eq!(
        database
            .execute("DELETE FROM lifecycle WHERE enabled = true;")
            .expect("all-row delete succeeds"),
        vec![StatementResult::Command {
            tag: "DELETE",
            affected_rows: 4,
        }]
    );

    let table = database.catalog().table("lifecycle").expect("table exists");
    assert_eq!(table.row_count(), 0);
    assert!(table.columns().iter().all(|column| column.is_empty()));
}

#[test]
fn multi_column_update_assignments_read_the_old_row() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE swaps (id Int64, left_value Int64, right_value Int64, total Int64);
             INSERT INTO swaps VALUES (1, 10, 20, 0), (2, 30, 40, 70);",
        )
        .expect("setup succeeds");

    let result = database
        .execute(
            "UPDATE swaps
             SET left_value = right_value,
                 right_value = left_value,
                 total = left_value + right_value * 2
             WHERE id = 1;",
        )
        .expect("update succeeds");
    assert_eq!(
        result,
        vec![StatementResult::Command {
            tag: "UPDATE",
            affected_rows: 1,
        }]
    );

    let rows = execute_query(
        &mut database,
        "SELECT id, left_value, right_value, total FROM swaps ORDER BY id;",
    );
    assert_eq!(
        rows.rows,
        vec![
            vec![
                Value::Int64(1),
                Value::Int64(20),
                Value::Int64(10),
                Value::Int64(50),
            ],
            vec![
                Value::Int64(2),
                Value::Int64(30),
                Value::Int64(40),
                Value::Int64(70),
            ],
        ]
    );
}

#[test]
fn failed_updates_leave_every_row_and_column_unchanged() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE ledger (id Int64, amount Int64, divisor Int64, label String);
             INSERT INTO ledger VALUES
                (1, 10, 2, 'first'),
                (2, 9223372036854775807, 0, 'second');",
        )
        .expect("setup succeeds");

    let type_error = database
        .execute("UPDATE ledger SET amount = label WHERE id = 1;")
        .expect_err("wrong assignment type is rejected");
    assert!(matches!(type_error, Error::TypeMismatch { .. }));

    let overflow = database
        .execute("UPDATE ledger SET amount = amount + 1 WHERE id >= 1;")
        .expect_err("later row overflows");
    assert!(matches!(overflow, Error::NumericOverflow(_)));

    let evaluation = database
        .execute("UPDATE ledger SET amount = amount / divisor WHERE id >= 1;")
        .expect_err("later row divides by zero");
    assert!(
        matches!(evaluation, Error::InvalidQuery(message) if message.contains("division by zero"))
    );

    let delete_type_error = database
        .execute("DELETE FROM ledger WHERE amount = label;")
        .expect_err("invalid delete predicate is rejected");
    assert!(matches!(delete_type_error, Error::TypeMismatch { .. }));

    let rows = execute_query(
        &mut database,
        "SELECT id, amount, divisor, label FROM ledger ORDER BY id;",
    );
    assert_eq!(
        rows.rows,
        vec![
            vec![
                Value::Int64(1),
                Value::Int64(10),
                Value::Int64(2),
                Value::String("first".to_owned()),
            ],
            vec![
                Value::Int64(2),
                Value::Int64(i64::MAX),
                Value::Int64(0),
                Value::String("second".to_owned()),
            ],
        ]
    );
    let table = database.catalog().table("ledger").expect("table exists");
    assert!(
        table
            .columns()
            .iter()
            .all(|column| column.len() == table.row_count())
    );
}

#[test]
fn grouped_queries_remain_correct_after_updates_and_ordered_deletes() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE sales_after_mutation (id Int64, region String, amount Int64);
             INSERT INTO sales_after_mutation VALUES
                (1, 'west', 5),
                (2, 'east', 10),
                (3, 'east', 7),
                (4, 'north', 3),
                (5, 'west', 2);
             UPDATE sales_after_mutation
             SET region = 'west', amount = amount * 2
             WHERE id = 2;
             DELETE FROM sales_after_mutation
             WHERE region = 'east' AND amount < 10;",
        )
        .expect("mutations succeed");

    let source = execute_query(&mut database, "SELECT id FROM sales_after_mutation;");
    assert_eq!(
        source.rows,
        vec![
            vec![Value::Int64(1)],
            vec![Value::Int64(2)],
            vec![Value::Int64(4)],
            vec![Value::Int64(5)],
        ]
    );

    let grouped = execute_query(
        &mut database,
        "SELECT region, COUNT(*) AS rows, SUM(amount) AS total
         FROM sales_after_mutation
         GROUP BY region
         ORDER BY total DESC;",
    );
    assert_eq!(
        grouped.rows,
        vec![
            vec![
                Value::String("west".to_owned()),
                Value::Int64(3),
                Value::Int64(27),
            ],
            vec![
                Value::String("north".to_owned()),
                Value::Int64(1),
                Value::Int64(3),
            ],
        ]
    );
}
