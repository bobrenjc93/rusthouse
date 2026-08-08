use rusthouse::batch::engine::{
    Database, QueryResult, QueryResultLimits, ResultColumn, StatementResult,
};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{BatchSqlLimits, SelectItem, Statement, parse, parse_with_limits};
use rusthouse::batch::value::{DataType, Value};
use rusthouse::batch::{
    run_csv_batch, run_json_batch, run_json_compact_each_row_batch, run_json_each_row_batch,
    run_table_batch, run_tsv_batch,
};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    match results.into_iter().last().expect("one result") {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

#[test]
fn parses_cast_as_a_bounded_select_item_with_an_optional_alias() {
    let statements = parse(
        "SELECT CAST(reading AS Float64), cast(reading as float64) AS converted \
         FROM samples WHERE reading < 0 LIMIT 2",
    )
    .expect("valid CAST projections");
    let Statement::Select(select) = &statements[0] else {
        panic!("expected SELECT");
    };

    assert_eq!(
        select.items,
        [
            SelectItem::Cast {
                name: "reading".to_owned(),
                target_type: DataType::Float64,
                alias: None,
            },
            SelectItem::Cast {
                name: "reading".to_owned(),
                target_type: DataType::Float64,
                alias: Some("converted".to_owned()),
            },
        ]
    );
    assert!(select.predicate.is_some());
    assert_eq!(select.limit, Some(2));

    let statements =
        parse("SELECT CAST(ratio AS Int64) AS whole FROM samples").expect("valid inverse CAST");
    let Statement::Select(select) = &statements[0] else {
        panic!("expected SELECT");
    };
    assert_eq!(
        select.items,
        [SelectItem::Cast {
            name: "ratio".to_owned(),
            target_type: DataType::Int64,
            alias: Some("whole".to_owned()),
        }]
    );

    let statements =
        parse("SELECT CAST(reading AS Bool) AS present FROM samples").expect("valid Bool CAST");
    let Statement::Select(select) = &statements[0] else {
        panic!("expected SELECT");
    };
    assert_eq!(
        select.items,
        [SelectItem::Cast {
            name: "reading".to_owned(),
            target_type: DataType::Bool,
            alias: Some("present".to_owned()),
        }]
    );

    let statements = parse(
        "SELECT CAST(enabled AS Int64) FROM samples \
         ORDER BY cast(enabled as int64) DESC",
    )
    .expect("valid Bool-to-Int64 CAST with expression ordering");
    let Statement::Select(select) = &statements[0] else {
        panic!("expected SELECT");
    };
    assert_eq!(
        select.items,
        [SelectItem::Cast {
            name: "enabled".to_owned(),
            target_type: DataType::Int64,
            alias: None,
        }]
    );
    assert_eq!(select.order_by[0].name, "CAST(enabled AS Int64)");
    assert!(select.order_by[0].descending);

    let statements = parse(
        "SELECT CAST(enabled AS Float64) AS probability FROM samples \
         ORDER BY cast(enabled as float64) DESC",
    )
    .expect("valid Bool-to-Float64 CAST with expression ordering");
    let Statement::Select(select) = &statements[0] else {
        panic!("expected SELECT");
    };
    assert_eq!(
        select.items,
        [SelectItem::Cast {
            name: "enabled".to_owned(),
            target_type: DataType::Float64,
            alias: Some("probability".to_owned()),
        }]
    );
    assert_eq!(select.order_by[0].name, "CAST(enabled AS Float64)");
    assert!(select.order_by[0].descending);

    let statements = parse(
        "SELECT CAST(enabled AS String) AS text FROM samples \
         ORDER BY cast(enabled as string)",
    )
    .expect("valid Bool-to-String CAST with expression ordering");
    let Statement::Select(select) = &statements[0] else {
        panic!("expected SELECT");
    };
    assert_eq!(
        select.items,
        [SelectItem::Cast {
            name: "enabled".to_owned(),
            target_type: DataType::String,
            alias: Some("text".to_owned()),
        }]
    );
    assert_eq!(select.order_by[0].name, "CAST(enabled AS String)");

    let statements = parse(
        "SELECT CAST(reading AS String) AS text FROM samples \
         ORDER BY cast(reading as string)",
    )
    .expect("valid Int64-to-String CAST with expression ordering");
    let Statement::Select(select) = &statements[0] else {
        panic!("expected SELECT");
    };
    assert_eq!(
        select.items,
        [SelectItem::Cast {
            name: "reading".to_owned(),
            target_type: DataType::String,
            alias: Some("text".to_owned()),
        }]
    );
    assert_eq!(select.order_by[0].name, "CAST(reading AS String)");

    let limits = BatchSqlLimits {
        max_ast_list_items: 1,
        ..BatchSqlLimits::default()
    };
    parse_with_limits("SELECT CAST(reading AS Float64) FROM samples", limits)
        .expect("one CAST item fits the limit");
    assert_eq!(
        parse_with_limits(
            "SELECT CAST(reading AS Float64), reading FROM samples",
            limits,
        ),
        Err(Error::ResourceLimitExceeded {
            resource: "SQL AST list items",
            actual: 2,
            max: 1,
        })
    );
}

#[test]
fn cast_of_a_null_named_column_remains_a_table_projection() {
    let statements = parse("SELECT CAST(NULL AS Int64) AS converted FROM samples")
        .expect("NULL remains a legal CAST column identifier");
    let Statement::Select(select) = &statements[0] else {
        panic!("expected a table-backed SELECT");
    };
    assert_eq!(
        select.items,
        [SelectItem::Cast {
            name: "NULL".to_owned(),
            target_type: DataType::Int64,
            alias: Some("converted".to_owned()),
        }]
    );
    assert_eq!(select.table, "samples");

    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (NULL Float64); \
             INSERT INTO samples VALUES (2.5);",
        )
        .expect("setup succeeds");
    let result = query(&mut database, "SELECT CAST(NULL AS Int64) FROM samples");
    assert_eq!(
        result.columns,
        [ResultColumn {
            name: "CAST(NULL AS Int64)".to_owned(),
            data_type: DataType::Int64,
        }]
    );
    assert_eq!(result.rows, [vec![Value::Int64(2)]]);
}

#[test]
fn float64_to_int64_truncates_fractions_signs_and_boundary_values() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (reading Float64); \
             INSERT INTO samples VALUES \
             (-9223372036854775808.0), (9223372036854774784.0), \
             (-12.9), (7.9), (-0.9), (0.9);",
        )
        .expect("setup");

    let all = query(&mut database, "SELECT CAST(reading AS Int64) FROM samples");
    assert_eq!(
        all.columns,
        [ResultColumn {
            name: "CAST(reading AS Int64)".to_owned(),
            data_type: DataType::Int64,
        }]
    );
    assert_eq!(
        all.rows,
        [
            vec![Value::Int64(i64::MIN)],
            vec![Value::Int64(9_223_372_036_854_774_784)],
            vec![Value::Int64(-12)],
            vec![Value::Int64(7)],
            vec![Value::Int64(0)],
            vec![Value::Int64(0)],
        ]
    );

    let filtered = query(
        &mut database,
        "SELECT CAST(reading AS Int64) AS converted FROM samples \
         WHERE reading > -13.0 AND reading < 10.0 \
         ORDER BY converted DESC LIMIT 3",
    );
    assert_eq!(
        filtered.columns,
        [ResultColumn {
            name: "converted".to_owned(),
            data_type: DataType::Int64,
        }]
    );
    assert_eq!(
        filtered.rows,
        [
            vec![Value::Int64(7)],
            vec![Value::Int64(0)],
            vec![Value::Int64(0)],
        ]
    );

    database
        .execute(
            "CREATE TABLE ordered (id Int64, reading Float64); \
             INSERT INTO ordered VALUES (1, 1.9), (2, 1.1), (3, -1.1), (4, -1.9);",
        )
        .expect("ordered setup");
    assert_eq!(
        query(
            &mut database,
            "SELECT id, CAST(reading AS Int64) AS converted \
             FROM ordered ORDER BY converted LIMIT 3",
        )
        .rows,
        [
            vec![Value::Int64(3), Value::Int64(-1)],
            vec![Value::Int64(4), Value::Int64(-1)],
            vec![Value::Int64(1), Value::Int64(1)],
        ]
    );
}

