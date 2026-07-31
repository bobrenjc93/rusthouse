//! Build provenance embedded by `build.rs` in every shipped binary.

#[allow(dead_code)]
#[path = "../benchmark/sha256.rs"]
mod sha256;

use std::any::TypeId;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

pub const ATTESTATION_VERSION: &str = "rusthouse-build-attestation-v2";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildInfo {
    pub source_commit: &'static str,
    pub source_dirty: bool,
    pub rustc_version: &'static str,
    pub target: &'static str,
    pub profile: &'static str,
    pub build_configuration_sha256: &'static str,
}

pub fn current(final_rustc_configuration_marker: &'static str) -> Result<BuildInfo, String> {
    let embedded_dirty = match env!("RUSTHOUSE_SOURCE_DIRTY") {
        "true" => true,
        "false" => false,
        _ => panic!("invalid embedded dirty state"),
    };
    let source_dirty = embedded_dirty
        || match env!("RUSTHOUSE_LIVE_GIT_SOURCE") {
            "true" => live_git_dirty().unwrap_or(true),
            "false" => false,
            _ => panic!("invalid embedded source kind"),
        };
    Ok(BuildInfo {
        source_commit: env!("RUSTHOUSE_SOURCE_COMMIT"),
        source_dirty,
        rustc_version: env!("RUSTHOUSE_RUSTC_VERSION"),
        target: env!("RUSTHOUSE_BUILD_TARGET"),
        profile: env!("RUSTHOUSE_BUILD_PROFILE"),
        build_configuration_sha256: build_configuration_sha256(final_rustc_configuration_marker)?,
    })
}

struct BuildConfigurationIdentity;

fn build_configuration_sha256(
    final_rustc_configuration_marker: &'static str,
) -> Result<&'static str, String> {
    static SHA256: OnceLock<String> = OnceLock::new();
    let encoded_configuration = final_rustc_configuration_marker
        .strip_prefix("rusthouse-final-rustc-")
        .filter(|encoded| {
            !encoded.is_empty()
                && encoded.len() % 2 == 0
                && encoded
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        .ok_or_else(|| "final rustc configuration attestation is unavailable".to_owned())?;
    Ok(SHA256
        .get_or_init(|| {
            let canonical = format!(
                "seed_sha256={}\ntarget_crate_type_id={:?}\nfinal_rustc_configuration={}\n",
                env!("RUSTHOUSE_BUILD_CONFIGURATION_SHA256"),
                TypeId::of::<BuildConfigurationIdentity>(),
                encoded_configuration,
            );
            sha256::digest_hex(canonical.as_bytes())
        })
        .as_str())
}

pub fn attestation(final_rustc_configuration_marker: &'static str) -> Result<String, String> {
    let info = current(final_rustc_configuration_marker)?;
    let mut output = String::new();
    writeln!(output, "{ATTESTATION_VERSION}").expect("writing to String cannot fail");
    writeln!(output, "source_commit={}", info.source_commit)
        .expect("writing to String cannot fail");
    writeln!(output, "source_dirty={}", info.source_dirty).expect("writing to String cannot fail");
    writeln!(output, "rustc_version={}", info.rustc_version)
        .expect("writing to String cannot fail");
    writeln!(output, "target={}", info.target).expect("writing to String cannot fail");
    writeln!(output, "profile={}", info.profile).expect("writing to String cannot fail");
    writeln!(
        output,
        "build_configuration_sha256={}",
        info.build_configuration_sha256
    )
    .expect("writing to String cannot fail");
    Ok(output)
}

fn live_git_dirty() -> Option<bool> {
    let source_root = Path::new(env!("RUSTHOUSE_SOURCE_ROOT"));
    let source_root = fs::canonicalize(source_root).ok()?;
    let top_level = git_output(&source_root, &["rev-parse", "--show-toplevel"])?;
    if fs::canonicalize(top_level).ok()? != source_root {
        return None;
    }
    let status = git_output(
        &source_root,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "--untracked-files=normal",
        ],
    )?;
    Some(!status.is_empty())
}

fn git_output(directory: &Path, arguments: &[&str]) -> Option<String> {
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
