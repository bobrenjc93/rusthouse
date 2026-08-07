use rusthouse::batch::engine::{Database, QueryResult, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{ComparisonOperator, Operand, Predicate, Statement, parse};
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

fn comparison(column: &str, operator: ComparisonOperator, value: Value) -> Predicate {
    Predicate::Comparison {
        left: Operand::Column(column.to_owned()),
        operator,
        right: Operand::Literal(value),
    }
}

fn between(column: &str, lower: Value, upper: Value) -> Predicate {
    Predicate::And(
        Box::new(comparison(
            column,
            ComparisonOperator::GreaterOrEqual,
            lower,
        )),
        Box::new(comparison(column, ComparisonOperator::LessOrEqual, upper)),
    )
}

#[test]
fn lowers_between_as_one_atom_in_regular_and_distinct_queries() {
    let expected = Predicate::Or(
        Box::new(Predicate::And(
            Box::new(Predicate::Not(Box::new(between(
                "id",
                Value::Int64(2),
                Value::Int64(4),
            )))),
            Box::new(comparison(
                "active",
                ComparisonOperator::Equal,
                Value::Bool(true),
            )),
        )),
        Box::new(between("id", Value::Int64(6), Value::Int64(6))),
    );

    for projection in ["id", "DISTINCT id"] {
        assert_eq!(
            predicate(&format!(
                "SELECT {projection} FROM events WHERE \
                 NOT id BETWEEN 2 AND 4 AND active = true OR id BETWEEN 6 AND 6"
            )),
            expected,
            "{projection}",
        );
    }

    assert_eq!(
        predicate("SELECT not FROM events WHERE not BETWEEN 1 AND 2"),
        between("not", Value::Int64(1), Value::Int64(2)),
        "a column named 'not' remains an operand before BETWEEN",
    );
}

#[test]
fn unary_not_preserves_between_as_a_column_in_regular_and_distinct_queries() {
    let expected = Predicate::Not(Box::new(comparison(
        "between",
        ComparisonOperator::Equal,
        Value::Int64(1),
    )));
    for projection in ["between", "DISTINCT between"] {
        assert_eq!(
            predicate(&format!(
                "SELECT {projection} FROM samples WHERE NOT between = 1"
            )),
            expected,
            "{projection}",
        );
    }

    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (between Int64); \
             INSERT INTO samples VALUES (1), (2), (1), (3);",
        )
        .expect("setup");

    assert_eq!(
        query(
            &mut database,
            "SELECT between FROM samples WHERE NOT between = 1"
        )
        .rows,
        [vec![Value::Int64(2)], vec![Value::Int64(3)]],
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT DISTINCT between FROM samples WHERE NOT between = 1"
        )
        .rows,
        [vec![Value::Int64(2)], vec![Value::Int64(3)]],
    );
}

#[test]
fn executes_inclusive_between_for_every_physical_type() {
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
        query(
            &mut database,
            "SELECT i FROM samples WHERE i BETWEEN 2 AND 3"
        )
        .rows,
        [vec![Value::Int64(2)], vec![Value::Int64(3)]],
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT f FROM samples WHERE f BETWEEN 2.5 AND 4",
        )
        .rows,
        [vec![Value::Float64(2.5)], vec![Value::Float64(4.0)]],
        "mixed numeric bounds retain comparison semantics",
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT b FROM samples WHERE b BETWEEN true AND true",
        )
        .rows,
        [vec![Value::Bool(true)], vec![Value::Bool(true)]],
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT s FROM samples WHERE s BETWEEN 'beta' AND 'gamma'",
        )
        .rows,
        [
            vec![Value::String("beta".to_owned())],
            vec![Value::String("gamma".to_owned())],
        ],
    );
}

