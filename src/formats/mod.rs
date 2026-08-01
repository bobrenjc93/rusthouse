//! Streaming bulk formats with bounded parsing and transactional table ingestion.

mod csv;
mod ndjson;

pub use csv::{
    CsvBatchReader, CsvExportOptions, CsvOptions, export_csv, ingest_csv, write_csv_batch,
};
pub use ndjson::{
    NdjsonBatchReader, NdjsonOptions, export_ndjson, ingest_ndjson, write_ndjson_batch,
};

use crate::storage::{Column, ColumnBatch, DataType, Schema, StorageError, Table};
use std::error::Error;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Highest supported JSON container depth for the recursive parser.
pub const MAX_JSON_NESTING_DEPTH: usize = 128;

/// Independent bounds applied while decoding a streaming input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatLimits {
    pub max_input_bytes: u64,
    pub max_rows: u64,
    pub max_fields_per_row: usize,
    pub max_field_bytes: usize,
    pub max_nesting_depth: usize,
    pub max_string_bytes: usize,
    pub max_record_bytes: usize,
    pub batch_rows: usize,
}

impl Default for FormatLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 256 * 1024 * 1024,
            max_rows: 10_000_000,
            max_fields_per_row: 1_024,
            max_field_bytes: 16 * 1024 * 1024,
            max_nesting_depth: 64,
            max_string_bytes: 16 * 1024 * 1024,
            max_record_bytes: 64 * 1024 * 1024,
            batch_rows: 8_192,
        }
    }
}

impl FormatLimits {
    pub(crate) fn validate(&self, schema: &Schema) -> Result<(), FormatError> {
        for (name, value) in [
            ("max_fields_per_row", self.max_fields_per_row),
            ("max_field_bytes", self.max_field_bytes),
            ("max_nesting_depth", self.max_nesting_depth),
            ("max_string_bytes", self.max_string_bytes),
            ("max_record_bytes", self.max_record_bytes),
            ("batch_rows", self.batch_rows),
        ] {
            if value == 0 {
                return Err(FormatError::InvalidOption(format!(
                    "{name} must be greater than zero"
                )));
            }
        }
        if schema.len() > self.max_fields_per_row {
            return Err(FormatError::LimitExceeded {
                kind: LimitKind::FieldsPerRow,
                limit: self.max_fields_per_row as u64,
                row: None,
            });
        }
        Ok(())
    }

    pub(crate) fn validate_json(&self, schema: &Schema) -> Result<(), FormatError> {
        self.validate(schema)?;
        if self.max_nesting_depth > MAX_JSON_NESTING_DEPTH {
            return Err(FormatError::InvalidOption(format!(
                "max_nesting_depth cannot exceed {MAX_JSON_NESTING_DEPTH}"
            )));
        }
        Ok(())
    }
}

/// The bounded resource whose configured maximum was exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitKind {
    InputBytes,
    Rows,
    FieldsPerRow,
    FieldBytes,
    NestingDepth,
    StringBytes,
    RecordBytes,
}

impl fmt::Display for LimitKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputBytes => f.write_str("input bytes"),
            Self::Rows => f.write_str("rows"),
            Self::FieldsPerRow => f.write_str("fields per row"),
            Self::FieldBytes => f.write_str("field bytes"),
            Self::NestingDepth => f.write_str("JSON nesting depth"),
            Self::StringBytes => f.write_str("decoded string bytes"),
            Self::RecordBytes => f.write_str("record bytes"),
        }
    }
}

/// A typed failure from parsing, conversion, staging, or destination validation.
#[derive(Debug)]
pub enum FormatError {
    Io(io::Error),
    InvalidOption(String),
    LimitExceeded {
        kind: LimitKind,
        limit: u64,
        row: Option<u64>,
    },
    CsvSyntax {
        row: u64,
        message: String,
    },
    JsonSyntax {
        row: u64,
        message: String,
    },
    InvalidUtf8 {
        row: u64,
        column: String,
    },
    FieldCount {
        row: u64,
        expected: usize,
        actual: usize,
    },
    HeaderMismatch {
        expected: Vec<String>,
        actual: Vec<String>,
    },
    UnknownField {
        row: u64,
        field: String,
    },
    DuplicateField {
        row: u64,
        field: String,
    },
    MissingField {
        row: u64,
        field: String,
    },
    NullNotAllowed {
        row: u64,
        column: String,
    },
    Conversion {
        row: u64,
        column: String,
        data_type: DataType,
        value: String,
    },
    Storage(StorageError),
    Staging(String),
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "format I/O failed: {error}"),
            Self::InvalidOption(message) => write!(f, "invalid format option: {message}"),
            Self::LimitExceeded { kind, limit, row } => match row {
                Some(row) => write!(f, "{kind} limit {limit} exceeded at row {row}"),
                None => write!(f, "{kind} limit {limit} exceeded"),
            },
            Self::CsvSyntax { row, message } => {
                write!(f, "invalid CSV at row {row}: {message}")
            }
            Self::JsonSyntax { row, message } => {
                write!(f, "invalid NDJSON at row {row}: {message}")
            }
            Self::InvalidUtf8 { row, column } => {
                write!(f, "invalid UTF-8 in column {column:?} at row {row}")
            }
            Self::FieldCount {
                row,
                expected,
                actual,
            } => write!(f, "row {row} has {actual} fields, expected {expected}"),
            Self::HeaderMismatch { expected, actual } => write!(
                f,
                "CSV header {:?} does not match schema {:?}",
                actual, expected
            ),
            Self::UnknownField { row, field } => {
                write!(f, "unknown field {field:?} at row {row}")
            }
            Self::DuplicateField { row, field } => {
                write!(f, "duplicate field {field:?} at row {row}")
            }
            Self::MissingField { row, field } => {
                write!(f, "missing field {field:?} at row {row}")
            }
            Self::NullNotAllowed { row, column } => {
                write!(f, "NULL is not allowed in column {column:?} at row {row}")
            }
            Self::Conversion {
                row,
                column,
                data_type,
                value,
            } => write!(
                f,
                "cannot convert {value:?} to {data_type} for column {column:?} at row {row}"
            ),
            Self::Storage(error) => write!(f, "columnar storage rejected data: {error}"),
            Self::Staging(message) => write!(f, "staged ingestion failed: {message}"),
        }
    }
}

