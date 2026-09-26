//! Reading records back out of a compressed file.
//!
//! Opening a reader costs the open, the seek table and frame 0's separator count. A reader
//! opened per range pays for all three every time. [`RecordReader`] is those three held open, so a
//! caller that reads one record at a time — the nushell plugin's cell paths, say — pays once.
//!
//! The reader inherits the same-count-per-frame invariant that it locates records by: a file
//! compressed without it is read at the wrong offsets, and says so only under
//! [`RecordReaderVerify`]. See `docs/format.md`.
use std::{
    borrow::Borrow,
    cell::Ref,
    fs::File,
    io::{Read, Seek, SeekFrom, Take, Write},
    path::PathBuf,
};

use anyhow::Context;
use memchr::memmem::Finder;
use zeekstd::Decoder;

use crate::find::{self, BoxFinder};
use crate::record;
use crate::seekzstdsep_lib::{count_records_in_frame, seek_table_decomp_frames};

/// The decoder, and the window records are read through.
///
/// The window owns the decoder rather than borrowing it: one that borrowed it could not outlive
/// the call that built it, and [`RecordReader::record`] leaves its window where the record ended
/// for the next lookup to walk on from. Everything that reads the file another way takes the
/// decoder back through [`Self::load_decoder`].
struct Lookup<S> {
    window: record::Reader<Take<Decoder<'static, S>>>,
    /// The frame the window is pointed at, or `None` once the decoder has moved since.
    frame: Option<usize>,
}

impl<S: Read + Seek> Lookup<S> {
    fn new(decoder: Decoder<'static, S>, frames: &[(u64, u64)]) -> Self {
        Self {
            window: record::Reader::new(decoder.take(0)).with_frame_ends(frames),
            frame: None,
        }
    }

    /// The decoder, for a caller that seeks it itself. What the window holds goes with it: after
    /// an arbitrary seek it no longer reads on from where it says it does.
    fn load_decoder(&mut self) -> &mut Decoder<'static, S> {
        self.frame = None;
        self.window.source_mut().get_mut()
    }

    /// How many records the window has to walk past to reach record `in_frame` of the frame
    /// covering `[start, start + len)`.
    ///
    /// The window is pointed at that frame first unless the walk can go on from where the last
    /// lookup left it — same frame, and the record either ahead of the walk or still buffered
    /// behind it. A compressed frame cannot be entered partway, so anything else is a decode from
    /// its start.
    fn walk_to(
        &mut self,
        frame: usize,
        (start, len): (u64, u64),
        in_frame: u64,
    ) -> anyhow::Result<u64> {
        if self.frame == Some(frame) {
            if let Some(skip) = self.window.walk_from(in_frame) {
                return Ok(skip);
            }
        }
        self.window.seek_to(start, len)?;
        self.frame = Some(frame);
        Ok(in_frame)
    }
}

/// The arguments a read of a record range hands
/// [`records_between_by_separator_in_frame`](crate::seekzstdsep_lib::records_between_by_separator_in_frame).
///
/// A record has no offset of its own — it is found by decoding from a frame boundary and counting
/// separators — so the bytes to decode and the records to skip inside them travel together.
struct RecordsRequest {
    /// Offset in the decompressed stream to seek to: the start of the frame the range begins in.
    start: u64,
    /// Decompressed bytes readable from `start`, out to the end of the frame the range can reach.
    len: u64,
    /// Records to skip after seeking to `start`, before the first one asked for.
    skip: u64,
}

/// What a read of `cnt` records from `from` has to ask for, `label` naming the file in a refusal.
///
/// A `cnt` of [`usize::MAX`] asks for the rest of the file, which is what an iterator with no
/// count to stop at reads.
///
/// # Errors
///
/// `from` being past the last frame.
fn records_request(
    frames: &[(u64, u64)],
    per_frame: usize,
    label: &str,
    from: usize,
    cnt: usize,
) -> anyhow::Result<RecordsRequest> {
    let total_sep_cnt = per_frame * frames.len();
    let mut frame_idx = frames.len().saturating_mul(from) / total_sep_cnt;
    // The final frame may contain one unterminated record after all its separators.
    if frame_idx == frames.len() && from == total_sep_cnt {
        frame_idx -= 1;
    }
    if frame_idx >= frames.len() {
        return Err(anyhow::anyhow!("record {from} is past the end of {label}"));
    }
    let idx_in_frame = from - frame_idx * per_frame;
    let start = frames[frame_idx].0;

    let end = from.saturating_add(cnt).saturating_add(1);
    let end_frame_idx = (frames.len().saturating_mul(end) / total_sep_cnt).min(frames.len() - 1);
    let len = frames[end_frame_idx].0 + frames[end_frame_idx].1 - start;
    Ok(RecordsRequest {
        start,
        len,
        skip: idx_in_frame as u64,
    })
}

/// Points `window` at the bytes `req` asks for and answers what watches the walk of them, the read
/// having been placed by record `from`.
///
/// Takes the reader's parts one by one: the watcher borrows the judge for as long as the walk it
/// is handed to, while the window is pointed through a borrow of its own.
fn place_span<'j, V: Verifier, S: Read + Seek>(
    lookup: &mut Lookup<S>,
    judge: &'j V::Judge,
    frames: &[(u64, u64)],
    per_frame: usize,
    from: usize,
    req: &RecordsRequest,
) -> anyhow::Result<V::Watch<'j>> {
    let watch = V::watch(judge, frames, per_frame, from, req.start, req.len);
    point_window(&mut lookup.frame, &mut lookup.window, req)?;
    Ok(watch)
}

#[inline(always)]
fn point_window<S: Read + Seek>(
    frame: &mut Option<usize>,
    window: &mut record::Reader<Take<Decoder<'static, S>>>,
    req: &RecordsRequest,
) -> anyhow::Result<()> {
    *frame = None;
    window.seek_to(req.start, req.len)
}

/// A compressed file held open for reading records by index.
///
/// Holds the decoder, the frame list and frame 0's separator count, plus the window
/// [`Self::record`] last read through: consecutive indices in the same frame decode it once.
///
/// `S` is the source the records are decoded from: a [`File`] for a reader opened on a path, and
/// anything `Read + Seek` through [`Self::from_reader`].
///
/// # Examples
///
/// ```
/// use seekzstdsep::RecordReader;
///
/// # use seekzstdsep::convert_to_seekable_zst_reader;
/// # let path = std::env::temp_dir().join("seekzstdsep-doc-reader.seek.zst");
/// # let input: &[u8] = b"record 1\nrecord 2\nrecord 3\nrecord 4\n";
/// # let mut compressed = Vec::new();
/// # convert_to_seekable_zst_reader(input, &mut compressed, 64 * 1024, true, b"\n", None)?;
/// # std::fs::write(&path, compressed)?;
/// let mut reader = RecordReader::open(path, b"\n")?;
///
/// assert_eq!(reader.total_records()?, 4);
/// assert_eq!(reader.record(1)?.unwrap(), b"record 2\n");
/// assert_eq!(reader.records(1, 2)?, b"record 2\nrecord 3\n");
/// # Ok::<(), anyhow::Error>(())
/// ```
pub struct RecordReader<V: Verifier = NoVerify, S = File> {
    label: String,
    lookup: Lookup<S>,
    frames: Vec<(u64, u64)>,
    boundary: Boundary,
    /// Records in frame 0, taken as the record count of every frame. Never 0: every record range
    /// divides by it, and [`Self::from_reader`] refuses a boundary that leaves it 0.
    sep_cnt: usize,
    judge: V::Judge,
}

/// A borrowed, forward-only iterator over records in a reader's reusable window.
///
/// Each item is a borrow guard. Drop it before asking for the next record; copy only records
/// that must outlive the current item. Construction performs no read.
pub struct BorrowedRecordIter<'a, V: Verifier, S = File> {
    window: Option<&'a mut record::Reader<Take<Decoder<'static, S>>>>,
    shared: Option<&'a record::Reader<Take<Decoder<'static, S>>>>,
    frame: &'a mut Option<usize>,
    frames: &'a [(u64, u64)],
    boundary: &'a Boundary,
    judge: &'a V::Judge,
    per_frame: usize,
    label: &'a str,
    from: usize,
    units: Option<
        record::RecordUnits<
            'a,
            Take<Decoder<'static, S>>,
            Box<dyn Fn(&[u8]) -> Option<usize> + 'a>,
            V::Watch<'a>,
        >,
    >,
    spent: bool,
}

