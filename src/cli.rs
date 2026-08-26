//! The subcommands, in a form that can be driven without spawning a process.
use std::{
    fs::File,
    io::{self, Read, Seek, Write},
    path::PathBuf,
};

use clap::Parser;
use tempfile::spooled_tempfile;

use crate::seekzstdsep_lib::{
    CompressOptions, ReadSeekable, compress_to_seekable_zst_with_opts,
    convert_to_seekable_zst_reader_with_opts,
};

/// Arguments of the `compress` and `convert` subcommands.
#[derive(Parser, Debug)]
pub struct ConvertArgs {
    /// Input file (default: stdin)
    #[arg(value_name = "INPUT", required = false)]
    input: Option<PathBuf>,
    /// Output file (default: INPUT.seek.zst, or stdout when INPUT is omitted too)
    #[arg(value_name = "OUTPUT", required = false)]
    output: Option<PathBuf>,
    /// Separator string (default: "\\n")
    #[arg(short, long, default_value = "\n")]
    separator: String,
    /// Max frame size in bytes (default: 65536)
    #[arg(long, default_value_t = 65536)]
    frame_size: usize,
    /// Limit multiplier for separator buffer (default: 4)
    #[arg(short, long, default_value = "4")]
    limit_multiplier: Option<usize>,
    #[arg(short, long)]
    /// Count of separators per frame (default: auto-detect). If specified, frames will be split by count of separators instead of frame size. This may result in better compression ratio if the input has many small lines.
    cnt_of_separator_per_frame: Option<usize>,
    /// Remove input file after conversion (only if input is file)
    #[arg(long)]
    rm: bool,
    /// Keep count of separators in frame (default: false). If true, count of sperator in frame is the same in all faramees without final frame.
    #[arg(short, long, default_value_t = true)]
    keep_cnt_of_separators_in_frame: bool,
    /// Leave the 32-bit content checksum out of every frame (default: written)
    #[arg(long)]
    no_check: bool,
}

/// Runs `compress` over `stdin` and `stdout`, which are parameters so a caller can supply its own.
///
/// # Errors
///
/// Whatever compression reports. A destination that cannot be opened panics, as it did when this
/// lived in `main`.
pub fn run_compress(
    args: &ConvertArgs,
    mut stdin: impl Read,
    stdout: impl Write,
) -> anyhow::Result<()> {
    // 入力・出力ファイル名の決定
    let use_stdin = args.input.is_none();
    let _use_stdout = args.output.is_none() && use_stdin;
    let input_path = args.input.clone();
    let output_path = args.output.clone().or_else(|| {
        input_path.as_ref().map(|p| {
            let mut s = p.to_string_lossy().to_string();
            s.push_str(".seek.zst");
            PathBuf::from(s)
        })
    });
    let mut comp_opts = CompressOptions {
        out_path: output_path.clone(),
        checksum: !args.no_check,
        ..Default::default()
    };

    let separator = args.separator.as_bytes();
    // 入力
    let mut input: Box<dyn ReadSeekable>;
    if let Some(ref path) = input_path {
        input = Box::new(File::open(path).expect("failed to open input file"));
    } else {
        let mut spool = spooled_tempfile(1024 * 1024);
        io::copy(&mut stdin, &mut spool)?;
        spool.rewind()?;
        input = Box::new(spool);
    };

    // 出力
    let mut output: Box<dyn Write> = if let Some(ref path) = output_path {
        comp_opts.out_dir = path.parent().map(|p| p.to_path_buf());
        Box::new(File::create(path).expect("failed to create output file"))
    } else {
        Box::new(stdout)
    };
    if args.cnt_of_separator_per_frame.is_some() {
        comp_opts.max_of_separator = args.cnt_of_separator_per_frame;
        convert_to_seekable_zst_reader_with_opts(
            &mut input,
            &mut output,
            args.frame_size,
            args.keep_cnt_of_separators_in_frame,
            separator,
            args.limit_multiplier,
            Some(comp_opts),
        )?;
    } else {
        compress_to_seekable_zst_with_opts(
            &mut input,
            &mut output,
            args.frame_size,
            args.keep_cnt_of_separators_in_frame,
            separator,
            args.limit_multiplier,
            Some(comp_opts),
        )?;
    }
    // --rm
    if args.rm {
        if let Some(ref path) = input_path {
            std::fs::remove_file(path).expect("failed to remove input file");
        } else {
            eprintln!("--rm requires input file");
            std::process::exit(1);
        }
    }

    Ok(())
}