impl Error for FormatError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Storage(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for FormatError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<StorageError> for FormatError {
    fn from(value: StorageError) -> Self {
        Self::Storage(value)
    }
}

pub(crate) struct LimitedInput<R> {
    inner: R,
    bytes: u64,
    max_bytes: u64,
}

impl<R: BufRead> LimitedInput<R> {
    pub(crate) fn new(inner: R, max_bytes: u64) -> Self {
        Self {
            inner,
            bytes: 0,
            max_bytes,
        }
    }

    pub(crate) fn read_byte(&mut self) -> Result<Option<u8>, FormatError> {
        let byte = {
            let available = self.inner.fill_buf()?;
            available.first().copied()
        };
        let Some(byte) = byte else {
            return Ok(None);
        };
        if self.bytes == self.max_bytes {
            return Err(FormatError::LimitExceeded {
                kind: LimitKind::InputBytes,
                limit: self.max_bytes,
                row: None,
            });
        }
        self.inner.consume(1);
        self.bytes += 1;
        Ok(Some(byte))
    }
}

pub(crate) fn empty_columns(schema: &Schema) -> Vec<Column> {
    schema
        .fields()
        .iter()
        .map(|field| Column::empty(field.data_type()))
        .collect()
}

pub(crate) fn push_null(
    column: &mut Column,
    column_name: &str,
    nullable: bool,
    row: u64,
) -> Result<(), FormatError> {
    if !nullable {
        return Err(FormatError::NullNotAllowed {
            row,
            column: column_name.to_owned(),
        });
    }
    match column {
        Column::Int64(values) => values.push(None),
        Column::Float64(values) => values.push(None),
        Column::Bool(values) => values.push(None),
        Column::String(values) => values.push(None),
    }
    Ok(())
}

pub(crate) fn finish_batch(
    schema: &Schema,
    columns: Vec<Column>,
) -> Result<ColumnBatch, FormatError> {
    ColumnBatch::new(schema, columns).map_err(FormatError::Storage)
}

pub(crate) fn ingest_batches<I>(batches: I, destination: &mut Table) -> Result<u64, FormatError>
where
    I: IntoIterator<Item = Result<ColumnBatch, FormatError>>,
{
    let mut staged = StagedBatches::create()?;
    let mut total = 0_u64;
    for batch in batches {
        let batch = batch?;
        total = total
            .checked_add(batch.rows() as u64)
            .ok_or_else(|| FormatError::Staging("row count overflow".to_owned()))?;
        staged.write_batch(&batch)?;
    }
    staged.rewind()?;

    let original_rows = destination.rows();
    let apply_result = (|| {
        while let Some(batch) = staged.read_batch(destination.schema())? {
            destination.append_batch(&batch)?;
        }
        Ok(())
    })();
    if let Err(error) = apply_result {
        destination.truncate(original_rows);
        return Err(error);
    }
    Ok(total)
}

static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct StagedBatches {
    file: Option<File>,
    path: PathBuf,
}

impl StagedBatches {
    fn create() -> Result<Self, FormatError> {
        let directory = std::env::temp_dir();
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for _ in 0..128 {
            let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = directory.join(format!(
                "rusthouse-ingest-{}-{timestamp}-{sequence}.tmp",
                std::process::id()
            ));
            let mut options = OpenOptions::new();
            options.read(true).write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(mut file) => {
                    let result = file.write_all(b"RHBATCH1");
                    if let Err(error) = result {
                        drop(file);
                        let _ = std::fs::remove_file(&path);
                        return Err(FormatError::Io(error));
                    }
                    return Ok(Self {
                        file: Some(file),
                        path,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(FormatError::Io(error)),
            }
        }
        Err(FormatError::Staging(
            "could not allocate a unique staging file".to_owned(),
        ))
    }

    fn write_batch(&mut self, batch: &ColumnBatch) -> Result<(), FormatError> {
        let file = self.file.as_mut().expect("staging file is open");
        write_u64(file, batch.rows() as u64)?;
        for column in batch.columns() {
            match column {
                Column::Int64(values) => {
                    for value in values {
                        write_presence(file, value.is_some())?;
                        if let Some(value) = value {
                            file.write_all(&value.to_le_bytes())?;
                        }
                    }
                }
                Column::Float64(values) => {
                    for value in values {
                        write_presence(file, value.is_some())?;
                        if let Some(value) = value {
                            file.write_all(&value.to_bits().to_le_bytes())?;
                        }
                    }
                }
                Column::Bool(values) => {
                    for value in values {
                        write_presence(file, value.is_some())?;
                        if let Some(value) = value {
                            file.write_all(&[u8::from(*value)])?;
                        }
                    }
                }
                Column::String(values) => {
                    for value in values {
                        write_presence(file, value.is_some())?;
                        if let Some(value) = value {
                            write_u64(file, value.len() as u64)?;
                            file.write_all(value.as_bytes())?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn rewind(&mut self) -> Result<(), FormatError> {
        let file = self.file.as_mut().expect("staging file is open");
        file.flush()?;
        file.seek(SeekFrom::Start(0))?;
        let mut magic = [0_u8; 8];
        file.read_exact(&mut magic)?;
        if &magic != b"RHBATCH1" {
            return Err(FormatError::Staging(
                "invalid staging file header".to_owned(),
            ));
        }
        Ok(())
    }

    fn read_batch(&mut self, schema: &Schema) -> Result<Option<ColumnBatch>, FormatError> {
        let file = self.file.as_mut().expect("staging file is open");
        let Some(rows) = read_optional_u64(file)? else {
            return Ok(None);
        };
        let rows = usize::try_from(rows)
            .map_err(|_| FormatError::Staging("batch row count is too large".to_owned()))?;
        let mut columns = empty_columns(schema);
        for column in &mut columns {
            for _ in 0..rows {
                let present = read_presence(file)?;
                match column {
                    Column::Int64(values) => values.push(if present {
                        Some(i64::from_le_bytes(read_array(file)?))
                    } else {
                        None
                    }),
                    Column::Float64(values) => values.push(if present {
                        Some(f64::from_bits(u64::from_le_bytes(read_array(file)?)))
                    } else {
                        None
                    }),
                    Column::Bool(values) => values.push(if present {
                        let byte = read_array::<1>(file)?[0];
                        match byte {
                            0 => Some(false),
                            1 => Some(true),
                            _ => {
                                return Err(FormatError::Staging(
                                    "invalid staged Boolean".to_owned(),
                                ));
                            }
                        }
                    } else {
                        None
                    }),
                    Column::String(values) => values.push(if present {
                        let length = read_u64(file)?;
                        let length = usize::try_from(length).map_err(|_| {
                            FormatError::Staging("staged string is too large".to_owned())
                        })?;
                        let mut bytes = vec![0_u8; length];
                        file.read_exact(&mut bytes)?;
                        Some(String::from_utf8(bytes).map_err(|_| {
                            FormatError::Staging("staged string is not UTF-8".to_owned())
                        })?)
                    } else {
                        None
                    }),
                }
            }
        }
        finish_batch(schema, columns).map(Some)
    }
}

impl Drop for StagedBatches {
    fn drop(&mut self) {
        drop(self.file.take());
        let _ = std::fs::remove_file(&self.path);
    }
}

fn write_presence(writer: &mut File, present: bool) -> io::Result<()> {
    writer.write_all(&[u8::from(present)])
}

fn read_presence(reader: &mut File) -> Result<bool, FormatError> {
    match read_array::<1>(reader)?[0] {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(FormatError::Staging(
            "invalid staged presence marker".to_owned(),
        )),
    }
}

fn write_u64(writer: &mut File, value: u64) -> io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn read_u64(reader: &mut File) -> Result<u64, FormatError> {
    Ok(u64::from_le_bytes(read_array(reader)?))
}

fn read_optional_u64(reader: &mut File) -> Result<Option<u64>, FormatError> {
    let mut first = [0_u8; 1];
    match reader.read(&mut first) {
        Ok(0) => Ok(None),
        Ok(1) => {
            let mut remaining = [0_u8; 7];
            reader.read_exact(&mut remaining)?;
            let mut bytes = [0_u8; 8];
            bytes[0] = first[0];
            bytes[1..].copy_from_slice(&remaining);
            Ok(Some(u64::from_le_bytes(bytes)))
        }
        Ok(_) => unreachable!(),
        Err(error) => Err(FormatError::Io(error)),
    }
}

fn read_array<const N: usize>(reader: &mut File) -> Result<[u8; N], FormatError> {
    let mut bytes = [0_u8; N];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}
