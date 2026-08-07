use rusthouse::batch::engine::{Database, QueryResultLimits, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{ComparisonOperator, Statement, parse};
use rusthouse::batch::storage::Column;
use rusthouse::batch::value::{DataType, Value};
use rusthouse::{DatabaseMetrics, SharedDatabase, SharedDatabaseError};

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
            "events",
            "id",
            ComparisonOperator::Equal,
            Value::Int64(-7),
        ),
        (
            "delete from Events where score != +2.5e1;",
            "Events",
            "score",
            ComparisonOperator::NotEqual,
            Value::Float64(25.0),
        ),
        (
            "DELETE FROM events WHERE active <> TRUE;",
            "events",
            "active",
            ComparisonOperator::NotEqual,
            Value::Bool(true),
        ),
        (
            "DELETE FROM events WHERE label < 'it''s here';",
            "events",
            "label",
            ComparisonOperator::Less,
            Value::String("it's here".to_owned()),
        ),
        (
            "DELETE FROM events WHERE id <= 7;",
            "events",
            "id",
            ComparisonOperator::LessOrEqual,
            Value::Int64(7),
        ),
        (
            "DELETE FROM events WHERE score > 2.5;",
            "events",
            "score",
            ComparisonOperator::Greater,
            Value::Float64(2.5),
        ),
        (
            "DELETE FROM events WHERE active >= false;",
            "events",
            "active",
            ComparisonOperator::GreaterOrEqual,
            Value::Bool(false),
        ),
    ];

    for (sql, table, column, operator, literal) in cases {
        assert_eq!(
            parse(sql).expect("valid comparison DELETE"),
            [Statement::Delete {
                table: table.to_owned(),
                column: column.to_owned(),
                operator,
                literal,
            }]
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
        "DELETE FROM events WHERE id = 1 AND active = true",
        "DELETE FROM events WHERE id = 1 ORDER BY id",
        "DELETE FROM events WHERE id = 1 LIMIT 1",
    ] {
        assert!(matches!(parse(sql), Err(Error::Sql { .. })), "{sql}");
    }
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
        database.execute_statement(Statement::Delete {
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
fn insert_only_execution_rejects_delete_without_mutation() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE events (id Int64); INSERT INTO events VALUES (1), (2);")
        .expect("setup succeeds");

    assert_eq!(
        database.execute_insert_batch("DELETE FROM events WHERE id = 1"),
        Err(Error::InsertOnlyStatementRequired {
            statement: "DELETE",
        })
    );
    assert_eq!(ids(&database, "events"), [1, 2]);
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
        deleting_handle.execute("DELETE FROM EVENTS WHERE label <> 'keep';"),
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
        database.query("DELETE FROM events WHERE id >= 1"),
        Err(SharedDatabaseError::ReadOnlyStatementRequired {
            statement: "DELETE",
        })
    );
}
