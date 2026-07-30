use rusthouse::{DataType, Database, Error, QueryResult, StatementResult, Value};

fn prepared_query(
    database: &mut Database,
    statement: &rusthouse::PreparedStatement,
    parameters: &[Value],
) -> QueryResult {
    match database
        .execute_prepared(statement, parameters)
        .expect("prepared query succeeds")
    {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

fn direct_query(database: &mut Database, sql: &str) -> QueryResult {
    match database
        .execute(sql)
        .expect("direct query succeeds")
        .pop()
        .expect("one result")
    {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

#[test]
fn prepared_insert_and_select_keep_strings_out_of_sql_syntax() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE events (id Int64, label String, score Float64, active Bool)")
        .expect("create table");

    let insert = database
        .prepare("INSERT INTO events VALUES (?, ?, ?, ?)")
        .expect("prepare insert");
    assert_eq!(
        insert.parameter_types(),
        &[
            DataType::Int64,
            DataType::String,
            DataType::Float64,
            DataType::Bool,
        ]
    );
    assert!(insert.result_columns().is_none());

    let injected = "x'); INSERT INTO events VALUES (99, 'owned', 0.0, false); --";
    let result = database
        .execute_prepared(
            &insert,
            &[
                Value::Int64(1),
                Value::String(injected.to_owned()),
                Value::Float64(4.5),
                Value::Bool(true),
            ],
        )
        .expect("insert bound string");
    assert_eq!(
        result,
        StatementResult::Command {
            tag: "INSERT",
            affected_rows: 1,
        }
    );

    let select = database
        .prepare("SELECT id, label FROM events WHERE label = ?")
        .expect("prepare select");
    assert_eq!(select.parameter_types(), &[DataType::String]);
    assert_eq!(
        select
            .result_columns()
            .expect("SELECT metadata")
            .iter()
            .map(|column| (&column.name, column.data_type))
            .collect::<Vec<_>>(),
        vec![
            (&"id".to_owned(), DataType::Int64),
            (&"label".to_owned(), DataType::String)
        ]
    );
    let selected = prepared_query(
        &mut database,
        &select,
        &[Value::String(injected.to_owned())],
    );
    assert_eq!(
        selected.rows,
        vec![vec![Value::Int64(1), Value::String(injected.to_owned())]]
    );

    let count = direct_query(&mut database, "SELECT COUNT(*) AS count FROM events");
    assert_eq!(count.rows, vec![vec![Value::Int64(1)]]);
}

#[test]
fn numeric_boundaries_and_direct_execution_are_equivalent() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE boundaries (id Int64, reading Float64)")
        .expect("create table");
    let insert = database
        .prepare("INSERT INTO boundaries VALUES ($1, $2)")
        .expect("prepare numbered parameters");

    for parameters in [
        [Value::Int64(i64::MIN), Value::Float64(-f64::MAX)],
        [Value::Int64(0), Value::Float64(0.0)],
        [Value::Int64(i64::MAX), Value::Float64(f64::MAX)],
    ] {
        database
            .execute_prepared(&insert, &parameters)
            .expect("insert numeric boundary");
    }

    let select = database
        .prepare(
            "SELECT id, reading FROM boundaries
             WHERE id >= ?1 ORDER BY id LIMIT ?2",
        )
        .expect("prepare boundary select");
    assert_eq!(
        select.parameter_types(),
        &[DataType::Int64, DataType::Int64]
    );
    let prepared = prepared_query(
        &mut database,
        &select,
        &[Value::Int64(i64::MIN), Value::Int64(2)],
    );
    let direct = direct_query(
        &mut database,
        "SELECT id, reading FROM boundaries
         WHERE id >= -9223372036854775808 ORDER BY id LIMIT 2",
    );
    assert_eq!(prepared, direct);

    let float_select = database
        .prepare("SELECT id FROM boundaries WHERE reading = ?")
        .expect("prepare Float64 predicate");
    assert_eq!(
        prepared_query(&mut database, &float_select, &[Value::Float64(f64::MAX)]).rows,
        vec![vec![Value::Int64(i64::MAX)]]
    );
}

#[test]
fn binding_count_type_and_value_errors_do_not_mutate_data() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE samples (id Int64, reading Float64, label String)")
        .expect("create table");
    let insert = database
        .prepare("INSERT INTO samples VALUES (?, ?, ?)")
        .expect("prepare insert");

    let missing = database
        .execute_prepared(&insert, &[Value::Int64(1)])
        .expect_err("missing parameters");
    assert_eq!(
        missing,
        Error::ParameterCount {
            expected: 3,
            actual: 1,
        }
    );

    let extra = database
        .execute_prepared(
            &insert,
            &[
                Value::Int64(1),
                Value::Float64(1.0),
                Value::String("ok".to_owned()),
                Value::Bool(false),
            ],
        )
        .expect_err("extra parameter");
    assert!(matches!(
        extra,
        Error::ParameterCount {
            expected: 3,
            actual: 4
        }
    ));

    let wrong_type = database
        .execute_prepared(
            &insert,
            &[
                Value::String("1".to_owned()),
                Value::Float64(1.0),
                Value::String("ok".to_owned()),
            ],
        )
        .expect_err("incorrect parameter type");
    assert!(matches!(
        wrong_type,
        Error::TypeMismatch { context, expected, actual }
            if context == "parameter $1" && expected == "Int64" && actual == "String"
    ));

    let non_finite = database
        .execute_prepared(
            &insert,
            &[
                Value::Int64(1),
                Value::Float64(f64::NAN),
                Value::String("ok".to_owned()),
            ],
        )
        .expect_err("non-finite parameter");
    assert!(matches!(
        non_finite,
        Error::InvalidQuery(message) if message.contains("parameter $2 must be a finite Float64")
    ));

    let count = direct_query(&mut database, "SELECT COUNT(*) AS count FROM samples");
    assert_eq!(count.rows, vec![vec![Value::Int64(0)]]);

    let conflict = database
        .prepare("INSERT INTO samples VALUES ($1, 1.0, $1)")
        .expect_err("one numbered parameter cannot have two types");
    assert!(matches!(conflict, Error::TypeMismatch { context, .. } if context == "parameter $1"));

    let negative_limit = database
        .prepare("SELECT id FROM samples LIMIT ?")
        .expect("prepare LIMIT parameter");
    assert!(matches!(
        database.execute_prepared(&negative_limit, &[Value::Int64(-1)]),
        Err(Error::InvalidQuery(message)) if message.contains("non-negative")
    ));
}

#[test]
fn data_changes_preserve_plans_but_schema_changes_make_them_stale() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE valueset (id Int64)")
        .expect("create table");
    let insert = database
        .prepare("INSERT INTO valueset VALUES (?)")
        .expect("prepare insert");
    let select = database
        .prepare("SELECT id FROM valueset WHERE id >= ? ORDER BY id")
        .expect("prepare select");

    database
        .execute("INSERT INTO valueset VALUES (1)")
        .expect("direct data change");
    database
        .execute_prepared(&insert, &[Value::Int64(2)])
        .expect("prepared plan survives data changes");
    assert_eq!(
        prepared_query(&mut database, &select, &[Value::Int64(1)]).rows,
        vec![vec![Value::Int64(1)], vec![Value::Int64(2)]]
    );

    database
        .execute("CREATE TABLE another_table (id Int64)")
        .expect("schema change");
    assert_eq!(
        database
            .execute_prepared(&select, &[Value::Int64(1)])
            .expect_err("SELECT plan is stale"),
        Error::StalePreparedStatement
    );
    assert_eq!(
        database
            .execute_prepared(&insert, &[Value::Int64(3)])
            .expect_err("INSERT plan is stale"),
        Error::StalePreparedStatement
    );
    assert_eq!(
        direct_query(&mut database, "SELECT COUNT(*) AS count FROM valueset").rows,
        vec![vec![Value::Int64(2)]]
    );
}

