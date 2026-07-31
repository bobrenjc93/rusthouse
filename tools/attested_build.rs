#[allow(dead_code)]
#[path = "../build_provenance.rs"]
mod build_provenance;
#[allow(dead_code)]
#[path = "../benchmark/sha256.rs"]
mod sha256;

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::time::{SystemTime, UNIX_EPOCH};

use build_provenance::{has_hidden_git_index_entries, owned_git_repository};

const MARKER_PREFIX: &str = "rusthouse-final-rustc-";

fn main() {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    let result = if arguments.is_empty() {
        build_attested_binaries()
    } else {
        wrap_rustc(&arguments)
    };
    match result {
        Ok(code) => process::exit(code),
        Err(error) => {
            eprintln!("attested build error: {error}");
            process::exit(1);
        }
    }
}

fn build_attested_binaries() -> Result<i32, String> {
    let wrapper = env::current_exe()
        .map_err(|error| format!("cannot locate attestation wrapper executable: {error}"))?;
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let source_root = cargo_source_root(&cargo)?;
    let target_directory = cargo_target_directory(&cargo)?;
    let initial_source = live_source_provenance(&source_root)?;
    let session = new_build_session()?;
    let snapshot = SourceSnapshot::create(&source_root, initial_source.as_ref(), &session)?;
    let manifest = snapshot.root.join("Cargo.toml");
    let mut rusthouse_command = Command::new(&cargo);
    rusthouse_command
        .args(["build", "--release", "--bin", "rusthouse"])
        .arg("--manifest-path")
        .arg(&manifest)
        .env("CARGO_TARGET_DIR", &target_directory)
        .env("RUSTC_WORKSPACE_WRAPPER", &wrapper)
        .env("RUSTHOUSE_ATTESTED_BUILD", "1")
        .env("RUSTHOUSE_ATTESTED_BUILD_SESSION", &session)
        .env_remove("RUSTHOUSE_ATTESTED_BUILD_TOKEN")
        .env_remove("RUSTHOUSE_ATTESTED_BINARY_SHA256");
    configure_live_source_provenance(&mut rusthouse_command, initial_source.as_ref());
    let (first_status, rusthouse_artifact) = cargo_artifact(&mut rusthouse_command, "rusthouse")?;
    if first_status != 0 {
        return Ok(first_status);
    }

    let rusthouse_path = require_compiled_artifact(rusthouse_artifact, "RustHouse")?;
    let initial_rusthouse = capture_artifact(&rusthouse_path, "RustHouse")?;
    let mut benchmark_command = Command::new(&cargo);
    benchmark_command
        .args(["build", "--release", "--bin", "clickhouse-parity-bench"])
        .arg("--manifest-path")
        .arg(&manifest)
        .env("CARGO_TARGET_DIR", &target_directory)
        .env("RUSTC_WORKSPACE_WRAPPER", wrapper)
        .env("RUSTHOUSE_ATTESTED_BUILD", "1")
        .env("RUSTHOUSE_ATTESTED_BUILD_SESSION", session)
        .env_remove("RUSTHOUSE_ATTESTED_BUILD_TOKEN")
        .env(
            "RUSTHOUSE_ATTESTED_BINARY_SHA256",
            &initial_rusthouse.sha256,
        );
    configure_live_source_provenance(&mut benchmark_command, initial_source.as_ref());
    let (status, benchmark_path) =
        cargo_artifact(&mut benchmark_command, "clickhouse-parity-bench")?;
    if status != 0 {
        return Ok(status);
    }
    let benchmark_path = require_compiled_artifact(benchmark_path, "benchmark")?;
    let final_benchmark = capture_artifact(&benchmark_path, "benchmark")?;
    let final_rusthouse = capture_artifact(&rusthouse_path, "RustHouse")?;
    validate_final_artifact_pair(&initial_rusthouse, &final_rusthouse, &final_benchmark)?;
    if let Some(initial_source) = initial_source.as_ref() {
        validate_live_source_after_build(
            &source_root,
            initial_source,
            &final_rusthouse.attestation.shared,
        )?;
    }
    Ok(status)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LiveSourceProvenance {
    commit: String,
    dirty: bool,
}

fn cargo_source_root(cargo: &std::ffi::OsStr) -> Result<PathBuf, String> {
    let output = Command::new(cargo)
        .args(["locate-project", "--workspace", "--message-format", "plain"])
        .output()
        .map_err(|error| format!("could not locate Cargo workspace manifest: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "could not locate Cargo workspace manifest: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let manifest = String::from_utf8(output.stdout)
        .map_err(|_| "Cargo workspace manifest path was not UTF-8".to_owned())?;
    let manifest = PathBuf::from(manifest.trim());
    let root = manifest
        .parent()
        .ok_or_else(|| format!("Cargo manifest '{}' has no parent", manifest.display()))?;
    fs_canonicalize(root, "Cargo source root")
}

fn cargo_target_directory(cargo: &std::ffi::OsStr) -> Result<PathBuf, String> {
    let output = Command::new(cargo)
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .output()
        .map_err(|error| format!("could not resolve Cargo target directory: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "could not resolve Cargo target directory: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let metadata =
        String::from_utf8(output.stdout).map_err(|_| "Cargo metadata was not UTF-8".to_owned())?;
    let target_directory = json_string_field(&metadata, "target_directory")?
        .filter(|path| !path.is_empty())
        .ok_or_else(|| "Cargo metadata omitted target_directory".to_owned())?;
    fs_canonicalize_or_absolute(Path::new(&target_directory), "Cargo target directory")
}

fn fs_canonicalize_or_absolute(path: &Path, name: &str) -> Result<PathBuf, String> {
    if path.exists() {
        return fs_canonicalize(path, name);
    }
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    env::current_dir()
        .map(|directory| directory.join(path))
        .map_err(|error| format!("could not resolve {name} '{}': {error}", path.display()))
}

fn fs_canonicalize(path: &Path, name: &str) -> Result<PathBuf, String> {
    fs::canonicalize(path)
        .map_err(|error| format!("could not resolve {name} '{}': {error}", path.display()))
}

struct SourceSnapshot {
    root: PathBuf,
    worktree_owner: Option<PathBuf>,
}

impl SourceSnapshot {
    fn create(
        source_root: &Path,
        live_source: Option<&LiveSourceProvenance>,
        session: &str,
    ) -> Result<Self, String> {
        let root = env::temp_dir().join(format!(
            "rusthouse-attested-source-{}-{}",
            process::id(),
            &session[..16]
        ));
        if root.exists() {
            return Err(format!(
                "attested source snapshot path already exists: '{}'",
                root.display()
            ));
        }

        if let Some(source) = live_source.filter(|source| !source.dirty) {
            let output = Command::new("git")
                .args(["worktree", "add", "--detach", "--quiet"])
                .arg(&root)
                .arg(&source.commit)
                .current_dir(source_root)
                .output()
                .map_err(|error| format!("could not create clean source snapshot: {error}"))?;
            if !output.status.success() {
                let _ = fs::remove_dir_all(&root);
                return Err(format!(
                    "could not create clean source snapshot: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            return Ok(Self {
                root,
                worktree_owner: Some(source_root.to_path_buf()),
            });
        }

        create_private_directory(&root)?;
        if let Err(error) = copy_source_tree(source_root, &root) {
            let _ = fs::remove_dir_all(&root);
            return Err(error);
        }
        Ok(Self {
            root,
            worktree_owner: None,
        })
    }
}

impl Drop for SourceSnapshot {
    fn drop(&mut self) {
        if let Some(owner) = &self.worktree_owner {
            let _ = Command::new("git")
                .args(["worktree", "remove", "--force"])
                .arg(&self.root)
                .current_dir(owner)
                .status();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn create_private_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt as _;

        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder
    };
    #[cfg(not(unix))]
    let builder = fs::DirBuilder::new();
    builder.create(path).map_err(|error| {
        format!(
            "could not create source snapshot '{}': {error}",
            path.display()
        )
    })
}

fn copy_source_tree(source: &Path, destination: &Path) -> Result<(), String> {
    let entries = fs::read_dir(source).map_err(|error| {
        format!(
            "could not read source directory '{}': {error}",
            source.display()
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "could not inspect source directory '{}': {error}",
                source.display()
            )
        })?;
        let name = entry.file_name();
        if matches!(name.to_str(), Some(".git" | ".burner" | "target")) {
            continue;
        }
        let source_path = entry.path();
        let destination_path = destination.join(&name);
        let file_type = entry.file_type().map_err(|error| {
            format!(
                "could not inspect source entry '{}': {error}",
                source_path.display()
            )
        })?;
        if file_type.is_dir() {
            fs::create_dir(&destination_path).map_err(|error| {
                format!(
                    "could not create snapshot directory '{}': {error}",
                    destination_path.display()
                )
            })?;
            copy_source_tree(&source_path, &destination_path)?;
        } else if file_type.is_file() {
            fs::copy(&source_path, &destination_path).map_err(|error| {
                format!(
                    "could not copy source '{}' to snapshot: {error}",
                    source_path.display()
                )
            })?;
        } else {
            return Err(format!(
                "source snapshot does not support special entry '{}'",
                source_path.display()
            ));
        }
    }
    Ok(())
}

fn configure_live_source_provenance(
    command: &mut Command,
    live_source: Option<&LiveSourceProvenance>,
) {
    if let Some(source) = live_source {
        command
            .env("RUSTHOUSE_BUILD_SOURCE_COMMIT", &source.commit)
            .env("RUSTHOUSE_BUILD_SOURCE_DIRTY", source.dirty.to_string());
    }
}

fn live_source_provenance(source_root: &Path) -> Result<Option<LiveSourceProvenance>, String> {
    let Some(repository) = owned_git_repository(source_root) else {
        return Ok(None);
    };
    let status = Command::new("git")
        .args([
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "--untracked-files=normal",
        ])
        .current_dir(source_root)
        .output()
        .map_err(|error| format!("could not inspect source checkout: {error}"))?;
    if !status.status.success() {
        return Err(format!(
            "could not inspect source checkout: {}",
            String::from_utf8_lossy(&status.stderr).trim()
        ));
    }
    let hidden_index_entries = has_hidden_git_index_entries(source_root)
        .ok_or_else(|| "could not inspect source checkout index flags".to_owned())?;
    Ok(Some(LiveSourceProvenance {
        commit: repository.commit,
        dirty: !status.stdout.is_empty() || hidden_index_entries,
    }))
}

fn validate_live_source_after_build(
    source_root: &Path,
    initial: &LiveSourceProvenance,
    artifact: &SharedArtifactProvenance,
) -> Result<(), String> {
    let final_source = live_source_provenance(source_root)?
        .ok_or_else(|| "owned Git source checkout disappeared during attested build".to_owned())?;
    if &final_source != initial {
        return Err("source checkout changed while attested artifacts were being built".to_owned());
    }
    if artifact.source_commit != final_source.commit || artifact.source_dirty != final_source.dirty
    {
        return Err(
            "final artifact source provenance does not match the revalidated source checkout"
                .to_owned(),
        );
    }
    Ok(())
}

fn new_build_session() -> Result<String, String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system clock is before Unix epoch: {error}"))?;
    let material = format!(
        "rusthouse-attested-session-v1\nprocess={}\nseconds={}\nnanos={}\n",
        process::id(),
        now.as_secs(),
        now.subsec_nanos()
    );
    Ok(sha256::digest_hex(material.as_bytes()))
}

fn wrap_rustc(arguments: &[String]) -> Result<i32, String> {
    let rustc = arguments
        .first()
        .ok_or_else(|| "rustc path is unavailable".to_owned())?;
    let rustc_arguments = &arguments[1..];
    let mut command = Command::new(rustc);
    command.args(rustc_arguments);
    if let Some(token) = env::var("RUSTHOUSE_ATTESTED_BUILD_TOKEN").ok()
        && let Some((source_path, configuration)) = final_configuration(rustc_arguments)?
    {
        require_lower_hex("attested build token", &token, 64)?;
        command.arg(format!(
            "--remap-path-prefix={source_path}={MARKER_PREFIX}{token}-{}",
            hex_encode(configuration.as_bytes())
        ));
    }
    command_status(&mut command)
}

fn final_configuration(arguments: &[String]) -> Result<Option<(String, String)>, String> {
    let mut configuration = Vec::new();
    let mut source_path = None;
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        match argument.as_str() {
            "--crate-name" | "--out-dir" => {
                index += 1;
                if arguments.get(index).is_none() {
                    return Err(format!("rustc argument {argument} is missing its value"));
                }
            }
            "-C" | "--codegen" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or_else(|| format!("rustc argument {argument} is missing its value"))?;
                record_codegen(&mut configuration, value);
            }
            value if value.starts_with("--codegen=") => {
                record_codegen(&mut configuration, &value["--codegen=".len()..]);
            }
            value if value.starts_with("-C") => {
                record_codegen(&mut configuration, &value[2..]);
            }
            value if value.starts_with("--crate-name=") || value.starts_with("--out-dir=") => {}
            value if option_takes_separate_value(value) => {
                configuration.push(value.to_owned());
                index += 1;
                let option_value = arguments
                    .get(index)
                    .ok_or_else(|| format!("rustc argument {argument} is missing its value"))?;
                configuration.push(option_value.to_owned());
            }
            value if value.starts_with('@') => {
                return Err("rustc response files are unsupported by build attestation".to_owned());
            }
            value
                if source_path.is_none()
                    && !value.starts_with('-')
                    && Path::new(value)
                        .extension()
                        .is_some_and(|extension| extension == "rs") =>
            {
                source_path = Some(value.to_owned());
            }
            value => configuration.push(value.to_owned()),
        }
        index += 1;
    }

    Ok(source_path.map(|source_path| (source_path, encode_arguments(&configuration))))
}

fn option_takes_separate_value(argument: &str) -> bool {
    matches!(
        argument,
        "--cfg"
            | "--check-cfg"
            | "-L"
            | "-l"
            | "--crate-type"
            | "--edition"
            | "--emit"
            | "--print"
            | "-o"
            | "--explain"
            | "--target"
            | "-A"
            | "--allow"
            | "-W"
            | "--warn"
            | "--force-warn"
            | "-D"
            | "--deny"
            | "-F"
            | "--forbid"
            | "--cap-lints"
            | "--extern"
            | "--error-format"
            | "--json"
            | "--color"
            | "--diagnostic-width"
            | "--remap-path-prefix"
            | "--sysroot"
    )
}

fn encode_arguments(arguments: &[String]) -> String {
    let mut encoded = format!("rustc-argv-v1:{};", arguments.len());
    for argument in arguments {
        write!(encoded, "{}:", argument.len()).expect("writing to String cannot fail");
        encoded.push_str(argument);
    }
    encoded
}

fn record_codegen(configuration: &mut Vec<String>, value: &str) {
    if !matches!(
        value.split_once('=').map(|(name, _)| name),
        Some("metadata" | "extra-filename" | "incremental")
    ) {
        configuration.push(format!("-C{value}"));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SharedArtifactProvenance {
    source_commit: String,
    source_dirty: bool,
    rustc_version: String,
    target: String,
    profile: String,
    build_configuration_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ArtifactAttestation {
    shared: SharedArtifactProvenance,
    rusthouse_binary_sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CapturedArtifact {
    sha256: String,
    attestation: ArtifactAttestation,
}

struct CargoArtifact {
    path: PathBuf,
    fresh: bool,
}

fn require_compiled_artifact(
    artifact: Option<CargoArtifact>,
    name: &str,
) -> Result<PathBuf, String> {
    let artifact =
        artifact.ok_or_else(|| format!("Cargo did not report the {name} executable artifact"))?;
    if artifact.fresh {
        return Err(format!(
            "Cargo reported the {name} executable as fresh; refusing to attest a cached artifact"
        ));
    }
    Ok(artifact.path)
}

fn capture_artifact(path: &Path, name: &str) -> Result<CapturedArtifact, String> {
    let before = sha256::file_digest_hex(path)?;
    let attestation = validate_artifact(path, name)?;
    let after = sha256::file_digest_hex(path)?;
    if after != before {
        return Err(format!(
            "completed {name} artifact '{}' changed while its attestation was being read",
            path.display()
        ));
    }
    Ok(CapturedArtifact {
        sha256: after,
        attestation,
    })
}

fn validate_final_artifact_pair(
    initial_rusthouse: &CapturedArtifact,
    final_rusthouse: &CapturedArtifact,
    final_benchmark: &CapturedArtifact,
) -> Result<(), String> {
    if final_rusthouse.sha256 != initial_rusthouse.sha256 {
        return Err(
            "completed RustHouse artifact changed after the benchmark binding was created"
                .to_owned(),
        );
    }
    if final_rusthouse.attestation.shared != initial_rusthouse.attestation.shared {
        return Err(
            "completed RustHouse provenance changed after the benchmark binding was created"
                .to_owned(),
        );
    }
    if final_benchmark.attestation.shared != final_rusthouse.attestation.shared {
        return Err(
            "completed RustHouse and benchmark artifacts have different source, compiler, target, profile, or build-configuration provenance"
                .to_owned(),
        );
    }
    if final_benchmark
        .attestation
        .rusthouse_binary_sha256
        .as_deref()
        != Some(final_rusthouse.sha256.as_str())
    {
        return Err(
            "completed benchmark artifact does not bind the final RustHouse SHA-256".to_owned(),
        );
    }
    Ok(())
}

fn validate_artifact(path: &Path, name: &str) -> Result<ArtifactAttestation, String> {
    let output = Command::new(path)
        .arg("--benchmark-attestation")
        .output()
        .map_err(|error| {
            format!(
                "could not execute completed {name} artifact '{}': {error}",
                path.display()
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "completed {name} artifact '{}' rejected its build attestation: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|_| format!("completed {name} artifact attestation was not UTF-8"))?;
    if stdout.lines().next() != Some("rusthouse-build-attestation-v2") {
        return Err(format!(
            "completed {name} artifact returned an unknown build attestation"
        ));
    }
    let source_commit = required_attestation_field(&stdout, name, "source_commit")?;
    if !matches!(source_commit.len(), 40 | 64)
        || !source_commit
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return Err(format!(
            "completed {name} artifact returned an invalid source commit"
        ));
    }
    let source_dirty = match required_attestation_field(&stdout, name, "source_dirty")?.as_str() {
        "true" => true,
        "false" => false,
        _ => {
            return Err(format!(
                "completed {name} artifact returned an invalid source dirty state"
            ));
        }
    };
    let rustc_version = required_attestation_field(&stdout, name, "rustc_version")?;
    let target = required_attestation_field(&stdout, name, "target")?;
    let profile = required_attestation_field(&stdout, name, "profile")?;
    let build_configuration_sha256 =
        required_attestation_field(&stdout, name, "build_configuration_sha256")?;
    require_lower_hex(
        &format!("completed {name} build configuration SHA-256"),
        &build_configuration_sha256,
        64,
    )?;
    let rusthouse_binary_sha256 = attestation_field(&stdout, "rusthouse_binary_sha256");
    if let Some(digest) = &rusthouse_binary_sha256 {
        require_lower_hex(
            &format!("completed {name} RustHouse binary SHA-256"),
            digest,
            64,
        )?;
    }
    Ok(ArtifactAttestation {
        shared: SharedArtifactProvenance {
            source_commit,
            source_dirty,
            rustc_version,
            target,
            profile,
            build_configuration_sha256,
        },
        rusthouse_binary_sha256,
    })
}

fn required_attestation_field(
    attestation: &str,
    artifact_name: &str,
    field_name: &str,
) -> Result<String, String> {
    attestation_field(attestation, field_name)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            format!("completed {artifact_name} artifact omitted attestation field {field_name}")
        })
}

fn attestation_field(attestation: &str, name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    attestation
        .lines()
        .find_map(|line| line.strip_prefix(&prefix).map(str::to_owned))
}

fn cargo_artifact(
    command: &mut Command,
    target_name: &str,
) -> Result<(i32, Option<CargoArtifact>), String> {
    command.arg("--message-format=json-render-diagnostics");
    let display = format!("{command:?}");
    let output = command
        .output()
        .map_err(|error| format!("could not execute {display}: {error}"))?;
    io::stderr()
        .write_all(&output.stderr)
        .map_err(|error| format!("could not forward Cargo stderr: {error}"))?;
    if !output.status.success() {
        io::stderr()
            .write_all(&output.stdout)
            .map_err(|error| format!("could not forward Cargo output: {error}"))?;
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|error| format!("Cargo artifact output was not UTF-8: {error}"))?;
    let artifact = cargo_executable_artifact(&stdout, target_name)?;
    Ok((output.status.code().unwrap_or(1), artifact))
}

fn cargo_executable_artifact(
    messages: &str,
    target_name: &str,
) -> Result<Option<CargoArtifact>, String> {
    let mut artifact = None;
    for message in messages.lines() {
        if json_string_field(message, "reason")?.as_deref() != Some("compiler-artifact") {
            continue;
        }
        let Some(target) = json_object_field(message, "target") else {
            continue;
        };
        if json_string_field(target, "name")?.as_deref() != Some(target_name) {
            continue;
        }
        if let Some(executable) = json_string_field(message, "executable")? {
            let fresh = json_bool_field(message, "fresh")?.ok_or_else(|| {
                format!("Cargo artifact for {target_name} omitted its fresh state")
            })?;
            artifact = Some(CargoArtifact {
                path: PathBuf::from(executable),
                fresh,
            });
        }
    }
    Ok(artifact)
}

fn json_bool_field(input: &str, name: &str) -> Result<Option<bool>, String> {
    let marker = format!("\"{name}\":");
    let Some(value) = input
        .find(&marker)
        .map(|index| input[index + marker.len()..].trim_start())
    else {
        return Ok(None);
    };
    if value.starts_with("true") {
        Ok(Some(true))
    } else if value.starts_with("false") {
        Ok(Some(false))
    } else {
        Err(format!("Cargo JSON field {name} was not a boolean"))
    }
}

fn json_object_field<'a>(input: &'a str, name: &str) -> Option<&'a str> {
    let marker = format!("\"{name}\":{{");
    let start = input.find(&marker)? + marker.len() - 1;
    let mut depth = 0_u32;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, character) in input[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&input[start..start + offset + 1]);
                }
            }
            _ => {}
        }
    }
    None
}

