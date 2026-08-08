use std::fmt::Write as _;

use rusthouse::batch::engine::{DEFAULT_MAX_CELLS_PER_TABLE, Database, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::Statement;
use rusthouse::batch::value::Value;

fn query_rows(database: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    let results = database.execute(sql).expect("query succeeds");
    let StatementResult::Query(result) = results.into_iter().next().expect("one result") else {
        panic!("expected query result");
    };
    result.rows
}

#[test]
fn original_public_positional_insert_ast_shape_remains_executable() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE events (id Int64, label String);")
        .expect("create target");

    assert_eq!(
        database.execute_statement(Statement::Insert {
            table: "events".to_owned(),
            rows: vec![vec![Value::Int64(7), Value::String("seven".to_owned()),]],
        }),
        Ok(StatementResult::Command {
            tag: "INSERT",
            affected_rows: 1,
        })
    );
    assert_eq!(
        query_rows(&mut database, "SELECT id, label FROM events;"),
        [vec![Value::Int64(7), Value::String("seven".to_owned()),]]
    );
}

#[test]
fn complete_insert_columns_reorder_every_type_and_preserve_positional_insert() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE metrics (id Int64, score Float64, active Bool, label String); \
             INSERT INTO metrics (LABEL, ACTIVE, Score, ID) VALUES \
                 ('first', true, 1.5, 1), \
                 ('second', false, 2.5, 2); \
             INSERT INTO metrics VALUES (3, 3.5, true, 'third');",
        )
        .expect("named and positional INSERT forms succeed");

    assert_eq!(
        query_rows(
            &mut database,
            "SELECT id, score, active, label FROM metrics ORDER BY id;",
        ),
        [
            vec![
                Value::Int64(1),
                Value::Float64(1.5),
                Value::Bool(true),
                Value::String("first".to_owned()),
            ],
            vec![
                Value::Int64(2),
                Value::Float64(2.5),
                Value::Bool(false),
                Value::String("second".to_owned()),
            ],
            vec![
                Value::Int64(3),
                Value::Float64(3.5),
                Value::Bool(true),
                Value::String("third".to_owned()),
            ],
        ]
    );
}

#[test]
fn explicit_insert_column_subsets_fill_every_typed_default_in_schema_order() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE metrics (id Int64, score Float64, active Bool, label String); \
             INSERT INTO metrics (LABEL, ID) VALUES ('first', 1); \
             INSERT INTO metrics (active, score) VALUES (true, 2.5);",
        )
        .expect("reordered subsets receive typed defaults");

    assert_eq!(
        query_rows(
            &mut database,
            "SELECT id, score, active, label FROM metrics ORDER BY id;",
        ),
        [
            vec![
                Value::Int64(0),
                Value::Float64(2.5),
                Value::Bool(true),
                Value::String(String::new()),
            ],
            vec![
                Value::Int64(1),
                Value::Float64(0.0),
                Value::Bool(false),
                Value::String("first".to_owned()),
            ],
        ]
    );
}

#[test]
fn explicit_insert_columns_reject_duplicates_unknowns_wrong_widths_and_types() {
    let cases = [
        (
            "INSERT INTO metrics (id, ID) VALUES (1, 2);",
            Error::DuplicateColumn("ID".to_owned()),
        ),
        (
            "INSERT INTO metrics (id, missing) VALUES (1, 'x');",
            Error::ColumnNotFound {
                table: "metrics".to_owned(),
                column: "missing".to_owned(),
            },
        ),
        (
            "INSERT INTO metrics (label, id) VALUES ('x');",
            Error::RowLength {
                table: "metrics".to_owned(),
                expected: 2,
                actual: 1,
            },
        ),
        (
            "INSERT INTO metrics (label, id) VALUES ('x', 1, true);",
            Error::RowLength {
                table: "metrics".to_owned(),
                expected: 2,
                actual: 3,
            },
        ),
        (
            "INSERT INTO metrics (label, id) VALUES ('x', 'not an integer');",
            Error::TypeMismatch {
                context: "column 'metrics.id'".to_owned(),
                expected: "Int64".to_owned(),
                actual: "String".to_owned(),
            },
        ),
    ];

    for (insert, expected) in cases {
        let mut database = Database::new();
        database
            .execute("CREATE TABLE metrics (id Int64, score Float64, active Bool, label String);")
            .expect("create target");
        assert_eq!(database.execute(insert), Err(expected), "{insert}");
        assert_eq!(database.catalog().table("metrics").unwrap().row_count(), 0);
    }
}

