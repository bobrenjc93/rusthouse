use std::io::{self, Read, Write};

use rusthouse::{Engine, OutputFormat, render};

const MAX_INPUT_BYTES: u64 = 64 * 1024 * 1024;

fn main() {
    if let Err(error) = run() {
        eprintln!("rusthouse: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let Some(format) = arguments()? else {
        print_help()?;
        return Ok(());
    };

    let mut input = Vec::new();
    io::stdin()
        .lock()
        .take(MAX_INPUT_BYTES + 1)
        .read_to_end(&mut input)?;
    if input.len() as u64 > MAX_INPUT_BYTES {
        return Err(format!(
            "SQL input exceeds the {} MiB limit",
            MAX_INPUT_BYTES / 1024 / 1024
        )
        .into());
    }
    let sql = String::from_utf8(input).map_err(|_| "SQL input must be valid UTF-8")?;
    let mut engine = Engine::new();
    let results = engine.execute(&sql)?;

    let stdout = io::stdout();
    let mut output = stdout.lock();
    for result in results {
        let encoded = render(&result, format)?;
        if let Err(error) = output.write_all(encoded.as_bytes()) {
            if error.kind() == io::ErrorKind::BrokenPipe {
                return Ok(());
            }
            return Err(error.into());
        }
    }
    Ok(())
}

fn arguments() -> Result<Option<OutputFormat>, Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    let mut format = OutputFormat::Csv;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "-h" | "--help" => return Ok(None),
            "--format" => {
                let value = arguments.next().ok_or("--format requires csv or json")?;
                format = OutputFormat::parse(&value)
                    .ok_or_else(|| format!("unsupported format '{value}'; expected csv or json"))?;
            }
            _ if argument.starts_with("--format=") => {
                let value = &argument["--format=".len()..];
                format = OutputFormat::parse(value)
                    .ok_or_else(|| format!("unsupported format '{value}'; expected csv or json"))?;
            }
            _ => return Err(format!("unknown argument '{argument}'; try --help").into()),
        }
    }
    Ok(Some(format))
}

fn print_help() -> io::Result<()> {
    writeln!(
        io::stdout().lock(),
        "{} - compact in-memory analytical SQL engine\n\nUSAGE:\n    rusthouse [--format csv|json]\n\nOPTIONS:\n    --format <FORMAT>  Result format: csv (default) or json\n    -h, --help         Print help\n\nReads one or more semicolon-delimited SQL statements from standard input.",
        rusthouse::product_name()
    )
}
