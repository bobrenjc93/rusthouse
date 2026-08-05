use std::mem::size_of;

use rusthouse::SharedDatabase;
use rusthouse::batch::engine::{
    Database, QueryResult, QueryResultLimits, ResultColumn, StatementResult,
};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{Statement, parse};
use rusthouse::batch::value::{DataType, Value};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    let [StatementResult::Query(result)] = results.as_slice() else {
        panic!("expected one query result");
    };
    result.clone()
}

fn describe_result_bytes(schema: &[(&str, DataType)]) -> usize {
    let row_count = schema.len();
    2 * size_of::<ResultColumn>()
        + "name".len()
        + "type".len()
        + row_count * size_of::<Vec<Value>>()
        + row_count * 2 * size_of::<Value>()
        + schema
            .iter()
            .map(|(name, data_type)| name.len() + data_type.to_string().len())
            .sum::<usize>()
}

#[test]
fn parses_only_describe_table_name_with_an_optional_semicolon() {
    for sql in ["DESCRIBE TABLE metrics", "describe table Metrics;"] {
        assert_eq!(
            parse(sql).expect("valid DESCRIBE TABLE"),
            [Statement::DescribeTable {
                name: if sql.contains("Metrics") {
                    "Metrics".to_owned()
                } else {
                    "metrics".to_owned()
                },
            }]
        );
    }

    assert!(matches!(
        parse("DESCRIBE metrics"),
        Err(Error::Sql { position: 9, .. })
    ));
    assert_eq!(
        parse("DESCRIBE TABLE metrics extra"),
        Err(Error::Sql {
            position: 23,
            message: "unexpected trailing input after DESCRIBE TABLE <name>".to_owned(),
        })
    );
}

#[test]
fn returns_all_four_types_in_schema_order_and_reports_missing_tables() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE Metrics (id Int64, score Float64, active Bool, label String);")
        .expect("setup succeeds");

    let result = query(&mut database, "DESCRIBE TABLE metrics;");
    assert_eq!(
        result.columns,
        [
            ResultColumn {
                name: "name".to_owned(),
                data_type: DataType::String,
            },
            ResultColumn {
                name: "type".to_owned(),
                data_type: DataType::String,
            },
        ]
    );
    assert_eq!(
        result.rows,
        [
            vec![
                Value::String("id".to_owned()),
                Value::String("Int64".to_owned())
            ],
            vec![
                Value::String("score".to_owned()),
                Value::String("Float64".to_owned())
            ],
            vec![
                Value::String("active".to_owned()),
                Value::String("Bool".to_owned())
            ],
            vec![
                Value::String("label".to_owned()),
                Value::String("String".to_owned())
            ],
        ]
    );
    assert_eq!(
        database.execute("DESCRIBE TABLE absent;"),
        Err(Error::TableNotFound("absent".to_owned()))
    );
}

#[test]
fn accepts_exact_and_rejects_exceeded_row_value_and_byte_limits() {
    const CREATE: &str =
        "CREATE TABLE metrics (id Int64, score Float64, active Bool, label String);";
    const SCHEMA: &[(&str, DataType)] = &[
        ("id", DataType::Int64),
        ("score", DataType::Float64),
        ("active", DataType::Bool),
        ("label", DataType::String),
    ];
    let exact_bytes = describe_result_bytes(SCHEMA);

    let mut exact = Database::with_query_result_limits(QueryResultLimits {
        max_rows: 4,
        max_values: 8,
        max_bytes: exact_bytes,
        ..QueryResultLimits::default()
    });
    exact.execute(CREATE).expect("setup succeeds");
    assert_eq!(query(&mut exact, "DESCRIBE TABLE metrics").rows.len(), 4);

    let cases = [
        (
            QueryResultLimits {
                max_rows: 3,
                ..QueryResultLimits::default()
            },
            Error::ResourceLimitExceeded {
                resource: "DESCRIBE TABLE result rows",
                actual: 4,
                max: 3,
            },
        ),
        (
            QueryResultLimits {
                max_rows: 4,
                max_values: 7,
                ..QueryResultLimits::default()
            },
            Error::ResourceLimitExceeded {
                resource: "DESCRIBE TABLE result values",
                actual: 8,
                max: 7,
            },
        ),
        (
            QueryResultLimits {
                max_rows: 4,
                max_values: 8,
                max_bytes: exact_bytes - 1,
                ..QueryResultLimits::default()
            },
            Error::ResourceLimitExceeded {
                resource: "DESCRIBE TABLE result bytes",
                actual: exact_bytes,
                max: exact_bytes - 1,
            },
        ),
    ];

    for (limits, expected) in cases {
        let mut database = Database::with_query_result_limits(limits);
        database.execute(CREATE).expect("setup succeeds");
        assert_eq!(database.execute("DESCRIBE TABLE metrics"), Err(expected));
    }
}

#[test]
fn shared_database_describes_under_the_read_only_query_api() {
    let database = SharedDatabase::default();
    database
        .execute("CREATE TABLE events (event_id Int64, payload String);")
        .expect("setup succeeds");

    assert_eq!(
        database
            .query("DESCRIBE TABLE EVENTS;")
            .expect("DESCRIBE is a read-only query")
            .rows,
        [
            vec![
                Value::String("event_id".to_owned()),
                Value::String("Int64".to_owned()),
            ],
            vec![
                Value::String("payload".to_owned()),
                Value::String("String".to_owned()),
            ],
        ]
    );
}
