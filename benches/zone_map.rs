use std::fmt::Write as _;
use std::hint::black_box;
use std::time::{Duration, Instant};

use rusthouse::storage::ROW_GROUP_SIZE;
use rusthouse::{Database, ScanStats, StatementResult, Value};

const ROWS: usize = 1_000_000;
const INSERT_BATCH_ROWS: usize = 10_000;

fn main() {
    let mut database = million_row_database();
    let point = measure(
        &mut database,
        "SELECT COUNT(*) AS matched FROM measurements WHERE id = 500000;",
        1_000,
    );
    let compound = measure(
        &mut database,
        "SELECT COUNT(*) AS matched FROM measurements
         WHERE id = 500000 OR (id >= 999000 AND flag = true);",
        1_000,
    );
    let full = measure(
        &mut database,
        "SELECT COUNT(*) AS matched FROM measurements;",
        20,
    );

    println!("one-million-row in-memory scan measurement");
    print_measurement("point", point);
    print_measurement("compound", compound);
    print_measurement("full", full);
}

#[derive(Debug, Clone, Copy)]
struct Measurement {
    mean: Duration,
    stats: ScanStats,
}

fn million_row_database() -> Database {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE measurements (id Int64, flag Bool);")
        .expect("create benchmark table");

    for start in (0..ROWS).step_by(INSERT_BATCH_ROWS) {
        let end = (start + INSERT_BATCH_ROWS).min(ROWS);
        let mut insert = String::with_capacity((end - start) * 16);
        insert.push_str("INSERT INTO measurements VALUES ");
        for row in start..end {
            if row > start {
                insert.push(',');
            }
            let flag = (row / ROW_GROUP_SIZE).is_multiple_of(2);
            write!(insert, "({row},{flag})").expect("write insert SQL");
        }
        insert.push(';');
        database.execute(&insert).expect("insert benchmark rows");
    }

    database
}

fn measure(database: &mut Database, sql: &str, iterations: u32) -> Measurement {
    let start = Instant::now();
    for _ in 0..iterations {
        let results = black_box(database.execute(black_box(sql)).expect("benchmark query"));
        let StatementResult::Query(result) = results.last().expect("query result") else {
            panic!("expected query result");
        };
        assert!(
            matches!(result.rows.as_slice(), [row] if matches!(row.as_slice(), [Value::Int64(_)]))
        );
        black_box(result);
    }
    let elapsed = start.elapsed();
    Measurement {
        mean: elapsed / iterations,
        stats: database.last_scan_stats(),
    }
}

fn print_measurement(name: &str, measurement: Measurement) {
    println!(
        "{name:8} {:>10.3} us/query; groups {:>3}/{:>3}; rows examined {:>7}",
        measurement.mean.as_secs_f64() * 1_000_000.0,
        measurement.stats.row_groups_scanned,
        measurement.stats.row_groups_total,
        measurement.stats.rows_examined,
    );
}
