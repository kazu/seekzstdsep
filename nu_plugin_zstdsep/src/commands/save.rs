//! `zstdsep save`: write records to a `.seek.zst` file, or add them to one.
use std::fs::File;
use std::io::Seek;
use std::path::{Path, PathBuf};

use nu_plugin::{EngineInterface, EvaluatedCall, PluginCommand};
use nu_protocol::{
    Category, Example, LabeledError, PipelineData, ShellError, Signature, Spanned, SyntaxShape,
    Type, shell_error::generic::GenericError,
};
use seekzstdsep::{
    CompressOptions, OnMissingSeparator, append_records, compress_to_seekable_zst_with_opts,
    convert_to_seekable_zst_reader_with_opts,
};
use tempfile::spooled_tempfile;

use crate::ZstdsepPlugin;
use crate::commands::{resolve, separator};
use crate::encode;
use crate::source;

/// Bytes held in memory before the staged records spill to a file.
const SPOOL_LIMIT: usize = 1024 * 1024;

/// The compressor's defaults, which are the CLI's.
const FRAME_SIZE: i64 = 65536;
const LIMIT_MULTIPLIER: i64 = 4;

pub struct Save;

impl PluginCommand for Save {
    type Plugin = ZstdsepPlugin;

    fn name(&self) -> &str {
        "zstdsep save"
    }

    fn description(&self) -> &str {
        "Write the input to a seekable zstd file as separator-terminated records."
    }

    fn extra_description(&self) -> &str {
        "Text is written as it came. Anything structured is piped through the `to <format>` \
         command named by the file's inner extension (`events.jsonl.seek.zst` uses `to jsonl`), \
         which --format overrides and --raw refuses; a list of strings is one record per item \
         either way.\n\n\
         --append adds to an existing file rather than writing a new one. It is the counterpart of \
         `zstdsep open`: what --separator says here is what has to be said there."
    }

    fn signature(&self) -> Signature {
        Signature::build(self.name())
            .input_output_types(vec![(Type::Any, Type::Nothing)])
            .required("path", SyntaxShape::Filepath, "the file to write")
            .switch(
                "append",
                "add the records to an existing file instead of writing a new one",
                Some('a'),
            )
            .switch("force", "overwrite an existing file", Some('f'))
            .named(
                "separator",
                SyntaxShape::String,
                "the separator to end records with (default: a newline)",
                Some('s'),
            )
            .named(
                "format",
                SyntaxShape::String,
                "serialise records with `to <format>` instead of the inner extension's",
                None,
            )
            .switch(
                "raw",
                "write the input as it is, serialising nothing",
                Some('r'),
            )
            .switch(
                "insert-separator",
                "with --append, close a file that ends in a fragment before adding to it",
                None,
            )
            .named(
                "frame-size",
                SyntaxShape::Int,
                "target size of a frame in bytes (default: 65536)",
                None,
            )
            .named(
                "records-per-frame",
                SyntaxShape::Int,
                "records per frame, instead of deriving it from --frame-size",
                None,
            )
            .named(
                "limit-multiplier",
                SyntaxShape::Int,
                "how much of a frame the separator search may buffer (default: 4)",
                None,
            )
            .switch(
                "no-check",
                "leave the 32-bit content checksum out of every frame",
                None,
            )
            .category(Category::Formats)
    }

