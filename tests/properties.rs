use proptest::prelude::*;
use rusthouse::{DataType, Database, Field, Schema, Table, Value, parse_sql_batch};

fn assert_position_is_valid(input: &str, error: &rusthouse::SqlError) {
    assert!(error.byte_offset() <= input.len());
    assert!(input.is_char_boundary(error.byte_offset()));
    assert!(error.line() >= 1);
    assert!(error.column() >= 1);
}

fn arbitrary_value() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(Value::Null),
        any::<i64>().prop_map(Value::Int64),
        any::<f64>().prop_map(Value::Float64),
        any::<bool>().prop_map(Value::Bool),
        any::<String>().prop_map(Value::String),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn scalar_parser_handles_arbitrary_utf8(input in any::<String>()) {
        if let Err(error) = parse_sql_batch(&input) {
            assert_position_is_valid(&input, &error);
        }
    }

    #[test]
    fn database_parser_handles_malformed_unicode_fragments(
        payload in any::<String>(),
        statement_kind in 0_u8..6,
    ) {
        let input = match statement_kind {
            0 => format!("SELECT {payload}"),
            1 => format!("SELECT '{payload};"),
            2 => format!("CREATE TABLE {payload} (value Nullable(Int64);"),
            3 => format!("INSERT INTO {payload} VALUES (1,);"),
            4 => format!("SELECT COUNT({payload}) FROM missing;"),
            _ => payload,
        };
        let mut database = Database::new();
        let before = database.clone();

        if let Err(error) = database.execute(&input) {
            assert_position_is_valid(&input, &error);
            prop_assert_eq!(database, before);
        }
    }

    #[test]
    fn arbitrary_storage_batches_are_atomic(
        rows in prop::collection::vec(
            prop::collection::vec(arbitrary_value(), 0..4),
            0..20,
        ),
    ) {
        let schema = Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("note", DataType::String, true),
        ]).unwrap();
        let mut table = Table::with_data_limit(schema, 16, 4 * 1024);
        let before = table.clone();
        let row_count = rows.len();

        match table.append_batch(rows) {
            Ok(()) => {
                prop_assert_eq!(table.row_count(), row_count);
                prop_assert!(table.data_size_bytes() <= table.data_byte_limit());
            }
            Err(_) => prop_assert_eq!(table, before),
        }
    }
}
