//! Where records end.
//!
//! A record ends where the finder says it does, and the finders live in [`crate::find`]. Every
//! count, every cut and every boundary test in the crate comes from here, or the same file gets
//! read two ways.
//!
//! `data.ends_with(separator)` is not one of these tests. With `"\n\n"` for a separator,
//! `"a\n\n\n"` holds one record and one byte over, and its last two bytes are a separator all the
//! same. [`ends_whole`] is the test.

use std::cell::{Ref, RefCell};
use std::io::{Read, Seek, SeekFrom, Take, Write};

use crate::seekzstdsep_lib::{READ_BUF_SIZE, READ_FRAME_BUF_SIZE};

/// Refuses a separator that no record could end with.
///
/// An empty needle matches at every position and spans no bytes, so every scan below would find a
/// record of nothing and never move past it.
pub(crate) fn check_separator(separator: &[u8]) -> anyhow::Result<()> {
    if separator.is_empty() {
        anyhow::bail!("separator must not be empty");
    }
    Ok(())
}

/// Where each record in `data` ends, in order.
///
/// The one walk. Everything else here that asks how `data` divides into records asks this.
pub(crate) fn ends<'a>(
    data: &'a [u8],
    find: impl Fn(&[u8]) -> Option<usize> + 'a,
) -> impl Iterator<Item = usize> + 'a {
    let mut at = 0usize;
    std::iter::from_fn(move || {
        at += find(&data[at..])?;
        Some(at)
    })
}

/// How many whole records `data` holds. A fragment after the last one does not count.
pub(crate) fn count(data: &[u8], find: impl Fn(&[u8]) -> Option<usize>) -> usize {
    ends(data, find).count()
}

/// Whether `data` ends with a whole record rather than with a fragment of one. Empty `data` ends
/// with neither.
pub(crate) fn ends_whole(data: &[u8], find: impl Fn(&[u8]) -> Option<usize>) -> bool {
    ends(data, find).last() == Some(data.len())
}

/// The buffer a record scan walks, and the source it is filled from.
///
/// Reading, scanning and cutting are what every caller that turns a stream into frames does, and
/// they are the same three each time. When to cut is what differs, and stays with the caller.
pub(crate) struct Stream<R, F> {
    source: R,
    find: F,
    buf: Vec<u8>,
    read_buf: Vec<u8>,
    /// Where the last record found ends, which is where the next scan starts.
    end: usize,
}

impl<R: Read, F: Fn(&[u8]) -> Option<usize>> Stream<R, F> {
    pub(crate) fn with_capacity(source: R, find: F, capacity: usize) -> Self {
        Self::from_buffer(source, find, Vec::with_capacity(capacity))
    }

    /// Starts from a buffer the caller made, for a caller that has to allocate it before it has a
    /// finder to hand over.
    pub(crate) fn from_buffer(source: R, find: F, buf: Vec<u8>) -> Self {
        Self {
            source,
            find,
            buf,
            read_buf: vec![0u8; READ_BUF_SIZE],
            end: 0,
        }
    }

    /// Reads once into the buffer. `false` once the source is spent.
    #[inline]
    pub(crate) fn fill(&mut self) -> anyhow::Result<bool> {
        let read = self.source.read(&mut self.read_buf)?;
        if read == 0 {
            return Ok(false);
        }
        self.buf.extend_from_slice(&self.read_buf[..read]);
        Ok(true)
    }

    /// Where the next record ends, counted from the start of [`Self::buffered`].
    ///
    /// Nothing is read here: what is buffered is all that is searched, so a boundary lying across
    /// two reads is found on the scan that follows the second.
    #[inline]
    pub(crate) fn next_end(&mut self) -> Option<usize> {
        let found = (self.find)(&self.buf[self.end..])?;
        self.end += found;
        Some(self.end)
    }

    /// Everything read and not yet dropped.
    #[inline]
    pub(crate) fn buffered(&self) -> &[u8] {
        &self.buf
    }

    /// Where the last record found ends, which is where the buffer gets cut.
    #[inline]
    pub(crate) fn last_end(&self) -> usize {
        self.end
    }

    /// How much has been read past the last record end.
    #[inline]
    pub(crate) fn unscanned(&self) -> usize {
        self.buf.len() - self.end
    }

    /// Drops everything up to the last record end.
    pub(crate) fn drop_to_last_end(&mut self) {
        self.buf.drain(..self.end);
        self.end = 0;
    }

    /// Everything buffered, leaving the stream empty.
    pub(crate) fn take_buffered(&mut self) -> Vec<u8> {
        self.end = 0;
        std::mem::take(&mut self.buf)
    }

