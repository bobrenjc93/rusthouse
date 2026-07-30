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
fn alter_modify_column_supports_the_complete_conversion_matrix() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE conversions (
                int_identity Int64, int_float Int64, int_string Int64,
                float_int Float64, float_identity Float64, float_string Float64,
                bool_identity Bool, bool_string Bool,
                string_int String, string_float String, string_bool String,
                string_identity String
             );
             INSERT INTO conversions VALUES
                (-7, 8, 9, -10.0, 11.5, 12.25, true, false,
                 '13', '14.5', 'TRUE', 'unchanged');",
        )
        .expect("setup succeeds");

    let alterations = [
        "ALTER TABLE conversions MODIFY COLUMN int_identity Int64",
        "ALTER TABLE conversions MODIFY COLUMN int_float Float64",
        "ALTER TABLE conversions MODIFY COLUMN int_string String",
        "ALTER TABLE conversions MODIFY COLUMN float_int Int64",
        "ALTER TABLE conversions MODIFY COLUMN float_identity Float64",
        "ALTER TABLE conversions MODIFY COLUMN float_string String",
        "ALTER TABLE conversions MODIFY COLUMN bool_identity Bool",
        "ALTER TABLE conversions MODIFY COLUMN bool_string String",
        "ALTER TABLE conversions MODIFY COLUMN string_int Int64",
        "ALTER TABLE conversions MODIFY COLUMN string_float Float64",
        "ALTER TABLE conversions MODIFY COLUMN string_bool Bool",
        "ALTER TABLE conversions MODIFY COLUMN string_identity String",
    ];
    for statement in alterations {
        assert_eq!(
            database.execute(statement).expect("conversion succeeds"),
            vec![StatementResult::Command {
                tag: "ALTER TABLE",
                affected_rows: 0,
            }]
        );
    }

    let result = execute_query(&mut database, "SELECT * FROM conversions");
    assert_eq!(
        result
            .columns
            .iter()
            .map(|column| (column.name.as_str(), column.data_type))
            .collect::<Vec<_>>(),
        vec![
            ("int_identity", DataType::Int64),
            ("int_float", DataType::Float64),
            ("int_string", DataType::String),
            ("float_int", DataType::Int64),
            ("float_identity", DataType::Float64),
            ("float_string", DataType::String),
            ("bool_identity", DataType::Bool),
            ("bool_string", DataType::String),
            ("string_int", DataType::Int64),
            ("string_float", DataType::Float64),
            ("string_bool", DataType::Bool),
            ("string_identity", DataType::String),
        ]
    );
    assert_eq!(
        result.rows,
        vec![vec![
            Value::Int64(-7),
            Value::Float64(8.0),
            Value::String("9".to_owned()),
            Value::Int64(-10),
            Value::Float64(11.5),
            Value::String("12.25".to_owned()),
            Value::Bool(true),
            Value::String("false".to_owned()),
            Value::Int64(13),
            Value::Float64(14.5),
            Value::Bool(true),
            Value::String("unchanged".to_owned()),
        ]]
    );

    database
        .execute(
            "INSERT INTO conversions VALUES
             (1, 2.5, '3', 4, 5.5, '6.0', false, 'true', 7, 8.5, false, 'new')",
        )
        .expect("inserts use the replacement types");
    let inserted = execute_query(
        &mut database,
        "SELECT int_float, float_int, string_bool, string_identity
         FROM conversions WHERE int_identity = 1",
    );
    assert_eq!(
        inserted.rows,
        vec![vec![
            Value::Float64(2.5),
            Value::Int64(4),
            Value::Bool(false),
            Value::String("new".to_owned()),
        ]]
    );
}

#[test]
fn int64_to_float64_requires_an_exact_representation() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE exact_integers (value Int64);
             INSERT INTO exact_integers VALUES
                (-9223372036854775808),
                (-9007199254740992),
                (9007199254740992),
                (9007199254740994);
             ALTER TABLE exact_integers MODIFY COLUMN value Float64;",
        )
        .expect("exactly representable boundaries convert");
    assert_eq!(
        execute_query(&mut database, "SELECT value FROM exact_integers").rows,
        vec![
            vec![Value::Float64(-9_223_372_036_854_775_808.0)],
            vec![Value::Float64(-9_007_199_254_740_992.0)],
            vec![Value::Float64(9_007_199_254_740_992.0)],
            vec![Value::Float64(9_007_199_254_740_994.0)],
        ]
    );

    for (literal, value) in [
        ("-9007199254740993", -9_007_199_254_740_993_i64),
        ("9007199254740993", 9_007_199_254_740_993_i64),
        ("9223372036854775807", i64::MAX),
    ] {
        let mut database = Database::new();
        database
            .execute(&format!(
                "CREATE TABLE inexact_integers (id Int64, value Int64);
                 INSERT INTO inexact_integers VALUES (1, 0), (2, {literal}), (3, 2)"
            ))
            .expect("setup succeeds");

        let error = database
            .execute("ALTER TABLE inexact_integers MODIFY COLUMN value Float64")
            .expect_err("inexact conversion fails");
        assert!(matches!(
            error,
            Error::ColumnConversion {
                from: DataType::Int64,
                to: DataType::Float64,
                row: Some(2),
                reason,
                ..
            } if reason.contains("cannot be represented exactly")
        ));

        database
            .execute("INSERT INTO inexact_integers VALUES (4, 9007199254740993)")
            .expect("rollback preserves the Int64 schema");
        let result = execute_query(
            &mut database,
            "SELECT id, value FROM inexact_integers ORDER BY id",
        );
        assert_eq!(result.columns[1].data_type, DataType::Int64);
        assert_eq!(result.rows[1][1], Value::Int64(value));
        assert_eq!(result.rows[3][1], Value::Int64(9_007_199_254_740_993));
    }
}

