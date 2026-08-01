use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::catalog::CatalogGeneration;
use crate::error::{Error, Result};
use crate::storage::{ColumnData, ColumnDef, DataType, Table};

const MAGIC: &[u8; 10] = b"RUSTHOUSE\0";
const FORMAT_VERSION: u32 = 1;
const HEADER_LEN: usize = MAGIC.len() + 4 + 8 + 8;
const MAX_SNAPSHOT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_DECODE_ALLOCATION: usize = 512 * 1024 * 1024;
const MAX_TABLES: usize = 100_000;
const MAX_COLUMNS_PER_TABLE: usize = 4_096;
const MAX_ROWS_PER_TABLE: usize = 10_000_000;
const MAX_STRING_BYTES: usize = 64 * 1024 * 1024;
const TABLE_MAP_ENTRY_ALLOCATION: usize = 512;
const HEAP_ALLOCATION_OVERHEAD: usize = 64;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub(crate) struct Persistence {
    path: PathBuf,
    _lock: File,
}

impl Persistence {
    pub(crate) fn acquire(path: PathBuf) -> Result<Self> {
        let path = normalize_path(&path)?;
        let mut lock_name = path.as_os_str().to_os_string();
        lock_name.push(".lock");
        let lock_path = PathBuf::from(lock_name);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|error| Error::io("open database lock", error))?;
        match lock.try_lock() {
            Ok(()) => Ok(Self { path, _lock: lock }),
            Err(std::fs::TryLockError::WouldBlock) => {
                Err(Error::DatabaseAlreadyOpen(path.display().to_string()))
            }
            Err(std::fs::TryLockError::Error(error)) => Err(Error::io("lock database", error)),
        }
    }

    pub(crate) fn load(&self) -> Result<CatalogGeneration> {
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(CatalogGeneration::empty());
            }
            Err(error) => return Err(Error::io("open snapshot", error)),
        };
        let size = file
            .metadata()
            .map_err(|error| Error::io("inspect snapshot", error))?
            .len();
        if size > MAX_SNAPSHOT_BYTES {
            return Err(Error::SnapshotTooLarge {
                size,
                maximum: MAX_SNAPSHOT_BYTES,
            });
        }
        let capacity = usize::try_from(size).map_err(|_| Error::SnapshotTooLarge {
            size,
            maximum: MAX_SNAPSHOT_BYTES,
        })?;
        let mut bytes = vec![0; capacity];
        file.read_exact(&mut bytes)
            .map_err(|error| Error::io("read snapshot", error))?;
        let mut extra = [0];
        if file
            .read(&mut extra)
            .map_err(|error| Error::io("read snapshot", error))?
            != 0
        {
            return Err(Error::SnapshotTooLarge {
                size: size.saturating_add(1),
                maximum: MAX_SNAPSHOT_BYTES,
            });
        }
        decode_snapshot(&bytes)
    }

    pub(crate) fn store(&self, generation: &CatalogGeneration) -> Result<()> {
        let bytes = encode_snapshot(generation)?;
        let parent = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)
            .map_err(|error| Error::io("create snapshot directory", error))?;

        let file_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("rusthouse.db");
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".{file_name}.tmp.{}.{}",
            std::process::id(),
            sequence
        ));

        let result = write_and_replace(&temporary, &self.path, parent, &bytes);
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

fn normalize_path(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return fs::canonicalize(path).map_err(|error| Error::io("resolve database path", error));
    }
    let file_name = path.file_name().ok_or_else(|| Error::Io {
        operation: "resolve database path",
        message: "database path must name a file".to_owned(),
    })?;
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| Error::io("create database directory", error))?;
    let parent =
        fs::canonicalize(parent).map_err(|error| Error::io("resolve database directory", error))?;
    Ok(parent.join(file_name))
}