#[test]
fn float64_to_int64_rejects_only_selected_out_of_range_values() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (reading Float64); \
             INSERT INTO samples VALUES \
             (9223372036854775808.0), (-9223372036854777856.0), (1.75);",
        )
        .expect("setup");

    assert_eq!(
        query(
            &mut database,
            "SELECT CAST(reading AS Int64) FROM samples \
             WHERE reading > -9223372036854775808.0 \
             AND reading < 9223372036854775808.0",
        )
        .rows,
        [vec![Value::Int64(1)]]
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT CAST(reading AS Int64) AS converted FROM samples \
             WHERE reading > 0.0 ORDER BY converted LIMIT 1",
        )
        .rows,
        [vec![Value::Int64(1)]]
    );

    for predicate in [
        "reading = 9223372036854775808.0",
        "reading = -9223372036854777856.0",
    ] {
        assert_eq!(
            database.execute(&format!(
                "SELECT CAST(reading AS Int64) FROM samples WHERE {predicate}"
            )),
            Err(Error::NumericOverflow("CAST(Float64 AS Int64)".to_owned())),
            "{predicate}"
        );
    }
}

#[test]
fn ordering_selects_below_range_casts_independent_of_source_order() {
    for values in [
        "(-9223372036854775808.0), (-9223372036854777856.0)",
        "(-9223372036854777856.0), (-9223372036854775808.0)",
    ] {
        let mut database = Database::new();
        database
            .execute(&format!(
                "CREATE TABLE samples (reading Float64); \
                 INSERT INTO samples VALUES {values};"
            ))
            .expect("setup");

        assert_eq!(
            database.execute(
                "SELECT CAST(reading AS Int64) AS converted \
                 FROM samples ORDER BY converted LIMIT 1"
            ),
            Err(Error::NumericOverflow("CAST(Float64 AS Int64)".to_owned())),
            "source values {values}"
        );
        assert_eq!(
            query(
                &mut database,
                "SELECT CAST(reading AS Int64) AS converted \
                 FROM samples ORDER BY converted DESC LIMIT 1",
            )
            .rows,
            [vec![Value::Int64(i64::MIN)]],
            "source values {values}"
        );
    }
}

#[test]
fn string_to_int64_accepts_signed_trim_free_decimal_text_and_extrema() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (id Int64, reading String); \
             INSERT INTO samples VALUES \
             (1, '-9223372036854775808'), (2, '+17'), (3, '000'), \
             (4, '-0'), (5, '9223372036854775807');",
        )
        .expect("setup");

    let all = query(
        &mut database,
        "SELECT CAST(reading AS Int64) AS converted FROM samples",
    );
    assert_eq!(
        all.columns,
        [ResultColumn {
            name: "converted".to_owned(),
            data_type: DataType::Int64,
        }]
    );
    assert_eq!(
        all.rows,
        [
            vec![Value::Int64(i64::MIN)],
            vec![Value::Int64(17)],
            vec![Value::Int64(0)],
            vec![Value::Int64(0)],
            vec![Value::Int64(i64::MAX)],
        ]
    );

    assert_eq!(
        query(
            &mut database,
            "SELECT id, CAST(reading AS Int64) FROM samples WHERE id >= 2 \
             ORDER BY CAST(reading AS Int64) DESC LIMIT 3 OFFSET 1",
        )
        .rows,
        [
            vec![Value::Int64(2), Value::Int64(17)],
            vec![Value::Int64(3), Value::Int64(0)],
            vec![Value::Int64(4), Value::Int64(0)],
        ]
    );
}

#[test]
fn string_to_int64_reports_invalid_and_overflowing_selected_values() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (id Int64, reading String); \
             INSERT INTO samples VALUES \
             (1, ''), (2, ' 1'), (3, '1 '), (4, '+'), (5, '-'), \
             (6, '1.0'), (7, '--1'), (8, 'twelve'), (9, '１２'), \
             (10, '9223372036854775808'), (11, '-9223372036854775809');",
        )
        .expect("setup");

    for id in 1..=9 {
        assert_eq!(
            database.execute(&format!(
                "SELECT CAST(reading AS Int64) FROM samples WHERE id = {id}"
            )),
            Err(Error::InvalidCast {
                source_type: DataType::String,
                target_type: DataType::Int64,
            }),
            "row {id}"
        );
    }
    for id in [10, 11] {
        assert_eq!(
            database.execute(&format!(
                "SELECT CAST(reading AS Int64) FROM samples WHERE id = {id}"
            )),
            Err(Error::NumericOverflow("CAST(String AS Int64)".to_owned())),
            "row {id}"
        );
    }
}

#[test]
fn string_to_int64_filters_and_pages_before_conversion_with_numeric_ordering() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (id Int64, reading String); \
             INSERT INTO samples VALUES \
             (1, 'bad'), (2, '10'), (3, '2'), (4, ''), (5, '-3'), \
             (6, '9223372036854775808'), (7, '-9223372036854775809');",
        )
        .expect("setup");

    assert_eq!(
        query(
            &mut database,
            "SELECT CAST(reading AS Int64) AS converted FROM samples \
             WHERE id = 2 OR id = 3 OR id = 5 ORDER BY converted",
        )
        .rows,
        [
            vec![Value::Int64(-3)],
            vec![Value::Int64(2)],
            vec![Value::Int64(10)],
        ]
    );

    assert_eq!(
        query(
            &mut database,
            "SELECT CAST(reading AS Int64) FROM samples LIMIT 2 OFFSET 1",
        )
        .rows,
        [vec![Value::Int64(10)], vec![Value::Int64(2)]]
    );

    assert_eq!(
        query(
            &mut database,
            "SELECT CAST(reading AS Int64) AS converted FROM samples \
             WHERE id = 2 OR id = 3 OR id = 6 OR id = 7 \
             ORDER BY converted LIMIT 2 OFFSET 1",
        )
        .rows,
        [vec![Value::Int64(2)], vec![Value::Int64(10)]]
    );
}

#[test]
fn repeated_string_to_int64_items_do_not_rescan_large_source_values() {
    const REPEATED_ITEMS: usize = 1_000;
    let mut database = Database::new();
    let large_zero = "0".repeat(1024 * 1024);
    database
        .execute(&format!(
            "CREATE TABLE samples (reading String); \
             INSERT INTO samples VALUES ('{large_zero}');"
        ))
        .expect("setup");

    let projections = (0..REPEATED_ITEMS)
        .map(|index| format!("CAST(reading AS Int64) AS c{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let ordering = (0..REPEATED_ITEMS)
        .map(|index| format!("c{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let result = query(
        &mut database,
        &format!("SELECT {projections} FROM samples ORDER BY {ordering}"),
    );

    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].len(), REPEATED_ITEMS);
    assert!(result.rows[0].iter().all(|value| *value == Value::Int64(0)));
}

#[test]
fn projects_negative_values_and_integer_extremes_with_filters_aliases_and_limits() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (reading Int64); \
             INSERT INTO samples VALUES \
             (-9223372036854775808), (-9007199254740993), (-7), (0), \
             (9223372036854775807);",
        )
        .expect("setup");

    let extremes = query(
        &mut database,
        "SELECT CAST(reading AS Float64) FROM samples \
         WHERE reading = -9223372036854775808 OR reading = 9223372036854775807",
    );
    assert_eq!(
        extremes.columns,
        [ResultColumn {
            name: "CAST(reading AS Float64)".to_owned(),
            data_type: DataType::Float64,
        }]
    );
    assert_eq!(
        extremes.rows,
        [
            vec![Value::Float64(-9_223_372_036_854_775_808.0)],
            vec![Value::Float64(9_223_372_036_854_775_808.0)],
        ]
    );

    let filtered = query(
        &mut database,
        "SELECT CAST(reading AS Float64) AS converted FROM samples \
         WHERE reading < 0 ORDER BY converted DESC LIMIT 2",
    );
    assert_eq!(
        filtered.columns,
        [ResultColumn {
            name: "converted".to_owned(),
            data_type: DataType::Float64,
        }]
    );
    assert_eq!(
        filtered.rows,
        [
            vec![Value::Float64(-7.0)],
            vec![Value::Float64(-9_007_199_254_740_992.0)],
        ]
    );
}

#[test]
fn int64_to_bool_maps_zero_and_nonzero_extrema_after_row_selection() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (id Int64, reading Int64); \
             INSERT INTO samples VALUES \
             (1, -9223372036854775808), (2, -1), (3, 0), (4, 1), \
             (5, 9223372036854775807);",
        )
        .expect("setup");

    let all = query(&mut database, "SELECT CAST(reading AS Bool) FROM samples");
    assert_eq!(
        all.columns,
        [ResultColumn {
            name: "CAST(reading AS Bool)".to_owned(),
            data_type: DataType::Bool,
        }]
    );
    assert_eq!(
        all.rows,
        [
            vec![Value::Bool(true)],
            vec![Value::Bool(true)],
            vec![Value::Bool(false)],
            vec![Value::Bool(true)],
            vec![Value::Bool(true)],
        ]
    );

    let selected = query(
        &mut database,
        "SELECT id, CAST(reading AS Bool) AS truthy FROM samples \
         WHERE reading >= 0 ORDER BY truthy, id DESC LIMIT 2",
    );
    assert_eq!(
        selected.columns,
        [
            ResultColumn {
                name: "id".to_owned(),
                data_type: DataType::Int64,
            },
            ResultColumn {
                name: "truthy".to_owned(),
                data_type: DataType::Bool,
            },
        ]
    );
    assert_eq!(
        selected.rows,
        [
            vec![Value::Int64(3), Value::Bool(false)],
            vec![Value::Int64(5), Value::Bool(true)],
        ]
    );
}

