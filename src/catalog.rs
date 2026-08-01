//! Typed catalog images and crash-safe local snapshots.
//!
//! [`CatalogImage`] is deliberately independent from a parser or execution
//! engine. Applications can construct a validated, typed columnar image and
//! persist it with [`SnapshotStore`]. The store serializes commits to one path,
//! bounds all allocations while reopening, and atomically replaces a previous
//! snapshot only after the replacement file has reached durable storage.

use std::collections::HashSet;
use std::error::Error;
use std::fmt;
#[cfg(any(test, windows))]
use std::fs;
use std::fs::File;
#[cfg(windows)]
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The eight bytes that identify a RustHouse catalog snapshot.
pub const SNAPSHOT_MAGIC: [u8; 8] = *b"RHCAT\0\r\n";

/// The snapshot format version written by this release.
pub const SNAPSHOT_FORMAT_VERSION: u16 = 1;

const HEADER_LEN: usize = 32;

/// The logical type of a catalog column.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataType {
    Int64,
    Float64,
    Bool,
    String,
}

/// Owned, nullable columnar values in a catalog image.
#[derive(Clone, Debug, PartialEq)]
pub enum ColumnData {
    Int64(Vec<Option<i64>>),
    Float64(Vec<Option<f64>>),
    Bool(Vec<Option<bool>>),
    String(Vec<Option<String>>),
}

impl ColumnData {
    /// Returns this column's logical type.
    #[must_use]
    pub const fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }

    /// Returns the number of rows in this column.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Int64(values) => values.len(),
            Self::Float64(values) => values.len(),
            Self::Bool(values) => values.len(),
            Self::String(values) => values.len(),
        }
    }

    /// Returns `true` when this column has no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn is_null(&self, index: usize) -> bool {
        match self {
            Self::Int64(values) => values[index].is_none(),
            Self::Float64(values) => values[index].is_none(),
            Self::Bool(values) => values[index].is_none(),
            Self::String(values) => values[index].is_none(),
        }
    }
}

/// A named, typed column in a [`TableImage`].
#[derive(Clone, Debug, PartialEq)]
pub struct ColumnImage {
    name: String,
    data: ColumnData,
}

impl ColumnImage {
    /// Creates a column, rejecting an empty or NUL-containing name.
    pub fn new(name: impl Into<String>, data: ColumnData) -> Result<Self, SnapshotError> {
        let name = name.into();
        validate_name(&name, "column")?;
        Ok(Self { name, data })
    }

    #[must_use]
    /// Returns the column name.
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    /// Returns the column's logical type.
    pub fn data_type(&self) -> DataType {
        self.data.data_type()
    }

    #[must_use]
    /// Returns the columnar values.
    pub fn data(&self) -> &ColumnData {
        &self.data
    }

    #[must_use]
    /// Returns the number of values in the column.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    #[must_use]
    /// Returns `true` when the column has no values.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

/// A validated table represented by equal-length typed columns.
#[derive(Clone, Debug, PartialEq)]
pub struct TableImage {
    name: String,
    columns: Vec<ColumnImage>,
    row_count: usize,
}

impl TableImage {
    /// Creates a table and validates column names and row counts.
    pub fn new(name: impl Into<String>, columns: Vec<ColumnImage>) -> Result<Self, SnapshotError> {
        let name = name.into();
        validate_name(&name, "table")?;
        let row_count = columns.first().map_or(0, ColumnImage::len);
        if columns.iter().any(|column| column.len() != row_count) {
            return Err(SnapshotError::InvalidImage(
                "all columns in a table must have the same row count".to_owned(),
            ));
        }
        ensure_unique(columns.iter().map(ColumnImage::name), "column")?;
        Ok(Self {
            name,
            columns,
            row_count,
        })
    }

    #[must_use]
    /// Returns the table name.
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    /// Returns the table's columns in schema order.
    pub fn columns(&self) -> &[ColumnImage] {
        &self.columns
    }

    #[must_use]
    /// Returns the shared row count of all columns.
    pub const fn row_count(&self) -> usize {
        self.row_count
    }
}

/// A named schema containing tables.
#[derive(Clone, Debug, PartialEq)]
pub struct SchemaImage {
    name: String,
    tables: Vec<TableImage>,
}

impl SchemaImage {
    /// Creates a schema, rejecting duplicate table names.
    pub fn new(name: impl Into<String>, tables: Vec<TableImage>) -> Result<Self, SnapshotError> {
        let name = name.into();
        validate_name(&name, "schema")?;
        ensure_unique(tables.iter().map(TableImage::name), "table")?;
        Ok(Self { name, tables })
    }

    #[must_use]
    /// Returns the schema name.
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    /// Returns the schema's tables.
    pub fn tables(&self) -> &[TableImage] {
        &self.tables
    }
}

/// A complete point-in-time catalog image.
#[derive(Clone, Debug, PartialEq)]
pub struct CatalogImage {
    generation: u64,
    schemas: Vec<SchemaImage>,
}

impl CatalogImage {
    /// Creates an image, rejecting duplicate schema names.
    pub fn new(generation: u64, schemas: Vec<SchemaImage>) -> Result<Self, SnapshotError> {
        ensure_unique(schemas.iter().map(SchemaImage::name), "schema")?;
        Ok(Self {
            generation,
            schemas,
        })
    }

    /// Returns an empty catalog at generation zero.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            generation: 0,
            schemas: Vec::new(),
        }
    }

    #[must_use]
    /// Returns the caller-assigned catalog generation.
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    /// Returns the catalog's schemas.
    pub fn schemas(&self) -> &[SchemaImage] {
        &self.schemas
    }
}

/// Resource limits applied before allocations while decoding and before commit.
///
/// Defaults accept practical local snapshots while preventing counts inside an
/// untrusted file from requesting unbounded memory.
#[derive(Clone, Debug)]
pub struct SnapshotLimits {
    /// Maximum complete file size, including the 32-byte header.
    pub max_snapshot_bytes: u64,
    /// Maximum schemas in one catalog.
    pub max_schemas: usize,
    /// Maximum total tables across all schemas.
    pub max_tables: usize,
    /// Maximum total columns across all tables.
    pub max_columns: usize,
    /// Maximum rows in any one table.
    pub max_rows_per_table: usize,
    /// Maximum sum of all column lengths.
    pub max_total_values: usize,
    /// Maximum UTF-8 bytes in a schema, table, or column name.
    pub max_name_bytes: usize,
    /// Maximum UTF-8 bytes in one string value.
    pub max_string_bytes: usize,
    /// Maximum UTF-8 bytes across all non-NULL string values.
    pub max_total_string_bytes: usize,
}

impl Default for SnapshotLimits {
    fn default() -> Self {
        Self {
            max_snapshot_bytes: 256 * 1024 * 1024,
            max_schemas: 1_024,
            max_tables: 16_384,
            max_columns: 65_536,
            max_rows_per_table: 16_000_000,
            max_total_values: 16_000_000,
            max_name_bytes: 16 * 1024,
            max_string_bytes: 16 * 1024 * 1024,
            max_total_string_bytes: 192 * 1024 * 1024,
        }
    }
}

impl SnapshotLimits {
    fn validate(&self) -> Result<(), SnapshotError> {
        if self.max_snapshot_bytes < HEADER_LEN as u64 {
            return Err(SnapshotError::InvalidLimits(
                "max_snapshot_bytes must include the 32-byte header",
            ));
        }
        Ok(())
    }
}

/// A format or structural error in an existing snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Corruption {
    /// A required header or payload field was incomplete.
    Truncated,
    /// The eight-byte file identifier was not recognized.
    BadMagic,
    /// The checksum covering header metadata did not match.
    BadHeaderChecksum,
    /// The checksum covering the complete payload did not match.
    BadPayloadChecksum,
    /// A header invariant was violated.
    InvalidHeader(&'static str),
    /// A payload invariant was violated.
    InvalidPayload(&'static str),
    /// A length-delimited name or value was not UTF-8.
    InvalidUtf8,
    /// Bytes remained after the declared payload.
    TrailingData,
}

impl fmt::Display for Corruption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("snapshot is truncated"),
            Self::BadMagic => f.write_str("snapshot magic does not match"),
            Self::BadHeaderChecksum => f.write_str("snapshot header checksum does not match"),
            Self::BadPayloadChecksum => f.write_str("snapshot payload checksum does not match"),
            Self::InvalidHeader(reason) => write!(f, "invalid snapshot header: {reason}"),
            Self::InvalidPayload(reason) => write!(f, "invalid snapshot payload: {reason}"),
            Self::InvalidUtf8 => f.write_str("snapshot contains invalid UTF-8"),
            Self::TrailingData => f.write_str("snapshot contains trailing data"),
        }
    }
}

/// Errors returned by catalog construction and snapshot operations.
#[derive(Debug)]
pub enum SnapshotError {
    /// A filesystem operation failed.
    Io(io::Error),
    /// Another process or handle owns the writer lock for this path.
    Locked(PathBuf),
    /// The filename is ambiguous or occupies the lock/temp sidecar namespace.
    ReservedSnapshotName(PathBuf),
    /// The final snapshot path is a symbolic link and cannot be atomically replaced safely.
    SymlinkSnapshot(PathBuf),
    /// This platform lacks the ACL operations required for private staging.
    UnsupportedPlatform(&'static str),
    /// The file was structurally invalid or failed a checksum.
    Corrupt(Corruption),
    /// The file has valid header integrity but uses an unknown version.
    UnsupportedVersion(u16),
    /// A configured resource limit was exceeded.
    LimitExceeded {
        /// The bounded resource.
        resource: &'static str,
        /// The configured maximum.
        limit: u64,
        /// The observed value, or `u64::MAX` after arithmetic overflow.
        actual: u64,
    },
    /// A caller tried to construct or commit an invalid catalog.
    InvalidImage(String),
    /// The supplied limit set could not describe even a file header.
    InvalidLimits(&'static str),
    /// A bounded, fallible memory reservation failed.
    AllocationFailed,
    #[cfg(test)]
    InjectedFailure(&'static str),
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "snapshot I/O failed: {error}"),
            Self::Locked(path) => write!(f, "snapshot writer is already open: {}", path.display()),
            Self::ReservedSnapshotName(path) => write!(
                f,
                "snapshot filename is reserved or ambiguous: {}",
                path.display()
            ),
            Self::SymlinkSnapshot(path) => write!(
                f,
                "snapshot path must not be a symbolic link: {}",
                path.display()
            ),
            Self::UnsupportedPlatform(reason) => {
                write!(f, "snapshot persistence is unsupported: {reason}")
            }
            Self::Corrupt(error) => write!(f, "corrupt snapshot: {error}"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported snapshot version {version}")
            }
            Self::LimitExceeded {
                resource,
                limit,
                actual,
            } => write!(f, "{resource} exceeds limit {limit} (found {actual})"),
            Self::InvalidImage(reason) => write!(f, "invalid catalog image: {reason}"),
            Self::InvalidLimits(reason) => write!(f, "invalid snapshot limits: {reason}"),
            Self::AllocationFailed => f.write_str("snapshot allocation failed"),
            #[cfg(test)]
            Self::InjectedFailure(point) => write!(f, "injected commit failure at {point}"),
        }
    }
}

