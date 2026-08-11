#![cfg(unix)]

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

use rusthouse::batch::error::Error;
use rusthouse::batch::storage::Column;
use rusthouse::snapshot::{NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN, SNAPSHOT_HEADER_LEN};
use rusthouse::{
    Database, DatabaseRleSnapshotSaveError, Int64TableRleFileSaveError, NullableI64RlePayloadCodec,
    NullableI64RlePayloadError, Schema, SharedDatabase, SharedDatabaseRleSnapshotSaveError,
    SnapshotCodec, SnapshotError, SnapshotReplaceError,
};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let base =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("target/batch-rle-snapshot-save-tests");
        fs::create_dir_all(&base).unwrap();
        loop {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = base.join(format!("{}-{sequence}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("could not create test directory: {error}"),
            }
        }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn rle_shape(rows: &[Option<i64>]) -> (usize, usize) {
    let run_count =
        usize::from(!rows.is_empty()) + rows.windows(2).filter(|pair| pair[0] != pair[1]).count();
    let value_run_count = rows
        .iter()
        .enumerate()
        .filter(|(index, value)| value.is_some() && (*index == 0 || rows[*index - 1] != **value))
        .count();
    let payload_len = NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN + run_count * 9 + value_run_count * 8;
    (run_count, payload_len)
}

fn exact_codecs(rows: &[Option<i64>]) -> (SnapshotCodec, NullableI64RlePayloadCodec) {
    let (run_count, payload_len) = rle_shape(rows);
    (
        SnapshotCodec::new(payload_len),
        NullableI64RlePayloadCodec::new(rows.len(), run_count, payload_len),
    )
}

fn nullable_database() -> Database {
    let mut database = Database::with_max_rows_per_table(8);
    database
        .execute(
            "CREATE TABLE Readings (Measurement Nullable(Int64)); \
             INSERT INTO Readings VALUES (NULL), (7), (7), (NULL);",
        )
        .unwrap();
    database
}

#[test]
fn database_saves_empty_repetitive_mixed_and_all_null_rows_at_exact_limits() {
    let directory = TestDirectory::new();
    let cases = [
        ("empty", true, "", vec![]),
        (
            "repetitive",
            false,
            "INSERT INTO source VALUES (9), (9), (9), (9);",
            vec![Some(9), Some(9), Some(9), Some(9)],
        ),
        (
            "mixed",
            true,
            "INSERT INTO source VALUES (NULL), (-2), (-2), (7), (NULL);",
            vec![None, Some(-2), Some(-2), Some(7), None],
        ),
        (
            "all-null",
            true,
            "INSERT INTO source VALUES (NULL), (NULL), (NULL);",
            vec![None, None, None],
        ),
    ];

    for (case, nullable, insert, rows) in cases {
        let path = directory.join(&format!("{case}.snapshot"));
        let mut source = Database::with_max_rows_per_table(8);
        let column_type = if nullable { "Nullable(Int64)" } else { "Int64" };
        source
            .execute(&format!(
                "CREATE TABLE source (reading {column_type}); {insert}"
            ))
            .unwrap();
        let (snapshot_codec, payload_codec) = exact_codecs(&rows);

        source
            .save_int64_table_rle_to_file("SOURCE", &path, snapshot_codec, payload_codec)
            .unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().len() as usize,
            SNAPSHOT_HEADER_LEN + payload_codec.max_payload_len(),
            "case {case}"
        );
        let mut reopened = Database::with_max_rows_per_table(8);
        reopened
            .restore_int64_table_rle_from_file(
                "archive",
                &path,
                Schema::int64("reading", nullable),
                8,
                snapshot_codec,
                payload_codec,
            )
            .unwrap();
        let restored = reopened.catalog().table("ARCHIVE").unwrap();
        match &restored.columns()[0] {
            Column::NullableInt64(values) if nullable => assert_eq!(values, &rows, "case {case}"),
            Column::Int64(values) if !nullable => assert_eq!(
                values,
                &rows.iter().copied().map(Option::unwrap).collect::<Vec<_>>(),
                "case {case}"
            ),
            column => panic!("case {case} restored unexpected physical column {column:?}"),
        }
    }
}