#[test]
fn reordered_multiple_type_errors_report_the_first_schema_column_for_both_insert_apis() {
    let expected = Error::TypeMismatch {
        context: "column 'metrics.id'".to_owned(),
        expected: "Int64".to_owned(),
        actual: "String".to_owned(),
    };
    let invalid = "INSERT INTO metrics (label, id) VALUES (1, 'x');";

    let mut ordinary = Database::new();
    ordinary
        .execute("CREATE TABLE metrics (id Int64, label String);")
        .expect("create ordinary target");
    assert_eq!(ordinary.execute(invalid), Err(expected.clone()));
    assert_eq!(ordinary.catalog().table("metrics").unwrap().row_count(), 0);

    let mut atomic = Database::new();
    atomic
        .execute(
            "CREATE TABLE events (id Int64); \
             CREATE TABLE metrics (id Int64, label String);",
        )
        .expect("create atomic targets");
    assert_eq!(
        atomic.execute_insert_batch(&format!("INSERT INTO events VALUES (1); {invalid}")),
        Err(expected),
    );
    assert_eq!(atomic.catalog().table("events").unwrap().row_count(), 0);
    assert_eq!(atomic.catalog().table("metrics").unwrap().row_count(), 0);
}

#[test]
fn named_insert_batch_reorders_before_validation_and_rolls_back_every_target() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE events (id Int64, label String); \
             CREATE TABLE metrics (id Int64, score Float64, active Bool, label String);",
        )
        .expect("create targets");

    for (invalid_insert, expected) in [
        (
            "INSERT INTO metrics (id, ID) VALUES (1, 2);",
            Error::DuplicateColumn("ID".to_owned()),
        ),
        (
            "INSERT INTO metrics (id, missing) VALUES (1, 2);",
            Error::ColumnNotFound {
                table: "metrics".to_owned(),
                column: "missing".to_owned(),
            },
        ),
        (
            "INSERT INTO metrics (label, score) VALUES ('bad');",
            Error::RowLength {
                table: "metrics".to_owned(),
                expected: 2,
                actual: 1,
            },
        ),
        (
            "INSERT INTO metrics (label, id) VALUES ('bad', 'not an integer');",
            Error::TypeMismatch {
                context: "column 'metrics.id'".to_owned(),
                expected: "Int64".to_owned(),
                actual: "String".to_owned(),
            },
        ),
    ] {
        let batch =
            format!("INSERT INTO events (label) VALUES ('kept only on success'); {invalid_insert}");
        assert_eq!(database.execute_insert_batch(&batch), Err(expected));
        assert_eq!(database.catalog().table("events").unwrap().row_count(), 0);
        assert_eq!(database.catalog().table("metrics").unwrap().row_count(), 0);
    }

    database
        .execute_insert_batch(
            "INSERT INTO events (LABEL) VALUES ('one'), ('two'); \
             INSERT INTO metrics (label, score) VALUES ('metric', 3.5);",
        )
        .expect("valid INSERT subsets commit with defaults");
    assert_eq!(database.catalog().table("events").unwrap().row_count(), 2);
    assert_eq!(database.catalog().table("metrics").unwrap().row_count(), 1);
}

#[test]
fn named_insert_batches_preflight_cumulative_capacity_before_defaulted_rows_commit() {
    let mut database = Database::with_max_rows_per_table(2);
    database
        .execute("CREATE TABLE events (id Int64, label String);")
        .expect("create target");

    assert_eq!(
        database.execute_insert_batch(
            "INSERT INTO events (label) VALUES ('first'); \
             INSERT INTO EVENTS (id) VALUES (2), (3);",
        ),
        Err(Error::ResourceLimitExceeded {
            resource: "table rows",
            actual: 3,
            max: 2,
        })
    );
    assert_eq!(database.catalog().table("events").unwrap().row_count(), 0);
}

