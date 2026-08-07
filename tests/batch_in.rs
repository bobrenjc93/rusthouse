use std::sync::Arc;

use rusthouse::batch::engine::{Database, QueryResult, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{
    BatchSqlLimits, ComparisonOperator, Operand, Predicate, Statement, parse, parse_with_limits,
};
use rusthouse::batch::value::{DataType, Value};

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

fn comparison(column: &str, value: Value) -> Predicate {
    Predicate::Comparison {
        left: Operand::Column(column.to_owned()),
        operator: ComparisonOperator::Equal,
        right: Operand::Literal(value),
    }
}

fn collect_in_literals<'a>(predicate: &'a Predicate, output: &mut Vec<&'a Value>) -> usize {
    match predicate {
        Predicate::Comparison {
            left: Operand::SharedColumn(_),
            operator: ComparisonOperator::Equal,
            right: Operand::Literal(value),
        } => {
            output.push(value);
            1
        }
        Predicate::Or(left, right) => {
            let left_leaves = collect_in_literals(left, output);
            let right_leaves = collect_in_literals(right, output);
            assert!(
                left_leaves.abs_diff(right_leaves) <= 1,
                "each lowered OR split must be balanced",
            );
            left_leaves + right_leaves
        }
        unexpected => panic!("unexpected IN lowering node: {unexpected:?}"),
    }
}

fn collect_shared_columns<'a>(predicate: &'a Predicate, output: &mut Vec<&'a Arc<str>>) {
    match predicate {
        Predicate::Comparison {
            left: Operand::SharedColumn(column),
            operator: ComparisonOperator::Equal,
            right: Operand::Literal(_),
        } => output.push(column),
        Predicate::Or(left, right) => {
            collect_shared_columns(left, output);
            collect_shared_columns(right, output);
        }
        unexpected => panic!("unexpected IN lowering node: {unexpected:?}"),
    }
}

#[test]
fn lowers_nonempty_in_lists_to_balanced_equality_or_trees() {
    let expected = Predicate::Or(
        Box::new(Predicate::Or(
            Box::new(comparison("id", Value::Int64(1))),
            Box::new(comparison("id", Value::Int64(2))),
        )),
        Box::new(Predicate::Or(
            Box::new(comparison("id", Value::Int64(3))),
            Box::new(comparison("id", Value::Int64(4))),
        )),
    );

    for projection in ["id", "DISTINCT id"] {
        assert_eq!(
            predicate(&format!(
                "SELECT {projection} FROM events WHERE id IN (1, 2, 3, 4)"
            )),
            expected,
            "{projection}",
        );
    }

    assert_eq!(
        predicate("SELECT id FROM events WHERE id IN (7)"),
        comparison("id", Value::Int64(7)),
        "a one-literal IN lowers directly to equality",
    );

    let sql = format!(
        "SELECT id FROM events WHERE id IN ({})",
        (0..127)
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let lowered = predicate(&sql);
    let mut literals = Vec::new();
    assert_eq!(collect_in_literals(&lowered, &mut literals), 127);
    assert_eq!(
        literals.into_iter().cloned().collect::<Vec<_>>(),
        (0..127).map(Value::Int64).collect::<Vec<_>>(),
        "balanced lowering preserves literal order",
    );
}

#[test]
fn maximum_in_list_retains_one_long_column_identifier_allocation() {
    const LONG_IDENTIFIER_BYTES: usize = 256 * 1024;
    const MAX_IN_LITERALS: usize = 128;

    let column = format!("c{}", "x".repeat(LONG_IDENTIFIER_BYTES - 1));
    let sql = format!(
        "SELECT id FROM events WHERE {column} IN ({})",
        (0..MAX_IN_LITERALS)
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let lowered = predicate(&sql);
    let mut retained_columns = Vec::new();
    collect_shared_columns(&lowered, &mut retained_columns);

    assert_eq!(retained_columns.len(), MAX_IN_LITERALS);
    assert_eq!(retained_columns[0].len(), LONG_IDENTIFIER_BYTES);
    assert!(
        retained_columns
            .iter()
            .all(|candidate| Arc::ptr_eq(retained_columns[0], candidate)),
        "all leaves at the predicate-node boundary must share one identifier allocation",
    );
}

#[test]
fn executes_in_for_all_types_and_numeric_comparison_compatibility() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (i Int64, f Float64, b Bool, s String); \
             INSERT INTO samples VALUES \
             (1, 1.0, false, 'alpha'), \
             (2, 2.5, true, 'beta'), \
             (3, 4.0, true, 'gamma'), \
             (4, 5.0, false, 'omega');",
        )
        .expect("setup");

    assert_eq!(
        query(&mut database, "SELECT i FROM samples WHERE i IN (1, 3)").rows,
        [vec![Value::Int64(1)], vec![Value::Int64(3)]],
    );
    assert_eq!(
        query(&mut database, "SELECT f FROM samples WHERE f IN (2.5, 4)").rows,
        [vec![Value::Float64(2.5)], vec![Value::Float64(4.0)]],
        "IN retains the comparison engine's Int64/Float64 compatibility",
    );
    assert_eq!(
        query(&mut database, "SELECT b FROM samples WHERE b IN (false)").rows,
        [vec![Value::Bool(false)], vec![Value::Bool(false)]],
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT DISTINCT s FROM samples WHERE s IN ('omega', 'beta', 'omega') ORDER BY s",
        )
        .rows,
        [
            vec![Value::String("beta".to_owned())],
            vec![Value::String("omega".to_owned())],
        ],
    );
}