/// Terminal operations for a fallible iterator of borrowed record bytes.
///
/// Standard `Iterator` adapters such as `filter`, `map`, and `take` remain the adapters used
/// between the source and these terminal operations.
pub trait RecordChainExt<'a>: Iterator<Item = anyhow::Result<Ref<'a, [u8]>>> + Sized {
    /// Write each record directly from the window to `dst`.
    #[inline(always)]
    fn write_to(mut self, dst: &mut impl Write) -> anyhow::Result<()> {
        self.try_for_each(|record| {
            dst.write_all(&record?)?;
            Ok(())
        })
    }

    /// Concatenate the records into one owned byte vector.
    #[inline(always)]
    fn to_vec(mut self) -> anyhow::Result<Vec<u8>> {
        self.try_fold(Vec::new(), |mut bytes, record| {
            bytes.extend_from_slice(&record?);
            Ok(bytes)
        })
    }
}

impl<'a, I> RecordChainExt<'a> for I where I: Iterator<Item = anyhow::Result<Ref<'a, [u8]>>> {}

/// A [`RecordReader`] that judges the frames it walks. [`RecordReader::verifying`] is how one is
/// made.
pub type RecordReaderVerify = RecordReader<AsRead>;

/// Where the reader finds records, held so that the public type gains no parameter.
///
/// A separator keeps its own search rather than arriving boxed, and [`with_find`] is how a read
/// reaches either one.
enum Boundary {
    Separator {
        finder: Finder<'static>,
        separator: Vec<u8>,
    },
    Finder(BoxFinder),
}

impl Boundary {
    fn tail_is_record(&self) -> bool {
        matches!(self, Self::Separator { .. })
    }

    /// The separator it was built from, and an empty slice for a finder.
    fn separator(&self) -> &[u8] {
        match self {
            Self::Separator { separator, .. } => separator,
            Self::Finder(_) => &[],
        }
    }
}

/// How far a read goes to confirm what it is asked to verify, as a type rather than a value:
/// [`RecordReader`] is [`NoVerify`] and [`RecordReaderVerify`] is [`AsRead`].
///
/// The reads themselves are written once per verifier, in the `impl` for that one, so what a
/// verifier asks of a read leaves no trace in the reader that asks for nothing. What the two share
/// is the reader's fields, and the one the verifier adds is [`Self::Judge`], of no size at all for
/// [`NoVerify`].
///
/// [`NoVerify`] and [`AsRead`] are the two this crate reads through. What [`Self::Watch`] has to
/// be is the crate's own and has no name outside it, so a third implementation can only borrow one
/// of theirs through `<AsRead as Verifier>::Watch` — which is why [`Self::watch`] answers for
/// arguments no reader would hand it rather than trusting the caller.
pub trait Verifier {
    /// Whether a read judges anything at all, for a read the two verifiers can share: one body
    /// branching on this compiles to the arm its verifier takes, and the other arm is not there.
    const VERIFIES: bool;

    /// What the read has to keep to judge the frames it walks.
    type Judge: Judge;

    /// What the walk of a read is told to: nothing at all for [`NoVerify`], the frame ends for
    /// [`AsRead`].
    type Watch<'a>: record::Watcher;

    /// What to watch the walk of `[start, start + len)` with, the read having been placed by
    /// record `from` of a file holding `per_frame` records to a frame.
    ///
    /// Where the frames end is worked out here rather than by the read, so that the read that
    /// watches nothing neither counts them nor allocates the list. A `per_frame` of 0, or a `from`
    /// past the last frame, leaves no frame end to reach and is watched against none.
    fn watch<'j>(
        judge: &'j Self::Judge,
        frames: &[(u64, u64)],
        per_frame: usize,
        from: usize,
        start: u64,
        len: u64,
    ) -> Self::Watch<'j>;

    /// [`Self::watch`] for a read that keeps its watcher rather than watching one call with it:
    /// what it hands over is the watcher's own copy of the judge, so the reader keeps judging
    /// after it.
    fn watch_owned(
        judge: &Self::Judge,
        frames: &[(u64, u64)],
        per_frame: usize,
        from: usize,
        start: u64,
        len: u64,
    ) -> Self::Watch<'static>;

    /// What to watch a walk with that has no frame end to report — one that stops inside a frame,
    /// and is judged where it stops rather than as it goes.
    ///
    /// Not the unwatched walk for [`AsRead`]: that one is the walk of a read that verifies
    /// nothing, and a walk shared with it is a walk that stops being inlined into it.
    fn watch_nothing<'j>() -> Self::Watch<'j>;
}

/// Reads without judging the frames it walks. What [`RecordReader`] is unless it is turned into
/// the other one.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoVerify;

/// Refuses a frame the read walks to the end of whose record count is not frame 0's, or that holds
/// bytes after its last record. The frame the file ends with may hold fewer records and may hold
/// the bytes; holding more is refused there too.
#[derive(Debug, Clone, Copy, Default)]
pub struct AsRead;

impl Verifier for NoVerify {
    const VERIFIES: bool = false;
    type Judge = ();
    type Watch<'a> = record::Unwatched;

    #[inline]
    fn watch(
        _judge: &(),
        _frames: &[(u64, u64)],
        _per_frame: usize,
        _from: usize,
        _start: u64,
        _len: u64,
    ) -> record::Unwatched {
        record::Unwatched
    }

    #[inline]
    fn watch_owned(
        _judge: &(),
        _frames: &[(u64, u64)],
        _per_frame: usize,
        _from: usize,
        _start: u64,
        _len: u64,
    ) -> record::Unwatched {
        record::Unwatched
    }

    #[inline]
    fn watch_nothing<'j>() -> Self::Watch<'j> {
        record::Unwatched
    }
}

impl Verifier for AsRead {
    const VERIFIES: bool = true;
    type Judge = FrameJudge;
    type Watch<'a> = record::Watch<'a>;

    fn watch<'j>(
        judge: &'j FrameJudge,
        frames: &[(u64, u64)],
        per_frame: usize,
        from: usize,
        start: u64,
        len: u64,
    ) -> record::Watch<'j> {
        watch_span::<FrameJudge>(judge, frames, per_frame, from, start, len)
    }

    fn watch_owned(
        judge: &FrameJudge,
        frames: &[(u64, u64)],
        per_frame: usize,
        from: usize,
        start: u64,
        len: u64,
    ) -> record::Watch<'static> {
        watch_span::<FrameJudge>(judge.clone(), frames, per_frame, from, start, len)
    }

    fn watch_nothing<'j>() -> record::Watch<'j> {
        record::Watch::nothing()
    }
}

/// What a walk asks about each frame it reaches the end of.
///
/// The one that judges nothing is `()`, whose calls compile away and whose size is nothing, so a
/// walk that holds one is the walk that was there before any of this.
/// What is asked is passed as a closure rather than a value: the judge that judges nothing never
/// calls it, so a read that keeps one does not go and count what it would have been asked about.
pub trait Judge {
    /// Built from what a refusal has to name and hold every frame to.
    fn new(label: String, last: usize, per_frame: u64) -> Self;

    /// Refuses the frame at `i` for the records it turns out to hold.
    fn count(&self, i: usize, held: impl FnOnce() -> u64) -> anyhow::Result<()>;

    /// Refuses the frame at `i` for holding bytes after its last record.
    fn ends_whole(&self, i: usize, ends_whole: impl FnOnce() -> bool) -> anyhow::Result<()>;
}

impl Judge for () {
    #[inline]
    fn new(_label: String, _last: usize, _per_frame: u64) -> Self {}

    #[inline]
    fn count(&self, _i: usize, _held: impl FnOnce() -> u64) -> anyhow::Result<()> {
        Ok(())
    }

