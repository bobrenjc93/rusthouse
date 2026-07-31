//! Build provenance embedded by `build.rs` in every shipped binary.

use std::fmt::Write as _;

pub const ATTESTATION_VERSION: &str = "rusthouse-build-attestation-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildInfo {
    pub source_commit: &'static str,
    pub source_dirty: bool,
    pub rustc_version: &'static str,
    pub target: &'static str,
    pub profile: &'static str,
}

pub fn current() -> BuildInfo {
    BuildInfo {
        source_commit: env!("RUSTHOUSE_SOURCE_COMMIT"),
        source_dirty: match env!("RUSTHOUSE_SOURCE_DIRTY") {
            "true" => true,
            "false" => false,
            _ => panic!("invalid embedded dirty state"),
        },
        rustc_version: env!("RUSTHOUSE_RUSTC_VERSION"),
        target: env!("RUSTHOUSE_BUILD_TARGET"),
        profile: env!("RUSTHOUSE_BUILD_PROFILE"),
    }
}

pub fn attestation() -> String {
    let info = current();
    let mut output = String::new();
    writeln!(output, "{ATTESTATION_VERSION}").expect("writing to String cannot fail");
    writeln!(output, "source_commit={}", info.source_commit)
        .expect("writing to String cannot fail");
    writeln!(output, "source_dirty={}", info.source_dirty).expect("writing to String cannot fail");
    writeln!(output, "rustc_version={}", info.rustc_version)
        .expect("writing to String cannot fail");
    writeln!(output, "target={}", info.target).expect("writing to String cannot fail");
    writeln!(output, "profile={}", info.profile).expect("writing to String cannot fail");
    output
}
