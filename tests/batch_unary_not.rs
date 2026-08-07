use rusthouse::batch::engine::{Database, QueryResult, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{
    ComparisonOperator, Operand, Predicate, Select, SelectItem, Statement, parse,
};
use rusthouse::batch::value::{DataType, Value};

const PREDICATE_DEPTH_CAP: usize = 64;
const PREDICATE_NODE_CAP: usize = 256;

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    match results.into_iter().last().expect("one result") {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

fn predicate(sql: &str) -> Predicate {
    let statements = parse(sql).expect("valid predicate query");
    let Statement::Select(select) = &statements[0] else {
        panic!("expected SELECT");
    };
    select.predicate.clone().expect("WHERE predicate")
}

fn comparison(column: &str, operator: ComparisonOperator, value: Value) -> Predicate {
    Predicate::Comparison {
        left: Operand::Column(column.to_owned()),
        operator,
        right: Operand::Literal(value),
    }
}

#[test]
fn parses_not_above_and_and_or_in_regular_and_distinct_queries() {
    assert_eq!(
        predicate("SELECT id FROM events WHERE NOT id = 1 AND enabled = true OR score > 2.5"),
        Predicate::Or(
            Box::new(Predicate::And(
                Box::new(Predicate::Not(Box::new(comparison(
                    "id",
                    ComparisonOperator::Equal,
                    Value::Int64(1),
                )))),
                Box::new(comparison(
                    "enabled",
                    ComparisonOperator::Equal,
                    Value::Bool(true),
                )),
            )),
            Box::new(comparison(
                "score",
                ComparisonOperator::Greater,
                Value::Float64(2.5),
            )),
        )
    );

    assert_eq!(
        predicate("SELECT DISTINCT id FROM events WHERE NOT NOT (id = 1 OR NOT enabled = false)"),
        Predicate::Not(Box::new(Predicate::Not(Box::new(Predicate::Or(
            Box::new(comparison("id", ComparisonOperator::Equal, Value::Int64(1),)),
            Box::new(Predicate::Not(Box::new(comparison(
                "enabled",
                ComparisonOperator::Equal,
                Value::Bool(false),
            )))),
        )))))
    );
}

#[test]
fn executes_not_for_every_comparison_operator_and_physical_type() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (i Int64, f Float64, b Bool, s String); \
             INSERT INTO samples VALUES \
             (1, 1.5, false, 'alpha'), \
             (2, 2.5, true, 'beta'), \
             (3, 3.5, false, 'gamma');",
        )
        .expect("setup");

    let integer_cases = [
        ("=", vec![1, 3]),
        ("!=", vec![2]),
        ("<>", vec![2]),
        ("<", vec![2, 3]),
        ("<=", vec![3]),
        (">", vec![1, 2]),
        (">=", vec![1]),
    ];
    for (operator, expected) in integer_cases {
        assert_eq!(
            query(
                &mut database,
                &format!("SELECT i FROM samples WHERE NOT i {operator} 2"),
            )
            .rows,
            expected
                .into_iter()
                .map(|value| vec![Value::Int64(value)])
                .collect::<Vec<_>>(),
            "NOT i {operator} 2",
        );
    }

    let typed_cases = [
        (
            "SELECT f FROM samples WHERE NOT f < 2.5",
            vec![vec![Value::Float64(2.5)], vec![Value::Float64(3.5)]],
        ),
        (
            "SELECT b FROM samples WHERE NOT b = true",
            vec![vec![Value::Bool(false)], vec![Value::Bool(false)]],
        ),
        (
            "SELECT s FROM samples WHERE NOT s >= 'beta'",
            vec![vec![Value::String("alpha".to_owned())]],
        ),
    ];
    for (sql, expected) in typed_cases {
        assert_eq!(query(&mut database, sql).rows, expected, "{sql}");
    }
    assert_eq!(
        query(&mut database, "SELECT i FROM samples WHERE NOT 2 < i").rows,
        [vec![Value::Int64(1)], vec![Value::Int64(2)]],
        "NOT also inverts comparisons with a literal left operand",
    );
}

#[test]
fn executes_nested_and_chained_not_for_regular_and_distinct_queries() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE events (id Int64, enabled Bool, kind String); \
             INSERT INTO events VALUES \
             (1, false, 'keep'), (2, true, 'drop'), \
             (3, false, 'keep'), (4, true, 'keep'), (5, false, 'keep');",
        )
        .expect("setup");

    assert_eq!(
        query(
            &mut database,
            "SELECT id FROM events WHERE NOT id = 1 AND enabled = true OR id = 3",
        )
        .rows,
        [
            vec![Value::Int64(2)],
            vec![Value::Int64(3)],
            vec![Value::Int64(4)],
        ]
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT id FROM events WHERE NOT (enabled = true OR id >= 5)",
        )
        .rows,
        [vec![Value::Int64(1)], vec![Value::Int64(3)]]
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT id FROM events WHERE NOT NOT NOT id < 3",
        )
        .rows,
        [
            vec![Value::Int64(3)],
            vec![Value::Int64(4)],
            vec![Value::Int64(5)],
        ]
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT DISTINCT kind FROM events WHERE NOT (enabled = true OR id >= 5)",
        )
        .rows,
        [vec![Value::String("keep".to_owned())]]
    );
}