    #[inline]
    fn ends_whole(&self, _i: usize, _ends_whole: impl FnOnce() -> bool) -> anyhow::Result<()> {
        Ok(())
    }
}

/// What [`AsRead`] holds to judge a frame: the source to name in a refusal, the last frame's index
/// because only that one may hold fewer records, and the count every other frame has to hold.
///
/// Cloned by a read that hands a copy to the watcher it keeps, the reader holding on to its own.
#[derive(Clone)]
pub struct FrameJudge {
    label: String,
    last: usize,
    per_frame: u64,
}

impl Judge for FrameJudge {
    fn new(label: String, last: usize, per_frame: u64) -> Self {
        Self {
            label,
            last,
            per_frame,
        }
    }

    fn count(&self, i: usize, held: impl FnOnce() -> u64) -> anyhow::Result<()> {
        verify_frame_count(&self.label, i, self.last, held(), self.per_frame)
    }

    fn ends_whole(&self, i: usize, ends_whole: impl FnOnce() -> bool) -> anyhow::Result<()> {
        verify_frame_ends_whole(&self.label, i, self.last, ends_whole())
    }
}

/// Refuses frame `i` holding a count other than `per_frame`. Only the frame the file ends with may
/// hold fewer.
fn verify_frame_count(
    label: &str,
    i: usize,
    last: usize,
    held: u64,
    per_frame: u64,
) -> anyhow::Result<()> {
    if held != per_frame && !(i == last && held < per_frame) {
        anyhow::bail!(
            "frame {i} of {label} holds {held} records rather than {per_frame}: a record index is \
             resolved by dividing it by the count frame 0 holds, so a frame holding another count \
             is read at the wrong offsets"
        );
    }
    Ok(())
}

/// Where the frames covering `[start, start + len)` end, as byte offsets from `start`, `first`
/// being the frame `start` is the start of.
///
/// The ends are a prefix of `frames[first..]`, so the n'th of them is frame `first + n` and a
/// refusal names the frame it judged. A frame that cannot be counted from `start` ends the prefix
/// rather than being passed over: skipping one would hand the next frame's end under its index. A
/// reader hands no such frame — `first` is the frame `start` begins — but [`AsRead::watch`] is
/// reachable from outside the crate.
fn frame_ends_from(frames: &[(u64, u64)], first: usize, start: u64, len: u64) -> Vec<u64> {
    frames[first..]
        .iter()
        .map(|&(frame_start, frame_len)| frame_start.checked_add(frame_len)?.checked_sub(start))
        .take_while(|end| end.is_some_and(|end| end <= len))
        .flatten()
        .collect()
}

/// What watches the walk of `[start, start + len)`, the read having been placed by record `from`
/// of a file holding `per_frame` records to a frame.
///
/// The body of [`AsRead::watch`] and [`AsRead::watch_owned`], which differ only in whether the
/// judge is borrowed from the reader or is the watcher's own copy.
fn watch_span<'j, J: Judge>(
    judge: impl Borrow<J> + 'j,
    frames: &[(u64, u64)],
    per_frame: usize,
    from: usize,
    start: u64,
    len: u64,
) -> record::Watch<'j> {
    // A file with no records to a frame, or a read placed past the last frame, has no frame
    // end for the walk to reach. A reader never asks either — its count is never 0 and a read
    // past the end is refused before it walks — but this is reachable from outside the crate.
    if per_frame == 0 {
        return record::Watch::nothing();
    }
    let first = from / per_frame;
    if first >= frames.len() {
        return record::Watch::nothing();
    }
    watch_frames::<J>(judge, first, frame_ends_from(frames, first, start, len))
}

/// What judges the frames a read walks past, frame `first` being the one the walk starts in.
fn watch_frames<'j, J: Judge>(
    judge: impl Borrow<J> + 'j,
    first: usize,
    ends: Vec<u64>,
) -> record::Watch<'j> {
    let mut walked = 0;
    record::Watch::new(ends, move |i, before, ends_whole| {
        let judge = judge.borrow();
        let frame = first + i;
        judge.count(frame, || before - walked)?;
        walked = before;
        judge.ends_whole(frame, || ends_whole)
    })
}

/// Refuses frame `i` holding bytes after its last record. Only the frame the file ends with may.
fn verify_frame_ends_whole(
    label: &str,
    i: usize,
    last: usize,
    ends_whole: bool,
) -> anyhow::Result<()> {
    if !ends_whole && i != last {
        anyhow::bail!(
            "frame {i} of {label} holds bytes after its last record: a record spans its end, so the \
             frames do not divide the file into records and a count per frame does not place them"
        );
    }
    Ok(())
}

/// Runs the body with `find` bound to the reader's record boundary.
///
/// The body is compiled once per arm, which is the point: a walk that reaches the boundary through
/// one shared type carries the choice into its loop, where it costs a branch and a reload of the
/// needle on every record — `benches/read.rs` and `Records::next` under callgrind are where that
/// shows. Resolving it here leaves the separator's walk calling `memchr` with the length in a
/// register, as it did before a record had a finder at all.
macro_rules! with_find {
    ($boundary:expr, |$find:ident| $body:expr) => {
        match $boundary {
            Boundary::Separator { finder, .. } => {
                let $find = crate::find::by_separator(finder);
                $body
            }
            Boundary::Finder(boxed) => {
                let $find = &**boxed;
                $body
            }
        }
    };
}

/// Whether the metadata's first index beyond all full frames names a real final record.
/// This is only needed at that boundary; ordinary positioning remains lazy.
fn final_slot_is_tail<S: Read + Seek>(
    window: &mut record::Reader<Take<Decoder<'static, S>>>,
    frames: &[(u64, u64)],
    boundary: &Boundary,
    per_frame: usize,
) -> anyhow::Result<bool> {
    if !boundary.tail_is_record() {
        return Ok(false);
    }
    let (start, len) = *frames.last().expect("reader has at least one frame");
    window.seek_to(start, len)?;
    let whole = with_find!(boundary, |find| window
        .records(|data: &[u8]| find(data))
        .count_records())?;
    Ok(whole == per_frame && !window.remainder().is_empty())
}

/// Resolve and position a record iterator's forward span. Both borrowed and owned adapters use
/// this path, including the exceptional final unterminated record slot.
fn position_iter_span<S: Read + Seek>(
    frame: &mut Option<usize>,
    window: &mut record::Reader<Take<Decoder<'static, S>>>,
    frames: &[(u64, u64)],
    boundary: &Boundary,
    per_frame: usize,
    label: &str,
    from: usize,
) -> anyhow::Result<RecordsRequest> {
    if from == per_frame * frames.len() && !final_slot_is_tail(window, frames, boundary, per_frame)?
    {
        anyhow::bail!("record {from} is past the end of {label}");
    }
    let req = records_request(frames, per_frame, label, from, usize::MAX)?;
    point_window(frame, window, &req)?;
    Ok(req)
}

macro_rules! with_units {
    ($reader:expr, $from:expr, $cnt:expr, |$units:ident, $window:ident| $body:expr) => {{
        let reader = $reader;
        let req = reader.records_request($from, $cnt)?;
        let watch = place_span::<V, S>(
            &mut reader.lookup,
            &reader.judge,
            &reader.frames,
            reader.sep_cnt,
            $from,
            &req,
        )?;
        let $window = &reader.lookup.window;
        with_find!(&reader.boundary, |find| {
            let mut $units = $window
                .records(|data: &[u8]| find(data))
                .watching(watch)
                .skip_records(req.skip)
                .into_units();
            let final_end = reader.frames.last().expect("reader has a frame");
            if req.start + req.len == final_end.0 + final_end.1 {
                $units = if reader.boundary.tail_is_record() {
                    $units.including_final_record()
                } else {
                    $units.rejecting_final_fragment()
                };
            }
            $units.prepare()?;
            $body
        })
    }};
}

