use rusthouse::batch::engine::{Database, QueryResult, ResultColumn, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{AggregateArgument, AggregateFunction, SelectItem, Statement, parse};
use rusthouse::batch::value::{DataType, Value};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    match results.into_iter().last().expect("one result") {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

#[test]
fn parses_empty_count_case_insensitively_and_rejects_other_empty_aggregates() {
    for spelling in ["COUNT", "count", "CoUnT"] {
        let statements = parse(&format!("SELECT {spelling}() FROM events"))
            .expect("empty COUNT argument list parses");
        let [Statement::Select(select)] = statements.as_slice() else {
            panic!("expected SELECT");
        };
        assert_eq!(
            select.items,
            [SelectItem::Aggregate {
                function: AggregateFunction::Count,
                argument: AggregateArgument::Empty,
                alias: None,
            }]
        );
    }

    for function in ["countIf", "SUM", "MIN", "MAX", "AVG"] {
        assert!(
            matches!(
                parse(&format!("SELECT {function}() FROM events")),
                Err(Error::Sql { ref message, .. }) if message == "expected aggregate column"
            ),
            "{function}() must still be rejected"
        );
    }
}

#[test]
fn empty_count_uses_global_and_grouped_count_execution() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE events (kind String, included Bool); \
             INSERT INTO events VALUES \
                 ('a', true), ('a', true), ('a', false), \
                 ('b', true), ('b', true), ('c', true);",
        )
        .expect("setup");

    let global = query(
        &mut database,
        "SELECT count() FROM events WHERE included = true",
    );
    assert_eq!(
        global.columns,
        [ResultColumn {
            name: "COUNT()".to_owned(),
            data_type: DataType::Int64,
        }]
    );
    assert_eq!(global.rows, [vec![Value::Int64(5)]]);

    let aliased_global = query(
        &mut database,
        "SELECT COUNT() AS matches FROM events WHERE included = true \
         HAVING matches = 5 ORDER BY matches DESC LIMIT 1 OFFSET 0",
    );
    assert_eq!(aliased_global.columns[0].name, "matches");
    assert_eq!(aliased_global.rows, global.rows);

    let grouped = query(
        &mut database,
        "SELECT kind, CoUnT() AS matches FROM events WHERE included = true \
         GROUP BY kind HAVING matches >= 2 \
         ORDER BY matches DESC, kind DESC LIMIT 1 OFFSET 1",
    );
    assert_eq!(
        grouped.rows,
        [vec![Value::String("a".to_owned()), Value::Int64(2)]]
    );

    let wildcard = query(
        &mut database,
        "SELECT kind, COUNT(*) AS matches FROM events WHERE included = true \
         GROUP BY kind HAVING matches >= 2 \
         ORDER BY matches DESC, kind DESC LIMIT 1 OFFSET 1",
    );
    assert_eq!(grouped.rows, wildcard.rows);
}
