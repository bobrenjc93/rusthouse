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
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub(crate) struct Persistence {
    path: PathBuf,
}

impl Persistence {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub(crate) fn load(&self) -> Result<CatalogGeneration> {
        let file = match File::open(&self.path) {
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
        let mut bytes = Vec::with_capacity(capacity);
        file.take(MAX_SNAPSHOT_BYTES.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| Error::io("read snapshot", error))?;
        if bytes.len() as u64 > MAX_SNAPSHOT_BYTES {
            return Err(Error::SnapshotTooLarge {
                size: bytes.len() as u64,
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

fn write_and_replace(
    temporary: &Path,
    destination: &Path,
    parent: &Path,
    bytes: &[u8],
) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temporary)
        .map_err(|error| Error::io("create temporary snapshot", error))?;
    file.write_all(bytes)
        .map_err(|error| Error::io("write temporary snapshot", error))?;
    file.sync_all()
        .map_err(|error| Error::io("sync temporary snapshot", error))?;
    drop(file);
    fs::rename(temporary, destination).map_err(|error| Error::io("replace snapshot", error))?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| Error::io("sync snapshot directory", error))?;
    Ok(())
}

fn encode_snapshot(generation: &CatalogGeneration) -> Result<Vec<u8>> {
    let payload_capacity = encoded_payload_len(generation)?;
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

    let mut decoder = Decoder::new(payload);
    let id = decoder.u64()?;
    let table_count = decoder.collection_len(1)?;
    if table_count > MAX_TABLES {
        return Err(Error::CorruptSnapshot(format!(
            "table count {table_count} exceeds maximum {MAX_TABLES}"
        )));
    }
    let mut tables = BTreeMap::new();
    for _ in 0..table_count {
        let name = decoder.string()?;
        let column_count = decoder.collection_len(3)?;
        if column_count == 0 || column_count > MAX_COLUMNS_PER_TABLE {
            return Err(Error::CorruptSnapshot(format!(
                "column count {column_count} is outside the supported range 1..={MAX_COLUMNS_PER_TABLE}"
            )));
        }
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
        if row_count > MAX_ROWS_PER_TABLE {
            return Err(Error::CorruptSnapshot(format!(
                "row count {row_count} exceeds maximum {MAX_ROWS_PER_TABLE}"
            )));
        }
        let mut columns = Vec::with_capacity(column_count);
        for column in &schema {
            columns.push(decoder.column(column.data_type, row_count)?);
        }
        let table = Arc::new(
            Table::from_parts(schema, columns)
                .map_err(|error| Error::CorruptSnapshot(error.to_string()))?,
        );
        if tables.insert(name.clone(), table).is_some() {
            return Err(Error::CorruptSnapshot(format!(
                "duplicate table name {name}"
            )));
        }
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
    allocation_remaining: usize,
}

impl<'a> Decoder<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            position: 0,
            allocation_remaining: MAX_DECODE_ALLOCATION,
        }
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
        if length > MAX_STRING_BYTES {
            return Err(Error::CorruptSnapshot(format!(
                "string length {length} exceeds maximum {MAX_STRING_BYTES}"
            )));
        }
        self.reserve_allocation(length)?;
        String::from_utf8(self.take(length)?.to_vec())
            .map_err(|_| Error::CorruptSnapshot("string is not valid UTF-8".to_owned()))
    }

    fn reserve_allocation(&mut self, bytes: usize) -> Result<()> {
        self.allocation_remaining =
            self.allocation_remaining
                .checked_sub(bytes)
                .ok_or_else(|| {
                    Error::CorruptSnapshot(format!(
                        "decoded data exceeds the {MAX_DECODE_ALLOCATION}-byte allocation limit"
                    ))
                })?;
        Ok(())
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
        let bytes = std::mem::size_of::<T>()
            .checked_mul(length)
            .ok_or_else(|| Error::CorruptSnapshot("column allocation overflowed".to_owned()))?;
        self.reserve_allocation(bytes)
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
