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
    fs::File,
    io::{Read, Seek, SeekFrom, Take, Write},
    path::{Path, PathBuf},
};

use anyhow::Context;
use memchr::memmem::Finder;
use zeekstd::Decoder;

use crate::find::BoxFinder;
use crate::record;
use crate::seekzstdsep_lib::{
    count_records_in_frame, read_records_in_frame, seek_table_decomp_frames,
};

/// The decoder, and the window records are read through.
///
/// The window owns the decoder rather than borrowing it: one that borrowed it could not outlive
/// the call that built it, and [`RecordReader::record`] leaves its window where the record ended
/// for the next lookup to walk on from. Everything that reads the file another way takes the
/// decoder back through [`Self::load_decoder`].
struct Lookup {
    window: record::Reader<Take<Decoder<'static, File>>>,
    /// The frame the window is pointed at, or `None` once the decoder has moved since.
    frame: Option<usize>,
}

impl Lookup {
    fn new(decoder: Decoder<'static, File>) -> Self {
        Self {
            window: record::Reader::new(decoder.take(0)),
            frame: None,
        }
    }

    /// The decoder, for a caller that seeks it itself. What the window holds goes with it: after
    /// an arbitrary seek it no longer reads on from where it says it does.
    fn load_decoder(&mut self) -> &mut Decoder<'static, File> {
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

/// A compressed file held open for reading records by index.
///
/// Holds the decoder, the frame list and frame 0's separator count, plus the window
/// [`Self::record`] last read through: consecutive indices in the same frame decode it once.
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
pub struct RecordReader<V: Verifier = NoVerify> {
    path: PathBuf,
    lookup: Lookup,
    frames: Vec<(u64, u64)>,
    boundary: Boundary,
    /// Records in frame 0, taken as the record count of every frame. Never 0: every record range
    /// divides by it, and [`Self::from_file`] refuses a boundary that leaves it 0.
    sep_cnt: usize,
    judge: V::Judge,
}

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
        watch_frames(judge, first, frame_ends_from(frames, first, start, len))
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
    fn new(path: PathBuf, last: usize, per_frame: u64) -> Self;

    /// Refuses the frame at `i` for the records it turns out to hold.
    fn count(&self, i: usize, held: impl FnOnce() -> u64) -> anyhow::Result<()>;

    /// Refuses the frame at `i` for holding bytes after its last record.
    fn ends_whole(&self, i: usize, ends_whole: impl FnOnce() -> bool) -> anyhow::Result<()>;
}

impl Judge for () {
    #[inline]
    fn new(_path: PathBuf, _last: usize, _per_frame: u64) -> Self {}

    #[inline]
    fn count(&self, _i: usize, _held: impl FnOnce() -> u64) -> anyhow::Result<()> {
        Ok(())
    }

    #[inline]
    fn ends_whole(&self, _i: usize, _ends_whole: impl FnOnce() -> bool) -> anyhow::Result<()> {
        Ok(())
    }
}

/// What [`AsRead`] holds to judge a frame: the file to name in a refusal, the last frame's index
/// because only that one may hold fewer records, and the count every other frame has to hold.
pub struct FrameJudge {
    path: PathBuf,
    last: usize,
    per_frame: u64,
}

impl Judge for FrameJudge {
    fn new(path: PathBuf, last: usize, per_frame: u64) -> Self {
        Self {
            path,
            last,
            per_frame,
        }
    }

    fn count(&self, i: usize, held: impl FnOnce() -> u64) -> anyhow::Result<()> {
        verify_frame_count(&self.path, i, self.last, held(), self.per_frame)
    }

    fn ends_whole(&self, i: usize, ends_whole: impl FnOnce() -> bool) -> anyhow::Result<()> {
        verify_frame_ends_whole(&self.path, i, self.last, ends_whole())
    }
}

