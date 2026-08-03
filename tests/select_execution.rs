use rusthouse::{
    Int64Table, OrderError, OrderLimits, ParseLimits, ScanError, ScanLimits, Schema,
    SelectExecutionError, execute_select, execute_select_with_limits,
    execute_select_with_order_limits, parse_select,
};

fn table(nullable: bool, values: &[Option<i64>]) -> Int64Table {
    let mut table = Int64Table::new(Schema::int64("value", nullable), values.len());
    table.append_batch(values).unwrap();
    table
}

#[test]
fn executes_a_parsed_select_against_an_empty_table() {
    let statement = parse_select("SELECT value FROM readings", ParseLimits::default()).unwrap();
    let table = table(false, &[]);

    let values = execute_select("readings", &table, &statement).unwrap();

    assert!(values.is_empty());
    assert!(std::ptr::eq(values.as_ref(), table.values()));
}

#[test]
fn borrows_populated_values_in_row_order() {
    let statement = parse_select("SELECT value FROM readings", ParseLimits::default()).unwrap();
    let table = table(false, &[Some(i64::MIN), Some(0), Some(i64::MAX)]);

    let values = execute_select("readings", &table, &statement).unwrap();

    assert_eq!(values.as_ref(), &[Some(i64::MIN), Some(0), Some(i64::MAX)]);
    assert!(std::ptr::eq(values.as_ref(), table.values()));
}

#[test]
fn returns_null_values_without_copying() {
    let statement = parse_select("SELECT value FROM readings", ParseLimits::default()).unwrap();
    let table = table(true, &[Some(1), None, Some(3)]);

    let values = execute_select("readings", &table, &statement).unwrap();

    assert_eq!(values.as_ref(), &[Some(1), None, Some(3)]);
    assert!(std::ptr::eq(values.as_ref(), table.values()));
}

#[test]
fn applies_limit_as_a_borrowed_prefix() {
    let table = table(true, &[Some(1), None, Some(3)]);

    for (limit, expected) in [
        (0, &[][..]),
        (2, &[Some(1), None][..]),
        (3, &[Some(1), None, Some(3)][..]),
        (100, &[Some(1), None, Some(3)][..]),
    ] {
        let statement = parse_select(
            &format!("SELECT value FROM readings LIMIT {limit}"),
            ParseLimits::default(),
        )
        .unwrap();

        let values = execute_select("readings", &table, &statement).unwrap();

        assert_eq!(values.as_ref(), expected, "LIMIT {limit}");
        assert!(std::ptr::eq(values.as_ptr(), table.values().as_ptr()));
    }
}

#[test]
fn orders_nulls_ties_and_integer_extremes_for_every_explicit_mode() {
    let table = table(
        true,
        &[
            Some(2),
            None,
            Some(i64::MIN),
            Some(2),
            Some(i64::MAX),
            None,
            Some(i64::MIN),
        ],
    );
    let cases = [
        (
            "ASC NULLS FIRST",
            vec![
                None,
                None,
                Some(i64::MIN),
                Some(i64::MIN),
                Some(2),
                Some(2),
                Some(i64::MAX),
            ],
        ),
        (
            "ASC NULLS LAST",
            vec![
                Some(i64::MIN),
                Some(i64::MIN),
                Some(2),
                Some(2),
                Some(i64::MAX),
                None,
                None,
            ],
        ),
        (
            "DESC NULLS FIRST",
            vec![
                None,
                None,
                Some(i64::MAX),
                Some(2),
                Some(2),
                Some(i64::MIN),
                Some(i64::MIN),
            ],
        ),
        (
            "DESC NULLS LAST",
            vec![
                Some(i64::MAX),
                Some(2),
                Some(2),
                Some(i64::MIN),
                Some(i64::MIN),
                None,
                None,
            ],
        ),
    ];

    for (mode, expected) in cases {
        let statement = parse_select(
            &format!("SELECT value FROM readings ORDER BY value {mode} LIMIT 7"),
            ParseLimits::default(),
        )
        .unwrap();
        let values = execute_select("readings", &table, &statement).unwrap();

        assert_eq!(values.as_ref(), expected, "{mode}");
        assert!(matches!(values, std::borrow::Cow::Owned(_)), "{mode}");
    }
}

#[test]
fn ordered_limit_uses_top_k_and_accepts_zero_or_more_than_the_input() {
    let table = table(true, &[Some(4), None, Some(-1), Some(9), Some(4)]);

    for (limit, expected) in [
        (0, &[][..]),
        (3, &[Some(9), Some(4), Some(4)][..]),
        (8, &[Some(9), Some(4), Some(4), Some(-1), None][..]),
    ] {
        let statement = parse_select(
            &format!("SELECT value FROM readings ORDER BY value DESC NULLS LAST LIMIT {limit}"),
            ParseLimits::default(),
        )
        .unwrap();

        assert_eq!(
            execute_select("readings", &table, &statement)
                .unwrap()
                .as_ref(),
            expected,
            "LIMIT {limit}"
        );
    }
}

