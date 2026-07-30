use std::fmt::Write as _;

use rusthouse::storage::ROW_GROUP_SIZE;
use rusthouse::{Database, StatementResult, Value};

fn query(database: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    let results = database.execute(sql).expect("query succeeds");
    match results.into_iter().last().expect("statement result") {
        StatementResult::Query(result) => result.rows,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

fn insert_range(database: &mut Database, start: usize, end: usize) {
    let mut sql = String::from("INSERT INTO events VALUES ");
    for row in start..end {
        if row > start {
            sql.push(',');
        }
        let flag = row >= ROW_GROUP_SIZE * 2;
        let measure = match row {
            0 => i64::MIN,
            row if row == ROW_GROUP_SIZE => 9_007_199_254_740_993,
            row if row == ROW_GROUP_SIZE * 2 => i64::MAX,
            _ => row as i64,
        };
        write!(sql, "({row},{measure},{flag},{row})").expect("write SQL");
    }
    sql.push(';');
    database.execute(&sql).expect("insert succeeds");
}

fn setup_appended_groups() -> Database {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE events (
                id Int64, measure Int64, flag Bool, peer Int64
             );",
        )
        .expect("create succeeds");
    insert_range(&mut database, 0, ROW_GROUP_SIZE);
    insert_range(&mut database, ROW_GROUP_SIZE, ROW_GROUP_SIZE * 2);
    insert_range(&mut database, ROW_GROUP_SIZE * 2, ROW_GROUP_SIZE * 2 + 3);
    database
}

#[test]
fn pruning_tracks_groups_appended_at_and_after_boundaries() {
    let mut database = setup_appended_groups();

    let boundary = query(
        &mut database,
        &format!("SELECT id FROM events WHERE id = {ROW_GROUP_SIZE}"),
    );
    assert_eq!(boundary, vec![vec![Value::Int64(ROW_GROUP_SIZE as i64)]]);
    assert_eq!(database.last_scan_stats().row_groups_total, 3);
    assert_eq!(database.last_scan_stats().row_groups_scanned, 1);
    assert_eq!(database.last_scan_stats().row_groups_pruned, 2);
    assert_eq!(database.last_scan_stats().rows_examined, ROW_GROUP_SIZE);

    let appended = query(
        &mut database,
        &format!(
            "SELECT id FROM events WHERE id = {}",
            ROW_GROUP_SIZE * 2 + 2
        ),
    );
    assert_eq!(
        appended,
        vec![vec![Value::Int64((ROW_GROUP_SIZE * 2 + 2) as i64)]]
    );
    assert_eq!(database.last_scan_stats().row_groups_scanned, 1);
    assert_eq!(database.last_scan_stats().rows_examined, 3);
}

#[test]
fn exact_mixed_numeric_extrema_do_not_create_false_negatives() {
    let mut database = setup_appended_groups();

    let rows = query(
        &mut database,
        "SELECT id, measure FROM events
         WHERE measure > 9007199254740992.0
         ORDER BY id",
    );
    assert_eq!(
        rows,
        vec![
            vec![
                Value::Int64(ROW_GROUP_SIZE as i64),
                Value::Int64(9_007_199_254_740_993),
            ],
            vec![
                Value::Int64((ROW_GROUP_SIZE * 2) as i64),
                Value::Int64(i64::MAX),
            ],
        ]
    );
    assert_eq!(database.last_scan_stats().row_groups_scanned, 2);
    assert_eq!(database.last_scan_stats().row_groups_pruned, 1);

    let literal_on_left = query(
        &mut database,
        &format!(
            "SELECT id FROM events WHERE {} <= id ORDER BY id",
            ROW_GROUP_SIZE * 2
        ),
    );
    assert_eq!(literal_on_left.len(), 3);
    assert_eq!(database.last_scan_stats().row_groups_scanned, 1);
    assert_eq!(database.last_scan_stats().rows_examined, 3);
}

#[test]
fn compound_checks_are_conservative_and_unsupported_comparisons_scan_all_groups() {
    let mut database = setup_appended_groups();
    let last_group = ROW_GROUP_SIZE * 2;

    let rows = query(
        &mut database,
        &format!(
            "SELECT id FROM events
             WHERE (id = 5 AND flag = false)
                OR (id >= {last_group} AND flag = true)
             ORDER BY id"
        ),
    );
    assert_eq!(
        rows,
        vec![
            vec![Value::Int64(5)],
            vec![Value::Int64(last_group as i64)],
            vec![Value::Int64(last_group as i64 + 1)],
            vec![Value::Int64(last_group as i64 + 2)],
        ]
    );
    assert_eq!(database.last_scan_stats().row_groups_scanned, 2);
    assert_eq!(database.last_scan_stats().row_groups_pruned, 1);

    let all = query(
        &mut database,
        "SELECT id FROM events WHERE id = peer ORDER BY id",
    );
    assert_eq!(all.len(), ROW_GROUP_SIZE * 2 + 3);
    assert_eq!(database.last_scan_stats().row_groups_scanned, 3);
    assert_eq!(database.last_scan_stats().row_groups_pruned, 0);
    assert_eq!(
        database.last_scan_stats().rows_examined,
        ROW_GROUP_SIZE * 2 + 3
    );
}
