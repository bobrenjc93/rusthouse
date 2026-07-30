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
// Reserve complete blocks for directory roots/indexes, file inodes/extent
// metadata, and a worst-case directory leaf plus index split per live name.
const WORKSPACE_METADATA_UNITS: u64 = 4;
const FILE_METADATA_UNITS: u64 = 2;
const DIRECTORY_ENTRY_UNITS: u64 = 2;
#[cfg(not(any(unix, windows)))]
const FALLBACK_ALLOCATION_UNIT_BYTES: u64 = 1024 * 1024;
const WORKSPACE_CREATION_ATTEMPTS: usize = 128;

#[derive(Debug)]
pub(crate) struct Partition {
    pub(crate) path: PathBuf,
    pub(crate) bytes: u64,
    allocated_bytes: u64,
    file_slot: u64,
    depth: usize,
}

#[derive(Debug)]
pub(crate) struct TempWorkspace {
    path: Option<PathBuf>,
    limit_bytes: u64,
    used_bytes: u64,
    allocation_unit_bytes: u64,
    live_files: usize,
    directory_slots: usize,
    next_file: u64,
    free_file_slots: Vec<u64>,
}

impl TempWorkspace {
    pub(crate) fn new(root: &Path, limit_bytes: u64) -> Result<Self> {
        fs::create_dir_all(root).map_err(|error| {
            io_error(
                format!("could not create temporary directory '{}'", root.display()),
                error,
            )
        })?;
        let allocation_unit_bytes = filesystem_allocation_unit(root)?;
        Self::new_with_allocation_unit(root, limit_bytes, allocation_unit_bytes)
    }

