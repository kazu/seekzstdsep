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
pub use source::FinderSpec;

/// The open files behind one handle, and which handle they belong to.
///
/// The readers alone would do if an id could only ever mean one set of files. It cannot: see
/// [`State`].
///
/// There is always at least one file: `zstdsep open` refuses a call that names none, and every
/// entry is built from what one of its handles carries.
struct OpenFiles {
    readers: Vec<RecordReader>,
    paths: Vec<PathBuf>,
    finder: source::FinderSpec,
    /// How many records the files up to each one hold, for as many as have been counted.
    ///
    /// `counted[i]` is the total of files `0..=i`, so an index below it falls in file `i` or
    /// earlier. Filled from the front as a lookup needs it and never shortened: counting a file
    /// decompresses its last frame, and a read that stops in file 0 has no reason to pay for that
    /// in the files behind it.
    ///
    /// A file's count is where the next file's indices start, so the known miscount of
    /// `total_records` (`docs/bugs.md`) shifts every record behind it rather than only the total.
    counted: Vec<usize>,
}

impl OpenFiles {
    /// How many records the files up to and including `i` hold, counting the ones not counted yet.
    fn counted_upto(&mut self, i: usize, span: nu_protocol::Span) -> Result<usize, ShellError> {
        while self.counted.len() <= i {
            let next = self.counted.len();
            let records = total_records(&mut self.readers[next], span)?;
            let before = self.counted.last().copied().unwrap_or(0);
            self.counted.push(before + records);
        }
        Ok(self.counted[i])
    }

    /// The record at `index` of the files read as one sequence.
    ///
    /// The last file is read without being counted first: a record past its end is the end of the
    /// sequence, which its own reader reports by returning nothing.
    fn record(
        &mut self,
        index: usize,
        span: nu_protocol::Span,
    ) -> Result<Option<Vec<u8>>, ShellError> {
        let last = self.readers.len() - 1;
        let mut file = last;
        let mut before = 0;
        for i in 0..last {
            let upto = self.counted_upto(i, span)?;
            if index < upto {
                file = i;
                break;
            }
            before = upto;
        }
        self.readers[file].record(index - before).map_err(|e| {
            ShellError::Generic(GenericError::new(
                format!("cannot read record {index}"),
                e.to_string(),
                span,
            ))
        })
    }

    /// How many records all of the files hold together.
    fn total(&mut self, span: nu_protocol::Span) -> Result<usize, ShellError> {
        self.counted_upto(self.readers.len() - 1, span)
    }
}

/// The open files, keyed by the id their handles carry.
///
/// A `RecordReader` holds a decoder, the seek table and the window its last lookup read through,
/// so the entry is what makes reading record 11 after record 10 cost a walk on rather than a
/// reopen.
///
/// **Ids are not unique across plugin processes.** Handles live engine-side and outlive the process
/// that made them, and the engine garbage collects an idle one after ten seconds; the next `open`
/// starts a new process whose counter begins again. Seeding the counter from the process id makes a
/// repeat unlikely, and checking the entry against the handle makes one harmless.
struct State {
    next_id: u64,
    readers: HashMap<u64, OpenFiles>,
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
    /// Takes the open files into the table and returns the id their handle will carry.
    fn register(
        &self,
        sources: &[source::Source],
        readers: Vec<RecordReader>,
    ) -> Result<u64, ShellError> {
        let mut state = self.lock()?;
        let id = state.next_id;
        state.next_id += 1;
        state.readers.insert(
            id,
            OpenFiles {
                readers,
                paths: sources.iter().map(|s| s.path.clone()).collect(),
                finder: sources[0].finder.clone(),
                counted: Vec::new(),
            },
        );
        Ok(id)
    }

    /// Runs `f` against the open files behind `handle`, opening them first if the table has none.
    ///
    /// The table is lost when the engine garbage collects the idle plugin process, and a cell path
    /// arriving afterwards has to work all the same. Everything needed to reopen travels in the
    /// handle, so the miss costs an open rather than an error.
    ///
    /// An entry under the right id for the wrong files is the same miss: ids repeat across
    /// processes (see [`State`]), and returning another file's records would be silent and wrong.
    fn with_files<T>(
        &self,
        handle: &ZstdsepHandle,
        span: nu_protocol::Span,
        f: impl FnOnce(&mut OpenFiles) -> Result<T, ShellError>,
    ) -> Result<T, ShellError> {
        let mut state = self.lock()?;
        let open = match state.readers.entry(handle.id) {
            Entry::Occupied(entry) => {
                let entry = entry.into_mut();
                if !handle.refers_to(&entry.paths, &entry.finder) {
                    *entry = open_for(handle, span)?;
                }
                entry
            }
            Entry::Vacant(entry) => entry.insert(open_for(handle, span)?),
        };
        f(open)
    }

