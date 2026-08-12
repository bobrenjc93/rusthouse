use rusthouse::batch::engine::{Database, QueryResultLimits, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{ComparisonOperator, DeleteComparisonPredicate, Statement, parse};
use rusthouse::batch::storage::Column;
use rusthouse::batch::value::{DataType, Value};
use rusthouse::{
    DatabaseMetrics, Int64MinMaxIndexAdmission, Int64MinMaxIndexLimits, SharedDatabase,
    SharedDatabaseError,
};

fn ids(database: &Database, table: &str) -> Vec<i64> {
    let table = database.catalog().table(table).expect("table exists");
    let Column::Int64(values) = &table.columns()[0] else {
        panic!("the first column is Int64");
    };
    values.clone()
}

#[test]
fn parses_exact_comparison_delete_for_every_operator_and_literal_type() {
    let cases = [
        (
            "DELETE FROM events WHERE id = -7",
            Statement::Delete {
                table: "events".to_owned(),
                column: "id".to_owned(),
                literal: Value::Int64(-7),
            },
        ),
        (
            "delete from Events where score != +2.5e1;",
            Statement::DeleteComparison {
                table: "Events".to_owned(),
                column: "score".to_owned(),
                operator: ComparisonOperator::NotEqual,
                literal: Value::Float64(25.0),
            },
        ),
        (
            "DELETE FROM events WHERE active <> TRUE;",
            Statement::DeleteComparison {
                table: "events".to_owned(),
                column: "active".to_owned(),
                operator: ComparisonOperator::NotEqual,
                literal: Value::Bool(true),
            },
        ),
        (
            "DELETE FROM events WHERE label < 'it''s here';",
            Statement::DeleteComparison {
                table: "events".to_owned(),
                column: "label".to_owned(),
                operator: ComparisonOperator::Less,
                literal: Value::String("it's here".to_owned()),
            },
        ),
        (
            "DELETE FROM events WHERE id <= 7;",
            Statement::DeleteComparison {
                table: "events".to_owned(),
                column: "id".to_owned(),
                operator: ComparisonOperator::LessOrEqual,
                literal: Value::Int64(7),
            },
        ),
        (
            "DELETE FROM events WHERE score > 2.5;",
            Statement::DeleteComparison {
                table: "events".to_owned(),
                column: "score".to_owned(),
                operator: ComparisonOperator::Greater,
                literal: Value::Float64(2.5),
            },
        ),
        (
            "DELETE FROM events WHERE active >= false;",
            Statement::DeleteComparison {
                table: "events".to_owned(),
                column: "active".to_owned(),
                operator: ComparisonOperator::GreaterOrEqual,
                literal: Value::Bool(false),
            },
        ),
    ];

    for (sql, statement) in cases {
        assert_eq!(parse(sql).expect("valid comparison DELETE"), [statement]);
    }
}

#[test]
fn parses_exactly_two_delete_comparisons_joined_by_and() {
    assert_eq!(
        parse("DELETE FROM Events WHERE id >= 2 AND label <> 'skip';"),
        Ok(vec![Statement::DeleteConjunction {
            table: "Events".to_owned(),
            first: DeleteComparisonPredicate {
                column: "id".to_owned(),
                operator: ComparisonOperator::GreaterOrEqual,
                literal: Value::Int64(2),
            },
            second: DeleteComparisonPredicate {
                column: "label".to_owned(),
                operator: ComparisonOperator::NotEqual,
                literal: Value::String("skip".to_owned()),
            },
        }])
    );
}

#[test]
fn parses_exact_single_nullness_delete_for_both_spellings() {
    for (sql, table, is_null) in [
        ("DELETE FROM readings WHERE value IS NULL", "readings", true),
        (
            "delete from Readings where Value is not null;",
            "Readings",
            false,
        ),
        (
            "ALTER TABLE readings DELETE WHERE value IS NULL;",
            "readings",
            true,
        ),
        (
            "alter table Readings delete where Value is not null",
            "Readings",
            false,
        ),
    ] {
        assert_eq!(
            parse(sql),
            Ok(vec![Statement::DeleteNullness {
                table: table.to_owned(),
                column: if table == "readings" {
                    "value".to_owned()
                } else {
                    "Value".to_owned()
                },
                is_null,
            }]),
            "{sql}"
        );
    }

    assert_eq!(
        parse("ALTER TABLE Events DELETE WHERE reading IS NOT NULL"),
        parse("DELETE FROM Events WHERE reading IS NOT NULL")
    );
}

#[test]
fn rejects_malformed_or_compound_delete_nullness_shapes() {
    for sql in [
        "DELETE FROM readings WHERE value IS",
        "DELETE FROM readings WHERE value IS NOT",
        "DELETE FROM readings WHERE value IS TRUE",
        "DELETE FROM readings WHERE value IS NOT FALSE",
        "DELETE FROM readings WHERE value IS NULL AND id = 1",
        "DELETE FROM readings WHERE value IS NOT NULL OR id = 1",
        "DELETE FROM readings WHERE id = 1 AND value IS NULL",
        "DELETE FROM readings WHERE (value IS NULL)",
        "DELETE FROM readings WHERE NOT value IS NULL",
        "DELETE FROM readings WHERE value IS NULL LIMIT 1",
        "ALTER TABLE readings DELETE WHERE value IS NULL AND value IS NOT NULL",
        "ALTER TABLE readings DELETE WHERE value IS NOT NULL ORDER BY value",
    ] {
        assert!(matches!(parse(sql), Err(Error::Sql { .. })), "{sql}");
    }
}

#[test]
fn alter_table_delete_lowers_to_the_existing_delete_statement_shapes() {
    for (alter, delete) in [
        (
            "ALTER TABLE Events DELETE WHERE id = -7;",
            "DELETE FROM Events WHERE id = -7;",
        ),
        (
            "alter table Events delete where score > +2.5e1;",
            "delete from Events where score > +2.5e1;",
        ),
        (
            "ALTER TABLE Events DELETE WHERE active = true AND label <> 'skip';",
            "DELETE FROM Events WHERE active = true AND label <> 'skip';",
        ),
    ] {
        assert_eq!(
            parse(alter),
            parse(delete),
            "both spellings must lower to one deletion AST: {alter}"
        );
    }
}

#[test]
fn rejects_every_non_exact_delete_shape() {
    for sql in [
        "DELETE events WHERE id = 1",
        "DELETE FROM events id = 1",
        "DELETE FROM events WHERE id",
        "DELETE FROM events WHERE id == 1",
        "DELETE FROM events WHERE id ! 1",
        "DELETE FROM events WHERE 1 < id",
        "DELETE FROM events WHERE id = other_id",
        "DELETE FROM events WHERE id = NULL",
        "DELETE FROM events WHERE id = 1 ORDER BY id",
        "DELETE FROM events WHERE id = 1 LIMIT 1",
    ] {
        assert!(matches!(parse(sql), Err(Error::Sql { .. })), "{sql}");
    }
}

#[test]
fn rejects_malformed_or_extra_delete_predicates() {
    for sql in [
        "DELETE FROM events WHERE id = 1 AND",
        "DELETE FROM events WHERE id = 1 AND active",
        "DELETE FROM events WHERE id = 1 AND active == true",
        "DELETE FROM events WHERE id = 1 AND 2 = id",
        "DELETE FROM events WHERE id = 1 AND active = other_column",
        "DELETE FROM events WHERE id = 1 AND active = NULL",
        "DELETE FROM events WHERE id = 1 AND AND active = true",
        "DELETE FROM events WHERE id = 1 OR active = true",
        "DELETE FROM events WHERE id = 1 AND active = true AND label = 'x'",
        "DELETE FROM events WHERE (id = 1) AND active = true",
    ] {
        assert!(matches!(parse(sql), Err(Error::Sql { .. })), "{sql}");
    }
}

#[test]
fn rejects_malformed_alter_table_delete_shapes() {
    for sql in [
        "ALTER TABLE events DELETE",
        "ALTER TABLE events DELETE id = 1",
        "ALTER TABLE events DELETE WHERE",
        "ALTER TABLE events DELETE WHERE id",
        "ALTER TABLE events DELETE WHERE 1 = id",
        "ALTER TABLE events DELETE WHERE id = NULL",
        "ALTER TABLE events DELETE WHERE id = 1 OR active = true",
        "ALTER TABLE events DELETE WHERE id = 1 AND active = true AND label = 'x'",
        "ALTER TABLE events DELETE WHERE id = 1 LIMIT 1",
    ] {
        assert!(matches!(parse(sql), Err(Error::Sql { .. })), "{sql}");
    }
}

#[test]
fn alter_table_delete_executes_equivalently_to_delete_from() {
    let setup = "CREATE TABLE Events (id Int64, active Bool, label String); \
                 INSERT INTO Events VALUES \
                    (1, true, 'keep'), (2, true, 'remove'), (3, false, 'remove');";
    let mut alter_database = Database::new();
    let mut delete_database = Database::new();
    alter_database.execute(setup).expect("setup succeeds");
    delete_database.execute(setup).expect("setup succeeds");

    let alter_result =
        alter_database.execute("ALTER TABLE events DELETE WHERE id >= 2 AND label = 'remove';");
    let delete_result =
        delete_database.execute("DELETE FROM events WHERE id >= 2 AND label = 'remove';");

    assert_eq!(alter_result, delete_result);
    assert_eq!(ids(&alter_database, "events"), [1]);
    let altered = alter_database
        .catalog()
        .table("events")
        .expect("altered table remains");
    let deleted = delete_database
        .catalog()
        .table("events")
        .expect("deleted table remains");
    assert_eq!(altered.schema(), deleted.schema());
    assert_eq!(altered.row_count(), deleted.row_count());
    assert_eq!(
        alter_database.execute("SELECT id, active, label FROM events;"),
        delete_database.execute("SELECT id, active, label FROM events;")
    );
}

#[test]
fn nullness_delete_handles_mixed_and_all_null_nullable_data_atomically() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE Readings (value Nullable(Int64)); \
             ALTER TABLE Readings ADD COLUMN id Int64; \
             INSERT INTO Readings VALUES (NULL, 1), (2, 2), (NULL, 3), (4, 4);",
        )
        .expect("setup succeeds");

    assert_eq!(
        database.execute("DELETE FROM readings WHERE VALUE IS NULL"),
        Ok(vec![StatementResult::Command {
            tag: "DELETE",
            affected_rows: 2,
        }])
    );
    assert_eq!(
        database
            .query("SELECT value, id FROM readings ORDER BY id")
            .unwrap()
            .rows,
        [
            vec![Value::Int64(2), Value::Int64(2)],
            vec![Value::Int64(4), Value::Int64(4)],
        ]
    );
    assert_eq!(
        database.metrics_snapshot(),
        Some(DatabaseMetrics {
            table_count: 1,
            column_count: 2,
            retained_row_count: 2,
            retained_value_bytes: 34,
        })
    );

    let mut all_null = Database::new();
    all_null
        .execute(
            "CREATE TABLE all_null (value Nullable(Int64)); \
             INSERT INTO all_null VALUES (NULL), (NULL), (NULL);",
        )
        .expect("all-NULL setup succeeds");
    assert_eq!(
        all_null.execute("ALTER TABLE all_null DELETE WHERE value IS NOT NULL"),
        Ok(vec![StatementResult::Command {
            tag: "DELETE",
            affected_rows: 0,
        }])
    );
    assert_eq!(
        all_null.execute("DELETE FROM all_null WHERE value IS NULL"),
        Ok(vec![StatementResult::Command {
            tag: "DELETE",
            affected_rows: 3,
        }])
    );
    assert_eq!(all_null.catalog().table("all_null").unwrap().row_count(), 0);
}

