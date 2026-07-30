use rusthouse::storage::{Column, ColumnDef, Table};
use rusthouse::{DataType, Value};

const ROW_COUNT: usize = 100_000;
const VALUES: [&str; 8] = [
    "customer-success-north-america",
    "customer-success-europe",
    "enterprise-sales-north-america",
    "enterprise-sales-europe",
    "product-analytics-north-america",
    "product-analytics-europe",
    "security-operations-north-america",
    "security-operations-europe",
];

fn table(name: &str, data_type: DataType) -> Table {
    Table::new(
        name.to_owned(),
        vec![ColumnDef {
            name: "dimension".to_owned(),
            data_type,
        }],
    )
    .expect("valid measurement schema")
}

fn fill(table: &mut Table) {
    for row in 0..ROW_COUNT {
        table
            .insert_row(vec![Value::String(VALUES[row % VALUES.len()].to_owned())])
            .expect("measurement insert succeeds");
    }
}

fn main() {
    let mut plain = table("plain", DataType::String);
    let mut low = table("low", DataType::LowCardinalityString);
    fill(&mut plain);
    fill(&mut low);

    let plain_bytes = plain.allocated_bytes();
    let low_bytes = low.allocated_bytes();
    let Column::LowCardinalityString(dictionary) = &low.columns()[0] else {
        unreachable!("measurement schema uses LowCardinality(String)");
    };
    println!("rows={ROW_COUNT} cardinality={}", dictionary.cardinality());
    println!("plain_string_allocated_bytes={plain_bytes}");
    println!("low_cardinality_allocated_bytes={low_bytes}");
    println!(
        "plain_over_low_ratio={:.2}",
        plain_bytes as f64 / low_bytes as f64
    );
}