#[test]
fn ordered_execution_enforces_exact_and_exceeded_operator_bounds() {
    let table = table(true, &[Some(3), None, Some(1)]);
    let statement = parse_select(
        "SELECT value FROM readings ORDER BY value ASC NULLS LAST LIMIT 2",
        ParseLimits::default(),
    )
    .unwrap();

    assert_eq!(
        execute_select_with_order_limits(
            "readings",
            &table,
            &statement,
            ScanLimits::new(3, 3),
            OrderLimits::new(3, 2),
        )
        .unwrap()
        .as_ref(),
        &[Some(1), Some(3)]
    );
    assert_eq!(
        execute_select_with_order_limits(
            "readings",
            &table,
            &statement,
            ScanLimits::new(3, 3),
            OrderLimits::new(2, 2),
        ),
        Err(SelectExecutionError::Order(
            OrderError::InputLimitExceeded {
                rows: 3,
                max_rows: 2,
            }
        ))
    );
    assert_eq!(
        execute_select_with_order_limits(
            "readings",
            &table,
            &statement,
            ScanLimits::new(3, 3),
            OrderLimits::new(3, 1),
        ),
        Err(SelectExecutionError::Order(OrderError::LimitExceeded {
            limit: 2,
            max_limit: 1,
        }))
    );
}

#[test]
fn orders_filtered_rows_and_applies_limit_after_filtering() {
    let statement = parse_select(
        "SELECT value FROM readings WHERE value >= 7 ORDER BY value DESC NULLS LAST LIMIT 2",
        ParseLimits::default(),
    )
    .unwrap();
    let table = table(true, &[Some(7), None, Some(2), Some(9), Some(7)]);

    let values = execute_select("readings", &table, &statement).unwrap();

    assert_eq!(values.as_ref(), &[Some(9), Some(7)]);
    assert!(matches!(values, std::borrow::Cow::Owned(_)));
}

#[test]
fn order_column_must_match_even_for_zero_limit() {
    let statement = parse_select(
        "SELECT value FROM readings ORDER BY other ASC NULLS LAST LIMIT 0",
        ParseLimits::default(),
    )
    .unwrap();
    let table = table(true, &[Some(1), None]);

    assert_eq!(
        execute_select("readings", &table, &statement),
        Err(SelectExecutionError::UnknownColumn {
            name: "other".to_owned(),
        })
    );
}

#[test]
fn executes_every_where_comparison_operator_and_excludes_nulls() {
    let table = table(
        true,
        &[
            None,
            Some(i64::MIN),
            Some(-2),
            Some(0),
            Some(2),
            Some(i64::MAX),
            None,
        ],
    );
    let cases = [
        ("=", vec![Some(0)]),
        (
            "!=",
            vec![Some(i64::MIN), Some(-2), Some(2), Some(i64::MAX)],
        ),
        (
            "<>",
            vec![Some(i64::MIN), Some(-2), Some(2), Some(i64::MAX)],
        ),
        ("<", vec![Some(i64::MIN), Some(-2)]),
        ("<=", vec![Some(i64::MIN), Some(-2), Some(0)]),
        (">", vec![Some(2), Some(i64::MAX)]),
        (">=", vec![Some(0), Some(2), Some(i64::MAX)]),
    ];

    for (operator, expected) in cases {
        let statement = parse_select(
            &format!("SELECT value FROM readings WHERE value {operator} 0"),
            ParseLimits::default(),
        )
        .unwrap();
        let values = execute_select("readings", &table, &statement).unwrap();

        assert_eq!(values.as_ref(), expected, "operator {operator}");
        assert!(matches!(values, std::borrow::Cow::Owned(_)));
    }
}