#[test]
fn nullness_delete_operates_on_a_directly_created_trailing_nullable_column() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE Readings (id Int64, value Nullable(Int64)); \
             INSERT INTO Readings VALUES (1, NULL), (2, 20), (3, NULL), (4, 40);",
        )
        .expect("mixed nullable setup succeeds");

    assert_eq!(
        database.execute("ALTER TABLE readings DELETE WHERE value IS NULL"),
        Ok(vec![StatementResult::Command {
            tag: "DELETE",
            affected_rows: 2,
        }])
    );
    assert_eq!(ids(&database, "readings"), [2, 4]);

    assert_eq!(
        database.execute("DELETE FROM readings WHERE value IS NOT NULL"),
        Ok(vec![StatementResult::Command {
            tag: "DELETE",
            affected_rows: 2,
        }])
    );
    assert!(ids(&database, "readings").is_empty());
}

#[test]
fn nullness_delete_has_typed_non_nullable_semantics() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE events (id Int64); INSERT INTO events VALUES (1), (2), (3);")
        .expect("setup succeeds");

    assert_eq!(
        database.execute("DELETE FROM events WHERE id IS NULL"),
        Ok(vec![StatementResult::Command {
            tag: "DELETE",
            affected_rows: 0,
        }])
    );
    assert_eq!(ids(&database, "events"), [1, 2, 3]);
    assert_eq!(
        database.execute("ALTER TABLE events DELETE WHERE id IS NOT NULL"),
        Ok(vec![StatementResult::Command {
            tag: "DELETE",
            affected_rows: 3,
        }])
    );
    assert!(ids(&database, "events").is_empty());
}

