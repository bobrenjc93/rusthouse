use csv::Terminator;
use rusthouse::sql::lexer::{LexerLimits, Span, tokenize};
use rusthouse::sql::parser::parse_create_table;
use rusthouse::{ColumnDef, CsvFormatter, CsvLimits, DataType};

const TEXT_ALPHABET: &[char] = &[
    'a',
    'Z',
    '0',
    '_',
    ' ',
    ',',
    '"',
    '\r',
    '\n',
    '\t',
    '\'',
    ';',
    '!',
    'e',
    '\u{e9}',
    '\u{4e2d}',
    '\u{1f642}',
];

struct DeterministicRng(u64);

impl DeterministicRng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }

    fn index(&mut self, upper: usize) -> usize {
        (self.next() as usize) % upper
    }

    fn text(&mut self, minimum: usize, maximum: usize) -> String {
        let length = minimum + self.index(maximum - minimum + 1);
        (0..length)
            .map(|_| TEXT_ALPHABET[self.index(TEXT_ALPHABET.len())])
            .collect()
    }

    fn case_word(&mut self, word: &str) -> String {
        word.chars()
            .map(|character| {
                if self.index(2) == 0 {
                    character.to_ascii_lowercase()
                } else {
                    character.to_ascii_uppercase()
                }
            })
            .collect()
    }

    fn whitespace(&mut self) -> &'static str {
        [" ", "  ", "\t", "\n", " \r\n"][self.index(5)]
    }
}

#[test]
fn lexer_fuzz_keeps_every_reported_span_valid() {
    let mut rng = DeterministicRng::new(0x5eed_1eaf);

    for _ in 0..5_000 {
        let input = rng.text(0, 64);
        let result = tokenize(&input, LexerLimits::new(input.len(), 256, 64));

        match result {
            Ok(tokens) => {
                let mut previous_end = 0;
                for token in tokens {
                    assert!(token.span.start() >= previous_end, "{input:?}");
                    assert!(token.span.start() < token.span.end(), "{input:?}");
                    assert!(token.span.end() <= input.len(), "{input:?}");
                    assert!(input.is_char_boundary(token.span.start()), "{input:?}");
                    assert!(input.is_char_boundary(token.span.end()), "{input:?}");
                    assert!(!input[token.span.start()..token.span.end()].is_empty());
                    previous_end = token.span.end();
                }
            }
            Err(error) => {
                assert!(error.span.start() <= error.span.end(), "{input:?}");
                assert!(error.span.end() <= input.len(), "{input:?}");
                assert!(input.is_char_boundary(error.span.start()), "{input:?}");
                assert!(input.is_char_boundary(error.span.end()), "{input:?}");
            }
        }
    }
}

#[test]
fn parser_property_accepts_generated_create_table_statements() {
    let mut rng = DeterministicRng::new(0xc0de_cafe);
    let types = [
        ("Int64", DataType::Int64),
        ("Float64", DataType::Float64),
        ("Bool", DataType::Bool),
        ("String", DataType::String),
    ];

    for case in 0..1_000 {
        let table_name = format!("table_{case}");
        let column_count = 1 + rng.index(8);
        let mut expected_columns = Vec::with_capacity(column_count);
        let mut sql = format!(
            "{}{}{}{}{}{}(",
            rng.case_word("CREATE"),
            rng.whitespace(),
            rng.case_word("TABLE"),
            rng.whitespace(),
            table_name,
            rng.whitespace(),
        );

        for column in 0..column_count {
            if column != 0 {
                sql.push(',');
                sql.push_str(rng.whitespace());
            }
            let column_name = format!("column_{column}");
            let (type_name, data_type) = types[rng.index(types.len())];
            sql.push_str(&column_name);
            sql.push_str(rng.whitespace());
            sql.push_str(&rng.case_word(type_name));
            expected_columns.push(ColumnDef::new(column_name, data_type));
        }
        sql.push(')');
        if rng.index(2) == 0 {
            sql.push(';');
        }

        let statement = parse_create_table(&sql, LexerLimits::new(sql.len(), 128, 1)).unwrap();
        assert_eq!(statement.name, table_name);
        assert_eq!(statement.columns, expected_columns);
    }
}

#[test]
fn parser_fuzz_returns_only_in_bounds_error_spans() {
    let mut rng = DeterministicRng::new(0xbad5_eed5);

    for _ in 0..3_000 {
        let input = rng.text(0, 96);
        if let Err(error) = parse_create_table(&input, LexerLimits::new(input.len(), 384, 96)) {
            assert!(error.span.start() <= error.span.end(), "{input:?}");
            assert!(error.span.end() <= input.len(), "{input:?}");
            assert!(input.is_char_boundary(error.span.start()), "{input:?}");
            assert!(input.is_char_boundary(error.span.end()), "{input:?}");
        }
    }
}

#[test]
fn csv_property_round_trips_generated_utf8_cells() {
    let mut rng = DeterministicRng::new(0xc5c5_c5c5);

    for _ in 0..1_000 {
        let width = 1 + rng.index(8);
        let row_count = rng.index(8);
        let header: Vec<_> = (0..width).map(|_| rng.text(0, 16)).collect();
        let rows: Vec<Vec<_>> = (0..row_count)
            .map(|_| (0..width).map(|_| rng.text(0, 24)).collect())
            .collect();
        let mut output = Vec::new();

        CsvFormatter::new(CsvLimits::new(width, 128).with_max_output_bytes(32 * 1024))
            .write(&mut output, &header, rows.iter())
            .unwrap();

        let parsed: Vec<Vec<String>> = csv::ReaderBuilder::new()
            .has_headers(false)
            .from_reader(output.as_slice())
            .records()
            .map(|record| record.unwrap().iter().map(str::to_owned).collect())
            .collect();
        let expected: Vec<_> = std::iter::once(header).chain(rows).collect();
        assert_eq!(parsed, expected);
    }
}

#[test]
fn csv_output_matches_the_reference_writer_for_generated_records() {
    let mut rng = DeterministicRng::new(0xd1ff_e2e7);

    for _ in 0..1_000 {
        let width = 1 + rng.index(8);
        let row_count = rng.index(8);
        // The reference writer intentionally uses a different policy for empty
        // fields. Empty cells are covered by the round-trip property above.
        let header: Vec<_> = (0..width).map(|_| rng.text(1, 16)).collect();
        let rows: Vec<Vec<_>> = (0..row_count)
            .map(|_| (0..width).map(|_| rng.text(1, 24)).collect())
            .collect();

        let mut actual = Vec::new();
        CsvFormatter::new(CsvLimits::new(width, 128).with_max_output_bytes(32 * 1024))
            .write(&mut actual, &header, rows.iter())
            .unwrap();

        let mut reference = csv::WriterBuilder::new()
            .terminator(Terminator::CRLF)
            .from_writer(Vec::new());
        reference.write_record(&header).unwrap();
        for row in &rows {
            reference.write_record(row).unwrap();
        }
        let expected = reference.into_inner().unwrap();

        assert_eq!(actual, expected);
    }
}

#[test]
#[should_panic(expected = "a span cannot end before it starts")]
fn span_rejects_reversed_bounds() {
    let _ = Span::new(2, 1);
}
