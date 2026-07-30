#![cfg(not(any(target_os = "linux", target_os = "macos")))]

#[test]
fn persistent_databases_are_rejected_on_unsupported_platforms() {
    let error = rusthouse::Database::open("unsupported.rsh").expect_err("persistence is rejected");
    assert!(matches!(error, rusthouse::Error::Persistence { .. }));
    assert!(
        error
            .to_string()
            .contains("supported only on Linux and macOS")
    );
}