#[test]
fn nullness_delete_rebuilds_an_admitted_nullable_int64_index() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE readings (value Nullable(Int64)); \
             INSERT INTO readings VALUES (NULL), (1), (2), (NULL);",
        )
        .expect("setup succeeds");
    assert!(matches!(
        database
            .create_int64_min_max_index(
                "readings",
                "value",
                Int64MinMaxIndexLimits::new(2, 2, usize::MAX),
            )
            .unwrap(),
        Int64MinMaxIndexAdmission::Created(_)
    ));

    database
        .execute("DELETE FROM readings WHERE value IS NULL")
        .expect("delete succeeds");
    let index = database
        .catalog()
        .table("readings")
        .unwrap()
        .int64_min_max_index_info()
        .expect("the index remains admitted and is rebuilt");
    assert_eq!(index.indexed_rows, 2);
    let results = database
        .execute("SELECT value FROM readings WHERE value >= 1 ORDER BY value")
        .unwrap();
    let [StatementResult::Query(result)] = results.as_slice() else {
        panic!("expected one query result")
    };
    assert_eq!(result.rows, [[Value::Int64(1)], [Value::Int64(2)]]);
}

#[test]
fn executes_every_comparison_operator_across_every_physical_type() {
    let physical_types = [
        ("Int64", "(1, 1), (2, 2), (3, 3)", "2", false),
        ("Float64", "(1, 1.5), (2, 2.5), (3, 3.5)", "2.5", false),
        ("Bool", "(1, false), (2, false), (3, true)", "false", true),
        (
            "String",
            "(1, 'alpha'), (2, 'middle'), (3, 'zulu')",
            "'middle'",
            false,
        ),
    ];

    for (data_type, rows, literal, is_bool) in physical_types {
        let comparisons: [(&str, &[i64]); 7] = if is_bool {
            [
                ("=", &[3]),
                ("!=", &[1, 2]),
                ("<>", &[1, 2]),
                ("<", &[1, 2, 3]),
                ("<=", &[3]),
                (">", &[1, 2]),
                (">=", &[]),
            ]
        } else {
            [
                ("=", &[1, 3]),
                ("!=", &[2]),
                ("<>", &[2]),
                ("<", &[2, 3]),
                ("<=", &[3]),
                (">", &[1, 2]),
                (">=", &[1]),
            ]
        };

        for (operator, remaining_ids) in comparisons {
            let mut database = Database::new();
            database
                .execute(&format!(
                    "CREATE TABLE Events (id Int64, target {data_type}); \
                     INSERT INTO Events VALUES {rows};"
                ))
                .expect("setup succeeds");

            let affected_rows = 3 - remaining_ids.len();
            let sql = format!("DELETE FROM events WHERE target {operator} {literal}");
            assert_eq!(
                database.execute(&sql),
                Ok(vec![StatementResult::Command {
                    tag: "DELETE",
                    affected_rows,
                }]),
                "{data_type} {operator}"
            );
            assert_eq!(
                ids(&database, "events"),
                remaining_ids,
                "{data_type} {operator}"
            );
        }
    }
}