impl Error for SnapshotError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Corrupt(error) => Some(error),
            _ => None,
        }
    }
}

impl Error for Corruption {}

impl From<io::Error> for SnapshotError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// An exclusively locked handle for one snapshot path.
///
/// `open` removes a temp file left by a crashed previous writer. [`load`](Self::load)
/// returns `Ok(None)` before the first commit. The lock is released when the
/// handle is dropped.
pub struct SnapshotStore {
    path: PathBuf,
    #[cfg(unix)]
    parent_dir: File,
    #[cfg(unix)]
    file_name: std::ffi::OsString,
    #[cfg(unix)]
    temp_name: std::ffi::OsString,
    temp_dir: PathBuf,
    temp_path: PathBuf,
    limits: SnapshotLimits,
    _lock: File,
    commit_lock: Mutex<()>,
}

impl fmt::Debug for SnapshotStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SnapshotStore")
            .field("path", &self.path)
            .field("temp_dir", &self.temp_dir)
            .field("temp_path", &self.temp_path)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl SnapshotStore {
    /// Opens a snapshot using [`SnapshotLimits::default`].
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SnapshotError> {
        Self::open_with_limits(path, SnapshotLimits::default())
    }

    /// Opens and exclusively locks a snapshot path with caller-supplied limits.
    ///
    /// The parent directory must already exist. A second writer receives
    /// [`SnapshotError::Locked`] rather than waiting indefinitely. The final
    /// path component must not be a symbolic link.
    pub fn open_with_limits(
        path: impl AsRef<Path>,
        limits: SnapshotLimits,
    ) -> Result<Self, SnapshotError> {
        ensure_platform_supported()?;
        limits.validate()?;
        let path = absolute_normalized_path(path.as_ref())?;
        let (lock_path, temp_dir) = sidecar_paths(&path)?;
        #[cfg(unix)]
        let parent_dir = File::open(path.parent().expect("normalized path has a parent"))?;
        #[cfg(unix)]
        let file_name = path
            .file_name()
            .expect("normalized path has a filename")
            .to_owned();
        #[cfg(unix)]
        drop(open_snapshot_file_at(&parent_dir, &file_name, &path)?);
        #[cfg(windows)]
        drop(open_snapshot_file_path(&path)?);
        #[cfg(unix)]
        let lock_name = lock_path.file_name().expect("sidecar path has a filename");
        #[cfg(unix)]
        let temp_name = temp_dir
            .file_name()
            .expect("sidecar path has a filename")
            .to_owned();
        #[cfg(unix)]
        let lock = open_lock_file_at(&parent_dir, lock_name)?;
        #[cfg(windows)]
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;
        match fs4::FileExt::try_lock(&lock) {
            Ok(()) => {}
            Err(fs4::TryLockError::WouldBlock) => {
                return Err(SnapshotError::Locked(path));
            }
            Err(fs4::TryLockError::Error(error)) => return Err(SnapshotError::Io(error)),
        }

        #[cfg(unix)]
        cleanup_staging_at(&parent_dir, &temp_name)?;
        #[cfg(windows)]
        cleanup_staging(&temp_dir, &path)?;
        let temp_path = temp_dir.join("snapshot");

        Ok(Self {
            path,
            #[cfg(unix)]
            parent_dir,
            #[cfg(unix)]
            file_name,
            #[cfg(unix)]
            temp_name,
            temp_dir,
            temp_path,
            limits,
            _lock: lock,
            commit_lock: Mutex::new(()),
        })
    }

    /// Returns the canonicalized absolute snapshot path captured at open.
    ///
    /// On Unix, filesystem operations remain tied to the opened parent
    /// directory if that directory is subsequently renamed.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the resource limits enforced by this handle.
    #[must_use]
    pub const fn limits(&self) -> &SnapshotLimits {
        &self.limits
    }

    /// Reads, validates, and decodes the current image.
    pub fn load(&self) -> Result<Option<CatalogImage>, SnapshotError> {
        let file = match self.open_current_snapshot()? {
            Some(file) => file,
            None => return Ok(None),
        };
        let file_len = file.metadata()?.len();
        check_limit("snapshot bytes", file_len, self.limits.max_snapshot_bytes)?;
        let allocation_len =
            usize::try_from(file_len).map_err(|_| SnapshotError::LimitExceeded {
                resource: "snapshot bytes",
                limit: usize::MAX as u64,
                actual: file_len,
            })?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(allocation_len)
            .map_err(|_| SnapshotError::AllocationFailed)?;
        file.take(file_len.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 != file_len {
            return Err(SnapshotError::Corrupt(Corruption::InvalidHeader(
                "file length changed while reading",
            )));
        }
        decode_snapshot(&bytes, &self.limits).map(Some)
    }

    /// Atomically and durably replaces the current snapshot.
    ///
    /// The encoded image is written inside a private staging directory on the
    /// destination filesystem, synced, and renamed over the destination. Unix
    /// then syncs the parent directory; Windows uses a write-through rename. A
    /// failure before rename leaves the previous snapshot untouched.
    pub fn commit(&self, image: &CatalogImage) -> Result<(), SnapshotError> {
        self.commit_inner(image, None)
    }

    fn commit_inner(
        &self,
        image: &CatalogImage,
        #[cfg_attr(not(test), allow(unused_variables))] failpoint: Option<Failpoint>,
    ) -> Result<(), SnapshotError> {
        validate_image_against_limits(image, &self.limits)?;
        let bytes = encode_snapshot(image, &self.limits)?;
        let _commit_guard = self
            .commit_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let existing = self.open_current_snapshot()?;

        #[cfg(unix)]
        cleanup_staging_at(&self.parent_dir, &self.temp_name)?;
        #[cfg(unix)]
        let staging_dir = create_secure_staging_dir_at(&self.parent_dir, &self.temp_name)?;
        #[cfg(unix)]
        let mut temp = create_secure_temp_at(&staging_dir)?;
        #[cfg(windows)]
        cleanup_staging(&self.temp_dir, &self.path)?;
        #[cfg(windows)]
        create_secure_staging_dir(&self.temp_dir)?;
        #[cfg(windows)]
        let mut temp = create_secure_temp(&self.temp_path)?;
        temp.write_all(&bytes)?;
        temp.sync_all()?;

        #[cfg(test)]
        if failpoint == Some(Failpoint::TempSynced) {
            return Err(SnapshotError::InjectedFailure("after temp sync"));
        }

        #[cfg(unix)]
        prepare_temp_security(&temp, existing.as_ref())?;
        #[cfg(windows)]
        prepare_temp_security(&temp, &self.temp_path, existing.as_ref())?;
        temp.sync_all()?;

        #[cfg(test)]
        if failpoint == Some(Failpoint::SecurityPrepared) {
            return Err(SnapshotError::InjectedFailure("after security preparation"));
        }

        drop(temp);
        #[cfg(unix)]
        self.publish_staged(&staging_dir)?;
        #[cfg(windows)]
        self.publish_staged()?;

        #[cfg(test)]
        if failpoint == Some(Failpoint::Renamed) {
            return Err(SnapshotError::InjectedFailure("after rename"));
        }

        #[cfg(unix)]
        {
            drop(staging_dir);
            let _ = rustix::fs::unlinkat(
                &self.parent_dir,
                &self.temp_name,
                rustix::fs::AtFlags::REMOVEDIR,
            );
            self.parent_dir.sync_all()?;
            Ok(())
        }
        #[cfg(windows)]
        {
            let _ = fs::remove_dir(&self.temp_dir);
            sync_parent(&self.path)
        }
    }

    #[cfg(unix)]
    fn open_current_snapshot(&self) -> Result<Option<File>, SnapshotError> {
        open_snapshot_file_at(&self.parent_dir, &self.file_name, &self.path)
    }

    #[cfg(windows)]
    fn open_current_snapshot(&self) -> Result<Option<File>, SnapshotError> {
        open_snapshot_file_path(&self.path)
    }

    #[cfg(unix)]
    fn publish_staged(&self, staging_dir: &File) -> Result<(), SnapshotError> {
        rustix::fs::renameat(staging_dir, "snapshot", &self.parent_dir, &self.file_name)
            .map_err(rustix_io_error)?;
        Ok(())
    }

    #[cfg(windows)]
    fn publish_staged(&self) -> Result<(), SnapshotError> {
        publish_temp(&self.temp_path, &self.path)
    }
}

#[cfg(unix)]
fn rustix_io_error(error: rustix::io::Errno) -> SnapshotError {
    io::Error::from_raw_os_error(error.raw_os_error()).into()
}

#[cfg(unix)]
fn open_lock_file_at(
    parent_dir: &File,
    lock_name: &std::ffi::OsStr,
) -> Result<File, SnapshotError> {
    use rustix::fs::{Mode, OFlags};

    let descriptor = rustix::fs::openat(
        parent_dir,
        lock_name,
        OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(rustix_io_error)?;
    let file = File::from(descriptor);
    if !file.metadata()?.file_type().is_file() {
        return Err(SnapshotError::InvalidImage(
            "snapshot lock path must identify a regular file".to_owned(),
        ));
    }
    Ok(file)
}

#[cfg(unix)]
fn open_snapshot_file_at(
    parent_dir: &File,
    file_name: &std::ffi::OsStr,
    display_path: &Path,
) -> Result<Option<File>, SnapshotError> {
    use rustix::fs::{Mode, OFlags};

    let descriptor = match rustix::fs::openat(
        parent_dir,
        file_name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(error) if error == rustix::io::Errno::NOENT => return Ok(None),
        Err(error) if error == rustix::io::Errno::LOOP => {
            return Err(SnapshotError::SymlinkSnapshot(display_path.to_owned()));
        }
        Err(error) => return Err(rustix_io_error(error)),
    };
    let file = File::from(descriptor);
    if !file.metadata()?.file_type().is_file() {
        return Err(SnapshotError::InvalidImage(
            "snapshot path must identify a regular file".to_owned(),
        ));
    }
    Ok(Some(file))
}

#[cfg(windows)]
fn open_snapshot_file_path(path: &Path) -> Result<Option<File>, SnapshotError> {
    use std::mem;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::ptr;
    use windows_sys::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FileAttributeTagInfo, GetFileInformationByHandleEx, OPEN_EXISTING, READ_CONTROL,
    };

    let wide_path = wide_path(path);
    // SAFETY: `wide_path` is NUL-terminated. OPEN_REPARSE_POINT opens the final
    // component itself, and the returned handle is either invalid or owned.
    let handle = unsafe {
        CreateFileW(
            wide_path.as_ptr(),
            GENERIC_READ | READ_CONTROL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let error = io::Error::last_os_error();
        return match error.raw_os_error().map(|code| code as u32) {
            Some(code) if code == ERROR_FILE_NOT_FOUND || code == ERROR_PATH_NOT_FOUND => Ok(None),
            _ => Err(error.into()),
        };
    }
    // SAFETY: CreateFileW returned a new owned handle. File closes it exactly
    // once, including all error returns below.
    let file = unsafe { File::from_raw_handle(handle) };
    let mut tag_info = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: The file handle is valid and `tag_info` is a correctly sized,
    // writable FILE_ATTRIBUTE_TAG_INFO buffer.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileAttributeTagInfo,
            (&raw mut tag_info).cast(),
            mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    if tag_info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(SnapshotError::SymlinkSnapshot(path.to_owned()));
    }
    Ok(Some(file))
}

fn absolute_normalized_path(path: &Path) -> Result<PathBuf, SnapshotError> {
    let file_name = path
        .file_name()
        .ok_or_else(|| SnapshotError::InvalidImage("snapshot path must name a file".to_owned()))?;
    if file_name.is_empty() {
        return Err(SnapshotError::InvalidImage(
            "snapshot path must name a file".to_owned(),
        ));
    }
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let parent = absolute.parent().ok_or_else(|| {
        SnapshotError::InvalidImage("snapshot path must have a parent directory".to_owned())
    })?;
    let canonical_parent = parent.canonicalize()?;
    Ok(canonical_parent.join(file_name))
}

#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
fn ensure_platform_supported() -> Result<(), SnapshotError> {
    Ok(())
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn ensure_platform_supported() -> Result<(), SnapshotError> {
    Err(SnapshotError::UnsupportedPlatform(
        "requires Windows, macOS, or Linux ACL semantics",
    ))
}

fn sidecar_paths(path: &Path) -> Result<(PathBuf, PathBuf), SnapshotError> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| SnapshotError::InvalidImage("snapshot filename must be UTF-8".to_owned()))?;
    let normalized_name = file_name.trim_end_matches([' ', '.']);
    #[cfg(windows)]
    if normalized_name != file_name {
        return Err(SnapshotError::ReservedSnapshotName(path.to_owned()));
    }
    if normalized_name.starts_with('.')
        && (normalized_name
            .get(normalized_name.len().saturating_sub(5)..)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".lock"))
            || normalized_name
                .get(normalized_name.len().saturating_sub(4)..)
                .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".tmp")))
    {
        return Err(SnapshotError::ReservedSnapshotName(path.to_owned()));
    }
    let parent = path.parent().ok_or_else(|| {
        SnapshotError::InvalidImage("snapshot path must have a parent directory".to_owned())
    })?;
    Ok((
        parent.join(format!(".{file_name}.lock")),
        parent.join(format!(".{file_name}.tmp")),
    ))
}