    /// Drops the first `upto` bytes, moving the last record end back with them.
    pub(crate) fn drop_front(&mut self, upto: usize) {
        self.buf.drain(..upto);
        self.end -= upto;
    }
}

/// Reads records out of a byte source through one reused window, [`READ_FRAME_BUF_SIZE`] to begin
/// with.
///
/// The source writes straight into the window and [`Self::records`] walks it, so nothing is copied
/// between the read and the caller. A run is handed out as offsets and its bytes are taken from
/// [`Self::bytes`], which is what lets the walk be an [`Iterator`]: an item that borrowed the
/// window could not survive the read that follows it.
///
/// The window holds one record: where it is full and no record has ended in it, it doubles. It
/// never shrinks, and a reader reuses it across frames. A record never spans a frame, so the
/// growth is bounded by the frame being read.
///
/// [`Stream`] is the compress side's accumulator, which holds records until a frame is cut. This
/// does not accumulate, which is why it is not that.
pub struct Reader<R> {
    /// Behind a cell because the walk reads it and the consumer takes bytes out of it while that
    /// walk is alive: both hold `&Reader`, and only [`Iterator::next`] borrows the inside mutably.
    window: RefCell<Window<R>>,
}

/// The window a [`Reader`] reads through, and where it is up to.
struct Window<R> {
    source: R,
    /// What the source reads into, [`READ_FRAME_BUF_SIZE`] until a record longer than that grows
    /// it.
    buf: Vec<u8>,
    /// How much of `buf` holds data.
    filled: usize,
    /// How much of that the caller has consumed.
    pos: usize,
    /// Whether the source has returned 0.
    eof: bool,
    /// Records that ended before `pos`, counted from where the window was last pointed.
    walked: u64,
    /// The record that starts at the front of the buffer, when one does. `None` once a slide has
    /// cut a record in two.
    front: Option<u64>,
    /// Whether `pos` sits where a record ended rather than inside such a record.
    on_boundary: bool,
}

/// Records that lie next to each other in the window: where they start, how many bytes they take
/// and how many records that is. [`Reader::bytes`] is the bytes.
#[derive(Clone, Copy, Debug)]
pub struct Run {
    start: usize,
    len: usize,
    pub(crate) count: u64,
}

/// A [`Reader`] over the decompressed bytes in `[start, start + len)`, for a caller that only
/// needs one for the length of a call. [`Reader::seek_to`] is the same move on a reader that
/// outlives it.
pub(crate) fn region<'d, 'z, S: zeekstd::Seekable>(
    decoder: &'d mut zeekstd::Decoder<'z, S>,
    start: u64,
    len: u64,
) -> anyhow::Result<Reader<Take<&'d mut zeekstd::Decoder<'z, S>>>> {
    let mut reader = Reader::new(decoder.take(0));
    reader.seek_to(start, len)?;
    Ok(reader)
}

impl<R: Read> Reader<R> {
    pub(crate) fn new(source: R) -> Self {
        Self {
            window: RefCell::new(Window {
                source,
                buf: vec![0u8; READ_FRAME_BUF_SIZE],
                filled: 0,
                pos: 0,
                eof: false,
                walked: 0,
                front: Some(0),
                on_boundary: true,
            }),
        }
    }

    /// The source, for a caller that reads it another way. What the window holds is left behind,
    /// so pair a move of the source with [`Reader::seek_to`].
    pub(crate) fn source_mut(&mut self) -> &mut R {
        &mut self.window.get_mut().source
    }

    /// The source, leaving the window behind. For a caller that reads the rest another way.
    pub(crate) fn into_source(self) -> R {
        self.window.into_inner().source
    }

