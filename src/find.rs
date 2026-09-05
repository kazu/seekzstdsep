//! The record finders: where a record ends is a function the caller supplies.
//!
//! A finder answers one question — how long is the record that starts at `data[0]` — and nothing
//! outside this module knows a record format.
//!
//! ```text
//! impl Fn(&[u8]) -> Option<usize>
//! ```
//!
//! - `data` always starts on a record boundary.
//! - `Some(0)` is never returned.
//! - A length longer than `data.len()` is never returned.
//! - `None` means "not yet": the caller reads more and asks again. Where nothing more can be read,
//!   what is left is a fragment, and a fragment is not a record.
//! - A finder is a pure function of `data`. Nothing is carried between calls.
//!
//! A finder that needs configuring is a function returning the finder; one that needs none is the
//! finder.
use anyhow::{Context, bail};
use memchr::memmem::Finder;

/// A finder held behind a pointer, for a caller whose type cannot carry one of its own.
///
/// Costs one indirect call per record, which is why the crate's own paths take the finder by value
/// wherever the type can be named.
pub type BoxFinder = Box<dyn Fn(&[u8]) -> Option<usize> + Send + Sync>;

/// Records that end with `finder`'s needle, which is the boundary this crate started with.
pub fn by_separator<'a>(finder: &'a Finder<'a>) -> impl Fn(&[u8]) -> Option<usize> + 'a {
    let separator_len = finder.needle().len();
    move |data| finder.find(data).map(|pos| pos + separator_len)
}

/// Records of a `u32` little-endian length not counting itself, then that many bytes, which is
/// what FlatBuffers' `FinishSizePrefixed` writes.
pub fn by_le32_prefix(data: &[u8]) -> Option<usize> {
    let n = u32::from_le_bytes(data.get(..4)?.try_into().ok()?) as usize;
    (data.len() >= 4 + n).then_some(4 + n)
}

/// Records of a constant length.
///
/// A `len` of 0 ends no record: a record of no bytes would leave every walk standing still, so the
/// finder reports that nothing ever ends rather than returning `Some(0)`.
pub fn by_fixed(len: usize) -> impl Fn(&[u8]) -> Option<usize> {
    move |data| (len > 0 && data.len() >= len).then_some(len)
}

/// Records of one MessagePack value, whose length comes from walking its type bytes.
///
/// A byte that no MessagePack value starts with ends nothing: the finder reports "not yet" as it
/// does for a value that is only partly here, so data that is not MessagePack reads as one endless
/// fragment rather than as a record of the wrong length.
pub fn by_msgpack(data: &[u8]) -> Option<usize> {
    // The values a container still owes, rather than recursion: nesting is the input's to choose
    // and the stack is not.
    let mut at = 0u64;
    let mut pending = 1u64;
    while pending > 0 {
        pending -= 1;
        let (len, children) = msgpack_head(data.get(usize::try_from(at).ok()?..)?)?;
        at = at.checked_add(len)?;
        if at > data.len() as u64 {
            return None;
        }
        pending = pending.checked_add(children)?;
    }
    usize::try_from(at).ok()
}

