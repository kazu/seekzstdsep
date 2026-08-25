//! Turning nushell values into record bytes.
//!
//! The reverse of [`crate::decode`], and asymmetric in the same place. `save` is a command
//! invocation, so `to <name>` can be resolved in the caller's scope — but only a builtin one:
//! `EngineInterface::call_decl` answers a command defined in nushell itself with "can't run custom
//! command with 'run'", and `to jsonl` is one of those (`std formats`). json, jsonl and ndjson are
//! therefore written in the plugin, exactly as [`crate::json`] reads them.
//!
//! What decides the route is the input, not the format: text is written as it came, because a
//! string is already the bytes the caller means. Everything else is structured and needs
//! serialising.
use std::io::{self, Write};

use nu_plugin::{EngineInterface, EvaluatedCall};
use nu_protocol::{
    ListStream, PipelineData, ShellError, Span, Value, shell_error::generic::GenericError,
};

use crate::json;
use crate::source::Format;

/// Writes `input` to `sink` as records ending with `separator`.
///
/// `format` names the `to` command that serialises structured input; text input ignores it. A
/// record the input does not terminate is terminated here, since a file ending in a fragment is
/// one that `--append` refuses later.
pub fn records(
    engine: &EngineInterface,
    input: PipelineData,
    format: Option<&str>,
    separator: &[u8],
    span: Span,
    sink: impl Write,
) -> Result<(), ShellError> {
    match input {
        PipelineData::Empty => Ok(()),
        // A list is one record per item, whatever the extension says, as long as the items are
        // already text. This is what `open --raw --no-partial` returns, so it round-trips.
        PipelineData::Value(Value::List { vals, .. }, metadata) => {
            let stream = ListStream::new(vals.into_iter(), span, engine.signals().clone());
            records(
                engine,
                PipelineData::list_stream(stream, metadata),
                format,
                separator,
                span,
                sink,
            )
        }
        PipelineData::ListStream(stream, metadata) => {
            let list_span = stream.span();
            let mut items = stream.into_inner();
            let Some(first) = items.next() else {
                return Ok(());
            };
            match text_bytes(first) {
                Ok(bytes) => {
                    let mut sink = sink;
                    write_record(&mut sink, &bytes, separator, span)?;
                    for value in items {
                        let bytes =
                            text_bytes(value).map_err(|value| not_text(&value, list_span))?;
                        write_record(&mut sink, &bytes, separator, span)?;
                    }
                    sink.flush().map_err(|e| failed_to_write(e, span))
                }
                Err(first) => {
                    let values = std::iter::once(first).chain(items);
                    if is_json(format) {
                        json_records(values, separator, sink, span)
                    } else {
                        let rest = ListStream::new(values, list_span, engine.signals().clone());
                        let input = PipelineData::list_stream(rest, metadata);
                        text(
                            serialise(engine, input, format, span)?,
                            separator,
                            sink,
                            span,
                        )
                    }
                }
            }
        }
        PipelineData::Value(Value::Error { error, .. }, _) => Err(*error),
        PipelineData::Value(value, metadata) => match text_bytes(value) {
            Ok(bytes) => {
                let mut sink = sink;
                write_record(&mut sink, &bytes, separator, span)?;
                sink.flush().map_err(|e| failed_to_write(e, span))
            }
            Err(value) if is_json(format) => {
                json_records(std::iter::once(value), separator, sink, span)
            }
            Err(value) => {
                let input = PipelineData::value(value, metadata);
                text(
                    serialise(engine, input, format, span)?,
                    separator,
                    sink,
                    span,
                )
            }
        },
        // A byte stream is bytes by construction; nothing here can add to them.
        input => text(input, separator, sink, span),
    }
}

/// Whether the plugin writes this format itself. The same three names [`crate::json`] reads.
fn is_json(format: Option<&str>) -> bool {
    matches!(format.map(Format::named), Some(Format::Json))
}

/// Writes one JSON record per value.
fn json_records(
    values: impl Iterator<Item = Value>,
    separator: &[u8],
    mut sink: impl Write,
    span: Span,
) -> Result<(), ShellError> {
    for value in values {
        let record = json::text(value, span)?;
        write_record(&mut sink, record.as_bytes(), separator, span)?;
    }
    sink.flush().map_err(|e| failed_to_write(e, span))
}

