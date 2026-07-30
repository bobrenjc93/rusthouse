use std::fs;
use std::io::{BufRead, Write};

use rusthouse::Database;
use rusthouse::format::OutputFormat;

use super::emit_results;

pub(crate) fn run(
    mut input: impl BufRead,
    mut stdout: impl Write,
    mut stderr: impl Write,
    initial_format: OutputFormat,
) -> Result<(), String> {
    let mut database = Database::new();
    let mut format = initial_format;
    let mut buffer = String::new();

    loop {
        if is_ignorable_sql(&buffer) {
            buffer.clear();
        }
        let prompt = if buffer.is_empty() {
            "rusthouse> "
        } else {
            "        -> "
        };
        write!(stderr, "{prompt}")
            .and_then(|()| stderr.flush())
            .map_err(|error| format!("could not write prompt: {error}"))?;

        let mut line = String::new();
        let bytes_read = input
            .read_line(&mut line)
            .map_err(|error| format!("could not read SQL from stdin: {error}"))?;
        if bytes_read == 0 {
            execute_trailing_sql(&mut database, &buffer, format, &mut stdout, &mut stderr)?;
            return Ok(());
        }

        if line.trim() == "\\q" && !has_open_string_literal(&buffer) {
            return Ok(());
        }

        if buffer.is_empty() && line.trim_start().starts_with('\\') {
            if handle_command(
                line.trim(),
                &mut database,
                &mut format,
                &mut stdout,
                &mut stderr,
            )? {
                return Ok(());
            }
            continue;
        }

        buffer.push_str(&line);
        for statement in take_complete_statements(&mut buffer) {
            execute_sql(&mut database, &statement, format, &mut stdout, &mut stderr)?;
        }
    }
}

