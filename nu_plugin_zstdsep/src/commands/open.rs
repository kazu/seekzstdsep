//! `zstdsep open`: a handle to read records from, or the records themselves.
use nu_plugin::{EngineInterface, EvaluatedCall, PluginCommand};
use nu_protocol::{
    Category, Example, LabeledError, PipelineData, Signature, Spanned, SyntaxShape, Type, Value,
};

use crate::ZstdsepPlugin;
use crate::commands::{resolve, separator};
use crate::decode;
use crate::handle::ZstdsepHandle;
use crate::source::{Format, Source};

pub struct Open;

impl PluginCommand for Open {
    type Plugin = ZstdsepPlugin;

    fn name(&self) -> &str {
        "zstdsep open"
    }

    fn description(&self) -> &str {
        "Open a seekable zstd file for reading records by index."
    }

    fn extra_description(&self) -> &str {
        "Returns a handle. A cell path into it (`get 10`, `$h.10`) decodes its frame up to that \
         record and returns it; nothing else is read. A frame's content checksum is checked only \
         when something decodes all of it.\n\n\
         The engine runs list commands itself and refuses a handle: `first`, `last`, `skip`, \
         `take`, `slice`, `length` and `where` all fail on one. Pass --no-partial to get a plain \
         list stream instead. That reads the whole file, though the engine still drops the stream \
         early for `first n`."
    }

    fn signature(&self) -> Signature {
        Signature::build(self.name())
            .input_output_types(vec![(Type::Nothing, Type::Any)])
            .required("path", SyntaxShape::Filepath, "the file to open")
            .named(
                "separator",
                SyntaxShape::String,
                "the separator records end with (default: a newline)",
                Some('s'),
            )
            .named(
                "format",
                SyntaxShape::String,
                "parse records with `from <format>` instead of the inner extension's",
                Some('f'),
            )
            .switch(
                "raw",
                "return records as strings, parsing nothing",
                Some('r'),
            )
            .switch(
                "no-partial",
                "return every record as a list stream instead of a handle",
                None,
            )
            .category(Category::Formats)
    }

    fn examples(&self) -> Vec<Example<'_>> {
        vec![
            Example {
                example: "let h = zstdsep open events.jsonl.seek.zst; $h.10",
                description: "Decompress one frame and parse record 10 out of it",
                result: None,
            },
            Example {
                example: "zstdsep open events.jsonl.seek.zst --no-partial | where level == error",
                description: "Read the whole file, for the commands that cannot take a handle",
                result: None,
            },
            Example {
                example: "zstdsep open events.log.seek.zst --raw --no-partial | first 3",
                description: "Records as strings, without resolving a `from` command",
                result: None,
            },
        ]
    }

    fn run(
        &self,
        plugin: &Self::Plugin,
        engine: &EngineInterface,
        call: &EvaluatedCall,
        _input: PipelineData,
    ) -> Result<PipelineData, LabeledError> {
        let path: Spanned<String> = call.req(0)?;
        let path = resolve(engine, &path.item)?;
        let separator = separator(call.get_flag("separator")?)?;
        let format = match (call.has_flag("raw")?, call.get_flag::<String>("format")?) {
            (true, _) => Format::Raw,
            (false, Some(name)) => Format::named(&name),
            (false, None) => Format::of_path(&path),
        };
        let source = Source {
            path,
            separator,
            format,
        };

        if call.has_flag("no-partial")? {
            return Ok(decode::stream(engine, &source, call.head)?);
        }

        // Opening here rather than on the first cell path means a missing file or a separator that
        // occurs nowhere is reported by the command the user typed.
        let reader = source.open(call.head)?;
        let id = plugin.register(&source, reader)?;
        Ok(PipelineData::Value(
            Value::custom(Box::new(ZstdsepHandle::new(id, &source)), call.head),
            None,
        ))
    }
}
