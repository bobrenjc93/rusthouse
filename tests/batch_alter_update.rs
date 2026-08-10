use rusthouse::batch::engine::{Database, QueryResultLimits, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{AlterUpdateLiteral, AlterUpdateValue, Statement, parse};
use rusthouse::batch::storage::{Column, Int64MinMaxBlockMetadata, Int64MinMaxIndexLimits};
use rusthouse::batch::value::{DataType, Value};
use rusthouse::{SharedDatabase, SharedDatabaseError};

fn int64_column(database: &Database, table: &str, column: &str) -> Vec<i64> {
    let table = database.catalog().table(table).expect("table exists");
    let index = table.column_index(column).expect("column exists");
    let Column::Int64(values) = &table.columns()[index] else {
        panic!("column is Int64");
    };
    values.clone()
}

fn bool_column(database: &Database, table: &str, column: &str) -> Vec<bool> {
    let table = database.catalog().table(table).expect("table exists");
    let index = table.column_index(column).expect("column exists");
    let Column::Bool(values) = &table.columns()[index] else {
        panic!("column is Bool");
    };
    values.clone()
}

fn float64_column(database: &Database, table: &str, column: &str) -> Vec<f64> {
    let table = database.catalog().table(table).expect("table exists");
    let index = table.column_index(column).expect("column exists");
    let Column::Float64(values) = &table.columns()[index] else {
        panic!("column is Float64");
    };
    values.clone()
}

fn string_column(database: &Database, table: &str, column: &str) -> Vec<String> {
    let table = database.catalog().table(table).expect("table exists");
    let index = table.column_index(column).expect("column exists");
    let Column::String(values) = &table.columns()[index] else {
        panic!("column is String");
    };
    values.clone()
}

fn nullable_int64_column(database: &Database, table: &str, column: &str) -> Vec<Option<i64>> {
    let table = database.catalog().table(table).expect("table exists");
    let index = table.column_index(column).expect("column exists");
    let Column::NullableInt64(values) = &table.columns()[index] else {
        panic!("column is Nullable(Int64)");
    };
    values.clone()
}

#[test]
fn parses_int64_bool_and_finite_float64_alter_update_literals() {
    assert_eq!(
        parse(
            "aLtEr TaBlE Events UpDaTe Value = -9223372036854775808 \
             WhErE Selector = +9223372036854775807;"
        ),
        Ok(vec![Statement::AlterUpdate {
            table: "Events".to_owned(),
            target_column: "Value".to_owned(),
            value: i64::MIN,
            predicate_column: "Selector".to_owned(),
            predicate_value: i64::MAX,
        }])
    );
    assert_eq!(
        parse("ALTER TABLE Events UPDATE Active = TrUe WHERE Selected = fAlSe"),
        Ok(vec![Statement::AlterUpdateTyped {
            table: "Events".to_owned(),
            target_column: "Active".to_owned(),
            value: AlterUpdateLiteral::Bool(true),
            predicate_column: "Selected".to_owned(),
            predicate_value: AlterUpdateLiteral::Bool(false),
        }])
    );

    let parsed = parse(
        "ALTER TABLE Events UPDATE Score = -0.0 \
         WHERE Selector = +1.7976931348623157e308",
    )
    .expect("finite Float64 extrema parse");
    let [
        Statement::AlterUpdateTyped {
            value: AlterUpdateLiteral::Float64(value),
            predicate_value: AlterUpdateLiteral::Float64(predicate_value),
            ..
        },
    ] = parsed.as_slice()
    else {
        panic!("decimal and scientific literals use the typed update shape");
    };
    assert_eq!(*value, 0.0);
    assert!(value.is_sign_negative());
    assert_eq!(*predicate_value, f64::MAX);

    assert_eq!(
        parse("ALTER TABLE Events UPDATE Score = 2.5e-1 WHERE Active = TRUE"),
        Ok(vec![Statement::AlterUpdateTyped {
            table: "Events".to_owned(),
            target_column: "Score".to_owned(),
            value: AlterUpdateLiteral::Float64(0.25),
            predicate_column: "Active".to_owned(),
            predicate_value: AlterUpdateLiteral::Bool(true),
        }])
    );
}

#[test]
fn legacy_alter_update_literal_remains_copy_const_and_exhaustive() {
    const LEGACY_DATA_TYPE: DataType = AlterUpdateLiteral::Int64(7).data_type();
    const LEGACY_VALUE: Value = AlterUpdateLiteral::Bool(true).value();

    fn requires_copy<T: Copy>() {}
    fn exhaustive(literal: AlterUpdateLiteral) -> DataType {
        match literal {
            AlterUpdateLiteral::Int64(_) => DataType::Int64,
            AlterUpdateLiteral::Float64(_) => DataType::Float64,
            AlterUpdateLiteral::Bool(_) => DataType::Bool,
        }
    }

    requires_copy::<AlterUpdateLiteral>();
    let data_type_method: fn(AlterUpdateLiteral) -> DataType = AlterUpdateLiteral::data_type;
    let value_method: fn(AlterUpdateLiteral) -> Value = AlterUpdateLiteral::value;
    assert_eq!(LEGACY_DATA_TYPE, DataType::Int64);
    assert_eq!(LEGACY_VALUE, Value::Bool(true));
    assert_eq!(
        data_type_method(AlterUpdateLiteral::Float64(1.5)),
        DataType::Float64
    );
    assert_eq!(value_method(AlterUpdateLiteral::Int64(9)), Value::Int64(9));
    assert_eq!(exhaustive(AlterUpdateLiteral::Bool(false)), DataType::Bool);
}

#[test]
fn parses_empty_unicode_and_doubled_quote_string_literals() {
    assert_eq!(
        parse("ALTER TABLE Events UPDATE Label = 'it''s 🚀' WHERE Category = 'café'"),
        Ok(vec![Statement::AlterUpdateOwned {
            table: "Events".to_owned(),
            target_column: "Label".to_owned(),
            value: AlterUpdateValue::String("it's 🚀".to_owned()),
            predicate_column: "Category".to_owned(),
            predicate_value: AlterUpdateValue::String("café".to_owned()),
        }])
    );
    assert_eq!(
        parse("ALTER TABLE Events UPDATE Label = '' WHERE Category = ''"),
        Ok(vec![Statement::AlterUpdateOwned {
            table: "Events".to_owned(),
            target_column: "Label".to_owned(),
            value: AlterUpdateValue::String(String::new()),
            predicate_column: "Category".to_owned(),
            predicate_value: AlterUpdateValue::String(String::new()),
        }])
    );
}

#[test]
fn parses_null_only_in_the_alter_update_assignment() {
    assert_eq!(
        parse("ALTER TABLE Readings UPDATE Measurement = nUlL WHERE Measurement = -2"),
        Ok(vec![Statement::AlterUpdateOwned {
            table: "Readings".to_owned(),
            target_column: "Measurement".to_owned(),
            value: AlterUpdateValue::Null,
            predicate_column: "Measurement".to_owned(),
            predicate_value: AlterUpdateLiteral::Int64(-2).into(),
        }])
    );
    assert!(parse("ALTER TABLE Readings UPDATE Measurement = 0 WHERE Measurement = NULL").is_err());
}

#[test]
fn sql_created_nullable_target_accepts_null_and_refreshes_its_index() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE Readings (Measurement Nullable(Int64)); \
             INSERT INTO Readings VALUES (7), (2), (7), (NULL);",
        )
        .expect("setup succeeds");
    database
        .create_int64_min_max_index(
            "readings",
            "measurement",
            Int64MinMaxIndexLimits::new(2, 2, usize::MAX),
        )
        .expect("nullable index is admitted");

    assert_eq!(
        database.execute("ALTER TABLE READINGS UPDATE MEASUREMENT = NULL WHERE measurement = 7;"),
        Ok(vec![StatementResult::Command {
            tag: "ALTER TABLE",
            affected_rows: 2,
        }])
    );
    assert_eq!(
        nullable_int64_column(&database, "readings", "measurement"),
        [None, Some(2), None, None]
    );
    assert_eq!(
        database
            .catalog()
            .table("readings")
            .unwrap()
            .int64_min_max_index_blocks()
            .unwrap(),
        [
            Int64MinMaxBlockMetadata {
                first_row: 0,
                row_count: 2,
                min: Some(2),
                max: Some(2),
                null_count: 1,
            },
            Int64MinMaxBlockMetadata {
                first_row: 2,
                row_count: 2,
                min: None,
                max: None,
                null_count: 2,
            },
        ]
    );

    assert_eq!(
        database.execute("ALTER TABLE readings UPDATE measurement = NULL WHERE measurement = 99;"),
        Ok(vec![StatementResult::Command {
            tag: "ALTER TABLE",
            affected_rows: 0,
        }])
    );
    assert_eq!(
        nullable_int64_column(&database, "readings", "measurement"),
        [None, Some(2), None, None]
    );
}

