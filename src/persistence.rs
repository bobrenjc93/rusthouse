use std::collections::BTreeMap;
#[cfg(not(unix))]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;
#[cfg(all(test, unix))]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::catalog::CatalogGeneration;
use crate::error::{Error, Result};
#[cfg(unix)]
use crate::sidecar::lock_name;
#[cfg(windows)]
use crate::sidecar::open_parent_directory_guard;
use crate::sidecar::{TEMP_PREFIX, is_reserved_name, lock_path};
use crate::storage::{ColumnData, ColumnDef, DataType, EngineTable as Table};

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
#[cfg(all(test, unix))]
static FAIL_DIRECTORY_SYNC: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static REPLACE_LOCK_BEFORE_ACQUIRE: Mutex<Option<PathBuf>> = Mutex::new(None);

#[derive(Debug)]
pub(crate) enum StoreStatus {
    Durable,
    #[cfg(any(unix, windows))]
    PublishedWithError(Error),
    #[cfg(windows)]
    RecoveryRequired(Error),
}

#[cfg(all(test, unix))]
pub(crate) fn fail_next_directory_sync() {
    FAIL_DIRECTORY_SYNC.store(true, Ordering::SeqCst);
}

#[derive(Debug)]
pub(crate) struct Persistence {
    path: PathBuf,
    #[cfg(unix)]
    parent_dir: File,
    #[cfg(unix)]
    file_name: std::ffi::OsString,
    #[cfg(windows)]
    _parent_guard: File,
    _lock: File,
}

impl Persistence {
    pub(crate) fn acquire(path: PathBuf) -> Result<Self> {
        let path = normalize_path(&path)?;
        if path.file_name().is_some_and(is_reserved_name) {
            return Err(Error::ReservedDatabasePath(path.display().to_string()));
        }
        let lock_path = lock_path(&path);
        #[cfg(unix)]
        let parent_dir = File::open(path.parent().expect("normalized path has a parent"))
            .map_err(|error| Error::io("open database directory", error))?;
        #[cfg(windows)]
        let parent_guard =
            open_parent_directory_guard(path.parent().expect("normalized path has a parent"))
                .map_err(|error| Error::io("open database directory", error))?;
        #[cfg(unix)]
        let file_name = path
            .file_name()
            .expect("normalized path has a filename")
            .to_owned();
        #[cfg(unix)]
        let lock = {
            let lock_name = lock_name(&file_name);
            open_database_lock_at(&parent_dir, &lock_name, &lock_path, &path)?
        };
        #[cfg(not(unix))]
        let lock = open_database_lock(&lock_path, &path)?;
        Ok(Self {
            path,
            #[cfg(unix)]
            parent_dir,
            #[cfg(unix)]
            file_name,
            #[cfg(windows)]
            _parent_guard: parent_guard,
            _lock: lock,
        })
    }

    pub(crate) fn load(&self) -> Result<CatalogGeneration> {
        #[cfg(unix)]
        let file = open_snapshot_at(&self.parent_dir, &self.file_name, &self.path)?;
        #[cfg(not(unix))]
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(CatalogGeneration::empty());
            }
            Err(error) => return Err(Error::io("open snapshot", error)),
        };
        #[cfg(unix)]
        let mut file = match file {
            Some(file) => file,
            None => return Ok(CatalogGeneration::empty()),
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

    pub(crate) fn store(&self, generation: &CatalogGeneration) -> Result<StoreStatus> {
        let bytes = encode_snapshot(generation)?;
        #[cfg(unix)]
        {
            let temporary_name = next_temporary_snapshot_name();
            let result = write_and_replace_at(
                &self.parent_dir,
                &temporary_name,
                &self.file_name,
                &self.path,
                &bytes,
            );
            if result.is_err() {
                let _ = rustix::fs::unlinkat(
                    &self.parent_dir,
                    &temporary_name,
                    rustix::fs::AtFlags::empty(),
                );
            }
            result
        }
        #[cfg(not(unix))]
        {
            let parent = self
                .path
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));

            let temporary = next_temporary_snapshot_path(parent);

            let result = write_and_replace(&temporary, &self.path, parent, &bytes);
            if result.is_err() {
                let _ = fs::remove_file(&temporary);
            }
            result
        }
    }
}

