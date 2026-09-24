//! The value `zstdsep open` hands back.
use std::path::PathBuf;

use nu_protocol::{CustomValue, ShellError, Span, Value, shell_error::generic::GenericError};
use serde::{Deserialize, Serialize};

use crate::source::{FinderSpec, Format, Source};

/// What `describe` and the engine's own error messages call this value. A builtin list command on
/// a handle fails engine-side with a message that prints this name, so it has to identify itself.
pub const TYPE_NAME: &str = "zstdsep handle";

/// Files opened for lazy reading: an index into the plugin's state table, plus everything needed
/// to rebuild that entry after the plugin has been garbage collected and restarted.
///
/// Several files are one handle rather than a handle each, because an index into it addresses the
/// records of all of them in the order they were named. The finder and the format are one for the
/// whole handle: a single run of indices only makes sense over records read the same way.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZstdsepHandle {
    /// Which entry of the plugin's state table this refers to.
    pub id: u64,
    /// The files, as absolute paths, in the order they were named. Never empty: `zstdsep open`
    /// refuses a call that names no file.
    pub paths: Vec<PathBuf>,
    /// Where their records end.
    pub finder: FinderSpec,
    /// The `from <name>` their records are parsed by, or `None` for raw strings.
    pub format: Option<String>,
}

impl ZstdsepHandle {
    /// The handle for `sources`, registered under `id`.
    ///
    /// The finder and the format are taken from the first source; `zstdsep open` builds them all
    /// with the same ones.
    pub fn new(id: u64, sources: &[Source]) -> Self {
        let first = &sources[0];
        Self {
            id,
            paths: sources.iter().map(|s| s.path.clone()).collect(),
            finder: first.finder.clone(),
            format: first.format.name().map(str::to_string),
        }
    }

    /// Whether this handle was made for `paths` read with `finder`.
    ///
    /// What identifies an entry to the plugin. The format is left out: it decides how a record is
    /// turned into a value, not which bytes are read, so two handles that differ only there can
    /// share one set of open files.
    pub fn refers_to(&self, paths: &[PathBuf], finder: &FinderSpec) -> bool {
        self.paths == paths && &self.finder == finder
    }

    /// What a record of this handle is decoded by, whichever file it came out of.
    ///
    /// The finder and the format are the handle's, so decoding needs no file; the first one stands
    /// for all of them.
    pub fn source(&self) -> Source {
        self.source_of(self.paths[0].clone())
    }

    /// One of the files, ready to open. Carried in the value rather than in the state table, so a
    /// cell path that arrives after a restart can reopen it.
    pub fn source_of(&self, path: PathBuf) -> Source {
        Source {
            path,
            finder: self.finder.clone(),
            format: match &self.format {
                None => Format::Raw,
                Some(name) => Format::named(name),
            },
        }
    }
}

/// Every operation the engine delegates needs the plugin's state table, so all of them are
/// implemented on [`crate::ZstdsepPlugin`] instead. What is left here is what the engine reads off
/// the value itself.
#[typetag::serde]
impl CustomValue for ZstdsepHandle {
    fn clone_value(&self, span: Span) -> Value {
        Value::custom(Box::new(self.clone()), span)
    }

    fn type_name(&self) -> String {
        TYPE_NAME.to_string()
    }

    fn to_base_value(&self, span: Span) -> Result<Value, ShellError> {
        Err(ShellError::Generic(GenericError::new(
            "a zstdsep handle can only be collapsed by the plugin that made it",
            "no plugin state to summarise from",
            span,
        )))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_mut_any(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn notify_plugin_on_drop(&self) -> bool {
        true
    }
}