#[test]
fn int64_to_string_uses_canonical_decimal_text_and_lexicographic_ordering() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (id Int64, reading Int64); \
             INSERT INTO samples VALUES \
             (1, 0), (2, -1), (3, 1), (4, -10), (5, 10), \
             (6, -9223372036854775808), (7, 9223372036854775807);",
        )
        .expect("setup");

    let all = query(&mut database, "SELECT CAST(reading AS String) FROM samples");
    assert_eq!(
        all.columns,
        [ResultColumn {
            name: "CAST(reading AS String)".to_owned(),
            data_type: DataType::String,
        }]
    );
    assert_eq!(
        all.rows,
        [
            vec![Value::String("0".to_owned())],
            vec![Value::String("-1".to_owned())],
            vec![Value::String("1".to_owned())],
            vec![Value::String("-10".to_owned())],
            vec![Value::String("10".to_owned())],
            vec![Value::String("-9223372036854775808".to_owned())],
            vec![Value::String("9223372036854775807".to_owned())],
        ]
    );

    let aliased = query(
        &mut database,
        "SELECT id, CAST(reading AS String) AS text FROM samples \
         WHERE id >= 2 ORDER BY text LIMIT 4 OFFSET 1",
    );
    assert_eq!(
        aliased.columns,
        [
            ResultColumn {
                name: "id".to_owned(),
                data_type: DataType::Int64,
            },
            ResultColumn {
                name: "text".to_owned(),
                data_type: DataType::String,
            },
        ]
    );
    assert_eq!(
        aliased.rows,
        [
            vec![Value::Int64(4), Value::String("-10".to_owned())],
            vec![
                Value::Int64(6),
                Value::String("-9223372036854775808".to_owned()),
            ],
            vec![Value::Int64(3), Value::String("1".to_owned())],
            vec![Value::Int64(5), Value::String("10".to_owned())],
        ]
    );

    let expression_ordered = query(
        &mut database,
        "SELECT CAST(reading AS String) AS text FROM samples \
         ORDER BY CAST(reading AS String)",
    );
    assert_eq!(
        expression_ordered.columns,
        [ResultColumn {
            name: "text".to_owned(),
            data_type: DataType::String,
        }]
    );
    assert_eq!(
        expression_ordered.rows,
        [
            vec![Value::String("-1".to_owned())],
            vec![Value::String("-10".to_owned())],
            vec![Value::String("-9223372036854775808".to_owned())],
            vec![Value::String("0".to_owned())],
            vec![Value::String("1".to_owned())],
            vec![Value::String("10".to_owned())],
            vec![Value::String("9223372036854775807".to_owned())],
        ]
    );

    assert_eq!(
        query(
            &mut database,
            "SELECT CAST(reading AS String) FROM samples \
             ORDER BY CAST(reading AS String) LIMIT 3",
        )
        .rows,
        [
            vec![Value::String("-1".to_owned())],
            vec![Value::String("-10".to_owned())],
            vec![Value::String("-9223372036854775808".to_owned())],
        ]
    );
}

#[test]
fn float64_to_string_uses_shortest_finite_text_and_preserves_signed_zero() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (id Int64, reading Float64); \
             INSERT INTO samples VALUES \
             (1, -0.0), (2, 0.0), (3, -12.5), (4, 1.25), \
             (5, -5e-324), (6, 5e-324), \
             (7, -1.7976931348623157e308), (8, 1.7976931348623157e308);",
        )
        .expect("setup");

    let all = query(&mut database, "SELECT CAST(reading AS String) FROM samples");
    assert_eq!(
        all.columns,
        [ResultColumn {
            name: "CAST(reading AS String)".to_owned(),
            data_type: DataType::String,
        }]
    );
    assert_eq!(
        all.rows,
        [
            vec![Value::String("-0".to_owned())],
            vec![Value::String("0".to_owned())],
            vec![Value::String("-12.5".to_owned())],
            vec![Value::String("1.25".to_owned())],
            vec![Value::String((-5e-324_f64).to_string())],
            vec![Value::String(5e-324_f64.to_string())],
            vec![Value::String(f64::MIN.to_string())],
            vec![Value::String(f64::MAX.to_string())],
        ]
    );

    let aliased = query(
        &mut database,
        "SELECT id, CAST(reading AS String) AS text FROM samples \
         WHERE id <= 4 ORDER BY text LIMIT 2 OFFSET 1",
    );
    assert_eq!(
        aliased.columns,
        [
            ResultColumn {
                name: "id".to_owned(),
                data_type: DataType::Int64,
            },
            ResultColumn {
                name: "text".to_owned(),
                data_type: DataType::String,
            },
        ]
    );
    assert_eq!(
        aliased.rows,
        [
            vec![Value::Int64(3), Value::String("-12.5".to_owned())],
            vec![Value::Int64(2), Value::String("0".to_owned())],
        ]
    );

    assert_eq!(
        query(
            &mut database,
            "SELECT id, CAST(reading AS String) FROM samples WHERE id <= 4 \
             ORDER BY CAST(reading AS String) LIMIT 3",
        )
        .rows,
        [
            vec![Value::Int64(1), Value::String("-0".to_owned())],
            vec![Value::Int64(3), Value::String("-12.5".to_owned())],
            vec![Value::Int64(2), Value::String("0".to_owned())],
        ]
    );
}

#[test]
fn float64_to_bool_maps_signed_zero_and_finite_nonzero_extrema_after_row_selection() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (id Int64, reading Float64); \
             INSERT INTO samples VALUES \
             (1, -1.7976931348623157e308), (2, -5e-324), (3, -0.0), \
             (4, 0.0), (5, 5e-324), (6, 1.7976931348623157e308);",
        )
        .expect("setup");

    let all = query(&mut database, "SELECT CAST(reading AS Bool) FROM samples");
    assert_eq!(
        all.columns,
        [ResultColumn {
            name: "CAST(reading AS Bool)".to_owned(),
            data_type: DataType::Bool,
        }]
    );
    assert_eq!(
        all.rows,
        [
            vec![Value::Bool(true)],
            vec![Value::Bool(true)],
            vec![Value::Bool(false)],
            vec![Value::Bool(false)],
            vec![Value::Bool(true)],
            vec![Value::Bool(true)],
        ]
    );

    let selected = query(
        &mut database,
        "SELECT id, CAST(reading AS Bool) AS truthy FROM samples \
         WHERE id >= 2 ORDER BY truthy, id DESC LIMIT 3 OFFSET 1",
    );
    assert_eq!(
        selected.columns,
        [
            ResultColumn {
                name: "id".to_owned(),
                data_type: DataType::Int64,
            },
            ResultColumn {
                name: "truthy".to_owned(),
                data_type: DataType::Bool,
            },
        ]
    );
    assert_eq!(
        selected.rows,
        [
            vec![Value::Int64(3), Value::Bool(false)],
            vec![Value::Int64(6), Value::Bool(true)],
            vec![Value::Int64(5), Value::Bool(true)],
        ]
    );

    assert_eq!(
        query(
            &mut database,
            "SELECT id, CAST(reading AS Bool) FROM samples \
             ORDER BY CAST(reading AS Bool), id LIMIT 3",
        )
        .rows,
        [
            vec![Value::Int64(3), Value::Bool(false)],
            vec![Value::Int64(4), Value::Bool(false)],
            vec![Value::Int64(1), Value::Bool(true)],
        ]
    );
}