impl<'a, V: Verifier, S: Read + Seek> BorrowedRecordIter<'a, V, S> {
    fn arm(&mut self) -> anyhow::Result<()> {
        let req = position_iter_span(
            self.frame,
            self.window
                .as_deref_mut()
                .expect("unarmed iterator has a window"),
            self.frames,
            self.boundary,
            self.per_frame,
            self.label,
            self.from,
        )?;
        let watch = V::watch(
            self.judge,
            self.frames,
            self.per_frame,
            self.from,
            req.start,
            req.len,
        );
        let window = self.window.take().expect("unarmed iterator has a window");
        let shared: &'a record::Reader<_> = window;
        let boundary = self.boundary;
        let find: Box<dyn Fn(&[u8]) -> Option<usize> + 'a> = Box::new(move |data| match boundary {
            Boundary::Separator { finder, .. } => {
                finder.find(data).map(|at| at + finder.needle().len())
            }
            Boundary::Finder(boxed) => boxed(data),
        });
        let mut units = shared
            .records(find)
            .watching(watch)
            .skip_records(req.skip)
            .into_units();
        units = if self.boundary.tail_is_record() {
            units.including_final_record()
        } else {
            units.rejecting_final_fragment()
        };
        units.prepare()?;
        self.shared = Some(shared);
        self.units = Some(units);
        Ok(())
    }
}

impl<'a, V: Verifier, S: Read + Seek> Iterator for BorrowedRecordIter<'a, V, S> {
    type Item = anyhow::Result<Ref<'a, [u8]>>;

    #[inline(always)]
    fn next(&mut self) -> Option<Self::Item> {
        if self.spent {
            return None;
        }
        if self.units.is_none() {
            if let Err(error) = self.arm() {
                self.spent = true;
                return Some(Err(error));
            }
        }
        let units = self.units.as_mut().expect("armed iterator has units");
        match units.next() {
            Some(Ok(run)) => Some(Ok(self
                .shared
                .expect("armed iterator has a window")
                .bytes(&run))),
            Some(Err(error)) => {
                self.spent = true;
                Some(Err(error))
            }
            None => {
                self.spent = true;
                match units.finish(usize::MAX) {
                    Ok(()) => None,
                    Err(error) => Some(Err(error)),
                }
            }
        }
    }
}

impl RecordReader<NoVerify> {
    /// Opens `path` and reads its seek table and frame 0's record count.
    ///
    /// # Errors
    ///
    /// An empty `separator`, the file not opening, a seek table with no frames in it, frame 0 not
    /// decompressing, or `separator` ending no record in frame 0.
    ///
    /// # Examples
    ///
    /// ```
    /// use seekzstdsep::RecordReader;
    ///
    /// # use seekzstdsep::convert_to_seekable_zst_reader;
    /// # let path = std::env::temp_dir().join("seekzstdsep-doc-open.seek.zst");
    /// # let input: &[u8] = b"record 1\nrecord 2\nrecord 3\n";
    /// # let mut compressed = Vec::new();
    /// # convert_to_seekable_zst_reader(input, &mut compressed, 64 * 1024, true, b"\n", None)?;
    /// # std::fs::write(&path, compressed)?;
    /// let mut reader = RecordReader::open(path, b"\n")?;
    ///
    /// assert_eq!(reader.total_records()?, 3);
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    pub fn open(path: PathBuf, separator: &[u8]) -> anyhow::Result<Self> {
        let file =
            File::open(&path).with_context(|| format!("failed to open {}", path.display()))?;
        Self::from_file(path, file, separator)
    }

    /// [`Self::open`] with the record boundary as a finder.
    ///
    /// # Errors
    ///
    /// Whatever [`Self::open`] refuses, bar the empty separator.
    ///
    /// # Examples
    ///
    /// ```
    /// use seekzstdsep::RecordReader;
    /// use seekzstdsep::find::by_fixed;
    ///
    /// # use seekzstdsep::convert_records_to_seekable_zst_reader_with_opts as compress_records;
    /// # let path = std::env::temp_dir().join("seekzstdsep-doc-open-with.seek.zst");
    /// # let input: &[u8] = b"aaaabbbbccccdddd";
    /// # let mut compressed = Vec::new();
    /// # compress_records(input, &mut compressed, 64 * 1024, true, by_fixed(4), None, None)?;
    /// # std::fs::write(&path, compressed)?;
    /// let mut reader = RecordReader::open_with(path, Box::new(by_fixed(4)))?;
    ///
    /// assert_eq!(reader.total_records()?, 4);
    /// assert_eq!(reader.record(2)?.unwrap(), b"cccc");
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    pub fn open_with(path: PathBuf, find: BoxFinder) -> anyhow::Result<Self> {
        let file =
            File::open(&path).with_context(|| format!("failed to open {}", path.display()))?;
        Self::from_file_with(path, file, find)
    }

    /// [`Self::open`] on an already-open file. `path` is carried for error messages only.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::fs::File;
    ///
    /// use seekzstdsep::RecordReader;
    ///
    /// # use seekzstdsep::convert_to_seekable_zst_reader;
    /// # let path = std::env::temp_dir().join("seekzstdsep-doc-from-file.seek.zst");
    /// # let input: &[u8] = b"record 1\nrecord 2\nrecord 3\n";
    /// # let mut compressed = Vec::new();
    /// # convert_to_seekable_zst_reader(input, &mut compressed, 64 * 1024, true, b"\n", None)?;
    /// # std::fs::write(&path, compressed)?;
    /// let file = File::open(&path)?;
    /// let mut reader = RecordReader::from_file(path, file, b"\n")?;
    ///
    /// assert_eq!(reader.total_records()?, 3);
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    pub fn from_file(path: PathBuf, file: File, separator: &[u8]) -> anyhow::Result<Self> {
        Self::from_reader(
            file,
            &path.to_string_lossy(),
            find::Boundary::Separator(separator.to_vec()),
        )
    }

    /// [`Self::from_file`] with the record boundary as a finder.
    ///
    /// # Errors
    ///
    /// Whatever [`Self::from_file`] refuses, bar the empty separator.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::fs::File;
    ///
    /// use seekzstdsep::RecordReader;
    /// use seekzstdsep::find::by_fixed;
    ///
    /// # use seekzstdsep::convert_records_to_seekable_zst_reader_with_opts as compress_records;
    /// # let path = std::env::temp_dir().join("seekzstdsep-doc-from-file-with.seek.zst");
    /// # let input: &[u8] = b"aaaabbbbccccdddd";
    /// # let mut compressed = Vec::new();
    /// # compress_records(input, &mut compressed, 64 * 1024, true, by_fixed(4), None, None)?;
    /// # std::fs::write(&path, compressed)?;
    /// let file = File::open(&path)?;
    /// let mut reader = RecordReader::from_file_with(path, file, Box::new(by_fixed(4)))?;
    ///
    /// assert_eq!(reader.total_records()?, 4);
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    pub fn from_file_with(path: PathBuf, file: File, find: BoxFinder) -> anyhow::Result<Self> {
        Self::from_reader(file, &path.to_string_lossy(), find::Boundary::Finder(find))
    }
}