#[cfg(unix)]
fn cleanup_staging_at(parent_dir: &File, temp_name: &std::ffi::OsStr) -> Result<(), SnapshotError> {
    use rustix::fs::{AtFlags, Mode, OFlags};

    let staging_dir = match rustix::fs::openat(
        parent_dir,
        temp_name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::DIRECTORY,
        Mode::empty(),
    ) {
        Ok(descriptor) => File::from(descriptor),
        Err(error) if error == rustix::io::Errno::NOENT => return Ok(()),
        Err(error) if error == rustix::io::Errno::LOOP || error == rustix::io::Errno::NOTDIR => {
            match rustix::fs::unlinkat(parent_dir, temp_name, AtFlags::empty()) {
                Ok(()) => parent_dir.sync_all()?,
                Err(error) if error == rustix::io::Errno::NOENT => {}
                Err(error) => return Err(rustix_io_error(error)),
            }
            return Ok(());
        }
        Err(error) => return Err(rustix_io_error(error)),
    };

    match rustix::fs::unlinkat(&staging_dir, "snapshot", AtFlags::empty()) {
        Ok(()) => {}
        Err(error) if error == rustix::io::Errno::NOENT => {}
        Err(error) => return Err(rustix_io_error(error)),
    }
    drop(staging_dir);
    match rustix::fs::unlinkat(parent_dir, temp_name, AtFlags::REMOVEDIR) {
        Ok(()) => parent_dir.sync_all()?,
        Err(error) if error == rustix::io::Errno::NOENT => {}
        Err(error) => return Err(rustix_io_error(error)),
    }
    Ok(())
}