#[test]
fn bool_to_int64_maps_both_values_after_filtering_ordering_and_pagination() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (id Int64, enabled Bool); \
             INSERT INTO samples VALUES \
             (1, true), (2, false), (3, true), (4, false), (5, true);",
        )
        .expect("setup");

    let all = query(&mut database, "SELECT CAST(enabled AS Int64) FROM samples");
    assert_eq!(
        all.columns,
        [ResultColumn {
            name: "CAST(enabled AS Int64)".to_owned(),
            data_type: DataType::Int64,
        }]
    );
    assert_eq!(
        all.rows,
        [
            vec![Value::Int64(1)],
            vec![Value::Int64(0)],
            vec![Value::Int64(1)],
            vec![Value::Int64(0)],
            vec![Value::Int64(1)],
        ]
    );

    let aliased = query(
        &mut database,
        "SELECT id, CAST(enabled AS Int64) AS enabled_i64 FROM samples \
         WHERE id >= 2 ORDER BY enabled_i64, id DESC LIMIT 2 OFFSET 1",
    );
    assert_eq!(
        aliased.columns,
        [
            ResultColumn {
                name: "id".to_owned(),
                data_type: DataType::Int64,
            },
            ResultColumn {
                name: "enabled_i64".to_owned(),
                data_type: DataType::Int64,
            },
        ]
    );
    assert_eq!(
        aliased.rows,
        [
            vec![Value::Int64(2), Value::Int64(0)],
            vec![Value::Int64(5), Value::Int64(1)],
        ]
    );

    assert_eq!(
        query(
            &mut database,
            "SELECT id, CAST(enabled AS Int64) FROM samples \
             ORDER BY CAST(enabled AS Int64) DESC, id LIMIT 3",
        )
        .rows,
        [
            vec![Value::Int64(1), Value::Int64(1)],
            vec![Value::Int64(3), Value::Int64(1)],
            vec![Value::Int64(5), Value::Int64(1)],
        ]
    );
}

#[test]
fn bool_to_float64_maps_both_values_after_filtering_ordering_and_pagination() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (id Int64, enabled Bool); \
             INSERT INTO samples VALUES \
             (1, true), (2, false), (3, true), (4, false), (5, true);",
        )
        .expect("setup");

    let all = query(
        &mut database,
        "SELECT CAST(enabled AS Float64) FROM samples",
    );
    assert_eq!(
        all.columns,
        [ResultColumn {
            name: "CAST(enabled AS Float64)".to_owned(),
            data_type: DataType::Float64,
        }]
    );
    assert_eq!(
        all.rows,
        [
            vec![Value::Float64(1.0)],
            vec![Value::Float64(0.0)],
            vec![Value::Float64(1.0)],
            vec![Value::Float64(0.0)],
            vec![Value::Float64(1.0)],
        ]
    );

    let aliased = query(
        &mut database,
        "SELECT id, CAST(enabled AS Float64) AS enabled_f64 FROM samples \
         WHERE id >= 2 ORDER BY enabled_f64, id DESC LIMIT 2 OFFSET 1",
    );
    assert_eq!(
        aliased.columns,
        [
            ResultColumn {
                name: "id".to_owned(),
                data_type: DataType::Int64,
            },
            ResultColumn {
                name: "enabled_f64".to_owned(),
                data_type: DataType::Float64,
            },
        ]
    );
    assert_eq!(
        aliased.rows,
        [
            vec![Value::Int64(2), Value::Float64(0.0)],
            vec![Value::Int64(5), Value::Float64(1.0)],
        ]
    );

    assert_eq!(
        query(
            &mut database,
            "SELECT id, CAST(enabled AS Float64) FROM samples \
             ORDER BY CAST(enabled AS Float64) DESC, id LIMIT 3",
        )
        .rows,
        [
            vec![Value::Int64(1), Value::Float64(1.0)],
            vec![Value::Int64(3), Value::Float64(1.0)],
            vec![Value::Int64(5), Value::Float64(1.0)],
        ]
    );
}

#[test]
fn bool_to_string_maps_both_values_after_filtering_ordering_and_pagination() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (id Int64, enabled Bool); \
             INSERT INTO samples VALUES \
             (1, true), (2, false), (3, true), (4, false), (5, true);",
        )
        .expect("setup");

    let all = query(&mut database, "SELECT CAST(enabled AS String) FROM samples");
    assert_eq!(
        all.columns,
        [ResultColumn {
            name: "CAST(enabled AS String)".to_owned(),
            data_type: DataType::String,
        }]
    );
    assert_eq!(
        all.rows,
        [
            vec![Value::String("true".to_owned())],
            vec![Value::String("false".to_owned())],
            vec![Value::String("true".to_owned())],
            vec![Value::String("false".to_owned())],
            vec![Value::String("true".to_owned())],
        ]
    );

    let aliased = query(
        &mut database,
        "SELECT id, CAST(enabled AS String) AS enabled_text FROM samples \
         WHERE id >= 2 ORDER BY enabled_text, id DESC LIMIT 2 OFFSET 1",
    );
    assert_eq!(
        aliased.columns,
        [
            ResultColumn {
                name: "id".to_owned(),
                data_type: DataType::Int64,
            },
            ResultColumn {
                name: "enabled_text".to_owned(),
                data_type: DataType::String,
            },
        ]
    );
    assert_eq!(
        aliased.rows,
        [
            vec![Value::Int64(2), Value::String("false".to_owned())],
            vec![Value::Int64(5), Value::String("true".to_owned())],
        ]
    );

    assert_eq!(
        query(
            &mut database,
            "SELECT id, CAST(enabled AS String) FROM samples \
             ORDER BY CAST(enabled AS String) DESC, id LIMIT 3",
        )
        .rows,
        [
            vec![Value::Int64(1), Value::String("true".to_owned())],
            vec![Value::Int64(3), Value::String("true".to_owned())],
            vec![Value::Int64(5), Value::String("true".to_owned())],
        ]
    );
}

#[test]
fn rejects_unknown_and_invalid_cast_inputs_with_typed_errors() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE samples (f Float64, b Bool, s String, i Int64);")
        .expect("setup");

    assert_eq!(
        database.execute("SELECT CAST(missing AS Float64) FROM samples"),
        Err(Error::ColumnNotFound {
            table: "samples".to_owned(),
            column: "missing".to_owned(),
        })
    );

    for (name, actual) in [("f", DataType::Float64), ("s", DataType::String)] {
        assert_eq!(
            database.execute(&format!("SELECT CAST({name} AS Float64) FROM samples")),
            Err(Error::TypeMismatch {
                context: format!("CAST argument '{name}'"),
                expected: "Int64 or Bool".to_owned(),
                actual: actual.to_string(),
            }),
            "column {name}"
        );
    }

    assert_eq!(
        database.execute("SELECT CAST(missing AS Int64) FROM samples"),
        Err(Error::ColumnNotFound {
            table: "samples".to_owned(),
            column: "missing".to_owned(),
        })
    );
    assert_eq!(
        database.execute("SELECT CAST(i AS Int64) FROM samples"),
        Err(Error::TypeMismatch {
            context: "CAST argument 'i'".to_owned(),
            expected: "Float64, Bool, or String".to_owned(),
            actual: DataType::Int64.to_string(),
        })
    );

    assert_eq!(
        database.execute("SELECT CAST(missing AS Bool) FROM samples"),
        Err(Error::ColumnNotFound {
            table: "samples".to_owned(),
            column: "missing".to_owned(),
        })
    );
    for (name, actual) in [("b", DataType::Bool), ("s", DataType::String)] {
        assert_eq!(
            database.execute(&format!("SELECT CAST({name} AS Bool) FROM samples")),
            Err(Error::TypeMismatch {
                context: format!("CAST argument '{name}'"),
                expected: "Int64 or Float64".to_owned(),
                actual: actual.to_string(),
            }),
            "column {name}"
        );
    }

    assert_eq!(
        database.execute("SELECT CAST(missing AS String) FROM samples"),
        Err(Error::ColumnNotFound {
            table: "samples".to_owned(),
            column: "missing".to_owned(),
        })
    );
    assert_eq!(
        database.execute("SELECT CAST(s AS String) FROM samples"),
        Err(Error::TypeMismatch {
            context: "CAST argument 's'".to_owned(),
            expected: "Int64, Float64, or Bool".to_owned(),
            actual: DataType::String.to_string(),
        })
    );
}

#[test]
fn rejects_malformed_or_unsupported_cast_syntax() {
    for sql in [
        "SELECT CAST() FROM samples",
        "SELECT CAST(* AS Float64) FROM samples",
        "SELECT CAST(reading Float64) FROM samples",
        "SELECT CAST(reading AS) FROM samples",
        "SELECT CAST(reading AS Missing) FROM samples",
        "SELECT CAST(reading AS Float64 FROM samples",
        "SELECT CAST(reading AS Float64) converted FROM samples",
        "SELECT CAST(CAST(reading AS Float64) AS Float64) FROM samples",
    ] {
        assert!(parse(sql).is_err(), "{sql:?} must be rejected");
    }
}