fn write_and_replace(
    temporary: &Path,
    destination: &Path,
    parent: &Path,
    bytes: &[u8],
) -> Result<()> {
    let destination_permissions = match fs::metadata(destination) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(Error::io("inspect snapshot permissions", error)),
    };
    #[cfg(any(target_os = "freebsd", target_os = "linux", target_os = "macos"))]
    let destination_acl = if destination_permissions.is_some() {
        match exacl::getfacl(destination, None) {
            Ok(acl) => Some(acl),
            Err(error) if error.kind() == std::io::ErrorKind::Unsupported => None,
            Err(error) => return Err(Error::io("inspect snapshot ACL", error)),
        }
    } else {
        None
    };
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_DELETE: u32 = 0x0000_0004;
        options.share_mode(FILE_SHARE_DELETE);
    }
    let mut file = options
        .open(temporary)
        .map_err(|error| Error::io("create private temporary snapshot", error))?;
    file.write_all(bytes)
        .map_err(|error| Error::io("write temporary snapshot", error))?;
    file.sync_all()
        .map_err(|error| Error::io("sync temporary snapshot", error))?;
    if let Some(permissions) = destination_permissions {
        file.set_permissions(permissions)
            .map_err(|error| Error::io("preserve snapshot permissions", error))?;
        #[cfg(any(target_os = "freebsd", target_os = "linux", target_os = "macos"))]
        if let Some(acl) = &destination_acl {
            exacl::setfacl(&[temporary], acl, None)
                .map_err(|error| Error::io("preserve snapshot ACL", error))?;
        }
        file.sync_all()
            .map_err(|error| Error::io("sync snapshot permissions", error))?;
    }
    #[cfg(not(windows))]
    {
        drop(file);
        replace_snapshot(temporary, destination, parent)
    }
    #[cfg(windows)]
    {
        let result = replace_snapshot(temporary, destination, parent);
        drop(file);
        result
    }
}

#[cfg(unix)]
fn replace_snapshot(temporary: &Path, destination: &Path, parent: &Path) -> Result<()> {
    fs::rename(temporary, destination).map_err(|error| Error::io("replace snapshot", error))?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| Error::io("sync snapshot directory", error))?;
    Ok(())
}

#[cfg(windows)]
fn replace_snapshot(temporary: &Path, destination: &Path, _parent: &Path) -> Result<()> {
    windows::replace_snapshot(temporary, destination)
}

#[cfg(not(any(unix, windows)))]
fn replace_snapshot(temporary: &Path, destination: &Path, _parent: &Path) -> Result<()> {
    fs::rename(temporary, destination).map_err(|error| Error::io("replace snapshot", error))
}

#[cfg(windows)]
mod windows {
    use std::ffi::c_void;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::ptr;

    use crate::error::{Error, Result};

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;

