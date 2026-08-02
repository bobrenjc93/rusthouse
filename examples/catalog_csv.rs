use std::error::Error;

use rusthouse::Catalog;
use rusthouse::csv::write_csv;
use rusthouse::sql::{parse_create_table, parse_insert, parse_select};

fn main() -> Result<(), Box<dyn Error>> {
    let mut catalog = Catalog::default();
    catalog.create_table(parse_create_table(
        "CREATE TABLE events (id Int64, label String)",
    )?)?;
    catalog.insert(parse_insert(
        "INSERT INTO events VALUES (1, 'first'), (2, 'with,comma')",
    )?)?;

    let mut output = Vec::new();
    write_csv(
        catalog.select(parse_select("SELECT * FROM events")?)?,
        &mut output,
    )?;
    print!("{}", String::from_utf8(output)?);
    Ok(())
}
