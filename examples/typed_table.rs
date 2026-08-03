use rusthouse::{DataType, Field, Table, Value};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut events = Table::with_row_limit(
        vec![
            Field::new("timestamp", DataType::Int64),
            Field::new("temperature", DataType::Float64),
            Field::new("healthy", DataType::Bool),
            Field::new("region", DataType::String),
        ],
        10_000,
    )?;

    events.insert_batch(vec![
        vec![
            Value::Int64(1_722_528_000),
            Value::Float64(21.5),
            Value::Bool(true),
            Value::String("west".to_owned()),
        ],
        vec![
            Value::Int64(1_722_528_060),
            Value::Float64(22.0),
            Value::Bool(true),
            Value::String("east".to_owned()),
        ],
    ])?;

    println!("rows: {}", events.len());
    println!("temperatures: {:?}", events.float64_column("temperature")?);
    Ok(())
}
