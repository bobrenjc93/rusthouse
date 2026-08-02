use std::process::Command;

#[test]
fn help_and_version_describe_the_supported_cli() {
    let help = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("Usage: rusthouse [OPTIONS]"));
    assert!(help.contains("SQL execution is not available"));

    let version = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8(version.stdout).unwrap(),
        format!("rusthouse {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn unsupported_cli_arguments_fail() {
    let output = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .arg("SELECT 1")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("unsupported arguments")
    );
}
