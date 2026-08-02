use rusthouse::{Column, ColumnSchema, DataType, Schema, Table, TableError, TableLimits, Value};

fn schema(columns: &[(&str, DataType)]) -> Schema {
    Schema::new(
        columns
            .iter()
            .map(|(name, data_type)| ColumnSchema::new(*name, *data_type))
            .collect(),
    )
    .unwrap()
}

fn limits(max_columns: usize, max_rows: usize, max_string_bytes: usize) -> TableLimits {
    TableLimits {
        max_columns,
        max_rows,
        max_cells: max_columns.saturating_mul(max_rows),
        max_string_bytes,
    }
}

#[test]
fn stores_every_type_in_name_addressable_columns() {
    let schema = schema(&[
        ("id", DataType::Int64),
        ("score", DataType::Float64),
        ("active", DataType::Bool),
        ("label", DataType::String),
    ]);
    let mut table = Table::new(schema, limits(4, 2, 6)).unwrap();

    table
        .insert_batch(vec![
            vec![
                Value::Int64(-4),
                Value::Float64(1.25),
                Value::Bool(true),
                Value::String("red".into()),
            ],
            vec![
                Value::Int64(9),
                Value::Float64(-0.5),
                Value::Bool(false),
                Value::String("sky".into()),
            ],
        ])
        .unwrap();

    assert_eq!(table.row_count(), 2);
    assert_eq!(table.string_bytes(), 6);
    assert_eq!(
        table.schema().column("score").unwrap().data_type(),
        DataType::Float64
    );
    assert_eq!(table.column("id").unwrap().as_int64(), Some(&[-4, 9][..]));
    assert_eq!(
        table.column("score").unwrap().as_float64(),
        Some(&[1.25, -0.5][..])
    );
    assert_eq!(
        table.column("active").unwrap().as_bool(),
        Some(&[true, false][..])
    );
    assert_eq!(
        table.column("label").unwrap(),
        &Column::String(vec!["red".into(), "sky".into()])
    );
    assert!(table.column("missing").is_none());
}

#[test]
fn rejects_duplicate_column_names() {
    let error = Schema::new(vec![
        ColumnSchema::new("same", DataType::Int64),
        ColumnSchema::new("same", DataType::String),
    ])
    .unwrap_err();

    assert_eq!(
        error,
        TableError::DuplicateColumnName {
            name: "same".into()
        }
    );
}

#[test]
fn enforces_column_limit_at_the_boundary() {
    Table::new(
        schema(&[("left", DataType::Int64), ("right", DataType::Bool)]),
        limits(2, 0, 0),
    )
    .unwrap();

    let error = Table::new(
        schema(&[("left", DataType::Int64), ("right", DataType::Bool)]),
        limits(1, 0, 0),
    )
    .unwrap_err();
    assert_eq!(
        error,
        TableError::ColumnLimitExceeded {
            limit: 1,
            attempted: 2
        }
    );
}

#[test]
fn enforces_row_and_utf8_byte_limits_at_the_boundary() {
    let mut table = Table::new(schema(&[("text", DataType::String)]), limits(1, 2, 4)).unwrap();
    table
        .insert_batch(vec![
            vec![Value::String("ab".into())],
            vec![Value::String("é".into())],
        ])
        .unwrap();
    assert_eq!(table.string_bytes(), 4);

    let snapshot = table.clone();
    assert_eq!(
        table.insert_row(vec![Value::String(String::new())]),
        Err(TableError::RowLimitExceeded {
            limit: 2,
            attempted: 3
        })
    );
    assert_eq!(table, snapshot);

    let mut byte_limited =
        Table::new(schema(&[("text", DataType::String)]), limits(1, 2, 3)).unwrap();
    let error = byte_limited
        .insert_batch(vec![
            vec![Value::String("ab".into())],
            vec![Value::String("é".into())],
        ])
        .unwrap_err();
    assert_eq!(
        error,
        TableError::StringByteLimitExceeded {
            limit: 3,
            attempted: 4
        }
    );
    assert!(byte_limited.is_empty());
}

