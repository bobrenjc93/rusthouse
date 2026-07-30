use rusthouse::plan::{LogicalOperator, PlanNode};
use rusthouse::sql::{self, Statement};
use rusthouse::{Database, QueryResult, StatementResult, Value};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    match database
        .execute(sql)
        .expect("SQL succeeds")
        .into_iter()
        .last()
        .expect("statement result")
    {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

fn plan_lines(result: QueryResult) -> Vec<String> {
    assert_eq!(result.columns.len(), 1);
    assert_eq!(result.columns[0].name, "plan");
    result
        .rows
        .into_iter()
        .map(|row| match row.as_slice() {
            [Value::String(line)] => line.clone(),
            _ => panic!("EXPLAIN rows contain one String"),
        })
        .collect()
}

fn input(node: &PlanNode) -> &PlanNode {
    match &node.operator {
        LogicalOperator::Scan { .. } => panic!("Scan has no input"),
        LogicalOperator::Filter { input, .. }
        | LogicalOperator::Aggregation { input, .. }
        | LogicalOperator::Projection { input, .. }
        | LogicalOperator::Sort { input, .. }
        | LogicalOperator::TopK { input, .. }
        | LogicalOperator::Limit { input, .. } => input,
    }
}

#[test]
fn planner_builds_resolved_analytical_pipeline() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE sales (region String, amount Int64, active Bool)")
        .expect("create table");
    let mut statements = sql::parse(
        "SELECT region, COUNT(*) AS n, SUM(amount) AS total \
         FROM sales WHERE active = true GROUP BY region \
         ORDER BY total DESC LIMIT 5",
    )
    .expect("parse SELECT");
    let Statement::Select(select) = statements.remove(0) else {
        panic!("expected SELECT")
    };

    let plan = database.plan(&select).expect("plan SELECT");
    let LogicalOperator::TopK {
        ordering, limit, ..
    } = &plan.root.operator
    else {
        panic!("expected TopK root")
    };
    assert_eq!(*limit, 5);
    assert_eq!(ordering[0].output, 2);
    assert_eq!(ordering[0].name, "total");

    let projection = input(&plan.root);
    assert!(matches!(
        projection.operator,
        LogicalOperator::Projection { .. }
    ));
    let aggregation = input(projection);
    let LogicalOperator::Aggregation {
        group_by,
        aggregates,
        ..
    } = &aggregation.operator
    else {
        panic!("expected Aggregation")
    };
    assert_eq!(group_by[0].index, 0);
    assert_eq!(group_by[0].name, "region");
    assert_eq!(
        aggregates[1].argument.as_ref().expect("SUM argument").index,
        1
    );
    assert!(matches!(
        input(aggregation).operator,
        LogicalOperator::Filter { .. }
    ));
    assert!(matches!(
        input(input(aggregation)).operator,
        LogicalOperator::Scan { .. }
    ));
}

#[test]
fn explain_is_deterministic_and_shows_top_k_shape() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE numbers (n Int64); INSERT INTO numbers VALUES (3), (1), (2)")
        .expect("setup");
    let sql = "EXPLAIN SELECT n FROM numbers WHERE n >= 2 ORDER BY n DESC LIMIT 1";

    let first = plan_lines(query(&mut database, sql));
    let second = plan_lines(query(&mut database, sql));
    assert_eq!(first, second);
    assert_eq!(
        first,
        [
            "TopK [order_by=[n DESC], limit=1]",
            "  Projection [n=n#0]",
            "    Filter [predicate=n#0 >= 2]",
            "      Scan [table=numbers, columns=[n:Int64]]",
        ]
    );
}

#[test]
fn explain_distinguishes_sort_and_limit_nodes() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE numbers (n Int64)")
        .expect("setup");

    let sorted = plan_lines(query(
        &mut database,
        "EXPLAIN SELECT n FROM numbers ORDER BY n",
    ));
    assert!(sorted[0].starts_with("Sort "));

    let limited = plan_lines(query(
        &mut database,
        "EXPLAIN SELECT n FROM numbers LIMIT 2",
    ));
    assert_eq!(limited[0], "Limit [limit=2]");
}

#[test]
fn explain_analyze_reports_each_operator_and_preserves_query_behavior() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE numbers (n Int64); INSERT INTO numbers VALUES (3), (1), (2)")
        .expect("setup");
    let select = "SELECT n FROM numbers WHERE n >= 2 ORDER BY n DESC LIMIT 1";
    let before = query(&mut database, select);

    let analyzed = plan_lines(query(&mut database, &format!("EXPLAIN ANALYZE {select}")));
    let expected = [("TopK", 1), ("Projection", 2), ("Filter", 2), ("Scan", 3)];
    assert_eq!(analyzed.len(), expected.len());
    for (line, (operator, rows)) in analyzed.iter().zip(expected) {
        assert!(line.trim_start().starts_with(operator), "{line}");
        assert!(line.ends_with("ns]"), "missing timing: {line}");
        assert!(line.contains(&format!("[rows={rows}, elapsed=")), "{line}");
        let _elapsed = line
            .rsplit_once("elapsed=")
            .expect("elapsed metric")
            .1
            .strip_suffix("ns]")
            .expect("nanosecond unit")
            .parse::<u128>()
            .expect("numeric timing");
    }

    let after = query(&mut database, select);
    assert_eq!(after, before);
    assert_eq!(after.rows, vec![vec![Value::Int64(3)]]);
}