#[test]
fn conjunction_deletes_only_rows_matching_mixed_typed_columns() {
    let cases = [
        ("id >= 2 AND score < 4.0", &[1, 4][..], "Int64 and Float64"),
        (
            "active = true AND label != 'alpha'",
            &[1, 2, 4][..],
            "Bool and String",
        ),
    ];

    for (predicate, remaining_ids, description) in cases {
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE Events (id Int64, score Float64, active Bool, label String); \
                 INSERT INTO Events VALUES \
                    (1, 1.5, true, 'alpha'), \
                    (2, 2.5, false, 'beta'), \
                    (3, 3.5, true, 'gamma'), \
                    (4, 4.5, false, 'zulu');",
            )
            .expect("setup succeeds");

        let affected_rows = 4 - remaining_ids.len();
        assert_eq!(
            database.execute(&format!("DELETE FROM events WHERE {predicate}")),
            Ok(vec![StatementResult::Command {
                tag: "DELETE",
                affected_rows,
            }]),
            "{description}"
        );
        assert_eq!(ids(&database, "events"), remaining_ids, "{description}");
    }
}

#[test]
fn comparison_delete_compacts_every_physical_column_together() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE Events (id Int64, score Float64, active Bool, label String); \
             INSERT INTO Events VALUES \
                 (1, 1.5, true, 'one'), \
                 (2, 2.5, false, 'two'), \
                 (3, 3.5, true, 'three');",
        )
        .expect("setup succeeds");

    assert_eq!(
        database.execute("DELETE FROM events WHERE score >= 2.5"),
        Ok(vec![StatementResult::Command {
            tag: "DELETE",
            affected_rows: 2,
        }])
    );
    assert_eq!(ids(&database, "events"), [1]);

    let table = database.catalog().table("events").expect("table remains");
    assert!(matches!(&table.columns()[1], Column::Float64(values) if values == &[1.5]));
    assert!(matches!(&table.columns()[2], Column::Bool(values) if values == &[true]));
    assert!(matches!(&table.columns()[3], Column::String(values) if values == &["one"]));
}