impl<S: Read + Seek> RecordReader<NoVerify, S> {
    /// [`Self::open`] on any `Read + Seek` source, with the record boundary as either a separator
    /// or a finder. `label` is what a refusal names the source by, as the path is for
    /// [`Self::open`].
    ///
    /// # Errors
    ///
    /// Whatever [`Self::open`] refuses, bar the file not opening.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::io::Cursor;
    ///
    /// use seekzstdsep::RecordReader;
    /// use seekzstdsep::find::Boundary;
    ///
    /// # use seekzstdsep::convert_to_seekable_zst_reader;
    /// # let input: &[u8] = b"record 1\nrecord 2\nrecord 3\n";
    /// # let mut compressed = Vec::new();
    /// # convert_to_seekable_zst_reader(input, &mut compressed, 64 * 1024, true, b"\n", None)?;
    /// let source = Cursor::new(compressed);
    /// let boundary = Boundary::Separator(b"\n".to_vec());
    /// let mut reader = RecordReader::from_reader(source, "in-memory", boundary)?;
    ///
    /// assert_eq!(reader.total_records()?, 3);
    /// assert_eq!(reader.record(1)?.unwrap(), b"record 2\n");
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    pub fn from_reader(source: S, label: &str, boundary: find::Boundary) -> anyhow::Result<Self> {
        let boundary = match boundary {
            find::Boundary::Separator(separator) => {
                record::check_separator(&separator)?;
                Boundary::Separator {
                    finder: Finder::new(&separator).into_owned(),
                    separator,
                }
            }
            find::Boundary::Finder(find) => Boundary::Finder(find),
        };
        let decoder = Decoder::new(source)
            .with_context(|| format!("failed to open {label} as a seekable zst"))?;
        let frames = seek_table_decomp_frames(&decoder)
            .ok_or_else(|| anyhow::anyhow!("no frames in {label}"))?;
        let mut reader = Self {
            label: label.to_owned(),
            lookup: Lookup::new(decoder, &frames),
            frames,
            boundary,
            sep_cnt: 0,
            judge: (),
        };
        let (start, len) = reader.frames[0];
        reader.sep_cnt = with_find!(&reader.boundary, |find| count_records_in_frame(
            reader.lookup.load_decoder(),
            start,
            len,
            find
        )?);
        if reader.sep_cnt == 0 {
            if reader.boundary.separator().is_empty() {
                anyhow::bail!("no record ends in frame 0 of {label}");
            }
            anyhow::bail!(
                "no record in frame 0 of {label} ends with {:?}: a file does not record the \
                 separator it was written with, so pass the one it was",
                String::from_utf8_lossy(reader.boundary.separator()),
            );
        }
        Ok(reader)
    }

    /// The same reader, judging the frames it walks from here on.
    ///
    /// A read placed by a frame it never walks is not covered: nothing counts that frame.
    ///
    /// # Examples
    ///
    /// ```
    /// use seekzstdsep::RecordReader;
    ///
    /// # use seekzstdsep::convert_to_seekable_zst_reader;
    /// # let path = std::env::temp_dir().join("seekzstdsep-doc-verifying.seek.zst");
    /// # let input: &[u8] = b"aaaa\nb\nb\nb\nb\nb\nb\n";
    /// # let mut compressed = Vec::new();
    /// # convert_to_seekable_zst_reader(input, &mut compressed, 6, false, b"\n", None)?;
    /// # std::fs::write(&path, compressed)?;
    /// // Seven records in frames of 2, 3 and 2: the counts were left to the byte target.
    /// let mut reader = RecordReader::open(path, b"\n")?.verifying();
    ///
    /// // A read inside frame 0 is answered; one that walks into frame 1 is not.
    /// assert_eq!(reader.records(0, 2)?, b"aaaa\nb\n");
    /// let err = reader.records(0, 7).unwrap_err();
    /// assert!(err.to_string().contains("frame 1"));
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    pub fn verifying(self) -> RecordReader<AsRead, S> {
        let mut lookup = self.lookup;
        // The verifying watcher reports frame count and fragments with its historical errors.
        // The lightweight boundary guard is for unverified reads only.
        lookup.window.without_frame_ends();
        let judge = FrameJudge::new(
            self.label.clone(),
            self.frames.len() - 1,
            self.sep_cnt as u64,
        );
        RecordReader {
            label: self.label,
            lookup,
            frames: self.frames,
            boundary: self.boundary,
            sep_cnt: self.sep_cnt,
            judge,
        }
    }
}

impl<V: Verifier, S: Read + Seek> RecordReader<V, S> {
    /// What a refusal names the source by: the path, for a reader opened on one.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The separator records are counted by, and an empty slice when the reader was opened with a
    /// finder instead.
    ///
    /// # Examples
    ///
    /// ```
    /// use seekzstdsep::RecordReader;
    /// use seekzstdsep::find::by_fixed;
    ///
    /// # use seekzstdsep::convert_to_seekable_zst_reader;
    /// # let path = std::env::temp_dir().join("seekzstdsep-doc-separator.seek.zst");
    /// # let input: &[u8] = b"record 1\nrecord 2\nrecord 3\n";
    /// # let mut compressed = Vec::new();
    /// # convert_to_seekable_zst_reader(input, &mut compressed, 64 * 1024, true, b"\n", None)?;
    /// # std::fs::write(&path, compressed)?;
    /// let reader = RecordReader::open(path.clone(), b"\n")?;
    /// assert_eq!(reader.separator(), b"\n");
    ///
    /// let reader = RecordReader::open_with(path, Box::new(by_fixed(9)))?;
    /// assert!(reader.separator().is_empty());
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    pub fn separator(&self) -> &[u8] {
        self.boundary.separator()
    }

    /// How many frames the file holds.
    ///
    /// # Examples
    ///
    /// ```
    /// use seekzstdsep::RecordReader;
    ///
    /// # use seekzstdsep::convert_to_seekable_zst_reader;
    /// # let path = std::env::temp_dir().join("seekzstdsep-doc-frame-count.seek.zst");
    /// # let input: &[u8] = b"record 1\nrecord 2\nrecord 3\n";
    /// # let mut compressed = Vec::new();
    /// # convert_to_seekable_zst_reader(input, &mut compressed, 64 * 1024, true, b"\n", None)?;
    /// # std::fs::write(&path, compressed)?;
    /// let reader = RecordReader::open(path, b"\n")?;
    ///
    /// assert_eq!(reader.frame_count(), 1);
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// Records in frame 0, which the invariant makes the record count of every frame but the last.
    ///
    /// # Examples
    ///
    /// ```
    /// use seekzstdsep::RecordReader;
    ///
    /// # use seekzstdsep::convert_to_seekable_zst_reader;
    /// # let path = std::env::temp_dir().join("seekzstdsep-doc-records-per-frame.seek.zst");
    /// # let input: &[u8] = b"record 1\nrecord 2\nrecord 3\n";
    /// # let mut compressed = Vec::new();
    /// # convert_to_seekable_zst_reader(input, &mut compressed, 64 * 1024, true, b"\n", None)?;
    /// # std::fs::write(&path, compressed)?;
    /// let reader = RecordReader::open(path, b"\n")?;
    ///
    /// assert_eq!(reader.records_per_frame(), 3);
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    pub fn records_per_frame(&self) -> usize {
        self.sep_cnt
    }

    /// How many records the file holds, including a nonempty unterminated final record.
    /// Decompresses only the last frame to count its records.
    ///
    /// # Examples
    ///
    /// ```
    /// use seekzstdsep::RecordReader;
    ///
    /// # use seekzstdsep::convert_to_seekable_zst_reader;
    /// # let path = std::env::temp_dir().join("seekzstdsep-doc-total-records.seek.zst");
    /// # let input: &[u8] = b"record 1\nrecord 2\nrecord 3\nrecord 4\n";
    /// # let mut compressed = Vec::new();
    /// # convert_to_seekable_zst_reader(input, &mut compressed, 64 * 1024, true, b"\n", None)?;
    /// # std::fs::write(&path, compressed)?;
    /// let mut reader = RecordReader::open(path, b"\n")?;
    ///
    /// assert_eq!(reader.total_records()?, 4);
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    // FIXME: counts records that [`Self::record`] cannot reach when a frame holds more than frame 0
    // does, which this crate's compressor never writes. See `docs/bugs.md`.
    pub fn total_records(&mut self) -> anyhow::Result<usize> {
        let last = self.frames.len() - 1;
        let (start, len) = self.frames[last];
        self.lookup.frame = None;
        self.lookup.window.seek_to(start, len)?;
        let include_tail = self.boundary.tail_is_record();
        let in_last = with_find!(&self.boundary, |find| {
            let mut units = self
                .lookup
                .window
                .records(|data: &[u8]| find(data))
                .into_units();
            if include_tail {
                units = units.including_final_record();
            } else {
                units = units.rejecting_final_fragment();
            }
            units.try_fold(0usize, |count, record| record.map(|_| count + 1))
        })?;
        Ok(self.sep_cnt * last + in_last)
    }

