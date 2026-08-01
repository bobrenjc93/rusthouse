use std::io::{self, Read};
use std::process::ExitCode;

use rusthouse::{CsvWriter, Database, Error, MAX_INPUT_BYTES, MAX_OUTPUT_BYTES, Result};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(Error::Io(error)) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rusthouse: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let mut format = "csv".to_owned();
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--format" => {
                format = arguments
                    .next()
                    .ok_or_else(|| Error::Execution("--format requires a value".to_owned()))?;
            }
            "--format=csv" => format = "csv".to_owned(),
            "--help" | "-h" => {
                println!(
                    "{}\n\nUsage: rusthouse [--format csv]\n\nReads a SQL script from stdin and writes SELECT results as CSVWithNames.",
                    rusthouse::product_name()
                );
                return Ok(());
            }
            _ => {
                return Err(Error::Execution(format!(
                    "unknown command-line argument '{argument}'"
                )));
            }
        }
    }
    if !format.eq_ignore_ascii_case("csv") {
        return Err(Error::Execution(format!(
            "unsupported output format '{format}'"
        )));
    }

    let mut input = Vec::new();
    io::stdin()
        .lock()
        .take((MAX_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut input)?;
    if input.len() > MAX_INPUT_BYTES {
        return Err(Error::Limit {
            resource: "SQL input bytes",
            limit: MAX_INPUT_BYTES,
        });
    }
    let sql = std::str::from_utf8(&input).map_err(|error| Error::Parse {
        position: error.valid_up_to(),
        message: "SQL input is not valid UTF-8".to_owned(),
    })?;

    let mut database = Database::new();
    let results = database.execute(sql)?;
    let stdout = io::stdout();
    let mut writer = CsvWriter::new(stdout.lock(), MAX_OUTPUT_BYTES);
    for result in &results {
        writer.write_result(result)?;
    }
    writer.flush()
}
