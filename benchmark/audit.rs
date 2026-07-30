use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::digest::sha256;
use crate::normalize::ColumnType;

pub const AUDIT_ROW_COUNT: usize = 257;
pub const QUERIES_PER_FAMILY: usize = 48;
pub const AUDIT_QUERY_COUNT: usize = QUERIES_PER_FAMILY * AuditFamily::ALL.len();
const TABLE_NAME: &str = "correctness_audit_data";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AuditFamily {
    ScalarBoundaries,
    Predicates,
    Projections,
    Aggregates,
    Grouping,
    Ordering,
    Limits,
}

impl AuditFamily {
    pub const ALL: [Self; 7] = [
        Self::ScalarBoundaries,
        Self::Predicates,
        Self::Projections,
        Self::Aggregates,
        Self::Grouping,
        Self::Ordering,
        Self::Limits,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::ScalarBoundaries => "scalar_boundaries",
            Self::Predicates => "predicates",
            Self::Projections => "projections",
            Self::Aggregates => "aggregates",
            Self::Grouping => "grouping",
            Self::Ordering => "ordering",
            Self::Limits => "limits",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AuditCase {
    pub id: String,
    pub family: AuditFamily,
    pub sql: String,
    pub columns: Vec<(String, ColumnType)>,
    pub max_rows: usize,
}

#[derive(Debug, Clone)]
pub struct AuditCorpus {
    pub row_count: usize,
    pub cases: Vec<AuditCase>,
    pub replay_sql: String,
    pub setup_sha256: String,
    pub queries_sha256: String,
    pub corpus_sha256: String,
}

impl AuditCorpus {
    pub fn generate(seed: u64) -> Self {
        let rows = generate_rows(seed);
        let setup_sql = setup_sql(&rows);
        let cases = generate_cases(seed, &rows);
        let mut query_sql = String::new();
        for case in &cases {
            query_sql.push_str(&case.sql);
        }
        let replay_sql = format!("{setup_sql}{query_sql}");

        Self {
            row_count: rows.len(),
            cases,
            setup_sha256: sha256(setup_sql.as_bytes()),
            queries_sha256: sha256(query_sql.as_bytes()),
            corpus_sha256: sha256(replay_sql.as_bytes()),
            replay_sql,
        }
    }

    pub fn family_counts(&self) -> BTreeMap<&'static str, usize> {
        let mut counts = BTreeMap::new();
        for case in &self.cases {
            *counts.entry(case.family.name()).or_insert(0) += 1;
        }
        counts
    }
}

#[derive(Debug, Clone)]
struct AuditRow {
    id: i64,
    signed_value: i64,
    measure: i64,
    boundary_float: f64,
    score: f64,
    low_key: String,
    high_key: String,
    payload: String,
    flag: bool,
}

fn generate_rows(seed: u64) -> Vec<AuditRow> {
    const INTEGER_BOUNDARIES: [i64; 16] = [
        i64::MIN,
        i64::MIN + 1,
        -9_007_199_254_740_993,
        -9_007_199_254_740_992,
        -1_000_000_000,
        -1,
        0,
        1,
        2,
        255,
        65_535,
        1_000_000_000,
        9_007_199_254_740_992,
        9_007_199_254_740_993,
        i64::MAX - 1,
        i64::MAX,
    ];
    const FLOAT_BOUNDARIES: [f64; 16] = [
        -1.0e300,
        -9_007_199_254_740_992.0,
        -1_000_000_000.125,
        -1.5,
        -0.125,
        -0.0,
        0.0,
        f64::MIN_POSITIVE,
        0.125,
        1.5,
        1_000_000_000.125,
        9_007_199_254_740_992.0,
        1.0e100,
        1.0e200,
        1.0e250,
        1.0e300,
    ];
    const LOW_KEYS: [&str; 8] = [
        "",
        "alpha",
        "beta",
        "comma,key",
        "quote'key",
        "symbols-_!",
        "z",
        "zzzz",
    ];
    const PAYLOADS: [&str; 8] = [
        "",
        "a",
        "comma,inside",
        "quote's payload",
        "double\"quote",
        "spaces inside",
        "symbols-_!",
        "a considerably longer payload value",
    ];

    let mut random = SplitMix64::new(seed ^ 0xa076_1d64_78bd_642f);
    let mut rows = Vec::with_capacity(AUDIT_ROW_COUNT);
    for index in 0..AUDIT_ROW_COUNT {
        let signed_value = INTEGER_BOUNDARIES
            .get(index)
            .copied()
            .unwrap_or_else(|| random.next() as i64);
        let boundary_float = FLOAT_BOUNDARIES.get(index).copied().unwrap_or_else(|| {
            let numerator = (random.next() % 16_000_001) as i64 - 8_000_000;
            numerator as f64 / 8.0
        });
        let measure = (random.next() % 2_001) as i64 - 1_000;
        let score = ((random.next() % 160_001) as i64 - 80_000) as f64 / 8.0;
        let low_key = LOW_KEYS[(random.next() as usize) % LOW_KEYS.len()].to_owned();
        let high_key = format!("entity_{index:04}");
        let payload = match PAYLOADS.get(index) {
            Some(payload) => (*payload).to_owned(),
            None => format!(
                "{}-{}{}",
                PAYLOADS[(random.next() as usize) % PAYLOADS.len()],
                index,
                "x".repeat((random.next() % 9) as usize)
            ),
        };
        let flag = if index == 0 {
            false
        } else if index == 1 {
            true
        } else {
            random.next() & 1 == 0
        };
        rows.push(AuditRow {
            id: index as i64,
            signed_value,
            measure,
            boundary_float,
            score,
            low_key,
            high_key,
            payload,
            flag,
        });
    }
    rows
}

fn setup_sql(rows: &[AuditRow]) -> String {
    let mut sql = String::with_capacity(rows.len().saturating_mul(180));
    writeln!(
        sql,
        "CREATE TABLE {TABLE_NAME} (id Int64, signed_value Int64, measure Int64, boundary_float Float64, score Float64, low_key String, high_key String, payload String, flag Bool);"
    )
    .expect("writing to String cannot fail");
    write!(sql, "INSERT INTO {TABLE_NAME} VALUES ").expect("writing to String cannot fail");
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            sql.push(',');
        }
        write!(
            sql,
            "({},{},{},{},{},'{}','{}','{}',{})",
            row.id,
            row.signed_value,
            row.measure,
            float_literal(row.boundary_float),
            float_literal(row.score),
            escape_sql_string(&row.low_key),
            escape_sql_string(&row.high_key),
            escape_sql_string(&row.payload),
            row.flag
        )
        .expect("writing to String cannot fail");
    }
    sql.push_str(";\n");
    sql
}