#[test]
fn parameter_comparisons_use_types_inferred_across_the_statement() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE typed_values (id Int64, reading Float64);
             INSERT INTO typed_values VALUES (1, 1.0), (2, 2.5)",
        )
        .expect("setup");

    let reused = database
        .prepare(
            "SELECT id FROM typed_values
             WHERE id = $1 AND $1 = $1",
        )
        .expect("the column comparison types every use of $1");
    assert_eq!(reused.parameter_types(), &[DataType::Int64]);
    assert_eq!(
        prepared_query(&mut database, &reused, &[Value::Int64(2)]).rows,
        vec![vec![Value::Int64(2)]]
    );

    let transitive = database
        .prepare(
            "SELECT id FROM typed_values
             WHERE $1 = $2 AND id = $2 ORDER BY id",
        )
        .expect("the column type propagates through the parameter comparison");
    assert_eq!(
        transitive.parameter_types(),
        &[DataType::Int64, DataType::Int64]
    );
    assert_eq!(
        prepared_query(
            &mut database,
            &transitive,
            &[Value::Int64(1), Value::Int64(1)]
        )
        .rows,
        vec![vec![Value::Int64(1)]]
    );
    assert!(
        prepared_query(
            &mut database,
            &transitive,
            &[Value::Int64(1), Value::Int64(2)]
        )
        .rows
        .is_empty()
    );

    let compatible_numeric_types = database
        .prepare(
            "SELECT id FROM typed_values
             WHERE id = $1 AND reading = $2 AND $1 < $2",
        )
        .expect("parameter comparisons preserve compatible concrete numeric types");
    assert_eq!(
        compatible_numeric_types.parameter_types(),
        &[DataType::Int64, DataType::Float64]
    );
    assert_eq!(
        prepared_query(
            &mut database,
            &compatible_numeric_types,
            &[Value::Int64(2), Value::Float64(2.5)]
        )
        .rows,
        vec![vec![Value::Int64(2)]]
    );

    let reused_numeric_type = database
        .prepare(
            "SELECT id FROM typed_values
             WHERE id = $1 AND reading > $1",
        )
        .expect("one numeric parameter can be compared with Int64 and Float64 columns");
    assert_eq!(reused_numeric_type.parameter_types(), &[DataType::Int64]);
    assert_eq!(
        prepared_query(&mut database, &reused_numeric_type, &[Value::Int64(2)]).rows,
        vec![vec![Value::Int64(2)]]
    );

    let float_predicate_with_limit = database
        .prepare(
            "SELECT id FROM typed_values
             WHERE reading >= $1 ORDER BY id LIMIT $1",
        )
        .expect("LIMIT's exact Int64 type satisfies the Float64 predicate");
    assert_eq!(
        float_predicate_with_limit.parameter_types(),
        &[DataType::Int64]
    );
    assert_eq!(
        prepared_query(
            &mut database,
            &float_predicate_with_limit,
            &[Value::Int64(1)]
        )
        .rows,
        vec![vec![Value::Int64(1)]]
    );

    assert!(matches!(
        database.prepare("INSERT INTO typed_values VALUES ($1, $1)"),
        Err(Error::TypeMismatch { context, expected, actual })
            if context == "parameter $1" && expected == "Int64" && actual == "Float64"
    ));
}

#[test]
fn prepare_rejects_unsupported_or_untyped_statements() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE t (id Int64)")
        .expect("create table");

    assert!(matches!(
        database.prepare("CREATE TABLE other (id Int64)"),
        Err(Error::InvalidQuery(message)) if message.contains("only SELECT and INSERT")
    ));
    assert!(matches!(
        database.prepare("SELECT id FROM t; SELECT id FROM t"),
        Err(Error::InvalidQuery(message)) if message.contains("exactly one")
    ));
    assert!(matches!(
        database.prepare("SELECT id FROM t WHERE $1 = $2"),
        Err(Error::InvalidQuery(message)) if message.contains("cannot infer a type")
    ));
    assert!(matches!(
        database.prepare("SELECT id FROM t WHERE id = $2"),
        Err(Error::InvalidQuery(message)) if message.contains("must be contiguous")
    ));
    assert!(matches!(
        database.execute("SELECT id FROM t WHERE id = ?"),
        Err(Error::InvalidQuery(message)) if message.contains("Database::prepare")
    ));
}