    fn new_with_allocation_unit(
        root: &Path,
        limit_bytes: u64,
        allocation_unit_bytes: u64,
    ) -> Result<Self> {
        let allocation_unit_bytes = allocation_unit_bytes.max(1);
        let workspace_bytes = allocation_unit_bytes
            .checked_mul(WORKSPACE_METADATA_UNITS)
            .filter(|bytes| *bytes <= limit_bytes)
            .ok_or(Error::TemporaryStorageLimit { limit_bytes })?;
        for _ in 0..WORKSPACE_CREATION_ATTEMPTS {
            let path = root.join(format!("rusthouse-group-{}", random_token()?));
            match create_private_directory(&path) {
                Ok(()) => {
                    let workspace = Self {
                        path: Some(path),
                        limit_bytes,
                        used_bytes: workspace_bytes,
                        allocation_unit_bytes,
                        live_files: 0,
                        directory_slots: 0,
                        next_file: 0,
                        free_file_slots: Vec::new(),
                    };
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

    fn allocate_path(&mut self) -> (u64, PathBuf) {
        let file_slot = self.free_file_slots.pop().unwrap_or_else(|| {
            let file_slot = self.next_file;
            self.next_file += 1;
            file_slot
        });
        (
            file_slot,
            self.path().join(format!("partition-{file_slot}.bin")),
        )
    }

    fn recycle_file_slot(&mut self, file_slot: u64) {
        self.free_file_slots.push(file_slot);
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

    fn allocation_bytes(&self, units: u64) -> Result<u64> {
        self.allocation_unit_bytes
            .checked_mul(units)
            .ok_or(Error::TemporaryStorageLimit {
                limit_bytes: self.limit_bytes,
            })
    }

    fn reserve_file(&mut self) -> Result<FileReservation> {
        let file_bytes = self.allocation_bytes(1 + FILE_METADATA_UNITS)?;
        let added_directory_slot = self.live_files == self.directory_slots;
        let directory_bytes = if added_directory_slot {
            self.allocation_bytes(DIRECTORY_ENTRY_UNITS)?
        } else {
            0
        };
        let reservation_bytes =
            file_bytes
                .checked_add(directory_bytes)
                .ok_or(Error::TemporaryStorageLimit {
                    limit_bytes: self.limit_bytes,
                })?;
        self.reserve(reservation_bytes)?;
        self.live_files += 1;
        if added_directory_slot {
            self.directory_slots += 1;
        }
        Ok(FileReservation {
            file_bytes,
            directory_bytes,
        })
    }

    fn rollback_file(&mut self, reservation: FileReservation) {
        self.live_files -= 1;
        if reservation.directory_bytes > 0 {
            self.directory_slots -= 1;
        }
        self.release(reservation.file_bytes + reservation.directory_bytes);
    }

    fn release_file(&mut self, bytes: u64) {
        self.live_files -= 1;
        self.release(bytes);
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
        self.recycle_file_slot(partition.file_slot);
        self.release_file(partition.allocated_bytes);
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

#[derive(Debug, Clone, Copy)]
struct FileReservation {
    file_bytes: u64,
    directory_bytes: u64,
}

#[cfg(unix)]
fn filesystem_allocation_unit(path: &Path) -> Result<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| Error::Io {
        context: format!(
            "could not inspect temporary filesystem for '{}'",
            path.display()
        ),
        message: "path contains an interior NUL byte".to_owned(),
    })?;
    let mut statistics = mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: path is NUL-terminated and statistics points to writable storage.
    let status = unsafe { libc::statvfs(path.as_ptr(), statistics.as_mut_ptr()) };
    if status != 0 {
        return Err(io_error(
            "could not inspect temporary filesystem allocation size".to_owned(),
            io::Error::last_os_error(),
        ));
    }
    // SAFETY: a successful statvfs call initialized the output structure.
    let statistics = unsafe { statistics.assume_init() };
    let unit = if statistics.f_frsize > 0 {
        statvfs_value_to_u64(statistics.f_frsize)
    } else {
        statvfs_value_to_u64(statistics.f_bsize)
    };
    Ok(unit.max(512))
}

#[cfg(all(unix, target_pointer_width = "64"))]
fn statvfs_value_to_u64(value: libc::c_ulong) -> u64 {
    value
}

#[cfg(all(unix, target_pointer_width = "32"))]
fn statvfs_value_to_u64(value: libc::c_ulong) -> u64 {
    u64::from(value)
}

#[cfg(windows)]
fn filesystem_allocation_unit(path: &Path) -> Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceW, GetVolumePathNameW};

    let absolute = fs::canonicalize(path).map_err(|error| {
        io_error(
            format!(
                "could not inspect temporary filesystem for '{}'",
                path.display()
            ),
            error,
        )
    })?;
    let path = absolute
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut volume_path = vec![0_u16; 32_768];
    // SAFETY: both UTF-16 buffers are NUL-terminated/writable for the supplied lengths.
    let status = unsafe {
        GetVolumePathNameW(
            path.as_ptr(),
            volume_path.as_mut_ptr(),
            volume_path.len() as u32,
        )
    };
    if status == 0 {
        return Err(io_error(
            "could not resolve the temporary filesystem volume".to_owned(),
            io::Error::last_os_error(),
        ));
    }

    let mut sectors_per_cluster = 0_u32;
    let mut bytes_per_sector = 0_u32;
    let mut free_clusters = 0_u32;
    let mut total_clusters = 0_u32;
    // SAFETY: volume_path contains the NUL-terminated path returned above, and all outputs
    // point to initialized writable integers.
    let status = unsafe {
        GetDiskFreeSpaceW(
            volume_path.as_ptr(),
            &mut sectors_per_cluster,
            &mut bytes_per_sector,
            &mut free_clusters,
            &mut total_clusters,
        )
    };
    if status == 0 {
        return Err(io_error(
            "could not inspect temporary filesystem allocation size".to_owned(),
            io::Error::last_os_error(),
        ));
    }
    u64::from(sectors_per_cluster)
        .checked_mul(u64::from(bytes_per_sector))
        .filter(|unit| *unit > 0)
        .ok_or_else(|| Error::Io {
            context: "could not inspect temporary filesystem allocation size".to_owned(),
            message: "the volume reported an invalid allocation unit".to_owned(),
        })
}

#[cfg(not(any(unix, windows)))]
fn filesystem_allocation_unit(_path: &Path) -> Result<u64> {
    // Deliberately over-reserve where the standard library has no allocation query.
    Ok(FALLBACK_ALLOCATION_UNIT_BYTES)
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
    file_slot: u64,
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
            let reservation = workspace.reserve_file()?;
            let (file_slot, path) = workspace.allocate_path();
            let file = match create_private_file(&path) {
                Ok(file) => file,
                Err(error) => {
                    workspace.recycle_file_slot(file_slot);
                    workspace.rollback_file(reservation);
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
                allocated_bytes: reservation.file_bytes,
                file_slot,
            });
        }
        let output = self.outputs[bucket].as_mut().expect("output was created");
        let new_bytes = output
            .bytes
            .checked_add(ROW_INDEX_BYTES)
            .expect("one partition cannot exceed the u64 storage counter");
        let payload_bytes = new_bytes
            .div_ceil(workspace.allocation_unit_bytes)
            .checked_mul(workspace.allocation_unit_bytes)
            .expect("rounded partition allocation fits in u64");
        let required_bytes = workspace
            .allocation_bytes(FILE_METADATA_UNITS)?
            .checked_add(payload_bytes)
            .ok_or(Error::TemporaryStorageLimit {
                limit_bytes: workspace.limit_bytes,
            })?;
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
                file_slot,
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
                file_slot,
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
    fn non_4k_allocation_accounting_bounds_files_and_uses_private_creation() {
        const TEST_ALLOCATION_UNIT: u64 = 64 * 1024;
        const ONE_FILE_LIMIT: u64 = TEST_ALLOCATION_UNIT
            * (WORKSPACE_METADATA_UNITS + 1 + FILE_METADATA_UNITS + DIRECTORY_ENTRY_UNITS);
        let root = test_root("allocation");
        let mut workspace =
            TempWorkspace::new_with_allocation_unit(&root, ONE_FILE_LIMIT, TEST_ALLOCATION_UNIT)
                .expect("create 64 KiB allocation workspace");
        assert_eq!(
            workspace.used_bytes,
            TEST_ALLOCATION_UNIT * WORKSPACE_METADATA_UNITS
        );

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
            .expect("one file reservation remains");
        assert_eq!(workspace.used_bytes, ONE_FILE_LIMIT);
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
                limit_bytes: ONE_FILE_LIMIT
            }
        );

        let partition = writers.finish().expect("flush partition").pop().unwrap();
        workspace
            .remove_partition(&partition)
            .expect("remove first partition");
        assert_eq!(
            workspace.used_bytes,
            TEST_ALLOCATION_UNIT * (WORKSPACE_METADATA_UNITS + DIRECTORY_ENTRY_UNITS)
        );

        let mut replacement = PartitionWriters::new(1);
        replacement
            .write_row(&mut workspace, 0, 9)
            .expect("released file allocation is reusable");
        assert_eq!(
            replacement.outputs[0]
                .as_ref()
                .expect("replacement partition")
                .path,
            file_path,
            "reusing the same name bounds retained directory metadata"
        );
        let partition = replacement
            .finish()
            .expect("flush replacement")
            .pop()
            .unwrap();
        workspace
            .remove_partition(&partition)
            .expect("remove replacement partition");
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
        let mut workspace = TempWorkspace::new(&root, u64::MAX).expect("create workspace");
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
