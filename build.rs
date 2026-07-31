use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is required");
    let manifest_dir = Path::new(&manifest_dir);

    let (source_commit, source_dirty) = source_provenance(manifest_dir);
    if !matches!(source_commit.len(), 40 | 64)
        || !source_commit
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        panic!("git returned an invalid source commit: {source_commit:?}");
    }

    let rustc = env::var("RUSTC").expect("RUSTC is required");
    let rustc_version = command_output(manifest_dir, &rustc, &["--version"]);
    let target = required_env("TARGET");
    let profile = required_env("PROFILE");

    emit(
        "RUSTHOUSE_SOURCE_COMMIT",
        &source_commit.to_ascii_lowercase(),
    );
    emit(
        "RUSTHOUSE_SOURCE_DIRTY",
        if source_dirty { "true" } else { "false" },
    );
    emit("RUSTHOUSE_RUSTC_VERSION", &rustc_version);
    emit("RUSTHOUSE_BUILD_TARGET", &target);
    emit("RUSTHOUSE_BUILD_PROFILE", &profile);

    println!("cargo:rerun-if-env-changed=RUSTC");
    println!("cargo:rerun-if-env-changed=RUSTHOUSE_BUILD_SOURCE_COMMIT");
    println!("cargo:rerun-if-env-changed=RUSTHOUSE_BUILD_SOURCE_DIRTY");
    println!("cargo:rerun-if-changed=.");
    if let Some(git_directory) =
        try_command_output(manifest_dir, "git", &["rev-parse", "--absolute-git-dir"])
    {
        println!("cargo:rerun-if-changed={git_directory}/HEAD");
        println!("cargo:rerun-if-changed={git_directory}/index");
    }
}

fn source_provenance(manifest_dir: &Path) -> (String, bool) {
    if let Some(commit) = try_command_output(manifest_dir, "git", &["rev-parse", "HEAD"]) {
        let status = command_output(
            manifest_dir,
            "git",
            &["status", "--porcelain=v1", "--untracked-files=normal"],
        );
        return (commit, !status.is_empty());
    }

    let explicit_commit = env::var("RUSTHOUSE_BUILD_SOURCE_COMMIT").ok();
    let explicit_dirty = env::var("RUSTHOUSE_BUILD_SOURCE_DIRTY").ok();
    match (explicit_commit, explicit_dirty) {
        (Some(commit), Some(dirty)) => {
            let dirty = parse_dirty(&dirty).unwrap_or_else(|| {
                panic!("RUSTHOUSE_BUILD_SOURCE_DIRTY must be true or false, got {dirty:?}")
            });
            return (commit, dirty);
        }
        (Some(_), None) | (None, Some(_)) => {
            panic!(
                "RUSTHOUSE_BUILD_SOURCE_COMMIT and RUSTHOUSE_BUILD_SOURCE_DIRTY must be supplied together"
            );
        }
        (None, None) => {}
    }

    let vcs_path = manifest_dir.join(".cargo_vcs_info.json");
    let contents = fs::read_to_string(&vcs_path).unwrap_or_else(|error| {
        panic!(
            "source provenance is unavailable: no Git checkout, explicit RUSTHOUSE_BUILD_SOURCE_COMMIT/RUSTHOUSE_BUILD_SOURCE_DIRTY inputs, or readable '{}': {error}",
            vcs_path.display()
        )
    });
    let commit = cargo_vcs_commit(&contents).unwrap_or_else(|| {
        panic!(
            "packaged VCS metadata '{}' does not contain a valid git.sha1",
            vcs_path.display()
        )
    });
    (commit, false)
}

fn parse_dirty(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn cargo_vcs_commit(contents: &str) -> Option<String> {
    let (_, after_key) = contents.split_once("\"sha1\"")?;
    let (_, after_colon) = after_key.split_once(':')?;
    let quoted = after_colon.trim_start().strip_prefix('"')?;
    let (commit, _) = quoted.split_once('"')?;
    if commit.len() == 40
        && commit
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        Some(commit.to_ascii_lowercase())
    } else {
        None
    }
}

fn required_env(name: &str) -> String {
    let value = env::var(name).unwrap_or_else(|_| panic!("{name} is required"));
    if value.trim().is_empty() || value.contains('\n') || value.contains('\r') {
        panic!("{name} is empty or contains a newline");
    }
    value
}

fn emit(name: &str, value: &str) {
    if value.is_empty() || value.contains('\n') || value.contains('\r') {
        panic!("build attestation value {name} is empty or contains a newline");
    }
    println!("cargo:rustc-env={name}={value}");
}

fn command_output(directory: &Path, program: &str, arguments: &[&str]) -> String {
    let output = Command::new(program)
        .args(arguments)
        .current_dir(directory)
        .output()
        .unwrap_or_else(|error| panic!("could not run {program}: {error}"));
    if !output.status.success() {
        panic!(
            "{program} {} failed with {}: {}",
            arguments.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout)
        .unwrap_or_else(|error| panic!("{program} output was not UTF-8: {error}"))
        .trim()
        .to_owned()
}

fn try_command_output(directory: &Path, program: &str, arguments: &[&str]) -> Option<String> {
    let output = Command::new(program)
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
