use rusthouse::batch::engine::{
    DEFAULT_MAX_QUERY_ORDERING_STATE_BYTES, Database, LENGTH_UTF8_ORDERING_CACHE_ENTRY_BYTES,
    QueryResult, QueryResultLimits, ResultColumn, StatementResult,
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
fn parses_length_utf8_as_a_bounded_select_item_with_an_optional_alias() {
    let statements = parse(
        "SELECT lengthUTF8(label), LENGTHUTF8(label) AS characters FROM samples \
         WHERE label != 'skip' ORDER BY LeNgThUtF8(label) DESC LIMIT 2 OFFSET 1",
    )
    .expect("valid lengthUTF8 projections");
    let Statement::Select(select) = &statements[0] else {
        panic!("expected SELECT");
    };

    assert_eq!(
        select.items,
        [
            SelectItem::LengthUtf8 {
                name: "label".to_owned(),
                alias: None,
            },
            SelectItem::LengthUtf8 {
                name: "label".to_owned(),
                alias: Some("characters".to_owned()),
            },
        ]
    );
    assert!(select.predicate.is_some());
    assert_eq!(select.order_by[0].name, "lengthUTF8(label)");
    assert!(select.order_by[0].descending);
    assert_eq!(select.limit, Some(2));
    assert_eq!(select.offset, Some(1));

    let limits = BatchSqlLimits {
        max_ast_list_items: 1,
        ..BatchSqlLimits::default()
    };
    parse_with_limits("SELECT lengthUTF8(label) FROM samples", limits)
        .expect("one lengthUTF8 item fits the limit");
    assert_eq!(
        parse_with_limits("SELECT lengthUTF8(label), label FROM samples", limits),
        Err(Error::ResourceLimitExceeded {
            resource: "SQL AST list items",
            actual: 2,
            max: 1,
        })
    );
}

#[test]
fn counts_ascii_and_unicode_scalars_and_preserves_projection_order() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (label String); \
             INSERT INTO samples VALUES \
             (''), ('ASCII'), ('é'), ('東京'), ('é'), ('👨‍👩‍👧‍👦');",
        )
        .expect("setup");

    let result = query(
        &mut database,
        "SELECT label, LENGTH(label) AS bytes, lengthUTF8(label) AS scalars FROM samples",
    );
    assert_eq!(
        result.columns,
        [
            ResultColumn {
                name: "label".to_owned(),
                data_type: DataType::String,
            },
            ResultColumn {
                name: "bytes".to_owned(),
                data_type: DataType::Int64,
            },
            ResultColumn {
                name: "scalars".to_owned(),
                data_type: DataType::Int64,
            },
        ]
    );
    assert_eq!(
        result.rows,
        [
            vec![
                Value::String("".to_owned()),
                Value::Int64(0),
                Value::Int64(0)
            ],
            vec![
                Value::String("ASCII".to_owned()),
                Value::Int64(5),
                Value::Int64(5),
            ],
            vec![
                Value::String("é".to_owned()),
                Value::Int64(2),
                Value::Int64(1)
            ],
            vec![
                Value::String("東京".to_owned()),
                Value::Int64(6),
                Value::Int64(2),
            ],
            vec![
                Value::String("é".to_owned()),
                Value::Int64(3),
                Value::Int64(2),
            ],
            vec![
                Value::String("👨‍👩‍👧‍👦".to_owned()),
                Value::Int64(25),
                Value::Int64(7),
            ],
        ]
    );
}