#[test]
fn int64_to_bool_cast_obeys_result_caps() {
    let mut database = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 2,
        max_values: 2,
        max_bytes: usize::MAX,
        ..QueryResultLimits::default()
    });
    database
        .execute(
            "CREATE TABLE samples (reading Int64); \
             INSERT INTO samples VALUES (0), (1), (-1);",
        )
        .expect("setup");

    assert_eq!(
        query(
            &mut database,
            "SELECT CAST(reading AS Bool) FROM samples LIMIT 2",
        )
        .rows,
        [vec![Value::Bool(false)], vec![Value::Bool(true)]]
    );
    assert_eq!(
        database.execute("SELECT CAST(reading AS Bool) FROM samples"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result rows",
            actual: 3,
            max: 2,
        })
    );

    let mut value_limited = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 3,
        max_values: 5,
        max_bytes: usize::MAX,
        ..QueryResultLimits::default()
    });
    value_limited
        .execute(
            "CREATE TABLE samples (reading Int64); \
             INSERT INTO samples VALUES (0), (1), (-1);",
        )
        .expect("setup");
    assert_eq!(
        value_limited.execute("SELECT CAST(reading AS Bool), CAST(reading AS Bool) FROM samples"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result values",
            actual: 6,
            max: 5,
        })
    );

    let mut byte_limited = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 1,
        max_values: 1,
        max_bytes: 0,
        ..QueryResultLimits::default()
    });
    byte_limited
        .execute("CREATE TABLE samples (reading Int64); INSERT INTO samples VALUES (0);")
        .expect("setup");
    assert!(matches!(
        byte_limited.execute("SELECT CAST(reading AS Bool) FROM samples"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result bytes",
            max: 0,
            ..
        })
    ));
}

#[test]
fn float64_to_bool_cast_obeys_result_caps() {
    let mut database = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 2,
        max_values: 2,
        max_bytes: usize::MAX,
        ..QueryResultLimits::default()
    });
    database
        .execute(
            "CREATE TABLE samples (reading Float64); \
             INSERT INTO samples VALUES (-0.0), (0.5), (-0.5);",
        )
        .expect("setup");

    assert_eq!(
        query(
            &mut database,
            "SELECT CAST(reading AS Bool) FROM samples LIMIT 2",
        )
        .rows,
        [vec![Value::Bool(false)], vec![Value::Bool(true)]]
    );
    assert_eq!(
        database.execute("SELECT CAST(reading AS Bool) FROM samples"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result rows",
            actual: 3,
            max: 2,
        })
    );

    let mut value_limited = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 3,
        max_values: 5,
        max_bytes: usize::MAX,
        ..QueryResultLimits::default()
    });
    value_limited
        .execute(
            "CREATE TABLE samples (reading Float64); \
             INSERT INTO samples VALUES (-0.0), (0.5), (-0.5);",
        )
        .expect("setup");
    assert_eq!(
        value_limited.execute("SELECT CAST(reading AS Bool), CAST(reading AS Bool) FROM samples"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result values",
            actual: 6,
            max: 5,
        })
    );

    let mut byte_limited = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 1,
        max_values: 1,
        max_bytes: 0,
        ..QueryResultLimits::default()
    });
    byte_limited
        .execute("CREATE TABLE samples (reading Float64); INSERT INTO samples VALUES (-0.0);")
        .expect("setup");
    assert!(matches!(
        byte_limited.execute("SELECT CAST(reading AS Bool) FROM samples"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result bytes",
            max: 0,
            ..
        })
    ));
}

#[test]
fn bool_to_int64_cast_obeys_result_caps() {
    let mut database = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 2,
        max_values: 2,
        max_bytes: usize::MAX,
        ..QueryResultLimits::default()
    });
    database
        .execute(
            "CREATE TABLE samples (enabled Bool); \
             INSERT INTO samples VALUES (false), (true), (false);",
        )
        .expect("setup");

    assert_eq!(
        query(
            &mut database,
            "SELECT CAST(enabled AS Int64) FROM samples LIMIT 2",
        )
        .rows,
        [vec![Value::Int64(0)], vec![Value::Int64(1)]]
    );
    assert_eq!(
        database.execute("SELECT CAST(enabled AS Int64) FROM samples"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result rows",
            actual: 3,
            max: 2,
        })
    );

    let mut value_limited = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 3,
        max_values: 5,
        max_bytes: usize::MAX,
        ..QueryResultLimits::default()
    });
    value_limited
        .execute(
            "CREATE TABLE samples (enabled Bool); \
             INSERT INTO samples VALUES (false), (true), (false);",
        )
        .expect("setup");
    assert_eq!(
        value_limited.execute("SELECT CAST(enabled AS Int64), CAST(enabled AS Int64) FROM samples"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result values",
            actual: 6,
            max: 5,
        })
    );

    let mut byte_limited = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 1,
        max_values: 1,
        max_bytes: 0,
        ..QueryResultLimits::default()
    });
    byte_limited
        .execute("CREATE TABLE samples (enabled Bool); INSERT INTO samples VALUES (false);")
        .expect("setup");
    assert!(matches!(
        byte_limited.execute("SELECT CAST(enabled AS Int64) FROM samples"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result bytes",
            max: 0,
            ..
        })
    ));
}

#[test]
fn string_to_int64_cast_obeys_result_caps() {
    let mut database = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 2,
        max_values: 2,
        max_bytes: usize::MAX,
        ..QueryResultLimits::default()
    });
    database
        .execute(
            "CREATE TABLE samples (reading String); \
             INSERT INTO samples VALUES ('0'), ('1'), ('2');",
        )
        .expect("setup");

    assert_eq!(
        query(
            &mut database,
            "SELECT CAST(reading AS Int64) FROM samples LIMIT 2",
        )
        .rows,
        [vec![Value::Int64(0)], vec![Value::Int64(1)]]
    );
    assert_eq!(
        database.execute("SELECT CAST(reading AS Int64) FROM samples"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result rows",
            actual: 3,
            max: 2,
        })
    );

    let mut value_limited = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 3,
        max_values: 5,
        max_bytes: usize::MAX,
        ..QueryResultLimits::default()
    });
    value_limited
        .execute(
            "CREATE TABLE samples (reading String); \
             INSERT INTO samples VALUES ('0'), ('1'), ('2');",
        )
        .expect("setup");
    assert_eq!(
        value_limited.execute("SELECT CAST(reading AS Int64), CAST(reading AS Int64) FROM samples"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result values",
            actual: 6,
            max: 5,
        })
    );

    let mut byte_limited = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 1,
        max_values: 1,
        max_bytes: 0,
        ..QueryResultLimits::default()
    });
    byte_limited
        .execute("CREATE TABLE samples (reading String); INSERT INTO samples VALUES ('0');")
        .expect("setup");
    assert!(matches!(
        byte_limited.execute("SELECT CAST(reading AS Int64) FROM samples"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result bytes",
            max: 0,
            ..
        })
    ));
}

#[test]
fn bool_to_float64_cast_obeys_result_caps() {
    let mut database = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 2,
        max_values: 2,
        max_bytes: usize::MAX,
        ..QueryResultLimits::default()
    });
    database
        .execute(
            "CREATE TABLE samples (enabled Bool); \
             INSERT INTO samples VALUES (false), (true), (false);",
        )
        .expect("setup");

    assert_eq!(
        query(
            &mut database,
            "SELECT CAST(enabled AS Float64) FROM samples LIMIT 2",
        )
        .rows,
        [vec![Value::Float64(0.0)], vec![Value::Float64(1.0)]]
    );
    assert_eq!(
        database.execute("SELECT CAST(enabled AS Float64) FROM samples"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result rows",
            actual: 3,
            max: 2,
        })
    );

    let mut value_limited = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 3,
        max_values: 5,
        max_bytes: usize::MAX,
        ..QueryResultLimits::default()
    });
    value_limited
        .execute(
            "CREATE TABLE samples (enabled Bool); \
             INSERT INTO samples VALUES (false), (true), (false);",
        )
        .expect("setup");
    assert_eq!(
        value_limited
            .execute("SELECT CAST(enabled AS Float64), CAST(enabled AS Float64) FROM samples",),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result values",
            actual: 6,
            max: 5,
        })
    );

    let mut byte_limited = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 1,
        max_values: 1,
        max_bytes: 0,
        ..QueryResultLimits::default()
    });
    byte_limited
        .execute("CREATE TABLE samples (enabled Bool); INSERT INTO samples VALUES (false);")
        .expect("setup");
    assert!(matches!(
        byte_limited.execute("SELECT CAST(enabled AS Float64) FROM samples"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result bytes",
            max: 0,
            ..
        })
    ));
}

