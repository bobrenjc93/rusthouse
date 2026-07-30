use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(all(test, unix))]
use std::cell::Cell;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, fchown};

use crate::catalog::Catalog;
use crate::error::{Error, Result};
use crate::storage::{Column, ColumnDef, Table};
use crate::value::DataType;

const MAGIC: &[u8; 8] = b"RSHOUSE\0";
const VERSION: u32 = 1;
const HEADER_LEN: usize = 8 + 4 + 8 + 4;
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[cfg(all(test, unix))]
thread_local! {
    static FAIL_DIRECTORY_SYNC: Cell<bool> = const { Cell::new(false) };
}

pub(crate) struct CheckpointError {
    error: Error,
    committed: bool,
}

impl CheckpointError {
    pub(crate) fn committed(&self) -> bool {
        self.committed
    }

    pub(crate) fn into_error(self) -> Error {
        self.error
    }

    fn after_commit(error: Error) -> Self {
        Self {
            error,
            committed: true,
        }
    }
}

impl From<Error> for CheckpointError {
    fn from(error: Error) -> Self {
        Self {
            error,
            committed: false,
        }
    }
}

type CheckpointResult<T> = std::result::Result<T, CheckpointError>;

pub(crate) fn ensure_supported(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let _ = path;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        Err(unsupported_platform(path))
    }
}

pub(crate) fn load(path: &Path) -> Result<Catalog> {
    ensure_supported(path)?;
    match fs::read(path) {
        Ok(bytes) => decode(&bytes, path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Catalog::new()),
        Err(error) => Err(io_error("read", path, error)),
    }
}

pub(crate) fn checkpoint(catalog: &Catalog, path: &Path) -> CheckpointResult<()> {
    ensure_supported(path)?;
    let security_metadata = existing_security_metadata(path)?;
    let snapshot = encode(catalog)?;
    let parent = usable_parent(path);
    let file_name = path
        .file_name()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| Error::Persistence {
            operation: "write".to_owned(),
            path: path.to_owned(),
            message: "the database path must name a file".to_owned(),
        })?;
    let mut temporary = TemporaryFile::create(parent, file_name, path)?;
    temporary
        .file
        .as_mut()
        .expect("temporary file is open")
        .write_all(&snapshot)
        .map_err(|error| io_error("write", path, error))?;
    temporary.preserve_security_metadata(security_metadata, path)?;
    temporary
        .file
        .as_ref()
        .expect("temporary file is open")
        .sync_all()
        .map_err(|error| io_error("sync", path, error))?;
    drop(temporary.file.take());

    fs::rename(&temporary.path, path).map_err(|error| io_error("replace", path, error))?;
    temporary.renamed = true;
    sync_directory(parent, path).map_err(CheckpointError::after_commit)?;
    Ok(())
}

fn encode(catalog: &Catalog) -> Result<Vec<u8>> {
    let mut payload = Vec::new();
    let mut tables = catalog.tables().collect::<Vec<_>>();
    tables.sort_unstable_by_key(|table| table.name().to_ascii_lowercase());
    put_len(&mut payload, tables.len(), "table count")?;

    for table in tables {
        put_string(&mut payload, table.name())?;
        put_len(&mut payload, table.schema().len(), "column count")?;
        put_len(&mut payload, table.row_count(), "row count")?;
        for field in table.schema() {
            put_string(&mut payload, &field.name)?;
            payload.push(type_tag(field.data_type));
        }
        for column in table.columns() {
            match column {
                Column::Int64(values) => {
                    for value in values {
                        payload.extend_from_slice(&value.to_le_bytes());
                    }
                }
                Column::Float64(values) => {
                    for value in values {
                        payload.extend_from_slice(&value.to_bits().to_le_bytes());
                    }
                }
                Column::Bool(values) => {
                    payload.extend(values.iter().map(|value| u8::from(*value)));
                }
                Column::String(values) => {
                    for value in values {
                        put_string(&mut payload, value)?;
                    }
                }
            }
        }
    }

    let payload_len = u64::try_from(payload.len())
        .map_err(|_| Error::InvalidQuery("database snapshot is too large to encode".to_owned()))?;
    let mut snapshot = Vec::with_capacity(HEADER_LEN + payload.len());
    snapshot.extend_from_slice(MAGIC);
    snapshot.extend_from_slice(&VERSION.to_le_bytes());
    snapshot.extend_from_slice(&payload_len.to_le_bytes());
    snapshot.extend_from_slice(&snapshot_checksum(payload_len, &payload).to_le_bytes());
    snapshot.extend_from_slice(&payload);
    Ok(snapshot)
}

