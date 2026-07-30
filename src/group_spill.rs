use std::fs::{self, File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::mem;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

use crate::error::{Error, Result};
use crate::storage::Table;

pub(crate) const PARTITION_FANOUT: usize = 16;
const MAX_PARTITION_DEPTH: usize = 64;
pub(crate) const MAX_LIVE_PARTITIONS: usize =
    1 + PARTITION_FANOUT + (PARTITION_FANOUT - 1) * MAX_PARTITION_DEPTH;
pub(crate) const ROW_INDEX_BYTES: u64 = mem::size_of::<u64>() as u64;
// Charge sparse files by allocation blocks, not only their eight-byte payloads.
const ALLOCATION_UNIT_BYTES: u64 = 4 * 1024;
const WORKSPACE_CREATION_ATTEMPTS: usize = 128;

#[derive(Debug)]
pub(crate) struct Partition {
    pub(crate) path: PathBuf,
    pub(crate) bytes: u64,
    allocated_bytes: u64,
    depth: usize,
}

#[derive(Debug)]
pub(crate) struct TempWorkspace {
    path: Option<PathBuf>,
    limit_bytes: u64,
    used_bytes: u64,
    next_file: u64,
}

impl TempWorkspace {
    pub(crate) fn new(root: &Path, limit_bytes: u64) -> Result<Self> {
        fs::create_dir_all(root).map_err(|error| {
            io_error(
                format!("could not create temporary directory '{}'", root.display()),
                error,
            )
        })?;
        for _ in 0..WORKSPACE_CREATION_ATTEMPTS {
            let path = root.join(format!("rusthouse-group-{}", random_token()?));
            match create_private_directory(&path) {
                Ok(()) => {
                    let mut workspace = Self {
                        path: Some(path),
                        limit_bytes,
                        used_bytes: 0,
                        next_file: 0,
                    };
                    if let Err(error) = workspace.reserve(ALLOCATION_UNIT_BYTES) {
                        let _ = workspace.cleanup();
                        return Err(error);
                    }
                    return Ok(workspace);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(io_error(
                        format!("could not create spill workspace '{}'", path.display()),
                        error,
                    ));
                }
            }
        }
        Err(Error::Io {
            context: format!(
                "could not create a unique spill workspace in '{}'",
                root.display()
            ),
            message: "secure random names repeatedly collided".to_owned(),
        })
    }

    fn path(&self) -> &Path {
        self.path.as_deref().expect("workspace is active")
    }

    fn allocate_path(&mut self, depth: usize, bucket: usize) -> PathBuf {
        let file = self.next_file;
        self.next_file += 1;
        self.path()
            .join(format!("partition-{depth}-{bucket}-{file}.bin"))
    }

    fn reserve(&mut self, bytes: u64) -> Result<()> {
        let Some(new_total) = self.used_bytes.checked_add(bytes) else {
            return Err(Error::TemporaryStorageLimit {
                limit_bytes: self.limit_bytes,
            });
        };
        if new_total > self.limit_bytes {
            return Err(Error::TemporaryStorageLimit {
                limit_bytes: self.limit_bytes,
            });
        }
        self.used_bytes = new_total;
        Ok(())
    }

    fn release(&mut self, bytes: u64) {
        self.used_bytes = self
            .used_bytes
            .checked_sub(bytes)
            .expect("released temporary storage was previously reserved");
    }

    pub(crate) fn remove_partition(&mut self, partition: &Partition) -> Result<()> {
        fs::remove_file(&partition.path).map_err(|error| {
            io_error(
                format!("could not remove spill file '{}'", partition.path.display()),
                error,
            )
        })?;
        self.release(partition.allocated_bytes);
        Ok(())
    }

    pub(crate) fn cleanup(&mut self) -> Result<()> {
        let Some(path) = self.path.as_ref() else {
            return Ok(());
        };
        match fs::remove_dir_all(path) {
            Ok(()) => {
                self.path = None;
                self.used_bytes = 0;
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.path = None;
                self.used_bytes = 0;
                Ok(())
            }
            Err(error) => Err(io_error(
                format!("could not clean spill workspace '{}'", path.display()),
                error,
            )),
        }
    }
}

fn random_token() -> Result<String> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|error| Error::Io {
        context: "could not obtain randomness for a private spill workspace".to_owned(),
        message: error.to_string(),
    })?;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut token = String::with_capacity(random.len() * 2);
    for byte in random {
        token.push(char::from(HEX[usize::from(byte >> 4)]));
        token.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(token)
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_directory(path: &Path) -> io::Result<()> {
    fs::create_dir(path)
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = fs::remove_dir_all(path);
        }
    }
}

fn io_error(context: String, error: io::Error) -> Error {
    Error::Io {
        context,
        message: error.to_string(),
    }
}

