//! The commands the plugin registers. Everything the engine will not delegate has to be one.
mod inspect;
mod open;
mod save;
mod zstdsep;

pub use inspect::Inspect;
pub use open::Open;
pub use save::Save;
pub use zstdsep::Zstdsep;

use std::path::PathBuf;

use nu_plugin::EngineInterface;
use nu_protocol::{ShellError, Spanned, shell_error::generic::GenericError};

/// The separator records end with, from `--separator`, defaulting to a newline.
///
/// An empty one has no record boundaries to find and would leave every scan matching at every
/// byte, so it is refused here rather than in the library.
pub fn separator(value: Option<Spanned<String>>) -> Result<String, ShellError> {
    match value {
        None => Ok("\n".to_string()),
        Some(sep) if sep.item.is_empty() => Err(ShellError::Generic(GenericError::new(
            "the separator must not be empty",
            "no record would end anywhere",
            sep.span,
        ))),
        Some(sep) => Ok(sep.item),
    }
}

/// A path as typed, made absolute against the caller's directory.
///
/// `SyntaxShape::Filepath` expands `~` but leaves a relative path relative, and the plugin process
/// has a working directory of its own.
pub fn resolve(engine: &EngineInterface, path: &str) -> Result<PathBuf, ShellError> {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        return Ok(path);
    }
    Ok(PathBuf::from(engine.get_current_dir()?).join(path))
}