#[test]
fn preserves_not_as_a_column_operand_in_regular_and_distinct_queries() {
    for operator in ["=", "!=", "<>", "<", "<=", ">", ">="] {
        let parsed = predicate(&format!("SELECT not FROM samples WHERE not {operator} 1"));
        let Predicate::Comparison { left, .. } = parsed else {
            panic!("not must remain the left comparison operand for {operator}");
        };
        assert_eq!(left, Operand::Column("not".to_owned()), "{operator}");
    }

    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (not Int64); \
             INSERT INTO samples VALUES (1), (2), (1), (3);",
        )
        .expect("setup");

    assert_eq!(
        query(&mut database, "SELECT not FROM samples WHERE not = 1").rows,
        [vec![Value::Int64(1)], vec![Value::Int64(1)]]
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT DISTINCT not FROM samples WHERE not <> 2",
        )
        .rows,
        [vec![Value::Int64(1)], vec![Value::Int64(3)]]
    );
    assert_eq!(
        query(&mut database, "SELECT not FROM samples WHERE NOT not = 1").rows,
        [vec![Value::Int64(2)], vec![Value::Int64(3)]]
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT DISTINCT not FROM samples WHERE NOT not >= 2",
        )
        .rows,
        [vec![Value::Int64(1)]]
    );
}

#[test]
fn not_preserves_comparison_type_and_null_literal_rules() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE samples (value Int64); INSERT INTO samples VALUES (1);")
        .expect("setup");

    assert_eq!(
        database
            .execute("SELECT value FROM samples WHERE NOT value = '1'")
            .expect_err("NOT does not make mismatched operands comparable"),
        Error::TypeMismatch {
            context: "WHERE comparison".to_owned(),
            expected: DataType::Int64.to_string(),
            actual: DataType::String.to_string(),
        }
    );

    let statement = Statement::Select(Select {
        distinct: false,
        items: vec![SelectItem::Column {
            name: "value".to_owned(),
            alias: None,
        }],
        table: "samples".to_owned(),
        predicate: Some(Predicate::Not(Box::new(comparison(
            "value",
            ComparisonOperator::Equal,
            Value::Null(DataType::Int64),
        )))),
        group_by: Vec::new(),
        having: None,
        order_by: Vec::new(),
        limit: None,
    });
    assert_eq!(
        database.execute_statement(statement),
        Err(Error::InvalidQuery(
            "WHERE comparisons do not support NULL literals".to_owned()
        ))
    );
}

#[test]
fn rejects_malformed_unary_not_predicates() {
    for sql in [
        "SELECT id FROM events WHERE NOT",
        "SELECT id FROM events WHERE NOT AND id = 1",
        "SELECT id FROM events WHERE NOT OR id = 1",
        "SELECT id FROM events WHERE NOT ()",
        "SELECT id FROM events WHERE NOT (id = 1",
        "SELECT id FROM events WHERE NOT NOT",
        "SELECT DISTINCT id FROM events WHERE NOT LIMIT 1",
        "SELECT DISTINCT id FROM events WHERE NOT *",
    ] {
        assert!(
            matches!(parse(sql), Err(Error::Sql { .. })),
            "{sql:?} must return a typed SQL error",
        );
    }
}

fn unary_query(prefix: &str, not_count: usize) -> String {
    format!("{prefix}{}id = 1", "NOT ".repeat(not_count))
}

fn node_query(prefix: &str, not_count: usize) -> String {
    let first = format!("{}id = 1", "NOT ".repeat(not_count));
    format!(
        "{prefix}{}",
        std::iter::once(first)
            .chain(std::iter::repeat_n("id = 1".to_owned(), 127))
            .collect::<Vec<_>>()
            .join(" OR ")
    )
}

#[test]
fn unary_not_charges_exact_nesting_and_node_limits() {
    for prefix in [
        "SELECT id FROM events WHERE ",
        "SELECT DISTINCT id FROM events WHERE ",
    ] {
        parse(&unary_query(prefix, PREDICATE_DEPTH_CAP))
            .expect("exact unary nesting cap should parse");

        let too_deep = unary_query(prefix, PREDICATE_DEPTH_CAP + 1);
        assert_eq!(
            parse(&too_deep),
            Err(Error::Sql {
                position: prefix.len() + "NOT ".len() * (PREDICATE_DEPTH_CAP + 1),
                message: format!("predicate nesting exceeds limit of {PREDICATE_DEPTH_CAP}"),
            }),
        );

        let exact_nodes = node_query(prefix, 1);
        parse(&exact_nodes).unwrap_or_else(|error| {
            panic!("exact {PREDICATE_NODE_CAP}-node predicate must parse: {error}")
        });

        let too_many_nodes = node_query(prefix, 2);
        assert_eq!(
            parse(&too_many_nodes),
            Err(Error::Sql {
                position: too_many_nodes.len(),
                message: format!(
                    "predicate is too complex; maximum {PREDICATE_NODE_CAP} expression nodes"
                ),
            }),
        );
    }

    let mixed_exact = format!(
        "SELECT id FROM events WHERE {}{}id = 1{}",
        "NOT ".repeat(PREDICATE_DEPTH_CAP / 2),
        "(".repeat(PREDICATE_DEPTH_CAP / 2),
        ")".repeat(PREDICATE_DEPTH_CAP / 2),
    );
    parse(&mixed_exact).expect("NOT and parentheses share the exact nesting budget");

    let mixed_too_deep = format!(
        "SELECT id FROM events WHERE {}{}id = 1{}",
        "NOT ".repeat(PREDICATE_DEPTH_CAP / 2),
        "(".repeat(PREDICATE_DEPTH_CAP / 2 + 1),
        ")".repeat(PREDICATE_DEPTH_CAP / 2 + 1),
    );
    assert!(matches!(
        parse(&mixed_too_deep),
        Err(Error::Sql { message, .. })
            if message == format!("predicate nesting exceeds limit of {PREDICATE_DEPTH_CAP}")
    ));
}
