use std::error::Error as StdError;
use std::io::{self, Read, Write};
use std::process::ExitCode;

use rusthouse::output::{OutputFormat, write_result};
use rusthouse::{Engine, EngineConfig, StatementResult};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
            {
                return ExitCode::SUCCESS;
            }
            eprintln!("rusthouse: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> std::result::Result<(), Box<dyn StdError>> {
    let (format, config) = parse_args(std::env::args().skip(1))?;
    let mut input = Vec::new();
    io::stdin()
        .take(config.max_input_bytes.saturating_add(1) as u64)
        .read_to_end(&mut input)?;
    if input.len() > config.max_input_bytes {
        return Err(rusthouse::Error::ResourceLimit {
            resource: "SQL input bytes",
            limit: config.max_input_bytes,
            actual: input.len(),
        }
        .into());
    }
    let sql =
        std::str::from_utf8(&input).map_err(|error| format!("stdin is not UTF-8: {error}"))?;
    let mut engine = Engine::new(config);
    let stdout = io::stdout();
    let mut writer = io::BufWriter::new(stdout.lock());
    let json_batch = format == OutputFormat::Json;
    let mut first_query = true;
    if json_batch {
        writer.write_all(b"[")?;
    }
    for result in engine.execute_iter(sql)? {
        let result = result?;
        if let StatementResult::Query(result) = result {
            if json_batch && !first_query {
                writer.write_all(b",")?;
            }
            write_result(&mut writer, &result, format)?;
            first_query = false;
        }
    }
    if json_batch {
        writer.write_all(b"]\n")?;
    }
    writer.flush()?;
    Ok(())
}

fn parse_args(
    mut args: impl Iterator<Item = String>,
) -> std::result::Result<(OutputFormat, EngineConfig), String> {
    let mut format = OutputFormat::Table;
    let mut config = EngineConfig::default();
    while let Some(argument) = args.next() {
        let (flag, inline_value) = argument
            .split_once('=')
            .map_or((argument.as_str(), None), |(flag, value)| {
                (flag, Some(value))
            });
        match flag {
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            "--format" => {
                let value = option_value("--format", inline_value, &mut args)?;
                format = OutputFormat::parse(&value)
                    .ok_or_else(|| format!("unknown output format: {value}"))?;
            }
            "--max-input-bytes" => {
                config.max_input_bytes =
                    parse_size_option(flag, &option_value(flag, inline_value, &mut args)?)?;
            }
            "--max-rows-per-insert" => {
                config.max_rows_per_insert =
                    parse_size_option(flag, &option_value(flag, inline_value, &mut args)?)?;
            }
            "--max-rows-per-table" => {
                config.max_rows_per_table =
                    parse_size_option(flag, &option_value(flag, inline_value, &mut args)?)?;
            }
            "--max-result-rows" => {
                config.max_result_rows =
                    parse_size_option(flag, &option_value(flag, inline_value, &mut args)?)?;
            }
            "--max-batch-result-bytes" => {
                config.max_batch_result_bytes =
                    parse_size_option(flag, &option_value(flag, inline_value, &mut args)?)?;
            }
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    Ok((format, config))
}

fn option_value(
    flag: &str,
    inline: Option<&str>,
    args: &mut impl Iterator<Item = String>,
) -> std::result::Result<String, String> {
    if let Some(value) = inline {
        return Ok(value.to_owned());
    }
    args.next()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn parse_size_option(flag: &str, value: &str) -> std::result::Result<usize, String> {
    value
        .parse::<usize>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| format!("{flag} requires a positive integer, found {value}"))
}

fn print_help() {
    println!(
        "{name} - in-memory analytical SQL engine\n\n\
         Usage: {binary} [OPTIONS] < queries.sql\n\n\
         Options:\n  \
           --format <table|csv|json>       Output format (default: table)\n  \
           --max-input-bytes <N>           Maximum SQL input size\n  \
           --max-rows-per-insert <N>       Maximum rows in one INSERT\n  \
           --max-rows-per-table <N>        Maximum rows stored in a table\n  \
           --max-result-rows <N>           Maximum emitted rows per SELECT\n  \
           --max-batch-result-bytes <N>    Maximum retained/intermediate result bytes\n  \
         -h, --help                        Print help",
        name = rusthouse::product_name(),
        binary = env!("CARGO_PKG_NAME")
    );
}
