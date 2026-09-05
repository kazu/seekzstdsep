//! The subcommands, in a form that can be driven without spawning a process.
use std::{
    fs::File,
    io::{self, Read, Seek, Write},
    path::{Path, PathBuf},
};

use anyhow::Context;
use clap::Parser;
use tempfile::spooled_tempfile;

use crate::edit::{Alignment, SeparatorCheck, copy_range, copy_range_with};
use crate::find::{self, Boundary};
use crate::seekzstdsep_lib::{
    CompressOptions, CompressionLevel, ReadSeekable, compress_records_to_seekable_zst_with_opts,
    compress_to_seekable_zst_with_opts, convert_records_to_seekable_zst_reader_with_opts,
    convert_to_seekable_zst_reader_with_opts,
};

/// Where a record ends, as the command line names it.
///
/// Flattened into every subcommand that has to know, so that the three flags are spelled once.
#[derive(clap::Args, Debug, Clone)]
pub struct BoundaryArgs {
    /// Record format: sep, fixed, flatbuffers or msgpack
    #[arg(long, default_value = "sep")]
    finder: String,
    /// What the finder is configured with: the bytes a record ends with for `sep`, the record
    /// length for `fixed`. Taken as given, with no escape processing
    #[arg(long, conflicts_with = "separator")]
    finder_arg: Option<String>,
    /// Separator string, which is `--finder sep --finder-arg <SEPARATOR>` (default: "\\n")
    #[arg(short, long)]
    separator: Option<String>,
}

impl BoundaryArgs {
    /// What the flags name.
    ///
    /// # Errors
    ///
    /// A `--separator` given alongside a finder that is not `sep`, along with whatever
    /// [`find::from_spec`] refuses.
    pub fn boundary(&self) -> anyhow::Result<Boundary> {
        if self.separator.is_some() && self.finder != "sep" {
            anyhow::bail!(
                "--separator is --finder sep --finder-arg, so it cannot be given with --finder {}",
                self.finder
            );
        }
        find::from_spec(
            &self.finder,
            self.finder_arg.as_deref().or(self.separator.as_deref()),
        )
    }

    /// Whether the boundary is a separator, which is what `append --insert-separator` needs it to
    /// be.
    pub fn is_separator(&self) -> bool {
        self.finder == "sep"
    }
}

/// Arguments of the `copy-range` subcommand.
#[derive(Parser, Debug)]
pub struct CopyRangeArgs {
    /// Input file
    #[arg(value_name = "INPUT")]
    input: PathBuf,
    /// Output file, or `-` for stdout
    #[arg(value_name = "OUTPUT")]
    output: PathBuf,
    /// First record to copy. Has to be the first record of a frame.
    #[arg(short, long)]
    from: u64,
    /// Records to copy (default: to the end of the file). The record it ends at has to be the
    /// first record of a frame, or the end of the file.
    #[arg(short, long)]
    cnt: Option<u64>,
    #[command(flatten)]
    boundary: BoundaryArgs,
    /// Copy a final frame that holds a different number of records than the rest, leaving a result
    /// that cannot be joined onto another file
    #[arg(long)]
    no_align: bool,
    /// Count a second frame as well, and refuse when the two hold different record counts. Costs
    /// one more frame decompressed
    #[arg(long)]
    check_uniform: bool,
}

/// The output file, created by the first write rather than when it is named.
///
/// Every refusal [`copy_range`] makes comes before the first byte it writes, so a refused copy
/// leaves whatever is at the path alone. Creating it up front would empty it instead.
struct LazyFile<'a> {
    path: &'a Path,
    file: Option<File>,
}

impl Write for LazyFile<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let file = match self.file {
            Some(ref mut file) => file,
            None => self.file.insert(File::create(self.path).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("failed to create {}: {e}", self.path.display()),
                )
            })?),
        };
        file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.file {
            Some(ref mut file) => file.flush(),
            None => Ok(()),
        }
    }
}

