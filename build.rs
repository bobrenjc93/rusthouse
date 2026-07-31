use std::env;
use std::path::Path;
use std::process::Command;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is required");
    let manifest_dir = Path::new(&manifest_dir);

    let source_commit = command_output(manifest_dir, "git", &["rev-parse", "HEAD"]);
    if !matches!(source_commit.len(), 40 | 64)
        || !source_commit
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        panic!("git returned an invalid source commit: {source_commit:?}");
    }

    let git_status = command_output(
        manifest_dir,
        "git",
        &["status", "--porcelain=v1", "--untracked-files=normal"],
    );
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
        if git_status.is_empty() {
            "false"
        } else {
            "true"
        },
    );
    emit("RUSTHOUSE_RUSTC_VERSION", &rustc_version);
    emit("RUSTHOUSE_BUILD_TARGET", &target);
    emit("RUSTHOUSE_BUILD_PROFILE", &profile);

    println!("cargo:rerun-if-env-changed=RUSTC");
    println!("cargo:rerun-if-env-changed=TARGET");
    println!("cargo:rerun-if-env-changed=PROFILE");
    for path in command_output(manifest_dir, "git", &["ls-files"]).lines() {
        println!("cargo:rerun-if-changed={path}");
    }
    for git_path in ["HEAD", "index"] {
        let path = command_output(manifest_dir, "git", &["rev-parse", "--git-path", git_path]);
        println!("cargo:rerun-if-changed={path}");
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