    /// The whole file decompressed, from the start, as a byte stream.
    ///
    /// The decoder this was reading frames through, rewound — no second open, no second seek
    /// table.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::io::Read;
    ///
    /// use seekzstdsep::RecordReader;
    ///
    /// # use seekzstdsep::convert_to_seekable_zst_reader;
    /// # let path = std::env::temp_dir().join("seekzstdsep-doc-into-bytes.seek.zst");
    /// # let input: &[u8] = b"record 1\nrecord 2\nrecord 3\n";
    /// # let mut compressed = Vec::new();
    /// # convert_to_seekable_zst_reader(input, &mut compressed, 64 * 1024, true, b"\n", None)?;
    /// # std::fs::write(&path, compressed)?;
    /// let reader = RecordReader::open(path, b"\n")?;
    ///
    /// let mut all = Vec::new();
    /// reader.into_bytes()?.read_to_end(&mut all)?;
    ///
    /// assert_eq!(all, b"record 1\nrecord 2\nrecord 3\n");
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    pub fn into_bytes(self) -> anyhow::Result<impl Read + Send + 'static>
    where
        S: Send + 'static,
    {
        let mut decoder = self.lookup.window.into_source().into_inner();
        decoder.seek(SeekFrom::Start(0))?;
        Ok(decoder)
    }

    /// What a read of `cnt` records from `from` has to ask
    /// [`records_between_by_separator_in_frame`] for.
    ///
    /// # Errors
    ///
    /// `from` being past the last frame.
    fn records_request(&self, from: usize, cnt: usize) -> anyhow::Result<RecordsRequest> {
        records_request(&self.frames, self.sep_cnt, &self.label, from, cnt)
    }

    /// Borrow records starting at `from` through the standard iterator adapters.
    ///
    /// No read is performed until the iterator is advanced. A record borrow guard must be
    /// dropped before the next item is requested; copy a record to keep it longer.
    ///
    /// A prebuilt finder can be reused across records without copying rejected records.
    /// Keep errors in the chain so `collect` can report them:
    ///
    /// ```
    /// # fn main() -> anyhow::Result<()> {
    /// # let path = std::env::temp_dir().join("seekzstdsep-doc-filter.seek.zst");
    /// # let mut compressed = Vec::new();
    /// # seekzstdsep::convert_to_seekable_zst_reader(&b"error one\nok\nerror two\n"[..], &mut compressed, 64 * 1024, true, b"\n", None)?;
    /// # std::fs::write(&path, compressed)?;
    /// # let mut reader = seekzstdsep::RecordReader::open(path, b"\n")?;
    /// let finder = memchr::memmem::Finder::new(b"error");
    /// let kept: Vec<Vec<u8>> = reader
    ///     .records_from(0)
    ///     .filter(|item| item.as_ref().map_or(true, |record| finder.find(&record[..]).is_some()))
    ///     .map(|item| item.map(|record| record.to_vec()))
    ///     .take(10)
    ///     .collect::<anyhow::Result<_>>()?;
    /// # assert_eq!(kept, [b"error one\n".to_vec(), b"error two\n".to_vec()]);
    /// # Ok(())
    /// # }
    /// ```
    pub fn records_from(&mut self, from: usize) -> BorrowedRecordIter<'_, V, S> {
        BorrowedRecordIter {
            window: Some(&mut self.lookup.window),
            shared: None,
            frame: &mut self.lookup.frame,
            frames: &self.frames,
            boundary: &self.boundary,
            judge: &self.judge,
            per_frame: self.sep_cnt,
            label: &self.label,
            from,
            units: None,
            spent: false,
        }
    }

    /// `cnt` records from `from`, or fewer when the file holds fewer, gathered into a `Vec`.
    /// [`Self::records_to`] writes the same records without building it.
    ///
    /// The frame is found by dividing `from` by the separator count of frame 0, so this rests on
    /// every frame holding the same count. On a file compressed without that invariant it returns
    /// the wrong records, and reports it only under [`RecordReaderVerify`].
    ///
    /// # Errors
    ///
    /// `from` being past the last frame, or a frame not decompressing. Under
    /// [`RecordReaderVerify`], also a frame the walk leaves behind that it refuses.
    ///
    /// # Examples
    ///
    /// ```
    /// use seekzstdsep::RecordReader;
    ///
    /// # use seekzstdsep::convert_to_seekable_zst_reader;
    /// # let path = std::env::temp_dir().join("seekzstdsep-doc-records.seek.zst");
    /// # let input: &[u8] = b"record 1\nrecord 2\nrecord 3\nrecord 4\n";
    /// # let mut compressed = Vec::new();
    /// # convert_to_seekable_zst_reader(input, &mut compressed, 64 * 1024, true, b"\n", None)?;
    /// # std::fs::write(&path, compressed)?;
    /// let mut reader = RecordReader::open(path, b"\n")?;
    ///
    /// assert_eq!(reader.records(1, 2)?, b"record 2\nrecord 3\n");
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    pub fn records(&mut self, from: usize, cnt: usize) -> anyhow::Result<Vec<u8>> {
        let mut out = Vec::new();
        self.records_to(from, cnt, &mut out)?;
        Ok(out)
    }

    /// [`Self::records`] into `dst`: the same `cnt` records from `from`, written as they are
    /// decoded instead of gathered into a `Vec`, so no more than the window is held at once.
    /// Decoding stops within one window of the separator that ends the last record asked for.
    ///
    /// # Errors
    ///
    /// `from` being past the last frame, a frame not decompressing, or `dst` refusing bytes. Under
    /// [`RecordReaderVerify`], also a frame the walk leaves behind that it refuses; the records
    /// walked before such a refusal have already gone to `dst`.
    ///
    /// # Examples
    ///
    /// ```
    /// use seekzstdsep::RecordReader;
    ///
    /// # use seekzstdsep::convert_to_seekable_zst_reader;
    /// # let path = std::env::temp_dir().join("seekzstdsep-doc-records-to.seek.zst");
    /// # let input: &[u8] = b"record 1\nrecord 2\nrecord 3\nrecord 4\n";
    /// # let mut compressed = Vec::new();
    /// # convert_to_seekable_zst_reader(input, &mut compressed, 64 * 1024, true, b"\n", None)?;
    /// # std::fs::write(&path, compressed)?;
    /// let mut reader = RecordReader::open(path, b"\n")?;
    ///
    /// let mut out = Vec::new();
    /// reader.records_to(1, 2, &mut out)?;
    ///
    /// assert_eq!(out, b"record 2\nrecord 3\n");
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    #[inline(always)]
    pub fn records_to(
        &mut self,
        from: usize,
        cnt: usize,
        dst: &mut impl Write,
    ) -> anyhow::Result<()> {
        if from == self.sep_cnt * self.frames.len() {
            self.lookup.frame = None;
            if !final_slot_is_tail(
                &mut self.lookup.window,
                &self.frames,
                &self.boundary,
                self.sep_cnt,
            )? {
                anyhow::bail!("record {from} is past the end of {}", self.label);
            }
        }
        with_units!(self, from, cnt, |units, _window| {
            units.write_to(cnt, dst)?;
            units.finish(cnt)?;
            Ok::<(), anyhow::Error>(())
        })
    }

    /// Folds records borrowed from the read window. The caller copies only records it keeps.
    ///
    /// # Errors
    ///
    /// The read starting past the last record, a frame not decompressing, or `f` refusing a
    /// record. Under [`RecordReaderVerify`], also a frame the walk leaves behind that it refuses.
    pub fn fold_records<B>(
        &mut self,
        from: usize,
        cnt: usize,
        init: B,
        mut f: impl FnMut(B, &[u8]) -> anyhow::Result<B>,
    ) -> anyhow::Result<B> {
        with_units!(self, from, cnt, |units, window| {
            let value = units
                .by_ref()
                .take(cnt)
                .try_fold(init, |acc, run| f(acc, &window.bytes(&run?)))?;
            units.finish(cnt)?;
            Ok(value)
        })
    }
}