#[cfg(windows)]
fn cleanup_staging(temp_dir: &Path, path: &Path) -> Result<(), SnapshotError> {
    match fs::symlink_metadata(temp_dir) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(temp_dir)?;
            sync_parent(path)?;
        }
        Ok(_) => {
            fs::remove_file(temp_dir)?;
            sync_parent(path)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

#[cfg(all(unix, any(target_os = "linux", target_os = "macos")))]
fn create_secure_staging_dir_at(
    parent_dir: &File,
    temp_name: &std::ffi::OsStr,
) -> Result<File, SnapshotError> {
    use rustix::fs::{Mode, OFlags};

    rustix::fs::mkdirat(parent_dir, temp_name, Mode::RWXU).map_err(rustix_io_error)?;
    let descriptor = rustix::fs::openat(
        parent_dir,
        temp_name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::DIRECTORY,
        Mode::empty(),
    )
    .map_err(rustix_io_error)?;
    let directory = File::from(descriptor);
    set_private_unix_acl(&directory, true)?;
    rustix::fs::fchmod(&directory, Mode::RWXU).map_err(rustix_io_error)?;
    Ok(directory)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn create_secure_staging_dir_at(
    _parent_dir: &File,
    _temp_name: &std::ffi::OsStr,
) -> Result<File, SnapshotError> {
    ensure_platform_supported()?;
    unreachable!("unsupported platforms return an error")
}

#[cfg(all(unix, any(target_os = "linux", target_os = "macos")))]
fn create_secure_temp_at(staging_dir: &File) -> Result<File, SnapshotError> {
    use rustix::fs::{Mode, OFlags};

    let descriptor = rustix::fs::openat(
        staging_dir,
        "snapshot",
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(rustix_io_error)?;
    let file = File::from(descriptor);
    set_private_unix_acl(&file, false)?;
    rustix::fs::fchmod(&file, Mode::RUSR | Mode::WUSR).map_err(rustix_io_error)?;
    Ok(file)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn create_secure_temp_at(_staging_dir: &File) -> Result<File, SnapshotError> {
    ensure_platform_supported()?;
    unreachable!("unsupported platforms return an error")
}

#[cfg(target_os = "linux")]
fn set_private_unix_acl(file: &File, is_directory: bool) -> Result<(), SnapshotError> {
    use exacl::{AclEntry, Perm};
    use std::os::fd::AsRawFd;

    let owner_permissions = if is_directory {
        Perm::READ | Perm::WRITE | Perm::EXECUTE
    } else {
        Perm::READ | Perm::WRITE
    };
    let acl = [
        AclEntry::allow_user("", owner_permissions, None),
        AclEntry::allow_group("", Perm::empty(), None),
        AclEntry::allow_other(Perm::empty(), None),
    ];
    let descriptor_path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
    exacl::setfacl(&[descriptor_path], &acl, None)?;
    Ok(())
}

#[cfg(target_os = "macos")]
const ACL_TYPE_EXTENDED: std::ffi::c_uint = 256;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn acl_free(object: *mut std::ffi::c_void) -> std::ffi::c_int;
    fn acl_get_fd_np(
        file_descriptor: std::ffi::c_int,
        acl_type: std::ffi::c_uint,
    ) -> *mut std::ffi::c_void;
    fn acl_init(entry_count: std::ffi::c_int) -> *mut std::ffi::c_void;
    fn acl_set_fd_np(
        file_descriptor: std::ffi::c_int,
        acl: *mut std::ffi::c_void,
        acl_type: std::ffi::c_uint,
    ) -> std::ffi::c_int;
}

#[cfg(target_os = "macos")]
fn install_macos_acl(file: &File, acl: *mut std::ffi::c_void) -> Result<(), SnapshotError> {
    use std::os::fd::AsRawFd;

    // SAFETY: `acl` is a live ACL object and `file` remains open for the call.
    let set_result = unsafe { acl_set_fd_np(file.as_raw_fd(), acl, ACL_TYPE_EXTENDED) };
    let set_error = (set_result != 0).then(io::Error::last_os_error);
    // SAFETY: The caller transfers ownership of `acl` to this function.
    let free_result = unsafe { acl_free(acl) };
    if let Some(error) = set_error {
        return Err(error.into());
    }
    if free_result != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn set_private_unix_acl(file: &File, _is_directory: bool) -> Result<(), SnapshotError> {
    // SAFETY: acl_init returns a new empty ACL object or null and sets errno.
    let acl = unsafe { acl_init(1) };
    if acl.is_null() {
        return Err(io::Error::last_os_error().into());
    }
    install_macos_acl(file, acl)
}

#[cfg(windows)]
struct PrivateSecurityDescriptor(windows_sys::Win32::Security::PSECURITY_DESCRIPTOR);

#[cfg(windows)]
impl PrivateSecurityDescriptor {
    fn new() -> Result<Self, SnapshotError> {
        use std::ptr;
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        use windows_sys::Win32::Security::PSECURITY_DESCRIPTOR;

        let sddl: Vec<u16> = "D:P(A;;FA;;;SY)(A;;FA;;;OW)\0".encode_utf16().collect();
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: `sddl` is NUL-terminated and `descriptor` points to writable
        // storage for the LocalAlloc-owned descriptor returned by Win32.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        } == 0
        {
            Err(io::Error::last_os_error().into())
        } else {
            Ok(Self(descriptor))
        }
    }

    fn attributes(&self) -> windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
        use std::mem;
        use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;

        SECURITY_ATTRIBUTES {
            nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        }
    }
}

#[cfg(windows)]
impl Drop for PrivateSecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: The descriptor is owned by this value and was allocated by
        // ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe { windows_sys::Win32::Foundation::LocalFree(self.0) };
    }
}

#[cfg(windows)]
fn create_secure_staging_dir(path: &Path) -> Result<(), SnapshotError> {
    use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;

    let path = wide_path(path);
    let descriptor = PrivateSecurityDescriptor::new()?;
    let attributes = descriptor.attributes();
    // SAFETY: `path` is NUL-terminated and both the attributes and owned
    // descriptor remain alive for the duration of the call.
    if unsafe { CreateDirectoryW(path.as_ptr(), &attributes) } == 0 {
        Err(io::Error::last_os_error().into())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn create_secure_temp(path: &Path) -> Result<File, SnapshotError> {
    use std::os::windows::io::FromRawHandle;
    use std::ptr;
    use windows_sys::Win32::Foundation::{GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, WRITE_DAC, WRITE_OWNER,
    };

    let path = wide_path(path);
    let descriptor = PrivateSecurityDescriptor::new()?;
    let attributes = descriptor.attributes();
    // SAFETY: `path` is NUL-terminated, `attributes` and its descriptor remain
    // alive for the call, and CREATE_NEW prevents following an existing temp.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_WRITE | WRITE_DAC | WRITE_OWNER,
            0,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };
    let create_error = (handle == INVALID_HANDLE_VALUE).then(io::Error::last_os_error);
    if let Some(error) = create_error {
        Err(error.into())
    } else {
        // SAFETY: CreateFileW returned a new, owned file handle. File assumes
        // ownership and closes it exactly once.
        Ok(unsafe { File::from_raw_handle(handle) })
    }
}

#[cfg(unix)]
fn prepare_temp_security(temp: &File, existing: Option<&File>) -> Result<(), SnapshotError> {
    use std::os::unix::fs::MetadataExt;

    if let Some(existing) = existing {
        let metadata = existing.metadata()?;
        rustix::fs::fchown(
            temp,
            Some(rustix::fs::Uid::from_raw(metadata.uid())),
            Some(rustix::fs::Gid::from_raw(metadata.gid())),
        )
        .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
        copy_unix_acl(temp, existing)?;
        temp.set_permissions(metadata.permissions())?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn copy_unix_acl(temp: &File, existing: &File) -> Result<(), SnapshotError> {
    use std::os::fd::AsRawFd;

    let existing_path = PathBuf::from(format!("/proc/self/fd/{}", existing.as_raw_fd()));
    let temp_path = PathBuf::from(format!("/proc/self/fd/{}", temp.as_raw_fd()));
    let acl = exacl::getfacl(existing_path, None)?;
    exacl::setfacl(&[temp_path], &acl, None)?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn copy_unix_acl(temp: &File, existing: &File) -> Result<(), SnapshotError> {
    use std::os::fd::AsRawFd;

    // SAFETY: Both descriptors are live for the duration of this function.
    // acl_get_fd_np returns an owned ACL object or null and sets errno.
    let acl = unsafe { acl_get_fd_np(existing.as_raw_fd(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(error.into())
        };
    }
    install_macos_acl(temp, acl)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn copy_unix_acl(_temp: &File, _existing: &File) -> Result<(), SnapshotError> {
    Ok(())
}

#[cfg(windows)]
fn prepare_temp_security(
    temp: &File,
    _temp_path: &Path,
    existing: Option<&File>,
) -> Result<(), SnapshotError> {
    use std::os::windows::io::AsRawHandle;
    use std::ptr;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, GetKernelObjectSecurity,
        GetSecurityDescriptorControl, OWNER_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
        SetKernelObjectSecurity, UNPROTECTED_DACL_SECURITY_INFORMATION,
    };

    let Some(existing) = existing else {
        return Ok(());
    };
    let requested =
        OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    let mut needed = 0u32;
    // SAFETY: The first call intentionally supplies no output buffer so Win32
    // reports the required self-relative security descriptor length.
    unsafe {
        GetKernelObjectSecurity(
            existing.as_raw_handle() as HANDLE,
            requested,
            ptr::null_mut(),
            0,
            &mut needed,
        )
    };
    if needed == 0 {
        return Err(io::Error::last_os_error().into());
    }
    let descriptor_len = usize::try_from(needed).map_err(|_| SnapshotError::AllocationFailed)?;
    let mut descriptor = Vec::new();
    descriptor
        .try_reserve_exact(descriptor_len)
        .map_err(|_| SnapshotError::AllocationFailed)?;
    descriptor.resize(descriptor_len, 0u8);
    let descriptor_ptr: PSECURITY_DESCRIPTOR = descriptor.as_mut_ptr().cast();
    // SAFETY: `descriptor` has the exact writable size requested by the first
    // call and both path and buffer remain valid for the duration of the call.
    if unsafe {
        GetKernelObjectSecurity(
            existing.as_raw_handle() as HANDLE,
            requested,
            descriptor_ptr,
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(io::Error::last_os_error().into());
    }

    let mut control = 0u16;
    let mut revision = 0u32;
    // SAFETY: `descriptor_ptr` contains a validated security descriptor
    // returned by GetKernelObjectSecurity; both output pointers are valid.
    if unsafe { GetSecurityDescriptorControl(descriptor_ptr, &mut control, &mut revision) } == 0 {
        return Err(io::Error::last_os_error().into());
    }
    let dacl_protection = if control & SE_DACL_PROTECTED != 0 {
        PROTECTED_DACL_SECURITY_INFORMATION
    } else {
        UNPROTECTED_DACL_SECURITY_INFORMATION
    };
    // SAFETY: `temp` owns a valid handle opened with WRITE_DAC and WRITE_OWNER,
    // and `descriptor_ptr` remains valid through this call.
    if unsafe {
        SetKernelObjectSecurity(
            temp.as_raw_handle() as HANDLE,
            requested | dacl_protection,
            descriptor_ptr,
        )
    } == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(windows)]
fn wide_path(path: &Path) -> Vec<u16> {
    use std::iter;
    use std::os::windows::ffi::OsStrExt;

    path.as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect()
}

#[cfg(windows)]
fn publish_temp(temp_path: &Path, path: &Path) -> Result<(), SnapshotError> {
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let temp_path = wide_path(temp_path);
    let path = wide_path(path);
    // SAFETY: Both pointers reference NUL-terminated UTF-16 buffers that remain
    // alive for the call. The paths are distinct files in the same directory.
    let result = unsafe {
        MoveFileExW(
            temp_path.as_ptr(),
            path.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error().into())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn sync_parent(_path: &Path) -> Result<(), SnapshotError> {
    // Windows has no portable directory-fsync operation. Snapshot publication
    // is made durable by MOVEFILE_WRITE_THROUGH in publish_temp instead.
    Ok(())
}

#[cfg(not(any(unix, windows)))]
compile_error!("catalog snapshots require Unix or Windows filesystem semantics");

fn validate_name(name: &str, kind: &'static str) -> Result<(), SnapshotError> {
    if name.is_empty() {
        return Err(SnapshotError::InvalidImage(format!(
            "{kind} name must not be empty"
        )));
    }
    if name.contains('\0') {
        return Err(SnapshotError::InvalidImage(format!(
            "{kind} name must not contain NUL"
        )));
    }
    Ok(())
}

fn ensure_unique<'a>(
    names: impl Iterator<Item = &'a str>,
    kind: &'static str,
) -> Result<(), SnapshotError> {
    let names = names;
    let mut seen = HashSet::new();
    seen.try_reserve(names.size_hint().0)
        .map_err(|_| SnapshotError::AllocationFailed)?;
    for name in names {
        if !seen.insert(name) {
            return Err(SnapshotError::InvalidImage(format!(
                "duplicate {kind} name {name:?}"
            )));
        }
    }
    Ok(())
}

fn check_limit(resource: &'static str, actual: u64, limit: u64) -> Result<(), SnapshotError> {
    if actual > limit {
        Err(SnapshotError::LimitExceeded {
            resource,
            limit,
            actual,
        })
    } else {
        Ok(())
    }
}

fn as_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn validate_image_against_limits(
    image: &CatalogImage,
    limits: &SnapshotLimits,
) -> Result<(), SnapshotError> {
    check_limit(
        "schemas",
        as_u64(image.schemas.len()),
        as_u64(limits.max_schemas),
    )?;
    let mut tables = 0usize;
    let mut columns = 0usize;
    let mut values = 0usize;
    let mut string_bytes = 0usize;

    for schema in &image.schemas {
        validate_bounded_name(&schema.name, "schema", limits)?;
        tables = tables
            .checked_add(schema.tables.len())
            .ok_or(SnapshotError::LimitExceeded {
                resource: "tables",
                limit: as_u64(limits.max_tables),
                actual: u64::MAX,
            })?;
        check_limit("tables", as_u64(tables), as_u64(limits.max_tables))?;
        for table in &schema.tables {
            validate_bounded_name(&table.name, "table", limits)?;
            check_limit(
                "rows per table",
                as_u64(table.row_count),
                as_u64(limits.max_rows_per_table),
            )?;
            columns =
                columns
                    .checked_add(table.columns.len())
                    .ok_or(SnapshotError::LimitExceeded {
                        resource: "columns",
                        limit: as_u64(limits.max_columns),
                        actual: u64::MAX,
                    })?;
            check_limit("columns", as_u64(columns), as_u64(limits.max_columns))?;
            for column in &table.columns {
                validate_bounded_name(&column.name, "column", limits)?;
                values = values
                    .checked_add(column.len())
                    .ok_or(SnapshotError::LimitExceeded {
                        resource: "total values",
                        limit: as_u64(limits.max_total_values),
                        actual: u64::MAX,
                    })?;
                check_limit(
                    "total values",
                    as_u64(values),
                    as_u64(limits.max_total_values),
                )?;
                if let ColumnData::String(entries) = &column.data {
                    for value in entries.iter().flatten() {
                        check_limit(
                            "string bytes",
                            as_u64(value.len()),
                            as_u64(limits.max_string_bytes),
                        )?;
                        string_bytes = string_bytes.checked_add(value.len()).ok_or(
                            SnapshotError::LimitExceeded {
                                resource: "total string bytes",
                                limit: as_u64(limits.max_total_string_bytes),
                                actual: u64::MAX,
                            },
                        )?;
                        check_limit(
                            "total string bytes",
                            as_u64(string_bytes),
                            as_u64(limits.max_total_string_bytes),
                        )?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn validate_bounded_name(
    name: &str,
    kind: &'static str,
    limits: &SnapshotLimits,
) -> Result<(), SnapshotError> {
    validate_name(name, kind)?;
    check_limit(
        "name bytes",
        as_u64(name.len()),
        as_u64(limits.max_name_bytes),
    )
}

struct Encoder {
    bytes: Vec<u8>,
    max_len: usize,
}

impl Encoder {
    fn new(max_len: usize) -> Self {
        Self {
            bytes: Vec::new(),
            max_len,
        }
    }

    fn extend(&mut self, bytes: &[u8]) -> Result<(), SnapshotError> {
        let new_len =
            self.bytes
                .len()
                .checked_add(bytes.len())
                .ok_or(SnapshotError::LimitExceeded {
                    resource: "snapshot bytes",
                    limit: as_u64(self.max_len),
                    actual: u64::MAX,
                })?;
        check_limit("snapshot bytes", as_u64(new_len), as_u64(self.max_len))?;
        self.bytes
            .try_reserve(bytes.len())
            .map_err(|_| SnapshotError::AllocationFailed)?;
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn u8(&mut self, value: u8) -> Result<(), SnapshotError> {
        self.extend(&[value])
    }

    fn u32(&mut self, value: u32) -> Result<(), SnapshotError> {
        self.extend(&value.to_le_bytes())
    }

    fn u64(&mut self, value: u64) -> Result<(), SnapshotError> {
        self.extend(&value.to_le_bytes())
    }

    fn usize_u32(&mut self, value: usize, resource: &'static str) -> Result<(), SnapshotError> {
        let value = u32::try_from(value).map_err(|_| SnapshotError::LimitExceeded {
            resource,
            limit: u64::from(u32::MAX),
            actual: as_u64(value),
        })?;
        self.u32(value)
    }

    fn string(&mut self, value: &str, resource: &'static str) -> Result<(), SnapshotError> {
        self.usize_u32(value.len(), resource)?;
        self.extend(value.as_bytes())
    }
}

fn encode_snapshot(
    image: &CatalogImage,
    limits: &SnapshotLimits,
) -> Result<Vec<u8>, SnapshotError> {
    let max_payload = usize::try_from(limits.max_snapshot_bytes - HEADER_LEN as u64)
        .unwrap_or(usize::MAX - HEADER_LEN);
    let mut payload = Encoder::new(max_payload);
    payload.u64(image.generation)?;
    payload.usize_u32(image.schemas.len(), "schemas")?;
    for schema in &image.schemas {
        payload.string(&schema.name, "name bytes")?;
        payload.usize_u32(schema.tables.len(), "tables")?;
        for table in &schema.tables {
            payload.string(&table.name, "name bytes")?;
            payload.u64(as_u64(table.row_count))?;
            payload.usize_u32(table.columns.len(), "columns")?;
            for column in &table.columns {
                payload.string(&column.name, "name bytes")?;
                payload.u8(type_tag(column.data_type()))?;
                payload.extend(&[0, 0, 0])?;
                let bitmap_len = table.row_count.div_ceil(8);
                let mut bitmap = try_vec(bitmap_len)?;
                bitmap.resize(bitmap_len, 0u8);
                for row in 0..table.row_count {
                    if !column.data.is_null(row) {
                        bitmap[row / 8] |= 1 << (row % 8);
                    }
                }
                payload.extend(&bitmap)?;
                encode_values(&mut payload, &column.data)?;
            }
        }
    }

    let payload_checksum = crc32(&payload.bytes);
    let payload_len = as_u64(payload.bytes.len());
    let mut header = [0u8; HEADER_LEN];
    header[0..8].copy_from_slice(&SNAPSHOT_MAGIC);
    header[8..10].copy_from_slice(&SNAPSHOT_FORMAT_VERSION.to_le_bytes());
    header[10..12].copy_from_slice(&0u16.to_le_bytes());
    header[12..16].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());
    header[16..24].copy_from_slice(&payload_len.to_le_bytes());
    header[24..28].copy_from_slice(&payload_checksum.to_le_bytes());
    let header_checksum = crc32(&header[..28]);
    header[28..32].copy_from_slice(&header_checksum.to_le_bytes());

    let total_len =
        HEADER_LEN
            .checked_add(payload.bytes.len())
            .ok_or(SnapshotError::LimitExceeded {
                resource: "snapshot bytes",
                limit: limits.max_snapshot_bytes,
                actual: u64::MAX,
            })?;
    check_limit(
        "snapshot bytes",
        as_u64(total_len),
        limits.max_snapshot_bytes,
    )?;
    let mut encoded = Vec::new();
    encoded
        .try_reserve_exact(total_len)
        .map_err(|_| SnapshotError::AllocationFailed)?;
    encoded.extend_from_slice(&header);
    encoded.extend_from_slice(&payload.bytes);
    Ok(encoded)
}

fn encode_values(encoder: &mut Encoder, data: &ColumnData) -> Result<(), SnapshotError> {
    match data {
        ColumnData::Int64(values) => {
            for value in values.iter().flatten() {
                encoder.extend(&value.to_le_bytes())?;
            }
        }
        ColumnData::Float64(values) => {
            for value in values.iter().flatten() {
                encoder.extend(&value.to_bits().to_le_bytes())?;
            }
        }
        ColumnData::Bool(values) => {
            for value in values.iter().flatten() {
                encoder.u8(u8::from(*value))?;
            }
        }
        ColumnData::String(values) => {
            for value in values.iter().flatten() {
                encoder.string(value, "string bytes")?;
            }
        }
    }
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

struct Decoder<'a> {
    bytes: &'a [u8],
    position: usize,
    limits: &'a SnapshotLimits,
    total_tables: usize,
    total_columns: usize,
    total_values: usize,
    total_string_bytes: usize,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8], limits: &'a SnapshotLimits) -> Self {
        Self {
            bytes,
            position: 0,
            limits,
            total_tables: 0,
            total_columns: 0,
            total_values: 0,
            total_string_bytes: 0,
        }
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], SnapshotError> {
        let end = self
            .position
            .checked_add(length)
            .ok_or(SnapshotError::Corrupt(Corruption::Truncated))?;
        let value = self
            .bytes
            .get(self.position..end)
            .ok_or(SnapshotError::Corrupt(Corruption::Truncated))?;
        self.position = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, SnapshotError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, SnapshotError> {
        let bytes: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| SnapshotError::Corrupt(Corruption::Truncated))?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn u64(&mut self) -> Result<u64, SnapshotError> {
        let bytes: [u8; 8] = self
            .take(8)?
            .try_into()
            .map_err(|_| SnapshotError::Corrupt(Corruption::Truncated))?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn bounded_count(
        &mut self,
        resource: &'static str,
        maximum: usize,
    ) -> Result<usize, SnapshotError> {
        let count = usize::try_from(self.u32()?).map_err(|_| SnapshotError::LimitExceeded {
            resource,
            limit: as_u64(maximum),
            actual: u64::MAX,
        })?;
        check_limit(resource, as_u64(count), as_u64(maximum))?;
        Ok(count)
    }

    fn string(&mut self, resource: &'static str, maximum: usize) -> Result<String, SnapshotError> {
        let length = self.bounded_count(resource, maximum)?;
        self.string_with_length(length)
    }

    fn string_with_length(&mut self, length: usize) -> Result<String, SnapshotError> {
        let bytes = self.take(length)?;
        let value = std::str::from_utf8(bytes)
            .map_err(|_| SnapshotError::Corrupt(Corruption::InvalidUtf8))?;
        let mut owned = String::new();
        owned
            .try_reserve_exact(length)
            .map_err(|_| SnapshotError::AllocationFailed)?;
        owned.push_str(value);
        Ok(owned)
    }

    fn add_total(
        current: &mut usize,
        amount: usize,
        resource: &'static str,
        maximum: usize,
    ) -> Result<(), SnapshotError> {
        *current = current
            .checked_add(amount)
            .ok_or(SnapshotError::LimitExceeded {
                resource,
                limit: as_u64(maximum),
                actual: u64::MAX,
            })?;
        check_limit(resource, as_u64(*current), as_u64(maximum))
    }

    fn done(&self) -> bool {
        self.position == self.bytes.len()
    }
}

fn decode_snapshot(bytes: &[u8], limits: &SnapshotLimits) -> Result<CatalogImage, SnapshotError> {
    if bytes.len() < HEADER_LEN {
        return Err(SnapshotError::Corrupt(Corruption::Truncated));
    }
    if bytes[..8] != SNAPSHOT_MAGIC {
        return Err(SnapshotError::Corrupt(Corruption::BadMagic));
    }
    let header_checksum = read_u32(&bytes[28..32]);
    if crc32(&bytes[..28]) != header_checksum {
        return Err(SnapshotError::Corrupt(Corruption::BadHeaderChecksum));
    }
    let version = read_u16(&bytes[8..10]);
    if version != SNAPSHOT_FORMAT_VERSION {
        return Err(SnapshotError::UnsupportedVersion(version));
    }
    if read_u16(&bytes[10..12]) != 0 {
        return Err(SnapshotError::Corrupt(Corruption::InvalidHeader(
            "flags must be zero",
        )));
    }
    if read_u32(&bytes[12..16]) != HEADER_LEN as u32 {
        return Err(SnapshotError::Corrupt(Corruption::InvalidHeader(
            "header length does not match version 1",
        )));
    }
    let payload_len = read_u64(&bytes[16..24]);
    check_limit(
        "snapshot bytes",
        payload_len.saturating_add(HEADER_LEN as u64),
        limits.max_snapshot_bytes,
    )?;
    let payload_len = usize::try_from(payload_len).map_err(|_| SnapshotError::LimitExceeded {
        resource: "snapshot bytes",
        limit: limits.max_snapshot_bytes,
        actual: u64::MAX,
    })?;
    let expected_len = HEADER_LEN
        .checked_add(payload_len)
        .ok_or(SnapshotError::Corrupt(Corruption::InvalidHeader(
            "payload length overflows address space",
        )))?;
    if expected_len > bytes.len() {
        return Err(SnapshotError::Corrupt(Corruption::Truncated));
    }
    if expected_len < bytes.len() {
        return Err(SnapshotError::Corrupt(Corruption::TrailingData));
    }
    let payload = &bytes[HEADER_LEN..];
    if crc32(payload) != read_u32(&bytes[24..28]) {
        return Err(SnapshotError::Corrupt(Corruption::BadPayloadChecksum));
    }
    decode_payload(payload, limits)
}

fn decode_payload(payload: &[u8], limits: &SnapshotLimits) -> Result<CatalogImage, SnapshotError> {
    let mut decoder = Decoder::new(payload, limits);
    let generation = decoder.u64()?;
    let schema_count = decoder.bounded_count("schemas", limits.max_schemas)?;
    let mut schemas = try_vec(schema_count)?;
    for _ in 0..schema_count {
        let name = decoder.string("name bytes", limits.max_name_bytes)?;
        validate_decoded_name(&name)?;
        let table_count = decoder.bounded_count("tables", limits.max_tables)?;
        Decoder::add_total(
            &mut decoder.total_tables,
            table_count,
            "tables",
            limits.max_tables,
        )?;
        let mut tables = try_vec(table_count)?;
        for _ in 0..table_count {
            tables.push(decode_table(&mut decoder)?);
        }
        ensure_unique_corrupt(tables.iter().map(TableImage::name), "duplicate table name")?;
        schemas.push(SchemaImage { name, tables });
    }
    if !decoder.done() {
        return Err(SnapshotError::Corrupt(Corruption::TrailingData));
    }
    ensure_unique_corrupt(
        schemas.iter().map(SchemaImage::name),
        "duplicate schema name",
    )?;
    Ok(CatalogImage {
        generation,
        schemas,
    })
}

fn decode_table(decoder: &mut Decoder<'_>) -> Result<TableImage, SnapshotError> {
    let name = decoder.string("name bytes", decoder.limits.max_name_bytes)?;
    validate_decoded_name(&name)?;
    let row_count_u64 = decoder.u64()?;
    check_limit(
        "rows per table",
        row_count_u64,
        as_u64(decoder.limits.max_rows_per_table),
    )?;
    let row_count = usize::try_from(row_count_u64).map_err(|_| SnapshotError::LimitExceeded {
        resource: "rows per table",
        limit: as_u64(decoder.limits.max_rows_per_table),
        actual: row_count_u64,
    })?;
    let column_count = decoder.bounded_count("columns", decoder.limits.max_columns)?;
    if column_count == 0 && row_count != 0 {
        return Err(SnapshotError::Corrupt(Corruption::InvalidPayload(
            "a table without columns must have zero rows",
        )));
    }
    Decoder::add_total(
        &mut decoder.total_columns,
        column_count,
        "columns",
        decoder.limits.max_columns,
    )?;
    let table_values = row_count
        .checked_mul(column_count)
        .ok_or(SnapshotError::LimitExceeded {
            resource: "total values",
            limit: as_u64(decoder.limits.max_total_values),
            actual: u64::MAX,
        })?;
    Decoder::add_total(
        &mut decoder.total_values,
        table_values,
        "total values",
        decoder.limits.max_total_values,
    )?;
    let mut columns = try_vec(column_count)?;
    for _ in 0..column_count {
        columns.push(decode_column(decoder, row_count)?);
    }
    ensure_unique_corrupt(
        columns.iter().map(ColumnImage::name),
        "duplicate column name",
    )?;
    Ok(TableImage {
        name,
        columns,
        row_count,
    })
}

fn decode_column(
    decoder: &mut Decoder<'_>,
    row_count: usize,
) -> Result<ColumnImage, SnapshotError> {
    let name = decoder.string("name bytes", decoder.limits.max_name_bytes)?;
    validate_decoded_name(&name)?;
    let tag = decoder.u8()?;
    if decoder.take(3)? != [0, 0, 0] {
        return Err(SnapshotError::Corrupt(Corruption::InvalidPayload(
            "column reserved bytes must be zero",
        )));
    }
    let bitmap_len = row_count.div_ceil(8);
    let bitmap = decoder.take(bitmap_len)?;
    if row_count & 7 != 0
        && bitmap
            .last()
            .is_some_and(|last| last & (!0u8 << (row_count % 8)) != 0)
    {
        return Err(SnapshotError::Corrupt(Corruption::InvalidPayload(
            "null bitmap padding bits must be zero",
        )));
    }
    let data = match tag {
        1 => ColumnData::Int64(decode_fixed(decoder, row_count, bitmap, |decoder| {
            let bytes: [u8; 8] = decoder
                .take(8)?
                .try_into()
                .map_err(|_| SnapshotError::Corrupt(Corruption::Truncated))?;
            Ok(i64::from_le_bytes(bytes))
        })?),
        2 => ColumnData::Float64(decode_fixed(decoder, row_count, bitmap, |decoder| {
            let bytes: [u8; 8] = decoder
                .take(8)?
                .try_into()
                .map_err(|_| SnapshotError::Corrupt(Corruption::Truncated))?;
            Ok(f64::from_bits(u64::from_le_bytes(bytes)))
        })?),
        3 => ColumnData::Bool(decode_fixed(
            decoder,
            row_count,
            bitmap,
            |decoder| match decoder.u8()? {
                0 => Ok(false),
                1 => Ok(true),
                _ => Err(SnapshotError::Corrupt(Corruption::InvalidPayload(
                    "boolean value must be zero or one",
                ))),
            },
        )?),
        4 => {
            let maximum = decoder.limits.max_string_bytes;
            let values = decode_fixed(decoder, row_count, bitmap, |decoder| {
                let length = decoder.bounded_count("string bytes", maximum)?;
                Decoder::add_total(
                    &mut decoder.total_string_bytes,
                    length,
                    "total string bytes",
                    decoder.limits.max_total_string_bytes,
                )?;
                decoder.string_with_length(length)
            })?;
            ColumnData::String(values)
        }
        _ => {
            return Err(SnapshotError::Corrupt(Corruption::InvalidPayload(
                "unknown column type tag",
            )));
        }
    };
    Ok(ColumnImage { name, data })
}

fn decode_fixed<T>(
    decoder: &mut Decoder<'_>,
    row_count: usize,
    bitmap: &[u8],
    mut decode_value: impl FnMut(&mut Decoder<'_>) -> Result<T, SnapshotError>,
) -> Result<Vec<Option<T>>, SnapshotError> {
    let mut values = try_vec(row_count)?;
    for row in 0..row_count {
        if bitmap[row / 8] & (1 << (row % 8)) == 0 {
            values.push(None);
        } else {
            values.push(Some(decode_value(decoder)?));
        }
    }
    Ok(values)
}

fn try_vec<T>(capacity: usize) -> Result<Vec<T>, SnapshotError> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(capacity)
        .map_err(|_| SnapshotError::AllocationFailed)?;
    Ok(values)
}

fn validate_decoded_name(name: &str) -> Result<(), SnapshotError> {
    if name.is_empty() || name.contains('\0') {
        Err(SnapshotError::Corrupt(Corruption::InvalidPayload(
            "names must be non-empty and contain no NUL",
        )))
    } else {
        Ok(())
    }
}

fn ensure_unique_corrupt<'a>(
    names: impl Iterator<Item = &'a str>,
    reason: &'static str,
) -> Result<(), SnapshotError> {
    let names = names;
    let mut seen = HashSet::new();
    seen.try_reserve(names.size_hint().0)
        .map_err(|_| SnapshotError::AllocationFailed)?;
    for name in names {
        if !seen.insert(name) {
            return Err(SnapshotError::Corrupt(Corruption::InvalidPayload(reason)));
        }
    }
    Ok(())
}

fn read_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes(bytes.try_into().expect("fixed-size header field"))
}

fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().expect("fixed-size header field"))
}

fn read_u64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes.try_into().expect("fixed-size header field"))
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Failpoint {
    TempSynced,
    SecurityPrepared,
    Renamed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicU64, Ordering},
    };

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let id = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rusthouse-catalog-test-{}-{id}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("create isolated test directory");
            Self(path)
        }

        fn snapshot(&self) -> PathBuf {
            self.0.join("catalog.rhcat")
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).expect("remove isolated test directory");
        }
    }

    fn sample_image(generation: u64) -> CatalogImage {
        let columns = vec![
            ColumnImage::new(
                "signed",
                ColumnData::Int64(vec![Some(i64::MIN), None, Some(generation as i64)]),
            )
            .expect("valid integer column"),
            ColumnImage::new(
                "measure",
                ColumnData::Float64(vec![Some(-0.0), Some(1.25), None]),
            )
            .expect("valid float column"),
            ColumnImage::new(
                "enabled",
                ColumnData::Bool(vec![Some(true), Some(false), None]),
            )
            .expect("valid boolean column"),
            ColumnImage::new(
                "label",
                ColumnData::String(vec![
                    Some(format!("generation-{generation}")),
                    None,
                    Some(String::new()),
                ]),
            )
            .expect("valid string column"),
        ];
        let events = TableImage::new("events", columns).expect("valid table");
        let schema = SchemaImage::new("analytics", vec![events]).expect("valid schema");
        CatalogImage::new(generation, vec![schema]).expect("valid catalog")
    }

    fn refresh_checksums(bytes: &mut [u8]) {
        let payload_checksum = crc32(&bytes[HEADER_LEN..]);
        bytes[24..28].copy_from_slice(&payload_checksum.to_le_bytes());
        let header_checksum = crc32(&bytes[..28]);
        bytes[28..32].copy_from_slice(&header_checksum.to_le_bytes());
    }

    #[cfg(target_os = "macos")]
    fn install_inheritable_test_acl(path: &Path) {
        use exacl::{AclEntry, Flag, Perm};

        let acl = [AclEntry::allow_user(
            "nobody",
            Perm::READ | Perm::EXECUTE,
            Flag::FILE_INHERIT | Flag::DIRECTORY_INHERIT,
        )];
        exacl::setfacl(&[path], &acl, None).expect("install inheritable parent ACL");
    }

    #[cfg(target_os = "linux")]
    fn install_inheritable_test_acl(path: &Path) {
        use exacl::{AclEntry, Flag, Perm};

        let mut acl = exacl::getfacl(path, None).expect("read parent ACL");
        acl.extend([
            AclEntry::allow_user("", Perm::READ | Perm::WRITE | Perm::EXECUTE, Flag::DEFAULT),
            AclEntry::allow_user("nobody", Perm::READ | Perm::EXECUTE, Flag::DEFAULT),
            AclEntry::allow_group("", Perm::empty(), Flag::DEFAULT),
            AclEntry::allow_mask(Perm::READ | Perm::EXECUTE, Flag::DEFAULT),
            AclEntry::allow_other(Perm::empty(), Flag::DEFAULT),
        ]);
        exacl::setfacl(&[path], &acl, None).expect("install inheritable parent ACL");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn acl_allows_nobody(path: &Path) -> bool {
        exacl::getfacl(path, None)
            .expect("read ACL")
            .iter()
            .any(|entry| entry.name.ends_with("nobody") && entry.perms.contains(exacl::Perm::READ))
    }

    #[test]
    fn typed_image_validation_rejects_bad_shapes_and_names() {
        let left =
            ColumnImage::new("value", ColumnData::Int64(vec![Some(1)])).expect("valid column");
        let short = ColumnImage::new("other", ColumnData::Bool(Vec::new())).expect("valid column");
        assert!(matches!(
            TableImage::new("bad", vec![left.clone(), short]),
            Err(SnapshotError::InvalidImage(_))
        ));
        assert!(matches!(
            TableImage::new("bad", vec![left.clone(), left]),
            Err(SnapshotError::InvalidImage(_))
        ));
        assert!(matches!(
            ColumnImage::new("", ColumnData::Bool(Vec::new())),
            Err(SnapshotError::InvalidImage(_))
        ));
    }

    #[test]
    fn commits_and_reopens_every_column_type() {
        let directory = TestDirectory::new();
        let path = directory.snapshot();
        let image = sample_image(42);

        let store = SnapshotStore::open(&path).expect("open new store");
        assert_eq!(store.load().expect("load empty store"), None);
        store.commit(&image).expect("commit image");
        assert_eq!(
            store.load().expect("load committed image"),
            Some(image.clone())
        );
        drop(store);

        let reopened = SnapshotStore::open(&path).expect("reopen store");
        assert_eq!(reopened.load().expect("load reopened image"), Some(image));
    }

    #[test]
    fn excludes_a_second_writer_until_the_first_is_dropped() {
        let directory = TestDirectory::new();
        let path = directory.snapshot();
        let first = SnapshotStore::open(&path).expect("open first writer");

        assert!(matches!(
            SnapshotStore::open(&path),
            Err(SnapshotError::Locked(locked_path)) if locked_path == first.path()
        ));
        drop(first);
        SnapshotStore::open(&path).expect("lock released with store");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_final_component_symlinks_without_touching_the_target() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new();
        let target = directory.0.join("target.rhcat");
        let link = directory.0.join("link.rhcat");
        let image = sample_image(4);
        let target_store = SnapshotStore::open(&target).expect("open target store");
        target_store.commit(&image).expect("commit target image");
        drop(target_store);
        symlink(&target, &link).expect("create snapshot symlink");

        assert!(matches!(
            SnapshotStore::open(&link),
            Err(SnapshotError::SymlinkSnapshot(_))
        ));
        assert!(!directory.0.join(".link.rhcat.lock").exists());
        let reopened = SnapshotStore::open(&target).expect("reopen target store");
        assert_eq!(reopened.load().expect("load target image"), Some(image));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn rejects_final_component_symlink_substitution_after_open() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new();
        let path = directory.snapshot();
        let outside_path = directory.0.join("outside.rhcat");
        let original = sample_image(5);
        let outside = sample_image(6);
        let store = SnapshotStore::open(&path).expect("open protected store");
        store.commit(&original).expect("commit protected image");
        let outside_store = SnapshotStore::open(&outside_path).expect("open outside store");
        outside_store
            .commit(&outside)
            .expect("commit outside image");
        drop(outside_store);

        fs::remove_file(&path).expect("remove protected snapshot");
        symlink(&outside_path, &path).expect("substitute final component");
        assert!(matches!(
            store.load(),
            Err(SnapshotError::SymlinkSnapshot(_))
        ));
        assert!(matches!(
            store.commit(&sample_image(7)),
            Err(SnapshotError::SymlinkSnapshot(_))
        ));
        assert!(!store.temp_dir.exists());

        let outside_store = SnapshotStore::open(&outside_path).expect("reopen outside store");
        assert_eq!(
            outside_store.load().expect("load outside image"),
            Some(outside)
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn parent_directory_replacement_keeps_writers_and_staging_isolated() {
        let directory = TestDirectory::new();
        let active_parent = directory.0.join("active");
        let moved_parent = directory.0.join("moved");
        fs::create_dir(&active_parent).expect("create original parent");
        let path = active_parent.join("catalog.rhcat");
        let moved_path = moved_parent.join("catalog.rhcat");
        let first = Arc::new(SnapshotStore::open(&path).expect("open original writer"));

        fs::rename(&active_parent, &moved_parent).expect("move original parent");
        assert!(matches!(
            SnapshotStore::open(&moved_path),
            Err(SnapshotError::Locked(_))
        ));
        fs::create_dir(&active_parent).expect("create replacement parent");
        let second = Arc::new(SnapshotStore::open(&path).expect("open replacement writer"));

        let first_image = sample_image(50);
        let second_image = sample_image(51);
        let barrier = Arc::new(Barrier::new(3));
        let first_thread = {
            let store = Arc::clone(&first);
            let barrier = Arc::clone(&barrier);
            let image = first_image.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store.commit(&image)
            })
        };
        let second_thread = {
            let store = Arc::clone(&second);
            let barrier = Arc::clone(&barrier);
            let image = second_image.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store.commit(&image)
            })
        };
        barrier.wait();
        first_thread
            .join()
            .expect("original writer thread")
            .expect("commit through pinned original parent");
        second_thread
            .join()
            .expect("replacement writer thread")
            .expect("commit through replacement parent");

        assert_eq!(
            first.load().expect("load original object"),
            Some(first_image)
        );
        assert_eq!(
            second.load().expect("load replacement object"),
            Some(second_image)
        );
        assert!(!moved_parent.join(".catalog.rhcat.tmp").exists());
        assert!(!active_parent.join(".catalog.rhcat.tmp").exists());
    }

    #[cfg(target_os = "freebsd")]
    #[test]
    fn freebsd_is_rejected_before_sidecar_creation() {
        let directory = TestDirectory::new();
        let path = directory.snapshot();
        assert!(matches!(
            SnapshotStore::open(&path),
            Err(SnapshotError::UnsupportedPlatform(_))
        ));
        assert!(!directory.0.join(".catalog.rhcat.lock").exists());
    }

    #[test]
    fn rejects_snapshot_names_that_overlap_live_sidecars() {
        let directory = TestDirectory::new();
        let path = directory.0.join("catalog");
        let original = sample_image(3);
        let store = SnapshotStore::open(&path).expect("open protected store");
        store.commit(&original).expect("commit protected snapshot");

        for reserved in [
            ".catalog.lock",
            ".catalog.tmp",
            ".CATALOG.LOCK",
            ".catalog.tmp.",
            ".catalog.lock ",
        ] {
            let reserved_path = directory.0.join(reserved);
            assert!(matches!(
                SnapshotStore::open(&reserved_path),
                Err(SnapshotError::ReservedSnapshotName(_))
            ));
        }

        assert_eq!(
            store.load().expect("load protected snapshot"),
            Some(original)
        );
        assert!(matches!(
            SnapshotStore::open(&path),
            Err(SnapshotError::Locked(_))
        ));
    }

    #[cfg(windows)]
    #[test]
    fn windows_write_through_publish_replaces_and_reopens_snapshot() {
        let directory = TestDirectory::new();
        let path = directory.snapshot();
        let store = SnapshotStore::open(&path).expect("open Windows store");
        store.commit(&sample_image(1)).expect("publish first image");
        let replacement = sample_image(2);
        store
            .commit(&replacement)
            .expect("write-through replacement succeeds");
        assert_eq!(
            store.load().expect("load replacement"),
            Some(replacement.clone())
        );
        drop(store);

        let reopened = SnapshotStore::open(&path).expect("reopen Windows store");
        assert_eq!(
            reopened.load().expect("load after reopen"),
            Some(replacement)
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_rejects_trailing_aliases_before_lock_creation() {
        let directory = TestDirectory::new();
        let path = directory.snapshot();
        let store = SnapshotStore::open(&path).expect("open canonical Windows writer");
        store
            .commit(&sample_image(3))
            .expect("create canonical snapshot");

        for (alias, lock_name) in [
            ("catalog.rhcat.", ".catalog.rhcat..lock"),
            ("catalog.rhcat ", ".catalog.rhcat .lock"),
        ] {
            assert!(matches!(
                SnapshotStore::open(directory.0.join(alias)),
                Err(SnapshotError::ReservedSnapshotName(_))
            ));
            assert!(!directory.0.join(lock_name).exists());
        }
        assert_eq!(
            store.load().expect("canonical snapshot remains readable"),
            Some(sample_image(3))
        );
    }

    #[test]
    fn serializes_threads_sharing_one_writer_handle() {
        let directory = TestDirectory::new();
        let path = directory.snapshot();
        let store = Arc::new(SnapshotStore::open(&path).expect("open shared writer"));
        let barrier = Arc::new(Barrier::new(5));
        let mut writers = Vec::new();

        for generation in 1..=4 {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            writers.push(std::thread::spawn(move || {
                let image = sample_image(generation);
                barrier.wait();
                store.commit(&image)
            }));
        }
        barrier.wait();
        for writer in writers {
            writer
                .join()
                .expect("writer thread did not panic")
                .expect("serialized commit succeeds");
        }

        let loaded = store
            .load()
            .expect("load final complete image")
            .expect("at least one commit");
        assert_eq!(loaded, sample_image(loaded.generation()));
    }

    #[test]
    fn removes_an_orphan_temp_only_after_acquiring_the_lock() {
        let directory = TestDirectory::new();
        let path = directory.snapshot();
        let temp_path = directory.0.join(".catalog.rhcat.tmp");
        fs::write(&temp_path, b"partial commit").expect("create orphan temp");

        let store = SnapshotStore::open(&path).expect("open store and recover");
        assert!(!temp_path.exists());
        assert_eq!(store.load().expect("load new store"), None);
    }

    #[test]
    fn rejects_bad_magic_checksums_versions_lengths_and_trailing_bytes() {
        let limits = SnapshotLimits::default();
        let original = encode_snapshot(&sample_image(7), &limits).expect("encode valid image");

        let mut bad_magic = original.clone();
        bad_magic[0] ^= 1;
        assert!(matches!(
            decode_snapshot(&bad_magic, &limits),
            Err(SnapshotError::Corrupt(Corruption::BadMagic))
        ));

        let mut bad_header = original.clone();
        bad_header[8] ^= 1;
        assert!(matches!(
            decode_snapshot(&bad_header, &limits),
            Err(SnapshotError::Corrupt(Corruption::BadHeaderChecksum))
        ));

        let mut unknown_version = original.clone();
        unknown_version[8..10].copy_from_slice(&2u16.to_le_bytes());
        let checksum = crc32(&unknown_version[..28]);
        unknown_version[28..32].copy_from_slice(&checksum.to_le_bytes());
        assert!(matches!(
            decode_snapshot(&unknown_version, &limits),
            Err(SnapshotError::UnsupportedVersion(2))
        ));

        let mut bad_payload = original.clone();
        *bad_payload.last_mut().expect("nonempty encoded image") ^= 1;
        assert!(matches!(
            decode_snapshot(&bad_payload, &limits),
            Err(SnapshotError::Corrupt(Corruption::BadPayloadChecksum))
        ));

        let mut truncated = original.clone();
        truncated.pop();
        assert!(matches!(
            decode_snapshot(&truncated, &limits),
            Err(SnapshotError::Corrupt(Corruption::Truncated))
        ));

        let mut trailing = original;
        trailing.push(0);
        assert!(matches!(
            decode_snapshot(&trailing, &limits),
            Err(SnapshotError::Corrupt(Corruption::TrailingData))
        ));
    }

    #[test]
    fn load_rejects_corruption_from_disk() {
        let directory = TestDirectory::new();
        let path = directory.snapshot();
        let store = SnapshotStore::open(&path).expect("open store");
        store.commit(&sample_image(1)).expect("commit image");
        let mut bytes = fs::read(&path).expect("read committed snapshot");
        *bytes.last_mut().expect("nonempty snapshot") ^= 1;
        fs::write(&path, bytes).expect("replace with corrupt snapshot");

        assert!(matches!(
            store.load(),
            Err(SnapshotError::Corrupt(Corruption::BadPayloadChecksum))
        ));
    }

    #[test]
    fn malicious_counts_are_rejected_before_allocation() {
        let limits = SnapshotLimits::default();
        let mut bytes = encode_snapshot(&CatalogImage::empty(), &limits).expect("encode empty");
        bytes[HEADER_LEN + 8..HEADER_LEN + 12].copy_from_slice(&u32::MAX.to_le_bytes());
        refresh_checksums(&mut bytes);

        assert!(matches!(
            decode_snapshot(&bytes, &limits),
            Err(SnapshotError::LimitExceeded {
                resource: "schemas",
                actual,
                ..
            }) if actual == u64::from(u32::MAX)
        ));

        let encoded = encode_snapshot(&sample_image(1), &limits).expect("encode strings");
        let string_limited = SnapshotLimits {
            max_total_string_bytes: 1,
            ..limits
        };
        assert!(matches!(
            decode_snapshot(&encoded, &string_limited),
            Err(SnapshotError::LimitExceeded {
                resource: "total string bytes",
                ..
            })
        ));
    }

    #[test]
    fn rejects_nonempty_tables_without_columns() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&0u64.to_le_bytes());
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.extend_from_slice(b"s");
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.extend_from_slice(b"t");
        payload.extend_from_slice(&1u64.to_le_bytes());
        payload.extend_from_slice(&0u32.to_le_bytes());

        assert!(matches!(
            decode_payload(&payload, &SnapshotLimits::default()),
            Err(SnapshotError::Corrupt(Corruption::InvalidPayload(
                "a table without columns must have zero rows"
            )))
        ));
    }

    #[test]
    fn file_and_image_limits_preserve_the_existing_snapshot() {
        let directory = TestDirectory::new();
        let path = directory.snapshot();
        let old = sample_image(1);
        let store = SnapshotStore::open(&path).expect("open store");
        store.commit(&old).expect("commit original image");
        drop(store);

        let limits = SnapshotLimits {
            max_rows_per_table: 2,
            ..SnapshotLimits::default()
        };
        let bounded = SnapshotStore::open_with_limits(&path, limits).expect("open bounded store");
        assert!(matches!(
            bounded.commit(&sample_image(2)),
            Err(SnapshotError::LimitExceeded {
                resource: "rows per table",
                ..
            })
        ));
        drop(bounded);

        let reopened = SnapshotStore::open(&path).expect("reopen with default limits");
        assert_eq!(reopened.load().expect("load original image"), Some(old));
        drop(reopened);

        let limits = SnapshotLimits {
            max_snapshot_bytes: HEADER_LEN as u64,
            ..SnapshotLimits::default()
        };
        let tiny = SnapshotStore::open_with_limits(&path, limits).expect("open tiny store");
        assert!(matches!(
            tiny.load(),
            Err(SnapshotError::LimitExceeded {
                resource: "snapshot bytes",
                ..
            })
        ));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn preserves_unix_security_metadata_and_keeps_failed_staging_private() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let directory = TestDirectory::new();
        let path = directory.snapshot();
        let original = sample_image(30);
        let replacement = sample_image(31);
        let store = SnapshotStore::open(&path).expect("open store");
        store.commit(&original).expect("commit original image");

        let original_gid = fs::metadata(&path).expect("initial metadata").gid();
        if let Some(group) = rustix::process::getgroups()
            .expect("read supplementary groups")
            .into_iter()
            .find(|group| group.as_raw() != original_gid)
        {
            rustix::fs::chown(&path, None, Some(group)).expect("set non-default group");
        }

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            #[cfg(target_os = "linux")]
            use exacl::AclEntryKind;
            use exacl::{AclEntry, Perm};

            let mut acl = exacl::getfacl(&path, None).expect("read original ACL");
            acl.push(AclEntry::allow_user("nobody", Perm::READ, None));
            #[cfg(target_os = "linux")]
            if let Some(mask) = acl
                .iter_mut()
                .find(|entry| entry.kind == AclEntryKind::Mask)
            {
                mask.perms |= Perm::READ;
            } else {
                acl.push(AclEntry::allow_mask(Perm::READ, None));
            }
            exacl::setfacl(&[&path], &acl, None).expect("install named ACL");
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640))
            .expect("set non-default snapshot mode");
        let expected_metadata = fs::metadata(&path).expect("original metadata");
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let expected_acl = exacl::getfacl(&path, None).expect("read expected ACL");

        store
            .commit(&replacement)
            .expect("replace restricted image");
        let replaced_metadata = fs::metadata(&path).expect("replacement metadata");
        assert_eq!(replaced_metadata.mode() & 0o777, 0o640);
        assert_eq!(replaced_metadata.uid(), expected_metadata.uid());
        assert_eq!(replaced_metadata.gid(), expected_metadata.gid());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        assert_eq!(
            exacl::getfacl(&path, None).expect("read replacement ACL"),
            expected_acl
        );

        assert!(matches!(
            store.commit_inner(&sample_image(32), Some(Failpoint::SecurityPrepared)),
            Err(SnapshotError::InjectedFailure("after security preparation"))
        ));
        assert_eq!(
            fs::metadata(&store.temp_dir)
                .expect("private staging directory metadata")
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&store.temp_path)
                .expect("prepared staged file metadata")
                .mode()
                & 0o777,
            0o640
        );
        assert_eq!(
            store.load().expect("load surviving image"),
            Some(replacement)
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn inherited_parent_acl_does_not_reach_staging_or_first_snapshot() {
        let directory = TestDirectory::new();
        install_inheritable_test_acl(&directory.0);
        let probe = directory.0.join("probe");
        fs::write(&probe, b"probe").expect("create ACL inheritance probe");
        assert!(acl_allows_nobody(&probe));
        fs::remove_file(probe).expect("remove inheritance probe");

        let path = directory.snapshot();
        let store = SnapshotStore::open(&path).expect("open store under inherited ACL");
        assert!(matches!(
            store.commit_inner(&sample_image(40), Some(Failpoint::TempSynced)),
            Err(SnapshotError::InjectedFailure("after temp sync"))
        ));
        assert!(!acl_allows_nobody(&store.temp_dir));
        assert!(!acl_allows_nobody(&store.temp_path));
        drop(store);

        let recovered = SnapshotStore::open(&path).expect("clean interrupted staging");
        let image = sample_image(41);
        recovered.commit(&image).expect("publish first snapshot");
        assert!(!acl_allows_nobody(&path));
        assert_eq!(recovered.load().expect("load first snapshot"), Some(image));
    }

    #[test]
    fn failure_after_temp_sync_leaves_previous_snapshot_and_cleans_orphan() {
        let directory = TestDirectory::new();
        let path = directory.snapshot();
        let old = sample_image(10);
        let new = sample_image(11);
        let store = SnapshotStore::open(&path).expect("open store");
        store.commit(&old).expect("commit original image");

        assert!(matches!(
            store.commit_inner(&new, Some(Failpoint::TempSynced)),
            Err(SnapshotError::InjectedFailure("after temp sync"))
        ));
        assert!(store.temp_path.exists());
        assert_eq!(
            store.load().expect("load previous image"),
            Some(old.clone())
        );
        let temp_path = store.temp_path.clone();
        drop(store);

        let recovered = SnapshotStore::open(&path).expect("reopen after interrupted commit");
        assert!(!temp_path.exists());
        assert_eq!(recovered.load().expect("load recovered image"), Some(old));
    }

    #[test]
    fn failure_after_rename_still_reopens_a_complete_new_snapshot() {
        let directory = TestDirectory::new();
        let path = directory.snapshot();
        let store = SnapshotStore::open(&path).expect("open store");
        store
            .commit(&sample_image(20))
            .expect("commit original image");
        let new = sample_image(21);

        assert!(matches!(
            store.commit_inner(&new, Some(Failpoint::Renamed)),
            Err(SnapshotError::InjectedFailure("after rename"))
        ));
        assert_eq!(store.load().expect("load renamed image"), Some(new.clone()));
        drop(store);

        let recovered = SnapshotStore::open(&path).expect("reopen renamed image");
        assert_eq!(recovered.load().expect("load complete image"), Some(new));
    }

    #[test]
    fn crc32_matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }
}
