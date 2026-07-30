use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use rusthouse::storage::BLOCK_SIZE;
use rusthouse::{Database, StatementResult, Value};

struct TrackingAllocator;

static TRACK_ALLOCATIONS: AtomicBool = AtomicBool::new(false);
static ALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);

#[global_allocator]
static ALLOCATOR: TrackingAllocator = TrackingAllocator;

unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if TRACK_ALLOCATIONS.load(Ordering::Relaxed) {
            ALLOCATED_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if TRACK_ALLOCATIONS.load(Ordering::Relaxed) {
            ALLOCATED_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let pointer = unsafe { System.realloc(pointer, layout, new_size) };
        if TRACK_ALLOCATIONS.load(Ordering::Relaxed) {
            ALLOCATED_BYTES.fetch_add(new_size, Ordering::Relaxed);
        }
        pointer
    }
}

#[test]
fn selective_scan_does_not_allocate_for_pruned_rows() {
    const ROW_COUNT: usize = BLOCK_SIZE * 128;
    const INSERT_ROWS: usize = BLOCK_SIZE * 8;

    let mut database = Database::new();
    database
        .execute("CREATE TABLE indexed (n Int64);")
        .expect("create succeeds");
    for start in (0..ROW_COUNT).step_by(INSERT_ROWS) {
        let values = (start..start + INSERT_ROWS)
            .map(|value| format!("({value})"))
            .collect::<Vec<_>>()
            .join(",");
        database
            .execute(&format!("INSERT INTO indexed VALUES {values};"))
            .expect("insert succeeds");
    }

    ALLOCATED_BYTES.store(0, Ordering::Relaxed);
    TRACK_ALLOCATIONS.store(true, Ordering::Relaxed);
    let results = database
        .execute(&format!(
            "SELECT n FROM indexed WHERE n = {};",
            ROW_COUNT - 1
        ))
        .expect("select succeeds");
    TRACK_ALLOCATIONS.store(false, Ordering::Relaxed);
    let allocated = ALLOCATED_BYTES.load(Ordering::Relaxed);

    let StatementResult::Query(result) = &results[0] else {
        panic!("expected query result");
    };
    assert_eq!(
        result.rows,
        vec![vec![Value::Int64((ROW_COUNT - 1) as i64)]]
    );

    let admitted_block_budget = BLOCK_SIZE * size_of::<usize>() * 8;
    assert!(
        allocated < admitted_block_budget,
        "selective scan allocated {allocated} bytes; expected less than {admitted_block_budget}"
    );
}
