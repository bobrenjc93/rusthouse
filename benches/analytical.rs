use std::fmt::Write;

use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
use rusthouse::{Engine, StatementResult};

const ROWS: usize = 50_000;
const INSERT_BATCH_ROWS: usize = 5_000;
const QUERY: &str = "
    SELECT category AS bucket,
           COUNT(*) AS rows,
           SUM(value) AS total,
           MIN(value) AS low,
           MAX(value) AS high,
           AVG(value) AS mean
    FROM facts
    WHERE active AND value >= -2500
    GROUP BY category
    HAVING COUNT(*) >= 1
    ORDER BY total DESC, bucket ASC
    LIMIT 20
";

fn generated_engine() -> Engine {
    let mut engine = Engine::default();
    engine
        .execute("CREATE TABLE facts (category String, value Int64, active Bool)")
        .unwrap();

    for batch_start in (0..ROWS).step_by(INSERT_BATCH_ROWS) {
        let batch_end = (batch_start + INSERT_BATCH_ROWS).min(ROWS);
        let mut values = String::with_capacity((batch_end - batch_start) * 24);
        for row in batch_start..batch_end {
            if row > batch_start {
                values.push(',');
            }
            let category = row % 32;
            let value = ((row * 37) % 10_000) as i64 - 5_000;
            let active = row % 3 != 0;
            write!(values, "('c{category:02}',{value},{active})").unwrap();
        }
        engine
            .execute(&format!("INSERT INTO facts VALUES {values}"))
            .unwrap();
    }
    engine
}

fn analytical_query(c: &mut Criterion) {
    let mut engine = generated_engine();
    let validation = engine.execute(QUERY).unwrap();
    assert!(matches!(
        validation.last(),
        Some(StatementResult::Query(result)) if !result.rows.is_empty()
    ));

    let mut group = c.benchmark_group("analytical");
    group.throughput(Throughput::Elements(ROWS as u64));
    group.sample_size(20);
    group.bench_function("grouped_aggregate_50k", |bencher| {
        bencher.iter(|| black_box(engine.execute(black_box(QUERY)).unwrap()));
    });
    group.finish();
}

criterion_group!(benches, analytical_query);
criterion_main!(benches);
