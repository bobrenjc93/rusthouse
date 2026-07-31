#[path = "../build_provenance.rs"]
mod build_provenance;

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use build_provenance::{
    CargoVcsProvenance, cargo_vcs_provenance, owned_git_repository, parse_dirty,
};

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
fn build_configuration_fingerprint_changes_with_effective_codegen_settings() {
    let repository = TemporaryRepository::new();
    repository.install_probe();
    repository.git(&["add", "."]);
    repository.git(&["commit", "-m", "probe sources"]);

    repository.cargo(&["build", "--release", "--quiet"]);
    let normal = repository.probe_build_configuration("release");
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
            "[package]\nname = \"provenance-probe\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        );
        self.write(".gitignore", "/target/\n");
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
        self.write(
            "src/main.rs",
            "mod build_info; fn main() { print!(\"{}\", build_info::attestation()); }\n",
        );
        self.cargo(&["generate-lockfile"]);
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

    fn cargo_with_env(&self, arguments: &[&str], environment: &[(&str, &str)]) {
        let _ = self.cargo_output(arguments, environment);
    }

    fn cargo_output(
        &self,
        arguments: &[&str],
        environment: &[(&str, &str)],
    ) -> std::process::Output {
        let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let mut command = Command::new(cargo);
        command
            .args(arguments)
            .env_remove("CARGO_TARGET_DIR")
            .current_dir(&self.path);
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
        let executable = self
            .path
            .join("target")
            .join(profile)
            .join(format!("provenance-probe{}", env::consts::EXE_SUFFIX));
        let output = Command::new(executable)
            .output()
            .expect("run provenance probe");
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).expect("UTF-8 probe output");
        stdout
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{field}=")))
            .unwrap_or_else(|| panic!("missing probe field {field:?} in {stdout:?}"))
            .to_owned()
    }
}

impl Drop for TemporaryRepository {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
