use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusthouse::snapshot::{SnapshotError, SnapshotStore};
use rusthouse::{
    Catalog, CatalogError, CatalogLimits, CatalogSnapshotError, DataType, ParseLimits,
    TableSnapshotError, write_select_csv_with_names,
};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(test_name: &str) -> Self {
        let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("catalog-snapshot-tests")
            .join(format!("{test_name}-{}-{sequence}", std::process::id()));
        fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }

    fn snapshot(&self) -> PathBuf {
        self.0.join("table.snapshot")
    }

    fn named_snapshot(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("remove test directory");
    }
}

#[test]
fn saves_and_loads_a_mixed_table_under_a_new_name() {
    let directory = TestDirectory::new("mixed-reopen");
    let path = directory.snapshot();
    let snapshots = SnapshotStore::new(4 * 1024);
    let mut original = Catalog::new();
    original
        .execute_create(
            "CREATE TABLE Readings (sequence Int64, value Float64, active Bool, label String)",
        )
        .unwrap();
    original
        .execute_insert(
            "INSERT INTO readings VALUES (-1, -0.0, false, ''), (42, 3.5, true, 'north')",
        )
        .unwrap();

    original.save_table("READINGS", &path, &snapshots).unwrap();

    let mut reopened = Catalog::new();
    let table = reopened
        .load_table("ImportedReadings", &path, &snapshots)
        .unwrap();

    assert_eq!(table.fields()[0].name(), "sequence");
    assert_eq!(table.fields()[1].data_type(), DataType::Float64);
    assert_eq!(table.int64_column("sequence").unwrap(), [-1, 42]);
    assert_eq!(
        table
            .float64_column("value")
            .unwrap()
            .iter()
            .map(|value| value.to_bits())
            .collect::<Vec<_>>(),
        [(-0.0_f64).to_bits(), 3.5_f64.to_bits()]
    );
    assert_eq!(
        table.bool_column("active").unwrap().collect::<Vec<_>>(),
        [false, true]
    );
    assert_eq!(table.string_column("label").unwrap(), ["", "north"]);
    assert_eq!(
        reopened.table_names().collect::<Vec<_>>(),
        ["ImportedReadings"]
    );
    assert_eq!(reopened.table("importedreadings").unwrap().len(), 2);
}

#[test]
fn reopened_tables_support_streaming_selects() {
    let directory = TestDirectory::new("streaming-select");
    let path = directory.snapshot();
    let snapshots = SnapshotStore::default();
    let mut original = Catalog::new();
    original
        .execute_create("CREATE TABLE events (id Int64, active Bool)")
        .unwrap();
    original
        .execute_insert("INSERT INTO events VALUES (1, true), (2, false), (3, true)")
        .unwrap();
    original.save_table("events", &path, &snapshots).unwrap();

    let mut reopened = Catalog::new();
    reopened
        .load_table("archived_events", &path, &snapshots)
        .unwrap();
    let result = reopened
        .execute_select("SELECT id FROM archived_events WHERE active = true")
        .unwrap();
    let mut output = Vec::new();
    write_select_csv_with_names(&result, &mut output).unwrap();

    assert_eq!(output, b"\"id\"\n1\n3\n");
}

#[test]
fn reports_missing_catalog_tables_and_snapshot_paths_without_mutation() {
    let directory = TestDirectory::new("missing");
    let path = directory.snapshot();
    let snapshots = SnapshotStore::default();
    let mut catalog = catalog_with_retained_table(3);

    let save_error = catalog
        .save_table("Missing", &path, &snapshots)
        .unwrap_err();
    assert!(matches!(
        save_error,
        CatalogSnapshotError::Catalog(CatalogError::TableNotFound { ref name })
            if name == "Missing"
    ));
    assert!(!path.exists());

    let load_error = catalog.load_table("Loaded", &path, &snapshots).unwrap_err();
    assert!(matches!(
        &load_error,
        CatalogSnapshotError::Snapshot(TableSnapshotError::Envelope(
            SnapshotError::Missing { path: missing_path }
        )) if missing_path == &path
    ));
    assert_eq!(
        load_error.source().unwrap().to_string(),
        load_error.to_string()
    );
    assert_retained_only(&catalog);
}

