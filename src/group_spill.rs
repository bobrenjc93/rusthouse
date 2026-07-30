use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Error, Result};
use crate::storage::Table;

const PARTITION_FANOUT: usize = 16;
const MAX_PARTITION_DEPTH: usize = 64;
pub(crate) const ROW_INDEX_BYTES: u64 = mem::size_of::<u64>() as u64;
static NEXT_WORKSPACE_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub(crate) struct Partition {
    pub(crate) path: PathBuf,
    pub(crate) bytes: u64,
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
        for _ in 0..1_000 {
            let id = NEXT_WORKSPACE_ID.fetch_add(1, Ordering::Relaxed);
            let path = root.join(format!("rusthouse-group-{}-{id}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => {
                    return Ok(Self {
                        path: Some(path),
                        limit_bytes,
                        used_bytes: 0,
                        next_file: 0,
                    });
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
            message: "too many conflicting directory names".to_owned(),
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

    pub(crate) fn remove_partition(&mut self, partition: &Partition) -> Result<()> {
        fs::remove_file(&partition.path).map_err(|error| {
            io_error(
                format!("could not remove spill file '{}'", partition.path.display()),
                error,
            )
        })?;
        self.used_bytes = self.used_bytes.saturating_sub(partition.bytes);
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
            let path = workspace.allocate_path(self.depth, bucket);
            let file = File::create(&path).map_err(|error| {
                io_error(
                    format!("could not create spill file '{}'", path.display()),
                    error,
                )
            })?;
            self.outputs[bucket] = Some(PartitionOutput {
                path,
                writer: BufWriter::new(file),
                bytes: 0,
            });
        }
        workspace.reserve(ROW_INDEX_BYTES)?;
        let output = self.outputs[bucket].as_mut().expect("output was created");
        let encoded = u64::try_from(row)
            .expect("RustHouse row indices fit in the on-disk u64 representation")
            .to_le_bytes();
        output.writer.write_all(&encoded).map_err(|error| {
            io_error(
                format!("could not write spill file '{}'", output.path.display()),
                error,
            )
        })?;
        output.bytes += ROW_INDEX_BYTES;
        Ok(())
    }

    fn finish(self) -> Result<Vec<Partition>> {
        let mut partitions = Vec::new();
        for output in self.outputs.into_iter().flatten() {
            let PartitionOutput {
                path,
                mut writer,
                bytes,
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

    #[test]
    fn cleanup_failures_are_reported_and_drop_retries() {
        let id = NEXT_WORKSPACE_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "rusthouse-cleanup-test-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).expect("create cleanup test root");
        let mut workspace = TempWorkspace::new(&root, 1024).expect("create workspace");
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
