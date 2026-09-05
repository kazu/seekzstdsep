//! What the compressor holds, in bytes.
//!
//! ```sh
//! cargo bench --bench memory
//! ```
//!
//! One line per case: the peak bytes handed out and not yet returned while the case ran, counted
//! by the allocator this binary installs. Neither the compressor nor anything it calls is
//! instrumented, so what is measured is whatever the case allocates, and the output goes to a sink
//! so that only the input side is counted.
//!
//! The number is deterministic — one thread, one input, no clock — so two builds are compared by
//! running this in each and diffing the output. That is what it is for: the buffer the compressor
//! reads into is bounded by `frame_size * limit_multiplier` and by where frames are cut, and
//! neither bound is stated anywhere that a test could fail on. `holds a frame's records` is the
//! case that would move if a change made the buffer follow the input size instead.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use seekzstdsep::{CompressOptions, convert_to_seekable_zst_reader_with_opts};

/// Bytes handed out and not yet returned.
static IN_USE: AtomicUsize = AtomicUsize::new(0);
/// The highest [`IN_USE`] has been since the last case started.
static PEAK: AtomicUsize = AtomicUsize::new(0);

/// The system allocator, counted.
struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            grew(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        IN_USE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if !ptr.is_null() {
            IN_USE.fetch_sub(layout.size(), Ordering::Relaxed);
            grew(new_size);
        }
        ptr
    }
}

fn grew(size: usize) {
    let in_use = IN_USE.fetch_add(size, Ordering::Relaxed) + size;
    PEAK.fetch_max(in_use, Ordering::Relaxed);
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// How far above what was already held `f` took the allocator.
///
/// The baseline is what the input itself takes, which is allocated before the case starts and is
/// not what the case is about.
fn peak_of(f: impl FnOnce()) -> usize {
    let baseline = IN_USE.load(Ordering::Relaxed);
    PEAK.store(baseline, Ordering::Relaxed);
    f();
    PEAK.load(Ordering::Relaxed).saturating_sub(baseline)
}

/// A record of `len` bytes, separator included, distinguishable by `seq`.
fn record(seq: usize, len: usize) -> Vec<u8> {
    let mut body = seq.to_string().into_bytes();
    body.resize(len - 1, b'x');
    body.push(b'\n');
    body
}

/// `count` records of `len` bytes.
fn records(count: usize, len: usize) -> Vec<u8> {
    (0..count).flat_map(|seq| record(seq, len)).collect()
}

const SEPARATOR: &[u8] = b"\n";
const FRAME_SIZE: usize = 65536;

fn main() {
    // The input is built before the case runs, so it is in the baseline rather than in the peak.
    let cases: Vec<(&str, Vec<u8>, usize)> = vec![
        // The one that says the buffer does not follow the input: twenty times the records of the
        // case above it, in a buffer that should not be twenty times larger.
        ("holds a frame's records", records(7_000, 137), FRAME_SIZE),
        (
            "twenty times as many records",
            records(140_000, 137),
            FRAME_SIZE,
        ),
        // A record that no frame size accommodates, which the buffer has to hold whole.
        ("one record of 100 KB", record(0, 100_000), FRAME_SIZE),
        // Nothing to cut on: the buffer grows to the limit and the compressor refuses.
        ("no separator at all", vec![b'x'; 5_000_000], FRAME_SIZE),
        // A smaller target, so the limit that bounds the buffer is smaller with it.
        ("a smaller frame target", records(140_000, 137), 16384),
    ];

    println!("case\tframe_size\tpeak_bytes");
    for (label, input, frame_size) in &cases {
        let peak = peak_of(|| {
            // The result is discarded: "no separator at all" is refused, and the refusal is as
            // much a case as the compressions are.
            let _ = convert_to_seekable_zst_reader_with_opts(
                &input[..],
                std::io::sink(),
                *frame_size,
                true,
                SEPARATOR,
                None,
                Some(CompressOptions::default()),
            );
        });
        println!("{label}\t{frame_size}\t{peak}");
    }
}