    fn examples(&self) -> Vec<Example<'_>> {
        vec![
            Example {
                example: "ls | zstdsep save listing.jsonl.seek.zst",
                description: "Serialise a table with `to jsonl` and compress it",
                result: None,
            },
            Example {
                example: "open access.log | zstdsep save access.log.seek.zst",
                description: "Compress text as it is, one record per line",
                result: None,
            },
            Example {
                example: "$new | zstdsep save --append events.jsonl.seek.zst",
                description: "Add records to a file that already holds some",
                result: None,
            },
        ]
    }

    fn run(
        &self,
        _plugin: &Self::Plugin,
        engine: &EngineInterface,
        call: &EvaluatedCall,
        input: PipelineData,
    ) -> Result<PipelineData, LabeledError> {
        let path: Spanned<String> = call.req(0)?;
        let path = resolve(engine, &path.item)?;
        let separator = separator(call.get_flag("separator")?)?;
        let appending = call.has_flag("append")?;

        let format = match (call.has_flag("raw")?, call.get_flag::<String>("format")?) {
            (true, _) => None,
            (false, Some(name)) => Some(name),
            (false, None) => source::inner_extension(&path),
        };

        if appending && call.has_flag("force")? {
            return Err(ShellError::Generic(GenericError::new(
                "--append and --force cannot be given together",
                "one adds to the file, the other replaces it",
                call.head,
            ))
            .into());
        }
        check_destination(&path, appending, call)?;

        // Staged rather than streamed because compression needs to seek over the records, and
        // because a file the caller cannot serialise should not have been created by then.
        let mut records = spooled_tempfile(SPOOL_LIMIT);
        encode::records(
            engine,
            input,
            format.as_deref(),
            separator.as_bytes(),
            call.head,
            &mut records,
        )?;
        records.rewind().map_err(|e| io_failed(&path, &e, call))?;

        if appending {
            let on_missing = if call.has_flag("insert-separator")? {
                OnMissingSeparator::Insert
            } else {
                OnMissingSeparator::Refuse
            };
            let mut file = File::options()
                .read(true)
                .write(true)
                .open(&path)
                .map_err(|e| io_failed(&path, &e, call))?;
            append_records(&mut file, records, separator.as_bytes(), on_missing)
                .map_err(|e| failed(&path, &e.to_string(), call))?;
        } else {
            compress(&path, &mut records, &separator, call)?;
        }

        Ok(PipelineData::Empty)
    }
}

/// Refuses a destination before anything is read: an existing file that nothing says to replace,
/// and a missing one there is nothing to add to.
fn check_destination(path: &Path, appending: bool, call: &EvaluatedCall) -> Result<(), ShellError> {
    match (appending, path.exists()) {
        (true, false) => Err(ShellError::Generic(
            GenericError::new(
                format!("{} does not exist", path.display()),
                "--append adds to a file's records; there are none here",
                call.head,
            )
            .with_help("write the file first, without --append"),
        )),
        (false, true) if !call.has_flag("force")? => Err(ShellError::Generic(
            GenericError::new(
                format!("{} already exists", path.display()),
                "this would replace a file",
                call.head,
            )
            .with_help("pass --force to overwrite it, or --append to add to its records"),
        )),
        _ => Ok(()),
    }
}

/// Compresses the staged records into `path`.
fn compress(
    path: &Path,
    records: &mut (impl std::io::Read + Seek),
    separator: &str,
    call: &EvaluatedCall,
) -> Result<(), ShellError> {
    let frame_size = call.get_flag::<i64>("frame-size")?.unwrap_or(FRAME_SIZE) as usize;
    let limit_multiplier = call
        .get_flag::<i64>("limit-multiplier")?
        .unwrap_or(LIMIT_MULTIPLIER) as usize;
    let records_per_frame = call
        .get_flag::<i64>("records-per-frame")?
        .map(|n| n as usize);
    let options = CompressOptions {
        max_of_separator: records_per_frame,
        out_dir: path.parent().map(PathBuf::from),
        out_path: Some(path.to_path_buf()),
        checksum: !call.has_flag("no-check")?,
    };
    // The output file is opened here and handed over as well: the library delivers to `out_path`
    // by reflink and falls back to writing this handle when the filesystem has no reflink.
    let out = File::create(path).map_err(|e| io_failed(path, &e, call))?;

    // A uniform separator count per frame is what locating a record by index relies on, so it is
    // not a choice the caller gets to make.
    let uniform_records_per_frame = true;
    let result = if records_per_frame.is_some() {
        convert_to_seekable_zst_reader_with_opts(
            records,
            out,
            frame_size,
            uniform_records_per_frame,
            separator.as_bytes(),
            Some(limit_multiplier),
            Some(options),
        )
    } else {
        compress_to_seekable_zst_with_opts(
            records,
            out,
            frame_size,
            uniform_records_per_frame,
            separator.as_bytes(),
            Some(limit_multiplier),
            Some(options),
        )
    };
    result.map_err(|e| failed(path, &e.to_string(), call))
}

fn io_failed(path: &Path, error: &std::io::Error, call: &EvaluatedCall) -> ShellError {
    failed(path, &error.to_string(), call)
}

fn failed(path: &Path, why: &str, call: &EvaluatedCall) -> ShellError {
    ShellError::Generic(GenericError::new(
        format!("cannot write {}", path.display()),
        why.to_string(),
        call.head,
    ))
}
