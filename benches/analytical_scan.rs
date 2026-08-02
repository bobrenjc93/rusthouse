use std::env;
use std::hint::black_box;
use std::time::Instant;

use rusthouse::{Column, ColumnSchema, DataType, Schema, Table, TableLimits, Value};

const DEFAULT_ROWS: usize = 100_000;
const DEFAULT_ITERATIONS: usize = 40;
const BATCH_ROWS: usize = 4_096;
const SEED: u64 = 0x4d59_5df4_d0f3_3173;

fn main() {
    let rows = setting("RUSTHOUSE_BENCH_ROWS", DEFAULT_ROWS);
    let iterations = setting("RUSTHOUSE_BENCH_ITERATIONS", DEFAULT_ITERATIONS);
    assert!(rows > 0, "RUSTHOUSE_BENCH_ROWS must be greater than zero");
    assert!(
        iterations > 0,
        "RUSTHOUSE_BENCH_ITERATIONS must be greater than zero"
    );

    let table = build_table(rows);
    let expected = scan(&table);
    let started = Instant::now();
    let mut checksum = 0u64;
    for _ in 0..iterations {
        let result = black_box(scan(black_box(&table)));
        assert_eq!(result, expected);
        checksum ^= result.fingerprint();
        checksum = checksum.rotate_left(7);
    }
    let elapsed = started.elapsed();
    let scanned_rows = rows.checked_mul(iterations).expect("scan size overflow");
    let rows_per_second = scanned_rows as f64 / elapsed.as_secs_f64().max(f64::MIN_POSITIVE);

    println!(
        "analytical_scan rows={rows} iterations={iterations} seed={SEED:#x} \
         elapsed_ms={:.3} rows_per_second={rows_per_second:.0} checksum={checksum:#x}",
        elapsed.as_secs_f64() * 1_000.0,
    );
}

fn setting(name: &str, default: usize) -> usize {
    env::var(name)
        .map(|value| {
            value
                .parse()
                .unwrap_or_else(|_| panic!("{name} must be a positive integer"))
        })
        .unwrap_or(default)
}

fn build_table(row_count: usize) -> Table {
    let schema = Schema::new(vec![
        ColumnSchema::new("event_id", DataType::Int64),
        ColumnSchema::new("metric", DataType::Float64),
        ColumnSchema::new("active", DataType::Bool),
        ColumnSchema::new("label", DataType::String),
    ])
    .unwrap();
    let mut table = Table::new(
        schema,
        TableLimits {
            max_columns: 4,
            max_rows: row_count,
            max_cells: row_count.checked_mul(4).expect("cell limit overflow"),
            max_string_bytes: row_count.checked_mul(12).expect("String limit overflow"),
        },
    )
    .unwrap();

    let mut state = SEED;
    for start in (0..row_count).step_by(BATCH_ROWS) {
        let end = (start + BATCH_ROWS).min(row_count);
        let mut batch = Vec::with_capacity(end - start);
        for row in start..end {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            batch.push(vec![
                Value::Int64(row as i64 - row_count as i64 / 2),
                Value::Float64(((state >> 16) % 1_000_000) as f64 / 100.0),
                Value::Bool(state & 1 == 0),
                Value::String(format!("group-{}", state % 128)),
            ]);
        }
        table.insert_batch(batch).unwrap();
    }
    table
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ScanResult {
    id_sum: i64,
    metric_sum: f64,
    active_count: usize,
    string_bytes: usize,
}

impl ScanResult {
    fn fingerprint(self) -> u64 {
        (self.id_sum as u64)
            ^ self.metric_sum.to_bits().rotate_left(11)
            ^ (self.active_count as u64).rotate_left(23)
            ^ (self.string_bytes as u64).rotate_left(37)
    }
}

fn scan(table: &Table) -> ScanResult {
    let ids = table.column("event_id").and_then(Column::as_int64).unwrap();
    let metrics = table.column("metric").and_then(Column::as_float64).unwrap();
    let active = table.column("active").and_then(Column::as_bool).unwrap();
    let labels = table.column("label").and_then(Column::as_string).unwrap();

    let mut result = ScanResult {
        id_sum: 0,
        metric_sum: 0.0,
        active_count: 0,
        string_bytes: 0,
    };
    for index in 0..table.row_count() {
        result.id_sum += ids[index];
        result.metric_sum += metrics[index];
        result.active_count += usize::from(active[index]);
        result.string_bytes += labels[index].len();
    }
    result
}