    /// The bytes of `run`, in the window the source read them into.
    pub(crate) fn bytes(&self, run: &Run) -> Ref<'_, [u8]> {
        Ref::map(self.window.borrow(), |window| {
            &window.buf[run.start..run.start + run.len]
        })
    }

    /// How many records have been handed out since the window was last pointed at a region.
    pub(crate) fn walked(&self) -> u64 {
        self.window.borrow().walked
    }

    /// Everything read and not handed out — after the source is spent, the trailing fragment.
    pub(crate) fn remainder(&self) -> Ref<'_, [u8]> {
        Ref::map(self.window.borrow(), |window| {
            &window.buf[window.pos..window.filled]
        })
    }

    /// Puts the walk on record `index`, counted from where the window was last pointed, and
    /// returns how many records it still has to pass to reach it.
    ///
    /// A record ahead of the walk needs nothing put back: walking reads on, sliding what is
    /// consumed out of the window. One behind it is still there to walk to for as long as the
    /// buffer holds it. `None` once a slide has dropped it, which is the caller's cue to point the
    /// window at the region again.
    pub(crate) fn walk_from(&mut self, index: u64) -> Option<u64> {
        let window = self.window.get_mut();
        if index >= window.walked {
            return Some(index - window.walked);
        }
        let front = window.front?;
        let skip = index.checked_sub(front)?;
        window.walked = front;
        window.pos = 0;
        window.on_boundary = true;
        Some(skip)
    }

    /// The records of this source, a [`Run`] of the window at a time rather than one record at a
    /// time: consecutive records go out in one write, and [`Self::bytes`] is where an item's bytes
    /// come from.
    pub(crate) fn records<F: Fn(&[u8]) -> Option<usize>>(&self, find: F) -> Records<'_, R, F> {
        Records {
            reader: self,
            find,
            left: None,
            watch: Unwatched,
        }
    }
}

impl<S: Read + Seek> Reader<Take<S>> {
    /// Points the window at the decompressed bytes in `[start, start + len)`, dropping what it
    /// holds of wherever it was pointed before.
    pub(crate) fn seek_to(&mut self, start: u64, len: u64) -> anyhow::Result<()> {
        let window = self.window.get_mut();
        window.source.get_mut().seek(SeekFrom::Start(start))?;
        window.source.set_limit(len);
        window.clear();
        Ok(())
    }
}

impl<R: Read> Window<R> {
    /// Forgets what is buffered, for reading a region after the source moved. The buffer keeps
    /// whatever length the longest record so far grew it to.
    fn clear(&mut self) {
        self.filled = 0;
        self.pos = 0;
        self.eof = false;
        self.walked = 0;
        self.front = Some(0);
        self.on_boundary = true;
    }

    /// How many bytes the records that end in the window take, and how many records that is, up to
    /// `want` of them.
    ///
    /// Compiled once per watcher, which it never reads: one copy shared between the walk a read
    /// watches and the walk it does not is a second caller, and a second caller is what stops it
    /// being inlined into either.
    fn walk<W: Watcher>(
        &self,
        find: &impl Fn(&[u8]) -> Option<usize>,
        want: Option<u64>,
    ) -> (usize, u64) {
        let held = &self.buf[self.pos..self.filled];
        let mut used = 0usize;
        let mut count = 0u64;
        while want.is_none_or(|want| count < want) {
            match find(&held[used..]) {
                Some(end) => {
                    used += end;
                    count += 1;
                }
                None => break,
            }
        }
        (used, count)
    }

    /// Doubles the window where it is full and holds no record end, which is what a record longer
    /// than it looks like.
    ///
    /// A finder that reads a length header cannot resume from the middle of a record, so the
    /// window has to reach the whole of one rather than hand out the piece it holds.
    fn grow_if_full(&mut self) {
        if !self.eof && self.pos == 0 && self.filled == self.buf.len() {
            self.buf.resize(self.buf.len() * 2, 0);
        }
    }

    /// Slides what is not consumed to the front and reads on behind it. `false` once the source is
    /// spent.
    fn refill(&mut self) -> anyhow::Result<bool> {
        if self.eof {
            return Ok(false);
        }
        if self.pos > 0 {
            self.buf.copy_within(self.pos..self.filled, 0);
            self.filled -= self.pos;
            self.pos = 0;
            // What the slide drops is gone for good: the source cannot be read backwards. The
            // record now at the front is the one the walk is on, unless the slide cut one in two.
            self.front = self.on_boundary.then_some(self.walked);
        }
        let read = self.source.read(&mut self.buf[self.filled..])?;
        self.filled += read;
        if read == 0 {
            self.eof = true;
            return Ok(false);
        }
        Ok(true)
    }
}

/// The runs of a [`Reader`], each the records that end in one window.
///
/// Records in a run are next to each other in the window, so a run goes out in one write of the
/// decoder's own bytes.
pub(crate) struct Records<'a, R, F, W = Unwatched> {
    reader: &'a Reader<R>,
    find: F,
    /// Records still wanted, or `None` for all of them.
    left: Option<u64>,
    watch: W,
}

/// What a walk tells as it hands out runs.
///
/// The walk is compiled once per watcher, so what a watcher asks of it leaves no trace in a walk
/// nobody watches.
pub trait Watcher {
    /// Whether anything is listening.
    const WATCHES: bool;

    /// Called with the run just handed out, once it is counted in the window.
    fn saw<R: Read>(
        &mut self,
        reader: &Reader<R>,
        run: &Run,
        find: &impl Fn(&[u8]) -> Option<usize>,
    ) -> anyhow::Result<()>;