#[cfg(not(unix))]
fn open_database_lock(path: &Path, database_path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error)
            if fs::symlink_metadata(path)
                .is_ok_and(|metadata| metadata.file_type().is_symlink()) =>
        {
            return Err(Error::UnsafeLockPath(path.display().to_string()));
        }
        Err(error) => return Err(Error::io("open database lock", error)),
    };
    let metadata = file
        .metadata()
        .map_err(|error| Error::io("inspect database lock", error))?;

    #[cfg(test)]
    {
        let mut replacement = REPLACE_LOCK_BEFORE_ACQUIRE
            .lock()
            .expect("lock replacement test hook must not be poisoned");
        if replacement.as_deref() == Some(path) {
            *replacement = None;
            fs::remove_file(path).map_err(|error| Error::io("inject lock replacement", error))?;
            File::create(path).map_err(|error| Error::io("inject lock replacement", error))?;
        }
    }

    match fs4::FileExt::try_lock(&file) {
        Ok(()) => {}
        Err(fs4::TryLockError::WouldBlock) => {
            return Err(Error::DatabaseAlreadyOpen(
                database_path.display().to_string(),
            ));
        }
        Err(fs4::TryLockError::Error(error)) => {
            return Err(Error::io("lock database", error));
        }
    }

    let path_metadata = fs::symlink_metadata(path)
        .map_err(|error| Error::io("inspect database lock path", error))?;
    if !metadata.is_file() || !path_metadata.is_file() || path_metadata.file_type().is_symlink() {
        return Err(Error::UnsafeLockPath(path.display().to_string()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.dev() != path_metadata.dev() || metadata.ino() != path_metadata.ino() {
            return Err(Error::UnsafeLockPath(path.display().to_string()));
        }
    }
    #[cfg(windows)]
    {
        if !crate::sidecar::same_file(&file, path)
            .map_err(|error| Error::io("inspect database lock identity", error))?
        {
            return Err(Error::UnsafeLockPath(path.display().to_string()));
        }
    }
    Ok(file)
}

#[cfg(unix)]
fn open_database_lock_at(
    parent_dir: &File,
    lock_name: &std::ffi::OsStr,
    lock_path: &Path,
    database_path: &Path,
) -> Result<File> {
    use rustix::fs::{Mode, OFlags};
    use std::os::unix::fs::MetadataExt;

    let descriptor = match rustix::fs::openat(
        parent_dir,
        lock_name,
        OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::RUSR | Mode::WUSR,
    ) {
        Ok(descriptor) => descriptor,
        Err(error) if error == rustix::io::Errno::LOOP => {
            return Err(Error::UnsafeLockPath(lock_path.display().to_string()));
        }
        Err(error) => return Err(rustix_error("open database lock", error)),
    };
    let file = File::from(descriptor);
    let metadata = file
        .metadata()
        .map_err(|error| Error::io("inspect database lock", error))?;
    if !metadata.is_file() {
        return Err(Error::UnsafeLockPath(lock_path.display().to_string()));
    }

    #[cfg(test)]
    {
        let mut replacement = REPLACE_LOCK_BEFORE_ACQUIRE
            .lock()
            .expect("lock replacement test hook must not be poisoned");
        if replacement.as_deref() == Some(lock_path) {
            *replacement = None;
            fs::remove_file(lock_path)
                .map_err(|error| Error::io("inject lock replacement", error))?;
            File::create(lock_path).map_err(|error| Error::io("inject lock replacement", error))?;
        }
    }

    match fs4::FileExt::try_lock(&file) {
        Ok(()) => {}
        Err(fs4::TryLockError::WouldBlock) => {
            return Err(Error::DatabaseAlreadyOpen(
                database_path.display().to_string(),
            ));
        }
        Err(fs4::TryLockError::Error(error)) => {
            return Err(Error::io("lock database", error));
        }
    }

    let current = rustix::fs::openat(
        parent_dir,
        lock_name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|_| Error::UnsafeLockPath(lock_path.display().to_string()))?;
    let current_metadata = current
        .metadata()
        .map_err(|error| Error::io("inspect database lock path", error))?;
    if !metadata.is_file()
        || !current_metadata.is_file()
        || metadata.dev() != current_metadata.dev()
        || metadata.ino() != current_metadata.ino()
    {
        return Err(Error::UnsafeLockPath(lock_path.display().to_string()));
    }
    Ok(file)
}

#[cfg(unix)]
fn open_snapshot_at(
    parent_dir: &File,
    file_name: &std::ffi::OsStr,
    display_path: &Path,
) -> Result<Option<File>> {
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
            return Err(Error::CorruptSnapshot(format!(
                "snapshot path is a symbolic link: {}",
                display_path.display()
            )));
        }
        Err(error) => return Err(rustix_error("open snapshot", error)),
    };
    let file = File::from(descriptor);
    if !file
        .metadata()
        .map_err(|error| Error::io("inspect snapshot", error))?
        .is_file()
    {
        return Err(Error::CorruptSnapshot(
            "snapshot path is not a regular file".to_owned(),
        ));
    }
    Ok(Some(file))
}

