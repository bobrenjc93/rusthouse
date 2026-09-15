#![cfg(unix)]

use std::error::Error as StdError;
use std::io;

use rusthouse::batch::error::Error;
use rusthouse::batch::value::DataType;
use rusthouse::{
    DatabaseRleSnapshotSaveError, DatabaseSnapshotSaveError, Int64TablePayloadError,
    Int64TablePayloadFileSaveError, Int64TableRleFileSaveError, NullableI64RlePayloadError,
    SnapshotReplaceError,
};

fn source_as<'a, T: StdError + 'static>(error: &'a (dyn StdError + 'static)) -> &'a T {
    error.source().unwrap().downcast_ref::<T>().unwrap()
}

#[test]
fn public_import_paths_refer_to_the_same_error_types() {
    let error = DatabaseSnapshotSaveError::NullableColumn {
        column: "reading".into(),
    };
    let error: rusthouse::batch::DatabaseSnapshotSaveError = error;
    let error: rusthouse::batch::engine::DatabaseSnapshotSaveError = error;
    let _: DatabaseSnapshotSaveError = error;

    let error = DatabaseRleSnapshotSaveError::UnsupportedColumnCount {
        table: "readings".into(),
        column_count: 2,
    };
    let error: rusthouse::batch::DatabaseRleSnapshotSaveError = error;
    let error: rusthouse::batch::engine::DatabaseRleSnapshotSaveError = error;
    let _: DatabaseRleSnapshotSaveError = error;
}

#[test]
fn table_conversions_preserve_variants_display_and_typed_sources() {
    let table_error = Error::TableNotFound("missing".into());
    let snapshot = DatabaseSnapshotSaveError::from(table_error.clone());
    let rle = DatabaseRleSnapshotSaveError::from(table_error.clone());
    assert!(matches!(&snapshot, DatabaseSnapshotSaveError::Table(error) if error == &table_error));
    assert!(matches!(&rle, DatabaseRleSnapshotSaveError::Table(error) if error == &table_error));
    assert!(!snapshot.destination_was_replaced());
    assert!(!rle.destination_was_replaced());

    for error in [&snapshot as &dyn StdError, &rle] {
        assert_eq!(error.to_string(), "table 'missing' does not exist");
        let source = source_as::<Error>(error);
        assert_eq!(source, &table_error);
        assert!(source.source().is_none());
    }
}

#[test]
fn validation_variants_have_no_source_and_never_replace_the_destination() {
    let snapshot_cases = [
        (
            DatabaseSnapshotSaveError::UnsupportedColumnCount {
                table: "readings".into(),
                column_count: 2,
            },
            "table 'readings' has 2 columns; batch snapshot save requires exactly one Int64 column",
        ),
        (
            DatabaseSnapshotSaveError::UnsupportedColumnType {
                column: "reading".into(),
                data_type: DataType::Float64,
            },
            "column 'reading' has type Float64; batch snapshot save requires exactly one Int64 column",
        ),
        (
            DatabaseSnapshotSaveError::NullableColumn {
                column: "reading".into(),
            },
            "column 'reading' is nullable; batch snapshot save requires exactly one non-nullable Int64 column",
        ),
    ];
    for (error, message) in snapshot_cases {
        assert_eq!(error.to_string(), message);
        assert!(error.source().is_none());
        assert!(!error.destination_was_replaced());
    }

    let rle_cases = [
        (
            DatabaseRleSnapshotSaveError::UnsupportedColumnCount {
                table: "readings".into(),
                column_count: 2,
            },
            "table 'readings' has 2 columns; batch RLE snapshot save requires exactly one Int64 or Nullable(Int64) column",
        ),
        (
            DatabaseRleSnapshotSaveError::UnsupportedColumnType {
                column: "reading".into(),
                data_type: DataType::Float64,
            },
            "column 'reading' has type Float64; batch RLE snapshot save requires Int64 or Nullable(Int64)",
        ),
    ];
    for (error, message) in rle_cases {
        assert_eq!(error.to_string(), message);
        assert!(error.source().is_none());
        assert!(!error.destination_was_replaced());
    }
}

