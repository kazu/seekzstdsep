//! Operations that work at frame boundaries: [`truncate`] and [`append`] modify a file,
//! [`copy_range`] derives a second one from it.
//!
//! The first two are destructive and rewrite nothing before the first byte they affect.
//! Non-destructive use of them is the caller's job: clone the file first. See
//! `docs/design/2026-08-24-truncate-append-split-concat.md`.
//!
//! Every operation checks the separator against the file first, since the file does not record
//! which one it was built with and reading a record range by the wrong one addresses the wrong
//! records.
//!
//! [`truncate`] and [`append`] compare the first frame against the last one that is not allowed to
//! be short: the two have to hold the same number of separators, and that number is the records per
//! frame. They therefore refuse a file of fewer than three frames, where the comparison cannot be
//! made. [`copy_range`] reads the count off frame 0 alone and checks that frame 0 *ends* with the
//! separator, which is where the compressor cuts; [`SeparatorCheck::TwoFrames`] asks for the
//! comparison as well.
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
};

use crate::record;
use crate::seekzstdsep_lib::{
    READ_BUF_SIZE, READ_FRAME_BUF_SIZE, decompressed_range_into, frame_encoder,
};
use anyhow::{Context, bail};
use memchr::memmem::Finder;
use zeekstd::{DecodeOptions, SeekTable};

/// What to do with a file whose last byte is not the separator, so that it ends in a fragment
/// rather than in a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnMissingSeparator {
    /// Refuse to join. Joining would merge the fragment with the first appended record and shift
    /// every later record index by one, silently.
    #[default]
    Refuse,
    /// Write one separator at the join, making the fragment a record of its own.
    Insert,
}

/// Whether the result has to be *aligned*: every frame holding the same number of records.
///
/// The frame a file ends with generally holds fewer than the rest, so a range reaching the end of
/// one is unaligned unless the file was written to be aligned in the first place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Alignment {
    /// Refuse a range whose last frame holds a count of its own.
    #[default]
    Required,
    /// Copy that frame as it is, leaving a result that is not aligned.
    NotRequired,
}

/// How many frames the separator is checked against before a range is copied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SeparatorCheck {
    /// Frame 0 alone. It has to end with the separator, which is where the compressor cuts, and
    /// the separators it holds are the records per frame.
    #[default]
    FirstFrame,
    /// A second frame as well, refused when the two hold different record counts.
    TwoFrames,
}

/// Shortens `f` to `record_len` records, which is the length that remains, not the number removed.
///
/// The cut is a frame boundary, so the frames that stay are left byte for byte as they were and
/// nothing is re-encoded. Dropping a trailing fragment or a short last frame is truncating to the
/// last boundary before it.
///
/// # Errors
///
/// Refuses a `record_len` of 0, one past the records `f` holds, or one that does not land on a
/// frame boundary, along with the refusals every operation in [this module](crate::edit) shares.
pub fn truncate(f: &mut File, record_len: u64, separator: &[u8]) -> anyhow::Result<()> {
    let (finder, table) = open_target(f, separator)?;
    let frames = table.num_frames();

    let mut reader = FrameReader::new(&*f, &table)?;
    let n = validate_separator(&mut reader, &finder)?;
    let last = frame_records(&mut reader, &finder, frames - 1)?;
    let total = n * u64::from(frames - 1) + last;

    if record_len == 0 {
        bail!("refusing to truncate to 0 records: a file of no frames cannot be read back");
    }
    if record_len > total {
        bail!("refusing to truncate to {record_len} records: the file holds {total}");
    }
    // The last frame does not have to hold n records, so a multiple of n past the other frames is
    // not a boundary either.
    let k = record_len / n;
    if record_len % n != 0 || (k >= u64::from(frames) && last != n) {
        bail!(
            "refusing to truncate to {record_len} records: the cut has to land on a frame \
             boundary, and a frame holds {n} records"
        );
    }

    // Every read of `f` is done, and the writes below need it exclusively. The decoder holds it
    // until it is dropped.
    drop(reader);

    cut_at(f, table.frame_end_comp(k as u32 - 1)?)?;

    let mut out = SeekTable::new();
    log_frames(&mut out, &table, k as u32)?;
    write_seek_table(f, out)
}