#[test]
fn filters_orders_and_pages_by_expression_or_alias() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (label String, keep Bool); \
             INSERT INTO samples VALUES \
             ('', true), ('é', true), ('東京', true), ('é', true), \
             ('abc', true), ('👨‍👩‍👧‍👦', true), ('discard', false);",
        )
        .expect("setup");

    let expression_ordered = query(
        &mut database,
        "SELECT lengthUTF8(label) FROM samples \
         WHERE keep = true ORDER BY lengthUTF8(label) DESC LIMIT 2 OFFSET 1",
    );
    assert_eq!(
        expression_ordered.columns,
        [ResultColumn {
            name: "lengthUTF8(label)".to_owned(),
            data_type: DataType::Int64,
        }]
    );
    assert_eq!(
        expression_ordered.rows,
        [vec![Value::Int64(3)], vec![Value::Int64(2)]]
    );

    let alias_ordered = query(
        &mut database,
        "SELECT lengthUTF8(label) AS characters FROM samples \
         WHERE keep = true ORDER BY characters LIMIT 2 OFFSET 1",
    );
    assert_eq!(alias_ordered.columns[0].name, "characters");
    assert_eq!(
        alias_ordered.rows,
        [vec![Value::Int64(1)], vec![Value::Int64(2)]]
    );
}

#[test]
fn cached_order_matches_full_stable_sort_for_unicode_ties_and_pagination_boundaries() {
    let source_rows = [
        ("discarded", false),
        ("é", true),
        ("東京", true),
        ("é", true),
        ("🦀", true),
        ("", true),
        ("👨‍👩‍👧‍👦", true),
        ("abc", true),
        ("🙂🙂", true),
        ("Z", true),
        ("also discarded", false),
    ];
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (label String, keep Bool); \
             INSERT INTO samples VALUES \
             ('discarded', false), ('é', true), ('東京', true), ('é', true), \
             ('🦀', true), ('', true), ('👨‍👩‍👧‍👦', true), ('abc', true), \
             ('🙂🙂', true), ('Z', true), ('also discarded', false);",
        )
        .expect("setup");

    let matching_count = source_rows.iter().filter(|(_, keep)| *keep).count();
    let pages = [
        (0, 0),
        (1, 0),
        (2, 0),
        (2, 1),
        (3, 2),
        (1, matching_count - 1),
        (2, matching_count),
        (matching_count, 0),
        (matching_count + 2, 1),
    ];

    for (direction, descending) in [("ASC", false), ("DESC", true)] {
        let mut full_order = source_rows
            .iter()
            .filter(|(_, keep)| *keep)
            .map(|(label, _)| *label)
            .collect::<Vec<_>>();
        full_order.sort_by(|left, right| {
            let comparison = left.chars().count().cmp(&right.chars().count());
            if descending {
                comparison.reverse()
            } else {
                comparison
            }
        });

        for (limit, offset) in pages {
            let expected = full_order
                .iter()
                .skip(offset)
                .take(limit)
                .map(|label| {
                    vec![
                        Value::String((*label).to_owned()),
                        Value::Int64(i64::try_from(label.chars().count()).unwrap()),
                    ]
                })
                .collect::<Vec<_>>();
            let actual = query(
                &mut database,
                &format!(
                    "SELECT label, lengthUTF8(label) AS scalars FROM samples \
                     WHERE keep = true ORDER BY scalars {direction} \
                     LIMIT {limit} OFFSET {offset}"
                ),
            );

            assert_eq!(
                actual.rows, expected,
                "{direction} LIMIT {limit} OFFSET {offset}"
            );
        }
    }
}

