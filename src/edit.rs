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
use crate::seekzstdsep_lib::{READ_BUF_SIZE, decompressed_range, frame_encoder};
use anyhow::bail;
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
/// The cut lands immediately after a separator, so the result always ends with one and a trailing
/// fragment is dropped. `f` is rewritten from the frame the cut falls in; earlier bytes are left
/// byte for byte as they were.
///
/// # Errors
///
/// Refuses a `record_len` of 0 or one past the records `f` holds, along with the refusals every
/// operation in [this module](crate::edit) shares.
pub fn truncate(f: &mut File, record_len: u64, separator: &[u8]) -> anyhow::Result<()> {
    let (finder, table) = open_target(f, separator)?;
    let frames = table.num_frames();

    let n = validate_separator(f, &table, &finder)?;
    let last = frame_records(f, &table, &finder, frames - 1)?;
    let before_last = n * u64::from(frames - 1);
    let total = before_last + last;

    if record_len == 0 {
        bail!("refusing to truncate to 0 records: a file of no frames cannot be read back");
    }
    if record_len > total {
        bail!("refusing to truncate to {record_len} records: the file holds {total}");
    }

    // A cut past every frame but the last one falls inside the last, whatever it holds. Dividing
    // there would place it by a record count the last frame does not have to obey.
    let (k, rem) = if record_len > before_last {
        (u64::from(frames - 1), record_len - before_last)
    } else {
        (record_len / n, record_len % n)
    };

    let tail = if rem == 0 {
        None
    } else {
        let frame = decode_frame(f, &table, k as u32)?;
        let Some(end) = record::nth_end(&frame, &finder, separator.len(), rem as usize - 1) else {
            bail!("frame {k} holds fewer than the {rem} records the truncation cuts it at");
        };
        Some(encode_frame(
            &frame[..end],
            frame_has_checksum(f, &table, k as u32)?,
        )?)
    };

    let cut = if k == 0 {
        0
    } else {
        table.frame_end_comp(k as u32 - 1)?
    };
    cut_at(f, cut)?;

    let mut out = SeekTable::new();
    log_frames(&mut out, &table, k as u32)?;
    if let Some((bytes, c_size, d_size)) = tail {
        f.write_all(&bytes)?;
        out.log_frame(c_size, d_size)?;
    }

    write_seek_table(f, out)
}

