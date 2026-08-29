//! Reading records back out of a compressed file.
//!
//! Opening a reader costs the open, the seek table and frame 0's separator count. A reader
//! opened per range pays for all three every time. [`RecordReader`] is those three held open, so a
//! caller that reads one record at a time — the nushell plugin's cell paths, say — pays once.
//!
//! The reader inherits the same-count-per-frame invariant that it locates records by: a
//! file compressed without it is read at the wrong offsets and reports no error. See
//! `docs/format.md`.
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
};

use memchr::memmem::Finder;
use zeekstd::Decoder;

use crate::record;
use crate::seekzstdsep_lib::{
    decompressed_range, records_between_by_separator_in_frame, seek_table_decomp_frames,
};

/// The one frame kept decompressed, and which frame it is.
struct CachedFrame {
    index: usize,
    data: Vec<u8>,
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
/// Holds the decoder, the frame list and frame 0's separator count, plus one decompressed frame:
/// consecutive indices in the same frame decompress it once.
pub struct RecordReader {
    path: PathBuf,
    decoder: Decoder<'static, File>,
    frames: Vec<(u64, u64)>,
    separator: Vec<u8>,
    finder: Finder<'static>,
    /// Separators in frame 0, taken as the record count of every frame.
    sep_cnt: usize,
    cache: Option<CachedFrame>,
}

impl RecordReader {
    /// Opens `path` and reads its seek table and frame 0's separator count.
    ///
    /// # Errors
    ///
    /// An empty `separator`, the file not opening, a seek table with no frames in it, or frame 0
    /// not decompressing.
    pub fn open(path: PathBuf, separator: &[u8]) -> anyhow::Result<Self> {
        let file = File::open(&path)?;
        Self::from_file(path, file, separator)
    }

    /// [`Self::open`] on an already-open file. `path` is carried for error messages only.
    pub fn from_file(path: PathBuf, file: File, separator: &[u8]) -> anyhow::Result<Self> {
        record::check_separator(separator)?;
        let decoder =
            Decoder::new(file).map_err(|e| anyhow::anyhow!("Failed to create decoder: {}", e))?;
        let frames = seek_table_decomp_frames(&decoder)
            .ok_or_else(|| anyhow::anyhow!("no frames in {}", path.display()))?;
        let mut reader = Self {
            path,
            decoder,
            frames,
            separator: separator.to_vec(),
            finder: Finder::new(separator).into_owned(),
            sep_cnt: 0,
            cache: None,
        };
        reader.decompress_frame(0)?;
        reader.sep_cnt = record::count(reader.cached_frame(), &reader.finder);
        Ok(reader)
    }

    /// The file this reads from.
    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// The separator records are counted by.
    pub fn separator(&self) -> &[u8] {
        &self.separator
    }

    /// How many frames the file holds.
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// Records in frame 0, which the invariant makes the record count of every frame but the last.
    pub fn records_per_frame(&self) -> usize {
        self.sep_cnt
    }

    /// How many whole records the file holds. Decompresses the last frame to count it, since the
    /// invariant says nothing about how full it is.
    // FIXME: counts records that [`Self::record`] cannot reach when a frame holds more than frame 0
    // does, which this crate's compressor never writes. See `docs/bugs.md`.
    pub fn total_records(&mut self) -> anyhow::Result<usize> {
        let last = self.frames.len() - 1;
        self.decompress_frame(last)?;
        Ok(self.sep_cnt * last + record::count(self.cached_frame(), &self.finder))
    }

    /// Record `index`, or `None` when the file holds no such whole record.
    ///
    /// The returned bytes carry the separator, as [`Self::records`] does. A trailing fragment with
    /// no separator after it is not a record and is not returned.
    pub fn record(&mut self, index: usize) -> anyhow::Result<Option<Vec<u8>>> {
        if self.sep_cnt == 0 {
            return Ok(None);
        }
        let frame_index = index / self.sep_cnt;
        if frame_index >= self.frames.len() {
            return Ok(None);
        }
        let index_in_frame = index % self.sep_cnt;
        let separator_len = self.separator.len();
        self.decompress_frame(frame_index)?;
        let data = self.cached_frame();

        let start = match record::nth_start(data, &self.finder, separator_len, index_in_frame) {
            Some(pos) => pos,
            None => return Ok(None),
        };
        match record::first_end(&data[start..], &self.finder, separator_len) {
            Some(len) => Ok(Some(data[start..start + len].to_vec())),
            None => Ok(None),
        }
    }