#[test]
fn zero_and_all_matches_report_counts_and_preserve_the_table() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE events (id Int64, active Bool); \
             INSERT INTO events VALUES (1, true), (2, true), (3, true);",
        )
        .expect("setup succeeds");

    assert_eq!(
        database.execute("DELETE FROM events WHERE id < 1;"),
        Ok(vec![StatementResult::Command {
            tag: "DELETE",
            affected_rows: 0,
        }])
    );
    assert_eq!(ids(&database, "events"), [1, 2, 3]);

    assert_eq!(
        database.execute("DELETE FROM events WHERE active >= true;"),
        Ok(vec![StatementResult::Command {
            tag: "DELETE",
            affected_rows: 3,
        }])
    );
    let table = database.catalog().table("events").expect("table remains");
    assert_eq!(table.row_count(), 0);
    assert!(table.columns().iter().all(Column::is_empty));
    assert_eq!(table.schema()[0].data_type, DataType::Int64);
    assert_eq!(table.schema()[1].data_type, DataType::Bool);
}

#[test]
fn invalid_types_and_scan_limit_errors_never_delete_rows() {
    let limits = QueryResultLimits {
        max_scan_rows: 2,
        ..QueryResultLimits::default()
    };
    let mut database = Database::with_query_result_limits(limits);
    database
        .execute(
            "CREATE TABLE Events (id Int64, score Float64, active Bool, label String); \
             INSERT INTO Events VALUES \
                (1, 1.5, true, 'one'), \
                (2, 2.5, false, 'two'), \
                (3, 3.5, true, 'three');",
        )
        .expect("setup succeeds");

    assert_eq!(
        database.execute("DELETE FROM missing WHERE id != 1"),
        Err(Error::TableNotFound("missing".to_owned()))
    );
    assert_eq!(
        database.execute("DELETE FROM events WHERE absent < 1"),
        Err(Error::ColumnNotFound {
            table: "Events".to_owned(),
            column: "absent".to_owned(),
        })
    );

    for (sql, expected, actual) in [
        ("DELETE FROM events WHERE id != true", "Int64", "Bool"),
        (
            "DELETE FROM events WHERE score < 'two'",
            "Float64",
            "String",
        ),
        ("DELETE FROM events WHERE active > 1", "Bool", "Int64"),
        ("DELETE FROM events WHERE label <= false", "String", "Bool"),
    ] {
        assert_eq!(
            database.execute(sql),
            Err(Error::TypeMismatch {
                context: "WHERE comparison".to_owned(),
                expected: expected.to_owned(),
                actual: actual.to_owned(),
            }),
            "{sql}"
        );
        assert_eq!(ids(&database, "events"), [1, 2, 3], "{sql}");
    }

    assert_eq!(
        database.execute("DELETE FROM events WHERE id >= 2"),
        Err(Error::ResourceLimitExceeded {
            resource: "DELETE scanned rows",
            actual: 3,
            max: 2,
        })
    );
    assert_eq!(ids(&database, "events"), [1, 2, 3]);

    assert_eq!(
        database.execute_statement(Statement::DeleteComparison {
            table: "events".to_owned(),
            column: "id".to_owned(),
            operator: ComparisonOperator::LessOrEqual,
            literal: Value::Float64(f64::NAN),
        }),
        Err(Error::InvalidQuery(
            "WHERE comparison Float64 literals must be finite".to_owned()
        ))
    );
    assert_eq!(ids(&database, "events"), [1, 2, 3]);
}

