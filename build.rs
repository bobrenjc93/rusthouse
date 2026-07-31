mod build_provenance;

use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

use build_provenance::{cargo_vcs_provenance, owned_git_repository, parse_dirty};

struct SourceProvenance {
    commit: String,
    dirty: bool,
    git_watch_paths: Vec<std::path::PathBuf>,
}

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is required");
    let manifest_dir = Path::new(&manifest_dir);

    let source = source_provenance(manifest_dir);
    if !matches!(source.commit.len(), 40 | 64)
        || !source
            .commit
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        panic!(
            "source provenance contains an invalid commit: {:?}",
            source.commit
        );
    }

    let rustc = env::var("RUSTC").expect("RUSTC is required");
    let rustc_version = command_output(manifest_dir, &rustc, &["--version"]);
    let target = required_env("TARGET");
    let profile = required_env("PROFILE");

    emit(
        "RUSTHOUSE_SOURCE_COMMIT",
        &source.commit.to_ascii_lowercase(),
    );
    emit(
        "RUSTHOUSE_SOURCE_DIRTY",
        if source.dirty { "true" } else { "false" },
    );
    emit("RUSTHOUSE_RUSTC_VERSION", &rustc_version);
    emit("RUSTHOUSE_BUILD_TARGET", &target);
    emit("RUSTHOUSE_BUILD_PROFILE", &profile);

    println!("cargo:rerun-if-env-changed=RUSTC");
    println!("cargo:rerun-if-env-changed=RUSTHOUSE_BUILD_SOURCE_COMMIT");
    println!("cargo:rerun-if-env-changed=RUSTHOUSE_BUILD_SOURCE_DIRTY");
    emit_source_watches(manifest_dir);
    for path in source.git_watch_paths {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn source_provenance(manifest_dir: &Path) -> SourceProvenance {
    if let Some(repository) = owned_git_repository(manifest_dir) {
        let status = command_output(
            manifest_dir,
            "git",
            &[
                "--no-optional-locks",
                "status",
                "--porcelain=v1",
                "--untracked-files=normal",
            ],
        );
        return SourceProvenance {
            commit: repository.commit,
            dirty: !status.is_empty(),
            git_watch_paths: repository.watch_paths,
        };
    }

    let vcs_path = manifest_dir.join(".cargo_vcs_info.json");
    if vcs_path.is_file() {
        let contents = fs::read_to_string(&vcs_path).unwrap_or_else(|error| {
            panic!(
                "could not read packaged VCS metadata '{}': {error}",
                vcs_path.display()
            )
        });
        let provenance = cargo_vcs_provenance(&contents).unwrap_or_else(|| {
            panic!(
                "packaged VCS metadata '{}' does not contain valid git.sha1/dirty fields",
                vcs_path.display()
            )
        });
        return SourceProvenance {
            commit: provenance.commit,
            dirty: provenance.dirty,
            git_watch_paths: Vec::new(),
        };
    }

    let explicit_commit = env::var("RUSTHOUSE_BUILD_SOURCE_COMMIT").ok();
    let explicit_dirty = env::var("RUSTHOUSE_BUILD_SOURCE_DIRTY").ok();
    match (explicit_commit, explicit_dirty) {
        (Some(commit), Some(dirty)) => {
            let dirty = parse_dirty(&dirty).unwrap_or_else(|| {
                panic!("RUSTHOUSE_BUILD_SOURCE_DIRTY must be true or false, got {dirty:?}")
            });
            return SourceProvenance {
                commit,
                dirty,
                git_watch_paths: Vec::new(),
            };
        }
        (Some(_), None) | (None, Some(_)) => {
            panic!(
                "RUSTHOUSE_BUILD_SOURCE_COMMIT and RUSTHOUSE_BUILD_SOURCE_DIRTY must be supplied together"
            );
        }
        (None, None) => {}
    }

    panic!(
        "source provenance is unavailable: no package-owned Git checkout, packaged .cargo_vcs_info.json, or explicit RUSTHOUSE_BUILD_SOURCE_COMMIT/RUSTHOUSE_BUILD_SOURCE_DIRTY inputs"
    );
}

fn emit_source_watches(manifest_dir: &Path) {
    let entries = fs::read_dir(manifest_dir)
        .unwrap_or_else(|error| panic!("could not enumerate package sources: {error}"));
    for entry in entries {
        let entry =
            entry.unwrap_or_else(|error| panic!("could not inspect package source: {error}"));
        let file_name = entry.file_name();
        if matches!(file_name.to_str(), Some(".git" | ".burner" | "target")) {
            continue;
        }
        println!("cargo:rerun-if-changed={}", entry.path().display());
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
