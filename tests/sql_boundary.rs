use rusthouse::{Database, DatabaseError, ExecutionResult, LimitKind, Limits, Value};

fn query(database: &mut Database, sql: &str) -> rusthouse::QueryResult {
    match database.execute_one(sql).unwrap() {
        ExecutionResult::Query(result) => result,
        result => panic!("expected query result, got {result:?}"),
    }
}

#[test]
fn varied_identifiers_types_and_scalar_projections_execute_generally() {
    let mut database = Database::new();
    let results = database
        .execute(
            "CREATE TABLE `Campaign Facts`
                (`Segment Name` String, impressions Int64, rate Float64, enabled Bool);
             INSERT INTO `Campaign Facts` (`enabled`, `rate`, `Segment Name`, `impressions`) VALUES
                (true, 0.25, 'north', 12),
                (false, 1.5, 'contains,comma', 7),
                (true, 2.0, 'north', 8);
             SELECT `Segment Name` AS segment, impressions * rate AS weighted, enabled
             FROM `Campaign Facts`
             WHERE (enabled = true AND impressions >= 8) OR rate > 3.0
             ORDER BY weighted DESC LIMIT 2;",
        )
        .unwrap();
    assert_eq!(results.len(), 3);
    let ExecutionResult::Query(result) = &results[2] else {
        panic!("third statement was not a query");
    };
    assert_eq!(
        result.rows,
        vec![
            vec![
                Value::String("north".into()),
                Value::Float64(16.0),
                Value::Bool(true),
            ],
            vec![
                Value::String("north".into()),
                Value::Float64(3.0),
                Value::Bool(true),
            ],
        ]
    );
}

#[test]
fn all_aggregates_work_with_multiple_schema_shapes() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE readings (site String, sample Int64, measurement Float64);
             INSERT INTO readings VALUES
               ('west', 1, 2.5), ('east', 4, 9.0),
               ('west', 3, 7.5), ('east', 2, 1.0);",
        )
        .unwrap();
    let result = query(
        &mut database,
        "SELECT site AS location, count(*) n, sum(sample) samples, min(measurement) low,
                max(measurement) high, avg(measurement) mean
         FROM readings GROUP BY location ORDER BY location",
    );
    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.rows[0][0], Value::String("east".into()));
    assert_eq!(result.rows[0][2], Value::Int64(6));
    assert_eq!(result.rows[1][5], Value::Float64(5.0));
}

#[test]
fn failed_late_row_does_not_partially_append() {
    let mut database = Database::new();
    database
        .execute_one("CREATE TABLE atomicity (a Int64, b Bool)")
        .unwrap();
    database
        .execute_one("INSERT INTO atomicity VALUES (1, true)")
        .unwrap();
    assert!(
        database
            .execute_one("INSERT INTO atomicity VALUES (2, false), (3, 'wrong')")
            .is_err()
    );
    assert_eq!(database.table_row_count("atomicity").unwrap(), 1);
}

#[test]
fn input_column_row_result_and_string_limits_are_enforced() {
    let base = Limits {
        max_input_bytes: 1_000,
        max_rows_per_insert: 2,
        max_rows_per_table: 2,
        max_result_rows: 1,
        max_columns_per_table: 2,
        max_string_bytes: 4,
        ..Limits::default()
    };
    let mut database = Database::with_limits(base.clone());
    database
        .execute("CREATE TABLE bounded (id Int64, tag String); INSERT INTO bounded VALUES (1, 'aa'), (2, 'bb')")
        .unwrap();
    let result_error = database.execute_one("SELECT * FROM bounded").unwrap_err();
    assert!(matches!(
        result_error,
        DatabaseError::LimitExceeded {
            kind: LimitKind::ResultRows,
            ..
        }
    ));
    assert_eq!(
        query(&mut database, "SELECT * FROM bounded LIMIT 1")
            .rows
            .len(),
        1
    );

    let mut columns = Database::with_limits(base.clone());
    assert!(matches!(
        columns.execute_one("CREATE TABLE too_wide (a Int64, b Int64, c Int64)"),
        Err(DatabaseError::LimitExceeded {
            kind: LimitKind::ColumnsPerTable,
            ..
        })
    ));

    let mut input = Database::with_limits(Limits {
        max_input_bytes: 5,
        ..base
    });
    assert!(matches!(
        input.execute_one("SELECT 1"),
        Err(DatabaseError::LimitExceeded {
            kind: LimitKind::InputBytes,
            ..
        })
    ));
}

