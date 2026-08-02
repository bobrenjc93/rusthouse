#![forbid(unsafe_code)]
#![deny(
    missing_docs,
    rust_2018_idioms,
    unreachable_pub,
    clippy::dbg_macro,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented,
    clippy::unwrap_used
)]

//! Minimal command-line entry point for RustHouse metadata and usage.

use std::env;
use std::process::ExitCode;

const HELP: &str = "RustHouse - a compact analytical database foundation

Usage: rusthouse [OPTIONS]

Options:
  -h, --help       Print help
  -V, --version    Print version

SQL execution is not available in the command-line interface yet.
";

fn main() -> ExitCode {
    let arguments: Vec<_> = env::args_os().skip(1).collect();

    match arguments.as_slice() {
        [] => {
            print!("{HELP}");
            ExitCode::SUCCESS
        }
        [argument] if argument == "-h" || argument == "--help" => {
            print!("{HELP}");
            ExitCode::SUCCESS
        }
        [argument] if argument == "-V" || argument == "--version" => {
            println!("rusthouse {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("unsupported arguments; run 'rusthouse --help' for usage");
            ExitCode::FAILURE
        }
    }
}