/// What [`append`] adds to a file.
#[derive(Debug)]
pub enum AppendInput<'a, R> {
    /// Records, as plain bytes.
    Records {
        /// The bytes to append. They are cut into frames at the records per frame the file was
        /// built with.
        data: R,
        /// What to do with a file that ends in a fragment rather than in a record.
        on_missing: OnMissingSeparator,
        /// Zstandard compression level of the frames this writes. 0 uses the zstd default.
        level: i32,
    },
    /// A record range of another seekable file, whose frames are copied as compressed bytes.
    ///
    /// Nothing is decoded and nothing is re-encoded, so the cost is the size of the range rather
    /// than of either file. That is only available where the frames fit together unchanged, which
    /// is what the extra refusals of this path establish.
    Frames {
        /// The file to copy frames from. Read and never written.
        input: &'a File,
        /// The first record to copy, which has to be the first record of a frame.
        from: u64,
        /// The records to copy, ending at the first record of a frame or at the end of `input`.
        /// [`None`] runs to the end.
        cnt: Option<u64>,
        /// How many of the copied frames are counted before any of them are taken.
        check: RangeCheck,
    },
}

/// How many of the frames being copied are counted before they are taken.
///
/// Only the count of the frames that end up in the result matters, and reading it off one frame
/// rests on the file having been written with a uniform count. Another writer's file need not have
/// been.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RangeCheck {
    /// Frame 0 of the input alone, which is where its records per frame is read from.
    ///
    /// A frame elsewhere in the range holding a different count is not seen, and copying it puts a
    /// short frame in the interior of the result, where record lookup divides by a count that no
    /// longer holds and returns the wrong records with no error.
    #[default]
    FirstFrame,
    /// Every frame of the copied range, which is the only check that rules the above out.
    ///
    /// It decompresses the range, so it costs what copying the bytes exists to avoid. That is the
    /// price of not trusting whoever wrote the input.
    EveryFrame,
}

/// Adds `input` to the end of `f`, keeping every frame but the last at the records per frame the
/// file was built with.
///
/// Nothing before the frame the operation affects is read or written.
///
/// # Errors
///
/// Whatever the variant of `input` refuses, along with the refusals every operation in
/// [this module](crate::edit) shares.
pub fn append<R: Read>(
    f: &mut File,
    input: AppendInput<'_, R>,
    separator: &[u8],
) -> anyhow::Result<()> {
    match input {
        AppendInput::Records {
            data,
            on_missing,
            level,
        } => append_records(f, data, separator, on_missing, level),
        AppendInput::Frames {
            input,
            from,
            cnt,
            check,
        } => append_frames(f, input, from, cnt, separator, check),
    }
}