#[test]
fn corrupt_envelopes_and_payloads_roll_back_catalog_loading() {
    let directory = TestDirectory::new("corrupt");
    let path = directory.snapshot();
    let snapshots = SnapshotStore::new(1024);
    let mut catalog = catalog_with_retained_table(3);

    snapshots.write(&path, b"checksum protected").unwrap();
    let mut bytes = fs::read(&path).unwrap();
    *bytes.last_mut().unwrap() ^= 0xff;
    fs::write(&path, bytes).unwrap();
    let envelope_error = catalog
        .load_table("EnvelopeFailure", &path, &snapshots)
        .unwrap_err();
    assert!(matches!(
        envelope_error,
        CatalogSnapshotError::Snapshot(TableSnapshotError::Envelope(SnapshotError::Corrupt(_)))
    ));
    assert_retained_only(&catalog);

    snapshots.write(&path, b"not a table payload").unwrap();
    let decode_error = catalog
        .load_table("DecodeFailure", &path, &snapshots)
        .unwrap_err();
    assert!(matches!(
        decode_error,
        CatalogSnapshotError::Snapshot(TableSnapshotError::InvalidMagic { .. })
    ));
    assert_retained_only(&catalog);
}

#[test]
fn duplicate_and_capacity_failures_precede_snapshot_reads() {
    let directory = TestDirectory::new("preflight");
    let missing_path = directory.snapshot();
    let snapshots = SnapshotStore::default();
    let mut catalog = catalog_with_retained_table(1);

    let duplicate = catalog
        .load_table("ReTaInEd", &missing_path, &snapshots)
        .unwrap_err();
    assert!(matches!(
        duplicate,
        CatalogSnapshotError::Catalog(CatalogError::DuplicateTable { ref name })
            if name == "ReTaInEd"
    ));
    let fallback_duplicate = catalog
        .load_table_with_fallback(
            "ReTaInEd",
            &missing_path,
            directory.named_snapshot("also-missing.snapshot"),
            &snapshots,
        )
        .unwrap_err();
    assert!(matches!(
        fallback_duplicate,
        CatalogSnapshotError::Catalog(CatalogError::DuplicateTable { ref name })
            if name == "ReTaInEd"
    ));
    assert_retained_only(&catalog);

    let capacity = catalog
        .load_table("another", &missing_path, &snapshots)
        .unwrap_err();
    assert!(matches!(
        capacity,
        CatalogSnapshotError::Catalog(CatalogError::TableLimitExceeded { limit: 1 })
    ));
    let fallback_capacity = catalog
        .load_table_with_fallback(
            "another",
            &missing_path,
            directory.named_snapshot("also-missing.snapshot"),
            &snapshots,
        )
        .unwrap_err();
    assert!(matches!(
        fallback_capacity,
        CatalogSnapshotError::Catalog(CatalogError::TableLimitExceeded { limit: 1 })
    ));
    assert_retained_only(&catalog);
}

#[test]
fn loads_a_table_from_an_explicit_fallback_snapshot() {
    let directory = TestDirectory::new("fallback-success");
    let primary = directory.named_snapshot("primary.snapshot");
    let fallback = directory.named_snapshot("fallback.snapshot");
    let snapshots = SnapshotStore::new(1024);
    let mut source = Catalog::new();
    source
        .execute_create("CREATE TABLE source (id Int64)")
        .unwrap();
    source
        .execute_insert("INSERT INTO source VALUES (11), (12)")
        .unwrap();
    source.save_table("source", &fallback, &snapshots).unwrap();
    fs::write(&primary, b"short").unwrap();

    let mut catalog = catalog_with_retained_table(3);
    let loaded = catalog
        .load_table_with_fallback("Recovered", &primary, &fallback, &snapshots)
        .unwrap();

    assert_eq!(loaded.int64_column("id").unwrap(), [11, 12]);
    let mut table_names = catalog.table_names().collect::<Vec<_>>();
    table_names.sort_unstable();
    assert_eq!(table_names, ["Recovered", "retained"]);
}