#[test]
fn bool_to_string_cast_accounts_for_each_payload_before_allocation() {
    let result_name = "text";
    let fixed_bytes = std::mem::size_of::<ResultColumn>()
        + result_name.len()
        + std::mem::size_of::<Vec<Value>>()
        + std::mem::size_of::<Value>();
    let sql = "SELECT CAST(enabled AS String) AS text FROM samples";

    for (literal, rendered) in [(false, "false"), (true, "true")] {
        let setup =
            format!("CREATE TABLE samples (enabled Bool); INSERT INTO samples VALUES ({literal});");
        let exact_bytes = fixed_bytes + rendered.len();
        let mut exact = Database::with_query_result_limits(QueryResultLimits {
            max_rows: 1,
            max_values: 1,
            max_bytes: exact_bytes,
            ..QueryResultLimits::default()
        });
        exact.execute(&setup).expect("setup");
        assert_eq!(
            query(&mut exact, sql).rows,
            [vec![Value::String(rendered.to_owned())]]
        );

        let mut limited = Database::with_query_result_limits(QueryResultLimits {
            max_rows: 1,
            max_values: 1,
            max_bytes: exact_bytes - 1,
            ..QueryResultLimits::default()
        });
        limited.execute(&setup).expect("setup");
        assert_eq!(
            limited.execute(sql),
            Err(Error::ResourceLimitExceeded {
                resource: "SELECT result bytes",
                actual: exact_bytes,
                max: exact_bytes - 1,
            }),
            "{rendered} payload"
        );
    }
}

#[test]
fn int64_to_string_cast_accounts_for_each_decimal_payload_before_allocation() {
    let result_name = "text";
    let fixed_bytes = std::mem::size_of::<ResultColumn>()
        + result_name.len()
        + std::mem::size_of::<Vec<Value>>()
        + std::mem::size_of::<Value>();
    let sql = "SELECT CAST(reading AS String) AS text FROM samples";

    for (literal, rendered) in [
        ("0", "0"),
        ("-7", "-7"),
        ("9223372036854775807", "9223372036854775807"),
        ("-9223372036854775808", "-9223372036854775808"),
    ] {
        let setup = format!(
            "CREATE TABLE samples (reading Int64); INSERT INTO samples VALUES ({literal});"
        );
        let exact_bytes = fixed_bytes + rendered.len();
        let mut exact = Database::with_query_result_limits(QueryResultLimits {
            max_rows: 1,
            max_values: 1,
            max_bytes: exact_bytes,
            ..QueryResultLimits::default()
        });
        exact.execute(&setup).expect("setup");
        assert_eq!(
            query(&mut exact, sql).rows,
            [vec![Value::String(rendered.to_owned())]]
        );

        let mut limited = Database::with_query_result_limits(QueryResultLimits {
            max_rows: 1,
            max_values: 1,
            max_bytes: exact_bytes - 1,
            ..QueryResultLimits::default()
        });
        limited.execute(&setup).expect("setup");
        assert_eq!(
            limited.execute(sql),
            Err(Error::ResourceLimitExceeded {
                resource: "SELECT result bytes",
                actual: exact_bytes,
                max: exact_bytes - 1,
            }),
            "{rendered} payload"
        );
    }
}

#[test]
fn float64_to_string_cast_accounts_for_each_payload_before_allocation() {
    let result_name = "text";
    let fixed_bytes = std::mem::size_of::<ResultColumn>()
        + result_name.len()
        + std::mem::size_of::<Vec<Value>>()
        + std::mem::size_of::<Value>();
    let sql = "SELECT CAST(reading AS String) AS text FROM samples";

    for (literal, rendered) in [
        ("-0.0", "-0".to_owned()),
        ("1.25", "1.25".to_owned()),
        ("-5e-324", (-5e-324_f64).to_string()),
        ("1.7976931348623157e308", f64::MAX.to_string()),
    ] {
        let setup = format!(
            "CREATE TABLE samples (reading Float64); INSERT INTO samples VALUES ({literal});"
        );
        let exact_bytes = fixed_bytes + rendered.len();
        let mut exact = Database::with_query_result_limits(QueryResultLimits {
            max_rows: 1,
            max_values: 1,
            max_bytes: exact_bytes,
            ..QueryResultLimits::default()
        });
        exact.execute(&setup).expect("setup");
        assert_eq!(
            query(&mut exact, sql).rows,
            [vec![Value::String(rendered.clone())]]
        );

        let mut limited = Database::with_query_result_limits(QueryResultLimits {
            max_rows: 1,
            max_values: 1,
            max_bytes: exact_bytes - 1,
            ..QueryResultLimits::default()
        });
        limited.execute(&setup).expect("setup");
        assert_eq!(
            limited.execute(sql),
            Err(Error::ResourceLimitExceeded {
                resource: "SELECT result bytes",
                actual: exact_bytes,
                max: exact_bytes - 1,
            }),
            "{rendered} payload"
        );
    }

    let mut selected_only = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 1,
        max_values: 1,
        max_bytes: fixed_bytes + 2,
        ..QueryResultLimits::default()
    });
    selected_only
        .execute(
            "CREATE TABLE samples (reading Float64); \
             INSERT INTO samples VALUES (1.7976931348623157e308), (-0.0);",
        )
        .expect("setup");
    assert_eq!(
        query(&mut selected_only, &format!("{sql} LIMIT 1 OFFSET 1")).rows,
        [vec![Value::String("-0".to_owned())]]
    );
}

#[test]
fn cast_remains_an_ordinary_projection() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (reading Int64, ratio Float64, enabled Bool, text String); \
             INSERT INTO samples VALUES (1, 1.5, true, '1');",
        )
        .expect("setup");

    assert_eq!(
        database.execute("SELECT CAST(reading AS Float64), COUNT(*) FROM samples GROUP BY reading"),
        Err(Error::InvalidQuery(
            "CAST projections are only supported in ungrouped SELECT queries".to_owned()
        ))
    );
    assert_eq!(
        database.execute("SELECT CAST(reading AS Bool), COUNT(*) FROM samples GROUP BY reading"),
        Err(Error::InvalidQuery(
            "CAST projections are only supported in ungrouped SELECT queries".to_owned()
        ))
    );
    assert_eq!(
        database.execute("SELECT CAST(enabled AS Int64), COUNT(*) FROM samples GROUP BY enabled"),
        Err(Error::InvalidQuery(
            "CAST projections are only supported in ungrouped SELECT queries".to_owned()
        ))
    );
    assert_eq!(
        database.execute("SELECT CAST(text AS Int64), COUNT(*) FROM samples GROUP BY text"),
        Err(Error::InvalidQuery(
            "CAST projections are only supported in ungrouped SELECT queries".to_owned()
        ))
    );
    assert_eq!(
        database.execute("SELECT CAST(enabled AS Float64), COUNT(*) FROM samples GROUP BY enabled"),
        Err(Error::InvalidQuery(
            "CAST projections are only supported in ungrouped SELECT queries".to_owned()
        ))
    );
    assert_eq!(
        database.execute("SELECT CAST(enabled AS String), COUNT(*) FROM samples GROUP BY enabled"),
        Err(Error::InvalidQuery(
            "CAST projections are only supported in ungrouped SELECT queries".to_owned()
        ))
    );
    assert_eq!(
        database.execute("SELECT CAST(reading AS String), COUNT(*) FROM samples GROUP BY reading"),
        Err(Error::InvalidQuery(
            "CAST projections are only supported in ungrouped SELECT queries".to_owned()
        ))
    );
    assert_eq!(
        database.execute("SELECT CAST(ratio AS String), COUNT(*) FROM samples GROUP BY ratio"),
        Err(Error::InvalidQuery(
            "CAST projections are only supported in ungrouped SELECT queries".to_owned()
        ))
    );
}

#[test]
fn emits_float64_to_int64_as_typed_csv_and_json() {
    let sql = "CREATE TABLE samples (reading Float64); \
               INSERT INTO samples VALUES (-7.9), (0.9), (12.4); \
               SELECT CAST(reading AS Int64) AS converted \
               FROM samples ORDER BY converted;";

    let mut csv = Vec::new();
    run_csv_batch(sql.as_bytes(), &mut csv).expect("CSV batch succeeds");
    assert_eq!(String::from_utf8(csv).unwrap(), "converted\n-7\n0\n12\n");

    let mut json = Vec::new();
    run_json_batch(sql.as_bytes(), &mut json).expect("JSON batch succeeds");
    assert_eq!(
        String::from_utf8(json).unwrap(),
        "{\"columns\":[{\"name\":\"converted\",\"type\":\"Int64\"}],\"rows\":[[-7],[0],[12]]}\n"
    );
}

