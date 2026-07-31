#[path = "../build_provenance.rs"]
mod build_provenance;

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use build_provenance::{
    CargoVcsProvenance, cargo_vcs_provenance, has_hidden_git_index_entries, owned_git_repository,
    parse_dirty,
};

const ATTESTED_BUILD_TOKEN: &str =
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[test]
fn cargo_vcs_metadata_preserves_dirty_package_state() {
    let clean = r#"{"git":{"sha1":"0123456789abcdef0123456789abcdef01234567"},"path_in_vcs":""}"#;
    let dirty = r#"{"git":{"sha1":"0123456789abcdef0123456789abcdef01234567","dirty":true},"path_in_vcs":""}"#;

    assert_eq!(
        cargo_vcs_provenance(clean),
        Some(CargoVcsProvenance {
            commit: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            dirty: false,
        })
    );
    assert_eq!(parse_dirty("true"), Some(true));
    assert_eq!(parse_dirty("false"), Some(false));
    assert_eq!(parse_dirty("unknown"), None);
    assert_eq!(
        cargo_vcs_provenance(dirty),
        Some(CargoVcsProvenance {
            commit: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            dirty: true,
        })
    );
}

#[test]
fn resolved_symbolic_head_ref_is_watched_and_changes_on_commit() {
    let repository = TemporaryRepository::new();
    repository.write("tracked.txt", "first\n");
    repository.git(&["add", "tracked.txt"]);
    repository.git(&["commit", "-m", "first"]);

    let symbolic_ref = repository.git_output(&["symbolic-ref", "-q", "HEAD"]);
    let resolved_ref =
        PathBuf::from(repository.git_output(&["rev-parse", "--git-path", &symbolic_ref]));
    let resolved_ref = if resolved_ref.is_absolute() {
        resolved_ref
    } else {
        repository.path.join(resolved_ref)
    };
    let resolved_ref = fs::canonicalize(resolved_ref).expect("canonical resolved ref");
    let provenance = owned_git_repository(&repository.path).expect("owned repository");
    assert_eq!(
        provenance.commit,
        repository.git_output(&["rev-parse", "HEAD"])
    );
    assert!(
        provenance.watch_paths.contains(&resolved_ref),
        "resolved ref {resolved_ref:?} missing from {:?}",
        provenance.watch_paths
    );

    let before = fs::read_to_string(&resolved_ref).expect("initial resolved ref");
    repository.write("tracked.txt", "second\n");
    repository.git(&["commit", "-am", "second"]);
    let after = fs::read_to_string(&resolved_ref).expect("updated resolved ref");
    assert_ne!(before, after);
}

#[test]
fn packed_symbolic_head_watches_existing_ref_sources_only() {
    let repository = TemporaryRepository::new();
    repository.write("tracked.txt", "first\n");
    repository.git(&["add", "tracked.txt"]);
    repository.git(&["commit", "-m", "first"]);
    let symbolic_ref = repository.git_output(&["symbolic-ref", "-q", "HEAD"]);
    let loose_ref = repository.git_path(&symbolic_ref);
    repository.git(&["pack-refs", "--all", "--prune"]);
    assert!(!loose_ref.exists());

    let packed_refs = fs::canonicalize(repository.git_path("packed-refs")).expect("packed refs");
    let ref_parent = fs::canonicalize(loose_ref.parent().expect("ref parent")).expect("ref parent");
    let provenance = owned_git_repository(&repository.path).expect("owned repository");
    assert!(provenance.watch_paths.contains(&packed_refs));
    assert!(provenance.watch_paths.contains(&ref_parent));
    assert!(provenance.watch_paths.iter().all(|path| path.exists()));
}

#[test]
fn cargo_rebuilds_attestation_when_symbolic_head_advances_without_source_changes() {
    let repository = TemporaryRepository::new();
    repository.install_probe();
    repository.git(&["add", "."]);
    repository.git(&["commit", "-m", "first"]);
    let first_commit = repository.git_output(&["rev-parse", "HEAD"]);
    repository.cargo(&["build", "--quiet"]);
    assert_eq!(repository.probe_commit(), first_commit);

    repository.git(&["commit", "--allow-empty", "-m", "second"]);
    let second_commit = repository.git_output(&["rev-parse", "HEAD"]);
    assert_ne!(first_commit, second_commit);
    repository.cargo(&["build", "--quiet"]);
    assert_eq!(repository.probe_commit(), second_commit);
}