#[test]
fn null_assignment_rejects_non_nullable_targets_and_honors_scan_limits_atomically() {
    let mut non_nullable = Database::new();
    non_nullable
        .execute("CREATE TABLE Readings (Measurement Int64); INSERT INTO Readings VALUES (1), (2);")
        .unwrap();
    assert_eq!(
        non_nullable
            .execute("ALTER TABLE Readings UPDATE Measurement = NULL WHERE Measurement = 2;"),
        Err(Error::TypeMismatch {
            context: "ALTER TABLE UPDATE target column 'Readings.Measurement'".to_owned(),
            expected: "Int64".to_owned(),
            actual: "NULL".to_owned(),
        })
    );
    assert_eq!(
        int64_column(&non_nullable, "readings", "measurement"),
        [1, 2]
    );

    let mut bounded = Database::with_query_result_limits(QueryResultLimits {
        max_scan_rows: 2,
        ..QueryResultLimits::default()
    });
    bounded
        .execute(
            "CREATE TABLE Readings (Measurement Nullable(Int64)); \
             INSERT INTO Readings VALUES (1), (2), (3);",
        )
        .unwrap();
    assert_eq!(
        bounded.execute("ALTER TABLE Readings UPDATE Measurement = NULL WHERE Measurement = 99;"),
        Err(Error::ResourceLimitExceeded {
            resource: "ALTER TABLE UPDATE scanned rows",
            actual: 3,
            max: 2,
        })
    );
    assert_eq!(
        nullable_int64_column(&bounded, "readings", "measurement"),
        [Some(1), Some(2), Some(3)]
    );
}