fn json_string_field(input: &str, name: &str) -> Result<Option<String>, String> {
    let marker = format!("\"{name}\":");
    let Some(start) = input.find(&marker).map(|index| index + marker.len()) else {
        return Ok(None);
    };
    if !input[start..].starts_with('"') {
        return Ok(None);
    }
    decode_json_string(&input[start..]).map(Some)
}

fn decode_json_string(input: &str) -> Result<String, String> {
    let mut characters = input
        .strip_prefix('"')
        .ok_or_else(|| "JSON string is missing an opening quote".to_owned())?
        .chars();
    let mut decoded = String::new();
    while let Some(character) = characters.next() {
        match character {
            '"' => return Ok(decoded),
            '\\' => {
                match characters.next() {
                    Some('"') => decoded.push('"'),
                    Some('\\') => decoded.push('\\'),
                    Some('/') => decoded.push('/'),
                    Some('b') => decoded.push('\u{0008}'),
                    Some('f') => decoded.push('\u{000c}'),
                    Some('n') => decoded.push('\n'),
                    Some('r') => decoded.push('\r'),
                    Some('t') => decoded.push('\t'),
                    Some('u') => {
                        let digits = characters.by_ref().take(4).collect::<String>();
                        let value = u32::from_str_radix(&digits, 16)
                            .map_err(|_| format!("invalid JSON Unicode escape {digits:?}"))?;
                        decoded.push(char::from_u32(value).ok_or_else(|| {
                            format!("invalid JSON Unicode scalar value {value:#x}")
                        })?);
                    }
                    Some(escape) => return Err(format!("invalid JSON escape \\{escape}")),
                    None => return Err("unterminated JSON escape".to_owned()),
                }
            }
            character if character.is_control() => {
                return Err("unescaped control character in JSON string".to_owned());
            }
            character => decoded.push(character),
        }
    }
    Err("unterminated JSON string".to_owned())
}

