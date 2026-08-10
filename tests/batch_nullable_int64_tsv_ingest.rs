use rusthouse::batch::engine::{Database, QueryResult, ResultColumn, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::format::{write_tsv, write_tsv_rows};
use rusthouse::batch::storage::TableLimits;
use rusthouse::batch::tsv::{TsvIngestError, TsvIngestLimits};
use rusthouse::batch::value::{DataType, Value};

fn nullable_database(row_cap: usize) -> Database {
    let mut database = Database::with_table_limits(TableLimits::new(row_cap, 1, row_cap));
    database
        .execute("CREATE TABLE readings (measurement Nullable(Int64));")
        .expect("create nullable table");
    database
}

fn query(database: &mut Database, sql: &str) -> QueryResult {
    match database.execute(sql).unwrap().remove(0) {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

fn values(database: &mut Database) -> Vec<Vec<Value>> {
    query(database, "SELECT measurement FROM readings;").rows
}

#[test]
fn nullable_writer_output_round_trips_with_and_without_names() {
    let mut source = nullable_database(3);
    source
        .execute(
            "INSERT INTO readings VALUES (-9223372036854775808), (NULL), (9223372036854775807);",
        )
        .unwrap();
    let expected = query(&mut source, "SELECT measurement FROM readings;");

    let mut named = Vec::new();
    write_tsv(&mut named, &expected).unwrap();
    assert_eq!(
        named,
        b"measurement\n-9223372036854775808\n\\N\n9223372036854775807\n"
    );
    let mut named_target = nullable_database(3);
    assert_eq!(
        named_target.ingest_tsv_with_names(
            "readings",
            &named,
            TsvIngestLimits::new(named.len(), 3, 3),
        ),
        Ok(3),
    );
    assert_eq!(
        query(&mut named_target, "SELECT measurement FROM readings;"),
        expected
    );

    let mut headerless = Vec::new();
    write_tsv_rows(&mut headerless, &expected).unwrap();
    assert_eq!(
        headerless,
        b"-9223372036854775808\n\\N\n9223372036854775807\n"
    );
    let mut headerless_target = nullable_database(3);
    assert_eq!(
        headerless_target.ingest_tsv(
            "readings",
            &headerless,
            TsvIngestLimits::new(headerless.len(), 3, 3),
        ),
        Ok(3),
    );
    assert_eq!(
        query(&mut headerless_target, "SELECT measurement FROM readings;"),
        expected
    );
}

#[test]
fn raw_null_is_physical_nullable_only_and_escaped_string_is_preserved() {
    let raw_null = b"\\N\n";

    let mut nullable = nullable_database(2);
    assert_eq!(
        nullable.ingest_tsv(
            "readings",
            raw_null,
            TsvIngestLimits::new(raw_null.len(), 1, 1),
        ),
        Ok(1),
    );
    let named_null = b"measurement\n\\N\n";
    assert_eq!(
        nullable.ingest_tsv_with_names(
            "readings",
            named_null,
            TsvIngestLimits::new(named_null.len(), 1, 1),
        ),
        Ok(1),
    );
    assert_eq!(
        values(&mut nullable),
        [
            vec![Value::Null(DataType::Int64)],
            vec![Value::Null(DataType::Int64)]
        ]
    );

    let mut integers = Database::new();
    integers
        .execute("CREATE TABLE integers (value Int64);")
        .unwrap();
    assert_eq!(
        integers.ingest_tsv(
            "integers",
            raw_null,
            TsvIngestLimits::new(raw_null.len(), 1, 1),
        ),
        Err(TsvIngestError::InvalidEscape { line: 1, column: 1 }),
    );
    let named_integer = b"value\n\\N\n";
    assert_eq!(
        integers.ingest_tsv_with_names(
            "integers",
            named_integer,
            TsvIngestLimits::new(named_integer.len(), 1, 1),
        ),
        Err(TsvIngestError::InvalidEscape { line: 2, column: 1 }),
    );

    let mut strings = Database::new();
    strings
        .execute("CREATE TABLE strings (value String);")
        .unwrap();
    assert_eq!(
        strings.ingest_tsv(
            "strings",
            raw_null,
            TsvIngestLimits::new(raw_null.len(), 1, 1),
        ),
        Err(TsvIngestError::InvalidEscape { line: 1, column: 1 }),
    );

    let string_result = QueryResult {
        columns: vec![ResultColumn {
            name: "value".to_owned(),
            data_type: DataType::String,
        }],
        rows: vec![vec![Value::String(r"\N".to_owned())]],
    };
    let mut escaped_string = Vec::new();
    write_tsv_rows(&mut escaped_string, &string_result).unwrap();
    assert_eq!(escaped_string, b"\\\\N\n");
    assert_eq!(
        strings.ingest_tsv(
            "strings",
            &escaped_string,
            TsvIngestLimits::new(escaped_string.len(), 1, 1),
        ),
        Ok(1),
    );
    let mut named_string = Vec::new();
    write_tsv(&mut named_string, &string_result).unwrap();
    assert_eq!(named_string, b"value\n\\\\N\n");
    assert_eq!(
        strings.ingest_tsv_with_names(
            "strings",
            &named_string,
            TsvIngestLimits::new(named_string.len(), 1, 1),
        ),
        Ok(1),
    );
    assert_eq!(
        query(&mut strings, "SELECT value FROM strings;").rows,
        [
            vec![Value::String(r"\N".to_owned())],
            vec![Value::String(r"\N".to_owned())]
        ]
    );
}

#[test]
fn late_malformed_fields_and_capacity_failures_roll_back_nullable_rows() {
    let mut malformed = nullable_database(4);
    malformed
        .execute("INSERT INTO readings VALUES (7);")
        .unwrap();
    let named = b"measurement\n\\N\n-2\nbad\\x\n";
    assert_eq!(
        malformed
            .ingest_tsv_with_names("readings", named, TsvIngestLimits::new(named.len(), 3, 3),),
        Err(TsvIngestError::InvalidEscape { line: 4, column: 1 }),
    );
    assert_eq!(values(&mut malformed), [vec![Value::Int64(7)]]);

    let mut bounded = nullable_database(1);
    bounded.execute("INSERT INTO readings VALUES (9);").unwrap();
    assert_eq!(
        bounded.ingest_tsv(
            "readings",
            raw_nullable_rows(),
            TsvIngestLimits::new(raw_nullable_rows().len(), 2, 2),
        ),
        Err(TsvIngestError::Database(Error::ResourceLimitExceeded {
            resource: "table rows",
            actual: 3,
            max: 1,
        })),
    );
    assert_eq!(values(&mut bounded), [vec![Value::Int64(9)]]);
}

fn raw_nullable_rows() -> &'static [u8] {
    b"\\N\n1\n"
}
