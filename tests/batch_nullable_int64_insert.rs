use rusthouse::batch::engine::Database;
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{Statement, parse};
use rusthouse::batch::storage::Column;
use rusthouse::batch::value::{DataType, Value};

fn nullable_values(database: &Database, table: &str) -> Vec<Option<i64>> {
    let Column::NullableInt64(values) = &database.catalog().table(table).unwrap().columns()[0]
    else {
        panic!("expected a physical NullableInt64 column")
    };
    values.clone()
}

#[test]
fn insert_parser_maps_bare_null_to_the_existing_typed_int64_null() {
    assert_eq!(
        parse("INSERT INTO readings VALUES (1), (nUlL), (-2)").unwrap(),
        vec![Statement::Insert {
            table: "readings".to_owned(),
            rows: vec![
                vec![Value::Int64(1)],
                vec![Value::Null(DataType::Int64)],
                vec![Value::Int64(-2)],
            ],
        }]
    );

    assert!(parse("SELECT NULL").is_err());
    assert!(parse("DELETE FROM readings WHERE value = NULL").is_err());
}

#[test]
fn positional_and_explicit_column_inserts_store_mixed_nullable_int64_rows() {
    let mut database = Database::new();
    database
        .create_nullable_int64_table("Readings", "Measurement", Vec::new())
        .unwrap();

    database
        .execute("INSERT INTO readings VALUES (7), (NULL), (-2);")
        .unwrap();
    database
        .execute("INSERT INTO READINGS (MEASUREMENT) VALUES (NULL), (9);")
        .unwrap();

    assert_eq!(
        nullable_values(&database, "readings"),
        [Some(7), None, Some(-2), None, Some(9)]
    );
}

#[test]
fn null_rejection_is_atomic_for_every_non_nullable_physical_target() {
    for (data_type, present) in [
        ("Int64", "1"),
        ("Float64", "1.5"),
        ("Bool", "true"),
        ("String", "'present'"),
    ] {
        let mut database = Database::new();
        database
            .execute(&format!("CREATE TABLE target (value {data_type});"))
            .unwrap();

        assert_eq!(
            database.execute(&format!(
                "INSERT INTO target VALUES ({present}), (NULL), ({present});"
            )),
            Err(Error::TypeMismatch {
                context: "column 'target.value'".to_owned(),
                expected: data_type.to_owned(),
                actual: "NULL".to_owned(),
            }),
            "physical {data_type} target"
        );
        assert_eq!(database.catalog().table("target").unwrap().row_count(), 0);
    }
}

#[test]
fn insert_only_batch_rejects_a_non_nullable_target_before_publishing_any_table() {
    let mut database = Database::new();
    database
        .create_nullable_int64_table("nullable", "value", Vec::new())
        .unwrap();
    database
        .execute("CREATE TABLE required (value Int64);")
        .unwrap();

    assert_eq!(
        database.execute_insert_batch(
            "INSERT INTO nullable VALUES (NULL); \
             INSERT INTO required VALUES (1), (NULL);",
        ),
        Err(Error::TypeMismatch {
            context: "column 'required.value'".to_owned(),
            expected: "Int64".to_owned(),
            actual: "NULL".to_owned(),
        })
    );
    assert!(nullable_values(&database, "nullable").is_empty());
    assert_eq!(database.catalog().table("required").unwrap().row_count(), 0);
}
