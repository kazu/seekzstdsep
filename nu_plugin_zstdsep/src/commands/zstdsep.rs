//! `zstdsep` on its own, which nushell requires a command for whenever subcommands exist.
use nu_plugin::{EngineInterface, EvaluatedCall, PluginCommand};
use nu_protocol::{Category, LabeledError, PipelineData, Signature, Type};

use crate::ZstdsepPlugin;

pub struct Zstdsep;

impl PluginCommand for Zstdsep {
    type Plugin = ZstdsepPlugin;

    fn name(&self) -> &str {
        "zstdsep"
    }

    fn description(&self) -> &str {
        "Read separator-aware seekable zstd files."
    }

    fn signature(&self) -> Signature {
        Signature::build(self.name())
            .input_output_types(vec![(Type::Nothing, Type::String)])
            .category(Category::Formats)
    }

    fn run(
        &self,
        _plugin: &Self::Plugin,
        engine: &EngineInterface,
        call: &EvaluatedCall,
        _input: PipelineData,
    ) -> Result<PipelineData, LabeledError> {
        Ok(PipelineData::Value(
            nu_protocol::Value::string(engine.get_help()?, call.head),
            None,
        ))
    }
}