fn decode(snapshot: &[u8], path: &Path) -> Result<Catalog> {
    if snapshot.len() < HEADER_LEN {
        return Err(invalid(path, "snapshot is truncated"));
    }
    if &snapshot[..MAGIC.len()] != MAGIC {
        return Err(invalid(path, "snapshot magic does not match RustHouse"));
    }

    let version = u32::from_le_bytes(snapshot[8..12].try_into().expect("fixed header range"));
    if version != VERSION {
        return Err(Error::UnsupportedSnapshotVersion {
            path: path.to_owned(),
            version,
            supported: VERSION,
        });
    }
    let payload_len_u64 =
        u64::from_le_bytes(snapshot[12..20].try_into().expect("fixed header range"));
    let payload_len = usize::try_from(payload_len_u64)
        .map_err(|_| invalid(path, "declared payload length is too large"))?;
    let expected_len = HEADER_LEN
        .checked_add(payload_len)
        .ok_or_else(|| invalid(path, "declared payload length is too large"))?;
    if snapshot.len() < expected_len {
        return Err(invalid(path, "snapshot is truncated"));
    }
    if snapshot.len() > expected_len {
        return Err(invalid(path, "snapshot has trailing data"));
    }

    let expected_checksum =
        u32::from_le_bytes(snapshot[20..24].try_into().expect("fixed header range"));
    if snapshot_checksum(payload_len_u64, &snapshot[HEADER_LEN..]) != expected_checksum {
        return Err(invalid(path, "snapshot checksum does not match"));
    }

    let mut reader = Reader::new(&snapshot[HEADER_LEN..], path);
    let table_count = reader.length("table count")?;
    reader.ensure_count(table_count, 8, "table count")?;
    let mut catalog = Catalog::new();
    for _ in 0..table_count {
        let name = reader.string("table name")?;
        let column_count = reader.length("column count")?;
        if column_count == 0 {
            return Err(invalid(path, "a table has no columns"));
        }
        reader.ensure_count(column_count, 9, "column count")?;
        let row_count = reader.length("row count")?;
        let mut schema = Vec::new();
        schema
            .try_reserve_exact(column_count)
            .map_err(|_| invalid(path, "column count is too large"))?;
        for _ in 0..column_count {
            let field_name = reader.string("column name")?;
            let data_type = data_type(reader.byte("column type")?)
                .ok_or_else(|| invalid(path, "snapshot contains an unknown column type"))?;
            schema.push(ColumnDef {
                name: field_name,
                data_type,
            });
        }

        let mut columns = Vec::new();
        columns
            .try_reserve_exact(column_count)
            .map_err(|_| invalid(path, "column count is too large"))?;
        for field in &schema {
            columns.push(reader.column(field.data_type, row_count)?);
        }
        let table = Table::from_columns(name, schema, columns, row_count)
            .map_err(|error| invalid(path, format!("invalid table data: {error}")))?;
        catalog
            .insert_table(table)
            .map_err(|error| invalid(path, format!("invalid catalog data: {error}")))?;
    }
    if !reader.is_empty() {
        return Err(invalid(path, "snapshot payload has trailing data"));
    }
    Ok(catalog)
}

struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
    path: &'a Path,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8], path: &'a Path) -> Self {
        Self {
            bytes,
            position: 0,
            path,
        }
    }

    fn is_empty(&self) -> bool {
        self.position == self.bytes.len()
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.position
    }

    fn take(&mut self, length: usize, context: &str) -> Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(length)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| {
                invalid(
                    self.path,
                    format!("snapshot is truncated while reading {context}"),
                )
            })?;
        let value = &self.bytes[self.position..end];
        self.position = end;
        Ok(value)
    }

    fn byte(&mut self, context: &str) -> Result<u8> {
        Ok(self.take(1, context)?[0])
    }

    fn u64(&mut self, context: &str) -> Result<u64> {
        Ok(u64::from_le_bytes(
            self.take(8, context)?
                .try_into()
                .expect("fixed read length"),
        ))
    }

    fn length(&mut self, context: &str) -> Result<usize> {
        usize::try_from(self.u64(context)?)
            .map_err(|_| invalid(self.path, format!("{context} is too large")))
    }

    fn string(&mut self, context: &str) -> Result<String> {
        let length = self.length(&format!("{context} length"))?;
        let bytes = self.take(length, context)?;
        let value = std::str::from_utf8(bytes)
            .map_err(|_| invalid(self.path, format!("{context} is not valid UTF-8")))?;
        Ok(value.to_owned())
    }

    fn ensure_count(&self, count: usize, minimum_size: usize, context: &str) -> Result<()> {
        if count > self.remaining() / minimum_size {
            return Err(invalid(
                self.path,
                format!("{context} exceeds the snapshot payload"),
            ));
        }
        Ok(())
    }

    fn column(&mut self, data_type: DataType, row_count: usize) -> Result<Column> {
        match data_type {
            DataType::Int64 => {
                self.ensure_count(row_count, 8, "Int64 row count")?;
                let mut values = Vec::new();
                values
                    .try_reserve_exact(row_count)
                    .map_err(|_| invalid(self.path, "row count is too large"))?;
                for _ in 0..row_count {
                    values.push(i64::from_le_bytes(
                        self.take(8, "Int64 value")?
                            .try_into()
                            .expect("fixed read length"),
                    ));
                }
                Ok(Column::Int64(values))
            }
            DataType::Float64 => {
                self.ensure_count(row_count, 8, "Float64 row count")?;
                let mut values = Vec::new();
                values
                    .try_reserve_exact(row_count)
                    .map_err(|_| invalid(self.path, "row count is too large"))?;
                for _ in 0..row_count {
                    let bits = u64::from_le_bytes(
                        self.take(8, "Float64 value")?
                            .try_into()
                            .expect("fixed read length"),
                    );
                    values.push(f64::from_bits(bits));
                }
                Ok(Column::Float64(values))
            }
            DataType::Bool => {
                self.ensure_count(row_count, 1, "Bool row count")?;
                let mut values = Vec::new();
                values
                    .try_reserve_exact(row_count)
                    .map_err(|_| invalid(self.path, "row count is too large"))?;
                for _ in 0..row_count {
                    values.push(match self.byte("Bool value")? {
                        0 => false,
                        1 => true,
                        _ => return Err(invalid(self.path, "Bool value is neither 0 nor 1")),
                    });
                }
                Ok(Column::Bool(values))
            }
            DataType::String => {
                self.ensure_count(row_count, 8, "String row count")?;
                let mut values = Vec::new();
                values
                    .try_reserve_exact(row_count)
                    .map_err(|_| invalid(self.path, "row count is too large"))?;
                for _ in 0..row_count {
                    values.push(self.string("String value")?);
                }
                Ok(Column::String(values))
            }
        }
    }
}

struct TemporaryFile {
    file: Option<File>,
    path: PathBuf,
    renamed: bool,
}

