use std::panic::{AssertUnwindSafe, catch_unwind};

use rusthouse::format::{OutputFormat, render};
use rusthouse::sql;
use rusthouse::{DataType, Error, QueryResult, ResultColumn, Value};

const PROPERTY_CASES: usize = 4_096;

#[test]
fn query_result_enforces_its_public_invariants() {
    let duplicate_columns = vec![
        ResultColumn {
            name: "value".to_owned(),
            data_type: DataType::Int64,
        },
        ResultColumn {
            name: "value".to_owned(),
            data_type: DataType::String,
        },
    ];
    let result = QueryResult::new(
        duplicate_columns.clone(),
        vec![vec![Value::Int64(7), Value::String("seven".to_owned())]],
    )
    .expect("duplicate aliases are positional and legal");
    assert_eq!(result.columns(), duplicate_columns);
    assert_eq!(result.rows().len(), 1);
    for format in [OutputFormat::Table, OutputFormat::Csv, OutputFormat::Json] {
        let rendered = render(&result, format);
        if format == OutputFormat::Json {
            assert!(is_valid_json(&rendered));
        }
    }

    assert!(matches!(
        QueryResult::new(duplicate_columns.clone(), vec![vec![Value::Int64(7)]]),
        Err(Error::QueryResultRowLength {
            row: 0,
            expected: 2,
            actual: 1,
        })
    ));
    assert!(matches!(
        QueryResult::new(
            duplicate_columns,
            vec![vec![Value::Bool(true), Value::String("wrong".to_owned())]],
        ),
        Err(Error::QueryResultTypeMismatch {
            row: 0,
            column: 0,
            expected: DataType::Int64,
            actual: DataType::Bool,
        })
    ));

    let float_column = vec![ResultColumn {
        name: "number".to_owned(),
        data_type: DataType::Float64,
    }];
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(matches!(
            QueryResult::new(float_column.clone(), vec![vec![Value::Float64(value)]]),
            Err(Error::QueryResultNonFiniteFloat { row: 0, column: 0 })
        ));
    }
}

#[test]
fn arbitrary_parser_input_never_panics() {
    let mut rng = Generator::new(0x8077_32a1_f05d_91e3);

    for case in 0..PROPERTY_CASES {
        let input = if case % 2 == 0 {
            random_text(&mut rng, 256)
        } else {
            let bytes = (0..rng.range(256))
                .map(|_| rng.next_u64() as u8)
                .collect::<Vec<_>>();
            String::from_utf8_lossy(&bytes).into_owned()
        };
        let outcome = catch_unwind(|| sql::parse(&input));
        assert!(outcome.is_ok(), "parser panicked for input {input:?}");
    }
}

#[test]
fn generated_results_are_rejected_or_render_safely() {
    let mut rng = Generator::new(0x130d_86f4_16a9_7bc2);
    let mut accepted = 0;
    let mut rejected = 0;
    let mut rendered_types = [false; 4];

    for case in 0..PROPERTY_CASES {
        let (columns, rows) = random_result_parts(&mut rng);
        let should_validate = result_parts_are_valid(&columns, &rows);
        let outcome = QueryResult::new(columns.clone(), rows.clone());
        assert_eq!(
            outcome.is_ok(),
            should_validate,
            "constructor disagreed with invariant oracle in case {case}"
        );

        let Ok(result) = outcome else {
            rejected += 1;
            continue;
        };
        accepted += 1;
        assert_eq!(result.columns(), columns);
        assert_eq!(result.rows(), rows);
        for value in result.rows().iter().flatten() {
            rendered_types[match value.data_type() {
                DataType::Int64 => 0,
                DataType::Float64 => 1,
                DataType::Bool => 2,
                DataType::String => 3,
            }] = true;
        }

        for format in [OutputFormat::Table, OutputFormat::Csv, OutputFormat::Json] {
            let rendered = catch_unwind(AssertUnwindSafe(|| render(&result, format)))
                .unwrap_or_else(|_| panic!("{format:?} renderer panicked in case {case}"));
            if format == OutputFormat::Json {
                assert!(
                    is_valid_json(&rendered),
                    "invalid JSON in case {case}: {rendered}"
                );
            }
        }
    }

    assert!(accepted > 0);
    assert!(rejected > 0);
    assert!(rendered_types.into_iter().all(|rendered| rendered));
}