/// Refuses frame `i` holding a count other than `per_frame`. Only the frame the file ends with may
/// hold fewer.
fn verify_frame_count(
    path: &Path,
    i: usize,
    last: usize,
    held: u64,
    per_frame: u64,
) -> anyhow::Result<()> {
    if held != per_frame && !(i == last && held < per_frame) {
        anyhow::bail!(
            "frame {i} of {} holds {held} records rather than {per_frame}: a record index is \
             resolved by dividing it by the count frame 0 holds, so a frame holding another count \
             is read at the wrong offsets",
            path.display()
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

/// What judges the frames a read walks past, frame `first` being the one the walk starts in.
fn watch_frames<J: Judge>(judge: &J, first: usize, ends: Vec<u64>) -> record::Watch<'_> {
    let mut walked = 0;
    record::Watch::new(ends, move |i, before, ends_whole| {
        let frame = first + i;
        judge.count(frame, || before - walked)?;
        walked = before;
        judge.ends_whole(frame, || ends_whole)
    })
}

/// Refuses frame `i` holding bytes after its last record. Only the frame the file ends with may.
fn verify_frame_ends_whole(
    path: &Path,
    i: usize,
    last: usize,
    ends_whole: bool,
) -> anyhow::Result<()> {
    if !ends_whole && i != last {
        anyhow::bail!(
            "frame {i} of {} holds bytes after its last record: a record spans its end, so the \
             frames do not divide the file into records and a count per frame does not place them",
            path.display()
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
        record::check_separator(separator)?;
        Self::build(
            path,
            file,
            Boundary::Separator {
                finder: Finder::new(separator).into_owned(),
                separator: separator.to_vec(),
            },
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
        Self::build(path, file, Boundary::Finder(find))
    }

    /// The reader both entry points build. A refusal names the separator where there is one, since
    /// passing the wrong one is what leaves frame 0 holding no record.
    fn build(path: PathBuf, file: File, boundary: Boundary) -> anyhow::Result<Self> {
        let decoder = Decoder::new(file)
            .with_context(|| format!("failed to open {} as a seekable zst", path.display()))?;
        let frames = seek_table_decomp_frames(&decoder)
            .ok_or_else(|| anyhow::anyhow!("no frames in {}", path.display()))?;
        let mut reader = Self {
            path,
            lookup: Lookup::new(decoder),
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
                anyhow::bail!("no record ends in frame 0 of {}", reader.path.display());
            }
            anyhow::bail!(
                "no record in frame 0 of {} ends with {:?}: a file does not record the separator \
                 it was written with, so pass the one it was",
                reader.path.display(),
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
    pub fn verifying(self) -> RecordReaderVerify {
        let judge = FrameJudge::new(
            self.path.clone(),
            self.frames.len() - 1,
            self.sep_cnt as u64,
        );
        RecordReader {
            path: self.path,
            lookup: self.lookup,
            frames: self.frames,
            boundary: self.boundary,
            sep_cnt: self.sep_cnt,
            judge,
        }
    }
}

impl<V: Verifier> RecordReader<V> {
    /// The file this reads from.
    pub fn path(&self) -> &PathBuf {
        &self.path
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

    /// How many whole records the file holds. Decompresses the last frame to count it, since the
    /// invariant says nothing about how full it is.
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
        let in_last = with_find!(&self.boundary, |find| count_records_in_frame(
            self.lookup.load_decoder(),
            start,
            len,
            find
        )?);
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
    pub fn into_bytes(self) -> anyhow::Result<impl Read + Send + 'static> {
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
        let total_sep_cnt = self.sep_cnt * self.frames.len();
        let frame_idx = self.frames.len().saturating_mul(from) / total_sep_cnt;
        if frame_idx >= self.frames.len() {
            return Err(anyhow::anyhow!(
                "record {from} is past the end of {}",
                self.path.display()
            ));
        }
        let idx_in_frame = from % self.sep_cnt;
        let start = self.frames[frame_idx].0;

        let end = from.saturating_add(cnt).saturating_add(1);
        let end_frame_idx =
            (self.frames.len().saturating_mul(end) / total_sep_cnt).min(self.frames.len() - 1);
        let len = self.frames[end_frame_idx].0 + self.frames[end_frame_idx].1 - start;
        Ok(RecordsRequest {
            start,
            len,
            skip: idx_in_frame as u64,
        })
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
        // A whole-span read has no walk to watch, so the verifying one takes the route that has
        // one and gathers what it writes. The other arm is not compiled for either.
        if V::VERIFIES {
            let mut out = Vec::new();
            self.records_to(from, cnt, &mut out)?;
            return Ok(out);
        }
        let req = self.records_request(from, cnt)?;
        with_find!(&self.boundary, |find| read_records_in_frame(
            self.lookup.load_decoder(),
            req.start,
            req.len,
            req.skip,
            cnt as u64,
            find,
        ))
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
        let req = self.records_request(from, cnt)?;
        let watch = V::watch(
            &self.judge,
            &self.frames,
            self.sep_cnt,
            from,
            req.start,
            req.len,
        );
        self.lookup.frame = None;
        self.lookup.window.seek_to(req.start, req.len)?;
        with_find!(&self.boundary, |find| self
            .lookup
            .window
            .records(|data| find(data))
            .watching(watch)
            .skip_records(req.skip)?
            .take_records(cnt as u64)
            .write_to(dst))
    }
}

impl RecordReader<AsRead> {
    /// [`RecordReader::into_records`], stopping at the first frame [`AsRead`] refuses — one
    /// holding a count other than frame 0's, or bytes after its last record.
    /// A reverse read checks the whole frame before returning any of its records.
    pub fn into_records(self) -> RecordIter<AsRead> {
        RecordIter {
            frames: self.frames,
            frame: 0,
            armed: false,
            front_read: 0,
            back_left: None,
            boundary: self.boundary,
            lookup: self.lookup,
            judge: self.judge,
        }
    }
}

impl RecordReader<NoVerify> {
    /// Every whole record in the file, in order, decoding a window at a time.
    ///
    /// Scans rather than divides, so unlike [`Self::record`] it does not rest on the
    /// same-count-per-frame invariant. What follows the last separator of a frame is dropped: the
    /// compressor cuts frames at separator boundaries, so only the end of the file can hold one.
    /// [`DoubleEndedIterator::next_back`] reads from the end; the two directions consume the
    /// same remaining records without overlap. See [`RecordIter`] for reverse reading costs.
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
    pub fn into_records(self) -> RecordIter {
        RecordIter {
            frames: self.frames,
            frame: 0,
            armed: false,
            front_read: 0,
            back_left: None,
            boundary: self.boundary,
            lookup: self.lookup,
            judge: (),
        }
    }
    /// Record `index`, or `None` when the file holds no such whole record.
    ///
    /// The returned bytes carry the separator, as [`Self::records`] does. A trailing fragment with
    /// no separator after it is not a record and is not returned.
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
        let frame = index / self.sep_cnt;
        if frame >= self.frames.len() {
            return Ok(None);
        }
        let in_frame = (index % self.sep_cnt) as u64;
        let region = self.frames[frame];
        let skip = self.lookup.walk_to(frame, region, in_frame)?;
        with_find!(&self.boundary, |find| {
            let (records, skipped) = self.lookup.window.records(find).skip_up_to(skip)?;
            if skipped < skip {
                return Ok(None);
            }
            records.next_owned()
        })
    }
}

impl RecordReader<AsRead> {
    /// [`RecordReader::record`], refusing a frame the walk ran out in that holds a record count of
    /// its own.
    ///
    /// # Errors
    ///
    /// A frame the walk ran out in that holds a count other than frame 0's — where
    /// [`RecordReader::record`] answers `None`.
    pub fn record(&mut self, index: usize) -> anyhow::Result<Option<Vec<u8>>> {
        let frame = index / self.sep_cnt;
        if frame >= self.frames.len() {
            return Ok(None);
        }
        let in_frame = (index % self.sep_cnt) as u64;
        let region = self.frames[frame];
        let skip = self.lookup.walk_to(frame, region, in_frame)?;
        with_find!(&self.boundary, |find| {
            let (records, skipped) = self
                .lookup
                .window
                .records(find)
                .watching(record::Watch::nothing())
                .skip_up_to(skip)?;
            if skipped < skip {
                self.verify_walked_frame(frame)?;
                return Ok(None);
            }
            let record = records.next_owned()?;
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

/// Every whole record of a [`RecordReader`], from either end. Made by [`RecordReader::into_records`].
///
/// Decodes each frame through the record stream's fixed window, so no frame has to fit in
/// memory — only the record being handed out does.
///
/// A reverse read first scans the frame to count its records. Records still in the window reuse
/// those bytes; reaching an earlier record outside it decodes from that frame's start again.
/// Either direction stops permanently after an error, or when the remaining records run out.
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
pub struct RecordIter<V: Verifier = NoVerify> {
    frames: Vec<(u64, u64)>,
    /// The frame being handed out, past the last one once the iterator is spent.
    frame: usize,
    /// Whether the window is positioned at the next forward record.
    armed: bool,
    /// Forward position saved while the window is used by a reverse read.
    front_read: u64,
    /// Exclusive back record index in the last remaining frame, once counted.
    back_left: Option<u64>,
    boundary: Boundary,
    lookup: Lookup,
    judge: V::Judge,
}

impl<V: Verifier> RecordIter<V> {
    fn check_frame(&self, frame: usize) -> anyhow::Result<()> {
        self.judge
            .count(frame, || self.lookup.window.walked())
            .and_then(|()| {
                self.judge
                    .ends_whole(frame, || self.lookup.window.remainder().is_empty())
            })
    }
}

impl<V: Verifier> Iterator for RecordIter<V> {
    type Item = anyhow::Result<Vec<u8>>;

    #[inline(always)]
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if !self.armed {
                if self.frame >= self.frames.len() {
                    return None;
                }
                let positioned = (|| {
                    let skip = self.lookup.walk_to(
                        self.frame,
                        self.frames[self.frame],
                        self.front_read,
                    )?;
                    with_find!(&self.boundary, |find| self
                        .lookup
                        .window
                        .records(|data| find(data))
                        .watching(V::watch_nothing())
                        .skip_up_to(skip)
                        .map(|_| ()))?;
                    Ok::<_, anyhow::Error>(())
                })();
                if let Err(error) = positioned {
                    self.frame = self.frames.len();
                    return Some(Err(error));
                }
                self.armed = true;
            }
            if let Some(back) = self.back_left {
                if self.frame + 1 == self.frames.len() && back == self.lookup.window.walked() {
                    self.frame = self.frames.len();
                    self.armed = false;
                    return None;
                }
            }
            match with_find!(&self.boundary, |find| self
                .lookup
                .window
                .records(find)
                .watching(V::watch_nothing())
                .next_owned())
            {
                Ok(Some(item)) => return Some(Ok(item)),
                Err(e) => {
                    self.frame = self.frames.len();
                    self.armed = false;
                    return Some(Err(e));
                }
                Ok(None) => {
                    // Only a fragment, or nothing, is left in this frame: judge what it turned out
                    // to hold, then drop it and move on as the frame-at-a-time iterator did.
                    let checked = self.check_frame(self.frame);
                    if let Err(e) = checked {
                        self.frame = self.frames.len();
                        self.armed = false;
                        return Some(Err(e));
                    }
                    self.frame += 1;
                    self.armed = false;
                    self.front_read = 0;
                }
            }
        }
    }
}

impl<V: Verifier> DoubleEndedIterator for RecordIter<V> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.armed {
            self.front_read = self.lookup.window.walked();
            self.armed = false;
        }
        loop {
            if self.frame >= self.frames.len() {
                return None;
            }
            let frame = self.frames.len() - 1;
            let item = (|| {
                let left = match self.back_left {
                    Some(left) => left,
                    None => {
                        self.lookup.walk_to(frame, self.frames[frame], 0)?;
                        let count = with_find!(&self.boundary, |find| self
                            .lookup
                            .window
                            .records(|data| find(data))
                            .watching(V::watch_nothing())
                            .count_records())?;
                        self.check_frame(frame)?;
                        count as u64
                    }
                };
                if left == 0 || (self.frame == frame && left == self.front_read) {
                    self.frames.pop();
                    self.back_left = None;
                    return Ok(None);
                }
                let index = left - 1;
                let skip = self.lookup.walk_to(frame, self.frames[frame], index)?;
                let record = with_find!(&self.boundary, |find| self
                    .lookup
                    .window
                    .records(|data| find(data))
                    .watching(V::watch_nothing())
                    .skip_records(skip)?
                    .next_owned())?;
                self.back_left = Some(index);
                Ok(record)
            })();
            match item {
                Ok(Some(record)) => return Some(Ok(record)),
                Ok(None) => continue,
                Err(error) => {
                    self.frame = self.frames.len();
                    return Some(Err(error));
                }
            }
        }
    }
}