#[derive(Debug)]
struct Item {
    expression: String,
    label: &'static str,
    column_type: ColumnType,
}

impl Item {
    fn new(expression: impl Into<String>, label: &'static str, column_type: ColumnType) -> Self {
        Self {
            expression: expression.into(),
            label,
            column_type,
        }
    }
}

fn make_case(
    index: usize,
    family: AuditFamily,
    items: Vec<Item>,
    max_rows: usize,
    tail: impl FnOnce(&[String]) -> String,
) -> AuditCase {
    let id = format!("q{index:04}");
    let aliases = items
        .iter()
        .map(|item| format!("{id}_{}", item.label))
        .collect::<Vec<_>>();
    let mut sql = String::from("SELECT ");
    for (item_index, (item, alias)) in items.iter().zip(&aliases).enumerate() {
        if item_index > 0 {
            sql.push_str(", ");
        }
        write!(sql, "{} AS {alias}", item.expression).expect("writing to String cannot fail");
    }
    writeln!(sql, " FROM {TABLE_NAME}{};", tail(&aliases)).expect("writing to String cannot fail");

    let columns = items
        .into_iter()
        .zip(aliases)
        .map(|(item, alias)| (alias, item.column_type))
        .collect();
    AuditCase {
        id,
        family,
        sql,
        columns,
        max_rows,
    }
}

