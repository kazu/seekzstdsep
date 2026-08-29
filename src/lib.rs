#![doc = include_str!("../README.md")]
#![warn(missing_docs)]

// Compiles README.ja.md's examples as doctests so the translation cannot drift.
#[doc = include_str!("../README.ja.md")]
#[cfg(doctest)]
pub struct ReadmeJaDoctests;

pub mod cli;
pub mod edit;
mod reader;
mod record;
pub mod seekzstdsep_lib;
pub use edit::Alignment;
pub use edit::AppendInput;
pub use edit::OnMissingSeparator;
pub use edit::RangeCheck;
pub use edit::SeparatorCheck;
pub use edit::append;
pub use edit::append_frames;
pub use edit::append_records;
pub use edit::copy_range;
pub use edit::count_frames;
pub use edit::truncate;
pub use reader::RecordIter;
pub use reader::RecordReader;
pub use seekzstdsep_lib::CompressOptions;
pub use seekzstdsep_lib::CompressionLevel;
pub use seekzstdsep_lib::InspectOptions;
pub use seekzstdsep_lib::ReadSeekable;
pub use seekzstdsep_lib::compress_to_seekable_zst;
pub use seekzstdsep_lib::compress_to_seekable_zst_with_opts;
pub use seekzstdsep_lib::convert_text_to_seekable_zst_reader;
pub use seekzstdsep_lib::convert_to_seekable_zst_reader;
pub use seekzstdsep_lib::convert_to_seekable_zst_reader_with_opts;
pub use seekzstdsep_lib::inspect;
