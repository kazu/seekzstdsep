//! seekzstdsep: generic seekable zst converter with separator support
use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use std::fs::File;
use std::path::PathBuf;

use seekzstdsep::cli::{BoundaryArgs, ConvertArgs, CopyRangeArgs, run_compress, run_copy_range};
use seekzstdsep::find::Boundary;
use seekzstdsep::{
    AppendInput, CompressionLevel, InspectOptions, OnMissingSeparator, RangeCheck, RecordReader,
    append, append_frames_with, append_records_with,
    seekzstdsep_lib::{inspect_records_with_opts, inspect_with_opts},
    truncate, truncate_records,
};
use std::io::{self, Read, Write};

#[derive(Parser)]
#[command(name = "seekzstdsep")]
#[command(about = "Convert text to seekable zst with separator, or inspect zst frames", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Args, Debug)]
struct CatArgs {
    #[arg(value_name = "FILE", required = true)]
    input: Option<PathBuf>,
    #[arg(short, long, required = true, default_value_t = 0)]
    from: usize,
    #[arg(short, long, required = true, default_value_t = 1)]
    cnt: usize,
    #[command(flatten)]
    boundary: BoundaryArgs,
}

#[derive(Args, Debug)]
struct InspectArgs {
    #[arg(value_name = "FILE", required = true)]
    zstfile: PathBuf,
    #[command(flatten)]
    boundary: BoundaryArgs,
    /// Report format: text or json
    #[arg(short, long, default_value = "text")]
    format: String,
    #[arg(short, long, default_value_t = false)]
    no_fast_mode: bool,
}

#[derive(Args, Debug)]
struct TruncateArgs {
    #[arg(value_name = "FILE", required = true)]
    zstfile: PathBuf,
    /// Records to keep: the resulting length, not the number removed. Has to be a multiple of the
    /// records per frame, so the cut lands on a frame boundary
    #[arg(short, long, required = true)]
    records: u64,
    #[command(flatten)]
    boundary: BoundaryArgs,
}

#[derive(Args, Debug)]
struct AppendArgs {
    #[arg(value_name = "FILE", required = true)]
    zstfile: PathBuf,
    /// Records to append (default: stdin)
    #[arg(value_name = "INPUT", required = false)]
    input: Option<PathBuf>,
    #[command(flatten)]
    boundary: BoundaryArgs,
    /// Write a separator at the join when FILE ends in a fragment rather than in a record
    #[arg(long, conflicts_with = "input_seekable")]
    insert_separator: bool,
    /// Treat INPUT as a seekable zst and copy its frames as bytes, decompressing neither file.
    /// Requires both files to hold the same number of records per frame, and FILE to end at a
    /// frame boundary
    #[arg(long, requires = "input")]
    input_seekable: bool,
    /// First record of INPUT to append (default: 0). Has to be the first record of a frame
    #[arg(long, requires = "input_seekable")]
    input_from: Option<u64>,
    /// Records of INPUT to append (default: to the end of INPUT). The record it ends at has to be
    /// the first record of a frame, or the end of INPUT
    #[arg(long, requires = "input_seekable")]
    input_cnt: Option<u64>,
    /// Count the records in every frame being copied rather than in the first one alone, refusing
    /// a frame that holds a count of its own. Decompresses the range, which is the cost the byte
    /// copy exists to avoid
    #[arg(long, requires = "input_seekable")]
    check_input_frames: bool,
    /// Zstandard compression level of the appended frames (default: zstd's default, 3)
    #[arg(long, conflicts_with = "input_seekable")]
    level: Option<i32>,
}

#[derive(Subcommand)]
enum Commands {
    /// Convert text to seekable zst
    Convert(ConvertArgs),
    Compress(ConvertArgs),
    /// Inspect zst file frames
    Inspect(InspectArgs),
    /// Write a range of records to stdout. Reads only the frames the range covers.
    ///
    /// A frame's content checksum is checked only when something decodes all of it, which this
    /// does not: see `docs/bugs.md`.
    Cat(CatArgs),
    /// Shorten a zst file to a record count, in place. Destructive.
    Truncate(TruncateArgs),
    /// Append records to a zst file, in place. Destructive.
    Append(AppendArgs),
    /// Copy a record range out of a zst file into a second one. Reads the input only.
    CopyRange(CopyRangeArgs),
}

use tracing::Level;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

fn setup_logger(level: Level) {
    let filter = EnvFilter::from_default_env() // 環境変数 RUST_LOG があれば優先
        .add_directive(level.into()); // なければコード内の設定を適用

    tracing_subscriber::registry()
        .with(fmt::layer().with_writer(std::io::stderr)) // 標準出力にフォーマットして出す
        .with(filter) // フィルタリングを適用
        .init();
}