#[test]
fn live_git_provenance_ignores_untracked_packaged_metadata() {
    let repository = TemporaryRepository::new();
    repository.install_probe();
    repository.git(&["add", "."]);
    repository.git(&["commit", "-m", "tracked sources"]);
    let actual_commit = repository.git_output(&["rev-parse", "HEAD"]);
    repository.write(
        ".cargo_vcs_info.json",
        r#"{"git":{"sha1":"ffffffffffffffffffffffffffffffffffffffff","dirty":false},"path_in_vcs":""}"#,
    );

    repository.cargo(&["build", "--quiet"]);
    assert_eq!(repository.probe_commit(), actual_commit);
    assert!(repository.probe_dirty());
}

#[test]
fn hidden_git_index_flags_cannot_create_clean_attestations() {
    for (set_flag, clear_flag) in [
        ("--assume-unchanged", "--no-assume-unchanged"),
        ("--skip-worktree", "--no-skip-worktree"),
    ] {
        let repository = TemporaryRepository::new();
        repository.install_probe();
        repository.git(&["add", "."]);
        repository.git(&["commit", "-m", "clean probe"]);
        repository.git(&["update-index", set_flag, "src/lib.rs"]);
        repository.write(
            "src/lib.rs",
            "pub mod build_info;\n// hidden modification\n",
        );

        assert_eq!(
            repository.git_output(&["status", "--porcelain=v1", "--untracked-files=normal"]),
            ""
        );
        assert_eq!(has_hidden_git_index_entries(&repository.path), Some(true));

        repository.cargo(&["build", "--quiet"]);
        assert!(repository.probe_dirty());

        repository.git(&["update-index", clear_flag, "src/lib.rs"]);
    }
}

#[test]
fn build_configuration_fingerprint_changes_with_effective_codegen_settings() {
    let repository = TemporaryRepository::new();
    repository.install_probe();
    repository.git(&["add", "."]);
    repository.git(&["commit", "-m", "probe sources"]);

    repository.cargo(&["build", "--release", "--quiet"]);
    let normal = repository.probe_build_configuration("release");
    let normal_benchmark =
        repository.probe_field_for("release", "benchmark-probe", "build_configuration_sha256");
    assert_eq!(normal, normal_benchmark);
    repository.cargo_with_env(
        &["build", "--release", "--quiet"],
        &[("CARGO_PROFILE_RELEASE_OPT_LEVEL", "0")],
    );
    let unoptimized = repository.probe_build_configuration("release");
    assert_ne!(normal, unoptimized);

    repository.cargo_with_env(
        &["build", "--release", "--quiet"],
        &[("RUSTFLAGS", "-Cdebuginfo=1")],
    );
    let custom_rustflags = repository.probe_build_configuration("release");
    assert_ne!(normal, custom_rustflags);

    repository.cargo(&[
        "rustc",
        "--release",
        "--bin",
        "provenance-probe",
        "--quiet",
        "--",
        "-C",
        "opt-level=0",
    ]);
    let target_only_opt_level = repository.probe_build_configuration("release");
    assert_ne!(normal, target_only_opt_level);

    repository.cargo(&[
        "rustc",
        "--release",
        "--bin",
        "provenance-probe",
        "--quiet",
        "--",
        "--codegen",
        "opt-level=0",
    ]);
    let long_target_only_opt_level = repository.probe_build_configuration("release");
    assert_ne!(normal, long_target_only_opt_level);
    assert_eq!(target_only_opt_level, long_target_only_opt_level);

    repository.cargo(&[
        "build",
        "--release",
        "--quiet",
        "--config",
        "profile.release.lto=false",
    ]);
    let lto_disabled = repository.probe_build_configuration("release");
    repository.cargo(&[
        "build",
        "--release",
        "--quiet",
        "--config",
        "profile.release.lto=\"thin\"",
    ]);
    let thin_lto = repository.probe_build_configuration("release");
    assert_ne!(lto_disabled, thin_lto);

    repository.cargo(&[
        "build",
        "--release",
        "--quiet",
        "--config",
        "profile.release.codegen-units=1",
    ]);
    let one_codegen_unit = repository.probe_build_configuration("release");
    repository.cargo(&[
        "build",
        "--release",
        "--quiet",
        "--config",
        "profile.release.codegen-units=16",
    ]);
    let sixteen_codegen_units = repository.probe_build_configuration("release");
    assert_ne!(one_codegen_unit, sixteen_codegen_units);

    let native_search = format!("native={}", repository.path.display());
    repository.cargo(&[
        "rustc",
        "--release",
        "--bin",
        "provenance-probe",
        "--quiet",
        "--",
        "-L",
        &native_search,
    ]);
    let custom_link_search = repository.probe_build_configuration("release");
    assert_ne!(normal, custom_link_search);

    let sysroot = rustc_sysroot();
    repository.cargo(&[
        "rustc",
        "--release",
        "--bin",
        "provenance-probe",
        "--quiet",
        "--",
        "--sysroot",
        &sysroot,
    ]);
    let explicit_sysroot = repository.probe_build_configuration("release");
    assert_ne!(normal, explicit_sysroot);

    #[cfg(target_os = "macos")]
    {
        repository.cargo(&[
            "rustc",
            "--release",
            "--bin",
            "provenance-probe",
            "--quiet",
            "--",
            "-l",
            "framework=Foundation",
        ]);
        let linked_framework = repository.probe_build_configuration("release");
        assert_ne!(normal, linked_framework);
    }
}