/// Appends the records `data` holds to `f`, which is [`AppendInput::Records`] called directly.
///
/// A caller that only ever appends records can reach this instead of going through [`append`], and
/// says so at the call site by doing it.
///
/// The last data frame generally holds fewer records than the rest, so appending after it would
/// leave a short frame in the interior. It is decoded, joined with `data` and cut again instead.
/// An empty `data` rewrites nothing. The frames this writes are compressed at `level`, 0 being
/// the zstd default; the frames before them keep whatever they were written with.
///
/// # Errors
///
/// Refuses a file that does not end with a whole record unless `on_missing` is
/// [`OnMissingSeparator::Insert`], which refuses in turn where writing one separator leaves
/// another fragment.
pub fn append_records(
    f: &mut File,
    mut data: impl Read,
    separator: &[u8],
    on_missing: OnMissingSeparator,
    level: i32,
) -> anyhow::Result<()> {
    let (finder, table) = open_target(f, separator)?;

    // Before the subtraction below: a seek table can hold no entries at all, and validation is
    // what refuses that.
    let mut reader = FrameReader::new(&*f, &table)?;
    let n = validate_separator(&mut reader, &finder)? as usize;
    // Replace the frame the last record is in; the set_len below drops the empty frames with it.
    let last = last_data_frame(&table)?;

    // Before anything is decoded, so that appending nothing costs one read and rewrites nothing.
    // A pipe hands back what has arrived rather than what was asked for, so the first bytes are
    // gathered until the check below can be made on them.
    let mut head = vec![0u8; READ_BUF_SIZE];
    let mut read = 0;
    while read < ZSTD_MAGIC.len() {
        let got = data.read(&mut head[read..])?;
        if got == 0 {
            break;
        }
        read += got;
    }
    if read == 0 {
        return Ok(());
    }
    head.truncate(read);

    // Compressed bytes are frames, not records. Counting the separator bytes a compressed stream
    // holds by chance as records corrupts silently rather than failing, so the one thing this path
    // must not accept is its own output. Reading the magic number costs nothing and needs no seek,
    // which is what lets it cover a pipe as well as a file.
    if head.starts_with(&ZSTD_MAGIC) {
        bail!(
            "refusing to append a zstd stream as records: its bytes are frames, and the separator \
             bytes a compressed stream holds by chance would each be counted as a record. \
             Appending the frames of a seekable zst is a different operation — \
             `AppendInput::Frames`, or `--input-seekable` on the command line"
        );
    }

    let checksum = frame_has_checksum(f, &table, last)?;
    let mut tail = reader.take_frame(last)?;
    drop(reader);
    if !record::ends_whole(&tail, &finder, separator.len()) {
        match on_missing {
            OnMissingSeparator::Refuse => bail!(
                "refusing to append to a file that does not end with a whole record: the first \
                 appended record would merge with the fragment it ends in"
            ),
            OnMissingSeparator::Insert => {
                tail.extend_from_slice(separator);
                if !record::ends_whole(&tail, &finder, separator.len()) {
                    bail!(
                        "refusing to append: writing a separator leaves the fragment the file \
                         ends in a fragment, which is what a separator overlapping itself does"
                    );
                }
            }
        }
    }
    tail.extend_from_slice(&head);

    // The records already in the file go in front of the ones being appended, so the join is cut
    // like any other record boundary.
    let mut cutter = Cutter::new(
        std::io::Cursor::new(tail).chain(data),
        &finder,
        separator.len(),
        n,
    );
    // The first frame holds the records the file already has, and they exist nowhere else between
    // the set_len below and the write that follows it. Compress it first, so that window is a
    // write rather than a compression.
    let (first, whole) = match cutter.next_group()? {
        Some(group) => (group, true),
        None => (cutter.take_remainder(), false),
    };
    let (first_bytes, first_comp, first_decomp) = encode_frame(&first, checksum, level)?;

    let cut = table.frame_start_comp(last)?;
    cut_at(f, cut)?;
    f.write_all(&first_bytes)?;

    let mut out = SeekTable::new();
    log_frames(&mut out, &table, last)?;
    out.log_frame(first_comp, first_decomp)?;

    if whole {
        let mut encoder = frame_encoder(&mut *f, checksum, level)?;
        let mut frames = 0;
        while let Some(group) = cutter.next_group()? {
            encoder.write_all(&group)?;
            encoder.end_frame()?;
            frames += 1;
        }
        let remainder = cutter.take_remainder();
        if !remainder.is_empty() {
            encoder.write_all(&remainder)?;
            encoder.end_frame()?;
            frames += 1;
        }
        if frames > 0 {
            encoder.flush()?;
            let written = encoder.into_seek_table();
            if written.num_frames() as usize != frames {
                bail!(
                    "compressing {frames} frames of {n} records produced {}: a frame of that many \
                     records is larger than one zstd frame can hold",
                    written.num_frames()
                );
            }
            log_frames(&mut out, &written, written.num_frames())?;
        }
    }

    write_seek_table(f, out)
}

