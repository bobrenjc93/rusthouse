use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, PartialEq, Eq)]
pub struct CargoVcsProvenance {
    pub commit: String,
    pub dirty: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct GitRepository {
    pub commit: String,
    pub watch_paths: Vec<PathBuf>,
}

pub fn cargo_vcs_provenance(contents: &str) -> Option<CargoVcsProvenance> {
    let commit = json_string_field(contents, "sha1")?;
    if commit.len() != 40
        || !commit
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return None;
    }
    let dirty = json_optional_bool_field(contents, "dirty")?.unwrap_or(false);
    Some(CargoVcsProvenance {
        commit: commit.to_ascii_lowercase(),
        dirty,
    })
}

pub fn parse_dirty(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

pub fn owned_git_repository(manifest_dir: &Path) -> Option<GitRepository> {
    let top_level = command_output(manifest_dir, &["rev-parse", "--show-toplevel"])?;
    let top_level = fs::canonicalize(top_level).ok()?;
    let manifest_dir = fs::canonicalize(manifest_dir).ok()?;
    if top_level != manifest_dir {
        return None;
    }

    let commit = command_output(&manifest_dir, &["rev-parse", "HEAD"])?;
    let git_directory = absolute_git_path(
        &manifest_dir,
        &command_output(&manifest_dir, &["rev-parse", "--absolute-git-dir"])?,
    );
    let mut watch_paths = vec![git_directory.join("HEAD"), git_directory.join("index")];

    if let Some(symbolic_ref) = command_output(&manifest_dir, &["symbolic-ref", "-q", "HEAD"])
        && let Some(path) =
            command_output(&manifest_dir, &["rev-parse", "--git-path", &symbolic_ref])
    {
        let resolved_ref = absolute_git_path(&manifest_dir, &path);
        if resolved_ref.exists() {
            watch_paths.push(resolved_ref);
        } else if let Some(parent) = nearest_existing_parent(&resolved_ref) {
            watch_paths.push(parent);
        }
    }
    if let Some(common_directory) =
        command_output(&manifest_dir, &["rev-parse", "--git-common-dir"])
    {
        let packed_refs = absolute_git_path(&manifest_dir, &common_directory).join("packed-refs");
        if packed_refs.is_file() {
            watch_paths.push(packed_refs);
        }
    }
    watch_paths.sort();
    watch_paths.dedup();

    Some(GitRepository {
        commit,
        watch_paths,
    })
}

fn json_string_field(contents: &str, field: &str) -> Option<String> {
    let marker = format!("\"{field}\"");
    let (_, after_key) = contents.split_once(&marker)?;
    let (_, after_colon) = after_key.split_once(':')?;
    let quoted = after_colon.trim_start().strip_prefix('"')?;
    let (value, _) = quoted.split_once('"')?;
    Some(value.to_owned())
}

fn json_optional_bool_field(contents: &str, field: &str) -> Option<Option<bool>> {
    let marker = format!("\"{field}\"");
    let Some((_, after_key)) = contents.split_once(&marker) else {
        return Some(None);
    };
    let (_, after_colon) = after_key.split_once(':')?;
    let value = after_colon.trim_start();
    for (spelling, parsed) in [("true", true), ("false", false)] {
        if let Some(remainder) = value.strip_prefix(spelling)
            && remainder
                .chars()
                .next()
                .is_none_or(|character| character.is_ascii_whitespace() || ",}".contains(character))
        {
            return Some(Some(parsed));
        }
    }
    None
}

fn absolute_git_path(manifest_dir: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        manifest_dir.join(path)
    }
}

fn nearest_existing_parent(path: &Path) -> Option<PathBuf> {
    let mut candidate = path.parent();
    while let Some(parent) = candidate {
        if parent.is_dir() {
            return Some(parent.to_owned());
        }
        candidate = parent.parent();
    }
    None
}

fn command_output(directory: &Path, arguments: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|value| value.trim().to_owned())
}
