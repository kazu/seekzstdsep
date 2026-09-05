//! What the `edit` operations hold while they count a frame's records, in bytes.
//!
//! Counting is what every operation there is built out of: two frames for separator validation, one
//! for the frame a file ends with, and the whole range where a range is checked. None of it needs
//! the bytes, so none of it may hold a frame — the record window is what it reads through, and that
//! is the same size whatever the file's frames are.
//!
//! Two files say so: the same records cut into frames four times larger. The peak the allocator
//! this binary installs records must not follow that, which an implementation decoding each frame
//! into a buffer could not manage. The peak is measured rather than asserted against a constant of
//! its own, because what the decoder allocates underneath is zstd's business and moves with its
//! version; the difference between the two files is this crate's.

use std::alloc::{GlobalAlloc, Layout, System};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use seekzstdsep::{
    Alignment, CompressOptions, SeparatorCheck, compress_to_seekable_zst_with_opts, copy_range,
    count_frames,
};
use zeekstd::SeekTable;

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
/// The baseline is what the fixture and the seek table take, which are built before the case starts
/// and are not what the case is about.
fn peak_of(f: impl FnOnce()) -> usize {
    let baseline = IN_USE.load(Ordering::Relaxed);
    PEAK.store(baseline, Ordering::Relaxed);
    f();
    PEAK.load(Ordering::Relaxed).saturating_sub(baseline)
}

const SEPARATOR: &[u8] = b"\n";
/// Every record is this long, separator included, so a frame's decompressed size is its record
/// count times this.
const RECORD_LEN: usize = 128;
/// Frames per fixture. Three is the fewest that separator validation accepts.
const FRAMES: usize = 3;

/// Records per frame in the two fixtures. The second holds four times the first, so its frames are
/// four times larger.
const SMALL_PER_FRAME: usize = 16_384;
const LARGE_PER_FRAME: usize = SMALL_PER_FRAME * 4;

/// How much the larger file's peak may exceed the smaller one's. Well under the 6 MiB that holding
/// a frame would add, and well above the nothing that reading through a window does.
const SLACK: usize = 1 << 20;

/// `count` records of [`RECORD_LEN`] bytes, each distinguishable by its sequence number.
fn body(count: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(count * RECORD_LEN);
    for seq in 0..count {
        let mut record = format!("{{\"seq\":{seq}").into_bytes();
        record.resize(RECORD_LEN - 1, b'x');
        record.push(b'\n');
        out.extend_from_slice(&record);
    }
    out
}

/// A file of [`FRAMES`] frames holding `per_frame` records each.
///
/// The frame size target is the frame the record count already asks for, so that the buffer the
/// compressor bounds by it is large enough to reach a cut.
fn fixture(dir: &Path, label: &str, per_frame: usize) -> PathBuf {
    let out = dir.join(format!("{label}.seek.zst"));
    compress_to_seekable_zst_with_opts(
        std::io::Cursor::new(body(per_frame * FRAMES)),
        &mut std::io::sink(),
        per_frame * RECORD_LEN,
        true,
        SEPARATOR,
        None,
        Some(CompressOptions {
            out_dir: Some(dir.to_path_buf()),
            out_path: Some(out.clone()),
            ..Default::default()
        }),
    )
    .expect("failed to compress the fixture");
    out
}

/// The peaks of counting every frame of a file and of copying one frame out of it, for a file whose
/// frames hold `per_frame` records.
fn peaks(dir: &Path, label: &str, per_frame: usize) -> (usize, usize) {
    let path = fixture(dir, label, per_frame);
    let file = File::open(&path).expect("no fixture");
    let mut src = &file;
    let table = SeekTable::from_seekable(&mut src).expect("no seek table");
    assert!(
        table.num_frames() as usize >= FRAMES,
        "{label} was cut into {} frames rather than {FRAMES}",
        table.num_frames()
    );

    let counted = peak_of(|| {
        count_frames(&file, &table, SEPARATOR, 0..table.num_frames()).expect("failed to count");
    });
    // Validation and the range arithmetic are the counting this one is made of; nothing of the
    // frames it copies passes through memory, since the bytes go out compressed as they sit.
    let copied = peak_of(|| {
        copy_range(
            &file,
            &mut std::io::sink(),
            0,
            Some(per_frame as u64),
            SEPARATOR,
            Alignment::Required,
            SeparatorCheck::TwoFrames,
        )
        .expect("failed to copy");
    });
    (counted, copied)
}

#[test]
fn counting_does_not_hold_the_frame() {
    let dir = tempfile::tempdir().expect("no temp dir");
    let (small_count, small_copy) = peaks(dir.path(), "small", SMALL_PER_FRAME);
    let (large_count, large_copy) = peaks(dir.path(), "large", LARGE_PER_FRAME);

    let frame_growth = (LARGE_PER_FRAME - SMALL_PER_FRAME) * RECORD_LEN;
    for (op, small, large) in [
        ("count_frames", small_count, large_count),
        ("copy_range", small_copy, large_copy),
    ] {
        assert!(
            large <= small + SLACK,
            "{op} held {large} bytes over frames of {} and {small} over frames of {}: a frame \
             {frame_growth} bytes larger cost {} more, which is the frame being held rather than \
             walked",
            LARGE_PER_FRAME * RECORD_LEN,
            SMALL_PER_FRAME * RECORD_LEN,
            large - small,
        );
    }
}
