mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use common::compress_body;
use seekzstdsep::RecordReader;
use tempfile::tempdir;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

struct CountingAllocator;

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        COUNTING.with(|enabled| {
            if enabled.get() {
                ALLOCATIONS.with(|count| count.set(count.get() + 1));
            }
        });
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        COUNTING.with(|enabled| {
            if enabled.get() {
                ALLOCATIONS.with(|count| count.set(count.get() + 1));
            }
        });
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        COUNTING.with(|enabled| {
            if enabled.get() {
                ALLOCATIONS.with(|count| count.set(count.get() + 1));
            }
        });
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

fn counted<T>(f: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<(T, usize)> {
    ALLOCATIONS.with(|count| count.set(0));
    COUNTING.with(|enabled| enabled.set(true));
    let result = f();
    COUNTING.with(|enabled| enabled.set(false));
    Ok((result?, ALLOCATIONS.with(Cell::get)))
}

#[test]
fn borrowed_filter_allocates_less_than_owned_filter() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let body = (0..1_000)
        .map(|i| format!("{}-{i:04}\n", if i % 10 == 0 { "hit" } else { "miss" }))
        .collect::<String>();
    let path = compress_body(dir.path(), "allocation-records", body.as_bytes());

    let mut borrowed = RecordReader::open(path.clone(), b"\n")?;
    let (borrowed_hits, borrowed_allocations) = counted(|| {
        borrowed
            .records_from(0)
            .filter(|record| {
                record
                    .as_ref()
                    .map_or(true, |record| record.starts_with(b"hit"))
            })
            .map(|record| record.map(|record| record.to_vec()))
            .collect::<anyhow::Result<Vec<_>>>()
    })?;

    let owned = RecordReader::open(path, b"\n")?;
    let (owned_hits, owned_allocations) = counted(|| {
        owned
            .into_records_from(0)
            .filter(|record| {
                record
                    .as_ref()
                    .map_or(true, |record| record.starts_with(b"hit"))
            })
            .collect::<anyhow::Result<Vec<_>>>()
    })?;

    assert_eq!(borrowed_hits.len(), 100);
    assert_eq!(owned_hits, borrowed_hits);
    eprintln!(
        "borrowed: {borrowed_allocations} allocations; owned: {owned_allocations} allocations"
    );
    assert!(
        borrowed_allocations < owned_allocations,
        "borrowed: {borrowed_allocations} allocations, owned: {owned_allocations} allocations"
    );
    Ok(())
}
