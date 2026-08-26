//! What a command needs to read a file: where it is, how records end, and what parses them.
use std::path::{Path, PathBuf};

use nu_protocol::{ShellError, Span, shell_error::generic::GenericError};
use seekzstdsep::RecordReader;

/// The extension the compressor adds, and the marker the crate's own files carry before it.
const COMPRESSED_EXTENSION: &str = "zst";
const SEEKABLE_MARKER: &str = "seek";

/// The formats the plugin parses itself. They are one thing here: a record is one JSON value.
const JSON_FORMATS: [&str; 3] = ["json", "jsonl", "ndjson"];

/// What the payload of a file gets parsed by.
///
/// Two ways of parsing rather than one, because a cell path cannot reach the caller's scope:
/// nushell services custom value ops with no execution context, so `FindDecl` fails there. See
/// [`crate::json`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Format {
    /// One string per record, separator removed.
    Raw,
    /// One JSON value per record, parsed in the plugin. Works in a cell path.
    Json,
    /// Piped through the `from <name>` command found in the caller's scope. Commands only; a cell
    /// path falls back to [`Format::Raw`].
    From(String),
}

impl Format {
    /// The format the name stands for, whether it is a `--format` argument or an extension.
    pub fn named(name: &str) -> Self {
        if JSON_FORMATS.contains(&name) {
            Format::Json
        } else {
            Format::From(name.to_string())
        }
    }

    /// The format named by the inner extension, so `events.jsonl.seek.zst` parses as `jsonl`.
    ///
    /// A path that names none is [`Format::Raw`].
    pub fn of_path(path: &Path) -> Self {
        match inner_extension(path) {
            Some(name) => Format::named(&name),
            None => Format::Raw,
        }
    }

    /// What the handle carries and `describe` shows. `None` is [`Format::Raw`].
    pub fn name(&self) -> Option<&str> {
        match self {
            Format::Raw => None,
            Format::Json => Some("json"),
            Format::From(name) => Some(name),
        }
    }
}

/// The format a path names, so `events.jsonl.seek.zst` names `jsonl`.
///
/// Both of the crate's own extensions are optional and are stripped in the order they are written:
/// what is left after `.zst` and `.seek` is the name.
///
/// The name rather than a [`Format`], because `save` needs the two that [`Format::named`] folds
/// together: `to json` writes one array and `to jsonl` writes one value per line.
pub fn inner_extension(path: &Path) -> Option<String> {
    let mut stem = path.to_path_buf();
    for extension in [COMPRESSED_EXTENSION, SEEKABLE_MARKER] {
        if stem.extension().is_some_and(|e| e == extension) {
            stem.set_extension("");
        }
    }
    stem.extension()
        .and_then(|e| e.to_str())
        .map(str::to_string)
}

/// A file to read, resolved: the three things every command and every cell path needs.
#[derive(Clone, Debug)]
pub struct Source {
    pub path: PathBuf,
    pub separator: String,
    pub format: Format,
}

impl Source {
    /// Opens the file. The reader reads the seek table and frame 0, so this is where an unreadable
    /// file and a separator that ends no record are both reported.
    ///
    /// A file does not record the separator it was written with, so the wrong one is an ordinary
    /// mistake, and one that costs nothing to make: every record count comes out 0 and every index
    /// is out of range. Refusing it here names the cause once, at the command the user typed. A
    /// file that really holds no records is refused with it — the message is true of both.
    pub fn open(&self, span: Span) -> Result<RecordReader, ShellError> {
        let reader =
            RecordReader::open(self.path.clone(), self.separator.as_bytes()).map_err(|e| {
                ShellError::Generic(GenericError::new(
                    format!("cannot read {}", self.path.display()),
                    e.to_string(),
                    span,
                ))
            })?;
        if reader.records_per_frame() == 0 {
            return Err(ShellError::Generic(
                GenericError::new(
                    format!(
                        "no record in {} ends with {:?}",
                        self.path.display(),
                        self.separator
                    ),
                    "the file holds no records that this separator can find".to_string(),
                    span,
                )
                .with_help(
                    "a file does not record its own separator; pass --separator with the one it \
                     was written with",
                ),
            ));
        }
        Ok(reader)
    }
}
