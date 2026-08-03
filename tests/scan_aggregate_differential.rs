use rusthouse::{
    AggregateError, AggregateLimits, ComparisonOperator, RowSelection, ScanLimits,
    aggregate_nullable_i64, scan_nullable_i64,
};

const OPERATORS: [ComparisonOperator; 6] = [
    ComparisonOperator::Eq,
    ComparisonOperator::Ne,
    ComparisonOperator::Lt,
    ComparisonOperator::Le,
    ComparisonOperator::Gt,
    ComparisonOperator::Ge,
];

const COMPARISON_VALUES: [i64; 9] = [
    i64::MIN,
    i64::MIN + 1,
    -9,
    -1,
    0,
    1,
    8,
    i64::MAX - 1,
    i64::MAX,
];

const SEEDED_CORPORA: [(u64, usize); 6] = [
    (0x0000_0000_0000_0000, 127),
    (0x0000_0000_0000_0001, 128),
    (0x0123_4567_89ab_cdef, 255),
    (0x5555_aaaa_ffff_0000, 256),
    (0xdead_beef_cafe_babe, 511),
    (0xffff_ffff_ffff_ffff, 512),
];

#[derive(Clone, Copy)]
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }
}

fn nullable_i64_corpus(seed: u64, len: usize) -> Vec<Option<i64>> {
    let mut rng = SplitMix64::new(seed);
    let mut values = Vec::with_capacity(len);

    for _ in 0..len {
        let value = match rng.next_u64() % 16 {
            0..=2 => None,
            3 => Some(i64::MIN),
            4 => Some(i64::MAX),
            5..=9 => Some((rng.next_u64() % 17) as i64 - 8),
            _ => Some(rng.next_u64() as i64),
        };
        values.push(value);
    }

    // Lock in NULL, duplicate, and overflow-producing extreme cases for every
    // generated corpus while leaving the remainder dependent on the seed.
    let required_prefix = [
        None,
        Some(i64::MIN),
        Some(i64::MIN),
        Some(i64::MAX),
        Some(i64::MAX),
        Some(0),
        Some(0),
        Some(-1),
        Some(1),
    ];
    for (destination, value) in values.iter_mut().zip(required_prefix) {
        *destination = value;
    }

    values
}

fn reference_matches(left: i64, operator: ComparisonOperator, right: i64) -> bool {
    match operator {
        ComparisonOperator::Eq => left == right,
        ComparisonOperator::Ne => left != right,
        ComparisonOperator::Lt => left < right,
        ComparisonOperator::Le => left <= right,
        ComparisonOperator::Gt => left > right,
        ComparisonOperator::Ge => left >= right,
    }
}

fn reference_scan(
    values: &[Option<i64>],
    operator: ComparisonOperator,
    comparison_value: i64,
) -> Vec<usize> {
    values
        .iter()
        .enumerate()
        .filter_map(|(index, value)| {
            value
                .filter(|&value| reference_matches(value, operator, comparison_value))
                .map(|_| index)
        })
        .collect()
}

fn assert_aggregate_matches_reference(values: &[Option<i64>], rows: &[usize], context: &str) {
    let actual = aggregate_nullable_i64(
        values,
        RowSelection::Indices(rows),
        AggregateLimits::new(values.len(), rows.len()),
    );

    let mut count_column = 0_u64;
    let mut exact_sum = 0_i128;
    for &row in rows {
        if let Some(value) = values[row] {
            count_column += 1;
            exact_sum += i128::from(value);
        }
    }

    match actual {
        Ok(actual) => {
            assert_eq!(
                actual.count_star(),
                rows.len() as u64,
                "COUNT(*): {context}"
            );
            assert_eq!(
                actual.count_column(),
                count_column,
                "COUNT(column): {context}"
            );

            let expected_sum = if count_column == 0 {
                None
            } else {
                Some(i64::try_from(exact_sum).unwrap_or_else(|_| {
                    panic!("expected SUM overflow was not reported: {context}; sum={exact_sum}")
                }))
            };
            assert_eq!(actual.sum(), expected_sum, "SUM: {context}");
        }
        Err(AggregateError::SumOverflow { sum }) => {
            assert!(
                count_column > 0 && i64::try_from(exact_sum).is_err(),
                "unexpected SUM overflow: {context}; sum={exact_sum}"
            );
            assert_eq!(sum, exact_sum, "overflow sum: {context}");
        }
        Err(error) => panic!("unexpected aggregate error: {context}; error={error:?}"),
    }
}

fn assert_scan_to_aggregate_pipeline(values: &[Option<i64>], corpus_name: &str, seed: u64) {
    for comparison_value in COMPARISON_VALUES {
        for operator in OPERATORS {
            let context = format!(
                "corpus={corpus_name}; seed={seed:#018x}; rows={}; operator={operator:?}; comparison={comparison_value}",
                values.len()
            );
            let expected_rows = reference_scan(values, operator, comparison_value);
            let actual_rows = scan_nullable_i64(
                values,
                operator,
                comparison_value,
                ScanLimits::new(values.len(), expected_rows.len()),
            )
            .unwrap_or_else(|error| panic!("unexpected scan error: {context}; error={error:?}"));

            assert_eq!(actual_rows, expected_rows, "scan rows: {context}");
            assert_aggregate_matches_reference(values, &actual_rows, &context);
        }
    }
}

#[test]
fn fixed_seed_corpora_match_the_reference_pipeline() {
    for (seed, len) in SEEDED_CORPORA {
        let values = nullable_i64_corpus(seed, len);
        assert_scan_to_aggregate_pipeline(&values, "generated", seed);
    }
}

#[test]
fn edge_corpora_match_at_exact_resource_bounds() {
    let corpora = [
        ("empty", vec![]),
        ("all-null", vec![None; 8]),
        (
            "duplicates",
            vec![Some(7), None, Some(7), Some(-3), Some(7), Some(-3)],
        ),
        (
            "extremes",
            vec![
                Some(i64::MIN),
                Some(i64::MIN),
                None,
                Some(-1),
                Some(0),
                Some(1),
                Some(i64::MAX),
                Some(i64::MAX),
            ],
        ),
    ];

    for (case_index, (name, values)) in corpora.iter().enumerate() {
        let case_seed = 0xec00_0000_0000_0000 | case_index as u64;
        assert_scan_to_aggregate_pipeline(values, name, case_seed);
    }
}
