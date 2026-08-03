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
fn reopened_tables_support_limited_streaming_selects() {
    let directory = TestDirectory::new("limited-select");
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
        .execute_select("SELECT id FROM archived_events WHERE active = true LIMIT 1")
        .unwrap();
    let mut output = Vec::new();
    write_select_csv_with_names(&result, &mut output).unwrap();

    assert_eq!(output, b"\"id\"\n1\n");
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
    assert_retained_only(&catalog);

    let capacity = catalog
        .load_table("another", &missing_path, &snapshots)
        .unwrap_err();
    assert!(matches!(
        capacity,
        CatalogSnapshotError::Catalog(CatalogError::TableLimitExceeded { limit: 1 })
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