struct PartitionOutput {
    path: PathBuf,
    writer: BufWriter<File>,
    bytes: u64,
    allocated_bytes: u64,
}

struct PartitionWriters {
    outputs: Vec<Option<PartitionOutput>>,
    depth: usize,
}

impl PartitionWriters {
    fn new(depth: usize) -> Self {
        Self {
            outputs: std::iter::repeat_with(|| None)
                .take(PARTITION_FANOUT)
                .collect(),
            depth,
        }
    }

    fn write_row(
        &mut self,
        workspace: &mut TempWorkspace,
        bucket: usize,
        row: usize,
    ) -> Result<()> {
        if self.outputs[bucket].is_none() {
            workspace.reserve(ALLOCATION_UNIT_BYTES)?;
            let path = workspace.allocate_path(self.depth, bucket);
            let file = match create_private_file(&path) {
                Ok(file) => file,
                Err(error) => {
                    workspace.release(ALLOCATION_UNIT_BYTES);
                    return Err(io_error(
                        format!("could not create spill file '{}'", path.display()),
                        error,
                    ));
                }
            };
            self.outputs[bucket] = Some(PartitionOutput {
                path,
                writer: BufWriter::new(file),
                bytes: 0,
                allocated_bytes: ALLOCATION_UNIT_BYTES,
            });
        }
        let output = self.outputs[bucket].as_mut().expect("output was created");
        let new_bytes = output
            .bytes
            .checked_add(ROW_INDEX_BYTES)
            .expect("one partition cannot exceed the u64 storage counter");
        let required_bytes = new_bytes
            .div_ceil(ALLOCATION_UNIT_BYTES)
            .checked_mul(ALLOCATION_UNIT_BYTES)
            .expect("rounded partition allocation fits in u64");
        if required_bytes > output.allocated_bytes {
            let additional_bytes = required_bytes - output.allocated_bytes;
            workspace.reserve(additional_bytes)?;
            output.allocated_bytes = required_bytes;
        }
        let encoded = u64::try_from(row)
            .expect("RustHouse row indices fit in the on-disk u64 representation")
            .to_le_bytes();
        output.writer.write_all(&encoded).map_err(|error| {
            io_error(
                format!("could not write spill file '{}'", output.path.display()),
                error,
            )
        })?;
        output.bytes = new_bytes;
        Ok(())
    }

    fn finish(self) -> Result<Vec<Partition>> {
        let mut partitions = Vec::new();
        for output in self.outputs.into_iter().flatten() {
            let PartitionOutput {
                path,
                mut writer,
                bytes,
                allocated_bytes,
            } = output;
            writer.flush().map_err(|error| {
                io_error(
                    format!("could not flush spill file '{}'", path.display()),
                    error,
                )
            })?;
            partitions.push(Partition {
                path,
                bytes,
                allocated_bytes,
                depth: self.depth,
            });
        }
        Ok(partitions)
    }
}

pub(crate) struct PartitionRows {
    reader: BufReader<File>,
    path: PathBuf,
    finished: bool,
}

impl PartitionRows {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(|error| {
            io_error(
                format!("could not read spill file '{}'", path.display()),
                error,
            )
        })?;
        Ok(Self {
            reader: BufReader::new(file),
            path: path.to_owned(),
            finished: false,
        })
    }
}

impl Iterator for PartitionRows {
    type Item = Result<usize>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        let mut encoded = [0_u8; mem::size_of::<u64>()];
        match self.reader.read(&mut encoded[..1]) {
            Ok(0) => {
                self.finished = true;
                None
            }
            Ok(_) => {
                if let Err(error) = self.reader.read_exact(&mut encoded[1..]) {
                    self.finished = true;
                    return Some(Err(io_error(
                        format!("could not read spill file '{}'", self.path.display()),
                        error,
                    )));
                }
                Some(
                    usize::try_from(u64::from_le_bytes(encoded)).map_err(|_| Error::Io {
                        context: format!("could not read spill file '{}'", self.path.display()),
                        message: "row index does not fit this platform".to_owned(),
                    }),
                )
            }
            Err(error) => {
                self.finished = true;
                Some(Err(io_error(
                    format!("could not read spill file '{}'", self.path.display()),
                    error,
                )))
            }
        }
    }
}

#[derive(Debug)]
struct StableHasher(u64);

