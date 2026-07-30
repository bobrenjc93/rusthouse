use std::process::Command;

#[test]
fn oversized_seed_count_emits_structured_failure_instead_of_panicking() {
    let output = Command::new(env!("CARGO_BIN_EXE_clickhouse-parity-bench"))
        .arg("--seed-count=18446744073709551615")
        .output()
        .expect("run benchmark CLI");

    assert!(!output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    assert_eq!(stdout.lines().count(), 1);
    assert!(stdout.starts_with("{\"score\":0.000000,"));
    assert!(stdout.contains("invalid seed count"));
    assert!(stdout.contains("between 1 and 64"));
    assert!(output.stderr.is_empty());
}