#[test]
fn deeply_nested_and_flat_expressions_return_typed_errors_without_crashing() {
    let mut database = Database::new();
    let nested = format!("SELECT {}1{}", "(".repeat(50_000), ")".repeat(50_000));
    assert!(matches!(
        database.execute_one(&nested),
        Err(DatabaseError::LimitExceeded {
            kind: LimitKind::ExpressionDepth,
            ..
        })
    ));

    let flat = format!("SELECT {}", vec!["1"; 1_000].join(" + "));
    assert!(matches!(
        database.execute_one(&flat),
        Err(DatabaseError::LimitExceeded {
            kind: LimitKind::ExpressionNodes,
            ..
        })
    ));
}

#[test]
fn execute_one_rejects_multiple_statements_before_mutating_catalog() {
    let mut database = Database::new();
    let error = database
        .execute_one("CREATE TABLE first (id Int64); CREATE TABLE second (id Int64)")
        .unwrap_err();
    assert!(matches!(error, DatabaseError::InvalidQuery(_)));
    assert!(matches!(
        database.schema("first"),
        Err(DatabaseError::TableNotFound(_))
    ));
    assert!(matches!(
        database.schema("second"),
        Err(DatabaseError::TableNotFound(_))
    ));
}

#[test]
fn quoted_dotted_columns_remain_distinct_from_qualification() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE dotted (\"a.b\" Int64, plain String);
             INSERT INTO dotted VALUES (7, 'ok')",
        )
        .unwrap();

    let wildcard = query(&mut database, "SELECT * FROM dotted");
    assert_eq!(
        wildcard.rows,
        vec![vec![Value::Int64(7), Value::String("ok".into())]]
    );
    let direct = query(
        &mut database,
        "SELECT \"a.b\" AS dotted_name, dotted.plain FROM dotted",
    );
    assert_eq!(direct.rows, wildcard.rows);
}

#[test]
fn not_binds_to_comparisons_before_and_or() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE truth (id Int64, enabled Bool);
             INSERT INTO truth VALUES (1, true), (2, true), (3, false)",
        )
        .unwrap();
    let result = query(
        &mut database,
        "SELECT id FROM truth WHERE NOT id = 1 AND enabled = true ORDER BY id",
    );
    assert_eq!(result.rows, vec![vec![Value::Int64(2)]]);
}

#[test]
fn string_limit_applies_to_query_literals_before_execution() {
    let mut database = Database::with_limits(Limits {
        max_string_bytes: 1,
        ..Limits::default()
    });
    assert!(matches!(
        database.execute_one("SELECT 'oversized'"),
        Err(DatabaseError::LimitExceeded {
            kind: LimitKind::StringBytes,
            ..
        })
    ));
}

#[test]
fn limit_streams_projections_and_order_by_uses_bounded_top_k() {
    let values = (0..100)
        .rev()
        .map(|id| format!("({id})"))
        .collect::<Vec<_>>()
        .join(",");
    let mut database = Database::with_limits(Limits {
        max_intermediate_rows: 2,
        max_intermediate_bytes: 4 * 1024,
        max_result_bytes: 512,
        ..Limits::default()
    });
    database
        .execute(&format!(
            "CREATE TABLE bounded_query (id Int64); INSERT INTO bounded_query VALUES {values}"
        ))
        .unwrap();

    let projected = query(
        &mut database,
        "SELECT 'a moderately sized result string' AS text FROM bounded_query LIMIT 1",
    );
    assert_eq!(projected.rows.len(), 1);
    assert!(matches!(
        database
            .execute_one("SELECT 'a moderately sized result string' AS text FROM bounded_query"),
        Err(DatabaseError::LimitExceeded {
            kind: LimitKind::ResultBytes,
            ..
        })
    ));
    let first_only = query(
        &mut database,
        "SELECT 10 / (id - 98) AS quotient FROM bounded_query LIMIT 1",
    );
    assert_eq!(first_only.rows, vec![vec![Value::Float64(10.0)]]);

    let sorted = query(
        &mut database,
        "SELECT id FROM bounded_query ORDER BY id ASC LIMIT 2",
    );
    assert_eq!(
        sorted.rows,
        vec![vec![Value::Int64(0)], vec![Value::Int64(1)]]
    );
    assert!(matches!(
        database.execute_one("SELECT id FROM bounded_query ORDER BY id LIMIT 3"),
        Err(DatabaseError::LimitExceeded {
            kind: LimitKind::IntermediateRows,
            ..
        })
    ));

    let mut byte_limited = Database::with_limits(Limits {
        max_intermediate_bytes: 1,
        ..Limits::default()
    });
    byte_limited
        .execute(
            "CREATE TABLE byte_limit (id Int64);
             INSERT INTO byte_limit VALUES (2), (1)",
        )
        .unwrap();
    assert!(matches!(
        byte_limited.execute_one("SELECT id FROM byte_limit ORDER BY id LIMIT 1"),
        Err(DatabaseError::LimitExceeded {
            kind: LimitKind::IntermediateBytes,
            ..
        })
    ));
}