impl<V: Verifier, S: Read + Seek> RecordReader<V, S> {
    /// Every record in the file, in order, decoding a window at a time.
    ///
    /// Scans rather than divides, so unlike [`Self::record`] it does not rest on the
    /// same-count-per-frame invariant. Nonempty bytes after the last separator are the final
    /// record, returned without a trailing separator.
    /// [`DoubleEndedIterator::next_back`] reads from the end; the two directions consume the
    /// same remaining records without overlap. See [`RecordIter`] for reverse reading costs.
    ///
    /// Under [`RecordReaderVerify`] it stops at the first frame [`AsRead`] refuses — one holding a
    /// count other than frame 0's, or bytes after its last record. A reverse read checks the whole
    /// frame before returning any of its records.
    ///
    /// # Examples
    ///
    /// ```
    /// use seekzstdsep::RecordReader;
    ///
    /// # use seekzstdsep::convert_to_seekable_zst_reader;
    /// # let path = std::env::temp_dir().join("seekzstdsep-doc-into-records.seek.zst");
    /// # let input: &[u8] = b"record 1\nrecord 2\nrecord 3\n";
    /// # let mut compressed = Vec::new();
    /// # convert_to_seekable_zst_reader(input, &mut compressed, 64 * 1024, true, b"\n", None)?;
    /// # std::fs::write(&path, compressed)?;
    /// let reader = RecordReader::open(path, b"\n")?;
    ///
    /// let mut records = reader.into_records();
    ///
    /// assert_eq!(records.next_back().transpose()?.unwrap(), b"record 3\n");
    /// assert_eq!(records.next().transpose()?.unwrap(), b"record 1\n");
    /// assert_eq!(records.next_back().transpose()?.unwrap(), b"record 2\n");
    /// assert!(records.next().is_none());
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    pub fn into_records(self) -> RecordIter<V, S> {
        self.into_records_from(0)
    }

    /// [`Self::into_records`] beginning at record `from` rather than at the first one.
    ///
    /// The records before it are not decoded: `from` places the read the way
    /// [`Self::records`] does, so the frames before the one holding it are never read. How many to
    /// take is [`Iterator::take`]'s question, which is why there is no count here.
    ///
    /// Nothing is read until the first record is asked for, so a `from` past the last record comes
    /// back as that call's error rather than here.
    ///
    /// # Examples
    ///
    /// ```
    /// use seekzstdsep::RecordReader;
    ///
    /// # use seekzstdsep::convert_to_seekable_zst_reader;
    /// # let path = std::env::temp_dir().join("seekzstdsep-doc-into-records-from.seek.zst");
    /// # let input: &[u8] = b"record 1\nrecord 2\nrecord 3\nrecord 4\n";
    /// # let mut compressed = Vec::new();
    /// # convert_to_seekable_zst_reader(input, &mut compressed, 64 * 1024, true, b"\n", None)?;
    /// # std::fs::write(&path, compressed)?;
    /// let reader = RecordReader::open(path, b"\n")?;
    ///
    /// let records: Vec<_> = reader
    ///     .into_records_from(2)
    ///     .take(1)
    ///     .collect::<anyhow::Result<_>>()?;
    ///
    /// assert_eq!(records, vec![b"record 3\n".to_vec()]);
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    pub fn into_records_from(self, from: usize) -> RecordIter<V, S> {
        RecordIter {
            back_frame: self.frames.len() - 1,
            frames: self.frames,
            per_frame: self.sep_cnt,
            label: self.label,
            front: from,
            back_left: None,
            armed: false,
            spent: false,
            boundary: self.boundary,
            lookup: self.lookup,
            judge: self.judge,
            cursor: None,
        }
    }
}

impl<S: Read + Seek> RecordReader<NoVerify, S> {
    /// Record `index`, or `None` when the file holds no such record.
    ///
    /// The returned bytes carry the separator, except for a nonempty unterminated final record.
    ///
    /// What is held is one window, not the frame the record is in: the walk is left where the
    /// record ended, and the next index in the same frame goes on from there. See
    /// [`Lookup::walk_to`] for what an index elsewhere costs.
    ///
    /// # Examples
    ///
    /// ```
    /// use seekzstdsep::RecordReader;
    ///
    /// # use seekzstdsep::convert_to_seekable_zst_reader;
    /// # let path = std::env::temp_dir().join("seekzstdsep-doc-record.seek.zst");
    /// # let input: &[u8] = b"record 1\nrecord 2\nrecord 3\n";
    /// # let mut compressed = Vec::new();
    /// # convert_to_seekable_zst_reader(input, &mut compressed, 64 * 1024, true, b"\n", None)?;
    /// # std::fs::write(&path, compressed)?;
    /// let mut reader = RecordReader::open(path, b"\n")?;
    ///
    /// assert_eq!(reader.record(0)?.unwrap(), b"record 1\n");
    /// assert_eq!(reader.record(2)?.unwrap(), b"record 3\n");
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    pub fn record(&mut self, index: usize) -> anyhow::Result<Option<Vec<u8>>> {
        let mut frame = index / self.sep_cnt;
        if frame == self.frames.len() && index == self.sep_cnt * self.frames.len() {
            self.lookup.frame = None;
            if !final_slot_is_tail(
                &mut self.lookup.window,
                &self.frames,
                &self.boundary,
                self.sep_cnt,
            )? {
                return Ok(None);
            }
            frame -= 1;
        }
        if frame >= self.frames.len() {
            return Ok(None);
        }
        let in_frame = (index - frame * self.sep_cnt) as u64;
        let region = self.frames[frame];
        let skip = self.lookup.walk_to(frame, region, in_frame)?;
        // A skip the frame runs out inside leaves nothing to hand back, which is what the walk
        // ending before a whole record says as well.
        with_find!(&self.boundary, |find| {
            let mut units = self
                .lookup
                .window
                .records(find)
                .skip_up_to(skip)
                .into_units();
            if frame == self.frames.len() - 1 {
                units = if self.boundary.tail_is_record() {
                    units.including_final_record()
                } else {
                    units.rejecting_final_fragment()
                };
            }
            units
                .next()
                .transpose()
                .map(|run| run.map(|run| self.lookup.window.bytes(&run).to_vec()))
        })
    }
}

impl<S: Read + Seek> RecordReader<AsRead, S> {
    /// [`RecordReader::record`], refusing a frame the walk ran out in that holds a record count of
    /// its own.
    ///
    /// # Errors
    ///
    /// A frame the walk ran out in that holds a count other than frame 0's — where
    /// [`RecordReader::record`] answers `None`.
    pub fn record(&mut self, index: usize) -> anyhow::Result<Option<Vec<u8>>> {
        let mut frame = index / self.sep_cnt;
        if frame == self.frames.len() && index == self.sep_cnt * self.frames.len() {
            self.lookup.frame = None;
            if !final_slot_is_tail(
                &mut self.lookup.window,
                &self.frames,
                &self.boundary,
                self.sep_cnt,
            )? {
                return Ok(None);
            }
            frame -= 1;
        }
        if frame >= self.frames.len() {
            return Ok(None);
        }
        let in_frame = (index - frame * self.sep_cnt) as u64;
        let region = self.frames[frame];
        let skip = self.lookup.walk_to(frame, region, in_frame)?;
        with_find!(&self.boundary, |find| {
            let mut units = self
                .lookup
                .window
                .records(find)
                .watching(record::Watch::nothing())
                .skip_up_to(skip)
                .into_units();
            if frame == self.frames.len() - 1 {
                units = if self.boundary.tail_is_record() {
                    units.including_final_record()
                } else {
                    units.rejecting_final_fragment()
                };
            }
            let record = units
                .next()
                .transpose()?
                .map(|run| self.lookup.window.bytes(&run).to_vec());
            if record.is_none() {
                self.verify_walked_frame(frame)?;
            }
            Ok(record)
        })
    }

    /// Refuses the frame the walk stopped in for the records it turned out to hold.
    fn verify_walked_frame(&self, frame: usize) -> anyhow::Result<()> {
        self.judge.count(frame, || self.lookup.window.walked())
    }
}