#[test]
fn snapshot_conversions_preserve_payload_variants_and_source_chains() {
    let snapshot = DatabaseSnapshotSaveError::from(Int64TablePayloadFileSaveError::Payload(
        Int64TablePayloadError::RowLimitExceeded {
            row_count: 4,
            max_rows: 3,
        },
    ));
    assert!(matches!(
        &snapshot,
        DatabaseSnapshotSaveError::Snapshot(Int64TablePayloadFileSaveError::Payload(_))
    ));
    assert_eq!(
        snapshot.to_string(),
        "could not save snapshot: could not encode self-describing table payload: Int64 table payload has 4 rows, exceeding the limit of 3"
    );
    let file_error = source_as::<Int64TablePayloadFileSaveError>(&snapshot);
    let payload_error = source_as::<Int64TablePayloadError>(file_error);
    assert_eq!(
        payload_error,
        &Int64TablePayloadError::RowLimitExceeded {
            row_count: 4,
            max_rows: 3,
        }
    );
    assert!(payload_error.source().is_none());
    assert!(!snapshot.destination_was_replaced());

    let rle = DatabaseRleSnapshotSaveError::from(Int64TableRleFileSaveError::Payload(
        NullableI64RlePayloadError::RunLimitExceeded {
            run_count: 4,
            max_runs: 3,
        },
    ));
    assert!(matches!(
        &rle,
        DatabaseRleSnapshotSaveError::Snapshot(Int64TableRleFileSaveError::Payload(_))
    ));
    assert_eq!(
        rle.to_string(),
        "could not save RLE snapshot: could not encode RLE table payload: nullable Int64 RLE payload has 4 runs, exceeding the limit of 3"
    );
    let file_error = source_as::<Int64TableRleFileSaveError>(&rle);
    let payload_error = source_as::<NullableI64RlePayloadError>(file_error);
    assert_eq!(
        payload_error,
        &NullableI64RlePayloadError::RunLimitExceeded {
            run_count: 4,
            max_runs: 3,
        }
    );
    assert!(payload_error.source().is_none());
    assert!(!rle.destination_was_replaced());
}

fn replacement_error(destination_was_replaced: bool) -> SnapshotReplaceError {
    let error = io::Error::other("injected I/O failure");
    if destination_was_replaced {
        SnapshotReplaceError::SyncDirectory(error)
    } else {
        SnapshotReplaceError::Rename(error)
    }
}

#[test]
fn replacement_chains_distinguish_rename_failure_from_directory_sync_uncertainty() {
    for (replaced, message) in [
        (
            false,
            "could not replace snapshot file: injected I/O failure",
        ),
        (
            true,
            "snapshot was replaced, but its parent directory could not be synced: injected I/O failure",
        ),
    ] {
        let snapshot = DatabaseSnapshotSaveError::from(Int64TablePayloadFileSaveError::Replace(
            replacement_error(replaced),
        ));
        let rle = DatabaseRleSnapshotSaveError::from(Int64TableRleFileSaveError::Replace(
            replacement_error(replaced),
        ));
        assert_eq!(snapshot.destination_was_replaced(), replaced);
        assert_eq!(rle.destination_was_replaced(), replaced);
        assert_eq!(
            snapshot.to_string(),
            format!("could not save snapshot: could not replace table snapshot: {message}")
        );
        assert_eq!(
            rle.to_string(),
            format!("could not save RLE snapshot: could not replace table snapshot: {message}")
        );

        let snapshot_file_error = source_as::<Int64TablePayloadFileSaveError>(&snapshot);
        let rle_file_error = source_as::<Int64TableRleFileSaveError>(&rle);
        for error in [
            snapshot_file_error as &dyn StdError,
            rle_file_error as &dyn StdError,
        ] {
            let replacement = source_as::<SnapshotReplaceError>(error);
            assert_eq!(
                matches!(replacement, SnapshotReplaceError::SyncDirectory(_)),
                replaced
            );
            let source = source_as::<io::Error>(replacement);
            assert_eq!(source.kind(), io::ErrorKind::Other);
            assert_eq!(source.to_string(), "injected I/O failure");
            assert!(source.source().is_none());
        }
    }
}