/// Appends `cnt` records of `input`, starting at record `from`, to `f` as the frames they already
/// are, which is [`AppendInput::Frames`] called directly.
///
/// Nothing is decoded of the range itself and nothing is re-encoded, which is available only where
/// the frames of the two files fit together as they stand: they have to hold the same number of
/// records, and `f` has to end at a frame boundary rather than partway through one. `input` may end
/// in a short frame, which becomes the last frame of the result.
///
/// # Errors
///
/// Refuses a record count per frame that differs between the two files, an `f` whose last data
/// frame is short or which does not end with a whole record, and a range that does not start at the
/// first record of a frame or end at one or at the end of `input`. Under
/// [`RangeCheck::EveryFrame`], also a frame inside the range holding a count of its own.
pub fn append_frames(
    f: &mut File,
    input: &File,
    from: u64,
    cnt: Option<u64>,
    separator: &[u8],
    check: RangeCheck,
) -> anyhow::Result<()> {
    let (finder, table) = open_target(f, separator)?;
    let mut reader = FrameReader::new(&*f, &table)?;
    let n = validate_separator(&mut reader, &finder)?;
    let last = last_data_frame(&table)?;

    // The frames being copied go after this one, so it is the one that has to be full: a short
    // frame here would end up in the interior of the result. It is decoded whatever happens, so
    // the check that the file ends with a whole record costs nothing beyond it.
    let end = reader.take_frame(last)?;
    drop(reader);
    if !record::ends_whole(&end, &finder, separator.len()) {
        bail!(
            "refusing to append to a file that does not end with a whole record: the first \
             appended record would merge with the fragment it ends in"
        );
    }
    let held = record::count(&end, &finder) as u64;
    if held != n {
        bail!(
            "refusing to append frames after frame {last}, which holds {held} records rather than \
             {n}: copying frames after a short one would leave it in the interior"
        );
    }

    // Frame 0 is where the input's records per frame is read from, and it has to be the count the
    // file being appended to holds. Whether the frames actually taken hold it too is what `check`
    // decides: reading it off frame 0 rests on the input having been written with a uniform count.
    let (in_finder, in_table) = finder_and_seek_table(input, separator)
        .context("refusing to append the frames of a file that is not a seekable zst")?;
    let mut in_reader = FrameReader::new(input, &in_table)?;
    let in_n = records_per_frame(&mut in_reader, &in_finder, SeparatorCheck::FirstFrame)?;
    if in_n != n {
        bail!(
            "refusing to append a file holding {in_n} records per frame to one holding {n}: the \
             frames only fit together unchanged where the two counts are equal"
        );
    }

    // The last frame of the input is allowed to be short, so what frame_range reports about it is
    // not asked for here: it becomes the last frame of the result, which is where a short frame is
    // permitted.
    let (k, j, _) = frame_range(&mut in_reader, &in_finder, n, from, cnt)?;
    if k == j {
        return Ok(());
    }
    if check == RangeCheck::EveryFrame {
        check_range_uniform(&mut in_reader, &in_finder, n, k, j)?;
    }
    let start = in_table.frame_start_comp(k)?;
    let stop = in_table.frame_end_comp(j - 1)?;

    // At the end of the last data frame rather than at its start: nothing is re-encoded, so no
    // records pass through memory and the window loses only the seek table. The cut takes the
    // frames carrying nothing with it, which must not survive into the interior of the result.
    cut_at(f, table.frame_end_comp(last)?)?;

    let mut src = input;
    src.seek(SeekFrom::Start(start))?;
    std::io::copy(&mut src.take(stop - start), &mut *f)?;

    let mut out = SeekTable::new();
    log_frames(&mut out, &table, last + 1)?;
    log_frames_range(&mut out, &in_table, k..j)?;
    write_seek_table(f, out)
}

/// Writes `cnt` records of `input`, starting at record `from`, to `output` as a seekable file of
/// its own. A `cnt` of `None` runs to the end of the file.
///
/// The frames are copied as compressed bytes and only the seek table is built fresh, so the cost is
/// the size of the range rather than of the file. `input` is read and never written.
///
/// # Errors
///
/// Refuses a range that does not start at the first record of a frame, or end at one or at the end
/// of the file, and a `cnt` of 0. Unless `align` is [`Alignment::NotRequired`], refuses a range
/// whose last frame holds a record count of its own, which the frame a file ends with generally
/// does. Along with the refusals every operation in [this module](crate::edit) shares.
pub fn copy_range(
    input: &File,
    mut output: impl Write,
    from: u64,
    cnt: Option<u64>,
    separator: &[u8],
    align: Alignment,
    check: SeparatorCheck,
) -> anyhow::Result<()> {
    let (finder, table) = finder_and_seek_table(input, separator)?;
    let mut reader = FrameReader::new(input, &table)?;
    let n = records_per_frame(&mut reader, &finder, check)?;

    let (k, j, tail) = frame_range(&mut reader, &finder, n, from, cnt)?;
    if cnt == Some(0) {
        bail!("refusing to copy 0 records: a file of no frames cannot be read back");
    }
    if let Some(tail) = tail
        && tail != n
        && align == Alignment::Required
    {
        bail!(
            "refusing to copy frame {}, which holds {tail} records rather than {n}: the result \
             would not hold the same count in every frame",
            j - 1
        );
    }

    let mut src = input;
    let start = table.frame_start_comp(k)?;
    let end = table.frame_end_comp(j - 1)?;
    src.seek(SeekFrom::Start(start))?;
    std::io::copy(&mut src.take(end - start), &mut output)?;

    let mut out = SeekTable::new();
    log_frames_range(&mut out, &table, k..j)?;
    write_seek_table_to(&mut output, out)
}

