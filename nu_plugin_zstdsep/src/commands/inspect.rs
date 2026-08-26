//! `zstdsep inspect`: the frame layout, one row per frame.
use nu_plugin::{EngineInterface, EvaluatedCall, PluginCommand};
use nu_protocol::{
    Category, Example, LabeledError, PipelineData, ShellError, Signature, Span, Spanned,
    SyntaxShape, Type, Value, record, shell_error::generic::GenericError,
};
use seekzstdsep::{InspectOptions, seekzstdsep_lib::inspect_with_opts};

use crate::ZstdsepPlugin;
use crate::commands::{resolve, separator};

pub struct Inspect;

impl PluginCommand for Inspect {
    type Plugin = ZstdsepPlugin;

    fn name(&self) -> &str {
        "zstdsep inspect"
    }

    fn description(&self) -> &str {
        "Report the frame layout of a seekable zstd file, one row per frame."
    }

    fn signature(&self) -> Signature {
        Signature::build(self.name())
            .input_output_types(vec![(Type::Nothing, Type::table())])
            .required("path", SyntaxShape::Filepath, "the file to inspect")
            .named(
                "separator",
                SyntaxShape::String,
                "the separator records end with (default: a newline)",
                Some('s'),
            )
            .switch(
                "no-fast-mode",
                "count separators in every frame instead of extrapolating from frame 0",
                None,
            )
            .category(Category::Formats)
    }

    fn examples(&self) -> Vec<Example<'_>> {
        vec![
            Example {
                example: "zstdsep inspect events.jsonl.seek.zst",
                description: "The frames, their sizes and how many records each holds",
                result: None,
            },
            Example {
                example: "zstdsep inspect events.jsonl.seek.zst --no-fast-mode | get records | uniq",
                description: "Check the uniform record count that indexing rests on",
                result: None,
            },
        ]
    }

    fn run(
        &self,
        _plugin: &Self::Plugin,
        engine: &EngineInterface,
        call: &EvaluatedCall,
        _input: PipelineData,
    ) -> Result<PipelineData, LabeledError> {
        let path: Spanned<String> = call.req(0)?;
        let path = resolve(engine, &path.item)?;
        let separator = separator(call.get_flag("separator")?)?;
        let options = InspectOptions {
            fast_mode: !call.has_flag("no-fast-mode")?,
        };

        let frames =
            inspect_with_opts(path.clone(), separator.as_bytes(), options).map_err(|e| {
                ShellError::Generic(GenericError::new(
                    format!("cannot inspect {}", path.display()),
                    e.to_string(),
                    call.head,
                ))
            })?;

        let head = call.head;
        let rows = frames
            .into_iter()
            .enumerate()
            .map(|(index, frame)| {
                Value::record(
                    record! {
                        "index" => Value::int(index as i64, head),
                        "comp_start" => Value::int(frame.comp_start as i64, head),
                        "comp_end" => Value::int(frame.comp_end as i64, head),
                        "comp_size" => filesize(frame.comp_size, head),
                        "decomp_start" => Value::int(frame.decomp_start as i64, head),
                        "decomp_end" => Value::int(frame.decomp_end as i64, head),
                        "decomp_size" => filesize(frame.decomp_size, head),
                        "records" => Value::int(frame.cnt_of_sep as i64, head),
                    },
                    head,
                )
            })
            .collect();

        Ok(PipelineData::Value(Value::list(rows, head), None))
    }
}

fn filesize(bytes: u64, span: Span) -> Value {
    Value::filesize(bytes as i64, span)
}