#[test]
fn multi_table_late_type_failure_does_not_retain_default_expansions() {
    const TABLE_COUNT: usize = 25;
    const COLUMN_COUNT: usize = 1_000;
    const ROWS_PER_TABLE: usize = 4_000;

    // The SQL input contains exactly the default 100,000-value parser bound,
    // but eagerly expanding each row would retain 100 million Values before
    // discovering the final type error.
    let mut create = String::new();
    for table in 0..TABLE_COUNT {
        write!(create, "CREATE TABLE wide{table} (").unwrap();
        for column in 0..COLUMN_COUNT {
            if column != 0 {
                create.push_str(", ");
            }
            write!(create, "c{column} Int64").unwrap();
        }
        create.push_str(");");
    }

    let mut batch = String::new();
    for table in 0..TABLE_COUNT {
        write!(batch, "INSERT INTO wide{table} (c0) VALUES ").unwrap();
        for row in 0..ROWS_PER_TABLE {
            if row != 0 {
                batch.push(',');
            }
            if table == TABLE_COUNT - 1 && row == ROWS_PER_TABLE - 1 {
                batch.push_str("('late type error')");
            } else {
                batch.push_str("(1)");
            }
        }
        batch.push(';');
    }

    let mut database = Database::new();
    database.execute(&create).expect("create wide targets");
    assert_eq!(
        database.execute_insert_batch(&batch),
        Err(Error::TypeMismatch {
            context: "column 'wide24.c0'".to_owned(),
            expected: "Int64".to_owned(),
            actual: "String".to_owned(),
        })
    );
    for table in 0..TABLE_COUNT {
        assert_eq!(
            database
                .catalog()
                .table(&format!("wide{table}"))
                .unwrap()
                .row_count(),
            0
        );
    }
}

#[test]
fn wide_subsets_reject_capacity_before_ordinary_or_atomic_default_expansion() {
    const COLUMN_COUNT: usize = 1_024;
    const ROW_COUNT: usize = 100_000;

    let mut create = String::from("CREATE TABLE wide (");
    for index in 0..COLUMN_COUNT {
        if index != 0 {
            create.push_str(", ");
        }
        write!(create, "c{index} Int64").unwrap();
    }
    create.push_str(");");

    let insert = |row_count: usize| {
        let mut sql = String::with_capacity(35 + row_count.saturating_mul(4));
        sql.push_str("INSERT INTO wide (c0) VALUES ");
        for row in 0..row_count {
            if row != 0 {
                sql.push(',');
            }
            sql.push_str("(1)");
        }
        sql.push(';');
        sql
    };

    let mut database = Database::new();
    database.execute(&create).expect("create wide target");
    let expected = Error::ResourceLimitExceeded {
        resource: "table cells",
        actual: ROW_COUNT * COLUMN_COUNT,
        max: DEFAULT_MAX_CELLS_PER_TABLE,
    };

    assert_eq!(database.execute(&insert(ROW_COUNT)), Err(expected.clone()));
    assert_eq!(database.catalog().table("wide").unwrap().row_count(), 0);

    let atomic = format!("{}{}", insert(1), insert(ROW_COUNT - 1),);
    assert_eq!(database.execute_insert_batch(&atomic), Err(expected));
    assert_eq!(database.catalog().table("wide").unwrap().row_count(), 0);
}

#[test]
fn insert_subsets_follow_the_schema_after_columns_are_added_and_dropped() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE Metrics (id Int64, score Float64); \
             INSERT INTO metrics (id) VALUES (1); \
             ALTER TABLE metrics ADD COLUMN active Bool; \
             ALTER TABLE metrics ADD COLUMN label String; \
             INSERT INTO metrics (LABEL, ID) VALUES ('second', 2); \
             ALTER TABLE metrics DROP COLUMN score;",
        )
        .expect("subsets use the schema in effect for each statement");

    assert_eq!(
        database.execute_insert_batch(
            "INSERT INTO metrics (active) VALUES (true); \
             INSERT INTO metrics (id, score) VALUES (3, 3.5);",
        ),
        Err(Error::ColumnNotFound {
            table: "Metrics".to_owned(),
            column: "score".to_owned(),
        })
    );
    assert_eq!(database.catalog().table("metrics").unwrap().row_count(), 2);

    database
        .execute_insert_batch("INSERT INTO metrics (ACTIVE) VALUES (true);")
        .expect("the current schema supplies defaults for both omitted columns");
    assert_eq!(
        query_rows(
            &mut database,
            "SELECT id, active, label FROM metrics ORDER BY id;",
        ),
        [
            vec![
                Value::Int64(0),
                Value::Bool(true),
                Value::String(String::new()),
            ],
            vec![
                Value::Int64(1),
                Value::Bool(false),
                Value::String(String::new()),
            ],
            vec![
                Value::Int64(2),
                Value::Bool(false),
                Value::String("second".to_owned()),
            ],
        ]
    );
}