#[cfg(unix)]
fn rustix_error(operation: &'static str, error: rustix::io::Errno) -> Error {
    Error::io(
        operation,
        std::io::Error::from_raw_os_error(error.raw_os_error()),
    )
}

fn next_temporary_snapshot_name() -> std::ffi::OsString {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{TEMP_PREFIX}{}.{}", std::process::id(), sequence).into()
}

#[cfg(any(test, not(unix)))]
fn next_temporary_snapshot_path(parent: &Path) -> PathBuf {
    parent.join(next_temporary_snapshot_name())
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
    let parent =
        fs::canonicalize(parent).map_err(|error| Error::io("resolve database directory", error))?;
    Ok(parent.join(file_name))
}

#[cfg(not(unix))]
fn write_and_replace(
    temporary: &Path,
    destination: &Path,
    parent: &Path,
    bytes: &[u8],
) -> Result<StoreStatus> {
    let destination_metadata = match fs::metadata(destination) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(Error::io("inspect snapshot permissions", error)),
    };
    let destination_permissions = destination_metadata.as_ref().map(fs::Metadata::permissions);
    #[cfg(unix)]
    let destination_ownership = destination_metadata.as_ref().map(|metadata| {
        use std::os::unix::fs::MetadataExt;
        (metadata.uid(), metadata.gid())
    });
    #[cfg(any(target_os = "freebsd", target_os = "linux", target_os = "macos"))]
    let destination_acl = if destination_metadata.is_some() {
        match exacl::getfacl(destination, None) {
            Ok(acl) => Some(acl),
            Err(error) if error.kind() == std::io::ErrorKind::Unsupported => None,
            Err(error) => return Err(Error::io("inspect snapshot ACL", error)),
        }
    } else {
        None
    };
    let mut file = crate::catalog::create_secure_temp(temporary).map_err(|error| Error::Io {
        operation: "create private temporary snapshot",
        message: error.to_string(),
    })?;
    file.write_all(bytes)
        .map_err(|error| Error::io("write temporary snapshot", error))?;
    file.sync_all()
        .map_err(|error| Error::io("sync temporary snapshot", error))?;
    #[cfg(unix)]
    if let Some((uid, gid)) = destination_ownership {
        preserve_snapshot_ownership(&file, uid, gid)?;
    }
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
    drop(file);
    replace_snapshot(temporary, destination, parent)
}