#[test]
fn nullness_delete_missing_column_and_scan_limit_failures_leave_rows_unchanged() {
    let mut database = Database::with_query_result_limits(QueryResultLimits {
        max_scan_rows: 2,
        ..QueryResultLimits::default()
    });
    database
        .execute(
            "CREATE TABLE Readings (value Nullable(Int64)); \
             INSERT INTO Readings VALUES (NULL), (7), (NULL);",
        )
        .expect("setup succeeds");

    assert_eq!(
        database.execute("DELETE FROM readings WHERE missing IS NULL"),
        Err(Error::ColumnNotFound {
            table: "Readings".to_owned(),
            column: "missing".to_owned(),
        })
    );
    assert_eq!(
        database.execute("ALTER TABLE readings DELETE WHERE value IS NULL"),
        Err(Error::ResourceLimitExceeded {
            resource: "DELETE scanned rows",
            actual: 3,
            max: 2,
        })
    );
    let table = database.catalog().table("readings").unwrap();
    assert_eq!(table.row_count(), 3);
    assert!(matches!(
        &table.columns()[0],
        Column::NullableInt64(values) if values == &[None, Some(7), None]
    ));
}

#[test]
fn conjunction_validation_and_scan_failures_never_delete_rows() {
    let limits = QueryResultLimits {
        max_scan_rows: 2,
        ..QueryResultLimits::default()
    };
    let mut database = Database::with_query_result_limits(limits);
    database
        .execute(
            "CREATE TABLE events (id Int64, score Float64, active Bool); \
             INSERT INTO events VALUES \
                (1, 1.5, true), (2, 2.5, false), (3, 3.5, true);",
        )
        .expect("setup succeeds");

    let failures = [
        (
            "DELETE FROM events WHERE id = true AND active = true",
            Error::TypeMismatch {
                context: "WHERE comparison".to_owned(),
                expected: "Int64".to_owned(),
                actual: "Bool".to_owned(),
            },
        ),
        (
            "DELETE FROM events WHERE id >= 2 AND absent = true",
            Error::ColumnNotFound {
                table: "events".to_owned(),
                column: "absent".to_owned(),
            },
        ),
        (
            "DELETE FROM events WHERE id >= 2 AND active = 1",
            Error::TypeMismatch {
                context: "WHERE comparison".to_owned(),
                expected: "Bool".to_owned(),
                actual: "Int64".to_owned(),
            },
        ),
        (
            "DELETE FROM events WHERE id >= 2 AND active = true",
            Error::ResourceLimitExceeded {
                resource: "DELETE scanned rows",
                actual: 3,
                max: 2,
            },
        ),
    ];

    for (sql, error) in failures {
        assert_eq!(database.execute(sql), Err(error), "{sql}");
        assert_eq!(ids(&database, "events"), [1, 2, 3], "{sql}");
    }

    assert_eq!(
        database.execute_statement(Statement::DeleteConjunction {
            table: "events".to_owned(),
            first: DeleteComparisonPredicate {
                column: "id".to_owned(),
                operator: ComparisonOperator::GreaterOrEqual,
                literal: Value::Int64(2),
            },
            second: DeleteComparisonPredicate {
                column: "score".to_owned(),
                operator: ComparisonOperator::Less,
                literal: Value::Float64(f64::NAN),
            },
        }),
        Err(Error::InvalidQuery(
            "WHERE comparison Float64 literals must be finite".to_owned()
        ))
    );
    assert_eq!(ids(&database, "events"), [1, 2, 3]);

    assert!(matches!(
        database.execute("DELETE FROM events WHERE id >= 2 AND active = true AND score > 0.0"),
        Err(Error::Sql { .. })
    ));
    assert_eq!(ids(&database, "events"), [1, 2, 3]);
}

