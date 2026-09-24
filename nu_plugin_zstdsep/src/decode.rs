//! Turning record bytes into nushell values.
//!
//! A command resolves `from <name>` in the caller's scope and pipes records through it, so a
//! logfmt plugin brings logfmt and a format written later needs no change here. A cell path
//! cannot: nushell runs custom value ops with no execution context, so there is no engine to ask.
//! json is parsed in process for that reason — see [`crate::json`].
use std::io::Read;

use nu_plugin::{EngineInterface, EvaluatedCall};
use nu_protocol::{
    ByteStream, ByteStreamType, ListStream, PipelineData, ShellError, Span, Value,
    shell_error::generic::GenericError,
};

use crate::json;
use crate::source::{Format, Source};

/// One record, without its separator, as a value.
///
/// Parsing here rather than in the caller is what keeps a deep cell path (`$h.10.user.name`)
/// working: the engine can only follow the rest of the path into a value it understands.
///
/// A cell path has no engine to call, so [`Format::From`] has nothing to resolve and the record
/// comes back as a string. `--no-partial` is where those formats get parsed.
pub fn record(source: &Source, bytes: &[u8], span: Span) -> Result<Value, ShellError> {
    let bytes = strip_separator(bytes, source.finder.separator());
    match (&source.format, std::str::from_utf8(bytes)) {
        (Format::Json, Ok(text)) => json::parse(text, span),
        _ => Ok(raw_value(bytes, span)),
    }
}

/// Every record of `sources` in the order they were named, decompressing one frame at a time.
///
/// The engine drops the stream when a command downstream stops reading, so `first 3` reads the
/// first frame and no more.
///
/// Every file is opened before anything is read, so a file that cannot be opened is reported by
/// the command the user typed rather than partway through the records.
pub fn stream(
    engine: &EngineInterface,
    sources: &[Source],
    span: Span,
) -> Result<PipelineData, ShellError> {
    let readers = sources
        .iter()
        .map(|source| source.open(span))
        .collect::<Result<Vec<_>, _>>()?;
    let name = match &sources[0].format {
        Format::From(name) => name.clone(),
        // Parsed the same way a cell path parses it, so `$h.10` and `--no-partial | get 10` agree.
        Format::Raw | Format::Json => {
            // One source for all of them: a record is decoded by the finder and the format, and
            // those are the same for every file of a handle.
            let owned = sources[0].clone();
            let values = readers.into_iter().flat_map(move |reader| {
                let source = owned.clone();
                reader.into_records().map(move |result| {
                    match result
                        .map_err(|e| {
                            ShellError::Generic(GenericError::new(
                                "cannot read a record",
                                e.to_string(),
                                span,
                            ))
                        })
                        .and_then(|bytes| record(&source, &bytes, span))
                    {
                        Ok(value) => value,
                        Err(e) => Value::error(e, span),
                    }
                })
            });
            return Ok(PipelineData::ListStream(
                ListStream::new(values, span, engine.signals().clone()),
                None,
            ));
        }
    };

    // The decoder is handed over whole rather than record by record: `from <name>` splits records
    // itself, and a byte stream is what it reads fastest. Several files are one stream, which is
    // what makes `from <name>` see one table rather than one per file.
    // Folded rather than chained onto an `empty()`, so that one file is the decoder itself.
    let mut bytes: Option<Box<dyn Read + Send>> = None;
    for (reader, source) in readers.into_iter().zip(sources) {
        let next = reader.into_bytes().map_err(|e| {
            ShellError::Generic(GenericError::new(
                format!("cannot read {}", source.path.display()),
                e.to_string(),
                span,
            ))
        })?;
        bytes = Some(match bytes {
            None => Box::new(next),
            Some(bytes) => Box::new(bytes.chain(next)),
        });
    }
    let bytes = bytes.expect("a handle is opened over at least one file");
    let input = PipelineData::ByteStream(
        ByteStream::read(
            bytes,
            span,
            engine.signals().clone(),
            ByteStreamType::String,
        ),
        None,
    );
    call_from(engine, &name, input, span)
}

/// Pipes `input` through `from <name>`, resolved in the caller's scope.
fn call_from(
    engine: &EngineInterface,
    name: &str,
    input: PipelineData,
    span: Span,
) -> Result<PipelineData, ShellError> {
    let command = format!("from {name}");
    let decl = engine.find_decl(command.clone())?.ok_or_else(|| {
        ShellError::Generic(
            GenericError::new(
                format!("`{command}` is not in scope"),
                "no command to parse the records with",
                span,
            )
            .with_help(
                "run `use std formats *` for jsonl and ndjson, pass --format to name another \
                     one, or pass --raw to get the records as strings",
            ),
        )
    })?;
    engine.call_decl(decl, EvaluatedCall::new(span), input, true, false)
}

/// A string when the bytes are text, binary when they are not.
fn raw_value(bytes: &[u8], span: Span) -> Value {
    match std::str::from_utf8(bytes) {
        Ok(text) => Value::string(text, span),
        Err(_) => Value::binary(bytes, span),
    }
}

/// The record without the separator that ends it. A record read past the end of a file has none.
fn strip_separator<'a>(bytes: &'a [u8], separator: &[u8]) -> &'a [u8] {
    bytes.strip_suffix(separator).unwrap_or(bytes)
}