    #[link(name = "Kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
        fn ReplaceFileW(
            replaced: *const u16,
            replacement: *const u16,
            backup: *const u16,
            flags: u32,
            exclude: *mut c_void,
            reserved: *mut c_void,
        ) -> i32;
    }

    pub(super) fn replace_snapshot(temporary: &Path, destination: &Path) -> Result<()> {
        let destination_exists = destination.exists();
        let temporary = wide_path(temporary)?;
        let destination = wide_path(destination)?;
        if destination_exists {
            // ReplaceFileW retains the replaced file's ACL and other security metadata.
            let replaced = unsafe {
                ReplaceFileW(
                    destination.as_ptr(),
                    temporary.as_ptr(),
                    ptr::null(),
                    0,
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            };
            if replaced != 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::NotFound {
                return Err(Error::io("replace snapshot", error));
            }
        }

        let moved = unsafe {
            MoveFileExW(
                temporary.as_ptr(),
                destination.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if moved == 0 {
            return Err(Error::io("replace snapshot", io::Error::last_os_error()));
        }
        Ok(())
    }

    fn wide_path(path: &Path) -> Result<Vec<u16>> {
        let mut path: Vec<u16> = path.as_os_str().encode_wide().collect();
        if path.contains(&0) {
            return Err(Error::Io {
                operation: "encode Windows snapshot path",
                message: "path contains a NUL character".to_owned(),
            });
        }
        path.push(0);
        Ok(path)
    }
}

fn encode_snapshot(generation: &CatalogGeneration) -> Result<Vec<u8>> {
    let payload_capacity = encoded_payload_len(generation)?;
    let total_len = payload_capacity
        .checked_add(HEADER_LEN)
        .ok_or(Error::SnapshotTooLarge {
            size: u64::MAX,
            maximum: MAX_SNAPSHOT_BYTES,
        })?;
    validate_generation(generation, total_len)?;
    let mut payload = Vec::with_capacity(payload_capacity);
    put_u64(&mut payload, generation.id);
    put_len(&mut payload, generation.tables.len())?;
    for (name, table) in &generation.tables {
        put_string(&mut payload, name)?;
        put_len(&mut payload, table.schema().len())?;
        for column in table.schema() {
            put_string(&mut payload, &column.name)?;
            payload.push(match column.data_type {
                DataType::Int64 => 1,
                DataType::Float64 => 2,
                DataType::Bool => 3,
                DataType::String => 4,
            });
            payload.push(u8::from(column.nullable));
        }
        put_len(&mut payload, table.row_count())?;
        for column in table.columns() {
            encode_column(&mut payload, column)?;
        }
    }
    let payload_len = u64::try_from(payload.len()).map_err(|_| Error::SnapshotTooLarge {
        size: u64::MAX,
        maximum: MAX_SNAPSHOT_BYTES,
    })?;
    let total_len = payload_len
        .checked_add(HEADER_LEN as u64)
        .ok_or(Error::SnapshotTooLarge {
            size: u64::MAX,
            maximum: MAX_SNAPSHOT_BYTES,
        })?;
    if total_len > MAX_SNAPSHOT_BYTES {
        return Err(Error::SnapshotTooLarge {
            size: total_len,
            maximum: MAX_SNAPSHOT_BYTES,
        });
    }

    let mut output = Vec::with_capacity(HEADER_LEN + payload.len());
    output.extend_from_slice(MAGIC);
    put_u32(&mut output, FORMAT_VERSION);
    put_u64(&mut output, payload_len);
    put_u64(&mut output, checksum(&payload));
    output.extend_from_slice(&payload);
    Ok(output)
}

fn validate_generation(generation: &CatalogGeneration, encoded_bytes: usize) -> Result<()> {
    writer_limit("table count", generation.tables.len(), MAX_TABLES)?;
    let mut allocation = heap_allocation(encoded_bytes);
    writer_limit("decode allocation bytes", allocation, MAX_DECODE_ALLOCATION)?;
    charge_allocation(&mut allocation, catalog_metadata_allocation())
        .map_err(snapshot_limit_error)?;

    for (name, table) in &generation.tables {
        writer_limit("string bytes", name.len(), MAX_STRING_BYTES)?;
        writer_limit(
            "columns per table",
            table.schema().len(),
            MAX_COLUMNS_PER_TABLE,
        )?;
        writer_limit("rows per table", table.row_count(), MAX_ROWS_PER_TABLE)?;
        charge_allocation(&mut allocation, heap_allocation(name.len()))
            .map_err(snapshot_limit_error)?;
        charge_allocation(
            &mut allocation,
            table_metadata_allocation(table.schema().len()),
        )
        .map_err(snapshot_limit_error)?;

        for column in table.schema() {
            writer_limit("string bytes", column.name.len(), MAX_STRING_BYTES)?;
            charge_allocation(&mut allocation, heap_allocation(column.name.len()))
                .map_err(snapshot_limit_error)?;
        }
        for column in table.columns() {
            charge_allocation(&mut allocation, column_allocation(column))
                .map_err(snapshot_limit_error)?;
            if let ColumnData::String(values) = column {
                for value in values.iter().flatten() {
                    writer_limit("string bytes", value.len(), MAX_STRING_BYTES)?;
                    charge_allocation(&mut allocation, heap_allocation(value.len()))
                        .map_err(snapshot_limit_error)?;
                }
            }
        }
    }
    Ok(())
}

fn encoded_payload_len(generation: &CatalogGeneration) -> Result<usize> {
    let mut total = 16usize;
    for (name, table) in &generation.tables {
        total = encoded_add(total, 8usize.saturating_add(name.len()))?;
        total = encoded_add(total, 8)?;
        for column in table.schema() {
            total = encoded_add(total, 10usize.saturating_add(column.name.len()))?;
        }
        total = encoded_add(total, 8)?;
        for column in table.columns() {
            let column_bytes = match column {
                ColumnData::Int64(values) => values.iter().fold(0usize, |size, value| {
                    size.saturating_add(if value.is_some() { 9 } else { 1 })
                }),
                ColumnData::Float64(values) => values.iter().fold(0usize, |size, value| {
                    size.saturating_add(if value.is_some() { 9 } else { 1 })
                }),
                ColumnData::Bool(values) => values.iter().fold(0usize, |size, value| {
                    size.saturating_add(if value.is_some() { 2 } else { 1 })
                }),
                ColumnData::String(values) => values.iter().fold(0usize, |size, value| {
                    size.saturating_add(
                        value
                            .as_ref()
                            .map_or(1, |value| 9usize.saturating_add(value.len())),
                    )
                }),
            };
            total = encoded_add(total, column_bytes)?;
        }
    }
    Ok(total)
}

fn encoded_add(total: usize, amount: usize) -> Result<usize> {
    let total = total.checked_add(amount).ok_or(Error::SnapshotTooLarge {
        size: u64::MAX,
        maximum: MAX_SNAPSHOT_BYTES,
    })?;
    if total.saturating_add(HEADER_LEN) as u64 > MAX_SNAPSHOT_BYTES {
        return Err(Error::SnapshotTooLarge {
            size: total.saturating_add(HEADER_LEN) as u64,
            maximum: MAX_SNAPSHOT_BYTES,
        });
    }
    Ok(total)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LimitViolation {
    resource: &'static str,
    limit: usize,
    attempted: usize,
}

fn check_limit(
    resource: &'static str,
    attempted: usize,
    limit: usize,
) -> std::result::Result<(), LimitViolation> {
    if attempted > limit {
        Err(LimitViolation {
            resource,
            limit,
            attempted,
        })
    } else {
        Ok(())
    }
}

fn writer_limit(resource: &'static str, attempted: usize, limit: usize) -> Result<()> {
    check_limit(resource, attempted, limit).map_err(snapshot_limit_error)
}

fn snapshot_limit_error(violation: LimitViolation) -> Error {
    Error::SnapshotLimitExceeded {
        resource: violation.resource,
        limit: violation.limit,
        attempted: violation.attempted,
    }
}

fn corrupt_limit_error(violation: LimitViolation) -> Error {
    Error::CorruptSnapshot(format!(
        "{} {} exceeds maximum {}",
        violation.resource, violation.attempted, violation.limit
    ))
}

fn charge_allocation(used: &mut usize, bytes: usize) -> std::result::Result<(), LimitViolation> {
    let attempted = used.saturating_add(bytes);
    check_limit("decode allocation bytes", attempted, MAX_DECODE_ALLOCATION)?;
    *used = attempted;
    Ok(())
}

fn table_metadata_allocation(column_count: usize) -> usize {
    TABLE_MAP_ENTRY_ALLOCATION
        .saturating_add(heap_allocation(
            std::mem::size_of::<Table>().saturating_add(2 * std::mem::size_of::<usize>()),
        ))
        .saturating_add(allocation_for::<ColumnDef>(column_count))
        .saturating_add(allocation_for::<ColumnData>(column_count))
}

fn catalog_metadata_allocation() -> usize {
    heap_allocation(
        std::mem::size_of::<CatalogGeneration>().saturating_add(2 * std::mem::size_of::<usize>()),
    )
}

fn column_allocation(column: &ColumnData) -> usize {
    match column {
        ColumnData::Int64(values) => allocation_for::<Option<i64>>(values.len()),
        ColumnData::Float64(values) => allocation_for::<Option<f64>>(values.len()),
        ColumnData::Bool(values) => allocation_for::<Option<bool>>(values.len()),
        ColumnData::String(values) => allocation_for::<Option<String>>(values.len()),
    }
}

fn allocation_for<T>(length: usize) -> usize {
    heap_allocation(std::mem::size_of::<T>().saturating_mul(length))
}

fn heap_allocation(bytes: usize) -> usize {
    if bytes == 0 {
        0
    } else {
        bytes.saturating_add(HEAP_ALLOCATION_OVERHEAD)
    }
}

fn encode_column(output: &mut Vec<u8>, column: &ColumnData) -> Result<()> {
    match column {
        ColumnData::Int64(values) => {
            for value in values {
                put_option(output, value, |output, value| {
                    output.extend_from_slice(&value.to_le_bytes());
                    Ok(())
                })?;
            }
        }
        ColumnData::Float64(values) => {
            for value in values {
                put_option(output, value, |output, value| {
                    output.extend_from_slice(&value.to_bits().to_le_bytes());
                    Ok(())
                })?;
            }
        }
        ColumnData::Bool(values) => {
            for value in values {
                put_option(output, value, |output, value| {
                    output.push(u8::from(*value));
                    Ok(())
                })?;
            }
        }
        ColumnData::String(values) => {
            for value in values {
                put_option(output, value, |output, value| put_string(output, value))?;
            }
        }
    }
    Ok(())
}

fn put_option<T>(
    output: &mut Vec<u8>,
    value: &Option<T>,
    encode: impl FnOnce(&mut Vec<u8>, &T) -> Result<()>,
) -> Result<()> {
    match value {
        Some(value) => {
            output.push(1);
            encode(output, value)
        }
        None => {
            output.push(0);
            Ok(())
        }
    }
}

fn decode_snapshot(bytes: &[u8]) -> Result<CatalogGeneration> {
    if bytes.len() < HEADER_LEN || &bytes[..MAGIC.len()] != MAGIC {
        return Err(Error::CorruptSnapshot("invalid file header".to_owned()));
    }
    let mut header = Decoder::new(&bytes[MAGIC.len()..]);
    let version = header.u32()?;
    if version != FORMAT_VERSION {
        return Err(Error::CorruptSnapshot(format!(
            "unsupported format version {version}"
        )));
    }
    let payload_len = header.usize()?;
    let expected_checksum = header.u64()?;
    if payload_len != bytes.len() - HEADER_LEN {
        return Err(Error::CorruptSnapshot(
            "declared payload length does not match file length".to_owned(),
        ));
    }
    let payload = &bytes[HEADER_LEN..];
    if checksum(payload) != expected_checksum {
        return Err(Error::CorruptSnapshot("checksum mismatch".to_owned()));
    }

    let mut decoder = Decoder::with_allocation(payload, heap_allocation(bytes.len()))?;
    decoder.reserve_allocation(catalog_metadata_allocation())?;
    let id = decoder.u64()?;
    let table_count = decoder.collection_len(1)?;
    decoder.limit("table count", table_count, MAX_TABLES)?;
    let mut tables = BTreeMap::new();
    for _ in 0..table_count {
        let name = decoder.string()?;
        let column_count = decoder.collection_len(3)?;
        if column_count == 0 {
            return Err(Error::CorruptSnapshot(
                "column count must be at least one".to_owned(),
            ));
        }
        decoder.limit("columns per table", column_count, MAX_COLUMNS_PER_TABLE)?;
        decoder.reserve_allocation(table_metadata_allocation(column_count))?;
        let mut schema = Vec::with_capacity(column_count);
        for _ in 0..column_count {
            let name = decoder.string()?;
            let data_type = match decoder.byte()? {
                1 => DataType::Int64,
                2 => DataType::Float64,
                3 => DataType::Bool,
                4 => DataType::String,
                tag => {
                    return Err(Error::CorruptSnapshot(format!(
                        "unknown column type tag {tag}"
                    )));
                }
            };
            let nullable = match decoder.byte()? {
                0 => false,
                1 => true,
                value => {
                    return Err(Error::CorruptSnapshot(format!(
                        "invalid nullable flag {value}"
                    )));
                }
            };
            schema.push(ColumnDef {
                name,
                data_type,
                nullable,
            });
        }
        let row_count = decoder.collection_len(column_count)?;
        decoder.limit("rows per table", row_count, MAX_ROWS_PER_TABLE)?;
        let mut columns = Vec::with_capacity(column_count);
        for column in &schema {
            columns.push(decoder.column(column.data_type, row_count)?);
        }
        let table = Arc::new(Table::from_parts(schema, columns)?);
        if tables.contains_key(&name) {
            return Err(Error::CorruptSnapshot("duplicate table name".to_owned()));
        }
        tables.insert(name, table);
    }
    if !decoder.is_empty() {
        return Err(Error::CorruptSnapshot(
            "trailing bytes after catalog".to_owned(),
        ));
    }
    Ok(CatalogGeneration { id, tables })
}

struct Decoder<'a> {
    input: &'a [u8],
    position: usize,
    allocation_used: usize,
}

impl<'a> Decoder<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            position: 0,
            allocation_used: 0,
        }
    }

    fn with_allocation(input: &'a [u8], allocation_used: usize) -> Result<Self> {
        check_limit(
            "decode allocation bytes",
            allocation_used,
            MAX_DECODE_ALLOCATION,
        )
        .map_err(corrupt_limit_error)?;
        Ok(Self {
            input,
            position: 0,
            allocation_used,
        })
    }

    fn is_empty(&self) -> bool {
        self.position == self.input.len()
    }

    fn remaining(&self) -> usize {
        self.input.len().saturating_sub(self.position)
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(length)
            .ok_or_else(|| Error::CorruptSnapshot("length arithmetic overflowed".to_owned()))?;
        let bytes = self
            .input
            .get(self.position..end)
            .ok_or_else(|| Error::CorruptSnapshot("snapshot ended unexpectedly".to_owned()))?;
        self.position = end;
        Ok(bytes)
    }

    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        let bytes: [u8; 4] = self
            .take(4)?
            .try_into()
            .expect("a four-byte slice always converts to an array");
        Ok(u32::from_le_bytes(bytes))
    }

    fn u64(&mut self) -> Result<u64> {
        let bytes: [u8; 8] = self
            .take(8)?
            .try_into()
            .expect("an eight-byte slice always converts to an array");
        Ok(u64::from_le_bytes(bytes))
    }

    fn usize(&mut self) -> Result<usize> {
        let value = self.u64()?;
        usize::try_from(value)
            .map_err(|_| Error::CorruptSnapshot("length does not fit in memory".to_owned()))
    }

    fn collection_len(&mut self, minimum_bytes_per_item: usize) -> Result<usize> {
        let length = self.usize()?;
        if length > self.remaining() / minimum_bytes_per_item.max(1) {
            return Err(Error::CorruptSnapshot(
                "collection length exceeds remaining snapshot data".to_owned(),
            ));
        }
        Ok(length)
    }

    fn string(&mut self) -> Result<String> {
        let length = self.collection_len(1)?;
        self.limit("string bytes", length, MAX_STRING_BYTES)?;
        self.reserve_allocation(heap_allocation(length))?;
        String::from_utf8(self.take(length)?.to_vec())
            .map_err(|_| Error::CorruptSnapshot("string is not valid UTF-8".to_owned()))
    }

    fn reserve_allocation(&mut self, bytes: usize) -> Result<()> {
        charge_allocation(&mut self.allocation_used, bytes).map_err(corrupt_limit_error)
    }

    fn limit(&self, resource: &'static str, attempted: usize, limit: usize) -> Result<()> {
        check_limit(resource, attempted, limit).map_err(corrupt_limit_error)
    }

    fn present(&mut self) -> Result<bool> {
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            value => Err(Error::CorruptSnapshot(format!(
                "invalid value presence flag {value}"
            ))),
        }
    }

    fn column(&mut self, data_type: DataType, row_count: usize) -> Result<ColumnData> {
        match data_type {
            DataType::Int64 => {
                self.reserve_column::<Option<i64>>(row_count)?;
                let mut values = Vec::with_capacity(row_count);
                for _ in 0..row_count {
                    values.push(if self.present()? {
                        Some(i64::from_le_bytes(
                            self.take(8)?.try_into().expect("validated slice length"),
                        ))
                    } else {
                        None
                    });
                }
                Ok(ColumnData::Int64(values))
            }
            DataType::Float64 => {
                self.reserve_column::<Option<f64>>(row_count)?;
                let mut values = Vec::with_capacity(row_count);
                for _ in 0..row_count {
                    values.push(if self.present()? {
                        Some(f64::from_bits(u64::from_le_bytes(
                            self.take(8)?.try_into().expect("validated slice length"),
                        )))
                    } else {
                        None
                    });
                }
                Ok(ColumnData::Float64(values))
            }
            DataType::Bool => {
                self.reserve_column::<Option<bool>>(row_count)?;
                let mut values = Vec::with_capacity(row_count);
                for _ in 0..row_count {
                    values.push(if self.present()? {
                        match self.byte()? {
                            0 => Some(false),
                            1 => Some(true),
                            value => {
                                return Err(Error::CorruptSnapshot(format!(
                                    "invalid boolean value {value}"
                                )));
                            }
                        }
                    } else {
                        None
                    });
                }
                Ok(ColumnData::Bool(values))
            }
            DataType::String => {
                self.reserve_column::<Option<String>>(row_count)?;
                let mut values = Vec::with_capacity(row_count);
                for _ in 0..row_count {
                    values.push(if self.present()? {
                        Some(self.string()?)
                    } else {
                        None
                    });
                }
                Ok(ColumnData::String(values))
            }
        }
    }

    fn reserve_column<T>(&mut self, length: usize) -> Result<()> {
        self.reserve_allocation(allocation_for::<T>(length))
    }
}

