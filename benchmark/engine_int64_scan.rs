use std::fmt::Write as _;
use std::hint::black_box;
use std::time::{Duration, Instant};

use rusthouse::storage::{Column, Int64Column};
use rusthouse::{Database, StatementResult, Value};

const ROWS: usize = 100_000;
const REPETITIONS: usize = 64;
const SAMPLES: usize = 7;
const QUERY: &str = "SELECT SUM(value) AS total FROM scan_data;";

fn main() {
    let cases = [
        ("constant", vec![17_i64; ROWS]),
        (
            "delta",
            (0..ROWS).map(|index| index as i64).collect::<Vec<_>>(),
        ),
        (
            "raw",
            (0..ROWS)
                .map(|index| {
                    if index.is_multiple_of(2) {
                        i64::MAX
                    } else {
                        i64::MIN
                    }
                })
                .collect::<Vec<_>>(),
        ),
    ];

    println!(
        "encoding,logical_bytes,encoded_bytes,indexed_ns_per_value,sql_ns_per_value,sql_indexed_ratio"
    );
    for (name, raw) in cases {
        let mut database = setup_database(&raw);
        let expected = raw
            .iter()
            .try_fold(0_i64, |sum, value| sum.checked_add(*value))
            .expect("benchmark sums do not overflow");
        assert_eq!(query_sum(&mut database), expected, "{name} SQL parity");

        let table = database
            .catalog()
            .table("scan_data")
            .expect("benchmark table exists");
        let Column::Int64(column) = &table.columns()[0] else {
            unreachable!("benchmark column is Int64")
        };
        let stats = column.storage_stats();
        indexed_sum(black_box(column), 2);
        let indexed_duration = median_duration(|| indexed_sum(black_box(column), REPETITIONS));

        query_sum(&mut database);
        let sql_duration = median_duration(|| query_sum_repeated(&mut database, REPETITIONS));
        let values_scanned = (ROWS * REPETITIONS) as f64;
        let indexed_ns = indexed_duration.as_nanos() as f64 / values_scanned;
        let sql_ns = sql_duration.as_nanos() as f64 / values_scanned;

        println!(
            "{name},{},{},{indexed_ns:.3},{sql_ns:.3},{:.3}",
            stats.logical_bytes,
            stats.encoded_bytes,
            sql_ns / indexed_ns
        );
    }
}

fn setup_database(values: &[i64]) -> Database {
    let mut sql =
        String::from("CREATE TABLE scan_data (value Int64); INSERT INTO scan_data VALUES ");
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            sql.push(',');
        }
        write!(sql, "({value})").expect("writing to String cannot fail");
    }
    sql.push(';');

    let mut database = Database::new();
    database.execute(&sql).expect("benchmark setup succeeds");
    database
}

fn indexed_sum(values: &Int64Column, repetitions: usize) -> i64 {
    let mut digest = 0_i64;
    for _ in 0..repetitions {
        let sum = (0..values.len())
            .try_fold(0_i64, |sum, row| sum.checked_add(values.get(row)?))
            .expect("benchmark sums do not overflow");
        digest = digest.wrapping_add(black_box(sum));
    }
    digest
}

fn query_sum_repeated(database: &mut Database, repetitions: usize) -> i64 {
    let mut digest = 0_i64;
    for _ in 0..repetitions {
        digest = digest.wrapping_add(black_box(query_sum(database)));
    }
    digest
}

fn query_sum(database: &mut Database) -> i64 {
    let StatementResult::Query(result) = database
        .execute(QUERY)
        .expect("benchmark query succeeds")
        .pop()
        .expect("query result exists")
    else {
        unreachable!("SELECT returns a query result")
    };
    let Value::Int64(sum) = result.rows[0][0] else {
        unreachable!("SUM(Int64) returns Int64")
    };
    sum
}

fn median_duration(mut scan: impl FnMut() -> i64) -> Duration {
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let started = Instant::now();
        black_box(scan());
        samples.push(started.elapsed());
    }
    samples.sort_unstable();
    samples[SAMPLES / 2]
}