/// Writes one record and the separator that ends it.
fn write_record(
    sink: &mut impl Write,
    bytes: &[u8],
    separator: &[u8],
    span: Span,
) -> Result<(), ShellError> {
    sink.write_all(bytes)
        .and_then(|()| terminate(sink, bytes, separator))
        .map_err(|e| failed_to_write(e, span))
}

/// The bytes a value already is, or the value back when it has to be serialised first.
///
/// The one place that says what counts as text, for the two questions asked of it: what to do with
/// the item a list starts with, and what to write for each item after that.
fn text_bytes(value: Value) -> Result<Vec<u8>, Value> {
    match value {
        Value::String { val, .. } => Ok(val.into_bytes()),
        Value::Binary { val, .. } => Ok(val.into_owned()),
        other => Err(other),
    }
}

/// Pipes `input` through the `to <format>` command found in the caller's scope.
fn serialise(
    engine: &EngineInterface,
    input: PipelineData,
    format: Option<&str>,
    span: Span,
) -> Result<PipelineData, ShellError> {
    let name = format.ok_or_else(|| {
        ShellError::Generic(
            GenericError::new(
                "no format to write these records with",
                "this input is not text, and nothing says how to serialise it",
                span,
            )
            .with_help(
                "name the format in the file's inner extension (events.jsonl.seek.zst), or pass \
                 --format",
            ),
        )
    })?;
    let command = format!("to {name}");
    let decl = engine.find_decl(command.clone())?.ok_or_else(|| {
        ShellError::Generic(
            GenericError::new(
                format!("`{command}` is not in scope"),
                "no command to serialise the records with",
                span,
            )
            .with_help(
                "pass --format to name another one, or pipe in text. Only nushell's own `to` \
                 commands can be called from a plugin; jsonl and ndjson are written here instead",
            ),
        )
    })?;
    engine.call_decl(decl, EvaluatedCall::new(span), input, true, false)
}

/// Writes the bytes of `input` through, ending them with `separator`.
fn text(
    input: PipelineData,
    separator: &[u8],
    sink: impl Write,
    span: Span,
) -> Result<(), ShellError> {
    let mut sink = Terminating {
        sink,
        separator,
        tail: Vec::with_capacity(separator.len()),
        wrote: false,
    };
    input.write_to(&mut sink)?;
    sink.finish().map_err(|e| failed_to_write(e, span))
}

/// Ends a record: writes `separator` unless `written` already ends with it.
///
/// The one place that says what terminates a record, for the two ways of arriving at one: an item
/// of a list, and the tail of a stream.
fn terminate(sink: &mut impl Write, written: &[u8], separator: &[u8]) -> io::Result<()> {
    if !written.ends_with(separator) {
        sink.write_all(separator)?;
    }
    Ok(())
}

/// A sink whose bytes end with `separator`, keeping only the tail that decides whether they do.
struct Terminating<'a, W> {
    sink: W,
    separator: &'a [u8],
    tail: Vec<u8>,
    wrote: bool,
}

impl<W: Write> Write for Terminating<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.sink.write(buf)?;
        let written = &buf[..n];
        if !written.is_empty() {
            self.wrote = true;
            if written.len() >= self.separator.len() {
                self.tail.clear();
                self.tail
                    .extend_from_slice(&written[written.len() - self.separator.len()..]);
            } else {
                self.tail.extend_from_slice(written);
                let excess = self.tail.len().saturating_sub(self.separator.len());
                self.tail.drain(..excess);
            }
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sink.flush()
    }
}

impl<W: Write> Terminating<'_, W> {
    /// Ends the last record. An input that wrote nothing gets no separator: a file of one empty
    /// record is not what saving nothing means.
    fn finish(mut self) -> io::Result<()> {
        if self.wrote {
            let tail = std::mem::take(&mut self.tail);
            terminate(&mut self.sink, &tail, self.separator)?;
        }
        self.sink.flush()
    }
}

fn not_text(value: &Value, span: Span) -> ShellError {
    ShellError::Generic(
        GenericError::new(
            format!("a {} is not a record", value.get_type()),
            "a list is saved one item per record, so every item has to be text",
            span,
        )
        .with_help("pass --format, or name the format in the file's inner extension"),
    )
}

fn failed_to_write(error: io::Error, span: Span) -> ShellError {
    ShellError::Generic(GenericError::new(
        "cannot write the records",
        error.to_string(),
        span,
    ))
}
