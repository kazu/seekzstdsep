//! A nushell plugin over `.seek.zst` files: `zstdsep open f | get 10` decompresses one frame, not
//! the file.
//!
//! `open f | from …` cannot be lazy — the plugin is handed a byte stream and never learns the
//! path, so it cannot reach the seek table at the end of the file. `zstdsep open <path>` is
//! therefore a command of its own, and what it returns is a handle. Cell paths into that handle
//! are the one thing the engine delegates back to the plugin; list commands (`first`, `last`,
//! `where`, …) run engine-side and refuse it, and the remedy is `--no-partial`.
//!
//! See `docs/design/2026-08-24-zstdsep-nu-plugin.md` in the repository.
#![warn(missing_docs)]
// Every fallible call here returns nushell's `ShellError`, which is large by design. Nushell's own
// commands carry it the same way; making it fit clippy's bound would mean boxing at every site.
#![allow(clippy::result_large_err)]

mod commands;
mod decode;
mod encode;
mod handle;
mod json;
mod source;

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::PathBuf;
use std::sync::Mutex;

use nu_plugin::{EngineInterface, Plugin, PluginCommand};
use nu_protocol::{
    CustomValue, LabeledError, Record, ShellError, Spanned, Value, casing::Casing, record,
    shell_error::generic::GenericError,
};
use seekzstdsep::RecordReader;

pub use handle::ZstdsepHandle;

/// An open file, and which handle it belongs to.
///
/// The reader alone would do if an id could only ever mean one file. It cannot: see [`State`].
struct OpenFile {
    reader: RecordReader,
    path: PathBuf,
    separator: String,
}

/// The open files, keyed by the id their handles carry.
///
/// A `RecordReader` holds a decoder, the seek table and one decompressed frame, so the entry is
/// what makes reading record 11 after record 10 cost a lookup rather than a reopen.
///
/// **Ids are not unique across plugin processes.** Handles live engine-side and outlive the process
/// that made them, and the engine garbage collects an idle one after ten seconds; the next `open`
/// starts a new process whose counter begins again. Seeding the counter from the process id makes a
/// repeat unlikely, and checking the entry against the handle makes one harmless.
struct State {
    next_id: u64,
    readers: HashMap<u64, OpenFile>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            // The low half counts, so a process would have to open 4 billion files to reach the
            // next one's range.
            next_id: (std::process::id() as u64) << 32,
            readers: HashMap::new(),
        }
    }
}

/// The plugin process, and everything it keeps between calls.
#[derive(Default)]
pub struct ZstdsepPlugin {
    state: Mutex<State>,
}

impl ZstdsepPlugin {
    /// Takes an open file into the table and returns the id its handle will carry.
    fn register(&self, source: &source::Source, reader: RecordReader) -> Result<u64, ShellError> {
        let mut state = self.lock()?;
        let id = state.next_id;
        state.next_id += 1;
        state.readers.insert(
            id,
            OpenFile {
                reader,
                path: source.path.clone(),
                separator: source.separator.clone(),
            },
        );
        Ok(id)
    }

    /// Runs `f` against the open file behind `handle`, opening it first if the table has none.
    ///
    /// The table is lost when the engine garbage collects the idle plugin process, and a cell path
    /// arriving afterwards has to work all the same. Everything needed to reopen travels in the
    /// handle, so the miss costs an open rather than an error.
    ///
    /// An entry under the right id for the wrong file is the same miss: ids repeat across processes
    /// (see [`State`]), and returning another file's records would be silent and wrong.
    fn with_reader<T>(
        &self,
        handle: &ZstdsepHandle,
        span: nu_protocol::Span,
        f: impl FnOnce(&mut RecordReader) -> Result<T, ShellError>,
    ) -> Result<T, ShellError> {
        let mut state = self.lock()?;
        let open = match state.readers.entry(handle.id) {
            Entry::Occupied(entry) => {
                let entry = entry.into_mut();
                if !handle.refers_to(&entry.path, &entry.separator) {
                    *entry = open_for(handle, span)?;
                }
                entry
            }
            Entry::Vacant(entry) => entry.insert(open_for(handle, span)?),
        };
        f(&mut open.reader)
    }