#[test]
fn length_utf8_ordering_cache_enforces_exact_filtered_byte_boundaries() {
    let setup = "CREATE TABLE samples (label String, keep Bool); \
                 INSERT INTO samples VALUES \
                 ('é', true), ('discarded', false), ('Z', true), ('東京', true);";
    let filtered_cache_bytes = 3 * LENGTH_UTF8_ORDERING_CACHE_ENTRY_BYTES;
    assert_eq!(
        QueryResultLimits::default().max_ordering_state_bytes,
        DEFAULT_MAX_QUERY_ORDERING_STATE_BYTES
    );

    let limits = QueryResultLimits {
        max_ordering_state_bytes: filtered_cache_bytes,
        ..QueryResultLimits::default()
    };
    let mut exact = Database::with_query_result_limits(limits);
    exact.execute(setup).expect("setup");

    assert_eq!(
        query(
            &mut exact,
            "SELECT label, lengthUTF8(label) AS scalars FROM samples \
             WHERE keep = true ORDER BY scalars ASC LIMIT 2 OFFSET 1"
        )
        .rows,
        [
            vec![Value::String("Z".to_owned()), Value::Int64(1)],
            vec![Value::String("東京".to_owned()), Value::Int64(2)],
        ]
    );
    assert_eq!(
        query(
            &mut exact,
            "SELECT label, lengthUTF8(label) AS scalars FROM samples \
             WHERE keep = true ORDER BY scalars DESC LIMIT 2 OFFSET 1"
        )
        .rows,
        [
            vec![Value::String("é".to_owned()), Value::Int64(1)],
            vec![Value::String("Z".to_owned()), Value::Int64(1)],
        ]
    );

    assert_eq!(
        exact.execute("SELECT lengthUTF8(label) AS scalars FROM samples ORDER BY scalars LIMIT 1"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT ordering state bytes",
            actual: 4 * LENGTH_UTF8_ORDERING_CACHE_ENTRY_BYTES,
            max: filtered_cache_bytes,
        })
    );

    let mut one_byte_short = Database::with_query_result_limits(QueryResultLimits {
        max_ordering_state_bytes: filtered_cache_bytes - 1,
        ..QueryResultLimits::default()
    });
    one_byte_short.execute(setup).expect("setup");
    assert_eq!(
        one_byte_short.execute(
            "SELECT lengthUTF8(label) AS scalars FROM samples \
             WHERE keep = true ORDER BY scalars LIMIT 1 OFFSET 1"
        ),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT ordering state bytes",
            actual: filtered_cache_bytes,
            max: filtered_cache_bytes - 1,
        })
    );
}

#[test]
fn rejects_unknown_non_string_and_grouped_length_utf8_inputs_with_typed_errors() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (i Int64, f Float64, b Bool, s String); \
             INSERT INTO samples VALUES (1, 1.5, true, 'one');",
        )
        .expect("setup");

    assert_eq!(
        database.execute("SELECT lengthUTF8(missing) FROM samples"),
        Err(Error::ColumnNotFound {
            table: "samples".to_owned(),
            column: "missing".to_owned(),
        })
    );

    for (name, actual) in [
        ("i", DataType::Int64),
        ("f", DataType::Float64),
        ("b", DataType::Bool),
    ] {
        assert_eq!(
            database.execute(&format!("SELECT lengthUTF8({name}) FROM samples")),
            Err(Error::TypeMismatch {
                context: format!("lengthUTF8 argument '{name}'"),
                expected: "String".to_owned(),
                actual: actual.to_string(),
            }),
            "column {name}"
        );
    }

    assert_eq!(
        database.execute("SELECT lengthUTF8(s), COUNT(*) FROM samples GROUP BY s"),
        Err(Error::InvalidQuery(
            "lengthUTF8 projections are only supported in ungrouped SELECT queries".to_owned()
        ))
    );
}

#[test]
fn rejects_malformed_length_utf8_syntax() {
    for sql in [
        "SELECT lengthUTF8() FROM samples",
        "SELECT lengthUTF8(*) FROM samples",
        "SELECT lengthUTF8('text') FROM samples",
        "SELECT lengthUTF8(label, label) FROM samples",
        "SELECT lengthUTF8(label FROM samples",
        "SELECT lengthUTF8(label) characters FROM samples",
        "SELECT lengthUTF8(lengthUTF8(label)) FROM samples",
        "SELECT lengthUTF8(label) FROM samples ORDER BY lengthUTF8()",
        "SELECT lengthUTF8(label) FROM samples ORDER BY lengthUTF8(*)",
        "SELECT lengthUTF8(label) FROM samples ORDER BY lengthUTF8(label",
    ] {
        assert!(parse(sql).is_err(), "{sql:?} must be rejected");
    }
}