impl TemporaryFile {
    fn create(parent: &Path, file_name: &OsStr, database_path: &Path) -> Result<Self> {
        loop {
            let mut temporary_name = file_name.to_os_string();
            temporary_name.push(format!(
                ".tmp.{}.{}",
                std::process::id(),
                TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            let temporary_path = parent.join(temporary_name);
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            match options.open(&temporary_path) {
                Ok(file) => {
                    return Ok(Self {
                        file: Some(file),
                        path: temporary_path,
                        renamed: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(io_error(
                        "create temporary snapshot for",
                        database_path,
                        error,
                    ));
                }
            }
        }
    }

    #[cfg(unix)]
    fn preserve_security_metadata(
        &self,
        metadata: Option<SecurityMetadata>,
        database_path: &Path,
    ) -> Result<()> {
        let Some(metadata) = metadata else {
            return Ok(());
        };
        let file = self.file.as_ref().expect("temporary file is open");
        let temporary_metadata = file
            .metadata()
            .map_err(|error| io_error("inspect temporary snapshot for", database_path, error))?;
        if temporary_metadata.uid() != metadata.uid || temporary_metadata.gid() != metadata.gid {
            fchown(file, Some(metadata.uid), Some(metadata.gid))
                .map_err(|error| io_error("preserve owner and group for", database_path, error))?;
        }
        file.set_permissions(fs::Permissions::from_mode(metadata.mode))
            .map_err(|error| io_error("preserve permissions for", database_path, error))
    }

    #[cfg(not(unix))]
    fn preserve_security_metadata(
        &self,
        _metadata: Option<SecurityMetadata>,
        database_path: &Path,
    ) -> Result<()> {
        Err(unsupported_platform(database_path))
    }
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if !self.renamed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn put_len(output: &mut Vec<u8>, value: usize, context: &str) -> Result<()> {
    let value = u64::try_from(value)
        .map_err(|_| Error::InvalidQuery(format!("{context} is too large to persist")))?;
    output.extend_from_slice(&value.to_le_bytes());
    Ok(())
}

fn put_string(output: &mut Vec<u8>, value: &str) -> Result<()> {
    put_len(output, value.len(), "string length")?;
    output.extend_from_slice(value.as_bytes());
    Ok(())
}

fn type_tag(data_type: DataType) -> u8 {
    match data_type {
        DataType::Int64 => 1,
        DataType::Float64 => 2,
        DataType::Bool => 3,
        DataType::String => 4,
    }
}

fn data_type(tag: u8) -> Option<DataType> {
    match tag {
        1 => Some(DataType::Int64),
        2 => Some(DataType::Float64),
        3 => Some(DataType::Bool),
        4 => Some(DataType::String),
        _ => None,
    }
}

fn usable_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

#[cfg(unix)]
#[derive(Clone, Copy)]
struct SecurityMetadata {
    mode: u32,
    uid: u32,
    gid: u32,
}

#[cfg(not(unix))]
struct SecurityMetadata;

#[cfg(unix)]
fn existing_security_metadata(path: &Path) -> Result<Option<SecurityMetadata>> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(Some(SecurityMetadata {
            mode: metadata.mode() & 0o7777,
            uid: metadata.uid(),
            gid: metadata.gid(),
        })),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error("inspect security metadata for", path, error)),
    }
}

#[cfg(not(unix))]
fn existing_security_metadata(path: &Path) -> Result<Option<SecurityMetadata>> {
    Err(unsupported_platform(path))
}

#[cfg(unix)]
fn sync_directory(parent: &Path, database_path: &Path) -> Result<()> {
    #[cfg(test)]
    if FAIL_DIRECTORY_SYNC.with(|fail| fail.replace(false)) {
        return Err(committed_sync_error(
            database_path,
            io::Error::other("injected directory sync failure"),
        ));
    }
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| committed_sync_error(database_path, error))
}

#[cfg(not(unix))]
fn sync_directory(_parent: &Path, database_path: &Path) -> Result<()> {
    Err(unsupported_platform(database_path))
}

#[cfg(all(test, unix))]
pub(crate) fn fail_next_directory_sync() {
    FAIL_DIRECTORY_SYNC.with(|fail| fail.set(true));
}

fn io_error(operation: &str, path: &Path, error: io::Error) -> Error {
    Error::Persistence {
        operation: operation.to_owned(),
        path: path.to_owned(),
        message: error.to_string(),
    }
}

#[cfg(unix)]
fn committed_sync_error(path: &Path, error: io::Error) -> Error {
    Error::Persistence {
        operation: "durably sync committed".to_owned(),
        path: path.to_owned(),
        message: format!(
            "{error}; the new snapshot is live and remains committed, but crash durability is uncertain"
        ),
    }
}

#[cfg(not(unix))]
fn unsupported_platform(path: &Path) -> Error {
    Error::Persistence {
        operation: "open persistent".to_owned(),
        path: path.to_owned(),
        message: "crash-safe database snapshots are currently supported only on Unix platforms"
            .to_owned(),
    }
}

fn invalid(path: &Path, message: impl Into<String>) -> Error {
    Error::InvalidSnapshot {
        path: path.to_owned(),
        message: message.into(),
    }
}

fn snapshot_checksum(payload_len: u64, payload: &[u8]) -> u32 {
    let mut checksum = Crc32::new();
    checksum.update(&VERSION.to_le_bytes());
    checksum.update(&payload_len.to_le_bytes());
    checksum.update(payload);
    checksum.finish()
}

struct Crc32(u32);

impl Crc32 {
    fn new() -> Self {
        Self(u32::MAX)
    }

    fn update(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u32::from(*byte);
            for _ in 0..8 {
                let mask = 0_u32.wrapping_sub(self.0 & 1);
                self.0 = (self.0 >> 1) ^ (0xedb8_8320 & mask);
            }
        }
    }

    fn finish(self) -> u32 {
        !self.0
    }
}

#[cfg(test)]
mod tests {
    use super::Crc32;

    #[test]
    fn checksum_matches_the_standard_crc32_test_vector() {
        let mut checksum = Crc32::new();
        checksum.update(b"123456789");
        assert_eq!(checksum.finish(), 0xcbf4_3926);
    }
}