fn handle_command(
    line: &str,
    database: &mut Database,
    format: &mut OutputFormat,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> Result<bool, String> {
    let mut parts = line.splitn(2, char::is_whitespace);
    let command = parts.next().unwrap_or_default();
    let argument = parts.next().unwrap_or_default().trim();

    match command {
        "\\q" if argument.is_empty() => Ok(true),
        "\\q" => {
            write_error(stderr, "\\q does not accept an argument")?;
            Ok(false)
        }
        "\\format" if argument.is_empty() => {
            writeln!(stderr, "format: {}", format_name(*format))
                .map_err(|error| format!("could not write output: {error}"))?;
            Ok(false)
        }
        "\\format" => {
            if let Some(new_format) = OutputFormat::parse(argument) {
                *format = new_format;
            } else {
                write_error(
                    stderr,
                    &format!("unknown output format '{argument}'; expected table, csv, or json"),
                )?;
            }
            Ok(false)
        }
        "\\read" if argument.is_empty() => {
            write_error(stderr, "\\read requires a file path")?;
            Ok(false)
        }
        "\\read" => {
            let path = unquote_path(argument);
            match fs::read_to_string(path) {
                Ok(mut sql) => {
                    for statement in take_complete_statements(&mut sql) {
                        execute_sql(database, &statement, *format, stdout, stderr)?;
                    }
                    execute_trailing_sql(database, &sql, *format, stdout, stderr)?;
                }
                Err(error) => {
                    write_error(stderr, &format!("could not read '{path}': {error}"))?;
                }
            }
            Ok(false)
        }
        _ => {
            write_error(stderr, &format!("unknown command '{command}'"))?;
            Ok(false)
        }
    }
}

fn execute_trailing_sql(
    database: &mut Database,
    sql: &str,
    format: OutputFormat,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> Result<(), String> {
    if !is_ignorable_sql(sql) {
        execute_sql(database, sql, format, stdout, stderr)?;
    }
    Ok(())
}

fn execute_sql(
    database: &mut Database,
    sql: &str,
    format: OutputFormat,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> Result<(), String> {
    if is_ignorable_sql(sql) {
        return Ok(());
    }

    match database.execute(sql) {
        Ok(results) => emit_results(results, format, false, stdout, stderr)
            .map_err(|error| format!("could not write output: {error}")),
        Err(error) => write_error(stderr, &error.to_string()),
    }
}

fn write_error(stderr: &mut impl Write, message: &str) -> Result<(), String> {
    writeln!(stderr, "error: {message}").map_err(|error| format!("could not write output: {error}"))
}

fn format_name(format: OutputFormat) -> &'static str {
    match format {
        OutputFormat::Table => "table",
        OutputFormat::Csv => "csv",
        OutputFormat::Json => "json",
    }
}

fn unquote_path(path: &str) -> &str {
    if path.len() >= 2
        && ((path.starts_with('\'') && path.ends_with('\''))
            || (path.starts_with('"') && path.ends_with('"')))
    {
        &path[1..path.len() - 1]
    } else {
        path
    }
}

fn is_ignorable_sql(sql: &str) -> bool {
    let mut characters = sql.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            value if value.is_whitespace() || value == ';' => {}
            '-' if characters.peek() == Some(&'-') => {
                characters.next();
                for comment_character in characters.by_ref() {
                    if comment_character == '\n' {
                        break;
                    }
                }
            }
            _ => return false,
        }
    }
    true
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SqlScanState {
    Normal,
    StringLiteral,
    LineComment,
}

fn scan_sql(sql: &str, mut on_statement_end: impl FnMut(usize)) -> SqlScanState {
    let bytes = sql.as_bytes();
    let mut index = 0;
    let mut state = SqlScanState::Normal;

    while index < bytes.len() {
        match state {
            SqlScanState::StringLiteral => {
                if bytes[index] == b'\'' {
                    if bytes.get(index + 1) == Some(&b'\'') {
                        index += 2;
                        continue;
                    }
                    state = SqlScanState::Normal;
                }
                index += 1;
            }
            SqlScanState::LineComment => {
                if bytes[index] == b'\n' {
                    state = SqlScanState::Normal;
                }
                index += 1;
            }
            SqlScanState::Normal => match bytes[index] {
                b'\'' => {
                    state = SqlScanState::StringLiteral;
                    index += 1;
                }
                b'-' if bytes.get(index + 1) == Some(&b'-') => {
                    state = SqlScanState::LineComment;
                    index += 2;
                }
                b';' => {
                    on_statement_end(index);
                    index += 1;
                }
                _ => index += 1,
            },
        }
    }
    state
}

fn has_open_string_literal(sql: &str) -> bool {
    scan_sql(sql, |_| {}) == SqlScanState::StringLiteral
}

fn take_complete_statements(buffer: &mut String) -> Vec<String> {
    let mut statement_ends = Vec::new();
    scan_sql(buffer, |index| statement_ends.push(index));

    let mut statements = Vec::with_capacity(statement_ends.len());
    let mut statement_start = 0;
    for statement_end in statement_ends {
        statements.push(buffer[statement_start..=statement_end].to_owned());
        statement_start = statement_end + 1;
    }

    if statement_start > 0 {
        buffer.drain(..statement_start);
    }
    statements
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_only_semicolons_outside_strings_and_comments() {
        let mut buffer = "INSERT INTO t VALUES ('one;''two'); -- ignored ;\nSELECT".to_owned();

        assert_eq!(
            take_complete_statements(&mut buffer),
            vec!["INSERT INTO t VALUES ('one;''two');"]
        );
        assert_eq!(buffer, " -- ignored ;\nSELECT");

        buffer.push_str(" * FROM t;");
        assert_eq!(
            take_complete_statements(&mut buffer),
            vec![" -- ignored ;\nSELECT * FROM t;"]
        );
        assert!(buffer.is_empty());
    }

    #[test]
    fn recognizes_only_whitespace_comments_and_separators_as_ignorable() {
        assert!(is_ignorable_sql(" \t;\n;; -- comment ;\n -- final"));
        assert!(is_ignorable_sql("\u{2003}-- Unicode whitespace"));
        assert!(!is_ignorable_sql("-"));
        assert!(!is_ignorable_sql("SELECT"));
        assert!(!is_ignorable_sql("'-- not a comment'"));
    }

    #[test]
    fn detects_open_strings_without_treating_comments_or_escaped_quotes_as_code() {
        assert!(has_open_string_literal("SELECT 'before'' quote\n\\q"));
        assert!(!has_open_string_literal("-- 'ignored\nSELECT 'closed'"));
    }
}
