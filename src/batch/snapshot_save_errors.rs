//! Error contracts for saving batch Int64 snapshots on Unix.
//!
//! Table validation and persistence operations remain in the engine and snapshot layer.

use std::fmt;

use crate::batch::error::Error;
use crate::batch::value::DataType;
use crate::snapshot::{Int64TablePayloadFileSaveError, Int64TableRleFileSaveError};

/// A failure while atomically saving one batch table as a self-describing
/// nullable or non-nullable `Int64` snapshot.
#[derive(Debug)]
pub enum DatabaseSnapshotSaveError {
    /// The requested table was not present in the database.
    Table(Error),
    /// The selected table does not have exactly one physical column.
    UnsupportedColumnCount {
        /// The stored display name of the selected table.
        table: String,
        /// The number of physical columns in the selected table.
        column_count: usize,
    },
    /// The selected table's only physical column is not `Int64`.
    UnsupportedColumnType {
        /// The stored display name of the unsupported column.
        column: String,
        /// The physical type found in the batch table.
        data_type: DataType,
    },
    /// Legacy nullable-column rejection retained for source compatibility.
    ///
    /// Nullable `Int64` columns are now supported, so
    /// [`Database::save_int64_table_to_file`](crate::batch::engine::Database::save_int64_table_to_file)
    /// no longer returns this variant.
    NullableColumn {
        /// The stored display name of the nullable column.
        column: String,
    },
    /// Encoding or atomically replacing the snapshot failed.
    Snapshot(Int64TablePayloadFileSaveError),
}

/// A failure while atomically saving one batch table as a row-only,
/// RLE-compressed nullable or non-nullable `Int64` snapshot.
#[derive(Debug)]
pub enum DatabaseRleSnapshotSaveError {
    /// The requested table was not present in the database.
    Table(Error),
    /// The selected table does not have exactly one physical column.
    UnsupportedColumnCount {
        /// The stored display name of the selected table.
        table: String,
        /// The number of physical columns in the selected table.
        column_count: usize,
    },
    /// The selected table's only physical column is not `Int64` or
    /// `Nullable(Int64)`.
    UnsupportedColumnType {
        /// The stored display name of the unsupported column.
        column: String,
        /// The physical type found in the batch table.
        data_type: DataType,
    },
    /// RLE encoding or atomically replacing the snapshot failed.
    Snapshot(Int64TableRleFileSaveError),
}

impl DatabaseSnapshotSaveError {
    /// Returns whether the destination was replaced before this error occurred.
    ///
    /// Shape validation, payload encoding, and every replacement failure before
    /// the rename return `false`. Only post-rename directory-sync uncertainty
    /// returns `true`.
    pub const fn destination_was_replaced(&self) -> bool {
        match self {
            Self::Snapshot(error) => error.destination_was_replaced(),
            Self::Table(_)
            | Self::UnsupportedColumnCount { .. }
            | Self::UnsupportedColumnType { .. }
            | Self::NullableColumn { .. } => false,
        }
    }
}

impl DatabaseRleSnapshotSaveError {
    /// Returns whether the destination was replaced before this error occurred.
    ///
    /// Table validation, RLE encoding, and every replacement failure before
    /// the rename return `false`. Only post-rename directory-sync uncertainty
    /// returns `true`.
    pub const fn destination_was_replaced(&self) -> bool {
        match self {
            Self::Snapshot(error) => error.destination_was_replaced(),
            Self::Table(_)
            | Self::UnsupportedColumnCount { .. }
            | Self::UnsupportedColumnType { .. } => false,
        }
    }
}

impl fmt::Display for DatabaseSnapshotSaveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Table(error) => error.fmt(formatter),
            Self::UnsupportedColumnCount {
                table,
                column_count,
            } => write!(
                formatter,
                "table '{table}' has {column_count} columns; batch snapshot save requires exactly one Int64 column"
            ),
            Self::UnsupportedColumnType { column, data_type } => write!(
                formatter,
                "column '{column}' has type {data_type}; batch snapshot save requires exactly one Int64 column"
            ),
            Self::NullableColumn { column } => write!(
                formatter,
                "column '{column}' is nullable; batch snapshot save requires exactly one non-nullable Int64 column"
            ),
            Self::Snapshot(error) => write!(formatter, "could not save snapshot: {error}"),
        }
    }
}

impl fmt::Display for DatabaseRleSnapshotSaveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Table(error) => error.fmt(formatter),
            Self::UnsupportedColumnCount {
                table,
                column_count,
            } => write!(
                formatter,
                "table '{table}' has {column_count} columns; batch RLE snapshot save requires exactly one Int64 or Nullable(Int64) column"
            ),
            Self::UnsupportedColumnType { column, data_type } => write!(
                formatter,
                "column '{column}' has type {data_type}; batch RLE snapshot save requires Int64 or Nullable(Int64)"
            ),
            Self::Snapshot(error) => write!(formatter, "could not save RLE snapshot: {error}"),
        }
    }
}

impl std::error::Error for DatabaseSnapshotSaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Table(error) => Some(error),
            Self::Snapshot(error) => Some(error),
            Self::UnsupportedColumnCount { .. }
            | Self::UnsupportedColumnType { .. }
            | Self::NullableColumn { .. } => None,
        }
    }
}

impl std::error::Error for DatabaseRleSnapshotSaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Table(error) => Some(error),
            Self::Snapshot(error) => Some(error),
            Self::UnsupportedColumnCount { .. } | Self::UnsupportedColumnType { .. } => None,
        }
    }
}

impl From<Error> for DatabaseSnapshotSaveError {
    fn from(error: Error) -> Self {
        Self::Table(error)
    }
}

impl From<Int64TablePayloadFileSaveError> for DatabaseSnapshotSaveError {
    fn from(error: Int64TablePayloadFileSaveError) -> Self {
        Self::Snapshot(error)
    }
}

impl From<Error> for DatabaseRleSnapshotSaveError {
    fn from(error: Error) -> Self {
        Self::Table(error)
    }
}

impl From<Int64TableRleFileSaveError> for DatabaseRleSnapshotSaveError {
    fn from(error: Int64TableRleFileSaveError) -> Self {
        Self::Snapshot(error)
    }
}
