const README: &str = include_str!("../README.md");

fn normalized_readme() -> String {
    README.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn readme_documents_the_bounded_nullable_int64_sql_ddl_shape() {
    let readme = normalized_readme();
    let expected = "Physical column vectors support `Int64`, `Nullable(Int64)`, `Bool`, \
                    `Float64`, and `String` storage. SQL accepts the exact one-column \
                    `CREATE TABLE <name> (<column> Nullable(Int64))` shape case-insensitively; \
                    other nullable types and nullable multi-column declarations remain outside \
                    the bounded grammar.";
    let expected = expected.split_whitespace().collect::<Vec<_>>().join(" ");

    assert!(
        readme.contains(&expected),
        "README must document the exact SQL Nullable(Int64) declaration boundary"
    );

    for stale_claim in [
        "Batch columns are currently non-nullable",
        "batch engine's existing non-nullable physical-column",
        "expressions, nullable storage, placement clauses",
        "SQL DDL cannot currently declare nullable columns",
    ] {
        assert!(
            !readme.contains(stale_claim),
            "README contains the obsolete nullable-storage claim: {stale_claim}"
        );
    }
}