impl StableHasher {
    fn for_depth(depth: usize) -> Self {
        let seed = (depth as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        Self(0xcbf2_9ce4_8422_2325 ^ seed)
    }
}

impl Hasher for StableHasher {
    fn finish(&self) -> u64 {
        let mut value = self.0;
        value ^= value >> 30;
        value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value ^= value >> 27;
        value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

fn group_hash(table: &Table, group_columns: &[usize], row: usize, depth: usize) -> u64 {
    let mut hasher = StableHasher::for_depth(depth);
    group_columns.len().hash(&mut hasher);
    for column in group_columns {
        table.columns()[*column].value_ref(row).hash(&mut hasher);
    }
    hasher.finish()
}

pub(crate) fn write_initial_partitions(
    workspace: &mut TempWorkspace,
    table: &Table,
    rows: &[usize],
    group_columns: &[usize],
) -> Result<Vec<Partition>> {
    let mut writers = PartitionWriters::new(0);
    for row in rows {
        let bucket = group_hash(table, group_columns, *row, 0) as usize % PARTITION_FANOUT;
        writers.write_row(workspace, bucket, *row)?;
    }
    writers.finish()
}

pub(crate) fn ensure_repartition_capacity(current_partitions: usize) -> Result<()> {
    if current_partitions > MAX_LIVE_PARTITIONS - PARTITION_FANOUT - 1 {
        return Err(Error::TemporaryPartitionLimit {
            limit: MAX_LIVE_PARTITIONS,
        });
    }
    Ok(())
}

pub(crate) fn repartition(
    workspace: &mut TempWorkspace,
    table: &Table,
    partition: &Partition,
    group_columns: &[usize],
) -> Result<Vec<Partition>> {
    if partition.depth >= MAX_PARTITION_DEPTH {
        return Err(Error::InvalidQuery(format!(
            "group keys could not be partitioned within the configured memory limit after \
             {MAX_PARTITION_DEPTH} repartitions"
        )));
    }
    let depth = partition.depth + 1;
    let mut writers = PartitionWriters::new(depth);
    {
        let rows = PartitionRows::open(&partition.path)?;
        for row in rows {
            let row = row?;
            let bucket = group_hash(table, group_columns, row, depth) as usize % PARTITION_FANOUT;
            writers.write_row(workspace, bucket, row)?;
        }
    }
    let partitions = writers.finish()?;
    workspace.remove_partition(partition)?;
    Ok(partitions)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "rusthouse-{label}-test-{}",
            random_token().expect("operating-system randomness")
        ));
        fs::create_dir(&root).expect("create spill test root");
        root
    }

    #[test]
    fn allocation_accounting_bounds_files_and_uses_exclusive_private_creation() {
        let root = test_root("allocation");
        let mut workspace =
            TempWorkspace::new(&root, ALLOCATION_UNIT_BYTES * 2).expect("create workspace");
        assert_eq!(workspace.used_bytes, ALLOCATION_UNIT_BYTES);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            assert_eq!(
                fs::metadata(workspace.path())
                    .expect("workspace metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }

        let mut writers = PartitionWriters::new(0);
        writers
            .write_row(&mut workspace, 0, 7)
            .expect("one allocation unit remains");
        assert_eq!(workspace.used_bytes, ALLOCATION_UNIT_BYTES * 2);
        let file_path = writers.outputs[0]
            .as_ref()
            .expect("first partition exists")
            .path
            .clone();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            assert_eq!(
                fs::metadata(&file_path)
                    .expect("partition metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert_eq!(
            create_private_file(&file_path)
                .expect_err("partition creation is exclusive")
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            writers
                .write_row(&mut workspace, 1, 8)
                .expect_err("a second file exceeds the physical allocation budget"),
            Error::TemporaryStorageLimit {
                limit_bytes: ALLOCATION_UNIT_BYTES * 2
            }
        );

        drop(writers);
        workspace.cleanup().expect("clean workspace");
        fs::remove_dir(&root).expect("remove test root");
    }

    #[test]
    fn live_partition_limit_includes_parent_and_new_fanout() {
        ensure_repartition_capacity(MAX_LIVE_PARTITIONS - PARTITION_FANOUT - 1)
            .expect("parent and a complete fanout fit");
        assert_eq!(
            ensure_repartition_capacity(MAX_LIVE_PARTITIONS - PARTITION_FANOUT)
                .expect_err("one more queued file would exceed the invariant"),
            Error::TemporaryPartitionLimit {
                limit: MAX_LIVE_PARTITIONS
            }
        );
    }

    #[test]
    fn cleanup_failures_are_reported_and_drop_retries() {
        let root = test_root("cleanup");
        let mut workspace =
            TempWorkspace::new(&root, ALLOCATION_UNIT_BYTES).expect("create workspace");
        let workspace_path = workspace.path().to_owned();
        let moved_path = root.join("moved-workspace");
        fs::rename(&workspace_path, &moved_path).expect("move workspace");
        File::create(&workspace_path).expect("replace workspace path with a file");

        let error = workspace
            .cleanup()
            .expect_err("remove_dir_all cannot remove a regular file");
        assert!(
            matches!(error, Error::Io { context, .. } if context.contains("clean spill workspace"))
        );

        fs::remove_file(&workspace_path).expect("remove replacement file");
        workspace.path = Some(moved_path);
        drop(workspace);
        fs::remove_dir(&root).expect("drop removed the moved workspace");
    }
}