#[test]
fn fallback_recovered_tables_support_grouped_and_distinct_counts() {
    let directory = TestDirectory::new("fallback-counts");
    let primary = directory.named_snapshot("primary.snapshot");
    let fallback = directory.named_snapshot("fallback.snapshot");
    let snapshots = SnapshotStore::new(4 * 1024);
    let mut source = Catalog::new();
    source
        .execute_create("CREATE TABLE source (region String, customer Int64)")
        .unwrap();
    source
        .execute_insert("INSERT INTO source VALUES ('east', 1), ('east', 1), ('west', 2)")
        .unwrap();
    source.save_table("source", &fallback, &snapshots).unwrap();
    fs::write(&primary, b"truncated").unwrap();

    let mut recovered = Catalog::new();
    recovered
        .load_table_with_fallback("events", &primary, &fallback, &snapshots)
        .unwrap();

    let grouped = recovered
        .execute_select("SELECT region, COUNT(*) FROM events GROUP BY region")
        .unwrap();
    assert_eq!(
        grouped.grouped_rows().collect::<Vec<_>>(),
        [
            (&rusthouse::Value::from("east"), 2),
            (&rusthouse::Value::from("west"), 1),
        ]
    );

    let distinct = recovered
        .execute_select("SELECT COUNT(DISTINCT customer) FROM events")
        .unwrap();
    assert_eq!(distinct.scalar_value(), Some(&rusthouse::Value::Int64(2)));
}

#[test]
fn invalid_fallback_generations_and_size_limits_leave_catalog_unchanged() {
    let directory = TestDirectory::new("fallback-rollback");
    let primary = directory.named_snapshot("primary.snapshot");
    let fallback = directory.named_snapshot("fallback.snapshot");
    let writer = SnapshotStore::new(1024);
    writer.write(&primary, b"primary").unwrap();
    let mut bytes = fs::read(&primary).unwrap();
    *bytes.last_mut().unwrap() ^= 0xff;
    fs::write(&primary, bytes).unwrap();
    fs::write(&fallback, b"short").unwrap();
    let mut catalog = catalog_with_retained_table(3);

    let invalid = catalog
        .load_table_with_fallback("Invalid", &primary, &fallback, &writer)
        .unwrap_err();
    assert!(matches!(
        invalid,
        CatalogSnapshotError::Snapshot(TableSnapshotError::Envelope(
            SnapshotError::FallbackFailed { primary, fallback }
        )) if matches!(
            *primary,
            SnapshotError::Corrupt(rusthouse::snapshot::SnapshotCorruption::ChecksumMismatch { .. })
        ) && matches!(*fallback, SnapshotError::Truncated { .. })
    ));
    assert_retained_only(&catalog);

    let mut source = Catalog::new();
    source
        .execute_create("CREATE TABLE source (id Int64)")
        .unwrap();
    source.save_table("source", &fallback, &writer).unwrap();
    fs::remove_file(&primary).unwrap();

    let oversized = catalog
        .load_table_with_fallback("Oversized", &primary, &fallback, &SnapshotStore::new(4))
        .unwrap_err();
    assert!(matches!(
        oversized,
        CatalogSnapshotError::Snapshot(TableSnapshotError::Envelope(
            SnapshotError::FallbackFailed { primary, fallback }
        )) if matches!(*primary, SnapshotError::Missing { .. })
            && matches!(*fallback, SnapshotError::Oversized { max_payload_len: 4, .. })
    ));
    assert_retained_only(&catalog);
}

fn catalog_with_retained_table(max_tables: usize) -> Catalog {
    let limits = CatalogLimits::new(ParseLimits::default(), max_tables, 10);
    let mut catalog = Catalog::with_limits(limits);
    catalog
        .execute_create("CREATE TABLE retained (id Int64)")
        .unwrap();
    catalog
        .execute_insert("INSERT INTO retained VALUES (7)")
        .unwrap();
    catalog
}

fn assert_retained_only(catalog: &Catalog) {
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog.table_names().collect::<Vec<_>>(), ["retained"]);
    assert_eq!(
        catalog
            .table("retained")
            .unwrap()
            .int64_column("id")
            .unwrap(),
        [7]
    );
}