/// Cuts a stream into groups of exactly `records` records.
///
/// A record ends with the separator, so a group ends immediately after its last one. What is left
/// when the stream runs out is the remainder, which is shorter than a group and belongs in the
/// frame the file ends with.
struct Cutter<'a, R> {
    stream: record::Stream<'a, R>,
    records: usize,
    found: usize,
}

impl<'a, R: Read> Cutter<'a, R> {
    fn new(reader: R, finder: &'a Finder<'a>, separator_len: usize, records: usize) -> Self {
        Self {
            stream: record::Stream::with_capacity(reader, finder, separator_len, READ_BUF_SIZE),
            records,
            found: 0,
        }
    }

    /// The next group, or `None` once what is left cannot fill one.
    fn next_group(&mut self) -> anyhow::Result<Option<Vec<u8>>> {
        loop {
            while let Some(end) = self.stream.next_end() {
                self.found += 1;
                if self.found == self.records {
                    let group = self.stream.buffered()[..end].to_vec();
                    self.stream.drop_to_last_end();
                    self.found = 0;
                    return Ok(Some(group));
                }
            }
            if !self.stream.fill()? {
                return Ok(None);
            }
        }
    }

    /// The bytes that fill no group, leaving the cutter empty.
    fn take_remainder(&mut self) -> Vec<u8> {
        self.stream.take_buffered()
    }
}

/// The separator search and the seek table, which is what every operation reads before it writes.
///
/// The table is kept rather than read again: [`cut_at`] destroys the copy on disk.
///
/// # Errors
///
/// An empty `separator`, and a file whose seek table cannot be read.
fn open_target<'a>(f: &mut File, separator: &'a [u8]) -> anyhow::Result<(Finder<'a>, SeekTable)> {
    finder_and_seek_table(f, separator)
}

/// [`open_target`] for an operation that only reads `f`, which takes no exclusive borrow to state
/// that it rewrites nothing.
fn finder_and_seek_table<'a>(
    f: &File,
    separator: &'a [u8],
) -> anyhow::Result<(Finder<'a>, SeekTable)> {
    record::check_separator(separator)?;
    let mut src = f;
    Ok((Finder::new(separator), SeekTable::from_seekable(&mut src)?))
}

/// Shortens `f` to `cut` and positions it there, which is where the replacement frames go.
///
/// This opens the window in which the file cannot be read: it carries no seek table from here
/// until [`write_seek_table`] has finished.
fn cut_at(f: &mut File, cut: u64) -> anyhow::Result<()> {
    f.set_len(cut)?;
    f.seek(SeekFrom::Start(cut))?;
    Ok(())
}

/// Confirms `finder`'s separator is the one `f` was built with, and returns the records per frame.
///
/// Compares the first frame against the last one that is not allowed to be short, so a count that
/// drifts anywhere between them is caught. Only those two are decoded: a frame in the middle that
/// differs from both is not detected, and finding it would mean decompressing the whole file.
fn validate_separator(reader: &mut FrameReader, finder: &Finder) -> anyhow::Result<u64> {
    let frames = reader.table.num_frames();
    if frames < 3 {
        bail!(
            "refusing to validate the separator against {frames} frames: the last frame is \
             legitimately short, so fewer than three cannot be compared"
        );
    }

    let first = record::count(reader.frame(0)?, finder);
    if first == 0 {
        bail!("the separator does not occur in frame 0");
    }
    let last_full = frames - 2;
    let second = record::count(reader.frame(last_full)?, finder);
    if first != second {
        bail!(
            "frame 0 holds {first} separators and frame {last_full} holds {second}: either the \
             separator is wrong or the file does not hold a uniform count"
        );
    }

    Ok(first as u64)
}

/// The four bytes every zstd frame starts with.
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

/// Whether frame `index` ends with a content checksum, read from bit 2 of its frame header
/// descriptor.
///
/// Public because it is the only way to ask: the seek table does not record it, and an operation
/// that rewrites a frame has to match whatever the file already carries.
///
/// Seeking to the frame header moves the file's position, and a [`FrameReader`] over the same file
/// shares it — a decoder held open across frames reads from where it left off, and only re-seeks
/// when the frame it is asked for moves. So the position is put back, and asking this between two
/// frame reads is safe rather than a corruption that only shows on frames larger than the decoder's
/// input buffer.
///
/// # Errors
///
/// A frame that does not start with the zstd magic number.
pub fn frame_has_checksum(mut f: &File, table: &SeekTable, index: u32) -> anyhow::Result<bool> {
    let at = f.stream_position()?;
    let found = frame_checksum_at(f, table, index);
    f.seek(SeekFrom::Start(at))?;
    found
}

