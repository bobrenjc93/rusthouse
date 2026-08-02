use std::fmt::Write;

use crate::SelectStatement;

/// Renders every result set as a CSV header followed by its single result row.
pub fn render_csv(statements: &[SelectStatement]) -> String {
    let mut output = String::new();
    for statement in statements {
        writeln!(output, "{}", statement.column_name()).expect("writing to a String cannot fail");
        writeln!(output, "{}", statement.value()).expect("writing to a String cannot fail");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_sql;

    #[test]
    fn renders_a_header_for_each_statement() {
        let statements = parse_sql("SELECT 1 AS one; SELECT -2").unwrap();

        assert_eq!(render_csv(&statements), "one\n1\nvalue\n-2\n");
    }
}