#[test]
fn unary_not_and_boolean_precedence_apply_to_in_atoms() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE events (id Int64, active Bool); \
             INSERT INTO events VALUES \
             (1, true), (2, true), (3, false), (4, true), (5, false), (5, false);",
        )
        .expect("setup");

    let where_clause = "NOT id IN (2, 3) AND active = true OR id IN (5)";
    assert_eq!(
        query(
            &mut database,
            &format!("SELECT id FROM events WHERE {where_clause}"),
        )
        .rows,
        [
            vec![Value::Int64(1)],
            vec![Value::Int64(4)],
            vec![Value::Int64(5)],
            vec![Value::Int64(5)],
        ],
    );
    assert_eq!(
        query(
            &mut database,
            &format!("SELECT DISTINCT active FROM events WHERE {where_clause} ORDER BY active"),
        )
        .rows,
        [vec![Value::Bool(false)], vec![Value::Bool(true)]],
    );

    assert!(matches!(
        predicate("SELECT not FROM events WHERE not IN (1, 2)"),
        Predicate::Or(_, _)
    ));
    assert_eq!(
        predicate("SELECT in FROM events WHERE NOT in = 1"),
        Predicate::Not(Box::new(comparison("in", Value::Int64(1)))),
        "IN and NOT remain usable as contextual column names",
    );
}

#[test]
fn rejects_empty_malformed_and_nonliteral_in_lists() {
    for projection in ["i", "DISTINCT i"] {
        for malformed in [
            "i IN ()",
            "i IN 1",
            "i IN (1",
            "i IN (,1)",
            "i IN (1,)",
            "i IN (1,,2)",
            "i IN (other)",
            "i IN ((1))",
            "i IN (NULL)",
            "i IN (1 + 2)",
            "1 IN (1, 2)",
            "i NOT IN (1, 2)",
            "i IN (1) i = 2",
        ] {
            let sql = format!("SELECT {projection} FROM samples WHERE {malformed}");
            assert!(
                matches!(parse(&sql), Err(Error::Sql { .. })),
                "{sql:?} must return a typed SQL error",
            );
        }
    }
}

#[test]
fn mixed_lists_retain_typed_comparison_errors() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (i Int64, f Float64, b Bool, s String); \
             INSERT INTO samples VALUES (1, 1.5, true, 'one');",
        )
        .expect("setup");

    for (sql, expected, actual) in [
        (
            "SELECT i FROM samples WHERE i IN (1, 'one')",
            DataType::Int64,
            DataType::String,
        ),
        (
            "SELECT f FROM samples WHERE f IN (1.5, false)",
            DataType::Float64,
            DataType::Bool,
        ),
        (
            "SELECT b FROM samples WHERE b IN (true, 1)",
            DataType::Bool,
            DataType::Int64,
        ),
        (
            "SELECT s FROM samples WHERE s IN ('one', 1)",
            DataType::String,
            DataType::Int64,
        ),
    ] {
        assert_eq!(
            database.execute(sql),
            Err(Error::TypeMismatch {
                context: "WHERE comparison".to_owned(),
                expected: expected.to_string(),
                actual: actual.to_string(),
            }),
            "{sql}",
        );
    }
}

fn in_query(prefix: &str, literals: usize, not_count: usize) -> String {
    format!(
        "{prefix}{}id IN ({})",
        "NOT ".repeat(not_count),
        (0..literals)
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

#[test]
fn in_literals_and_every_expanded_node_obey_exact_ast_limits() {
    for prefix in [
        "SELECT id FROM events WHERE ",
        "SELECT DISTINCT id FROM events WHERE ",
    ] {
        let exact_nodes = in_query(prefix, 128, 1);
        parse(&exact_nodes).unwrap_or_else(|error| {
            panic!("exact {PREDICATE_NODE_CAP}-node predicate must parse: {error}")
        });

        let too_many_nodes = in_query(prefix, 129, 0);
        assert!(matches!(
            parse(&too_many_nodes),
            Err(Error::Sql { message, .. })
                if message == format!(
                    "predicate is too complex; maximum {PREDICATE_NODE_CAP} expression nodes"
                )
        ));

        let three_literals = in_query(prefix, 3, 0);
        parse_with_limits(
            &three_literals,
            BatchSqlLimits {
                max_ast_list_items: 4,
                ..BatchSqlLimits::default()
            },
        )
        .expect("one projection plus three IN literals fit exactly");
        assert_eq!(
            parse_with_limits(
                &three_literals,
                BatchSqlLimits {
                    max_ast_list_items: 3,
                    ..BatchSqlLimits::default()
                },
            ),
            Err(Error::ResourceLimitExceeded {
                resource: "SQL AST list items",
                actual: 4,
                max: 3,
            }),
        );
    }

    let cumulative = "SELECT id FROM events WHERE id IN (1); \
                      SELECT id FROM events WHERE id IN (2)";
    assert_eq!(
        parse_with_limits(
            cumulative,
            BatchSqlLimits {
                max_ast_list_items: 3,
                ..BatchSqlLimits::default()
            },
        ),
        Err(Error::ResourceLimitExceeded {
            resource: "SQL AST list items",
            actual: 4,
            max: 3,
        }),
        "IN literal charges accumulate across the parsed batch",
    );
}