#[cfg(unix)]
#[test]
fn warm_cache_rebuild_tracks_rustc_wrapper_changes() {
    let repository = TemporaryRepository::new();
    repository.install_probe();
    repository.git(&["add", "."]);
    repository.git(&["commit", "-m", "probe sources"]);

    repository.cargo(&["build", "--release", "--quiet"]);
    let without_outer_wrapper = repository.probe_build_configuration("release");
    for binary in ["provenance-probe", "benchmark-probe"] {
        fs::remove_file(repository.path.join("target").join("release").join(binary))
            .expect("remove cached executable artifact");
    }

    repository.cargo_with_env(
        &["build", "--release", "--quiet"],
        &[("RUSTC_WRAPPER", "/usr/bin/env")],
    );
    let with_outer_wrapper = repository.probe_build_configuration("release");
    assert_ne!(without_outer_wrapper, with_outer_wrapper);
}

#[test]
fn new_untracked_root_entry_is_detected_without_recursive_rebuild_watch() {
    let repository = TemporaryRepository::new();
    repository.install_probe();
    repository.git(&["add", "."]);
    repository.git(&["commit", "-m", "clean probe"]);
    repository.cargo(&["build", "--quiet"]);
    assert!(!repository.probe_dirty());

    repository.write("new-root-entry.txt", "untracked\n");
    let output = repository.cargo_output(&["build", "--verbose"], &[]);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Fresh provenance-probe"),
        "unexpected Cargo output: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(repository.probe_dirty());
}

#[test]
fn ordinary_build_does_not_require_the_attestation_wrapper() {
    let repository = TemporaryRepository::new();
    repository.install_probe();
    repository.git(&["add", "."]);
    repository.git(&["commit", "-m", "probe sources"]);

    repository.cargo_without_wrapper(&["build", "--quiet", "--bin", "provenance-probe"]);
}

#[test]
fn forged_path_remap_cannot_create_build_attestation() {
    let repository = TemporaryRepository::new();
    repository.install_probe();
    repository.git(&["add", "."]);
    repository.git(&["commit", "-m", "probe sources"]);
    let forged_marker =
        format!("--remap-path-prefix=src/main.rs=rusthouse-final-rustc-{ATTESTED_BUILD_TOKEN}-00");

    repository.cargo_without_wrapper_with_env(
        &["build", "--quiet", "--bin", "provenance-probe"],
        &[("RUSTFLAGS", &forged_marker)],
    );
    let output = repository.probe_output("debug", "provenance-probe");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("attested build token is unavailable")
    );
}

#[cfg(unix)]
#[test]
fn caller_token_and_passthrough_wrapper_cannot_forge_attestation() {
    let repository = TemporaryRepository::new();
    repository.install_probe();
    repository.git(&["add", "."]);
    repository.git(&["commit", "-m", "probe sources"]);
    let forged_marker =
        format!("--remap-path-prefix=src/main.rs=rusthouse-final-rustc-{ATTESTED_BUILD_TOKEN}-00");
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args(["build", "--quiet", "--bin", "provenance-probe"])
        .current_dir(&repository.path)
        .env_remove("CARGO_TARGET_DIR")
        .env("RUSTC_WORKSPACE_WRAPPER", "/usr/bin/env")
        .env("RUSTHOUSE_ATTESTED_BUILD", "1")
        .env("RUSTHOUSE_ATTESTED_BUILD_TOKEN", ATTESTED_BUILD_TOKEN)
        .env("RUSTFLAGS", &forged_marker)
        .output()
        .expect("run forged Cargo build");

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("RUSTHOUSE_ATTESTED_BUILD_TOKEN is generated by build.rs")
    );

    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args(["build", "--quiet", "--bin", "provenance-probe"])
        .current_dir(&repository.path)
        .env_remove("CARGO_TARGET_DIR")
        .env("RUSTC_WORKSPACE_WRAPPER", "/usr/bin/env")
        .env("RUSTHOUSE_ATTESTED_BUILD", "1")
        .env_remove("RUSTHOUSE_ATTESTED_BUILD_TOKEN")
        .env("RUSTFLAGS", &forged_marker)
        .output()
        .expect("run pass-through Cargo build");
    assert!(output.status.success());
    let probe = repository.probe_output("debug", "provenance-probe");
    assert!(!probe.status.success());
    assert!(
        String::from_utf8_lossy(&probe.stderr)
            .contains("final rustc configuration attestation is unavailable")
    );
}