    /// What a read of `cnt` records from `from` has to ask
    /// [`records_between_by_separator_in_frame`] for.
    ///
    /// # Errors
    ///
    /// `from` being past the last frame.
    fn records_request(&self, from: usize, cnt: usize) -> anyhow::Result<RecordsRequest> {
        let total_sep_cnt = self.sep_cnt * self.frames.len();
        let frame_idx = self.frames.len() * from / total_sep_cnt;
        if frame_idx >= self.frames.len() {
            return Err(anyhow::anyhow!(
                "record {from} is past the end of {}",
                self.path.display()
            ));
        }
        let idx_in_frame = from % self.sep_cnt;
        let start = self.frames[frame_idx].0;

        let end_frame_idx =
            (self.frames.len() * (from + cnt + 1) / total_sep_cnt).min(self.frames.len() - 1);
        let len = self.frames[end_frame_idx].0 + self.frames[end_frame_idx].1 - start;
        Ok(RecordsRequest {
            start,
            len,
            skip: idx_in_frame as u64,
        })
    }

    /// `cnt` records from `from`, or fewer when the file holds fewer. This is what
    /// the CLI `cat` subcommand prints.
    ///
    /// # Errors
    ///
    /// `from` being past the last frame, or a frame not decompressing.
    pub fn records(&mut self, from: usize, cnt: usize) -> anyhow::Result<Vec<u8>> {
        let req = self.records_request(from, cnt)?;
        records_between_by_separator_in_frame(
            &mut self.decoder,
            req.start,
            req.len,
            req.skip,
            cnt as u64,
            &self.finder,
            &self.separator,
        )
    }

    /// Every whole record in the file, in order, decompressing one frame at a time.
    ///
    /// Scans rather than divides, so unlike [`Self::record`] it does not rest on the
    /// same-count-per-frame invariant. What follows the last separator of a frame is dropped: the
    /// compressor cuts frames at separator boundaries, so only the end of the file can hold one.
    pub fn into_records(self) -> RecordIter {
        RecordIter {
            reader: self,
            frame: 0,
            offset: 0,
        }
    }

    /// The whole file decompressed, from the start, as a byte stream.
    ///
    /// The decoder this was reading frames through, rewound — no second open, no second seek
    /// table.
    pub fn into_bytes(mut self) -> anyhow::Result<impl Read + Send + 'static> {
        self.decoder.seek(SeekFrom::Start(0))?;
        Ok(self.decoder)
    }

    /// Puts frame `index` in the cache, decompressing it unless it is the one already there.
    fn decompress_frame(&mut self, index: usize) -> anyhow::Result<()> {
        if self.cache.as_ref().map(|c| c.index) != Some(index) {
            let (start, len) = self.frames[index];
            let data = decompressed_range(&mut self.decoder, start, len)?;
            self.cache = Some(CachedFrame { index, data });
        }
        Ok(())
    }

    /// The frame [`Self::decompress_frame`] last put in the cache. Panics if called before it.
    fn cached_frame(&self) -> &[u8] {
        &self.cache.as_ref().expect("no frame decompressed yet").data
    }
}

/// Every whole record of a [`RecordReader`], in order. Made by [`RecordReader::into_records`].
pub struct RecordIter {
    reader: RecordReader,
    /// The frame being handed out, past the last one once the iterator is spent.
    frame: usize,
    /// How far into that frame the last record ended.
    offset: usize,
}

impl Iterator for RecordIter {
    type Item = anyhow::Result<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.frame >= self.reader.frames.len() {
                return None;
            }
            if let Err(e) = self.reader.decompress_frame(self.frame) {
                self.frame = self.reader.frames.len();
                return Some(Err(e));
            }
            let separator_len = self.reader.separator.len();
            let data = self.reader.cached_frame();
            match record::first_end(&data[self.offset..], &self.reader.finder, separator_len) {
                Some(len) => {
                    let start = self.offset;
                    self.offset += len;
                    return Some(Ok(data[start..start + len].to_vec()));
                }
                None => {
                    self.frame += 1;
                    self.offset = 0;
                }
            }
        }
    }
}
