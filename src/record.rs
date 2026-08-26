//! Where records end.
//!
//! A record ends one byte past its separator, and separators are non-overlapping matches: the next
//! one is looked for from the end of the last, never inside it. Every count, every cut and every
//! boundary test in the crate comes from here, or the same file gets read two ways.
//!
//! `data.ends_with(separator)` is not one of these tests. With `"\n\n"` for a separator,
//! `"a\n\n\n"` holds one record and one byte over, and its last two bytes are a separator all the
//! same. [`ends_whole`] is the test.

use std::io::Read;

use memchr::memmem::Finder;

use crate::seekzstdsep_lib::READ_BUF_SIZE;

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
pub(crate) fn ends<'a, 'n>(
    data: &'a [u8],
    finder: &'a Finder<'n>,
    separator_len: usize,
) -> impl Iterator<Item = usize> + 'a {
    finder.find_iter(data).map(move |pos| pos + separator_len)
}

/// Where the first record in `data` ends, if it holds one.
///
/// `Finder::find` rather than the first of [`ends`]: `find_iter` hands the iterator a `Finder` of
/// its own, so taking one match through it copies the whole searcher. This is called once per
/// record.
#[inline]
pub(crate) fn first_end(data: &[u8], finder: &Finder, separator_len: usize) -> Option<usize> {
    finder.find(data).map(|pos| pos + separator_len)
}

/// Where the record at `index` ends, if `data` holds that many.
pub(crate) fn nth_end(
    data: &[u8],
    finder: &Finder,
    separator_len: usize,
    index: usize,
) -> Option<usize> {
    ends(data, finder, separator_len).nth(index)
}

/// Where the record at `index` starts, if `data` holds that many before it.
///
/// A record starts where the one before it ended, and record 0 starts at the top. Both callers
/// that address a record by its number in a frame need exactly that, and disagree only on what a
/// `None` means.
pub(crate) fn nth_start(
    data: &[u8],
    finder: &Finder,
    separator_len: usize,
    index: usize,
) -> Option<usize> {
    match index.checked_sub(1) {
        None => Some(0),
        Some(before) => nth_end(data, finder, separator_len, before),
    }
}

/// How many whole records `data` holds. A fragment after the last one does not count.
pub(crate) fn count(data: &[u8], finder: &Finder) -> usize {
    finder.find_iter(data).count()
}

/// Whether `data` ends with a whole record rather than with a fragment of one.
pub(crate) fn ends_whole(data: &[u8], finder: &Finder, separator_len: usize) -> bool {
    ends(data, finder, separator_len).last() == Some(data.len())
}

/// The buffer a separator scan walks, and the source it is filled from.
///
/// Reading, scanning and cutting are what every caller that turns a stream into frames does, and
/// they are the same three each time. When to cut is what differs, and stays with the caller.
pub(crate) struct Stream<'a, R> {
    source: R,
    finder: &'a Finder<'a>,
    separator_len: usize,
    buf: Vec<u8>,
    read_buf: Vec<u8>,
    /// Where the last record found ends, which is where the next scan starts.
    end: usize,
}

impl<'a, R: Read> Stream<'a, R> {
    pub(crate) fn with_capacity(
        source: R,
        finder: &'a Finder<'a>,
        separator_len: usize,
        capacity: usize,
    ) -> Self {
        Self::from_buffer(source, finder, separator_len, Vec::with_capacity(capacity))
    }

    /// Starts from a buffer the caller made, for a caller that has to allocate it before it has a
    /// finder to hand over.
    pub(crate) fn from_buffer(
        source: R,
        finder: &'a Finder<'a>,
        separator_len: usize,
        buf: Vec<u8>,
    ) -> Self {
        Self {
            source,
            finder,
            separator_len,
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
    /// Nothing is read here: what is buffered is all that is searched, so a separator lying across
    /// two reads is found on the scan that follows the second.
    #[inline]
    pub(crate) fn next_end(&mut self) -> Option<usize> {
        let found = first_end(&self.buf[self.end..], self.finder, self.separator_len)?;
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