#[test]
fn json_test_parser_rejects_invalid_scalars_and_escaping() {
    for invalid in [
        r#"{"value":NaN}"#,
        r#"{"value":Infinity}"#,
        r#"{"value":-Infinity}"#,
        "{\"value\":\"line\nfeed\"}",
        r#"{"value":"\q"}"#,
        r#"{"value":1} trailing"#,
    ] {
        assert!(
            !is_valid_json(invalid),
            "accepted invalid JSON: {invalid:?}"
        );
    }
}

fn random_result_parts(rng: &mut Generator) -> (Vec<ResultColumn>, Vec<Vec<Value>>) {
    let column_count = rng.range(9);
    let mut columns: Vec<ResultColumn> = Vec::with_capacity(column_count);
    for index in 0..column_count {
        let name = if index > 0 && rng.range(3) == 0 {
            columns[rng.range(index)].name.clone()
        } else {
            random_text(rng, 16)
        };
        columns.push(ResultColumn {
            name,
            data_type: random_data_type(rng),
        });
    }

    let row_count = rng.range(13);
    let mut rows = Vec::with_capacity(row_count);
    for _ in 0..row_count {
        let width = if rng.range(4) != 0 {
            column_count
        } else {
            rng.range(11)
        };
        let mut row = Vec::with_capacity(width);
        let declared_types = columns
            .iter()
            .map(|column| Some(column.data_type))
            .chain(std::iter::repeat(None));
        for declared_type in declared_types.take(width) {
            let data_type = match declared_type {
                Some(data_type) if rng.range(5) != 0 => data_type,
                _ => random_data_type(rng),
            };
            row.push(random_value_for_type(rng, data_type));
        }
        rows.push(row);
    }
    (columns, rows)
}

fn result_parts_are_valid(columns: &[ResultColumn], rows: &[Vec<Value>]) -> bool {
    rows.iter().all(|row| {
        row.len() == columns.len()
            && columns.iter().zip(row).all(|(column, value)| {
                column.data_type == value.data_type()
                    && !matches!(value, Value::Float64(number) if !number.is_finite())
            })
    })
}

fn random_data_type(rng: &mut Generator) -> DataType {
    match rng.range(4) {
        0 => DataType::Int64,
        1 => DataType::Float64,
        2 => DataType::Bool,
        _ => DataType::String,
    }
}

fn random_value_for_type(rng: &mut Generator, data_type: DataType) -> Value {
    match data_type {
        DataType::Int64 => Value::Int64(rng.next_u64() as i64),
        DataType::Float64 => {
            let value = match rng.range(32) {
                0 => f64::NAN,
                1 => f64::INFINITY,
                2 => f64::NEG_INFINITY,
                _ => f64::from_bits(rng.next_u64()),
            };
            Value::Float64(value)
        }
        DataType::Bool => Value::Bool(rng.range(2) == 0),
        DataType::String => Value::String(random_text(rng, 32)),
    }
}

fn random_text(rng: &mut Generator, max_chars: usize) -> String {
    const INTERESTING: &[char] = &[
        '\0', '\n', '\r', '\t', '\u{1b}', '\u{7f}', ' ', 'a', 'Z', '0', '_', '\'', '"', '\\', ',',
        ';', '(', ')', '-', '+', '.', '=', '<', '>', '*', '/', 'é', '中', '😀',
    ];

    let length = rng.range(max_chars + 1);
    let mut output = String::new();
    for _ in 0..length {
        if rng.range(4) != 0 {
            output.push(INTERESTING[rng.range(INTERESTING.len())]);
        } else {
            loop {
                let candidate = (rng.next_u64() % 0x11_0000) as u32;
                if let Some(character) = char::from_u32(candidate) {
                    output.push(character);
                    break;
                }
            }
        }
    }
    output
}