    /// Called where the walk stops with the source spent and records still wanted.
    ///
    /// An offset the walk never reached is not reported by [`Self::saw`], and a walk that runs out
    /// inside a region has reached nothing past where it stopped. What is left there is a fragment
    /// of a record rather than one, so the last stretch the walk covered is told about here or not
    /// at all.
    fn spent<R: Read>(&mut self, reader: &Reader<R>) -> anyhow::Result<()>;
}

/// The watcher of a walk nobody watches.
pub struct Unwatched;

impl Watcher for Unwatched {
    const WATCHES: bool = false;

    #[inline]
    fn saw<R: Read>(
        &mut self,
        _reader: &Reader<R>,
        _run: &Run,
        _find: &impl Fn(&[u8]) -> Option<usize>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    #[inline]
    fn spent<R: Read>(&mut self, _reader: &Reader<R>) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Byte offsets a walk is watched against, counted from where the window was pointed and in order,
/// and what to call with the offset's place among them, the records that ended before it, and
/// whether one ended on it.
pub struct Watch<'a> {
    at: Vec<u64>,
    next: usize,
    /// Bytes the runs reported so far came to. Kept here rather than by the window, which would
    /// then count them for every walk, watched or not.
    seen: u64,
    passed: Box<dyn FnMut(usize, u64, bool) -> anyhow::Result<()> + 'a>,
}

impl<'a> Watch<'a> {
    pub(crate) fn new(
        at: Vec<u64>,
        passed: impl FnMut(usize, u64, bool) -> anyhow::Result<()> + 'a,
    ) -> Self {
        Self {
            at,
            next: 0,
            seen: 0,
            passed: Box::new(passed),
        }
    }

    /// Watched against no offset at all, for a read that verifies elsewhere than in its runs.
    ///
    /// Not [`Unwatched`]: that one is the walk of a read that verifies nothing, and a walk shared
    /// with it is a walk that stops being inlined into it. The closure captures nothing, so the
    /// box is of no size and allocates nothing.
    pub(crate) fn nothing() -> Self {
        Self::new(Vec::new(), |_, _, _| Ok(()))
    }
}

impl Watcher for Watch<'_> {
    const WATCHES: bool = true;

    /// Reports every offset the run just handed out reached.
    ///
    /// A run holds the records that lie next to each other in the window, so one straddles an
    /// offset: the records before it are counted by scanning that much of the run again.
    // Forced for the same reason as `Records::next`: left to LLVM it is not inlined into the walk.
    #[inline(always)]
    fn saw<R: Read>(
        &mut self,
        reader: &Reader<R>,
        run: &Run,
        find: &impl Fn(&[u8]) -> Option<usize>,
    ) -> anyhow::Result<()> {
        let began = self.seen;
        let ended = began + run.len as u64;
        self.seen = ended;
        if self.next >= self.at.len() || self.at[self.next] > ended {
            return Ok(());
        }
        let bytes = reader.bytes(run);
        let walked = reader.walked() - run.count;
        while self.next < self.at.len() && self.at[self.next] <= ended {
            let upto = (self.at[self.next] - began) as usize;
            let before = walked + count(&bytes[..upto], find) as u64;
            (self.passed)(self.next, before, ends_whole(&bytes[..upto], find))?;
            self.next += 1;
        }
        Ok(())
    }

    /// Reports the stretch the walk stopped in as if the offset it never reached were where it
    /// stopped: everything it walked belongs to that stretch, and what is left over is not a
    /// record.
    fn spent<R: Read>(&mut self, reader: &Reader<R>) -> anyhow::Result<()> {
        if self.next >= self.at.len() {
            return Ok(());
        }
        (self.passed)(self.next, reader.walked(), reader.remainder().is_empty())?;
        self.next += 1;
        Ok(())
    }
}