#[test]
fn original_public_int64_ast_shape_remains_directly_executable() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE events (id Int64, value Int64); \
             INSERT INTO events VALUES (1, 10), (2, 20), (2, 30);",
        )
        .expect("setup succeeds");

    assert_eq!(
        database.execute_statement(Statement::AlterUpdate {
            table: "events".to_owned(),
            target_column: "value".to_owned(),
            value: -7,
            predicate_column: "id".to_owned(),
            predicate_value: 2,
        }),
        Ok(StatementResult::Command {
            tag: "ALTER TABLE",
            affected_rows: 2,
        })
    );
    assert_eq!(int64_column(&database, "events", "value"), vec![10, -7, -7]);
}

#[test]
fn rejects_non_exact_alter_update_syntax_and_unsupported_literals() {
    for sql in [
        "ALTER events UPDATE value = 1 WHERE selector = 2",
        "ALTER TABLE events value = 1 WHERE selector = 2",
        "ALTER TABLE events UPDATE = 1 WHERE selector = 2",
        "ALTER TABLE events UPDATE value 1 WHERE selector = 2",
        "ALTER TABLE events UPDATE value = 1 selector = 2",
        "ALTER TABLE events UPDATE value = 1 WHERE = 2",
        "ALTER TABLE events UPDATE value = 1 WHERE selector != 2",
        "ALTER TABLE events UPDATE value = 1 WHERE selector = 2 AND value = 0",
        "ALTER TABLE events UPDATE value = 1 WHERE selector = 2 LIMIT 1",
        "ALTER TABLE events UPDATE value = 1 WHERE selector = NULL",
        "ALTER TABLE events UPDATE value = +true WHERE selector = 2",
        "ALTER TABLE events UPDATE value = 9223372036854775808 WHERE selector = 2",
        "ALTER TABLE events UPDATE value = 1 WHERE selector = -9223372036854775809",
        "ALTER TABLE events UPDATE value = 1e309 WHERE selector = 2",
        "ALTER TABLE events UPDATE value = 1.0 WHERE selector = -1e309",
        "ALTER TABLE events UPDATE value = 1e WHERE selector = 2",
        "ALTER TABLE events UPDATE value = 'unterminated WHERE selector = 2",
    ] {
        assert!(parse(sql).is_err(), "{sql:?} must be rejected");
    }
}

#[test]
fn string_targets_and_predicates_compose_with_every_existing_physical_type() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE events (id Int64, score Float64, active Bool, label String, category String); \
             INSERT INTO events VALUES \
                 (1, 1.5, false, 'one', 'queued'), \
                 (2, 2.5, false, 'two', 'queued'), \
                 (3, 3.5, true, 'three', 'done'), \
                 (4, 4.5, false, 'four', 'apostrophe''s');",
        )
        .expect("setup succeeds");
    let retained_before = database
        .catalog()
        .table("events")
        .unwrap()
        .retained_value_bytes();

    assert_eq!(
        database.execute(
            "ALTER TABLE EVENTS UPDATE LABEL = 'it''s 🚀' WHERE CATEGORY = 'queued'; \
             ALTER TABLE events UPDATE label = '' WHERE id = 3; \
             ALTER TABLE events UPDATE category = 'float match' WHERE score = 4.5; \
             ALTER TABLE events UPDATE category = 'bool match' WHERE active = true; \
             ALTER TABLE events UPDATE id = -7 WHERE label = 'it''s 🚀'; \
             ALTER TABLE events UPDATE score = -0.25 WHERE category = 'float match'; \
             ALTER TABLE events UPDATE active = false WHERE label = '';"
        ),
        Ok(vec![
            StatementResult::Command {
                tag: "ALTER TABLE",
                affected_rows: 2,
            },
            StatementResult::Command {
                tag: "ALTER TABLE",
                affected_rows: 1,
            },
            StatementResult::Command {
                tag: "ALTER TABLE",
                affected_rows: 1,
            },
            StatementResult::Command {
                tag: "ALTER TABLE",
                affected_rows: 1,
            },
            StatementResult::Command {
                tag: "ALTER TABLE",
                affected_rows: 2,
            },
            StatementResult::Command {
                tag: "ALTER TABLE",
                affected_rows: 1,
            },
            StatementResult::Command {
                tag: "ALTER TABLE",
                affected_rows: 1,
            },
        ])
    );

    assert_eq!(int64_column(&database, "events", "ID"), vec![-7, -7, 3, 4]);
    assert_eq!(
        float64_column(&database, "events", "score"),
        vec![1.5, 2.5, 3.5, -0.25]
    );
    assert_eq!(bool_column(&database, "events", "active"), vec![false; 4]);
    assert_eq!(
        string_column(&database, "EVENTS", "LABEL"),
        vec!["it's 🚀", "it's 🚀", "", "four"]
    );
    assert_eq!(
        string_column(&database, "events", "category"),
        vec!["queued", "queued", "bool match", "float match"]
    );
    assert_eq!(
        database
            .catalog()
            .table("events")
            .unwrap()
            .retained_value_bytes(),
        retained_before + 12
    );
}