#[test]
fn alter_table_delete_scan_limit_failure_leaves_every_row_unchanged() {
    let limits = QueryResultLimits {
        max_scan_rows: 2,
        ..QueryResultLimits::default()
    };
    let mut database = Database::with_query_result_limits(limits);
    database
        .execute(
            "CREATE TABLE events (id Int64, label String); \
             INSERT INTO events VALUES (1, 'keep'), (2, 'remove'), (3, 'remove');",
        )
        .expect("setup succeeds");

    assert_eq!(
        database.execute("ALTER TABLE events DELETE WHERE id >= 2 AND label = 'remove';"),
        Err(Error::ResourceLimitExceeded {
            resource: "DELETE scanned rows",
            actual: 3,
            max: 2,
        })
    );
    assert_eq!(ids(&database, "events"), [1, 2, 3]);
    let table = database.catalog().table("events").expect("table remains");
    assert!(matches!(
        &table.columns()[1],
        Column::String(values) if values == &["keep", "remove", "remove"]
    ));
}

#[test]
fn original_public_equality_delete_ast_shape_remains_directly_executable() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE events (id Int64); INSERT INTO events VALUES (1), (2), (2);")
        .expect("setup succeeds");

    assert_eq!(
        database.execute_statement(Statement::Delete {
            table: "events".to_owned(),
            column: "id".to_owned(),
            literal: Value::Int64(2),
        }),
        Ok(StatementResult::Command {
            tag: "DELETE",
            affected_rows: 2,
        })
    );
    assert_eq!(ids(&database, "events"), [1]);
}

#[test]
fn insert_only_execution_rejects_delete_without_mutation() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE events (id Int64); INSERT INTO events VALUES (1), (2);")
        .expect("setup succeeds");

    for sql in [
        "DELETE FROM events WHERE id = 1",
        "DELETE FROM events WHERE id >= 1",
        "DELETE FROM events WHERE id >= 1 AND id < 2",
        "DELETE FROM events WHERE id IS NULL",
        "ALTER TABLE events DELETE WHERE id IS NOT NULL",
    ] {
        assert_eq!(
            database.execute_insert_batch(sql),
            Err(Error::InsertOnlyStatementRequired {
                statement: "DELETE",
            })
        );
        assert_eq!(ids(&database, "events"), [1, 2]);
    }
}

#[test]
fn shared_database_executes_delete_under_its_write_lock() {
    let database = SharedDatabase::default();
    let deleting_handle = database.clone();
    database
        .execute(
            "CREATE TABLE events (id Int64, label String); \
             INSERT INTO events VALUES (1, 'keep'), (2, 'remove'), (3, 'remove');",
        )
        .expect("setup succeeds");

    assert_eq!(
        deleting_handle.execute("DELETE FROM EVENTS WHERE id >= 2 AND label <> 'keep';"),
        Ok(vec![StatementResult::Command {
            tag: "DELETE",
            affected_rows: 2,
        }])
    );
    assert_eq!(
        database.metrics_snapshot(),
        Some(DatabaseMetrics {
            table_count: 1,
            column_count: 2,
            retained_row_count: 1,
            retained_value_bytes: 12,
        })
    );
    assert_eq!(
        database
            .query("SELECT id, label FROM events;")
            .unwrap()
            .rows,
        [vec![Value::Int64(1), Value::String("keep".to_owned())]]
    );
    assert_eq!(
        database.query("DELETE FROM events WHERE id >= 1 AND label = 'keep'"),
        Err(SharedDatabaseError::ReadOnlyStatementRequired {
            statement: "DELETE",
        })
    );
}