fn frame_checksum_at(mut f: &File, table: &SeekTable, index: u32) -> anyhow::Result<bool> {
    f.seek(SeekFrom::Start(table.frame_start_comp(index)?))?;
    let mut head = [0u8; 5];
    f.read_exact(&mut head)?;
    if head[..4] != ZSTD_MAGIC {
        bail!("frame {index} does not start with the zstd magic number");
    }
    Ok(head[4] & 0b100 != 0)
}

/// The records per frame, read off frame 0 alone.
///
/// The compressor ends a frame immediately after a separator, so a candidate that does not end
/// frame 0 is not the one the file was built with — that is the whole check, and it costs the one
/// frame the count has to be read from anyway. Only the frame a file ends with is allowed to end
/// elsewhere, so frame 0 serves as long as it is not that frame.
///
/// [`SeparatorCheck::TwoFrames`] counts the farthest frame that is not allowed to be short as well,
/// which catches a count that drifts between the two at the price of a second frame decoded.
fn records_per_frame(
    reader: &mut FrameReader,
    finder: &Finder,
    check: SeparatorCheck,
) -> anyhow::Result<u64> {
    let last = last_data_frame(reader.table)?;
    if last == 0 {
        bail!(
            "refusing to read the separator off a file of one data frame: that frame is the one \
             allowed to end anywhere, so a separator that does not end it proves nothing"
        );
    }

    // Both answers are taken from the one borrow, so that reading a second frame below does not
    // need this one copied out to survive it.
    let first = reader.frame(0)?;
    let n = record::count(first, finder) as u64;
    let ends_whole = record::ends_whole(first, finder, finder.needle().len());

    // Before the check below, which would refuse a separator that occurs nowhere for the less
    // specific of the two reasons.
    if n == 0 {
        bail!("the separator does not occur in frame 0");
    }
    if !ends_whole {
        bail!(
            "frame 0 does not end with the separator: a frame ends immediately after one, so this \
             is not the separator the file was built with"
        );
    }

    if check == SeparatorCheck::TwoFrames {
        let full = last - 1;
        if full == 0 {
            bail!(
                "refusing to compare two frames in a file of two data frames: frame 0 is the only \
                 one that has to hold a full count"
            );
        }
        let second = frame_records(reader, finder, full)?;
        if n != second {
            bail!(
                "frame 0 holds {n} records and frame {full} holds {second}: either the separator \
                 is wrong or the file does not hold a uniform count"
            );
        }
    }

    Ok(n)
}

/// Confirms every frame of `k..j` holds `n` records, which is what reading the count off frame 0
/// alone leaves open.
///
/// Only the frame the range ends at may hold fewer, and only when it is the frame the file ends
/// with. A short frame anywhere earlier lands in the interior of the result, where record lookup
/// divides by a count that no longer holds. A short frame that ends the range without ending the
/// file is legal in the result, but it means the range holds fewer records than were asked for, so
/// it is refused too rather than delivered short.
///
/// Costs the range decompressed, which is what copying the bytes exists to avoid. It is therefore
/// asked for rather than assumed. The whole range is counted before any of it is judged, so a
/// refusal costs the same as an acceptance; the refusal still comes before anything is written.
fn check_range_uniform(
    reader: &mut FrameReader,
    finder: &Finder,
    n: u64,
    k: u32,
    j: u32,
) -> anyhow::Result<()> {
    let last = last_data_frame(reader.table)?;
    for (offset, held) in frame_counts(reader, finder, k..j)?.into_iter().enumerate() {
        let i = k + offset as u32;
        if held == n {
            continue;
        }
        if i == j - 1 && i == last && held < n {
            continue;
        }
        bail!(
            "refusing to copy frame {i}, which holds {held} records rather than {n}: a short frame \
             before the end of the range lands in the interior of the result, where record lookup \
             divides by a count that no longer holds, and one ending a range that does not end the \
             file means fewer records than were asked for"
        );
    }
    Ok(())
}