fn require_lower_hex(name: &str, value: &str, length: usize) -> Result<(), String> {
    if value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(format!(
            "{name} must contain exactly {length} lowercase hexadecimal characters"
        ))
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

fn command_status(command: &mut Command) -> Result<i32, String> {
    let display = format!("{command:?}");
    let status = command
        .status()
        .map_err(|error| format!("could not execute {display}: {error}"))?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        ArtifactAttestation, CapturedArtifact, CargoArtifact, SharedArtifactProvenance,
        SourceSnapshot, cargo_executable_artifact, encode_arguments, final_configuration,
        live_source_provenance, require_compiled_artifact, validate_final_artifact_pair,
        validate_live_source_after_build,
    };

    #[test]
    fn cargo_artifact_messages_supply_the_executable_path() {
        let messages = concat!(
            "{\"reason\":\"compiler-artifact\",\"target\":{\"name\":\"dependency\"},\"executable\":null,\"fresh\":true}\n",
            "{\"reason\":\"compiler-artifact\",\"target\":{\"name\":\"rusthouse\"},\"executable\":\"C:\\\\work\\\\target\\\\triple\\\\release\\\\rusthouse.exe\",\"fresh\":false}\n"
        );
        let artifact = cargo_executable_artifact(messages, "rusthouse")
            .expect("artifact messages")
            .expect("RustHouse artifact");
        assert_eq!(
            artifact.path,
            PathBuf::from("C:\\work\\target\\triple\\release\\rusthouse.exe")
        );
        assert!(!artifact.fresh);
    }

    #[test]
    fn cached_executable_artifact_is_rejected() {
        let error = require_compiled_artifact(
            Some(CargoArtifact {
                path: PathBuf::from("rusthouse"),
                fresh: true,
            }),
            "RustHouse",
        )
        .expect_err("fresh Cargo artifact must be rejected");
        assert!(error.contains("cached artifact"));
    }

    #[test]
    fn source_mutation_after_provenance_capture_is_rejected() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "rusthouse-attested-source-race-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&root).expect("temporary source root");
        for arguments in [
            vec!["init", "--quiet"],
            vec!["config", "user.name", "RustHouse Test"],
            vec!["config", "user.email", "test@example.invalid"],
        ] {
            let status = Command::new("git")
                .args(arguments)
                .current_dir(&root)
                .status()
                .expect("run git");
            assert!(status.success());
        }
        fs::write(root.join("source.rs"), "fn original() {}\n").expect("source file");
        assert!(
            Command::new("git")
                .args(["add", "source.rs"])
                .current_dir(&root)
                .status()
                .expect("git add")
                .success()
        );
        let commit = Command::new("git")
            .args(["commit", "-m", "source"])
            .current_dir(&root)
            .output()
            .expect("git commit");
        assert!(commit.status.success());
        let initial = live_source_provenance(&root)
            .expect("initial provenance")
            .expect("owned Git checkout");
        let artifact = SharedArtifactProvenance {
            source_commit: initial.commit.clone(),
            source_dirty: initial.dirty,
            rustc_version: "rustc test".to_owned(),
            target: "test-target".to_owned(),
            profile: "release".to_owned(),
            build_configuration_sha256: "d".repeat(64),
        };

        fs::write(root.join("source.rs"), "fn mutated() {}\n").expect("mutated source");
        let error = validate_live_source_after_build(&root, &initial, &artifact)
            .expect_err("source mutation must reject artifacts");
        assert!(error.contains("source checkout changed"));
        fs::remove_dir_all(root).expect("cleanup source root");
    }

    #[test]
    fn clean_source_snapshot_isolated_from_transient_live_mutation() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "rusthouse-attested-snapshot-test-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&root).expect("temporary source root");
        for arguments in [
            vec!["init", "--quiet"],
            vec!["config", "user.name", "RustHouse Test"],
            vec!["config", "user.email", "test@example.invalid"],
        ] {
            assert!(
                Command::new("git")
                    .args(arguments)
                    .current_dir(&root)
                    .status()
                    .expect("run git")
                    .success()
            );
        }
        fs::write(root.join("source.rs"), "fn committed() {}\n").expect("source file");
        assert!(
            Command::new("git")
                .args(["add", "source.rs"])
                .current_dir(&root)
                .status()
                .expect("git add")
                .success()
        );
        assert!(
            Command::new("git")
                .args(["commit", "-m", "source"])
                .current_dir(&root)
                .status()
                .expect("git commit")
                .success()
        );
        let initial = live_source_provenance(&root)
            .expect("initial provenance")
            .expect("owned Git checkout");
        assert!(!initial.dirty);
        let snapshot = SourceSnapshot::create(&root, Some(&initial), &"a".repeat(64))
            .expect("clean source snapshot");

        fs::write(root.join("source.rs"), "fn transient() {}\n").expect("transient source");
        assert_eq!(
            fs::read_to_string(snapshot.root.join("source.rs")).expect("snapshot source"),
            "fn committed() {}\n"
        );
        fs::write(root.join("source.rs"), "fn committed() {}\n").expect("restore source");
        assert_eq!(
            live_source_provenance(&root).expect("final provenance"),
            Some(initial)
        );

        drop(snapshot);
        fs::remove_dir_all(root).expect("cleanup source root");
    }

    #[test]
    fn short_and_long_codegen_spellings_are_equivalent() {
        let short = final_configuration(&arguments(&["-C", "opt-level=0"]))
            .expect("short configuration")
            .expect("source");
        let long = final_configuration(&arguments(&["--codegen=opt-level=0"]))
            .expect("long configuration")
            .expect("source");
        let split_long = final_configuration(&arguments(&["--codegen", "opt-level=0"]))
            .expect("split long configuration")
            .expect("source");
        assert_eq!(short.1, long.1);
        assert_eq!(short.1, split_long.1);
    }

    #[test]
    fn linkage_and_sysroot_arguments_are_captured() {
        let (_, configuration) = final_configuration(&arguments(&[
            "-l",
            "framework=Foundation",
            "-L",
            "native=/tmp/libraries",
            "--extern",
            "dep=/tmp/libdep.rlib",
            "--sysroot",
            "/tmp/sysroot",
        ]))
        .expect("configuration")
        .expect("source");

        let expected = [
            "--crate-type=bin",
            "-l",
            "framework=Foundation",
            "-L",
            "native=/tmp/libraries",
            "--extern",
            "dep=/tmp/libdep.rlib",
            "--sysroot",
            "/tmp/sysroot",
        ]
        .map(str::to_owned);
        assert_eq!(configuration, encode_arguments(&expected));
    }

    #[test]
    fn argument_encoding_preserves_boundaries_around_newlines() {
        let one_argument = ["-Ldependency=/tmp/a\n--cfg=foo".to_owned()];
        let two_arguments = ["-Ldependency=/tmp/a".to_owned(), "--cfg=foo".to_owned()];

        assert_ne!(
            encode_arguments(&one_argument),
            encode_arguments(&two_arguments)
        );
    }

    #[test]
    fn rs_suffixed_option_values_do_not_replace_the_positional_source() {
        let joined = final_configuration(&arguments(&["-Lnative=/tmp/libs.rs"]))
            .expect("joined search path")
            .expect("source");
        let split = final_configuration(&arguments(&["-L", "native=/tmp/libs.rs"]))
            .expect("split search path")
            .expect("source");

        assert_eq!(joined.0, "src/main.rs");
        assert_eq!(split.0, "src/main.rs");
    }

    #[test]
    fn final_artifact_pair_rejects_replacement_and_shared_provenance_mismatch() {
        let digest = "a".repeat(64);
        let initial = captured_artifact(&digest, "1".repeat(40), None);
        let replaced = captured_artifact(&"b".repeat(64), "1".repeat(40), None);
        let benchmark = captured_artifact(&"c".repeat(64), "1".repeat(40), Some(&digest));
        assert!(validate_final_artifact_pair(&initial, &replaced, &benchmark).is_err());

        let final_rusthouse = initial.clone();
        let mismatched_benchmark =
            captured_artifact(&"c".repeat(64), "2".repeat(40), Some(&digest));
        assert!(
            validate_final_artifact_pair(&initial, &final_rusthouse, &mismatched_benchmark)
                .is_err()
        );
        validate_final_artifact_pair(&initial, &final_rusthouse, &benchmark)
            .expect("consistent final pair");
    }

    fn captured_artifact(
        digest: &str,
        source_commit: String,
        bound_rusthouse: Option<&str>,
    ) -> CapturedArtifact {
        CapturedArtifact {
            sha256: digest.to_owned(),
            attestation: ArtifactAttestation {
                shared: SharedArtifactProvenance {
                    source_commit,
                    source_dirty: false,
                    rustc_version: "rustc test".to_owned(),
                    target: "test-target".to_owned(),
                    profile: "release".to_owned(),
                    build_configuration_sha256: "d".repeat(64),
                },
                rusthouse_binary_sha256: bound_rusthouse.map(str::to_owned),
            },
        }
    }

    fn arguments(extra: &[&str]) -> Vec<String> {
        let mut arguments = vec![
            "--crate-name".to_owned(),
            "rusthouse".to_owned(),
            "src/main.rs".to_owned(),
            "--crate-type=bin".to_owned(),
        ];
        arguments.extend(extra.iter().map(|value| (*value).to_owned()));
        arguments
    }
}
