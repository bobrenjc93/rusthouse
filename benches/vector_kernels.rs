use std::hint::black_box;
use std::time::Instant;

use rusthouse::batch::{
    BatchConfig, Column, DataType, DictionaryArray, Field, Int64Array, RecordBatch, Schema,
};
use rusthouse::kernels::{
    AggregateExpr, AggregateKind, ComparisonOp, GroupByConfig, SumValue, compare_i64, hash_group,
    sum,
};

const ROWS: usize = 16_384;
const SCAN_ITERATIONS: usize = 500;
const GROUP_ITERATIONS: usize = 100;

fn workload() -> RecordBatch {
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut values = Int64Array::with_capacity(ROWS);
    let mut groups = DictionaryArray::with_capacity(ROWS).unwrap();
    let labels = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta"];
    for row in 0..ROWS {
        state = state
            .wrapping_mul(2_862_933_555_777_941_757)
            .wrapping_add(3_037_000_493);
        values
            .push((row % 31 != 0).then_some((state as i64) % 1_000_003))
            .unwrap();
        groups
            .push((row % 47 != 0).then_some(labels[row % labels.len()]))
            .unwrap();
    }
    RecordBatch::try_new(
        Schema::new(vec![
            Field::new("value", DataType::Int64, true),
            Field::new("group", DataType::String, true),
        ]),
        vec![Column::Int64(values), Column::String(groups)],
        BatchConfig::unlimited(ROWS),
    )
    .unwrap()
}

fn main() {
    let mut batch = workload();

    let started = Instant::now();
    let mut scan_checksum = 0_i128;
    for _ in 0..SCAN_ITERATIONS {
        batch.reset_selection();
        let selection = compare_i64(&batch, 0, ComparisonOp::GreaterEq, 100_000).unwrap();
        batch.replace_selection(selection).unwrap();
        let Some(SumValue::Int128(value)) = sum(&batch, 0).unwrap() else {
            unreachable!()
        };
        scan_checksum ^= black_box(value);
    }
    batch.reset_selection();
    let scan_elapsed = started.elapsed();

    let aggregates = [
        AggregateExpr::count_all(),
        AggregateExpr::new(AggregateKind::Sum, 0),
        AggregateExpr::new(AggregateKind::Min, 0),
        AggregateExpr::new(AggregateKind::Max, 0),
        AggregateExpr::new(AggregateKind::Avg, 0),
    ];
    let started = Instant::now();
    let mut group_checksum = 0_usize;
    for _ in 0..GROUP_ITERATIONS {
        let groups = hash_group(&batch, &[1], &aggregates, GroupByConfig::unlimited(16)).unwrap();
        group_checksum ^= black_box(groups.len() + groups.retained_bytes());
    }
    let group_elapsed = started.elapsed();

    assert_eq!(scan_checksum, 0, "even iteration count fixes the checksum");
    assert_eq!(group_checksum, 0, "even iteration count fixes the checksum");
    println!("vector_scan rows={ROWS} iterations={SCAN_ITERATIONS} elapsed={scan_elapsed:?}");
    println!("hash_group rows={ROWS} iterations={GROUP_ITERATIONS} elapsed={group_elapsed:?}");
}