#[test]
fn string_validation_failures_roll_back_values_and_retained_bytes() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE events (id Int64, label String, category String); \
             INSERT INTO events VALUES (1, 'one', 'queued'), (2, 'two', 'done');",
        )
        .expect("setup succeeds");
    let retained_before = database
        .catalog()
        .table("events")
        .unwrap()
        .retained_value_bytes();

    assert_eq!(
        database.execute("ALTER TABLE events UPDATE id = 'changed' WHERE label = 'one';"),
        Err(Error::TypeMismatch {
            context: "ALTER TABLE UPDATE target column 'events.id'".to_owned(),
            expected: "String".to_owned(),
            actual: "Int64".to_owned(),
        })
    );
    assert_eq!(
        database.execute("ALTER TABLE events UPDATE label = 'changed' WHERE id = '1';"),
        Err(Error::TypeMismatch {
            context: "ALTER TABLE UPDATE WHERE column 'events.id'".to_owned(),
            expected: "String".to_owned(),
            actual: "Int64".to_owned(),
        })
    );
    assert!(matches!(
        database.execute("ALTER TABLE events UPDATE label = 'unterminated WHERE id = 1;"),
        Err(Error::Sql { .. })
    ));

    assert_eq!(int64_column(&database, "events", "id"), vec![1, 2]);
    assert_eq!(
        string_column(&database, "events", "label"),
        vec!["one", "two"]
    );
    assert_eq!(
        string_column(&database, "events", "category"),
        vec!["queued", "done"]
    );
    assert_eq!(
        database
            .catalog()
            .table("events")
            .unwrap()
            .retained_value_bytes(),
        retained_before
    );
}

fn string_update_database(max_replacement_bytes: usize) -> Database {
    let mut database = Database::with_query_result_limits(QueryResultLimits {
        max_bytes: max_replacement_bytes,
        ..QueryResultLimits::default()
    });
    database
        .execute(
            "CREATE TABLE events (id Int64, selected Bool, label String); \
             INSERT INTO events VALUES \
                 (1, true, 'a'), (2, true, 'b'), (3, false, 'c');",
        )
        .expect("setup succeeds");
    database
}

#[test]
fn string_replacement_bytes_accept_the_exact_limit_and_reject_before_mutation() {
    let replacement = "é🚀";
    let required_bytes = replacement.len() * 2;
    let mut exact = string_update_database(required_bytes);

    assert_eq!(
        exact.execute("ALTER TABLE events UPDATE label = 'é🚀' WHERE selected = true;"),
        Ok(vec![StatementResult::Command {
            tag: "ALTER TABLE",
            affected_rows: 2,
        }])
    );
    assert_eq!(
        string_column(&exact, "events", "label"),
        vec![replacement, replacement, "c"]
    );

    let mut exceeded = string_update_database(required_bytes - 1);
    let retained_before = exceeded
        .catalog()
        .table("events")
        .unwrap()
        .retained_value_bytes();
    assert_eq!(
        exceeded.execute("ALTER TABLE events UPDATE label = 'é🚀' WHERE selected = true;"),
        Err(Error::ResourceLimitExceeded {
            resource: "ALTER TABLE UPDATE replacement String bytes",
            actual: required_bytes,
            max: required_bytes - 1,
        })
    );
    assert_eq!(
        string_column(&exceeded, "events", "label"),
        vec!["a", "b", "c"]
    );
    assert_eq!(
        exceeded
            .catalog()
            .table("events")
            .unwrap()
            .retained_value_bytes(),
        retained_before
    );
}

