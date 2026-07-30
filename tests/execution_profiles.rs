use rusthouse::storage::BLOCK_SIZE;
use rusthouse::{Database, QueryProfile, StatementResult, Value};

fn profile(database: &mut Database, sql: &str) -> (StatementResult, QueryProfile) {
    let execution = database.execute_profiled(sql).expect("query succeeds");
    assert_eq!(execution.results.len(), 1);
    assert_eq!(execution.profiles.len(), 1);
    (
        execution.results.into_iter().next().expect("query result"),
        execution
            .profiles
            .into_iter()
            .next()
            .expect("query profile"),
    )
}

#[test]
fn filter_profile_counts_scan_work_and_matches() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE numbers (n Int64, active Bool);
             INSERT INTO numbers VALUES
                (1, true), (2, false), (3, true), (4, false), (5, true);",
        )
        .expect("setup succeeds");

    let (result, profile) = profile(
        &mut database,
        "SELECT n FROM numbers WHERE n >= 2 AND active = true;",
    );

    assert_eq!(profile.rows_read, 5);
    assert_eq!(profile.blocks_read, 1);
    assert_eq!(profile.blocks_pruned, 0);
    assert_eq!(profile.predicate_matches, 2);
    assert_eq!(profile.groups_created, 0);
    assert_eq!(profile.sort_inputs, 0);
    assert_eq!(profile.output_rows, 2);
    let StatementResult::Query(result) = result else {
        panic!("expected query result");
    };
    assert_eq!(
        result.rows,
        vec![vec![Value::Int64(3)], vec![Value::Int64(5)]]
    );
}

#[test]
fn grouping_profile_counts_created_groups() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE sales (region String, amount Int64);
             INSERT INTO sales VALUES
                ('west', 10), ('east', 4), ('west', 7), ('north', 2);",
        )
        .expect("setup succeeds");

    let (_, profile) = profile(
        &mut database,
        "SELECT region, SUM(amount) AS total FROM sales GROUP BY region;",
    );

    assert_eq!(profile.rows_read, 4);
    assert_eq!(profile.blocks_read, 1);
    assert_eq!(profile.blocks_pruned, 0);
    assert_eq!(profile.predicate_matches, 4);
    assert_eq!(profile.groups_created, 3);
    assert_eq!(profile.sort_inputs, 0);
    assert_eq!(profile.output_rows, 3);
}

#[test]
fn top_k_profile_counts_every_sort_input_before_limit() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE ranked (id Int64, score Int64);
             INSERT INTO ranked VALUES (1, 4), (2, 9), (3, 1), (4, 7), (5, 3);",
        )
        .expect("setup succeeds");

    let (_, profile) = profile(
        &mut database,
        "SELECT id, score FROM ranked ORDER BY score DESC LIMIT 2;",
    );

    assert_eq!(profile.rows_read, 5);
    assert_eq!(profile.blocks_read, 1);
    assert_eq!(profile.blocks_pruned, 0);
    assert_eq!(profile.predicate_matches, 5);
    assert_eq!(profile.groups_created, 0);
    assert_eq!(profile.sort_inputs, 5);
    assert_eq!(profile.output_rows, 2);
}

#[test]
fn skip_indexes_prune_whole_blocks_before_rows_are_read() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE indexed (n Int64);")
        .expect("create succeeds");
    let first_matching = BLOCK_SIZE * 2;
    let values = (0..first_matching + 3)
        .map(|value| format!("({value})"))
        .collect::<Vec<_>>()
        .join(",");
    database
        .execute(&format!("INSERT INTO indexed VALUES {values};"))
        .expect("insert succeeds");

    let (result, profile) = profile(
        &mut database,
        &format!("SELECT n FROM indexed WHERE n >= {first_matching} ORDER BY n;"),
    );

    assert_eq!(profile.rows_read, 3);
    assert_eq!(profile.blocks_read, 1);
    assert_eq!(profile.blocks_pruned, 2);
    assert_eq!(profile.predicate_matches, 3);
    assert_eq!(profile.groups_created, 0);
    assert_eq!(profile.sort_inputs, 3);
    assert_eq!(profile.output_rows, 3);
    let StatementResult::Query(result) = result else {
        panic!("expected query result");
    };
    assert_eq!(
        result.rows,
        vec![
            vec![Value::Int64(first_matching as i64)],
            vec![Value::Int64(first_matching as i64 + 1)],
            vec![Value::Int64(first_matching as i64 + 2)],
        ]
    );
}