fn generate_cases(seed: u64, rows: &[AuditRow]) -> Vec<AuditCase> {
    let mut random = SplitMix64::new(seed ^ 0xe703_7ed1_a0b4_28db);
    let mut cases = Vec::with_capacity(AUDIT_QUERY_COUNT);

    for iteration in 0..QUERIES_PER_FAMILY {
        let index = cases.len();
        let row = &rows[iteration % 16];
        let predicate = match iteration % 5 {
            0 => format!("id = {}", row.id),
            1 => format!("signed_value = {}", row.signed_value),
            2 => format!("boundary_float = {}", float_literal(row.boundary_float)),
            3 => format!("flag = {} AND id <= {}", row.flag, row.id + 16),
            _ => format!("payload = '{}'", escape_sql_string(&row.payload)),
        };
        cases.push(make_case(
            index,
            AuditFamily::ScalarBoundaries,
            vec![
                Item::new("id", "id", ColumnType::Integer),
                Item::new("signed_value", "signed", ColumnType::Integer),
                Item::new("boundary_float", "float", ColumnType::Float),
                Item::new("payload", "text", ColumnType::String),
                Item::new("flag", "flag", ColumnType::Boolean),
            ],
            8,
            |aliases| format!(" WHERE {predicate} ORDER BY {} LIMIT 8", aliases[0]),
        ));
    }

    let operators = ["=", "!=", "<>", "<", "<=", ">", ">="];
    for iteration in 0..QUERIES_PER_FAMILY {
        let index = cases.len();
        let measure = (random.next() % 1_801) as i64 - 900;
        let score = ((random.next() % 120_001) as i64 - 60_000) as f64 / 8.0;
        let key = &rows[(random.next() as usize) % rows.len()].low_key;
        let flag = random.next() & 1 == 0;
        let limit = 1 + (random.next() % 32) as usize;
        let predicate = format!(
            "(measure {} {measure} AND score {} {}) OR (flag = {flag} AND low_key != '{}')",
            operators[iteration % operators.len()],
            operators[(iteration * 3 + 1) % operators.len()],
            float_literal(score),
            escape_sql_string(key)
        );
        cases.push(make_case(
            index,
            AuditFamily::Predicates,
            vec![
                Item::new("id", "id", ColumnType::Integer),
                Item::new("measure", "measure", ColumnType::Integer),
                Item::new("score", "score", ColumnType::Float),
                Item::new("flag", "flag", ColumnType::Boolean),
            ],
            limit,
            |aliases| format!(" WHERE {predicate} ORDER BY {} LIMIT {limit}", aliases[0]),
        ));
    }

    for iteration in 0..QUERIES_PER_FAMILY {
        let index = cases.len();
        let start = (random.next() % 225) as usize;
        let end = start + 31;
        let limit = 1 + (random.next() % 24) as usize;
        let items = match iteration % 4 {
            0 => vec![
                Item::new("id", "id", ColumnType::Integer),
                Item::new("payload", "text", ColumnType::String),
            ],
            1 => vec![
                Item::new("id", "id", ColumnType::Integer),
                Item::new("signed_value", "signed", ColumnType::Integer),
                Item::new("boundary_float", "float", ColumnType::Float),
            ],
            2 => vec![
                Item::new("id", "id", ColumnType::Integer),
                Item::new("low_key", "key", ColumnType::String),
                Item::new("flag", "flag", ColumnType::Boolean),
                Item::new("score", "score", ColumnType::Float),
            ],
            _ => vec![
                Item::new("id", "id", ColumnType::Integer),
                Item::new("measure", "measure", ColumnType::Integer),
                Item::new("high_key", "entity", ColumnType::String),
                Item::new("payload", "text", ColumnType::String),
                Item::new("flag", "flag", ColumnType::Boolean),
            ],
        };
        cases.push(make_case(
            index,
            AuditFamily::Projections,
            items,
            limit,
            |aliases| {
                format!(
                    " WHERE id >= {start} AND id <= {end} ORDER BY {} DESC LIMIT {limit}",
                    aliases[0]
                )
            },
        ));
    }

    for _ in 0..QUERIES_PER_FAMILY {
        let index = cases.len();
        let start = (random.next() % 225) as usize;
        let end = start + 31;
        cases.push(make_case(
            index,
            AuditFamily::Aggregates,
            vec![
                Item::new("COUNT(*)", "count", ColumnType::Integer),
                Item::new("SUM(measure)", "sum", ColumnType::Integer),
                Item::new("MIN(signed_value)", "min", ColumnType::Integer),
                Item::new("MAX(signed_value)", "max", ColumnType::Integer),
                Item::new("AVG(score)", "mean", ColumnType::Float),
            ],
            1,
            |_| format!(" WHERE id >= {start} AND id <= {end}"),
        ));
    }

    for iteration in 0..QUERIES_PER_FAMILY {
        let index = cases.len();
        let start = (random.next() % 129) as usize;
        let end = start + 127;
        let (items, group_by) = match iteration % 4 {
            0 => (
                vec![
                    Item::new("low_key", "key", ColumnType::String),
                    Item::new("COUNT(*)", "count", ColumnType::Integer),
                    Item::new("SUM(measure)", "sum", ColumnType::Integer),
                    Item::new("AVG(score)", "mean", ColumnType::Float),
                ],
                "low_key",
            ),
            1 => (
                vec![
                    Item::new("flag", "flag", ColumnType::Boolean),
                    Item::new("COUNT(*)", "count", ColumnType::Integer),
                    Item::new("MIN(measure)", "min", ColumnType::Integer),
                    Item::new("MAX(measure)", "max", ColumnType::Integer),
                ],
                "flag",
            ),
            2 => (
                vec![
                    Item::new("low_key", "key", ColumnType::String),
                    Item::new("flag", "flag", ColumnType::Boolean),
                    Item::new("COUNT(*)", "count", ColumnType::Integer),
                    Item::new("SUM(measure)", "sum", ColumnType::Integer),
                ],
                "low_key, flag",
            ),
            _ => (
                vec![
                    Item::new("high_key", "entity", ColumnType::String),
                    Item::new("COUNT(*)", "count", ColumnType::Integer),
                    Item::new("SUM(measure)", "sum", ColumnType::Integer),
                ],
                "high_key",
            ),
        };
        cases.push(make_case(
            index,
            AuditFamily::Grouping,
            items,
            16,
            |aliases| {
                let order = aliases
                    .iter()
                    .take(if iteration % 4 == 2 { 2 } else { 1 })
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    " WHERE id >= {start} AND id <= {end} GROUP BY {group_by} ORDER BY {order} LIMIT 16"
                )
            },
        ));
    }

    for iteration in 0..QUERIES_PER_FAMILY {
        let index = cases.len();
        let limit = 1 + (random.next() % 32) as usize;
        let (column, label, column_type) = match iteration % 6 {
            0 => ("signed_value", "signed", ColumnType::Integer),
            1 => ("boundary_float", "float", ColumnType::Float),
            2 => ("score", "score", ColumnType::Float),
            3 => ("low_key", "key", ColumnType::String),
            4 => ("payload", "text", ColumnType::String),
            _ => ("flag", "flag", ColumnType::Boolean),
        };
        let descending = if iteration.is_multiple_of(2) {
            " DESC"
        } else {
            " ASC"
        };
        cases.push(make_case(
            index,
            AuditFamily::Ordering,
            vec![
                Item::new("id", "id", ColumnType::Integer),
                Item::new(column, label, column_type),
                Item::new("high_key", "entity", ColumnType::String),
            ],
            limit,
            |aliases| {
                format!(
                    " ORDER BY {}{descending}, {} ASC LIMIT {limit}",
                    aliases[1], aliases[0]
                )
            },
        ));
    }

    for iteration in 0..QUERIES_PER_FAMILY {
        let index = cases.len();
        let limit = iteration % 33;
        let start = (random.next() % 97) as usize;
        cases.push(make_case(
            index,
            AuditFamily::Limits,
            vec![
                Item::new("id", "id", ColumnType::Integer),
                Item::new("measure", "measure", ColumnType::Integer),
                Item::new("payload", "text", ColumnType::String),
                Item::new("flag", "flag", ColumnType::Boolean),
            ],
            limit,
            |aliases| {
                format!(
                    " WHERE id >= {start} ORDER BY {} ASC LIMIT {limit}",
                    aliases[0]
                )
            },
        ));
    }

    debug_assert_eq!(cases.len(), AUDIT_QUERY_COUNT);
    cases
}