/// The bytes one MessagePack value takes before its children, and how many children follow it.
fn msgpack_head(rest: &[u8]) -> Option<(u64, u64)> {
    let byte = *rest.first()?;
    Some(match byte {
        // positive fixint, negative fixint, nil, false, true
        0x00..=0x7f | 0xc0 | 0xc2 | 0xc3 | 0xe0..=0xff => (1, 0),
        // fixmap, fixarray, fixstr
        0x80..=0x8f => (1, 2 * u64::from(byte & 0x0f)),
        0x90..=0x9f => (1, u64::from(byte & 0x0f)),
        0xa0..=0xbf => (1 + u64::from(byte & 0x1f), 0),
        // never used
        0xc1 => return None,
        // bin 8/16/32
        0xc4 => (2 + msgpack_len(rest, 1)?, 0),
        0xc5 => (3 + msgpack_len(rest, 2)?, 0),
        0xc6 => (5 + msgpack_len(rest, 4)?, 0),
        // ext 8/16/32, which carry a type byte after the length
        0xc7 => (3 + msgpack_len(rest, 1)?, 0),
        0xc8 => (4 + msgpack_len(rest, 2)?, 0),
        0xc9 => (6 + msgpack_len(rest, 4)?, 0),
        // float 32/64
        0xca => (5, 0),
        0xcb => (9, 0),
        // uint and int of 8, 16, 32 and 64 bits
        0xcc | 0xd0 => (2, 0),
        0xcd | 0xd1 => (3, 0),
        0xce | 0xd2 => (5, 0),
        0xcf | 0xd3 => (9, 0),
        // fixext 1/2/4/8/16, each a type byte and its payload
        0xd4 => (3, 0),
        0xd5 => (4, 0),
        0xd6 => (6, 0),
        0xd7 => (10, 0),
        0xd8 => (18, 0),
        // str 8/16/32
        0xd9 => (2 + msgpack_len(rest, 1)?, 0),
        0xda => (3 + msgpack_len(rest, 2)?, 0),
        0xdb => (5 + msgpack_len(rest, 4)?, 0),
        // array 16/32, map 16/32
        0xdc => (3, msgpack_len(rest, 2)?),
        0xdd => (5, msgpack_len(rest, 4)?),
        0xde => (3, 2 * msgpack_len(rest, 2)?),
        0xdf => (5, 2 * msgpack_len(rest, 4)?),
    })
}

/// The `width`-byte big-endian number that follows a MessagePack type byte.
fn msgpack_len(rest: &[u8], width: usize) -> Option<u64> {
    let bytes = rest.get(1..1 + width)?;
    Some(
        bytes
            .iter()
            .fold(0u64, |n, &byte| (n << 8) | u64::from(byte)),
    )
}

/// What a `--finder` names, once its `--finder-arg` has been read.
///
/// A separator is kept as the bytes rather than as a finder because the paths that take one do
/// more with it than find a boundary: they write it at a join, name it in a refusal, and hand it
/// back from [`crate::RecordReader::separator`].
pub enum Boundary {
    /// `sep`: the bytes a record ends with.
    Separator(Vec<u8>),
    /// Any other format, as the finder it configures.
    Finder(BoxFinder),
}

impl std::fmt::Debug for Boundary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Separator(separator) => f.debug_tuple("Separator").field(separator).finish(),
            Self::Finder(_) => f.write_str("Finder(..)"),
        }
    }
}

/// The finder a `--finder` and its `--finder-arg` name.
///
/// The only place in the crate that names the formats. What a format is configured with is bound
/// into the finder it returns, so nothing is carried alongside it.
///
/// # Errors
///
/// A format that is not one of `sep`, `fixed`, `flatbuffers` or `msgpack`, a `fixed` without a
/// length or with one of 0, and a parameter given to a format that takes none.
pub fn from_spec(name: &str, param: Option<&str>) -> anyhow::Result<Boundary> {
    let takes_none = |format: &str| -> anyhow::Result<()> {
        if param.is_some() {
            bail!("--finder {format} takes no --finder-arg");
        }
        Ok(())
    };
    match name {
        // The separator a file was written with is not recorded in it, and "\n" is what the
        // separator-only command line defaulted to.
        "sep" => Ok(Boundary::Separator(
            param.unwrap_or("\n").as_bytes().to_vec(),
        )),
        "fixed" => {
            let param = param.context("--finder fixed needs --finder-arg <length>, in bytes")?;
            let len: usize = param
                .parse()
                .with_context(|| format!("--finder-arg {param} is not a record length"))?;
            if len == 0 {
                bail!("--finder fixed needs a length above 0: a record of no bytes never ends");
            }
            Ok(Boundary::Finder(Box::new(by_fixed(len))))
        }
        "flatbuffers" => {
            takes_none("flatbuffers")?;
            Ok(Boundary::Finder(Box::new(by_le32_prefix)))
        }
        "msgpack" => {
            takes_none("msgpack")?;
            Ok(Boundary::Finder(Box::new(by_msgpack)))
        }
        other => bail!("unknown --finder {other}: sep, fixed, flatbuffers or msgpack"),
    }
}