#[test]
fn alter_modify_column_handles_empty_and_large_tables() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE empty_values (id Int64, value String);
             ALTER TABLE empty_values MODIFY COLUMN value Float64;
             INSERT INTO empty_values VALUES (1, 2.5);",
        )
        .expect("empty physical column converts");
    assert_eq!(
        execute_query(&mut database, "SELECT * FROM empty_values").rows,
        vec![vec![Value::Int64(1), Value::Float64(2.5)]]
    );

    const ROW_COUNT: usize = 25_000;
    let values = (0..ROW_COUNT)
        .map(|value| format!("('{value}')"))
        .collect::<Vec<_>>()
        .join(",");
    database
        .execute("CREATE TABLE large_values (value String)")
        .expect("large table created");
    database
        .execute(&format!("INSERT INTO large_values VALUES {values}"))
        .expect("large table populated");
    database
        .execute("ALTER TABLE large_values MODIFY COLUMN value Int64")
        .expect("large physical column converts");

    let result = execute_query(
        &mut database,
        "SELECT COUNT(*) AS rows, SUM(value) AS total FROM large_values",
    );
    assert_eq!(
        result.rows,
        vec![vec![
            Value::Int64(ROW_COUNT as i64),
            Value::Int64(312_487_500),
        ]]
    );
}

#[test]
fn alter_modify_column_failures_report_rows_and_roll_back() {
    let cases = [
        ("Int64", "not-an-int", "invalid Int64 value"),
        ("Int64", "9223372036854775808", "Int64 overflow"),
        ("Float64", "1e999", "non-finite Float64"),
    ];

    for (target, bad_value, expected_reason) in cases {
        let mut database = Database::new();
        database
            .execute(&format!(
                "CREATE TABLE raw_values (id Int64, value String);
                 INSERT INTO raw_values VALUES (1, '10'), (2, '{bad_value}'), (3, '30')"
            ))
            .expect("setup succeeds");

        let error = database
            .execute(&format!(
                "ALTER TABLE raw_values MODIFY COLUMN value {target}"
            ))
            .expect_err("conversion fails");
        assert!(matches!(
            error,
            Error::ColumnConversion {
                table,
                column,
                row: Some(2),
                reason,
                ..
            } if table == "raw_values"
                && column == "value"
                && reason.contains(expected_reason)
        ));

        database
            .execute("INSERT INTO raw_values VALUES (4, 'still a string')")
            .expect("the original schema remains active");
        let result = execute_query(
            &mut database,
            "SELECT id, value FROM raw_values ORDER BY id",
        );
        assert_eq!(result.columns[1].data_type, DataType::String);
        assert_eq!(result.rows[1][1], Value::String(bad_value.to_owned()));
        assert_eq!(
            result.rows[3],
            vec![Value::Int64(4), Value::String("still a string".to_owned())]
        );
    }

    for (bad_value, expected_reason) in [("3.5", "not an integer"), ("1e300", "overflow")] {
        let mut database = Database::new();
        database
            .execute(&format!(
                "CREATE TABLE floats (id Int64, value Float64);
                 INSERT INTO floats VALUES (1, 2.0), (2, {bad_value}), (3, 4.0)"
            ))
            .expect("setup succeeds");
        let error = database
            .execute("ALTER TABLE floats MODIFY COLUMN value Int64")
            .expect_err("checked narrowing fails");
        assert!(matches!(
            error,
            Error::ColumnConversion {
                row: Some(2),
                reason,
                ..
            } if reason.contains(expected_reason)
        ));
        let result = execute_query(&mut database, "SELECT value FROM floats");
        assert_eq!(result.columns[0].data_type, DataType::Float64);
        assert_eq!(result.rows[0], vec![Value::Float64(2.0)]);
        assert_eq!(result.rows[2], vec![Value::Float64(4.0)]);
    }
}

#[test]
fn alter_modify_column_rejects_unsupported_conversions_atomically() {
    for (source, value, target) in [
        (DataType::Int64, "1", DataType::Bool),
        (DataType::Float64, "1.0", DataType::Bool),
        (DataType::Bool, "true", DataType::Int64),
        (DataType::Bool, "false", DataType::Float64),
    ] {
        let mut database = Database::new();
        database
            .execute(&format!(
                "CREATE TABLE unsupported (value {source});
                 INSERT INTO unsupported VALUES ({value})"
            ))
            .expect("setup succeeds");
        let error = database
            .execute(&format!(
                "ALTER TABLE unsupported MODIFY COLUMN value {target}"
            ))
            .expect_err("conversion is unsupported");
        assert!(matches!(
            error,
            Error::ColumnConversion {
                row: None,
                reason,
                ..
            } if reason.contains("not supported")
        ));
        assert_eq!(
            database
                .catalog()
                .table("unsupported")
                .expect("table remains")
                .schema()[0]
                .data_type,
            source
        );
    }
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