/// The records to append: the file that was named, or standard input.
fn records_input(opened: Option<File>) -> Box<dyn Read> {
    match opened {
        Some(f) => Box::new(f),
        None => Box::new(io::stdin().lock()),
    }
}

fn main() -> anyhow::Result<()> {
    let default_level = if cfg!(debug_assertions) {
        Level::TRACE
    } else {
        Level::INFO
    };
    setup_logger(default_level);

    let cli = Cli::parse();
    // MENTION: cat, inspect, truncate and append belong in functions too, as compress now is. A
    // body here can only be reached by spawning the binary.
    match cli.command {
        Commands::Cat(args) => {
            let path = args.input.expect("not found");
            let mut reader = match args.boundary.boundary()? {
                Boundary::Separator(sep) => RecordReader::open(path, &sep)?,
                Boundary::Finder(find) => RecordReader::open_with(path, find)?,
            };
            let mut stdout = io::stdout().lock(); // ロックを取得すると高速
            // 窓ごとに書き出すので、結果全体をメモリに持たない
            reader.records_to(args.from, args.cnt, &mut stdout)?;
            stdout.flush().expect("failed to flush stdout");
        }
        Commands::Convert(args) | Commands::Compress(args) => {
            run_compress(&args, io::stdin().lock(), io::stdout())?;
        }
        Commands::Truncate(args) => {
            let mut file = File::options()
                .read(true)
                .write(true)
                .open(&args.zstfile)
                .with_context(|| format!("failed to open {}", args.zstfile.display()))?;
            match args.boundary.boundary()? {
                Boundary::Separator(sep) => truncate(&mut file, args.records, &sep)?,
                Boundary::Finder(find) => truncate_records(&mut file, args.records, &*find)?,
            }
        }
        Commands::Append(args) => {
            let mut file = File::options()
                .read(true)
                .write(true)
                .open(&args.zstfile)
                .with_context(|| format!("failed to open {}", args.zstfile.display()))?;
            let opened = match args.input {
                Some(ref path) => Some(
                    File::open(path)
                        .with_context(|| format!("failed to open {}", path.display()))?,
                ),
                None => None,
            };

            if args.insert_separator && !args.boundary.is_separator() {
                anyhow::bail!(
                    "--insert-separator writes a separator at the join, so it needs --finder sep"
                );
            }
            let check = if args.check_input_frames {
                RangeCheck::EveryFrame
            } else {
                RangeCheck::FirstFrame
            };
            let on_missing = if args.insert_separator {
                OnMissingSeparator::Insert
            } else {
                OnMissingSeparator::Refuse
            };
            let level = args.level.unwrap_or(CompressionLevel::default());
            let from = args.input_from.unwrap_or(0);
            match args.boundary.boundary()? {
                Boundary::Separator(sep) => {
                    let input: AppendInput<Box<dyn Read>> = if args.input_seekable {
                        AppendInput::Frames {
                            input: opened.as_ref().expect("--input-seekable requires INPUT"),
                            from,
                            cnt: args.input_cnt,
                            check,
                        }
                    } else {
                        AppendInput::Records {
                            data: records_input(opened),
                            on_missing,
                            level,
                        }
                    };
                    append(&mut file, input, &sep)?;
                }
                Boundary::Finder(find) if args.input_seekable => append_frames_with(
                    &mut file,
                    opened.as_ref().expect("--input-seekable requires INPUT"),
                    from,
                    args.input_cnt,
                    &*find,
                    check,
                )?,
                Boundary::Finder(find) => append_records_with(
                    &mut file,
                    records_input(opened),
                    &*find,
                    on_missing,
                    level,
                )?,
            }
        }
        Commands::CopyRange(args) => {
            run_copy_range(&args, io::stdout().lock())?;
        }
        Commands::Inspect(args) => {
            let opts = InspectOptions {
                fast_mode: !args.no_fast_mode,
            };
            let outs = match args.boundary.boundary()? {
                Boundary::Separator(sep) => inspect_with_opts(args.zstfile, &sep, opts)?,
                Boundary::Finder(find) => inspect_records_with_opts(args.zstfile, &*find, opts)?,
            };

            if args.format == "text" {
                outs.iter().for_each(|f| println!("{:?}", f));
            }
            if args.format == "json" {
                let json =
                    serde_json::to_string_pretty(&outs).expect("failed to serialize to json");
                println!("{}", json);
            }
        }
    }
    Ok(())
}