#[test]
fn emits_string_to_int64_in_all_cli_formats() {
    let sql = "CREATE TABLE samples (reading String); \
               INSERT INTO samples VALUES ('2'), ('-10'), ('+0'); \
               SELECT CAST(reading AS Int64) AS converted \
               FROM samples ORDER BY converted;";

    let mut table = Vec::new();
    run_table_batch(sql.as_bytes(), &mut table).expect("table batch succeeds");
    assert_eq!(
        String::from_utf8(table).unwrap(),
        "+-----------+\n\
         | converted |\n\
         +-----------+\n\
         | -10       |\n\
         | 0         |\n\
         | 2         |\n\
         +-----------+\n"
    );

    let mut csv = Vec::new();
    run_csv_batch(sql.as_bytes(), &mut csv).expect("CSV batch succeeds");
    assert_eq!(String::from_utf8(csv).unwrap(), "converted\n-10\n0\n2\n");

    let mut tsv = Vec::new();
    run_tsv_batch(sql.as_bytes(), &mut tsv).expect("TSV batch succeeds");
    assert_eq!(String::from_utf8(tsv).unwrap(), "converted\n-10\n0\n2\n");

    let mut json = Vec::new();
    run_json_batch(sql.as_bytes(), &mut json).expect("JSON batch succeeds");
    assert_eq!(
        String::from_utf8(json).unwrap(),
        "{\"columns\":[{\"name\":\"converted\",\"type\":\"Int64\"}],\"rows\":[[-10],[0],[2]]}\n"
    );

    let mut json_each_row = Vec::new();
    run_json_each_row_batch(sql.as_bytes(), &mut json_each_row)
        .expect("JSONEachRow batch succeeds");
    assert_eq!(
        String::from_utf8(json_each_row).unwrap(),
        "{\"converted\":-10}\n{\"converted\":0}\n{\"converted\":2}\n"
    );

    let mut json_compact_each_row = Vec::new();
    run_json_compact_each_row_batch(sql.as_bytes(), &mut json_compact_each_row)
        .expect("JSONCompactEachRow batch succeeds");
    assert_eq!(
        String::from_utf8(json_compact_each_row).unwrap(),
        "[-10]\n[0]\n[2]\n"
    );
}

#[test]
fn emits_float64_to_bool_in_all_cli_formats() {
    let sql = "CREATE TABLE samples (reading Float64); \
               INSERT INTO samples VALUES (-0.0), (-0.25), (0.25); \
               SELECT CAST(reading AS Bool) AS enabled \
               FROM samples ORDER BY enabled;";

    let mut table = Vec::new();
    run_table_batch(sql.as_bytes(), &mut table).expect("table batch succeeds");
    assert_eq!(
        String::from_utf8(table).unwrap(),
        "+---------+\n\
         | enabled |\n\
         +---------+\n\
         | false   |\n\
         | true    |\n\
         | true    |\n\
         +---------+\n"
    );

    let mut csv = Vec::new();
    run_csv_batch(sql.as_bytes(), &mut csv).expect("CSV batch succeeds");
    assert_eq!(
        String::from_utf8(csv).unwrap(),
        "enabled\nfalse\ntrue\ntrue\n"
    );

    let mut tsv = Vec::new();
    run_tsv_batch(sql.as_bytes(), &mut tsv).expect("TSV batch succeeds");
    assert_eq!(
        String::from_utf8(tsv).unwrap(),
        "enabled\nfalse\ntrue\ntrue\n"
    );

    let mut json = Vec::new();
    run_json_batch(sql.as_bytes(), &mut json).expect("JSON batch succeeds");
    assert_eq!(
        String::from_utf8(json).unwrap(),
        "{\"columns\":[{\"name\":\"enabled\",\"type\":\"Bool\"}],\"rows\":[[false],[true],[true]]}\n"
    );

    let mut json_each_row = Vec::new();
    run_json_each_row_batch(sql.as_bytes(), &mut json_each_row)
        .expect("JSONEachRow batch succeeds");
    assert_eq!(
        String::from_utf8(json_each_row).unwrap(),
        "{\"enabled\":false}\n{\"enabled\":true}\n{\"enabled\":true}\n"
    );

    let mut json_compact_each_row = Vec::new();
    run_json_compact_each_row_batch(sql.as_bytes(), &mut json_compact_each_row)
        .expect("JSONCompactEachRow batch succeeds");
    assert_eq!(
        String::from_utf8(json_compact_each_row).unwrap(),
        "[false]\n[true]\n[true]\n"
    );
}

#[test]
fn emits_int64_to_bool_in_all_cli_formats() {
    let sql = "CREATE TABLE samples (reading Int64); \
               INSERT INTO samples VALUES (-7), (0), (12); \
               SELECT CAST(reading AS Bool) AS enabled \
               FROM samples ORDER BY enabled;";

    let mut table = Vec::new();
    run_table_batch(sql.as_bytes(), &mut table).expect("table batch succeeds");
    assert_eq!(
        String::from_utf8(table).unwrap(),
        "+---------+\n\
         | enabled |\n\
         +---------+\n\
         | false   |\n\
         | true    |\n\
         | true    |\n\
         +---------+\n"
    );

    let mut csv = Vec::new();
    run_csv_batch(sql.as_bytes(), &mut csv).expect("CSV batch succeeds");
    assert_eq!(
        String::from_utf8(csv).unwrap(),
        "enabled\nfalse\ntrue\ntrue\n"
    );

    let mut tsv = Vec::new();
    run_tsv_batch(sql.as_bytes(), &mut tsv).expect("TSV batch succeeds");
    assert_eq!(
        String::from_utf8(tsv).unwrap(),
        "enabled\nfalse\ntrue\ntrue\n"
    );

    let mut json = Vec::new();
    run_json_batch(sql.as_bytes(), &mut json).expect("JSON batch succeeds");
    assert_eq!(
        String::from_utf8(json).unwrap(),
        "{\"columns\":[{\"name\":\"enabled\",\"type\":\"Bool\"}],\"rows\":[[false],[true],[true]]}\n"
    );

    let mut json_each_row = Vec::new();
    run_json_each_row_batch(sql.as_bytes(), &mut json_each_row)
        .expect("JSONEachRow batch succeeds");
    assert_eq!(
        String::from_utf8(json_each_row).unwrap(),
        "{\"enabled\":false}\n{\"enabled\":true}\n{\"enabled\":true}\n"
    );

    let mut json_compact_each_row = Vec::new();
    run_json_compact_each_row_batch(sql.as_bytes(), &mut json_compact_each_row)
        .expect("JSONCompactEachRow batch succeeds");
    assert_eq!(
        String::from_utf8(json_compact_each_row).unwrap(),
        "[false]\n[true]\n[true]\n"
    );
}

#[test]
fn emits_bool_to_int64_in_all_cli_formats() {
    let sql = "CREATE TABLE samples (enabled Bool); \
               INSERT INTO samples VALUES (true), (false), (true); \
               SELECT CAST(enabled AS Int64) AS enabled_i64 \
               FROM samples ORDER BY enabled_i64;";

    let mut table = Vec::new();
    run_table_batch(sql.as_bytes(), &mut table).expect("table batch succeeds");
    assert_eq!(
        String::from_utf8(table).unwrap(),
        "+-------------+\n\
         | enabled_i64 |\n\
         +-------------+\n\
         | 0           |\n\
         | 1           |\n\
         | 1           |\n\
         +-------------+\n"
    );

    let mut csv = Vec::new();
    run_csv_batch(sql.as_bytes(), &mut csv).expect("CSV batch succeeds");
    assert_eq!(String::from_utf8(csv).unwrap(), "enabled_i64\n0\n1\n1\n");

    let mut tsv = Vec::new();
    run_tsv_batch(sql.as_bytes(), &mut tsv).expect("TSV batch succeeds");
    assert_eq!(String::from_utf8(tsv).unwrap(), "enabled_i64\n0\n1\n1\n");

    let mut json = Vec::new();
    run_json_batch(sql.as_bytes(), &mut json).expect("JSON batch succeeds");
    assert_eq!(
        String::from_utf8(json).unwrap(),
        "{\"columns\":[{\"name\":\"enabled_i64\",\"type\":\"Int64\"}],\"rows\":[[0],[1],[1]]}\n"
    );

    let mut json_each_row = Vec::new();
    run_json_each_row_batch(sql.as_bytes(), &mut json_each_row)
        .expect("JSONEachRow batch succeeds");
    assert_eq!(
        String::from_utf8(json_each_row).unwrap(),
        "{\"enabled_i64\":0}\n{\"enabled_i64\":1}\n{\"enabled_i64\":1}\n"
    );

    let mut json_compact_each_row = Vec::new();
    run_json_compact_each_row_batch(sql.as_bytes(), &mut json_compact_each_row)
        .expect("JSONCompactEachRow batch succeeds");
    assert_eq!(
        String::from_utf8(json_compact_each_row).unwrap(),
        "[0]\n[1]\n[1]\n"
    );
}