#[test]
fn enforces_total_cell_limit_without_mutating_columns() {
    let mut table = Table::new(
        schema(&[("id", DataType::Int64), ("active", DataType::Bool)]),
        TableLimits {
            max_columns: 2,
            max_rows: 10,
            max_cells: 4,
            max_string_bytes: 0,
        },
    )
    .unwrap();
    table
        .insert_row(vec![Value::Int64(1), Value::Bool(true)])
        .unwrap();
    assert_eq!(table.cell_count(), 2);
    let snapshot = table.clone();

    assert_eq!(
        table.insert_batch(vec![
            vec![Value::Int64(2), Value::Bool(false)],
            vec![Value::Int64(3), Value::Bool(true)],
        ]),
        Err(TableError::CellLimitExceeded {
            limit: 4,
            attempted: 6,
        })
    );
    assert_eq!(table, snapshot);
}

#[test]
fn rejects_row_shape_without_mutating_the_table() {
    let mut table = Table::new(
        schema(&[("id", DataType::Int64), ("ok", DataType::Bool)]),
        limits(2, 3, 0),
    )
    .unwrap();

    let error = table.insert_row(vec![Value::Int64(1)]).unwrap_err();
    assert_eq!(
        error,
        TableError::RowShapeMismatch {
            row: 0,
            expected: 2,
            actual: 1
        }
    );
    assert!(table.is_empty());
    assert!(table.columns().iter().all(Column::is_empty));
}

#[test]
fn rejects_a_late_type_error_without_partially_appending_the_batch() {
    let mut table = Table::new(
        schema(&[("id", DataType::Int64), ("name", DataType::String)]),
        limits(2, 4, 20),
    )
    .unwrap();
    table
        .insert_row(vec![Value::Int64(1), Value::String("kept".into())])
        .unwrap();
    let snapshot = table.clone();

    let error = table
        .insert_batch(vec![
            vec![Value::Int64(2), Value::String("valid".into())],
            vec![Value::Bool(false), Value::String("invalid".into())],
        ])
        .unwrap_err();

    assert_eq!(
        error,
        TableError::TypeMismatch {
            row: 1,
            column: 0,
            column_name: "id".into(),
            expected: DataType::Int64,
            actual: DataType::Bool,
        }
    );
    assert_eq!(table, snapshot);
}

#[derive(Debug)]
struct FixedRng(u64);

impl FixedRng {
    fn new(seed: u64) -> Self {
        assert_ne!(seed, 0);
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        // Xorshift64 is sufficient here: the fixed stream is test data, not entropy.
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, exclusive_upper_bound: usize) -> usize {
        assert_ne!(exclusive_upper_bound, 0);
        (self.next() % exclusive_upper_bound as u64) as usize
    }
}

#[derive(Debug)]
struct ReferenceTable {
    int64: Vec<i64>,
    float64: Vec<f64>,
    bools: Vec<bool>,
    strings: Vec<String>,
    limits: TableLimits,
}

impl ReferenceTable {
    fn new(limits: TableLimits) -> Self {
        Self {
            int64: Vec::new(),
            float64: Vec::new(),
            bools: Vec::new(),
            strings: Vec::new(),
            limits,
        }
    }

    fn row_count(&self) -> usize {
        self.int64.len()
    }

    fn string_bytes(&self) -> usize {
        self.strings.iter().map(String::len).sum()
    }

    fn insert_batch(&mut self, rows: &[Vec<Value>]) -> bool {
        let Some(attempted_rows) = self.row_count().checked_add(rows.len()) else {
            return false;
        };
        if attempted_rows > self.limits.max_rows {
            return false;
        }
        let Some(attempted_cells) = attempted_rows.checked_mul(4) else {
            return false;
        };
        if attempted_cells > self.limits.max_cells {
            return false;
        }

        let mut added_string_bytes = 0usize;
        for row in rows {
            let [
                Value::Int64(_),
                Value::Float64(_),
                Value::Bool(_),
                Value::String(string),
            ] = row.as_slice()
            else {
                return false;
            };
            let Some(bytes) = added_string_bytes.checked_add(string.len()) else {
                return false;
            };
            added_string_bytes = bytes;
        }

        let Some(attempted_string_bytes) = self.string_bytes().checked_add(added_string_bytes)
        else {
            return false;
        };
        if attempted_string_bytes > self.limits.max_string_bytes {
            return false;
        }

        for row in rows {
            match row.as_slice() {
                [
                    Value::Int64(int64),
                    Value::Float64(float64),
                    Value::Bool(value),
                    Value::String(string),
                ] => {
                    self.int64.push(*int64);
                    self.float64.push(*float64);
                    self.bools.push(*value);
                    self.strings.push(string.clone());
                }
                _ => unreachable!("the complete batch was validated above"),
            }
        }
        true
    }
}