fn put_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_len(output: &mut Vec<u8>, value: usize) -> Result<()> {
    let value = u64::try_from(value)
        .map_err(|_| Error::Unsupported("value length exceeds u64".to_owned()))?;
    put_u64(output, value);
    Ok(())
}

fn put_string(output: &mut Vec<u8>, value: &str) -> Result<()> {
    put_len(output, value.len())?;
    output.extend_from_slice(value.as_bytes());
    Ok(())
}

fn checksum(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_shape_limits_are_inclusive() {
        let decoder = Decoder::with_allocation(&[], 0).unwrap();
        for (resource, limit) in [
            ("table count", MAX_TABLES),
            ("columns per table", MAX_COLUMNS_PER_TABLE),
            ("rows per table", MAX_ROWS_PER_TABLE),
            ("string bytes", MAX_STRING_BYTES),
        ] {
            assert_eq!(check_limit(resource, limit, limit), Ok(()));
            assert!(writer_limit(resource, limit, limit).is_ok());
            assert!(decoder.limit(resource, limit, limit).is_ok());
            assert_eq!(
                check_limit(resource, limit + 1, limit),
                Err(LimitViolation {
                    resource,
                    limit,
                    attempted: limit + 1,
                })
            );
            assert!(matches!(
                writer_limit(resource, limit + 1, limit),
                Err(Error::SnapshotLimitExceeded {
                    resource: actual_resource,
                    limit: actual_limit,
                    attempted,
                }) if actual_resource == resource
                    && actual_limit == limit
                    && attempted == limit + 1
            ));
            assert!(matches!(
                decoder.limit(resource, limit + 1, limit),
                Err(Error::CorruptSnapshot(_))
            ));
        }
    }

    #[test]
    fn wide_zero_row_tables_exhaust_metadata_budget_before_allocation() {
        let mut decoder = Decoder::with_allocation(&[], 0).unwrap();
        let metadata = table_metadata_allocation(MAX_COLUMNS_PER_TABLE);
        let accepted_tables = MAX_DECODE_ALLOCATION / metadata;
        for _ in 0..accepted_tables {
            decoder.reserve_allocation(metadata).unwrap();
        }
        assert!(matches!(
            decoder.reserve_allocation(metadata),
            Err(Error::CorruptSnapshot(message))
                if message.contains("decode allocation bytes")
        ));
    }

    #[test]
    fn checksummed_snapshot_cannot_put_null_in_non_nullable_column() {
        let mut payload = Vec::new();
        put_u64(&mut payload, 1);
        put_u64(&mut payload, 1);
        put_string(&mut payload, "invalid").unwrap();
        put_u64(&mut payload, 1);
        put_string(&mut payload, "id").unwrap();
        payload.push(1);
        payload.push(0);
        put_u64(&mut payload, 1);
        payload.push(0);

        let mut snapshot = Vec::new();
        snapshot.extend_from_slice(MAGIC);
        put_u32(&mut snapshot, FORMAT_VERSION);
        put_u64(&mut snapshot, payload.len() as u64);
        put_u64(&mut snapshot, checksum(&payload));
        snapshot.extend_from_slice(&payload);

        assert!(matches!(
            decode_snapshot(&snapshot),
            Err(Error::CorruptSnapshot(message))
                if message.contains("non-nullable column contains NULL")
        ));
    }
}
