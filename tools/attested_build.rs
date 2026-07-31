#[allow(dead_code)]
#[path = "../benchmark/sha256.rs"]
mod sha256;

use std::env;
use std::fmt::Write as _;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{self, Command};

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
    let (first_status, rusthouse_path) = cargo_artifact(
        Command::new(&cargo)
            .args(["build", "--release", "--bin", "rusthouse"])
            .env("RUSTC_WORKSPACE_WRAPPER", &wrapper)
            .env("RUSTHOUSE_ATTESTED_BUILD", "1")
            .env_remove("RUSTHOUSE_ATTESTED_BUILD_TOKEN")
            .env_remove("RUSTHOUSE_ATTESTED_BINARY_SHA256"),
        "rusthouse",
    )?;
    if first_status != 0 {
        return Ok(first_status);
    }

    let rusthouse_path = rusthouse_path
        .ok_or_else(|| "Cargo did not report the RustHouse executable artifact".to_owned())?;
    let initial_rusthouse = capture_artifact(&rusthouse_path, "RustHouse")?;
    let (status, benchmark_path) = cargo_artifact(
        Command::new(cargo)
            .args(["build", "--release", "--bin", "clickhouse-parity-bench"])
            .env("RUSTC_WORKSPACE_WRAPPER", wrapper)
            .env("RUSTHOUSE_ATTESTED_BUILD", "1")
            .env_remove("RUSTHOUSE_ATTESTED_BUILD_TOKEN")
            .env(
                "RUSTHOUSE_ATTESTED_BINARY_SHA256",
                &initial_rusthouse.sha256,
            ),
        "clickhouse-parity-bench",
    )?;
    if status != 0 {
        return Ok(status);
    }
    let benchmark_path = benchmark_path
        .ok_or_else(|| "Cargo did not report the benchmark executable artifact".to_owned())?;
    let final_benchmark = capture_artifact(&benchmark_path, "benchmark")?;
    let final_rusthouse = capture_artifact(&rusthouse_path, "RustHouse")?;
    validate_final_artifact_pair(&initial_rusthouse, &final_rusthouse, &final_benchmark)?;
    Ok(status)
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
) -> Result<(i32, Option<PathBuf>), String> {
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

fn cargo_executable_artifact(messages: &str, target_name: &str) -> Result<Option<PathBuf>, String> {
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
            artifact = Some(PathBuf::from(executable));
        }
    }
    Ok(artifact)
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
    use std::path::PathBuf;

    use super::{
        ArtifactAttestation, CapturedArtifact, SharedArtifactProvenance, cargo_executable_artifact,
        encode_arguments, final_configuration, validate_final_artifact_pair,
    };

    #[test]
    fn cargo_artifact_messages_supply_the_executable_path() {
        let messages = concat!(
            "{\"reason\":\"compiler-artifact\",\"target\":{\"name\":\"dependency\"},\"executable\":null}\n",
            "{\"reason\":\"compiler-artifact\",\"target\":{\"name\":\"rusthouse\"},\"executable\":\"C:\\\\work\\\\target\\\\triple\\\\release\\\\rusthouse.exe\"}\n"
        );
        assert_eq!(
            cargo_executable_artifact(messages, "rusthouse").expect("artifact messages"),
            Some(PathBuf::from(
                "C:\\work\\target\\triple\\release\\rusthouse.exe"
            ))
        );
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