/// Every record of a [`RecordReader`], from either end. Made by [`RecordReader::into_records`].
///
/// Reads forward as a range read does: one seek, and the rest of the file decoded through the
/// record stream's fixed window, so no frame has to fit in memory — only the record being handed
/// out does. The frames it crosses are judged as it crosses them, by what watches a range read.
///
/// A reverse read first scans the frame to count its records. Records still in the window reuse
/// those bytes; reaching an earlier record outside it decodes from that frame's start again, and
/// leaves the forward read to seek again when it resumes. Either direction stops permanently after
/// an error, or when the remaining records run out.
///
/// # Examples
///
/// ```
/// use seekzstdsep::RecordReader;
///
/// # use seekzstdsep::convert_to_seekable_zst_reader;
/// # let path = std::env::temp_dir().join("seekzstdsep-doc-iter.seek.zst");
/// # let input: &[u8] = b"record 1\nrecord 2\nrecord 3\n";
/// # let mut compressed = Vec::new();
/// # convert_to_seekable_zst_reader(input, &mut compressed, 64 * 1024, true, b"\n", None)?;
/// # std::fs::write(&path, compressed)?;
/// let reader = RecordReader::open(path, b"\n")?;
///
/// let mut count = 0;
/// for record in reader.into_records().rev() {
///     assert!(record?.ends_with(b"\n"));
///     count += 1;
/// }
/// assert_eq!(count, 3);
/// # Ok::<(), anyhow::Error>(())
/// ```
pub struct RecordIter<V: Verifier = NoVerify, S = File> {
    frames: Vec<(u64, u64)>,
    /// Records to a frame, which every frame but the last one holds.
    per_frame: usize,
    /// The file, to name in a refusal.
    label: String,
    /// The record handed out next going forward.
    front: usize,
    /// The frame a reverse read is in.
    back_frame: usize,
    /// Records of `back_frame` a reverse read has not handed out, once that frame is counted.
    back_left: Option<u64>,
    /// Whether the window is pointed at the span `front` begins.
    armed: bool,
    /// Set once a read failed or a frame was refused: nothing more is handed out.
    spent: bool,
    boundary: Boundary,
    lookup: Lookup<S>,
    judge: V::Judge,
    /// The same record-unit cursor as the borrowed Iterator, held next to its owned reader.
    cursor: Option<record::RecordUnitCursor<V::Watch<'static>>>,
}

impl<V: Verifier, S: Read + Seek> RecordIter<V, S> {
    /// Points the window at the records from `front` on and builds what watches that walk. A
    /// reverse read moves the window, so this runs again when the forward read resumes.
    ///
    /// # Errors
    ///
    /// `front` being past the last record, or a frame not decompressing.
    fn arm(&mut self) -> anyhow::Result<()> {
        let req = position_iter_span(
            &mut self.lookup.frame,
            &mut self.lookup.window,
            &self.frames,
            &self.boundary,
            self.per_frame,
            &self.label,
            self.front,
        )?;
        let watch = V::watch_owned(
            &self.judge,
            &self.frames,
            self.per_frame,
            self.front,
            req.start,
            req.len,
        );
        let cursor = record::RecordUnitCursor::new(watch, req.skip);
        self.cursor = Some(if self.boundary.tail_is_record() {
            cursor.including_final_record()
        } else {
            cursor.rejecting_final_fragment()
        });
        self.armed = true;
        Ok(())
    }

    /// How many records frame `frame` holds, judged as a range read judges a frame it walks past.
    ///
    /// # Errors
    ///
    /// The frame not decompressing, or being refused for what it holds.
    fn count_frame(&mut self, frame: usize) -> anyhow::Result<u64> {
        let region = self.frames[frame];
        self.lookup.walk_to(frame, region, 0)?;
        let mut watch = V::watch(
            &self.judge,
            &self.frames,
            self.per_frame,
            frame * self.per_frame,
            region.0,
            region.1,
        );
        let count = with_find!(&self.boundary, |find| {
            let mut units = self
                .lookup
                .window
                .records(|data: &[u8]| find(data))
                .watching(&mut watch)
                .into_units();
            if frame == self.frames.len() - 1 {
                units = if self.boundary.tail_is_record() {
                    units.including_final_record()
                } else {
                    units.rejecting_final_fragment()
                };
            }
            let count = units
                .by_ref()
                .try_fold(0u64, |count, record| record.map(|_| count + 1))?;
            units.finish(usize::MAX)?;
            Ok::<u64, anyhow::Error>(count)
        })?;
        // Counting consumes the final unterminated record too, so the next reverse lookup must
        // start from the frame rather than treating the count's EOF position as its first record.
        self.lookup.frame = None;
        Ok(count)
    }
}

impl<V: Verifier, S: Read + Seek> Iterator for RecordIter<V, S> {
    type Item = anyhow::Result<Vec<u8>>;

    #[inline(always)]
    fn next(&mut self) -> Option<Self::Item> {
        if self.spent || self.met_the_reverse_read() {
            return None;
        }
        if !self.armed {
            if let Err(error) = self.arm() {
                self.spent = true;
                return Some(Err(error));
            }
        }
        let cursor = self.cursor.as_mut().expect("armed iterator has a cursor");
        match with_find!(&self.boundary, |find| {
            cursor
                .next(&self.lookup.window, |data: &[u8]| find(data))
                .transpose()
                .map(|run| run.map(|run| self.lookup.window.bytes(&run).to_vec()))
        }) {
            Ok(Some(record)) => {
                self.front += 1;
                Some(Ok(record))
            }
            Ok(None) => {
                // Nothing whole is left in the span: what the walk stopped in is judged where it
                // stopped, as it is for a range read that stops short.
                self.spent = true;
                match cursor.finish(&self.lookup.window, usize::MAX) {
                    Ok(()) => None,
                    Err(error) => Some(Err(error)),
                }
            }
            Err(error) => {
                self.spent = true;
                Some(Err(error))
            }
        }
    }
}

impl<V: Verifier, S: Read + Seek> RecordIter<V, S> {
    /// Whether the forward read has reached the record a reverse read handed out last.
    fn met_the_reverse_read(&self) -> bool {
        self.back_left
            .is_some_and(|left| self.front >= self.back_frame * self.per_frame + left as usize)
    }
}

impl<V: Verifier, S: Read + Seek> DoubleEndedIterator for RecordIter<V, S> {
    fn next_back(&mut self) -> Option<Self::Item> {
        // The window is about to be pointed at the frame this reads from, which is not where the
        // forward read left it.
        self.armed = false;
        loop {
            if self.spent {
                return None;
            }
            let left = match self.back_left {
                Some(left) => left,
                None => match self.count_frame(self.back_frame) {
                    Ok(count) => {
                        if self.back_frame == self.frames.len() - 1
                            && self.front > self.back_frame * self.per_frame + count as usize
                        {
                            // Use the forward walk for the error itself, so both directions
                            // report the same failure for this invalid starting position.
                            return self.next();
                        }
                        self.back_left = Some(count);
                        count
                    }
                    Err(error) => {
                        self.spent = true;
                        return Some(Err(error));
                    }
                },
            };
            if left == 0 {
                // Nothing left in this frame: on to the one before it, which is counted for the
                // reason this one was.
                if self.back_frame == 0 {
                    self.spent = true;
                    return None;
                }
                self.back_frame -= 1;
                self.back_left = None;
                continue;
            }
            let index = left - 1;
            // The two directions meet: the forward read has already handed out this record.
            if (self.back_frame * self.per_frame + index as usize) < self.front {
                self.spent = true;
                return None;
            }
            let item = (|| {
                let skip =
                    self.lookup
                        .walk_to(self.back_frame, self.frames[self.back_frame], index)?;
                let final_frame =
                    self.boundary.tail_is_record() && self.back_frame == self.frames.len() - 1;
                with_find!(&self.boundary, |find| {
                    let mut units = self
                        .lookup
                        .window
                        .records(|data: &[u8]| find(data))
                        .watching(V::watch_nothing())
                        .skip_records(skip)
                        .into_units();
                    if final_frame {
                        units = units.including_final_record();
                    }
                    units
                        .next()
                        .transpose()
                        .map(|run| run.map(|run| self.lookup.window.bytes(&run).to_vec()))
                })
            })();
            match item {
                Ok(Some(record)) => {
                    self.back_left = Some(index);
                    return Some(Ok(record));
                }
                Ok(None) => {
                    self.back_left = Some(index);
                    continue;
                }
                Err(error) => {
                    self.spent = true;
                    return Some(Err(error));
                }
            }
        }
    }
}