    /// What a handle says about its file: everything but the records.
    fn summary(
        &self,
        handle: &ZstdsepHandle,
        span: nu_protocol::Span,
    ) -> Result<Record, ShellError> {
        self.with_reader(handle, span, |reader| {
            Ok(record! {
                "path" => Value::string(handle.path.to_string_lossy(), span),
                "separator" => Value::string(handle.separator.clone(), span),
                "format" => match &handle.format {
                    Some(name) => Value::string(name.clone(), span),
                    None => Value::nothing(span),
                },
                "frames" => Value::int(reader.frame_count() as i64, span),
                "records_per_frame" => Value::int(reader.records_per_frame() as i64, span),
                "records" => Value::int(total_records(reader, span)? as i64, span),
            })
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, State>, ShellError> {
        self.state.lock().map_err(|_| {
            ShellError::Generic(GenericError::new_internal(
                "the zstdsep plugin state is poisoned",
                "an earlier call panicked while holding it",
            ))
        })
    }
}

/// Opens what `handle` refers to, ready to go into the table under its id.
fn open_for(handle: &ZstdsepHandle, span: nu_protocol::Span) -> Result<OpenFile, ShellError> {
    Ok(OpenFile {
        reader: handle.source().open(span)?,
        path: handle.path.clone(),
        separator: handle.separator.clone(),
    })
}

/// How many records the file holds, which needs the last frame decompressed.
fn total_records(reader: &mut RecordReader, span: nu_protocol::Span) -> Result<usize, ShellError> {
    reader.total_records().map_err(|e| {
        ShellError::Generic(GenericError::new(
            "cannot count the records",
            e.to_string(),
            span,
        ))
    })
}

/// The handle inside a custom value, or an error naming what arrived instead.
fn handle_of(value: &dyn CustomValue) -> Result<&ZstdsepHandle, LabeledError> {
    value
        .as_any()
        .downcast_ref::<ZstdsepHandle>()
        .ok_or_else(|| {
            LabeledError::new(format!("expected a {}", handle::TYPE_NAME))
                .with_label(value.type_name(), nu_protocol::Span::unknown())
        })
}

impl Plugin for ZstdsepPlugin {
    fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").into()
    }

    fn commands(&self) -> Vec<Box<dyn PluginCommand<Plugin = Self>>> {
        vec![
            Box::new(commands::Zstdsep),
            Box::new(commands::Inspect),
            Box::new(commands::Open),
            Box::new(commands::Save),
        ]
    }

    /// The summary shown when a handle is displayed, not the data behind it.
    ///
    /// Displaying `$h` triggers this, and materialising a whole file because it was named at a
    /// prompt would be a footgun. The data stays behind `--no-partial`.
    fn custom_value_to_base_value(
        &self,
        _engine: &EngineInterface,
        custom_value: Spanned<Box<dyn CustomValue>>,
    ) -> Result<Value, LabeledError> {
        let span = custom_value.span;
        let handle = handle_of(custom_value.item.as_ref())?;
        Ok(Value::record(self.summary(handle, span)?, span))
    }

    /// `$h.records` and the like: a field of the summary, not of a record in the file.
    ///
    /// Indices address the file and names address the handle, which is the split the summary
    /// already draws. Without this the summary would only be reachable by displaying it.
    fn custom_value_follow_path_string(
        &self,
        _engine: &EngineInterface,
        custom_value: Spanned<Box<dyn CustomValue>>,
        column_name: Spanned<String>,
        optional: bool,
        casing: Casing,
    ) -> Result<Value, LabeledError> {
        let span = custom_value.span;
        let handle = handle_of(custom_value.item.as_ref())?;
        let summary = self.summary(handle, span)?;
        match summary.cased(casing).get(&column_name.item) {
            Some(value) => Ok(value.clone()),
            None if optional => Ok(Value::nothing(column_name.span)),
            None => Err(LabeledError::from(ShellError::CantFindColumn {
                col_name: column_name.item,
                span: Some(column_name.span),
                src_span: span,
            })),
        }
    }

    /// `$h.10` and `get 10`: one frame decompressed, one record parsed.
    fn custom_value_follow_path_int(
        &self,
        _engine: &EngineInterface,
        custom_value: Spanned<Box<dyn CustomValue>>,
        index: Spanned<usize>,
        optional: bool,
    ) -> Result<Value, LabeledError> {
        let span = custom_value.span;
        let handle = handle_of(custom_value.item.as_ref())?;
        let source = handle.source();
        let bytes = self.with_reader(handle, span, |reader| {
            reader.record(index.item).map_err(|e| {
                ShellError::Generic(GenericError::new(
                    format!("cannot read record {}", index.item),
                    e.to_string(),
                    index.span,
                ))
            })
        })?;

        match bytes {
            Some(bytes) => Ok(decode::record(&source, &bytes, index.span)?),
            None if optional => Ok(Value::nothing(index.span)),
            None => Err(LabeledError::from(ShellError::AccessBeyondEnd {
                max_idx: self
                    .with_reader(handle, span, |reader| total_records(reader, span))
                    .map(|n| n.saturating_sub(1))
                    .unwrap_or(0),
                span: index.span,
            })),
        }
    }

    /// The handle went out of scope engine-side; nothing is left to keep the file open for.
    fn custom_value_dropped(
        &self,
        _engine: &EngineInterface,
        custom_value: Box<dyn CustomValue>,
    ) -> Result<(), LabeledError> {
        let handle = handle_of(custom_value.as_ref())?;
        self.lock()?.readers.remove(&handle.id);
        Ok(())
    }
}
