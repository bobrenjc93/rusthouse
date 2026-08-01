use std::ffi::OsStr;
#[cfg(unix)]
use std::ffi::OsString;
#[cfg(windows)]
use std::fs::File;
use std::path::{Path, PathBuf};

pub(crate) const LOCK_SUFFIX: &str = ".rusthouse-lock";
pub(crate) const TEMP_PREFIX: &str = ".rusthouse-tmp.";

pub(crate) fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(LOCK_SUFFIX);
    PathBuf::from(name)
}

#[cfg(unix)]
pub(crate) fn lock_name(file_name: &OsStr) -> OsString {
    let mut name = file_name.to_owned();
    name.push(LOCK_SUFFIX);
    name
}

pub(crate) fn is_reserved_name(file_name: &OsStr) -> bool {
    let name = file_name
        .to_string_lossy()
        .trim_end_matches([' ', '.'])
        .chars()
        .flat_map(char::to_lowercase)
        .collect::<String>();
    name.starts_with('.') || name.ends_with(LOCK_SUFFIX) || name.starts_with(TEMP_PREFIX)
}

#[cfg(windows)]
pub(crate) fn open_parent_directory_guard(path: &Path) -> std::io::Result<File> {
    use std::os::windows::io::FromRawHandle;
    use std::ptr;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let path = wide_path(path);
    // Omitting FILE_SHARE_DELETE pins the parent name until this handle is dropped.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        Err(std::io::Error::last_os_error())
    } else {
        // SAFETY: CreateFileW returned a new owned handle.
        Ok(unsafe { File::from_raw_handle(handle) })
    }
}

#[cfg(windows)]
pub(crate) fn same_file(opened: &File, path: &Path) -> std::io::Result<bool> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let current = match OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
    {
        Ok(file) => file,
        Err(_) => return Ok(false),
    };
    if current.metadata()?.file_type().is_symlink() {
        return Ok(false);
    }
    Ok(file_identity(opened)? == file_identity(&current)?)
}

#[cfg(windows)]
fn file_identity(file: &File) -> std::io::Result<(u32, u64)> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;

    let mut information = MaybeUninit::<ByHandleFileInformation>::uninit();
    // SAFETY: The handle is live and the output points to the documented structure.
    let succeeded =
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) };
    if succeeded == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: GetFileInformationByHandle initializes the structure on success.
    let information = unsafe { information.assume_init() };
    let file_index =
        (u64::from(information.file_index_high) << 32) | u64::from(information.file_index_low);
    Ok((information.volume_serial_number, file_index))
}

#[cfg(windows)]
#[repr(C)]
struct FileTime {
    low_date_time: u32,
    high_date_time: u32,
}

#[cfg(windows)]
#[repr(C)]
struct ByHandleFileInformation {
    file_attributes: u32,
    creation_time: FileTime,
    last_access_time: FileTime,
    last_write_time: FileTime,
    volume_serial_number: u32,
    file_size_high: u32,
    file_size_low: u32,
    number_of_links: u32,
    file_index_high: u32,
    file_index_low: u32,
}

#[cfg(windows)]
#[link(name = "Kernel32")]
unsafe extern "system" {
    fn GetFileInformationByHandle(
        file: *mut std::ffi::c_void,
        information: *mut ByHandleFileInformation,
    ) -> i32;
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