#[test]
fn zero_match_string_update_clones_nothing_and_fits_a_zero_byte_limit() {
    let mut database = string_update_database(0);
    let retained_before = database
        .catalog()
        .table("events")
        .unwrap()
        .retained_value_bytes();
    let large_assignment = "x".repeat(64 * 1024);
    let sql = format!("ALTER TABLE events UPDATE label = '{large_assignment}' WHERE id = 99;");

    assert_eq!(
        database.execute(&sql),
        Ok(vec![StatementResult::Command {
            tag: "ALTER TABLE",
            affected_rows: 0,
        }])
    );
    assert_eq!(
        string_column(&database, "events", "label"),
        vec!["a", "b", "c"]
    );
    assert_eq!(
        database
            .catalog()
            .table("events")
            .unwrap()
            .retained_value_bytes(),
        retained_before
    );
}

#[test]
fn bool_targets_and_predicates_support_zero_all_and_mixed_type_matches() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE flags (id Int64, active Bool, selected Bool, revision Int64); \
             INSERT INTO flags VALUES \
                 (1, false, true, 0), \
                 (2, false, true, 0), \
                 (3, false, true, 0);",
        )
        .expect("setup succeeds");

    assert_eq!(
        database.execute(
            "ALTER TABLE FLAGS UPDATE ACTIVE = TRUE WHERE SELECTED = TRUE; \
             ALTER TABLE flags UPDATE revision = 9 WHERE active = true;"
        ),
        Ok(vec![
            StatementResult::Command {
                tag: "ALTER TABLE",
                affected_rows: 3,
            },
            StatementResult::Command {
                tag: "ALTER TABLE",
                affected_rows: 3,
            },
        ])
    );
    assert_eq!(bool_column(&database, "flags", "active"), vec![true; 3]);
    assert_eq!(int64_column(&database, "flags", "revision"), vec![9; 3]);

    assert_eq!(
        database.execute("ALTER TABLE flags UPDATE active = false WHERE id = 99;"),
        Ok(vec![StatementResult::Command {
            tag: "ALTER TABLE",
            affected_rows: 0,
        }])
    );
    assert_eq!(bool_column(&database, "flags", "active"), vec![true; 3]);
}

#[test]
fn float64_targets_and_predicates_compose_with_int64_and_bool() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE metrics (id Int64, score Float64, selector Float64, selected Bool, active Bool, revision Int64); \
             INSERT INTO metrics VALUES \
                 (1, 10.5, -0.0, true, false, 0), \
                 (2, 20.5, +0.0, false, false, 0), \
                 (3, 30.5, 2.5e-1, false, false, 0), \
                 (4, 40.5, 1.7976931348623157e308, false, false, 0);",
        )
        .expect("setup succeeds");

    assert_eq!(
        database.execute("ALTER TABLE metrics UPDATE score = -0.0 WHERE id = 1;"),
        Ok(vec![StatementResult::Command {
            tag: "ALTER TABLE",
            affected_rows: 1,
        }])
    );
    let scores = float64_column(&database, "metrics", "score");
    assert_eq!(scores[0], 0.0);
    assert!(scores[0].is_sign_negative());

    assert_eq!(
        database.execute(
            "ALTER TABLE metrics UPDATE revision = -7 WHERE selector = 2.5e-1; \
             ALTER TABLE metrics UPDATE active = true WHERE selector = -0.0; \
             ALTER TABLE metrics UPDATE score = -1.7976931348623157e308 WHERE active = true; \
             ALTER TABLE metrics UPDATE score = +1.7976931348623157e308 WHERE id = 4; \
             ALTER TABLE metrics UPDATE selected = true WHERE selector = 1.7976931348623157e308;"
        ),
        Ok(vec![
            StatementResult::Command {
                tag: "ALTER TABLE",
                affected_rows: 1,
            },
            StatementResult::Command {
                tag: "ALTER TABLE",
                affected_rows: 2,
            },
            StatementResult::Command {
                tag: "ALTER TABLE",
                affected_rows: 2,
            },
            StatementResult::Command {
                tag: "ALTER TABLE",
                affected_rows: 1,
            },
            StatementResult::Command {
                tag: "ALTER TABLE",
                affected_rows: 1,
            },
        ])
    );
    assert_eq!(
        int64_column(&database, "metrics", "revision"),
        vec![0, 0, -7, 0]
    );
    assert_eq!(
        bool_column(&database, "metrics", "active"),
        vec![true, true, false, false]
    );
    assert_eq!(
        bool_column(&database, "metrics", "selected"),
        vec![true, false, false, true]
    );
    assert_eq!(
        float64_column(&database, "metrics", "score"),
        vec![-f64::MAX, -f64::MAX, 30.5, f64::MAX]
    );
}

