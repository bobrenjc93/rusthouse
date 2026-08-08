use rusthouse::batch::engine::{
    Database, QueryResult, QueryResultLimits, ResultColumn, StatementResult,
};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{
    AggregateArgument, AggregateFunction, BatchSqlLimits, SelectItem, Statement, parse,
    parse_with_limits,
};
use rusthouse::batch::value::{DataType, Value};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    match results.into_iter().last().expect("one result") {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

#[test]
fn parses_count_if_case_insensitively_under_ast_limits() {
    let statements =
        parse("SELECT countIf(active), COUNTIF(included) AS included_count FROM events")
            .expect("countIf aggregates parse");
    let [Statement::Select(select)] = statements.as_slice() else {
        panic!("expected SELECT");
    };
    assert_eq!(
        select.items,
        [
            SelectItem::Aggregate {
                function: AggregateFunction::CountIf,
                argument: AggregateArgument::Column("active".to_owned()),
                alias: None,
            },
            SelectItem::Aggregate {
                function: AggregateFunction::CountIf,
                argument: AggregateArgument::Column("included".to_owned()),
                alias: Some("included_count".to_owned()),
            },
        ]
    );

    let limits = BatchSqlLimits {
        max_ast_list_items: 1,
        ..BatchSqlLimits::default()
    };
    parse_with_limits("SELECT countIf(active) FROM events", limits)
        .expect("one countIf projection fits the AST limit");
    assert_eq!(
        parse_with_limits(
            "SELECT countIf(active), countIf(included) FROM events",
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
fn counts_true_values_after_where_in_global_and_grouped_queries() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE events (kind String, active Bool, included Bool); \
             INSERT INTO events VALUES \
                 ('a', true, true), ('a', false, true), ('a', true, false), \
                 ('b', true, true), ('b', false, true), ('b', true, true), \
                 ('c', false, true);",
        )
        .expect("setup");

    let global = query(
        &mut database,
        "SELECT countIf(active) AS matches FROM events WHERE included = true \
         HAVING matches = 3 ORDER BY matches DESC LIMIT 1 OFFSET 0",
    );
    assert_eq!(
        global.columns,
        [ResultColumn {
            name: "matches".to_owned(),
            data_type: DataType::Int64,
        }]
    );
    assert_eq!(global.rows, [vec![Value::Int64(3)]]);

    let grouped = query(
        &mut database,
        "SELECT kind, COUNTIF(active) AS true_count FROM events \
         WHERE included = true GROUP BY kind HAVING true_count >= 1 \
         ORDER BY true_count DESC, kind ASC LIMIT 1 OFFSET 1",
    );
    assert_eq!(
        grouped.rows,
        [vec![Value::String("a".to_owned()), Value::Int64(1)]]
    );
}

#[test]
fn returns_int64_zero_for_empty_global_input_and_no_empty_groups() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE empty_events (kind String, active Bool); \
             CREATE TABLE events (kind String, active Bool); \
             INSERT INTO events VALUES ('present', true);",
        )
        .expect("setup");

    let empty = query(&mut database, "SELECT countIf(active) FROM empty_events");
    assert_eq!(
        empty.columns,
        [ResultColumn {
            name: "countIf(active)".to_owned(),
            data_type: DataType::Int64,
        }]
    );
    assert_eq!(empty.rows, [vec![Value::Int64(0)]]);
    assert_eq!(
        query(
            &mut database,
            "SELECT countIf(active) AS matches FROM events WHERE kind = 'missing'",
        )
        .rows,
        [vec![Value::Int64(0)]]
    );
    assert!(
        query(
            &mut database,
            "SELECT kind, countIf(active) FROM empty_events GROUP BY kind",
        )
        .rows
        .is_empty()
    );
}

#[test]
fn rejects_wildcard_and_non_bool_count_if_arguments_with_typed_errors() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (i Int64, f Float64, b Bool, s String); \
             INSERT INTO samples VALUES (1, 1.5, true, 'one');",
        )
        .expect("setup");

    assert_eq!(
        database.execute("SELECT countIf(*) FROM samples"),
        Err(Error::InvalidQuery(
            "countIf(*) is not supported; use a column argument".to_owned()
        ))
    );
    assert_eq!(
        database.execute("SELECT countIf(true) FROM samples"),
        Err(Error::ColumnNotFound {
            table: "samples".to_owned(),
            column: "true".to_owned(),
        })
    );
    for (name, actual) in [
        ("i", DataType::Int64),
        ("f", DataType::Float64),
        ("s", DataType::String),
    ] {
        assert_eq!(
            database.execute(&format!("SELECT countIf({name}) FROM samples")),
            Err(Error::TypeMismatch {
                context: "countIf argument".to_owned(),
                expected: "Bool".to_owned(),
                actual: actual.to_string(),
            }),
            "column {name}"
        );
    }
}

#[test]
fn count_if_uses_the_existing_bounded_aggregate_state_cells() {
    let mut database = Database::with_query_result_limits(QueryResultLimits {
        max_aggregate_state_cells: 1,
        ..QueryResultLimits::default()
    });
    database
        .execute(
            "CREATE TABLE events (active Bool, included Bool); \
             INSERT INTO events VALUES (true, true);",
        )
        .expect("setup");

    assert_eq!(
        database.execute("SELECT countIf(active), countIf(included) FROM events"),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT aggregate state cells",
            actual: 2,
            max: 1,
        })
    );
}

#[test]
fn rejects_malformed_count_if_syntax() {
    for sql in [
        "SELECT countIf() FROM events",
        "SELECT countIf(active, included) FROM events",
        "SELECT countIf(countIf(active)) FROM events",
        "SELECT countIf(active FROM events",
    ] {
        assert!(parse(sql).is_err(), "{sql:?} must be rejected");
    }
}
