const README: &str = include_str!("../README.md");

fn normalized_readme() -> String {
    README.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn readme_distinguishes_physical_nullable_storage_from_sql_ddl() {
    let readme = normalized_readme();
    let expected = "Physical column vectors support `Int64`, `Nullable(Int64)`, `Bool`, \
                    `Float64`, and `String` storage. SQL DDL cannot currently declare nullable \
                    columns; `Nullable(Int64)` storage is instead created through library APIs \
                    or WAL recovery.";
    let expected = expected.split_whitespace().collect::<Vec<_>>().join(" ");

    assert!(
        readme.contains(&expected),
        "README must distinguish supported physical Nullable(Int64) storage from the SQL DDL limitation"
    );

    for stale_claim in [
        "Batch columns are currently non-nullable",
        "batch engine's existing non-nullable physical-column",
        "expressions, nullable storage, placement clauses",
    ] {
        assert!(
            !readme.contains(stale_claim),
            "README contains the obsolete nullable-storage claim: {stale_claim}"
        );
    }
}