#[test]
fn zero_and_all_matches_are_atomic_and_support_int64_extrema() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE Events (value Int64, selector Int64, label String); \
             INSERT INTO Events VALUES \
                 (0, 9223372036854775807, 'first'), \
                 (1, 9223372036854775807, 'second'), \
                 (2, 9223372036854775807, 'third');",
        )
        .expect("setup succeeds");

    assert_eq!(
        database.execute(
            "ALTER TABLE events UPDATE VALUE = -9223372036854775808 \
             WHERE SELECTOR = +9223372036854775807;"
        ),
        Ok(vec![StatementResult::Command {
            tag: "ALTER TABLE",
            affected_rows: 3,
        }])
    );
    assert_eq!(
        int64_column(&database, "events", "value"),
        vec![i64::MIN; 3]
    );

    assert_eq!(
        database.execute(
            "ALTER TABLE EVENTS UPDATE value = +9223372036854775807 \
             WHERE selector = -9223372036854775808;"
        ),
        Ok(vec![StatementResult::Command {
            tag: "ALTER TABLE",
            affected_rows: 0,
        }])
    );
    assert_eq!(
        int64_column(&database, "events", "value"),
        vec![i64::MIN; 3]
    );
    assert_eq!(
        database
            .execute("SELECT selector, label FROM events ORDER BY label;")
            .expect("unselected columns remain queryable")[0],
        StatementResult::Query(rusthouse::batch::engine::QueryResult {
            columns: vec![
                rusthouse::batch::engine::ResultColumn {
                    name: "selector".to_owned(),
                    data_type: DataType::Int64,
                },
                rusthouse::batch::engine::ResultColumn {
                    name: "label".to_owned(),
                    data_type: DataType::String,
                },
            ],
            rows: vec![
                vec![Value::Int64(i64::MAX), Value::String("first".to_owned())],
                vec![Value::Int64(i64::MAX), Value::String("second".to_owned())],
                vec![Value::Int64(i64::MAX), Value::String("third".to_owned())],
            ],
        })
    );
}

#[test]
fn missing_names_and_literal_type_mismatches_fail_without_changes() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE events (id Int64, value Int64, active Bool, selected Bool, score Float64, label String); \
             INSERT INTO events VALUES \
                 (1, 10, false, true, 1.5, 'one'), \
                 (2, 20, true, false, 2.5, 'two');",
        )
        .expect("setup succeeds");

    assert_eq!(
        database.execute("ALTER TABLE missing UPDATE value = 0 WHERE id = 1;"),
        Err(Error::TableNotFound("missing".to_owned()))
    );
    assert_eq!(
        database.execute("ALTER TABLE events UPDATE absent = 0 WHERE id = 1;"),
        Err(Error::ColumnNotFound {
            table: "events".to_owned(),
            column: "absent".to_owned(),
        })
    );
    assert_eq!(
        database.execute("ALTER TABLE events UPDATE value = 0 WHERE absent = 1;"),
        Err(Error::ColumnNotFound {
            table: "events".to_owned(),
            column: "absent".to_owned(),
        })
    );
    assert_eq!(
        database.execute("ALTER TABLE events UPDATE score = 0 WHERE id = 1;"),
        Err(Error::TypeMismatch {
            context: "ALTER TABLE UPDATE target column 'events.score'".to_owned(),
            expected: "Int64".to_owned(),
            actual: "Float64".to_owned(),
        })
    );
    assert_eq!(
        database.execute("ALTER TABLE events UPDATE value = 0 WHERE label = 1;"),
        Err(Error::TypeMismatch {
            context: "ALTER TABLE UPDATE WHERE column 'events.label'".to_owned(),
            expected: "Int64".to_owned(),
            actual: "String".to_owned(),
        })
    );
    assert_eq!(
        database.execute("ALTER TABLE events UPDATE active = 0 WHERE id = 1;"),
        Err(Error::TypeMismatch {
            context: "ALTER TABLE UPDATE target column 'events.active'".to_owned(),
            expected: "Int64".to_owned(),
            actual: "Bool".to_owned(),
        })
    );
    assert_eq!(
        database.execute("ALTER TABLE events UPDATE value = true WHERE id = 1;"),
        Err(Error::TypeMismatch {
            context: "ALTER TABLE UPDATE target column 'events.value'".to_owned(),
            expected: "Bool".to_owned(),
            actual: "Int64".to_owned(),
        })
    );
    assert_eq!(
        database.execute("ALTER TABLE events UPDATE active = true WHERE selected = 1;"),
        Err(Error::TypeMismatch {
            context: "ALTER TABLE UPDATE WHERE column 'events.selected'".to_owned(),
            expected: "Int64".to_owned(),
            actual: "Bool".to_owned(),
        })
    );
    assert_eq!(
        database.execute("ALTER TABLE events UPDATE score = 0 WHERE absent = 1;"),
        Err(Error::ColumnNotFound {
            table: "events".to_owned(),
            column: "absent".to_owned(),
        })
    );
    assert_eq!(
        database.execute("ALTER TABLE events UPDATE value = 1.0 WHERE id = 1;"),
        Err(Error::TypeMismatch {
            context: "ALTER TABLE UPDATE target column 'events.value'".to_owned(),
            expected: "Float64".to_owned(),
            actual: "Int64".to_owned(),
        })
    );
    assert_eq!(
        database.execute("ALTER TABLE events UPDATE score = true WHERE id = 1;"),
        Err(Error::TypeMismatch {
            context: "ALTER TABLE UPDATE target column 'events.score'".to_owned(),
            expected: "Bool".to_owned(),
            actual: "Float64".to_owned(),
        })
    );
    assert_eq!(
        database.execute("ALTER TABLE events UPDATE score = 1.0 WHERE id = 1.0;"),
        Err(Error::TypeMismatch {
            context: "ALTER TABLE UPDATE WHERE column 'events.id'".to_owned(),
            expected: "Float64".to_owned(),
            actual: "Int64".to_owned(),
        })
    );
    assert_eq!(
        database.execute("ALTER TABLE events UPDATE score = 1.0 WHERE selected = 1.0;"),
        Err(Error::TypeMismatch {
            context: "ALTER TABLE UPDATE WHERE column 'events.selected'".to_owned(),
            expected: "Float64".to_owned(),
            actual: "Bool".to_owned(),
        })
    );

    assert_eq!(int64_column(&database, "events", "value"), vec![10, 20]);
    assert_eq!(
        bool_column(&database, "events", "active"),
        vec![false, true]
    );
    assert_eq!(float64_column(&database, "events", "score"), vec![1.5, 2.5]);
}

