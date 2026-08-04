use rusthouse::batch::engine::{
    Database, QueryResult, QueryResultLimits, ResultColumn, StatementResult,
};
use rusthouse::batch::error::Error;
use rusthouse::batch::run_csv_batch;
use rusthouse::batch::sql::{BatchSqlLimits, SelectItem, Statement, parse, parse_with_limits};
use rusthouse::batch::value::{DataType, Value};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    match results.into_iter().last().expect("one result") {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

#[test]
fn parses_only_the_bounded_one_column_distinct_shape() {
    for sql in [
        "SELECT DISTINCT value FROM samples",
        "select distinct Value from Samples limit 0;",
    ] {
        let statements = parse(sql).expect("valid DISTINCT query");
        let Statement::Select(select) = &statements[0] else {
            panic!("expected SELECT");
        };
        assert!(select.distinct);
        assert!(matches!(
            select.items.as_slice(),
            [SelectItem::Column { alias: None, .. }]
        ));
    }

    for sql in [
        "SELECT DISTINCT * FROM samples",
        "SELECT DISTINCT value, other FROM samples",
        "SELECT DISTINCT value AS renamed FROM samples",
        "SELECT DISTINCT CAST(value AS Float64) FROM samples",
        "SELECT DISTINCT COUNT(value) FROM samples",
        "SELECT DISTINCT value FROM samples WHERE value = 1",
        "SELECT DISTINCT value FROM samples GROUP BY value",
        "SELECT DISTINCT value FROM samples HAVING value = 1",
        "SELECT DISTINCT value FROM samples ORDER BY value",
        "SELECT DISTINCT value FROM samples LIMIT -1",
    ] {
        assert!(
            matches!(parse(sql), Err(Error::Sql { .. })),
            "{sql:?} must return a typed SQL error"
        );
    }

    parse_with_limits(
        "SELECT DISTINCT value FROM samples",
        BatchSqlLimits {
            max_ast_list_items: 1,
            ..BatchSqlLimits::default()
        },
    )
    .expect("one DISTINCT projection fits the AST limit");
    assert_eq!(
        parse_with_limits(
            "SELECT DISTINCT value FROM samples",
            BatchSqlLimits {
                max_ast_list_items: 0,
                ..BatchSqlLimits::default()
            },
        ),
        Err(Error::ResourceLimitExceeded {
            resource: "SQL AST list items",
            actual: 1,
            max: 0,
        })
    );
}

#[test]
fn deduplicates_all_physical_types_in_first_seen_order() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (i Int64, f Float64, b Bool, s String); \
             INSERT INTO samples VALUES \
             (2, 2.5, true, 'beta'), \
             (1, -1.0, false, 'alpha'), \
             (2, 2.5, true, 'beta'), \
             (3, 4.0, false, 'gamma'), \
             (1, -1.0, true, 'alpha');",
        )
        .expect("setup");

    let cases = [
        (
            "i",
            DataType::Int64,
            vec![Value::Int64(2), Value::Int64(1), Value::Int64(3)],
        ),
        (
            "f",
            DataType::Float64,
            vec![
                Value::Float64(2.5),
                Value::Float64(-1.0),
                Value::Float64(4.0),
            ],
        ),
        (
            "b",
            DataType::Bool,
            vec![Value::Bool(true), Value::Bool(false)],
        ),
        (
            "s",
            DataType::String,
            vec![
                Value::String("beta".to_owned()),
                Value::String("alpha".to_owned()),
                Value::String("gamma".to_owned()),
            ],
        ),
    ];

    for (name, data_type, values) in cases {
        let result = query(
            &mut database,
            &format!("SELECT DISTINCT {name} FROM samples"),
        );
        assert_eq!(
            result.columns,
            [ResultColumn {
                name: name.to_owned(),
                data_type,
            }]
        );
        assert_eq!(
            result.rows,
            values
                .into_iter()
                .map(|value| vec![value])
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn handles_empty_input_and_zero_exact_and_exceeded_limits() {
    let mut empty = Database::with_query_result_limits(QueryResultLimits {
        max_groups: 0,
        ..QueryResultLimits::default()
    });
    empty
        .execute("CREATE TABLE samples (value Int64)")
        .expect("setup");
    assert!(
        query(&mut empty, "SELECT DISTINCT value FROM samples")
            .rows
            .is_empty()
    );

    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (value Int64); \
             INSERT INTO samples VALUES (2), (1), (2), (3), (1);",
        )
        .expect("setup");

    assert!(
        query(&mut database, "SELECT DISTINCT value FROM samples LIMIT 0")
            .rows
            .is_empty()
    );
    assert_eq!(
        query(&mut database, "SELECT DISTINCT value FROM samples LIMIT 3").rows,
        [
            vec![Value::Int64(2)],
            vec![Value::Int64(1)],
            vec![Value::Int64(3)]
        ]
    );
    assert_eq!(
        query(&mut database, "SELECT DISTINCT value FROM samples LIMIT 10").rows,
        [
            vec![Value::Int64(2)],
            vec![Value::Int64(1)],
            vec![Value::Int64(3)]
        ]
    );
}

#[test]
fn enforces_group_cap_before_limit_and_result_cap_after_limit() {
    let setup = "CREATE TABLE samples (value Int64); \
        INSERT INTO samples VALUES (1), (2), (1), (3);";
    let mut group_limited = Database::with_query_result_limits(QueryResultLimits {
        max_rows: usize::MAX,
        max_values: usize::MAX,
        max_bytes: usize::MAX,
        max_groups: 2,
        ..QueryResultLimits::default()
    });
    group_limited.execute(setup).expect("setup");
    assert_eq!(
        group_limited
            .execute("SELECT DISTINCT value FROM samples LIMIT 0")
            .expect_err("LIMIT cannot bypass DISTINCT working-state limits"),
        Error::ResourceLimitExceeded {
            resource: "SELECT groups",
            actual: 3,
            max: 2,
        }
    );

    let mut result_limited = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 2,
        max_values: 2,
        max_bytes: usize::MAX,
        max_groups: 3,
        ..QueryResultLimits::default()
    });
    result_limited.execute(setup).expect("setup");
    assert_eq!(
        query(
            &mut result_limited,
            "SELECT DISTINCT value FROM samples LIMIT 2"
        )
        .rows,
        [vec![Value::Int64(1)], vec![Value::Int64(2)]]
    );
    assert_eq!(
        result_limited
            .execute("SELECT DISTINCT value FROM samples")
            .expect_err("three output rows exceed the result cap"),
        Error::ResourceLimitExceeded {
            resource: "SELECT result rows",
            actual: 3,
            max: 2,
        }
    );
}

#[test]
fn csv_batch_emits_distinct_strings_with_escaping() {
    let input = b"CREATE TABLE labels (label String); \
        INSERT INTO labels VALUES ('beta'), ('comma,value'), ('beta'), ('alpha'); \
        SELECT DISTINCT label FROM labels LIMIT 3;";
    let mut output = Vec::new();

    run_csv_batch(&input[..], &mut output).expect("CSV batch succeeds");

    assert_eq!(output, b"label\nbeta\n\"comma,value\"\nalpha\n");
}