#[test]
fn database_reports_typed_shape_encoding_and_replacement_failures_without_data_loss() {
    let directory = TestDirectory::new();
    let path = directory.join("preserved.snapshot");
    let original = b"existing destination bytes";
    fs::write(&path, original).unwrap();
    let mut database = nullable_database();
    database
        .execute(
            "CREATE TABLE multiple (a Int64, b Int64); \
             CREATE TABLE text_only (value String);",
        )
        .unwrap();
    let exact_payload_len = NULLABLE_I64_RLE_PAYLOAD_HEADER_LEN + 9 + 17 + 9;

    let missing = database
        .save_int64_table_rle_to_file(
            "missing",
            &path,
            SnapshotCodec::new(exact_payload_len),
            NullableI64RlePayloadCodec::new(4, 3, exact_payload_len),
        )
        .unwrap_err();
    assert!(matches!(
        missing,
        DatabaseRleSnapshotSaveError::Table(Error::TableNotFound(ref table)) if table == "missing"
    ));
    assert!(!missing.destination_was_replaced());

    let multiple = database
        .save_int64_table_rle_to_file(
            "MULTIPLE",
            &path,
            SnapshotCodec::new(exact_payload_len),
            NullableI64RlePayloadCodec::new(4, 3, exact_payload_len),
        )
        .unwrap_err();
    assert!(matches!(
        multiple,
        DatabaseRleSnapshotSaveError::UnsupportedColumnCount {
            ref table,
            column_count: 2,
        } if table == "multiple"
    ));

    let wrong_type = database
        .save_int64_table_rle_to_file(
            "text_only",
            &path,
            SnapshotCodec::new(exact_payload_len),
            NullableI64RlePayloadCodec::new(4, 3, exact_payload_len),
        )
        .unwrap_err();
    assert!(matches!(
        wrong_type,
        DatabaseRleSnapshotSaveError::UnsupportedColumnType { ref column, .. }
            if column == "value"
    ));

    let encoding = database
        .save_int64_table_rle_to_file(
            "readings",
            &path,
            SnapshotCodec::new(exact_payload_len),
            NullableI64RlePayloadCodec::new(4, 2, exact_payload_len),
        )
        .unwrap_err();
    assert!(matches!(
        encoding,
        DatabaseRleSnapshotSaveError::Snapshot(Int64TableRleFileSaveError::Payload(
            NullableI64RlePayloadError::RunLimitExceeded {
                run_count: 3,
                max_runs: 2,
            }
        ))
    ));
    assert!(!encoding.destination_was_replaced());

    let replacement = database
        .save_int64_table_rle_to_file(
            "readings",
            &path,
            SnapshotCodec::new(exact_payload_len - 1),
            NullableI64RlePayloadCodec::new(4, 3, exact_payload_len),
        )
        .unwrap_err();
    assert!(matches!(
        replacement,
        DatabaseRleSnapshotSaveError::Snapshot(Int64TableRleFileSaveError::Replace(
            SnapshotReplaceError::Encode(SnapshotError::PayloadTooLarge { .. })
        ))
    ));
    assert!(!replacement.destination_was_replaced());
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
}

#[test]
fn filesystem_replacement_failure_preserves_the_existing_destination() {
    let directory = TestDirectory::new();
    let path = directory.join("destination");
    fs::create_dir(&path).unwrap();
    let marker = path.join("marker");
    fs::write(&marker, b"keep me").unwrap();
    let database = nullable_database();
    let rows = [None, Some(7), Some(7), None];
    let (snapshot_codec, payload_codec) = exact_codecs(&rows);

    let error = database
        .save_int64_table_rle_to_file("readings", &path, snapshot_codec, payload_codec)
        .unwrap_err();

    assert!(matches!(
        error,
        DatabaseRleSnapshotSaveError::Snapshot(Int64TableRleFileSaveError::Replace(
            SnapshotReplaceError::Rename(_)
        ))
    ));
    assert!(!error.destination_was_replaced());
    assert_eq!(fs::read(marker).unwrap(), b"keep me");
    assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
}

