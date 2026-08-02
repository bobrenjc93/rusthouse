use std::panic::{UnwindSafe, catch_unwind};

use rusthouse::sql::{ParseError, Value, parse_create_table, parse_insert, parse_select};

#[derive(Debug)]
struct FixedRng(u64);

impl FixedRng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, upper: usize) -> usize {
        (self.next() % upper as u64) as usize
    }

    fn scalar(&mut self) -> char {
        loop {
            let candidate = (self.next() % 0x11_0000) as u32;
            if let Some(character) = char::from_u32(candidate) {
                return character;
            }
        }
    }
}

fn arbitrary_utf8(rng: &mut FixedRng, max_chars: usize) -> String {
    const SQL_CHARS: [char; 18] = [
        ' ', '\t', '\n', '\r', '\'', ';', ',', '(', ')', '*', '+', '-', '.', '_', '0', '9', 'A',
        'z',
    ];
    let length = rng.below(max_chars + 1);
    (0..length)
        .map(|_| {
            if rng.below(3) == 0 {
                rng.scalar()
            } else {
                SQL_CHARS[rng.below(SQL_CHARS.len())]
            }
        })
        .collect()
}

fn exercise<T, F>(name: &str, sql: &str, parser: F)
where
    F: FnOnce(&str) -> Result<T, ParseError> + UnwindSafe,
{
    let result = catch_unwind(|| parser(sql))
        .unwrap_or_else(|_| panic!("{name} panicked for input {sql:?}"));
    if let Err(error) = result {
        assert!(error.position() <= sql.len(), "{name}: {sql:?}: {error:?}");
        assert!(
            sql.is_char_boundary(error.position()),
            "{name} returned a non-character-boundary position for {sql:?}: {error:?}"
        );
    }
}

fn exercise_all(sql: &str) {
    exercise("CREATE", sql, parse_create_table);
    exercise("INSERT", sql, parse_insert);
    exercise("SELECT", sql, parse_select);
}

#[test]
fn fixed_seed_arbitrary_utf8_never_panics_and_reports_valid_offsets() {
    let mut rng = FixedRng(0xd1b5_4a32_d192_ed03);
    for _ in 0..4_000 {
        let input = arbitrary_utf8(&mut rng, 96);
        exercise_all(&input);
    }
}

#[test]
fn fixed_seed_malformed_mutations_never_panic() {
    const VALID: [&str; 3] = [
        "CREATE TABLE readings (id Int64, label String);",
        "INSERT INTO readings VALUES (1, 'first'), (2, 'it''s next');",
        "SELECT * FROM readings;",
    ];
    let mut rng = FixedRng(0xa076_1d64_78bd_642f);

    for _ in 0..2_000 {
        let base = VALID[rng.below(VALID.len())];
        let mut boundaries = base
            .char_indices()
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        boundaries.push(base.len());
        let split = boundaries[rng.below(boundaries.len())];
        let fragment = arbitrary_utf8(&mut rng, 24);
        let mutated = match rng.below(3) {
            0 => base[..split].to_owned(),
            1 => format!("{}{fragment}{}", &base[..split], &base[split..]),
            _ => {
                let other = boundaries[rng.below(boundaries.len())];
                let (left, right) = if split <= other {
                    (split, other)
                } else {
                    (other, split)
                };
                format!("{}{}", &base[..left], &base[right..])
            }
        };
        exercise_all(&mutated);
    }
}

#[test]
fn arbitrary_utf8_string_literals_round_trip() {
    let mut rng = FixedRng(0xe703_7ed1_a0b4_28db);
    for _ in 0..1_000 {
        let value = arbitrary_utf8(&mut rng, 64);
        let escaped = value.replace('\'', "''");
        let sql = format!("INSERT INTO t VALUES ('{escaped}')");
        let statement = parse_insert(&sql).unwrap_or_else(|error| panic!("{sql:?}: {error:?}"));
        assert_eq!(statement.rows, vec![vec![Value::String(value)]]);
    }
}