fn float_literal(value: f64) -> String {
    format!("{value:.17e}")
}

fn escape_sql_string(value: &str) -> String {
    value.replace('\'', "''")
}

#[derive(Debug, Clone, Copy)]
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corpus_is_reproducible_and_runtime_seeded() {
        let first = AuditCorpus::generate(42);
        let repeated = AuditCorpus::generate(42);
        let different = AuditCorpus::generate(43);

        assert_eq!(first.replay_sql, repeated.replay_sql);
        assert_eq!(first.corpus_sha256, repeated.corpus_sha256);
        assert_ne!(first.setup_sha256, different.setup_sha256);
        assert_ne!(first.queries_sha256, different.queries_sha256);
        assert_ne!(first.corpus_sha256, different.corpus_sha256);
    }

    #[test]
    fn corpus_has_hundreds_of_bounded_queries_across_every_family() {
        let corpus = AuditCorpus::generate(20_260_729);
        assert_eq!(corpus.row_count, AUDIT_ROW_COUNT);
        assert_eq!(corpus.cases.len(), 336);
        assert_eq!(
            corpus.setup_sha256,
            "ccad37505e1d27ac031d372c57977b3743a216ffbb296696639ff983c54c7ca9"
        );
        assert_eq!(
            corpus.queries_sha256,
            "e4ba78092924cea45b9c9e210ecbdf249ed5a8e790629a8f2fe2098e3eac8217"
        );
        assert_eq!(
            corpus.corpus_sha256,
            "bb50c91360348fd5229728f7952fc5bd7b9b305f5e53edfb428946d7d3b2463d"
        );
        assert_eq!(
            corpus.family_counts().values().copied().collect::<Vec<_>>(),
            vec![QUERIES_PER_FAMILY; AuditFamily::ALL.len()]
        );
        assert!(corpus.cases.iter().all(|case| case.max_rows <= 32));
        assert!(corpus.cases.iter().all(|case| case.sql.ends_with(";\n")));
        assert!(corpus.replay_sql.contains(&i64::MIN.to_string()));
        assert!(corpus.replay_sql.contains(&i64::MAX.to_string()));
        assert!(corpus.replay_sql.contains("GROUP BY"));
        assert!(corpus.replay_sql.contains("ORDER BY"));
        assert!(corpus.replay_sql.contains("LIMIT 0"));
        assert!(corpus.replay_sql.contains("AVG("));
    }

    #[test]
    fn every_query_uses_unique_result_headers() {
        let corpus = AuditCorpus::generate(7);
        let mut names = corpus
            .cases
            .iter()
            .flat_map(|case| case.columns.iter().map(|(name, _)| name))
            .collect::<Vec<_>>();
        let original_len = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), original_len);
    }
}