impl<R: Read, F: Fn(&[u8]) -> Option<usize>, W: Watcher> Iterator for Records<'_, R, F, W> {
    type Item = anyhow::Result<Run>;

    /// The next run, refilling the window until one turns up. `None` once the source is spent or
    /// the count asked for is reached. A watcher is told about the run before it goes out, and
    /// what it refuses comes back as the item's error.
    // The reader's source is a type parameter, so every walk is instantiated in the caller's crate,
    // where this has several callers and LLVM stops inlining it into `next_owned`. Forced: a call
    // per record is what `into_records` pays otherwise. See the commit that added it for the numbers.
    #[inline(always)]
    fn next(&mut self) -> Option<Self::Item> {
        if self.left == Some(0) {
            return None;
        }
        let mut window = self.reader.window.borrow_mut();
        loop {
            let (used, count) = window.walk::<W>(&self.find, self.left);
            if count > 0 {
                let run = Run {
                    start: window.pos,
                    len: used,
                    count,
                };
                window.pos += used;
                window.walked += count;
                window.on_boundary = true;
                if let Some(left) = self.left.as_mut() {
                    *left -= count;
                }
                if W::WATCHES {
                    drop(window);
                    if let Err(e) = self.watch.saw(self.reader, &run, &self.find) {
                        return Some(Err(e));
                    }
                }
                return Some(Ok(run));
            }
            window.grow_if_full();
            match window.refill() {
                Ok(true) => {}
                Ok(false) => return None,
                Err(e) => return Some(Err(e)),
            }
        }
    }
}

impl<'a, R: Read, F: Fn(&[u8]) -> Option<usize>> Records<'a, R, F> {
    /// Told about every run this hands out.
    pub(crate) fn watching<W: Watcher>(self, watch: W) -> Records<'a, R, F, W> {
        Records {
            reader: self.reader,
            find: self.find,
            left: self.left,
            watch,
        }
    }
}

impl<R: Read, F: Fn(&[u8]) -> Option<usize>, W: Watcher> Records<'_, R, F, W> {
    /// At most `n` records in all. [`Iterator::take`] counts runs, which is not the same question.
    pub(crate) fn take_records(mut self, n: u64) -> Self {
        self.left = Some(n);
        self
    }

    /// Past the first `n` records, and how many there were to pass. Fewer than `n` means the
    /// source ended first, which is not an error to every caller — but a watcher is told where the
    /// walk stopped, since that offset is one it will never reach.
    ///
    /// # Errors
    ///
    /// A read failing, or a watcher refusing where the walk stopped.
    pub(crate) fn skip_up_to(mut self, n: u64) -> anyhow::Result<(Self, u64)> {
        if n == 0 {
            return Ok((self, 0));
        }
        let wanted = self.left;
        self.left = Some(n);
        let skipped = self
            .by_ref()
            .try_fold(0u64, |skipped, run| run.map(|run| skipped + run.count))?;
        if W::WATCHES && skipped < n {
            self.watch.spent(self.reader)?;
        }
        self.left = wanted;
        Ok((self, skipped))
    }

    /// Past the first `n` records.
    ///
    /// # Errors
    ///
    /// The source ending before `n` of them, or a read failing. A watcher sees the walk run out
    /// first, so what it refuses there is what a caller gets rather than the shortfall.
    pub(crate) fn skip_records(self, n: u64) -> anyhow::Result<Self> {
        let (records, skipped) = self.skip_up_to(n)?;
        if skipped < n {
            return Err(anyhow::anyhow!("No separator found in frame"));
        }
        Ok(records)
    }

    /// How many records there are.
    ///
    /// # Errors
    ///
    /// A read failing.
    pub(crate) fn count_records(mut self) -> anyhow::Result<usize> {
        let count = self.try_fold(0u64, |count, run| run.map(|run| count + run.count))?;
        Ok(count as usize)
    }

    /// Writes them to `dst`, one write per run.
    ///
    /// When the source ends before the count asked for, what followed the last record goes to
    /// `dst` as well: that is what a whole-span read returned. A watcher is told where the walk
    /// stopped first, so a refusal there is raised before that fragment is written.
    ///
    /// # Errors
    ///
    /// A read failing, `dst` refusing bytes, or a watcher refusing what the walk passed.
    pub(crate) fn write_to(mut self, dst: &mut impl Write) -> anyhow::Result<()> {
        let reader = self.reader;
        self.by_ref().try_for_each(|run| -> anyhow::Result<()> {
            dst.write_all(&reader.bytes(&run?))?;
            Ok(())
        })?;
        if self.left.is_some_and(|left| left > 0) {
            if W::WATCHES {
                self.watch.spent(reader)?;
            }
            dst.write_all(&reader.remainder())?;
        }
        Ok(())
    }

    /// The first one, owned, for a caller that outlives the window.
    ///
    /// # Errors
    ///
    /// A read failing.
    pub(crate) fn next_owned(mut self) -> anyhow::Result<Option<Vec<u8>>> {
        self.left = Some(1);
        let record = match self.next().transpose()? {
            Some(run) => Some(self.reader.bytes(&run).to_vec()),
            None => None,
        };
        // A trailing fragment is not a record: only a run that ended one leaves `left` short.
        Ok(match self.left {
            Some(0) => record,
            _ => None,
        })
    }
}
