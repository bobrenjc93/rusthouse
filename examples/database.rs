use std::io;

use rusthouse::{Database, write_csv};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut database = Database::new();
    let results = database.execute(
        "CREATE TABLE events (id Int64, note Nullable(String));
         INSERT INTO events VALUES (1, NULL), (2, 'ready');
         SELECT COUNT(*) AS event_count FROM events;",
    )?;

    write_csv(&results, io::stdout().lock())?;
    Ok(())
}
