use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

static TEMP_FILE_ID: AtomicU64 = AtomicU64::new(0);

fn evidence_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benchmark/results/default-20260729.json")
}

fn verifier(path: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_clickhouse-parity-bench"))
        .args(["--verify-details", path.to_str().expect("UTF-8 test path")])
        .env("RUSTHOUSE_CLICKHOUSE_BIN", "/definitely/missing/clickhouse")
        .env("RUSTHOUSE_BIN", "/definitely/missing/rusthouse")
        .output()
        .expect("run details verifier")
}

fn read_details() -> Value {
    serde_json::from_str(&fs::read_to_string(evidence_path()).expect("read retained evidence"))
        .expect("parse retained evidence")
}

fn temp_details(details: &Value) -> PathBuf {
    let id = TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "rusthouse-benchmark-verifier-{}-{id}.json",
        std::process::id()
    ));
    fs::write(
        &path,
        serde_json::to_vec(details).expect("serialize test details"),
    )
    .expect("write test details");
    path
}

fn assert_rejected(details: &Value, expected_error: &str) {
    let path = temp_details(details);
    let output = verifier(&path);
    fs::remove_file(path).expect("remove test details");
    assert!(!output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).expect("failure JSON report");
    let evidence = report["evidence"]
        .as_array()
        .expect("failure evidence array")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        evidence.contains(expected_error),
        "expected {expected_error:?} in {evidence:?}"
    );
}

#[test]
fn checked_in_evidence_verifies_without_engine_executables() {
    let output = verifier(&evidence_path());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("success JSON report");
    let reported_score = report["score"].as_f64().expect("report score");
    let retained_score = read_details()["score"].as_f64().expect("retained score");
    assert!((reported_score - retained_score).abs() < 0.000_001);
    assert!(
        report["summary"]
            .as_str()
            .expect("summary")
            .contains("24 canonical cases")
    );
}

#[test]
fn tampering_fixtures_are_rejected() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for fixture in [
        "changed-sample.json",
        "changed-score.json",
        "changed-configuration.json",
        "changed-case-identity.json",
    ] {
        let patch: Value = serde_json::from_str(
            &fs::read_to_string(manifest.join("benchmark/fixtures").join(fixture))
                .expect("read tampering fixture"),
        )
        .expect("parse tampering fixture");
        let mut details = read_details();
        let pointer = patch["pointer"].as_str().expect("fixture pointer");
        *details
            .pointer_mut(pointer)
            .unwrap_or_else(|| panic!("fixture pointer {pointer:?}")) =
            patch["replacement"].clone();
        assert_rejected(
            &details,
            patch["expected_error"].as_str().expect("expected error"),
        );
    }
}

#[test]
fn duplicate_and_missing_cases_are_rejected() {
    let mut duplicate = read_details();
    let cases = duplicate["cases"].as_array_mut().expect("cases");
    cases[1] = cases[0].clone();
    assert_rejected(&duplicate, "duplicate benchmark case identity");

    let mut missing = read_details();
    missing["cases"].as_array_mut().expect("cases").pop();
    assert_rejected(&missing, "details.cases count mismatch");
}

#[test]
fn duplicate_json_fields_are_rejected() {
    let details = fs::read_to_string(evidence_path()).expect("read retained evidence");
    let tampered = details.replacen("\"score\":", "\"score\":1,\"score\":", 1);
    let id = TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "rusthouse-benchmark-verifier-duplicate-field-{}-{id}.json",
        std::process::id()
    ));
    fs::write(&path, tampered).expect("write duplicate-field details");
    let output = verifier(&path);
    fs::remove_file(path).expect("remove duplicate-field details");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("duplicate JSON object field"));
}