fn generated_row(rng: &mut FixedRng) -> Vec<Value> {
    const STRING_PARTS: [&str; 8] = ["", "a", "rust", "é", "東京", "line\n", "nul\0", "xyz"];

    let magnitude = rng.below(20_001) as i64 - 10_000;
    let float = (rng.below(80_001) as f64 - 40_000.0) / 8.0;
    let string = format!(
        "{}:{}",
        STRING_PARTS[rng.below(STRING_PARTS.len())],
        rng.below(97)
    );
    vec![
        Value::Int64(magnitude),
        Value::Float64(float),
        Value::Bool(rng.below(2) == 0),
        Value::String(string),
    ]
}

fn generated_batch(rng: &mut FixedRng, row_count: usize) -> Vec<Vec<Value>> {
    (0..row_count).map(|_| generated_row(rng)).collect()
}

fn assert_matches_reference(table: &Table, reference: &ReferenceTable, step: usize) {
    assert_eq!(table.row_count(), reference.row_count(), "step {step}");
    assert_eq!(
        table.string_bytes(),
        reference.string_bytes(),
        "step {step}"
    );
    assert_eq!(
        table.column("int64").and_then(Column::as_int64),
        Some(reference.int64.as_slice()),
        "step {step}"
    );
    assert_eq!(
        table.column("float64").and_then(Column::as_float64),
        Some(reference.float64.as_slice()),
        "step {step}"
    );
    assert_eq!(
        table.column("bool").and_then(Column::as_bool),
        Some(reference.bools.as_slice()),
        "step {step}"
    );
    assert_eq!(
        table.column("string").and_then(Column::as_string),
        Some(reference.strings.as_slice()),
        "step {step}"
    );
}

#[test]
fn fixed_seed_batches_match_a_reference_model_and_rejections_are_atomic() {
    const STEPS: usize = 240;
    const SEED: u64 = 0x5eed_5eed_cafe_f00d;

    let table_limits = limits(4, 512, 4_096);
    let mut table = Table::new(
        schema(&[
            ("int64", DataType::Int64),
            ("float64", DataType::Float64),
            ("bool", DataType::Bool),
            ("string", DataType::String),
        ]),
        table_limits,
    )
    .unwrap();
    let mut reference = ReferenceTable::new(table_limits);
    let mut rng = FixedRng::new(SEED);

    for step in 0..STEPS {
        let batch = match step % 8 {
            // Include empty, single-row, and multi-row valid batches.
            0 | 1 | 4 | 7 => {
                let row_count = rng.below(5);
                generated_batch(&mut rng, row_count)
            }
            // Rotate the corrupted column so every expected physical type is rejected.
            2 => {
                let row_count = rng.below(4) + 1;
                let invalid_row = rng.below(row_count);
                let invalid_column = (step / 8) % 4;
                let mut rows = generated_batch(&mut rng, row_count);
                rows[invalid_row][invalid_column] = match invalid_column {
                    0 => Value::String("not an Int64".into()),
                    1 => Value::Bool(false),
                    2 => Value::Int64(0),
                    3 => Value::Float64(0.0),
                    _ => unreachable!(),
                };
                rows
            }
            3 => {
                let row_count = rng.below(3) + 1;
                let mut rows = generated_batch(&mut rng, row_count);
                let invalid_row = rng.below(rows.len());
                rows[invalid_row].truncate(rng.below(4));
                rows
            }
            5 => {
                let row_count = rng.below(3) + 1;
                let mut rows = generated_batch(&mut rng, row_count);
                let invalid_row = rng.below(rows.len());
                rows[invalid_row].push(Value::Bool(true));
                rows
            }
            6 if (step / 8) % 2 == 0 => {
                let mut row = generated_row(&mut rng);
                let remaining = table_limits.max_string_bytes - reference.string_bytes();
                row[3] = Value::String("x".repeat(remaining + 1));
                vec![row]
            }
            6 => {
                let attempted = table_limits.max_rows - reference.row_count() + 1;
                generated_batch(&mut rng, attempted)
            }
            _ => unreachable!(),
        };

        let snapshot = table.clone();
        let expected_acceptance = reference.insert_batch(&batch);
        let result = table.insert_batch(batch);
        if result.is_err() {
            assert_eq!(
                table, snapshot,
                "rejected batch mutated table at step {step}"
            );
        }
        assert_eq!(
            result.is_ok(),
            expected_acceptance,
            "reference and table disagreed at step {step}: {result:?}"
        );
        assert_matches_reference(&table, &reference, step);
    }
}
