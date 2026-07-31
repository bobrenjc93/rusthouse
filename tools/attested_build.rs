use std::env;
use std::fmt::Write as _;
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
    command_status(
        Command::new(cargo)
            .args([
                "build",
                "--release",
                "--bin",
                "rusthouse",
                "--bin",
                "clickhouse-parity-bench",
            ])
            .env("RUSTC_WORKSPACE_WRAPPER", wrapper),
    )
}

fn wrap_rustc(arguments: &[String]) -> Result<i32, String> {
    let rustc = arguments
        .first()
        .ok_or_else(|| "rustc path is unavailable".to_owned())?;
    let rustc_arguments = &arguments[1..];
    let mut command = Command::new(rustc);
    command.args(rustc_arguments);
    if let Some((source_path, configuration)) = final_configuration(rustc_arguments)? {
        command.arg(format!(
            "--remap-path-prefix={source_path}={MARKER_PREFIX}{}",
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
            "-C" | "--codegen" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or_else(|| format!("rustc argument {argument} is missing its value"))?;
                record_codegen(&mut configuration, value);
            }
            "-Z" | "--cfg" | "--target" | "--crate-type" | "--edition" | "--check-cfg" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or_else(|| format!("rustc argument {argument} is missing its value"))?;
                configuration.push(format!("{argument}={value}"));
            }
            value if value.starts_with("--codegen=") => {
                record_codegen(&mut configuration, &value["--codegen=".len()..]);
            }
            value if value.starts_with("-C") => {
                record_codegen(&mut configuration, &value[2..]);
            }
            value
                if value.starts_with("-Z")
                    || value == "-O"
                    || value == "-g"
                    || value == "--test"
                    || value.starts_with("--cfg=")
                    || value.starts_with("--target=")
                    || value.starts_with("--crate-type=")
                    || value.starts_with("--edition=")
                    || value.starts_with("--check-cfg=") =>
            {
                configuration.push(value.to_owned());
            }
            value if value.starts_with('@') => {
                return Err("rustc response files are unsupported by build attestation".to_owned());
            }
            value if value.ends_with(".rs") => source_path = Some(value.to_owned()),
            _ => {}
        }
        index += 1;
    }

    Ok(source_path.map(|source_path| (source_path, configuration.join("\n"))))
}

fn record_codegen(configuration: &mut Vec<String>, value: &str) {
    if !matches!(
        value.split_once('=').map(|(name, _)| name),
        Some("metadata" | "extra-filename" | "incremental")
    ) {
        configuration.push(format!("-C{value}"));
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
    use super::final_configuration;

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