    /// What a handle says about its files: everything but the records.
    ///
    /// `records` costs one frame decompressed per file, and it is the only field that does, so a
    /// caller that wants one of the others passes `with_records` false and pays for none of them.
    fn summary(
        &self,
        handle: &ZstdsepHandle,
        with_records: bool,
        span: nu_protocol::Span,
    ) -> Result<Record, ShellError> {
        self.with_files(handle, span, |open| {
            let mut summary = record! {
                "path" => per_file(
                    handle.paths.iter().map(|p| Value::string(p.to_string_lossy(), span)),
                    span,
                ),
                "finder" => Value::string(handle.finder.finder.clone(), span),
                "finder_arg" => match &handle.finder.arg {
                    Some(arg) => Value::string(arg.clone(), span),
                    None => Value::nothing(span),
                },
                "format" => match &handle.format {
                    Some(name) => Value::string(name.clone(), span),
                    None => Value::nothing(span),
                },
                "frames" => per_file(
                    open.readers.iter().map(|r| Value::int(r.frame_count() as i64, span)),
                    span,
                ),
                "records_per_frame" => per_file(
                    open.readers.iter().map(|r| Value::int(r.records_per_frame() as i64, span)),
                    span,
                ),
            };
            if with_records {
                // One number, not one per file: the whole point of a handle over several files is
                // that their records are one run of indices.
                summary.push("records", Value::int(open.total(span)? as i64, span));
            }
            Ok(summary)
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
fn open_for(handle: &ZstdsepHandle, span: nu_protocol::Span) -> Result<OpenFiles, ShellError> {
    let readers = handle
        .paths
        .iter()
        .map(|path| handle.source_of(path.clone()).open(span))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(OpenFiles {
        readers,
        paths: handle.paths.clone(),
        finder: handle.finder.clone(),
        counted: Vec::new(),
    })
}

/// A per-file field of the summary: the value itself for one file, a list of them for several.
///
/// A handle over one file is the common case and reads as one file's, so widening its summary to
/// lists would be a change for everyone to pay for the files they do not open.
fn per_file(values: impl Iterator<Item = Value>, span: nu_protocol::Span) -> Value {
    let mut values: Vec<Value> = values.collect();
    match values.len() {
        1 => values.pop().expect("a length of one has an element"),
        _ => Value::list(values, span),
    }
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
        Ok(Value::record(self.summary(handle, true, span)?, span))
    }

    /// `$h.records` and the like: a field of the summary, not of a record in the file.
    ///
    /// Indices address the file and names address the handle, which is the split the summary
    /// already draws. Without this the summary would only be reachable by displaying it.
    ///
    /// Only `records` is counted, and only when it is the field asked for: `$h.path` over a
    /// hundred files would otherwise decompress a hundred last frames to answer with a path.
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
        // Case is folded here whether or not the caller folds it: the name that does not match
        // then costs a count and still fails to find its column, which is the cheap way round.
        let summary = self.summary(
            handle,
            column_name.item.eq_ignore_ascii_case("records"),
            span,
        )?;
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

    /// `$h.10` and `get 10`: one frame decoded up to the record, one record parsed.
    fn custom_value_follow_path_int(
        &self,
        _engine: &EngineInterface,
        custom_value: Spanned<Box<dyn CustomValue>>,
        index: Spanned<usize>,
        optional: bool,
    ) -> Result<Value, LabeledError> {
        let span = custom_value.span;
        let handle = handle_of(custom_value.item.as_ref())?;
        let found = self.with_files(handle, span, |open| open.record(index.item, index.span))?;

        match found {
            Some(bytes) => Ok(decode::record(&handle.source(), &bytes, index.span)?),
            None if optional => Ok(Value::nothing(index.span)),
            None => Err(LabeledError::from(ShellError::AccessBeyondEnd {
                max_idx: self
                    .with_files(handle, span, |open| open.total(span))
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