/// The frames a record range covers, as `k..j`, along with the record count of the last data frame
/// where the range reaches it.
///
/// A range starts at the first record of a frame and ends at one or at the end of the file, so both
/// ends fall out of `n` by arithmetic. The exception is the end of the file, whose frame holds a
/// count the seek table cannot supply; that count is what the decode here is spent on, and it comes
/// back with the range because what a frame holding a count of its own means is the caller's to
/// decide.
///
/// # Errors
///
/// A `from` that is not the first record of a frame or is past the last one, and a `cnt` that ends
/// the range anywhere but at the first record of a frame or the end of the file.
fn frame_range(
    reader: &mut FrameReader,
    finder: &Finder,
    n: u64,
    from: u64,
    cnt: Option<u64>,
) -> anyhow::Result<(u32, u32, Option<u64>)> {
    let last = last_data_frame(reader.table)?;

    if from % n != 0 {
        bail!(
            "refusing to copy from record {from}: a range starts at the first record of a frame, \
             and a frame holds {n} records"
        );
    }
    if from / n > u64::from(last) {
        bail!(
            "refusing to copy from record {from}: the file holds {} frames of {n} records",
            last + 1
        );
    }
    let k = (from / n) as u32;

    // Every frame but the last holds n records, so the seek table places the end by arithmetic.
    // The last one is the exception, and its count is what a decode is spent on where the range
    // reaches it.
    match cnt.and_then(|c| from.checked_add(c)) {
        Some(end) if end % n == 0 && end / n <= u64::from(last) => Ok((k, (end / n) as u32, None)),
        _ => {
            let tail = frame_records(reader, finder, last)?;
            let total = n * u64::from(last) + tail;
            if let Some(c) = cnt
                && from.saturating_add(c) != total
            {
                bail!(
                    "refusing to copy {c} records from record {from}: a range ends at the first \
                     record of a frame or at the end of the file, which holds {total} records"
                );
            }
            Ok((k, last + 1, Some(tail)))
        }
    }
}

/// The last frame that carries data, which is the frame the record a file ends with is in.
///
/// `Encoder::finish` writes a frame carrying nothing after an `end_frame`, so the last frame of a
/// file is not always the last one holding records. No record range reaches the empty ones.
fn last_data_frame(table: &SeekTable) -> anyhow::Result<u32> {
    let mut last = table.num_frames() - 1;
    while last > 0 && table.frame_size_decomp(last)? == 0 {
        last -= 1;
    }
    Ok(last)
}

/// The number of records each frame of `range` holds.
///
/// Every operation here is built on reading frames and counting separators in them, one frame at a
/// time or a range of them. Public because that is the cost they are all made of, and a benchmark
/// is the only thing that holds it to what the documentation claims.
///
/// # Errors
///
/// An empty `separator`, and a range naming a frame the file does not have.
pub fn count_frames(
    f: &File,
    table: &SeekTable,
    separator: &[u8],
    range: std::ops::Range<u32>,
) -> anyhow::Result<Vec<u64>> {
    record::check_separator(separator)?;
    let widest = widest_frame(table, range.clone())?;
    frame_counts(
        &mut FrameReader::with_capacity(f, table, widest)?,
        &Finder::new(separator),
        range,
    )
}

/// [`count_frames`] for a caller that already has the separator search and has checked it.
fn frame_counts(
    reader: &mut FrameReader,
    finder: &Finder,
    range: std::ops::Range<u32>,
) -> anyhow::Result<Vec<u64>> {
    let mut counts = Vec::with_capacity(range.len());
    for i in range {
        counts.push(record::count(reader.frame(i)?, finder) as u64);
    }
    Ok(counts)
}

/// A decoder held open across the frames of one file, with the seek table it was built from and the
/// buffer the frames are read into.
///
/// Building a decoder clones the seek table, so building one per frame rebuilds the whole table for
/// each of them. Holding the decoder makes that once, and holding the buffer makes the allocation
/// once — one reader, one buffer, however many frames go through it. `benches/edit.rs` is what
/// holds this to being worth having.
///
/// A reader is per-thread whether or not it is written that way: the decoder carries the position it
/// last read to, and two of them over one `File` would share the operating system's, so neither the
/// decoder nor the file behind it can be shared. Holding the buffer therefore shares nothing that
/// was not already private to one thread.
struct FrameReader<'a> {
    decoder: zeekstd::Decoder<'a, &'a File>,
    table: &'a SeekTable,
    buf: Vec<u8>,
}

