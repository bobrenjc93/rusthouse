use std::sync::{Arc, Barrier, RwLock};
use std::thread;

use rusthouse::{
    Catalog, CatalogError, CatalogLimits, InsertError, InsertExecutionError, ParseLimits,
    SharedCatalog, SharedCatalogError,
};

fn shared_catalog(max_rows_per_table: usize) -> SharedCatalog {
    let catalog = SharedCatalog::with_limits(CatalogLimits::new(1, max_rows_per_table));
    catalog
        .execute_create(
            "CREATE TABLE readings (value Int64 NULL)",
            ParseLimits::default(),
        )
        .unwrap();
    catalog
}

#[test]
fn readers_observe_consistent_owned_snapshots() {
    const ROWS: i64 = 6;

    let catalog = shared_catalog(ROWS as usize);
    let row_published = Arc::new(Barrier::new(2));
    let read_finished = Arc::new(Barrier::new(2));

    let writer_catalog = catalog.clone();
    let writer_published = Arc::clone(&row_published);
    let writer_finished = Arc::clone(&read_finished);
    let writer = thread::spawn(move || {
        for value in 0..ROWS {
            writer_catalog
                .execute_insert(
                    &format!("INSERT INTO readings VALUES ({value})"),
                    ParseLimits::default(),
                )
                .unwrap();
            writer_published.wait();
            writer_finished.wait();
        }
    });

    let reader_catalog = catalog.clone();
    let reader_published = Arc::clone(&row_published);
    let reader_finished = Arc::clone(&read_finished);
    let reader = thread::spawn(move || {
        let mut snapshots = Vec::new();
        for _ in 0..ROWS {
            reader_published.wait();
            let rows: Vec<Option<i64>> = reader_catalog
                .execute_select("SELECT value FROM readings", ParseLimits::default())
                .unwrap();
            snapshots.push(rows);
            reader_finished.wait();
        }
        snapshots
    });

    writer.join().unwrap();
    let snapshots = reader.join().unwrap();

    for (index, snapshot) in snapshots.iter().enumerate() {
        let expected = (0..=index as i64).map(Some).collect::<Vec<_>>();
        assert_eq!(snapshot, &expected);
    }
    assert_eq!(snapshots[0], vec![Some(0)]);
}

#[test]
fn concurrent_inserts_are_serialized_without_lost_rows() {
    const WRITERS: usize = 16;

    let catalog = shared_catalog(WRITERS);
    let start = Arc::new(Barrier::new(WRITERS + 1));
    let mut writers = Vec::new();

    for value in 0..WRITERS {
        let writer_catalog = catalog.clone();
        let writer_start = Arc::clone(&start);
        writers.push(thread::spawn(move || {
            writer_start.wait();
            writer_catalog.execute_insert(
                &format!("INSERT INTO readings VALUES ({value})"),
                ParseLimits::default(),
            )
        }));
    }

    start.wait();
    for writer in writers {
        writer.join().unwrap().unwrap();
    }

    let mut rows = catalog
        .execute_select("SELECT value FROM readings", ParseLimits::default())
        .unwrap();
    rows.sort_unstable();
    assert_eq!(rows, (0..WRITERS as i64).map(Some).collect::<Vec<_>>());
}

#[test]
fn concurrent_inserts_enforce_the_shared_row_bound() {
    const ROW_CAP: usize = 4;
    const WRITERS: usize = 12;

    let catalog = shared_catalog(ROW_CAP);
    let start = Arc::new(Barrier::new(WRITERS + 1));
    let mut writers = Vec::new();

    for value in 0..WRITERS {
        let writer_catalog = catalog.clone();
        let writer_start = Arc::clone(&start);
        writers.push(thread::spawn(move || {
            writer_start.wait();
            writer_catalog.execute_insert(
                &format!("INSERT INTO readings VALUES ({value})"),
                ParseLimits::default(),
            )
        }));
    }

    start.wait();
    let results = writers
        .into_iter()
        .map(|writer| writer.join().unwrap())
        .collect::<Vec<_>>();

    assert_eq!(
        results.iter().filter(|result| result.is_ok()).count(),
        ROW_CAP
    );
    for error in results.into_iter().filter_map(Result::err) {
        assert_eq!(
            error,
            SharedCatalogError::Catalog(CatalogError::Insert(InsertExecutionError::Insert(
                InsertError::RowCapExceeded {
                    row_cap: ROW_CAP,
                    current_rows: ROW_CAP,
                    incoming_rows: 1,
                }
            )))
        );
    }
    assert_eq!(
        catalog
            .execute_select("SELECT value FROM readings", ParseLimits::default())
            .unwrap()
            .len(),
        ROW_CAP
    );
}

#[test]
fn poisoned_lock_is_reported_as_a_typed_error() {
    let inner = Arc::new(RwLock::new(Catalog::new(CatalogLimits::new(1, 1))));
    let catalog = SharedCatalog::from_arc(Arc::clone(&inner));
    let poisoner = thread::spawn(move || {
        let _guard = inner.write().unwrap();
        panic!("poison the catalog lock");
    });

    assert!(poisoner.join().is_err());
    assert_eq!(
        catalog.execute_create(
            "CREATE TABLE readings (value Int64)",
            ParseLimits::default(),
        ),
        Err(SharedCatalogError::LockPoisoned)
    );
    assert_eq!(
        catalog.execute_select("SELECT value FROM readings", ParseLimits::default()),
        Err(SharedCatalogError::LockPoisoned)
    );
}