#[test]
fn full_table_scan_limit_is_checked_after_names_and_types_and_before_mutation() {
    let limits = QueryResultLimits {
        max_scan_rows: 2,
        ..QueryResultLimits::default()
    };
    let mut database = Database::with_query_result_limits(limits);
    database
        .execute(
            "CREATE TABLE events (id Int64, value Int64, active Bool, selected Bool, label String, score Float64); \
             INSERT INTO events VALUES \
                 (1, 10, false, false, 'one', 1.5), \
                 (2, 20, false, true, 'two', 2.5), \
                 (3, 30, false, true, 'three', 3.5);",
        )
        .expect("setup is not subject to scan limits");

    assert_eq!(
        database.execute("ALTER TABLE events UPDATE value = 0 WHERE id = 3;"),
        Err(Error::ResourceLimitExceeded {
            resource: "ALTER TABLE UPDATE scanned rows",
            actual: 3,
            max: 2,
        })
    );
    assert_eq!(int64_column(&database, "events", "value"), vec![10, 20, 30]);

    assert_eq!(
        database.execute("ALTER TABLE events UPDATE active = true WHERE selected = true;"),
        Err(Error::ResourceLimitExceeded {
            resource: "ALTER TABLE UPDATE scanned rows",
            actual: 3,
            max: 2,
        })
    );
    assert_eq!(bool_column(&database, "events", "active"), vec![false; 3]);

    assert_eq!(
        database.execute("ALTER TABLE events UPDATE score = 0.5 WHERE score = 3.5;"),
        Err(Error::ResourceLimitExceeded {
            resource: "ALTER TABLE UPDATE scanned rows",
            actual: 3,
            max: 2,
        })
    );
    assert_eq!(
        float64_column(&database, "events", "score"),
        vec![1.5, 2.5, 3.5]
    );

    let retained_before = database
        .catalog()
        .table("events")
        .unwrap()
        .retained_value_bytes();
    assert_eq!(
        database.execute("ALTER TABLE EVENTS UPDATE LABEL = 'changed' WHERE label = 'three';"),
        Err(Error::ResourceLimitExceeded {
            resource: "ALTER TABLE UPDATE scanned rows",
            actual: 3,
            max: 2,
        })
    );
    assert_eq!(
        string_column(&database, "events", "label"),
        vec!["one", "two", "three"]
    );
    assert_eq!(
        database
            .catalog()
            .table("events")
            .unwrap()
            .retained_value_bytes(),
        retained_before
    );

    assert_eq!(
        database.execute_statement(Statement::AlterUpdateTyped {
            table: "events".to_owned(),
            target_column: "score".to_owned(),
            value: AlterUpdateLiteral::Float64(f64::INFINITY),
            predicate_column: "id".to_owned(),
            predicate_value: AlterUpdateLiteral::Int64(1),
        }),
        Err(Error::InvalidQuery(
            "ALTER TABLE UPDATE assignment Float64 literal must be finite".to_owned()
        ))
    );
    assert_eq!(
        database.execute_statement(Statement::AlterUpdateTyped {
            table: "events".to_owned(),
            target_column: "score".to_owned(),
            value: AlterUpdateLiteral::Float64(0.0),
            predicate_column: "score".to_owned(),
            predicate_value: AlterUpdateLiteral::Float64(f64::NAN),
        }),
        Err(Error::InvalidQuery(
            "ALTER TABLE UPDATE WHERE Float64 literal must be finite".to_owned()
        ))
    );
    assert_eq!(
        float64_column(&database, "events", "score"),
        vec![1.5, 2.5, 3.5]
    );

    assert!(matches!(
        database.execute("ALTER TABLE events UPDATE missing = 0 WHERE id = 3;"),
        Err(Error::ColumnNotFound { column, .. }) if column == "missing"
    ));
    assert!(matches!(
        database.execute("ALTER TABLE events UPDATE value = 0 WHERE label = 3;"),
        Err(Error::TypeMismatch { context, .. }) if context.contains("WHERE column")
    ));
    assert!(matches!(
        database.execute("ALTER TABLE events UPDATE active = 1 WHERE selected = true;"),
        Err(Error::TypeMismatch { context, .. }) if context.contains("target column")
    ));
    assert_eq!(int64_column(&database, "events", "value"), vec![10, 20, 30]);
    assert_eq!(bool_column(&database, "events", "active"), vec![false; 3]);

    let mut boundary = Database::with_query_result_limits(limits);
    assert_eq!(
        boundary.execute(
            "CREATE TABLE events (id Int64, active Bool); \
             INSERT INTO events VALUES (1, false), (2, false); \
             ALTER TABLE events UPDATE active = true WHERE id = 2;"
        ),
        Ok(vec![
            StatementResult::Command {
                tag: "CREATE TABLE",
                affected_rows: 0,
            },
            StatementResult::Command {
                tag: "INSERT",
                affected_rows: 2,
            },
            StatementResult::Command {
                tag: "ALTER TABLE",
                affected_rows: 1,
            },
        ])
    );
    assert_eq!(
        bool_column(&boundary, "events", "active"),
        vec![false, true]
    );
}