impl<'a> FrameReader<'a> {
    /// A reader whose buffer starts at [`READ_FRAME_BUF_SIZE`] and grows to the frames it is asked
    /// for.
    ///
    /// A caller that knows how large the frames it wants are says so with
    /// [`FrameReader::with_capacity`] instead; the five that use this one know only that they want
    /// a few frames of whatever the file holds.
    fn new(f: &'a File, table: &'a SeekTable) -> anyhow::Result<Self> {
        Self::with_capacity(f, table, READ_FRAME_BUF_SIZE)
    }

    /// [`FrameReader::new`] with the buffer sized up front, for a caller that knows the largest
    /// frame it will read and would rather not grow into it.
    fn with_capacity(f: &'a File, table: &'a SeekTable, capacity: usize) -> anyhow::Result<Self> {
        Ok(Self {
            decoder: DecodeOptions::new(f)
                .seek_table(table.clone())
                .into_decoder()?,
            table,
            buf: Vec::with_capacity(capacity),
        })
    }

    /// Decompresses frame `index`, replacing whatever the buffer held.
    fn frame(&mut self, index: u32) -> anyhow::Result<&[u8]> {
        decompressed_range_into(
            &mut self.decoder,
            self.table.frame_start_decomp(index)?,
            self.table.frame_size_decomp(index)?,
            &mut self.buf,
        )?;
        Ok(&self.buf)
    }

    /// [`FrameReader::frame`] handed over rather than lent, for the one caller that goes on to add
    /// to what it read. The buffer moves out and the reader starts the next frame from nothing,
    /// which costs it an allocation and the caller no copy.
    fn take_frame(&mut self, index: u32) -> anyhow::Result<Vec<u8>> {
        self.frame(index)?;
        Ok(std::mem::take(&mut self.buf))
    }
}

/// The largest frame of `range`, which is what a reader over it has to hold at once.
fn widest_frame(table: &SeekTable, range: std::ops::Range<u32>) -> anyhow::Result<usize> {
    let mut widest = 0;
    for i in range {
        widest = widest.max(table.frame_size_decomp(i)? as usize);
    }
    Ok(widest)
}

/// The number of records frame `index` holds, which costs one frame decoded.
///
/// The seek table cannot answer this: it records sizes, not record counts. Only the last frame
/// needs asking, since every other one holds the count validation established.
fn frame_records(reader: &mut FrameReader, finder: &Finder, index: u32) -> anyhow::Result<u64> {
    Ok(record::count(reader.frame(index)?, finder) as u64)
}

/// Compresses `data` as a single frame, returning its bytes and the sizes its seek table entry
/// needs.
fn encode_frame(data: &[u8], checksum: bool, level: i32) -> anyhow::Result<(Vec<u8>, u32, u32)> {
    let mut out = Vec::new();
    let mut encoder = frame_encoder(&mut out, checksum, level)?;

    encoder.write_all(data)?;
    encoder.end_frame()?;
    encoder.flush()?;

    let table = encoder.into_seek_table();
    if table.num_frames() != 1 {
        bail!(
            "re-encoding {} bytes produced {} frames, not one",
            data.len(),
            table.num_frames()
        );
    }

    Ok((
        out,
        table.frame_size_comp(0)? as u32,
        table.frame_size_decomp(0)? as u32,
    ))
}

/// Copies the first `frames` entries of `table` into `out`.
///
/// Rebuilding is the only way to drop an entry: no zeekstd API removes one from a `SeekTable`.
fn log_frames(out: &mut SeekTable, table: &SeekTable, frames: u32) -> anyhow::Result<()> {
    log_frames_range(out, table, 0..frames)
}

/// [`log_frames`] over a range that need not start at the first frame, for an operation whose
/// result keeps a range of the frames rather than a prefix of them.
fn log_frames_range(
    out: &mut SeekTable,
    table: &SeekTable,
    frames: std::ops::Range<u32>,
) -> anyhow::Result<()> {
    for i in frames {
        out.log_frame(
            table.frame_size_comp(i)? as u32,
            table.frame_size_decomp(i)? as u32,
        )?;
    }
    Ok(())
}

fn write_seek_table(f: &mut File, table: SeekTable) -> anyhow::Result<()> {
    write_seek_table_to(f, table)
}

/// [`write_seek_table`] to any writer, for an operation that writes somewhere other than the file
/// it read.
fn write_seek_table_to(f: &mut impl Write, table: SeekTable) -> anyhow::Result<()> {
    let mut serializer = table.into_serializer();
    let mut buf = vec![0u8; serializer.encoded_len()];
    let n = serializer.write_into(&mut buf);
    Ok(f.write_all(&buf[..n])?)
}