/// Appends the records `data` holds to `f`, keeping every frame but the last at the records per
/// frame the file was built with.
///
/// The last data frame generally holds fewer records than the rest, so appending after it would
/// leave a short frame in the interior. It is decoded, joined with `data` and cut again instead;
/// nothing before it is read or written. An empty `data` rewrites nothing.
///
/// # Errors
///
/// Refuses a file that does not end with a whole record unless `on_missing` is
/// [`OnMissingSeparator::Insert`], which refuses in turn where writing one separator leaves
/// another fragment. Along with the refusals every operation in [this module](crate::edit) shares.
pub fn append(
    f: &mut File,
    mut data: impl Read,
    separator: &[u8],
    on_missing: OnMissingSeparator,
) -> anyhow::Result<()> {
    let (finder, table) = open_target(f, separator)?;

    // Before the subtraction below: a seek table can hold no entries at all, and validation is
    // what refuses that.
    let n = validate_separator(f, &table, &finder)? as usize;
    // Replace the frame the last record is in; the set_len below drops the empty frames with it.
    let last = last_data_frame(&table)?;

    // Before anything is decoded, so that appending nothing costs one read and rewrites nothing.
    let mut head = vec![0u8; READ_BUF_SIZE];
    let read = data.read(&mut head)?;
    if read == 0 {
        return Ok(());
    }
    head.truncate(read);

    let checksum = frame_has_checksum(f, &table, last)?;
    let mut tail = decode_frame(f, &table, last)?;
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
    let (first_bytes, first_comp, first_decomp) = encode_frame(&first, checksum)?;

    let cut = table.frame_start_comp(last)?;
    cut_at(f, cut)?;
    f.write_all(&first_bytes)?;

    let mut out = SeekTable::new();
    log_frames(&mut out, &table, last)?;
    out.log_frame(first_comp, first_decomp)?;

    if whole {
        let mut encoder = frame_encoder(&mut *f, checksum)?;
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
    let n = records_per_frame(input, &table, &finder, check)?;
    let last = last_data_frame(&table)?;

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
    let j = match cnt {
        Some(0) => bail!("refusing to copy 0 records: a file of no frames cannot be read back"),
        Some(c) if (from + c) % n == 0 && (from + c) / n <= u64::from(last) => {
            ((from + c) / n) as u32
        }
        c => {
            let tail = frame_records(input, &table, &finder, last)?;
            let total = n * u64::from(last) + tail;
            if let Some(c) = c
                && from.saturating_add(c) != total
            {
                bail!(
                    "refusing to copy {c} records from record {from}: a range ends at the first \
                     record of a frame or at the end of the file, which holds {total} records"
                );
            }
            if tail != n && align == Alignment::Required {
                bail!(
                    "refusing to copy frame {last}, which holds {tail} records rather than {n}: \
                     the result would not hold the same count in every frame"
                );
            }
            last + 1
        }
    };

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
fn validate_separator(f: &File, table: &SeekTable, finder: &Finder) -> anyhow::Result<u64> {
    let frames = table.num_frames();
    if frames < 3 {
        bail!(
            "refusing to validate the separator against {frames} frames: the last frame is \
             legitimately short, so fewer than three cannot be compared"
        );
    }

    let first = record::count(&decode_frame(f, table, 0)?, finder);
    if first == 0 {
        bail!("the separator does not occur in frame 0");
    }
    let last_full = frames - 2;
    let second = record::count(&decode_frame(f, table, last_full)?, finder);
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
/// # Errors
///
/// A frame that does not start with the zstd magic number.
pub fn frame_has_checksum(mut f: &File, table: &SeekTable, index: u32) -> anyhow::Result<bool> {
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
    f: &File,
    table: &SeekTable,
    finder: &Finder,
    check: SeparatorCheck,
) -> anyhow::Result<u64> {
    let last = last_data_frame(table)?;
    if last == 0 {
        bail!(
            "refusing to read the separator off a file of one data frame: that frame is the one \
             allowed to end anywhere, so a separator that does not end it proves nothing"
        );
    }

    let first = decode_frame(f, table, 0)?;
    // Before the check below, which would refuse a separator that occurs nowhere for the less
    // specific of the two reasons.
    let n = record::count(&first, finder) as u64;
    if n == 0 {
        bail!("the separator does not occur in frame 0");
    }
    if !record::ends_whole(&first, finder, finder.needle().len()) {
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
        let second = frame_records(f, table, finder, full)?;
        if n != second {
            bail!(
                "frame 0 holds {n} records and frame {full} holds {second}: either the separator \
                 is wrong or the file does not hold a uniform count"
            );
        }
    }

    Ok(n)
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

/// The number of records frame `index` holds, which costs one frame decoded.
///
/// The seek table cannot answer this: it records sizes, not record counts. Only the last frame
/// needs asking, since every other one holds the count validation established.
fn frame_records(f: &File, table: &SeekTable, finder: &Finder, index: u32) -> anyhow::Result<u64> {
    Ok(record::count(&decode_frame(f, table, index)?, finder) as u64)
}

/// Decompresses one frame.
fn decode_frame(f: &File, table: &SeekTable, index: u32) -> anyhow::Result<Vec<u8>> {
    let mut decoder = DecodeOptions::new(f)
        .seek_table(table.clone())
        .into_decoder()?;

    decompressed_range(
        &mut decoder,
        table.frame_start_decomp(index)?,
        table.frame_size_decomp(index)?,
    )
}

/// Compresses `data` as a single frame, returning its bytes and the sizes its seek table entry
/// needs.
fn encode_frame(data: &[u8], checksum: bool) -> anyhow::Result<(Vec<u8>, u32, u32)> {
    let mut out = Vec::new();
    let mut encoder = frame_encoder(&mut out, checksum)?;

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