/// Runs `copy-range` over `stdout`, which is a parameter so a caller can supply its own.
///
/// # Errors
///
/// An input that cannot be opened or an output that cannot be created, along with whatever
/// [`copy_range`] refuses.
pub fn run_copy_range(args: &CopyRangeArgs, stdout: impl Write) -> anyhow::Result<()> {
    let input = File::open(&args.input)
        .with_context(|| format!("failed to open {}", args.input.display()))?;
    let mut output: Box<dyn Write> = if args.output == Path::new("-") {
        Box::new(stdout)
    } else {
        Box::new(LazyFile {
            path: &args.output,
            file: None,
        })
    };

    let align = if args.no_align {
        Alignment::NotRequired
    } else {
        Alignment::Required
    };
    let check = if args.check_uniform {
        SeparatorCheck::TwoFrames
    } else {
        SeparatorCheck::FirstFrame
    };
    match args.boundary.boundary()? {
        Boundary::Separator(sep) => {
            copy_range(&input, &mut output, args.from, args.cnt, &sep, align, check)
        }
        Boundary::Finder(find) => copy_range_with(
            &input,
            &mut output,
            args.from,
            args.cnt,
            &*find,
            align,
            check,
        ),
    }
}

/// Arguments of the `compress` and `convert` subcommands.
#[derive(Parser, Debug)]
pub struct ConvertArgs {
    /// Input file (default: stdin)
    #[arg(value_name = "INPUT", required = false)]
    input: Option<PathBuf>,
    /// Output file (default: INPUT.seek.zst, or stdout when INPUT is omitted too)
    #[arg(value_name = "OUTPUT", required = false)]
    output: Option<PathBuf>,
    #[command(flatten)]
    boundary: BoundaryArgs,
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
    /// Zstandard compression level (default: zstd's default, 3)
    #[arg(long)]
    level: Option<i32>,
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
        level: args.level.unwrap_or(CompressionLevel::default()),
        ..Default::default()
    };

    let boundary = args.boundary.boundary()?;
    // 入力
    let mut input: Box<dyn ReadSeekable>;
    if let Some(ref path) = input_path {
        input = Box::new(
            File::open(path).with_context(|| format!("failed to open {}", path.display()))?,
        );
    } else {
        let mut spool = spooled_tempfile(1024 * 1024);
        io::copy(&mut stdin, &mut spool)?;
        spool.rewind()?;
        input = Box::new(spool);
    };

    // 出力
    let mut output: Box<dyn Write> = if let Some(ref path) = output_path {
        comp_opts.out_dir = path.parent().map(|p| p.to_path_buf());
        Box::new(
            File::create(path).with_context(|| format!("failed to create {}", path.display()))?,
        )
    } else {
        Box::new(stdout)
    };
    // A count per frame given on the command line is the framing, so there is nothing for the
    // retry to derive and the conversion runs once.
    let converts = args.cnt_of_separator_per_frame.is_some();
    if converts {
        comp_opts.max_of_separator = args.cnt_of_separator_per_frame;
    }
    match (boundary, converts) {
        (Boundary::Separator(sep), true) => convert_to_seekable_zst_reader_with_opts(
            &mut input,
            &mut output,
            args.frame_size,
            args.keep_cnt_of_separators_in_frame,
            &sep,
            args.limit_multiplier,
            Some(comp_opts),
        )?,
        (Boundary::Separator(sep), false) => compress_to_seekable_zst_with_opts(
            &mut input,
            &mut output,
            args.frame_size,
            args.keep_cnt_of_separators_in_frame,
            &sep,
            args.limit_multiplier,
            Some(comp_opts),
        )?,
        (Boundary::Finder(find), true) => convert_records_to_seekable_zst_reader_with_opts(
            &mut input,
            &mut output,
            args.frame_size,
            args.keep_cnt_of_separators_in_frame,
            &*find,
            args.limit_multiplier,
            Some(comp_opts),
        )?,
        (Boundary::Finder(find), false) => compress_records_to_seekable_zst_with_opts(
            &mut input,
            &mut output,
            args.frame_size,
            args.keep_cnt_of_separators_in_frame,
            &*find,
            args.limit_multiplier,
            Some(comp_opts),
        )?,
    }
    // --rm
    if args.rm {
        if let Some(ref path) = input_path {
            std::fs::remove_file(path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
        } else {
            eprintln!("--rm requires input file");
            std::process::exit(1);
        }
    }

    Ok(())
}
