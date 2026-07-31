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
    repository.write(
        "Cargo.toml",
        "[package]\nname = \"provenance-probe\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    );
    repository.write(".gitignore", "/target/\n");
    repository.write("build.rs", include_str!("../build.rs"));
    repository.write(
        "build_provenance.rs",
        include_str!("../build_provenance.rs"),
    );
    repository.write(
        "src/main.rs",
        "fn main() { println!(\"{}\", env!(\"RUSTHOUSE_SOURCE_COMMIT\")); }\n",
    );
    repository.cargo(&["generate-lockfile"]);
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

struct TemporaryRepository {
    path: PathBuf,
}

impl TemporaryRepository {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "rusthouse-build-provenance-test-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("temporary repository");
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
        let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let output = Command::new(cargo)
            .args(arguments)
            .env_remove("CARGO_TARGET_DIR")
            .current_dir(&self.path)
            .output()
            .expect("run cargo");
        assert!(
            output.status.success(),
            "cargo {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn probe_commit(&self) -> String {
        let executable = self
            .path
            .join("target/debug")
            .join(format!("provenance-probe{}", env::consts::EXE_SUFFIX));
        let output = Command::new(executable)
            .output()
            .expect("run provenance probe");
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .expect("UTF-8 probe output")
            .trim()
            .to_owned()
    }
}

impl Drop for TemporaryRepository {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