#[test]
fn shared_database_executes_bool_alter_update_under_the_write_api() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE events (id Int64, active Bool, selected Bool); \
             INSERT INTO events VALUES \
                 (1, false, false), (2, false, true), (3, false, true);",
        )
        .expect("setup succeeds");

    assert_eq!(
        database.execute("ALTER TABLE EVENTS UPDATE active = TRUE WHERE selected = true;"),
        Ok(vec![StatementResult::Command {
            tag: "ALTER TABLE",
            affected_rows: 2,
        }])
    );
    assert_eq!(
        database
            .query("SELECT id, active FROM events ORDER BY id;")
            .expect("updated values are visible")
            .rows,
        vec![
            vec![Value::Int64(1), Value::Bool(false)],
            vec![Value::Int64(2), Value::Bool(true)],
            vec![Value::Int64(3), Value::Bool(true)],
        ]
    );
    assert_eq!(
        database.query("ALTER TABLE events UPDATE active = true WHERE id = 1;"),
        Err(SharedDatabaseError::ReadOnlyStatementRequired {
            statement: "ALTER TABLE",
        })
    );
    assert_eq!(
        database
            .query("SELECT active FROM events WHERE id = 1;")
            .expect("read-only rejection leaves the row unchanged")
            .rows,
        vec![vec![Value::Bool(false)]]
    );
}

#[test]
fn shared_database_metrics_track_string_replacement_bytes() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE events (id Int64, label String); \
             INSERT INTO events VALUES (1, 'a'), (2, 'é');",
        )
        .expect("setup succeeds");
    let before = database.metrics_snapshot().expect("metrics are available");

    assert_eq!(
        database.execute("ALTER TABLE EVENTS UPDATE LABEL = '🚀' WHERE ID = 1;"),
        Ok(vec![StatementResult::Command {
            tag: "ALTER TABLE",
            affected_rows: 1,
        }])
    );
    assert_eq!(
        database
            .query("SELECT id, label FROM events ORDER BY id;")
            .expect("updated strings are visible")
            .rows,
        vec![
            vec![Value::Int64(1), Value::String("🚀".to_owned())],
            vec![Value::Int64(2), Value::String("é".to_owned())],
        ]
    );
    let after = database.metrics_snapshot().expect("metrics are available");
    assert_eq!(after.retained_value_bytes, before.retained_value_bytes + 3);
    assert_eq!(after.table_count, before.table_count);
    assert_eq!(after.column_count, before.column_count);
    assert_eq!(after.retained_row_count, before.retained_row_count);
}
