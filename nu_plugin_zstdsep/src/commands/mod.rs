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

use nu_plugin::{EngineInterface, EvaluatedCall};
use nu_protocol::{
    ShellError, Signature, Spanned, SyntaxShape, shell_error::generic::GenericError,
};

use crate::source::FinderSpec;

/// Where records end, from `--finder`, `--finder-arg` and `--separator`.
///
/// `--separator` is `--finder sep --finder-arg`, so it is refused alongside any other finder and
/// alongside `--finder-arg`. `sep` defaults to a newline. An empty separator has no record
/// boundaries to find and would leave every scan matching at every byte, so it is refused here
/// rather than in the library.
pub fn finder(call: &EvaluatedCall) -> Result<FinderSpec, ShellError> {
    let finder = call
        .get_flag::<String>("finder")?
        .unwrap_or_else(|| "sep".to_string());
    let arg: Option<Spanned<String>> = call.get_flag("finder-arg")?;
    let separator: Option<Spanned<String>> = call.get_flag("separator")?;
    if let Some(sep) = &separator {
        if finder != "sep" {
            return Err(ShellError::Generic(GenericError::new(
                format!("--separator cannot be given with --finder {finder}"),
                "--separator is --finder sep --finder-arg",
                sep.span,
            )));
        }
        if arg.is_some() {
            return Err(ShellError::Generic(GenericError::new(
                "--separator and --finder-arg cannot be given together",
                "--separator is --finder sep --finder-arg",
                sep.span,
            )));
        }
    }
    let arg = arg.or(separator);
    if finder == "sep" {
        if let Some(sep) = arg.as_ref().filter(|sep| sep.item.is_empty()) {
            return Err(ShellError::Generic(GenericError::new(
                "the separator must not be empty",
                "no record would end anywhere",
                sep.span,
            )));
        }
        return Ok(FinderSpec {
            finder,
            arg: Some(arg.map_or_else(|| "\n".to_string(), |sep| sep.item)),
        });
    }
    Ok(FinderSpec {
        finder,
        arg: arg.map(|arg| arg.item),
    })
}

/// The three flags every command takes, spelled once.
pub fn finder_flags(signature: Signature) -> Signature {
    signature
        .named(
            "finder",
            SyntaxShape::String,
            "record format: sep, fixed, flatbuffers or msgpack (default: sep)",
            None,
        )
        .named(
            "finder-arg",
            SyntaxShape::String,
            "what the finder is configured with: the separator for sep, the record length for fixed",
            None,
        )
        .named(
            "separator",
            SyntaxShape::String,
            "the separator records end with, which is --finder sep --finder-arg (default: a newline)",
            Some('s'),
        )
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
