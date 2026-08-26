//! The value `zstdsep open` hands back.
use std::path::{Path, PathBuf};

use nu_protocol::{CustomValue, ShellError, Span, Value, shell_error::generic::GenericError};
use serde::{Deserialize, Serialize};

use crate::source::{Format, Source};

/// What `describe` and the engine's own error messages call this value. A builtin list command on
/// a handle fails engine-side with a message that prints this name, so it has to identify itself.
pub const TYPE_NAME: &str = "zstdsep handle";

/// A file opened for lazy reading: an index into the plugin's state table, plus everything needed
/// to rebuild that entry after the plugin has been garbage collected and restarted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZstdsepHandle {
    /// Which entry of the plugin's state table this refers to.
    pub id: u64,
    /// The file, as an absolute path.
    pub path: PathBuf,
    /// The separator its records end with.
    pub separator: String,
    /// The `from <name>` its records are parsed by, or `None` for raw strings.
    pub format: Option<String>,
}

impl ZstdsepHandle {
    /// The handle for `source`, registered under `id`.
    pub fn new(id: u64, source: &Source) -> Self {
        Self {
            id,
            path: source.path.clone(),
            separator: source.separator.clone(),
            format: source.format.name().map(str::to_string),
        }
    }

    /// Whether this handle was made for `path` read with `separator`.
    ///
    /// What identifies a file to the plugin. The format is left out: it decides how a record is
    /// turned into a value, not which bytes are read, so two handles that differ only there can
    /// share one open file.
    pub fn refers_to(&self, path: &Path, separator: &str) -> bool {
        self.path == path && self.separator == separator
    }

    /// The file this refers to. Carried in the value rather than in the state table, so a cell
    /// path that arrives after a restart can reopen it.
    pub fn source(&self) -> Source {
        Source {
            path: self.path.clone(),
            separator: self.separator.clone(),
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