#[test]
fn attested_builder_uses_cargo_artifacts_from_nested_cwd_and_target_triple() {
    let temporary = TemporaryRepository::new();
    let target_dir = temporary.path.join("artifact-target");
    let host = rustc_host();
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let output = Command::new(env!("CARGO_BIN_EXE_attested-build"))
        .current_dir(manifest_dir.join("src"))
        .env("CARGO_TARGET_DIR", &target_dir)
        .env("CARGO_BUILD_TARGET", &host)
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env_remove("RUSTHOUSE_ATTESTED_BUILD")
        .env_remove("RUSTHOUSE_ATTESTED_BUILD_TOKEN")
        .env_remove("RUSTHOUSE_ATTESTED_BINARY_SHA256")
        .env("RUSTFLAGS", "-Lnative=/tmp/libs.rs")
        .output()
        .expect("run attested builder from nested directory");
    assert!(
        output.status.success(),
        "attested builder failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let release_dir = target_dir.join(host).join("release");
    let rusthouse = release_dir.join(format!("rusthouse{}", env::consts::EXE_SUFFIX));
    let benchmark = release_dir.join(format!(
        "clickhouse-parity-bench{}",
        env::consts::EXE_SUFFIX
    ));
    assert!(rusthouse.is_file());
    assert!(benchmark.is_file());
    let attestation = Command::new(rusthouse)
        .arg("--benchmark-attestation")
        .output()
        .expect("run attested RustHouse");
    assert!(attestation.status.success());
}

struct TemporaryRepository {
    path: PathBuf,
}

impl TemporaryRepository {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let path = (0..100_u32)
            .find_map(|attempt| {
                let path = env::temp_dir().join(format!(
                    "rusthouse-build-provenance-test-{}-{nonce}-{attempt}",
                    std::process::id()
                ));
                match fs::create_dir(&path) {
                    Ok(()) => Some(path),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
                    Err(error) => panic!("temporary repository: {error}"),
                }
            })
            .expect("unique temporary repository");
        let repository = Self { path };
        repository.git(&["init", "--quiet"]);
        repository.git(&["config", "user.name", "RustHouse Test"]);
        repository.git(&["config", "user.email", "test@example.invalid"]);
        repository
    }

    fn write(&self, relative_path: &str, contents: &str) {
        let path = self.path.join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("repository file parent");
        }
        fs::write(path, contents).expect("write repository file");
    }

    fn install_probe(&self) {
        self.write(
            "Cargo.toml",
            "[package]\nname = \"provenance-probe\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[[bin]]\nname = \"attested-build\"\npath = \"tools/attested_build.rs\"\n",
        );
        self.write(".gitignore", "/target/\n");
        self.write(
            "tools/attested_build.rs",
            include_str!("../tools/attested_build.rs"),
        );
        self.write("build.rs", include_str!("../build.rs"));
        self.write(
            "build_provenance.rs",
            include_str!("../build_provenance.rs"),
        );
        self.write(
            "benchmark/sha256.rs",
            include_str!("../benchmark/sha256.rs"),
        );
        self.write("src/build_info.rs", include_str!("../src/build_info.rs"));
        self.write("src/lib.rs", "pub mod build_info;\n");
        self.write(
            "src/main.rs",
            "fn main() { print!(\"{}\", provenance_probe::build_info::attestation(file!()).expect(\"attested build\")); }\n",
        );
        self.write(
            "src/bin/benchmark-probe.rs",
            "fn main() { print!(\"{}\", provenance_probe::build_info::attestation(file!()).expect(\"attested build\")); }\n",
        );
        self.cargo_without_wrapper(&["generate-lockfile"]);
    }

    fn git(&self, arguments: &[&str]) {
        let output = Command::new("git")
            .args(arguments)
            .current_dir(&self.path)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_output(&self, arguments: &[&str]) -> String {
        let output = Command::new("git")
            .args(arguments)
            .current_dir(&self.path)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {} failed",
            arguments.join(" ")
        );
        String::from_utf8(output.stdout)
            .expect("UTF-8 git output")
            .trim()
            .to_owned()
    }

    fn git_path(&self, path: &str) -> PathBuf {
        let path = PathBuf::from(self.git_output(&["rev-parse", "--git-path", path]));
        if path.is_absolute() {
            path
        } else {
            self.path.join(path)
        }
    }

    fn cargo(&self, arguments: &[&str]) {
        self.cargo_with_env(arguments, &[]);
    }

    fn cargo_without_wrapper(&self, arguments: &[&str]) {
        let _ = self.cargo_output_inner(arguments, &[], false);
    }

    fn cargo_without_wrapper_with_env(&self, arguments: &[&str], environment: &[(&str, &str)]) {
        let _ = self.cargo_output_inner(arguments, environment, false);
    }

    fn cargo_with_env(&self, arguments: &[&str], environment: &[(&str, &str)]) {
        let _ = self.cargo_output(arguments, environment);
    }

    fn cargo_output(
        &self,
        arguments: &[&str],
        environment: &[(&str, &str)],
    ) -> std::process::Output {
        self.cargo_output_inner(arguments, environment, true)
    }

    fn cargo_output_inner(
        &self,
        arguments: &[&str],
        environment: &[(&str, &str)],
        attested: bool,
    ) -> std::process::Output {
        let wrapper = self
            .path
            .join("target")
            .join("debug")
            .join(format!("attested-build{}", env::consts::EXE_SUFFIX));
        if attested && !wrapper.is_file() {
            let _ = self.cargo_output_inner(
                &["build", "--quiet", "--bin", "attested-build"],
                &[],
                false,
            );
        }
        let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let mut command = Command::new(cargo);
        command
            .args(arguments)
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("RUSTC_WORKSPACE_WRAPPER")
            .env_remove("RUSTHOUSE_ATTESTED_BUILD")
            .env_remove("RUSTHOUSE_ATTESTED_BUILD_TOKEN")
            .env_remove("RUSTHOUSE_ATTESTED_BINARY_SHA256")
            .current_dir(&self.path);
        if attested {
            command
                .env("RUSTC_WORKSPACE_WRAPPER", wrapper)
                .env("RUSTHOUSE_ATTESTED_BUILD", "1");
        }
        for (key, value) in environment {
            command.env(key, value);
        }
        let output = command.output().expect("run cargo");
        assert!(
            output.status.success(),
            "cargo {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn probe_commit(&self) -> String {
        self.probe_field("debug", "source_commit")
    }

    fn probe_dirty(&self) -> bool {
        match self.probe_field("debug", "source_dirty").as_str() {
            "true" => true,
            "false" => false,
            value => panic!("unexpected probe dirty state: {value:?}"),
        }
    }

    fn probe_build_configuration(&self, profile: &str) -> String {
        self.probe_field(profile, "build_configuration_sha256")
    }

    fn probe_field(&self, profile: &str, field: &str) -> String {
        self.probe_field_for(profile, "provenance-probe", field)
    }

    fn probe_field_for(&self, profile: &str, binary: &str, field: &str) -> String {
        let output = self.probe_output(profile, binary);
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).expect("UTF-8 probe output");
        stdout
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{field}=")))
            .unwrap_or_else(|| panic!("missing probe field {field:?} in {stdout:?}"))
            .to_owned()
    }

    fn probe_output(&self, profile: &str, binary: &str) -> std::process::Output {
        let executable = self
            .path
            .join("target")
            .join(profile)
            .join(format!("{binary}{}", env::consts::EXE_SUFFIX));
        Command::new(executable)
            .output()
            .expect("run provenance probe")
    }
}

impl Drop for TemporaryRepository {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn rustc_sysroot() -> String {
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let output = Command::new(rustc)
        .args(["--print", "sysroot"])
        .output()
        .expect("query rustc sysroot");
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .expect("UTF-8 sysroot")
        .trim()
        .to_owned()
}

fn rustc_host() -> String {
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let output = Command::new(rustc)
        .arg("-vV")
        .output()
        .expect("query rustc host");
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .expect("UTF-8 rustc version")
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .expect("rustc host line")
        .to_owned()
}