struct Generator(u64);

impl Generator {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    fn range(&mut self, upper: usize) -> usize {
        (self.next_u64() % upper as u64) as usize
    }
}

fn is_valid_json(input: &str) -> bool {
    let mut parser = JsonParser {
        input: input.as_bytes(),
        position: 0,
    };
    parser.whitespace();
    parser.value() && {
        parser.whitespace();
        parser.position == parser.input.len()
    }
}

struct JsonParser<'a> {
    input: &'a [u8],
    position: usize,
}

impl JsonParser<'_> {
    fn value(&mut self) -> bool {
        self.whitespace();
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => self.string(),
            Some(b't') => self.keyword(b"true"),
            Some(b'f') => self.keyword(b"false"),
            Some(b'n') => self.keyword(b"null"),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => false,
        }
    }

    fn object(&mut self) -> bool {
        self.position += 1;
        self.whitespace();
        if self.take(b'}') {
            return true;
        }
        loop {
            self.whitespace();
            if !self.string() {
                return false;
            }
            self.whitespace();
            if !self.take(b':') || !self.value() {
                return false;
            }
            self.whitespace();
            if self.take(b'}') {
                return true;
            }
            if !self.take(b',') {
                return false;
            }
        }
    }

    fn array(&mut self) -> bool {
        self.position += 1;
        self.whitespace();
        if self.take(b']') {
            return true;
        }
        loop {
            if !self.value() {
                return false;
            }
            self.whitespace();
            if self.take(b']') {
                return true;
            }
            if !self.take(b',') {
                return false;
            }
        }
    }

    fn string(&mut self) -> bool {
        if !self.take(b'"') {
            return false;
        }
        while let Some(byte) = self.peek() {
            match byte {
                b'"' => {
                    self.position += 1;
                    return true;
                }
                b'\\' => {
                    self.position += 1;
                    match self.peek() {
                        Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => {
                            self.position += 1;
                        }
                        Some(b'u') => {
                            self.position += 1;
                            for _ in 0..4 {
                                if !self.peek().is_some_and(|value| value.is_ascii_hexdigit()) {
                                    return false;
                                }
                                self.position += 1;
                            }
                        }
                        _ => return false,
                    }
                }
                0..=0x1f => return false,
                _ => self.position += 1,
            }
        }
        false
    }

    fn number(&mut self) -> bool {
        self.take(b'-');
        if self.take(b'0') {
            if self.peek().is_some_and(|value| value.is_ascii_digit()) {
                return false;
            }
        } else if !self.digits() {
            return false;
        }
        if self.take(b'.') && !self.digits() {
            return false;
        }
        if self
            .peek()
            .is_some_and(|value| matches!(value, b'e' | b'E'))
        {
            self.position += 1;
            if self
                .peek()
                .is_some_and(|value| matches!(value, b'+' | b'-'))
            {
                self.position += 1;
            }
            if !self.digits() {
                return false;
            }
        }
        true
    }

    fn digits(&mut self) -> bool {
        let start = self.position;
        while self.peek().is_some_and(|value| value.is_ascii_digit()) {
            self.position += 1;
        }
        self.position > start
    }

    fn keyword(&mut self, keyword: &[u8]) -> bool {
        if self.input[self.position..].starts_with(keyword) {
            self.position += keyword.len();
            true
        } else {
            false
        }
    }

    fn whitespace(&mut self) {
        while self
            .peek()
            .is_some_and(|value| matches!(value, b' ' | b'\n' | b'\r' | b'\t'))
        {
            self.position += 1;
        }
    }

    fn take(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.position).copied()
    }
}