#[test]
fn empty_non_count_aggregates_return_typed_errors() {
    let mut database = Database::new();
    database
        .execute_one("CREATE TABLE empty_metrics (i Int64, f Float64, b Bool, s String)")
        .unwrap();

    let count = query(&mut database, "SELECT COUNT(*) AS n FROM empty_metrics");
    assert_eq!(count.rows, vec![vec![Value::Int64(0)]]);
    for function in ["SUM(i)", "MIN(i)", "MAX(f)", "AVG(i)"] {
        assert!(matches!(
            database.execute_one(&format!("SELECT {function} FROM empty_metrics")),
            Err(DatabaseError::EmptyAggregate(_))
        ));
    }

    database
        .execute_one("INSERT INTO empty_metrics VALUES (4, 2.5, true, 'present')")
        .unwrap();
    assert!(matches!(
        database.execute_one("SELECT MIN(i) FROM empty_metrics WHERE b = false"),
        Err(DatabaseError::EmptyAggregate(function)) if function == "min"
    ));
    assert!(
        query(
            &mut database,
            "SELECT i, COUNT(*) FROM empty_metrics WHERE b = false GROUP BY i"
        )
        .rows
        .is_empty()
    );
}

#[test]
fn group_by_matches_recursively_resolved_column_identity() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE expression_groups (a Int64);
             INSERT INTO expression_groups VALUES (1), (1), (2)",
        )
        .unwrap();

    let expression = query(
        &mut database,
        "SELECT A + 1 AS shifted, COUNT(*) AS n
         FROM expression_groups GROUP BY a + 1 ORDER BY shifted",
    );
    assert_eq!(
        expression.rows,
        vec![
            vec![Value::Int64(2), Value::Int64(2)],
            vec![Value::Int64(3), Value::Int64(1)],
        ]
    );

    let qualified = query(
        &mut database,
        "SELECT expression_groups.A AS value, COUNT(*) AS n
         FROM expression_groups GROUP BY a ORDER BY value",
    );
    assert_eq!(qualified.rows[0], vec![Value::Int64(1), Value::Int64(2)]);
}

#[test]
fn unicode_identifiers_are_case_insensitive_across_catalog_and_aliases() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE Ångström (Énergie Int64, Straße String);
             INSERT INTO ångström (énergie, STRASSE) VALUES (2, 'b'), (1, 'a')",
        )
        .unwrap();
    let result = query(
        &mut database,
        "SELECT ÉNERGIE AS Résultat, strasse FROM ÅNGSTRÖM ORDER BY résultat",
    );
    assert_eq!(
        result.rows,
        vec![
            vec![Value::Int64(1), Value::String("a".into())],
            vec![Value::Int64(2), Value::String("b".into())],
        ]
    );
}

#[test]
fn mixed_numeric_comparisons_preserve_int64_precision() {
    let mut database = Database::new();
    let result = query(
        &mut database,
        "SELECT
           9007199254740993 = 9007199254740992.0 AS rounded_equal,
           9007199254740993 > 9007199254740992.0 AS exact_greater,
           9223372036854775807 < 9223372036854775808.0 AS below_upper_bound,
           -9223372036854775808 = -9223372036854775808.0 AS minimum_equal,
           -1 > -1.5 AS negative_fraction",
    );
    assert_eq!(
        result.rows,
        vec![vec![
            Value::Bool(false),
            Value::Bool(true),
            Value::Bool(true),
            Value::Bool(true),
            Value::Bool(true),
        ]]
    );
}

#[test]
fn wide_projection_byte_limits_are_enforced_before_late_expressions() {
    let payload = "x".repeat(32 * 1024);
    let mut database = Database::with_limits(Limits {
        max_result_bytes: 128 * 1024,
        max_string_bytes: 64 * 1024,
        ..Limits::default()
    });
    database
        .execute(&format!(
            "CREATE TABLE wide_values (id Int64, payload String);
             INSERT INTO wide_values VALUES (1, '{payload}')"
        ))
        .unwrap();
    let mut projections = vec!["payload"; 100];
    projections.push("1 / (id - id) AS late_failure");
    let projection_sql = projections.join(", ");

    for suffix in ["", " ORDER BY id"] {
        assert!(matches!(
            database.execute_one(&format!("SELECT {projection_sql} FROM wide_values{suffix}")),
            Err(DatabaseError::LimitExceeded {
                kind: LimitKind::ResultBytes,
                ..
            })
        ));
    }
}