#[test]
fn where_comparisons_handle_int64_extremes_without_overflow() {
    let table = table(
        true,
        &[
            Some(i64::MIN),
            Some(-1),
            Some(i64::MAX),
            Some(i64::MIN),
            None,
            Some(i64::MAX),
        ],
    );
    let cases = [
        ("=", i64::MIN, vec![Some(i64::MIN), Some(i64::MIN)]),
        (
            "!=",
            i64::MIN,
            vec![Some(-1), Some(i64::MAX), Some(i64::MAX)],
        ),
        (
            "<>",
            i64::MAX,
            vec![Some(i64::MIN), Some(-1), Some(i64::MIN)],
        ),
        ("<", i64::MIN, vec![]),
        ("<=", i64::MIN, vec![Some(i64::MIN), Some(i64::MIN)]),
        (">", i64::MAX, vec![]),
        (">=", i64::MAX, vec![Some(i64::MAX), Some(i64::MAX)]),
    ];

    for (operator, comparison_value, expected) in cases {
        let statement = parse_select(
            &format!("SELECT value FROM readings WHERE value {operator} {comparison_value}"),
            ParseLimits::default(),
        )
        .unwrap();

        assert_eq!(
            execute_select("readings", &table, &statement)
                .unwrap()
                .as_ref(),
            expected,
            "operator {operator}"
        );
    }
}

#[test]
fn where_column_must_match_for_every_operator() {
    let table = table(true, &[Some(1), None]);

    for operator in ["=", "!=", "<>", "<", "<=", ">", ">="] {
        let statement = parse_select(
            &format!("SELECT value FROM readings WHERE other {operator} 1"),
            ParseLimits::default(),
        )
        .unwrap();

        assert_eq!(
            execute_select("readings", &table, &statement),
            Err(SelectExecutionError::UnknownColumn {
                name: "other".to_owned(),
            }),
            "operator {operator}"
        );
    }
}

#[test]
fn every_where_operator_preserves_scan_input_and_result_limits() {
    let table = table(true, &[Some(1), None, Some(1), Some(2), Some(3)]);
    let cases = [
        ("=", 1, vec![Some(1), Some(1)]),
        ("!=", 1, vec![Some(2), Some(3)]),
        ("<>", 1, vec![Some(2), Some(3)]),
        ("<", 2, vec![Some(1), Some(1)]),
        ("<=", 1, vec![Some(1), Some(1)]),
        (">", 1, vec![Some(2), Some(3)]),
        (">=", 2, vec![Some(2), Some(3)]),
    ];

    for (operator, comparison_value, expected) in cases {
        let statement = parse_select(
            &format!("SELECT value FROM readings WHERE value {operator} {comparison_value}"),
            ParseLimits::default(),
        )
        .unwrap();

        assert_eq!(
            execute_select_with_limits(
                "readings",
                &table,
                &statement,
                ScanLimits::new(4, expected.len()),
            ),
            Err(SelectExecutionError::Scan(ScanError::InputLimitExceeded {
                rows: 5,
                max_rows: 4,
            })),
            "input bound for {operator}"
        );
        assert_eq!(
            execute_select_with_limits(
                "readings",
                &table,
                &statement,
                ScanLimits::new(5, expected.len() - 1),
            ),
            Err(SelectExecutionError::Scan(ScanError::ResultLimitExceeded {
                rows: expected.len(),
                max_rows: expected.len() - 1,
            })),
            "result bound for {operator}"
        );
        assert_eq!(
            execute_select_with_limits(
                "readings",
                &table,
                &statement,
                ScanLimits::new(5, expected.len()),
            )
            .unwrap()
            .as_ref(),
            expected,
            "exact bounds for {operator}"
        );
    }
}

#[test]
fn where_comparison_preserves_source_order_and_applies_limit_after_filtering() {
    let statement = parse_select(
        "SELECT value FROM readings WHERE value != 7 LIMIT 3",
        ParseLimits::default(),
    )
    .unwrap();
    let table = table(
        true,
        &[Some(7), None, Some(3), Some(9), Some(3), None, Some(10)],
    );

    assert_eq!(
        execute_select("readings", &table, &statement)
            .unwrap()
            .as_ref(),
        &[Some(3), Some(9), Some(3)]
    );
}

#[test]
fn table_name_matching_is_exact_and_a_mismatch_does_not_mutate() {
    let statement = parse_select("SELECT value FROM Readings", ParseLimits::default()).unwrap();
    let table = table(true, &[Some(1), None]);
    let original = table.clone();

    let error = execute_select("readings", &table, &statement).unwrap_err();

    assert_eq!(
        error,
        SelectExecutionError::UnknownTable {
            name: "Readings".to_owned(),
        }
    );
    assert_eq!(table, original);
}

#[test]
fn column_name_matching_is_exact_and_a_mismatch_does_not_mutate() {
    let statement = parse_select("SELECT Value FROM readings", ParseLimits::default()).unwrap();
    let table = table(true, &[Some(1), None]);
    let original = table.clone();

    let error = execute_select("readings", &table, &statement).unwrap_err();

    assert_eq!(
        error,
        SelectExecutionError::UnknownColumn {
            name: "Value".to_owned(),
        }
    );
    assert_eq!(table, original);
}
