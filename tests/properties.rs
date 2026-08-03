use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use proptest::prelude::*;
use rusthouse::snapshot::SnapshotStore;
use rusthouse::{
    Catalog, DataType, Field, SelectParseLimits, SelectResult, Table, Value,
    parse_select_with_limits,
};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct PropertyDirectory(PathBuf);

impl PropertyDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("property-tests")
            .join(format!("{label}-{}-{sequence}", std::process::id()));
        fs::create_dir_all(&path).expect("create property-test directory");
        Self(path)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for PropertyDirectory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("remove property-test directory");
    }
}

fn selected_ids(result: &SelectResult<'_>) -> Vec<i64> {
    let ids = result.table().int64_column("id").unwrap();
    result.selected_rows().map(|row| ids[row]).collect()
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        max_shrink_iters: 4_096,
        ..ProptestConfig::default()
    })]

    #[test]
    fn select_parser_is_total_for_bounded_utf8(input in prop::collection::vec(any::<u8>(), 0..384)) {
        let input = String::from_utf8_lossy(&input);
        let limits = SelectParseLimits::new(128, 8)
            .with_max_predicates(8)
            .with_max_predicate_groups(4)
            .with_max_order_keys(4);

        if let Err(error) = parse_select_with_limits(&input, limits) {
            prop_assert!(error.position <= input.len());
            prop_assert!(!error.to_string().is_empty());
        }
    }

    #[test]
    fn snapshot_decoders_are_total_for_bounded_bytes(
        envelope in prop::collection::vec(any::<u8>(), 0..384),
        table_payload in prop::collection::vec(any::<u8>(), 0..257),
    ) {
        let directory = PropertyDirectory::new("snapshot-decode");
        let envelope_path = directory.path("arbitrary.snapshot");
        let table_path = directory.path("table.snapshot");

        fs::write(&envelope_path, envelope).unwrap();
        let bounded = SnapshotStore::new(64);
        if let Ok(payload) = bounded.read(&envelope_path) {
            prop_assert!(payload.len() <= bounded.max_payload_len());
        }

        let table_store = SnapshotStore::new(256);
        table_store.write(&table_path, &table_payload).unwrap();
        if let Ok(table) = table_store.read_table(&table_path) {
            prop_assert!(table.len() <= table.row_limit());
            prop_assert!(!table.fields().is_empty());
        }
    }

    #[test]
    fn bounded_ordering_is_a_prefix_of_reference_order(
        rows in prop::collection::vec((any::<i16>(), any::<i16>()), 0..96),
        limit in 0usize..112,
    ) {
        let mut catalog = Catalog::new();
        catalog
            .execute_create("CREATE TABLE ranked (id Int64, key Int64)")
            .unwrap();
        catalog
            .table_mut("ranked")
            .unwrap()
            .insert_batch(rows.iter().map(|(key, id)| {
                vec![Value::Int64(i64::from(*id)), Value::Int64(i64::from(*key))]
            }))
            .unwrap();

        let mut reference_rows = (0..rows.len()).collect::<Vec<_>>();
        reference_rows.sort_unstable_by(|left, right| {
            rows[*left]
                .0
                .cmp(&rows[*right].0)
                .then_with(|| rows[*right].1.cmp(&rows[*left].1))
                .then_with(|| left.cmp(right))
        });
        let expected = reference_rows
            .iter()
            .map(|row| i64::from(rows[*row].1))
            .collect::<Vec<_>>();

        let full = catalog
            .execute_select("SELECT id FROM ranked ORDER BY key ASC, id DESC")
            .unwrap();
        prop_assert_eq!(selected_ids(&full), expected.clone());
        drop(full);

        let bounded = catalog
            .execute_select(&format!(
                "SELECT id FROM ranked ORDER BY key ASC, id DESC LIMIT {limit}"
            ))
            .unwrap();
        prop_assert_eq!(
            selected_ids(&bounded),
            expected[..expected.len().min(limit)].to_vec(),
        );
    }

    #[test]
    fn integer_reductions_match_reference_arithmetic(
        values in prop::collection::vec(any::<i32>(), 0..128),
    ) {
        let mut table = Table::new(vec![Field::new("value", DataType::Int64)]).unwrap();
        table
            .insert_batch(
                values
                    .iter()
                    .map(|value| vec![Value::Int64(i64::from(*value))]),
            )
            .unwrap();

        let expected_sum = values.iter().map(|value| i64::from(*value)).sum::<i64>();
        let expected_min = values.iter().min().map(|value| Value::Int64(i64::from(*value)));
        let expected_max = values.iter().max().map(|value| Value::Int64(i64::from(*value)));
        let expected_avg = (!values.is_empty()).then(|| {
            Value::Float64(expected_sum as f64 / values.len() as f64)
        });

        prop_assert_eq!(table.count(None), Ok(values.len()));
        prop_assert_eq!(table.sum("value", None), Ok(Value::Int64(expected_sum)));
        prop_assert_eq!(table.avg("value", None), Ok(expected_avg));
        prop_assert_eq!(table.min("value", None), Ok(expected_min));
        prop_assert_eq!(table.max("value", None), Ok(expected_max));
    }
}