#[test]
fn length_utf8_obeys_selected_result_caps_without_charging_source_bytes() {
    let setup = "CREATE TABLE samples (label String); \
                 INSERT INTO samples VALUES ('one'), ('東京'), ('👨‍👩‍👧‍👦');";

    let mut selected_only = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 1,
        max_values: 1,
        max_bytes: usize::MAX,
        ..QueryResultLimits::default()
    });
    selected_only.execute(setup).expect("setup");
    assert_eq!(
        query(
            &mut selected_only,
            "SELECT lengthUTF8(label) FROM samples LIMIT 1 OFFSET 1"
        )
        .rows,
        [vec![Value::Int64(2)]]
    );
    assert_eq!(
        selected_only.execute("SELECT lengthUTF8(label) FROM samples"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result rows",
            actual: 3,
            max: 1,
        })
    );

    let result_name = "characters";
    let fixed_bytes = std::mem::size_of::<ResultColumn>()
        + result_name.len()
        + std::mem::size_of::<Vec<Value>>()
        + std::mem::size_of::<Value>();
    let sql = "SELECT lengthUTF8(label) AS characters FROM samples LIMIT 1 OFFSET 2";

    let mut exact = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 1,
        max_values: 1,
        max_bytes: fixed_bytes,
        ..QueryResultLimits::default()
    });
    exact.execute(setup).expect("setup");
    assert_eq!(query(&mut exact, sql).rows, [vec![Value::Int64(7)]]);

    let mut byte_limited = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 1,
        max_values: 1,
        max_bytes: fixed_bytes - 1,
        ..QueryResultLimits::default()
    });
    byte_limited.execute(setup).expect("setup");
    assert_eq!(
        byte_limited.execute(sql),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT result bytes",
            actual: fixed_bytes,
            max: fixed_bytes - 1,
        })
    );
}

#[test]
fn emits_length_utf8_as_int64_in_all_batch_output_formats() {
    let sql = "CREATE TABLE samples (label String); \
               INSERT INTO samples VALUES ('東京'), (''), ('é'); \
               SELECT lengthUTF8(label) AS characters FROM samples ORDER BY characters;";

    let mut table = Vec::new();
    run_table_batch(sql.as_bytes(), &mut table).expect("table batch succeeds");
    assert_eq!(
        String::from_utf8(table).unwrap(),
        "+------------+\n\
         | characters |\n\
         +------------+\n\
         | 0          |\n\
         | 1          |\n\
         | 2          |\n\
         +------------+\n"
    );

    let mut csv = Vec::new();
    run_csv_batch(sql.as_bytes(), &mut csv).expect("CSV batch succeeds");
    assert_eq!(String::from_utf8(csv).unwrap(), "characters\n0\n1\n2\n");

    let mut tsv = Vec::new();
    run_tsv_batch(sql.as_bytes(), &mut tsv).expect("TSV batch succeeds");
    assert_eq!(String::from_utf8(tsv).unwrap(), "characters\n0\n1\n2\n");

    let mut json = Vec::new();
    run_json_batch(sql.as_bytes(), &mut json).expect("JSON batch succeeds");
    assert_eq!(
        String::from_utf8(json).unwrap(),
        "{\"columns\":[{\"name\":\"characters\",\"type\":\"Int64\"}],\"rows\":[[0],[1],[2]]}\n"
    );

    let mut json_each_row = Vec::new();
    run_json_each_row_batch(sql.as_bytes(), &mut json_each_row)
        .expect("JSONEachRow batch succeeds");
    assert_eq!(
        String::from_utf8(json_each_row).unwrap(),
        "{\"characters\":0}\n{\"characters\":1}\n{\"characters\":2}\n"
    );

    let mut json_compact_each_row = Vec::new();
    run_json_compact_each_row_batch(sql.as_bytes(), &mut json_compact_each_row)
        .expect("JSONCompactEachRow batch succeeds");
    assert_eq!(
        String::from_utf8(json_compact_each_row).unwrap(),
        "[0]\n[1]\n[2]\n"
    );
}
