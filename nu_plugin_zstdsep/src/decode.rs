//! Turning record bytes into nushell values.
//!
//! A command resolves `from <name>` in the caller's scope and pipes records through it, so a
//! logfmt plugin brings logfmt and a format written later needs no change here. A cell path
//! cannot: nushell runs custom value ops with no execution context, so there is no engine to ask.
//! json is parsed in process for that reason — see [`crate::json`].
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
    let bytes = strip_separator(bytes, source.separator.as_bytes());
    match (&source.format, std::str::from_utf8(bytes)) {
        (Format::Json, Ok(text)) => json::parse(text, span),
        _ => Ok(raw_value(bytes, span)),
    }
}

/// Every record of `source`, decompressing one frame at a time.
///
/// The engine drops the stream when a command downstream stops reading, so `first 3` reads the
/// first frame and no more.
pub fn stream(
    engine: &EngineInterface,
    source: &Source,
    span: Span,
) -> Result<PipelineData, ShellError> {
    let reader = source.open(span)?;
    let name = match &source.format {
        Format::From(name) => name.clone(),
        // Parsed the same way a cell path parses it, so `$h.10` and `--no-partial | get 10` agree.
        Format::Raw | Format::Json => {
            let source = source.clone();
            let values = reader.into_records().map(move |result| {
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
            });
            return Ok(PipelineData::ListStream(
                ListStream::new(values, span, engine.signals().clone()),
                None,
            ));
        }
    };

    // The decoder is handed over whole rather than record by record: `from <name>` splits records
    // itself, and a byte stream is what it reads fastest.
    let bytes = reader.into_bytes().map_err(|e| {
        ShellError::Generic(GenericError::new(
            format!("cannot read {}", source.path.display()),
            e.to_string(),
            span,
        ))
    })?;
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