#[test]
fn count_expression_propagates_runtime_errors() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE count_values (id Int64);
             INSERT INTO count_values VALUES (1)",
        )
        .unwrap();
    assert!(matches!(
        database.execute_one("SELECT COUNT(1 / (id - 1)) FROM count_values"),
        Err(DatabaseError::InvalidValue(message)) if message == "division by zero"
    ));
    assert!(matches!(
        database.execute_one("SELECT COUNT(id + 9223372036854775807) FROM count_values"),
        Err(DatabaseError::ArithmeticOverflow(_))
    ));
    assert_eq!(
        query(&mut database, "SELECT COUNT(*) FROM count_values").rows,
        vec![vec![Value::Int64(1)]]
    );
}

#[test]
fn quoted_identifiers_preserve_exact_case() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE \"CaseTable\" (\"CaseName\" Int64, \"casename\" String);
             CREATE TABLE \"casetable\" (id Int64);
             INSERT INTO \"CaseTable\" (\"CaseName\", \"casename\") VALUES (7, 'lower')",
        )
        .unwrap();
    let result = query(
        &mut database,
        "SELECT \"CaseTable\".\"CaseName\", \"CaseTable\".\"casename\"
         FROM \"CaseTable\"",
    );
    assert_eq!(
        result.rows,
        vec![vec![Value::Int64(7), Value::String("lower".into())]]
    );
    assert!(matches!(
        database.execute_one("SELECT \"CASENAME\" FROM \"CaseTable\""),
        Err(DatabaseError::ColumnNotFound(_))
    ));

    assert!(matches!(
        database.schema("CaseTable"),
        Err(DatabaseError::AmbiguousTable(name)) if name == "CaseTable"
    ));
    let upper = database.schema_quoted("CaseTable").unwrap();
    assert!(matches!(
        upper.resolve_column_index("CaseName"),
        Err(DatabaseError::AmbiguousColumn(name)) if name == "CaseName"
    ));
    assert_eq!(upper.column_index_quoted("CaseName"), Some(0));
    assert_eq!(upper.column_index_quoted("casename"), Some(1));
    assert_eq!(upper.column_index_unquoted("CaseName"), Some(1));
    assert_eq!(
        database.schema_quoted("casetable").unwrap().columns()[0].name,
        "id"
    );
    assert_eq!(
        database.schema_unquoted("CaseTable").unwrap().columns()[0].name,
        "id"
    );
    assert_eq!(database.table_row_count_quoted("CaseTable").unwrap(), 1);
    assert_eq!(database.table_row_count_quoted("casetable").unwrap(), 0);
}

fn balanced_sum(terms: usize) -> String {
    if terms == 1 {
        return "1".into();
    }
    let left = terms / 2;
    format!("({} + {})", balanced_sum(left), balanced_sum(terms - left))
}

#[test]
fn balanced_expressions_use_depth_and_a_separate_node_limit() {
    let mut database = Database::new();
    let result = query(&mut database, &format!("SELECT {}", balanced_sum(66)));
    assert_eq!(result.rows, vec![vec![Value::Int64(66)]]);

    assert!(matches!(
        database.execute_one(&format!("SELECT {}", balanced_sum(1_100))),
        Err(DatabaseError::LimitExceeded {
            kind: LimitKind::ExpressionNodes,
            ..
        })
    ));
}

#[test]
fn order_by_rejects_ambiguous_output_names() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE alias_collision (a Int64, b Int64);
             INSERT INTO alias_collision VALUES (2, 1), (1, 2)",
        )
        .unwrap();
    assert!(matches!(
        database.execute_one("SELECT a, b AS a FROM alias_collision ORDER BY a"),
        Err(DatabaseError::AmbiguousColumn(name)) if name == "a"
    ));
}

#[test]
fn caller_cannot_raise_expression_nodes_above_the_safe_cap() {
    let mut database = Database::with_limits(Limits {
        max_expression_nodes: usize::MAX,
        ..Limits::default()
    });
    assert_eq!(database.limits().max_expression_nodes, 256);

    let flat = format!("SELECT {}", vec!["1"; 50_000].join(" + "));
    assert!(matches!(
        database.execute_one(&flat),
        Err(DatabaseError::LimitExceeded {
            kind: LimitKind::ExpressionNodes,
            ..
        })
    ));
    let unary = format!("SELECT {}1", "+".repeat(50_000));
    assert!(matches!(
        database.execute_one(&unary),
        Err(DatabaseError::LimitExceeded {
            kind: LimitKind::ExpressionNodes,
            ..
        })
    ));
}

#[test]
fn scalar_select_does_not_consume_the_table_column_limit() {
    let mut database = Database::with_limits(Limits {
        max_columns_per_table: 0,
        ..Limits::default()
    });
    assert_eq!(
        query(&mut database, "SELECT 1 AS value").rows,
        vec![vec![Value::Int64(1)]]
    );
    assert!(matches!(
        database.execute_one("CREATE TABLE forbidden (id Int64)"),
        Err(DatabaseError::LimitExceeded {
            kind: LimitKind::ColumnsPerTable,
            ..
        })
    ));
}