#[test]
fn shared_saver_round_trips_while_an_existing_reader_is_held() {
    let directory = TestDirectory::new();
    let path = directory.join("shared.snapshot");
    let inner = Arc::new(RwLock::new(nullable_database()));
    let shared = SharedDatabase::from_arc(Arc::clone(&inner));
    let reader = inner.read().unwrap();
    let rows = [None, Some(7), Some(7), None];
    let (snapshot_codec, payload_codec) = exact_codecs(&rows);

    shared
        .try_save_int64_table_rle_to_file("READINGS", &path, snapshot_codec, payload_codec)
        .unwrap();

    assert_eq!(reader.catalog().table("readings").unwrap().row_count(), 4);
    drop(reader);
    let mut reopened = Database::new();
    reopened
        .restore_int64_table_rle_from_file(
            "archive",
            path,
            Schema::int64("Measurement", true),
            8,
            snapshot_codec,
            payload_codec,
        )
        .unwrap();
    let [Column::NullableInt64(restored)] = reopened.catalog().table("archive").unwrap().columns()
    else {
        panic!("shared RLE save must preserve nullable rows");
    };
    assert_eq!(restored, &rows);
}

#[test]
fn shared_saver_returns_busy_before_validation_or_destination_access() {
    let directory = TestDirectory::new();
    let path = directory.join("preserved.snapshot");
    let original = b"existing destination bytes";
    fs::write(&path, original).unwrap();
    let inner = Arc::new(RwLock::new(nullable_database()));
    let shared = SharedDatabase::from_arc(Arc::clone(&inner));
    let writer = inner.write().unwrap();
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        sender
            .send(shared.try_save_int64_table_rle_to_file(
                "missing",
                path,
                SnapshotCodec::new(1),
                NullableI64RlePayloadCodec::new(0, 0, 0),
            ))
            .unwrap();
    });

    let error = receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("RLE snapshot lock acquisition must not wait for the writer")
        .unwrap_err();
    assert!(matches!(
        error,
        SharedDatabaseRleSnapshotSaveError::DatabaseBusy
    ));
    assert!(!error.destination_was_replaced());
    assert_eq!(
        fs::read(directory.join("preserved.snapshot")).unwrap(),
        original
    );

    drop(writer);
    worker.join().unwrap();
}

#[test]
fn shared_saver_preserves_typed_failures_and_reports_poisoning() {
    let directory = TestDirectory::new();
    let path = directory.join("preserved.snapshot");
    let original = b"existing destination bytes";
    fs::write(&path, original).unwrap();
    let shared = SharedDatabase::new(nullable_database());
    let rows = [None, Some(7), Some(7), None];
    let (_, exact_payload_len) = rle_shape(&rows);

    let error = shared
        .try_save_int64_table_rle_to_file(
            "readings",
            &path,
            SnapshotCodec::new(exact_payload_len),
            NullableI64RlePayloadCodec::new(rows.len(), 2, exact_payload_len),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        SharedDatabaseRleSnapshotSaveError::Snapshot(DatabaseRleSnapshotSaveError::Snapshot(
            Int64TableRleFileSaveError::Payload(
                NullableI64RlePayloadError::RunLimitExceeded { .. }
            )
        ))
    ));
    assert!(!error.destination_was_replaced());
    assert_eq!(fs::read(&path).unwrap(), original);

    let inner = Arc::new(RwLock::new(nullable_database()));
    let poisoned = SharedDatabase::from_arc(Arc::clone(&inner));
    let poisoner = thread::spawn(move || {
        let _guard = inner.write().unwrap();
        panic!("poison the database lock");
    });
    assert!(poisoner.join().is_err());

    let poison_error = poisoned
        .try_save_int64_table_rle_to_file(
            "readings",
            &path,
            SnapshotCodec::new(exact_payload_len),
            NullableI64RlePayloadCodec::new(rows.len(), 3, exact_payload_len),
        )
        .unwrap_err();
    assert!(matches!(
        poison_error,
        SharedDatabaseRleSnapshotSaveError::LockPoisoned
    ));
    assert!(!poison_error.destination_was_replaced());
    assert_eq!(fs::read(path).unwrap(), original);
}