#[test]
fn not_and_or_precedence_executes_in_regular_and_distinct_queries() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE events (id Int64, active Bool); \
             INSERT INTO events VALUES \
             (1, true), (2, true), (3, false), (4, true), (5, false), (6, false);",
        )
        .expect("setup");

    assert_eq!(
        query(
            &mut database,
            "SELECT id FROM events WHERE \
             NOT id BETWEEN 2 AND 4 AND active = true OR id BETWEEN 6 AND 6",
        )
        .rows,
        [vec![Value::Int64(1)], vec![Value::Int64(6)]],
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT DISTINCT active FROM events WHERE \
             NOT id BETWEEN 2 AND 4 AND active = true OR id BETWEEN 6 AND 6 \
             ORDER BY active",
        )
        .rows,
        [vec![Value::Bool(false)], vec![Value::Bool(true)]],
    );
}

#[test]
fn reversed_bounds_match_no_values_of_every_physical_type() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (i Int64, f Float64, b Bool, s String); \
             INSERT INTO samples VALUES (2, 2.5, false, 'beta'), (3, 4.0, true, 'gamma');",
        )
        .expect("setup");

    for sql in [
        "SELECT i FROM samples WHERE i BETWEEN 3 AND 2",
        "SELECT f FROM samples WHERE f BETWEEN 4.0 AND 2.5",
        "SELECT b FROM samples WHERE b BETWEEN true AND false",
        "SELECT s FROM samples WHERE s BETWEEN 'gamma' AND 'beta'",
    ] {
        assert!(query(&mut database, sql).rows.is_empty(), "{sql}");
    }
}

#[test]
fn between_retains_comparison_type_errors_for_each_bound() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE samples (i Int64); INSERT INTO samples VALUES (1);")
        .expect("setup");

    for sql in [
        "SELECT i FROM samples WHERE i BETWEEN '0' AND 2",
        "SELECT i FROM samples WHERE i BETWEEN 0 AND '2'",
    ] {
        assert_eq!(
            database.execute(sql),
            Err(Error::TypeMismatch {
                context: "WHERE comparison".to_owned(),
                expected: DataType::Int64.to_string(),
                actual: DataType::String.to_string(),
            }),
            "{sql}",
        );
    }
}

#[test]
fn rejects_every_malformed_between_shape_in_regular_and_distinct_queries() {
    for projection in ["i", "DISTINCT i"] {
        for malformed in [
            "i BETWEEN",
            "i BETWEEN AND 2",
            "i BETWEEN 1",
            "i BETWEEN 1 OR i = 2",
            "i BETWEEN 1 AND",
            "i BETWEEN other AND 2",
            "i BETWEEN 1 AND other",
            "1 BETWEEN 0 AND 2",
            "i BETWEEN (1) AND 2",
            "i NOT BETWEEN 1 AND 2",
            "i BETWEEN 1 AND 2 3",
            "i BETWEEN 1 AND 2 AND",
        ] {
            let sql = format!("SELECT {projection} FROM samples WHERE {malformed}");
            assert!(
                matches!(parse(&sql), Err(Error::Sql { .. })),
                "{sql:?} must return a typed SQL error",
            );
        }
    }
}

fn between_node_query(prefix: &str, not_count: usize) -> String {
    let first = format!("{}id BETWEEN 1 AND 2", "NOT ".repeat(not_count));
    format!(
        "{prefix}{}",
        std::iter::once(first)
            .chain(std::iter::repeat_n("id BETWEEN 1 AND 2".to_owned(), 63))
            .collect::<Vec<_>>()
            .join(" OR ")
    )
}

#[test]
fn between_charges_both_comparisons_and_internal_and_against_node_limit() {
    for prefix in [
        "SELECT id FROM events WHERE ",
        "SELECT DISTINCT id FROM events WHERE ",
    ] {
        let exact = between_node_query(prefix, 1);
        parse(&exact).unwrap_or_else(|error| {
            panic!("exact {PREDICATE_NODE_CAP}-node predicate must parse: {error}")
        });

        let too_many = between_node_query(prefix, 2);
        assert_eq!(
            parse(&too_many),
            Err(Error::Sql {
                position: too_many.len(),
                message: format!(
                    "predicate is too complex; maximum {PREDICATE_NODE_CAP} expression nodes"
                ),
            }),
        );
    }
}