#[test]
fn emits_bool_to_float64_in_all_cli_formats() {
    let sql = "CREATE TABLE samples (enabled Bool); \
               INSERT INTO samples VALUES (true), (false), (true); \
               SELECT CAST(enabled AS Float64) AS enabled_f64 \
               FROM samples ORDER BY enabled_f64;";

    let mut table = Vec::new();
    run_table_batch(sql.as_bytes(), &mut table).expect("table batch succeeds");
    assert_eq!(
        String::from_utf8(table).unwrap(),
        "+-------------+\n\
         | enabled_f64 |\n\
         +-------------+\n\
         | 0.0         |\n\
         | 1.0         |\n\
         | 1.0         |\n\
         +-------------+\n"
    );

    let mut csv = Vec::new();
    run_csv_batch(sql.as_bytes(), &mut csv).expect("CSV batch succeeds");
    assert_eq!(
        String::from_utf8(csv).unwrap(),
        "enabled_f64\n0.0\n1.0\n1.0\n"
    );

    let mut tsv = Vec::new();
    run_tsv_batch(sql.as_bytes(), &mut tsv).expect("TSV batch succeeds");
    assert_eq!(
        String::from_utf8(tsv).unwrap(),
        "enabled_f64\n0.0\n1.0\n1.0\n"
    );

    let mut json = Vec::new();
    run_json_batch(sql.as_bytes(), &mut json).expect("JSON batch succeeds");
    assert_eq!(
        String::from_utf8(json).unwrap(),
        "{\"columns\":[{\"name\":\"enabled_f64\",\"type\":\"Float64\"}],\"rows\":[[0.0],[1.0],[1.0]]}\n"
    );

    let mut json_each_row = Vec::new();
    run_json_each_row_batch(sql.as_bytes(), &mut json_each_row)
        .expect("JSONEachRow batch succeeds");
    assert_eq!(
        String::from_utf8(json_each_row).unwrap(),
        "{\"enabled_f64\":0.0}\n{\"enabled_f64\":1.0}\n{\"enabled_f64\":1.0}\n"
    );

    let mut json_compact_each_row = Vec::new();
    run_json_compact_each_row_batch(sql.as_bytes(), &mut json_compact_each_row)
        .expect("JSONCompactEachRow batch succeeds");
    assert_eq!(
        String::from_utf8(json_compact_each_row).unwrap(),
        "[0.0]\n[1.0]\n[1.0]\n"
    );
}

#[test]
fn emits_bool_to_string_in_all_cli_formats() {
    let sql = "CREATE TABLE samples (enabled Bool); \
               INSERT INTO samples VALUES (true), (false); \
               SELECT CAST(enabled AS String) AS text \
               FROM samples ORDER BY text;";

    let mut table = Vec::new();
    run_table_batch(sql.as_bytes(), &mut table).expect("table batch succeeds");
    assert_eq!(
        String::from_utf8(table).unwrap(),
        "+-------+\n\
         | text  |\n\
         +-------+\n\
         | false |\n\
         | true  |\n\
         +-------+\n"
    );

    let mut csv = Vec::new();
    run_csv_batch(sql.as_bytes(), &mut csv).expect("CSV batch succeeds");
    assert_eq!(String::from_utf8(csv).unwrap(), "text\nfalse\ntrue\n");

    let mut tsv = Vec::new();
    run_tsv_batch(sql.as_bytes(), &mut tsv).expect("TSV batch succeeds");
    assert_eq!(String::from_utf8(tsv).unwrap(), "text\nfalse\ntrue\n");

    let mut json = Vec::new();
    run_json_batch(sql.as_bytes(), &mut json).expect("JSON batch succeeds");
    assert_eq!(
        String::from_utf8(json).unwrap(),
        "{\"columns\":[{\"name\":\"text\",\"type\":\"String\"}],\"rows\":[[\"false\"],[\"true\"]]}\n"
    );

    let mut json_each_row = Vec::new();
    run_json_each_row_batch(sql.as_bytes(), &mut json_each_row)
        .expect("JSONEachRow batch succeeds");
    assert_eq!(
        String::from_utf8(json_each_row).unwrap(),
        "{\"text\":\"false\"}\n{\"text\":\"true\"}\n"
    );

    let mut json_compact_each_row = Vec::new();
    run_json_compact_each_row_batch(sql.as_bytes(), &mut json_compact_each_row)
        .expect("JSONCompactEachRow batch succeeds");
    assert_eq!(
        String::from_utf8(json_compact_each_row).unwrap(),
        "[\"false\"]\n[\"true\"]\n"
    );
}

#[test]
fn emits_int64_to_string_in_all_cli_formats() {
    let sql = "CREATE TABLE samples (reading Int64); \
               INSERT INTO samples VALUES (2), (-10), (0); \
               SELECT CAST(reading AS String) AS text \
               FROM samples ORDER BY text;";

    let mut table = Vec::new();
    run_table_batch(sql.as_bytes(), &mut table).expect("table batch succeeds");
    assert_eq!(
        String::from_utf8(table).unwrap(),
        "+------+\n\
         | text |\n\
         +------+\n\
         | -10  |\n\
         | 0    |\n\
         | 2    |\n\
         +------+\n"
    );

    let mut csv = Vec::new();
    run_csv_batch(sql.as_bytes(), &mut csv).expect("CSV batch succeeds");
    assert_eq!(String::from_utf8(csv).unwrap(), "text\n-10\n0\n2\n");

    let mut tsv = Vec::new();
    run_tsv_batch(sql.as_bytes(), &mut tsv).expect("TSV batch succeeds");
    assert_eq!(String::from_utf8(tsv).unwrap(), "text\n-10\n0\n2\n");

    let mut json = Vec::new();
    run_json_batch(sql.as_bytes(), &mut json).expect("JSON batch succeeds");
    assert_eq!(
        String::from_utf8(json).unwrap(),
        "{\"columns\":[{\"name\":\"text\",\"type\":\"String\"}],\"rows\":[[\"-10\"],[\"0\"],[\"2\"]]}\n"
    );

    let mut json_each_row = Vec::new();
    run_json_each_row_batch(sql.as_bytes(), &mut json_each_row)
        .expect("JSONEachRow batch succeeds");
    assert_eq!(
        String::from_utf8(json_each_row).unwrap(),
        "{\"text\":\"-10\"}\n{\"text\":\"0\"}\n{\"text\":\"2\"}\n"
    );

    let mut json_compact_each_row = Vec::new();
    run_json_compact_each_row_batch(sql.as_bytes(), &mut json_compact_each_row)
        .expect("JSONCompactEachRow batch succeeds");
    assert_eq!(
        String::from_utf8(json_compact_each_row).unwrap(),
        "[\"-10\"]\n[\"0\"]\n[\"2\"]\n"
    );
}

#[test]
fn emits_float64_to_string_in_all_cli_formats() {
    let sql = "CREATE TABLE samples (reading Float64); \
               INSERT INTO samples VALUES (10.0), (-0.0), (1.25); \
               SELECT CAST(reading AS String) AS text \
               FROM samples ORDER BY text;";

    let mut table = Vec::new();
    run_table_batch(sql.as_bytes(), &mut table).expect("table batch succeeds");
    assert_eq!(
        String::from_utf8(table).unwrap(),
        "+------+\n\
         | text |\n\
         +------+\n\
         | -0   |\n\
         | 1.25 |\n\
         | 10   |\n\
         +------+\n"
    );

    let mut csv = Vec::new();
    run_csv_batch(sql.as_bytes(), &mut csv).expect("CSV batch succeeds");
    assert_eq!(String::from_utf8(csv).unwrap(), "text\n-0\n1.25\n10\n");

    let mut tsv = Vec::new();
    run_tsv_batch(sql.as_bytes(), &mut tsv).expect("TSV batch succeeds");
    assert_eq!(String::from_utf8(tsv).unwrap(), "text\n-0\n1.25\n10\n");

    let mut json = Vec::new();
    run_json_batch(sql.as_bytes(), &mut json).expect("JSON batch succeeds");
    assert_eq!(
        String::from_utf8(json).unwrap(),
        "{\"columns\":[{\"name\":\"text\",\"type\":\"String\"}],\"rows\":[[\"-0\"],[\"1.25\"],[\"10\"]]}\n"
    );

    let mut json_each_row = Vec::new();
    run_json_each_row_batch(sql.as_bytes(), &mut json_each_row)
        .expect("JSONEachRow batch succeeds");
    assert_eq!(
        String::from_utf8(json_each_row).unwrap(),
        "{\"text\":\"-0\"}\n{\"text\":\"1.25\"}\n{\"text\":\"10\"}\n"
    );

    let mut json_compact_each_row = Vec::new();
    run_json_compact_each_row_batch(sql.as_bytes(), &mut json_compact_each_row)
        .expect("JSONCompactEachRow batch succeeds");
    assert_eq!(
        String::from_utf8(json_compact_each_row).unwrap(),
        "[\"-0\"]\n[\"1.25\"]\n[\"10\"]\n"
    );
}
