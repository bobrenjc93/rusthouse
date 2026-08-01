use std::ffi::OsStr;
#[cfg(unix)]
use std::ffi::OsString;
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