#[cfg(unix)]
fn write_and_replace_at(
    parent_dir: &File,
    temporary_name: &std::ffi::OsStr,
    destination_name: &std::ffi::OsStr,
    display_path: &Path,
    bytes: &[u8],
) -> Result<StoreStatus> {
    use rustix::fs::{Mode, OFlags};

    let existing = open_snapshot_at(parent_dir, destination_name, display_path)?;
    let descriptor = rustix::fs::openat(
        parent_dir,
        temporary_name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(|error| rustix_error("create private temporary snapshot", error))?;
    let mut file = File::from(descriptor);
    crate::catalog::protect_temp_security(&file).map_err(|error| Error::Io {
        operation: "protect temporary snapshot",
        message: error.to_string(),
    })?;
    file.write_all(bytes)
        .map_err(|error| Error::io("write temporary snapshot", error))?;
    file.sync_all()
        .map_err(|error| Error::io("sync temporary snapshot", error))?;
    crate::catalog::prepare_temp_security(&file, existing.as_ref()).map_err(|error| Error::Io {
        operation: "preserve snapshot security metadata",
        message: error.to_string(),
    })?;
    file.sync_all()
        .map_err(|error| Error::io("sync snapshot permissions", error))?;
    drop(file);

    rustix::fs::renameat(parent_dir, temporary_name, parent_dir, destination_name)
        .map_err(|error| rustix_error("replace snapshot", error))?;
    #[cfg(test)]
    if FAIL_DIRECTORY_SYNC.swap(false, Ordering::SeqCst) {
        return Ok(StoreStatus::PublishedWithError(Error::Io {
            operation: "sync snapshot directory",
            message: "injected directory sync failure".to_owned(),
        }));
    }
    match parent_dir.sync_all() {
        Ok(()) => Ok(StoreStatus::Durable),
        Err(error) => Ok(StoreStatus::PublishedWithError(Error::io(
            "sync snapshot directory",
            error,
        ))),
    }
}

#[cfg(windows)]
fn replace_snapshot(temporary: &Path, destination: &Path, _parent: &Path) -> Result<StoreStatus> {
    windows::replace_snapshot(temporary, destination)
}

#[cfg(not(any(unix, windows)))]
fn replace_snapshot(temporary: &Path, destination: &Path, _parent: &Path) -> Result<StoreStatus> {
    fs::rename(temporary, destination).map_err(|error| Error::io("replace snapshot", error))?;
    Ok(StoreStatus::Durable)
}

#[cfg(any(test, windows))]
#[derive(Debug)]
enum WindowsRecoveryOutcome {
    OriginalRestored,
    CandidatePublished {
        restore_error: std::io::Error,
    },
    ManualRecoveryRequired {
        restore_error: std::io::Error,
        publish_error: std::io::Error,
    },
}

#[cfg(any(test, windows))]
const WINDOWS_ERROR_UNABLE_TO_MOVE_REPLACEMENT: i32 = 1176;
#[cfg(any(test, windows))]
const WINDOWS_ERROR_UNABLE_TO_MOVE_REPLACEMENT_2: i32 = 1177;

#[cfg(any(test, windows))]
fn windows_replacement_relocated_original(error_code: i32) -> bool {
    match error_code {
        WINDOWS_ERROR_UNABLE_TO_MOVE_REPLACEMENT => false,
        WINDOWS_ERROR_UNABLE_TO_MOVE_REPLACEMENT_2 => true,
        _ => false,
    }
}

#[cfg(any(test, windows))]
fn windows_existing_replacement_status() -> StoreStatus {
    StoreStatus::PublishedWithError(Error::Io {
        operation: "durably replace Windows snapshot",
        message: "ReplaceFileW published the snapshot, but Windows provides no supported write-through metadata barrier"
            .to_owned(),
    })
}

#[cfg(any(test, windows))]
fn recover_relocated_windows_snapshot<R, P>(restore: R, publish: P) -> WindowsRecoveryOutcome
where
    R: FnOnce() -> std::io::Result<()>,
    P: FnOnce() -> std::io::Result<()>,
{
    match restore() {
        Ok(()) => WindowsRecoveryOutcome::OriginalRestored,
        Err(restore_error) => match publish() {
            Ok(()) => WindowsRecoveryOutcome::CandidatePublished { restore_error },
            Err(publish_error) => WindowsRecoveryOutcome::ManualRecoveryRequired {
                restore_error,
                publish_error,
            },
        },
    }
}

#[cfg(windows)]
mod windows {
    use std::ffi::c_void;
    use std::fs;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::ptr;

    use super::{
        StoreStatus, WindowsRecoveryOutcome, recover_relocated_windows_snapshot,
        windows_existing_replacement_status, windows_replacement_relocated_original,
    };
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

    pub(super) fn replace_snapshot(
        temporary_path: &Path,
        destination_path: &Path,
    ) -> Result<StoreStatus> {
        let backup_path = backup_path(temporary_path);
        let destination_exists = destination_path.exists();
        let temporary = wide_path(temporary_path)?;
        let destination = wide_path(destination_path)?;
        let backup = wide_path(&backup_path)?;
        if destination_exists {
            // ReplaceFileW retains the replaced file's ACL and other security metadata.
            let replaced = unsafe {
                ReplaceFileW(
                    destination.as_ptr(),
                    temporary.as_ptr(),
                    backup.as_ptr(),
                    0,
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            };
            if replaced != 0 {
                let _ = fs::remove_file(&backup_path);
                return Ok(windows_existing_replacement_status());
            }
            let error = io::Error::last_os_error();
            if error
                .raw_os_error()
                .is_some_and(windows_replacement_relocated_original)
            {
                let replace_message = error.to_string();
                return match recover_relocated_windows_snapshot(
                    || move_file_write_through(&backup, &destination),
                    || move_file_write_through(&temporary, &destination),
                ) {
                    WindowsRecoveryOutcome::OriginalRestored => {
                        Err(Error::io("replace snapshot", error))
                    }
                    WindowsRecoveryOutcome::CandidatePublished { restore_error } => {
                        Ok(StoreStatus::PublishedWithError(Error::Io {
                            operation: "recover partial Windows snapshot replacement",
                            message: format!(
                                "ReplaceFileW failed ({replace_message}); restoring the old snapshot failed ({restore_error}); the candidate was published and the old snapshot remains at {}",
                                backup_path.display()
                            ),
                        }))
                    }
                    WindowsRecoveryOutcome::ManualRecoveryRequired {
                        restore_error,
                        publish_error,
                    } => Ok(StoreStatus::RecoveryRequired(Error::Io {
                        operation: "recover partial Windows snapshot replacement",
                        message: format!(
                            "ReplaceFileW failed ({replace_message}); restoring {} failed ({restore_error}); publishing {} failed ({publish_error}); both recovery files were retained",
                            backup_path.display(),
                            temporary_path.display()
                        ),
                    })),
                };
            }
            if error.kind() != io::ErrorKind::NotFound {
                return Err(Error::io("replace snapshot", error));
            }
        }

        move_file_write_through(&temporary, &destination)
            .map_err(|error| Error::io("replace snapshot", error))?;
        Ok(StoreStatus::Durable)
    }

    fn backup_path(temporary: &Path) -> std::path::PathBuf {
        let mut backup = temporary.as_os_str().to_os_string();
        backup.push(".backup");
        backup.into()
    }

    fn move_file_write_through(existing: &[u16], new: &[u16]) -> io::Result<()> {
        let moved = unsafe {
            MoveFileExW(
                existing.as_ptr(),
                new.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if moved == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
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

    fn recovery_paths(label: &str) -> (PathBuf, PathBuf, PathBuf) {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "rusthouse-windows-recovery-{label}-{}-{sequence}",
            std::process::id()
        ));
        (
            base.with_extension("destination"),
            base.with_extension("candidate"),
            base.with_extension("backup"),
        )
    }

    fn remove_recovery_files(paths: &[&Path]) {
        for path in paths {
            let _ = fs::remove_file(path);
        }
    }

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
    fn generated_temporary_and_backup_paths_are_reserved() {
        let temporary = next_temporary_snapshot_path(&std::env::temp_dir());
        assert!(matches!(
            Persistence::acquire(temporary.clone()),
            Err(Error::ReservedDatabasePath(_))
        ));

        let mut backup = temporary.as_os_str().to_os_string();
        backup.push(".backup");
        assert!(matches!(
            Persistence::acquire(PathBuf::from(backup)),
            Err(Error::ReservedDatabasePath(_))
        ));
    }

    #[test]
    fn lock_replacement_between_open_and_acquire_is_rejected() {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let database = std::env::temp_dir().join(format!(
            "rusthouse-lock-race-{}-{sequence}.db",
            std::process::id()
        ));
        let normalized = normalize_path(&database).unwrap();
        let mut lock_name = normalized.as_os_str().to_os_string();
        lock_name.push(crate::sidecar::LOCK_SUFFIX);
        let lock = PathBuf::from(lock_name);
        *REPLACE_LOCK_BEFORE_ACQUIRE
            .lock()
            .expect("lock replacement test hook must not be poisoned") = Some(lock.clone());

        assert!(matches!(
            Persistence::acquire(database.clone()),
            Err(Error::UnsafeLockPath(_))
        ));
        let _ = fs::remove_file(lock);
        let _ = fs::remove_file(database);
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

    #[test]
    fn windows_partial_replace_error_codes_have_safe_layouts() {
        assert!(!windows_replacement_relocated_original(1176));
        assert!(windows_replacement_relocated_original(1177));
    }

    #[test]
    fn windows_existing_replacement_is_not_acknowledged_durable() {
        let StoreStatus::PublishedWithError(error) = windows_existing_replacement_status() else {
            panic!("existing Windows replacement must report uncertain durability");
        };
        assert!(error.to_string().contains("no supported write-through"));
    }

    #[test]
    fn windows_partial_replace_restores_old_snapshot_first() {
        let (destination, candidate, backup) = recovery_paths("restore");
        fs::write(&candidate, b"candidate").unwrap();
        fs::write(&backup, b"old").unwrap();

        let outcome = recover_relocated_windows_snapshot(
            || fs::rename(&backup, &destination),
            || -> std::io::Result<()> { panic!("candidate fallback must not run") },
        );
        assert!(matches!(outcome, WindowsRecoveryOutcome::OriginalRestored));
        assert_eq!(fs::read(&destination).unwrap(), b"old");
        assert_eq!(fs::read(&candidate).unwrap(), b"candidate");
        remove_recovery_files(&[&destination, &candidate, &backup]);
    }

    #[test]
    fn windows_partial_replace_publishes_candidate_if_restore_fails() {
        let (destination, candidate, backup) = recovery_paths("publish");
        fs::write(&candidate, b"candidate").unwrap();
        fs::write(&backup, b"old").unwrap();

        let outcome = recover_relocated_windows_snapshot(
            || Err(std::io::Error::other("injected restore failure")),
            || fs::rename(&candidate, &destination),
        );
        let WindowsRecoveryOutcome::CandidatePublished { restore_error } = outcome else {
            panic!("expected candidate publication");
        };
        assert_eq!(restore_error.kind(), std::io::ErrorKind::Other);
        assert_eq!(fs::read(&destination).unwrap(), b"candidate");
        assert_eq!(fs::read(&backup).unwrap(), b"old");
        remove_recovery_files(&[&destination, &candidate, &backup]);
    }

    #[test]
    fn windows_partial_replace_retains_both_files_if_recovery_fails() {
        let (destination, candidate, backup) = recovery_paths("manual");
        fs::write(&candidate, b"candidate").unwrap();
        fs::write(&backup, b"old").unwrap();

        let outcome = recover_relocated_windows_snapshot(
            || Err(std::io::Error::other("injected restore failure")),
            || Err(std::io::Error::other("injected publish failure")),
        );
        let WindowsRecoveryOutcome::ManualRecoveryRequired {
            restore_error,
            publish_error,
        } = outcome
        else {
            panic!("expected retained recovery files");
        };
        assert_eq!(restore_error.kind(), std::io::ErrorKind::Other);
        assert_eq!(publish_error.kind(), std::io::ErrorKind::Other);
        assert!(!destination.exists());
        assert_eq!(fs::read(&candidate).unwrap(), b"candidate");
        assert_eq!(fs::read(&backup).unwrap(), b"old");
        remove_recovery_files(&[&destination, &candidate, &backup]);
    }
}
