//! seekzstdsep: generic seekable zst converter with separator support
use clap::{Args, Parser, Subcommand};
use std::fs::File;
use std::path::PathBuf;

use seekzstdsep::cli::{ConvertArgs, CopyRangeArgs, run_compress, run_copy_range};
use seekzstdsep::{
    AppendInput, InspectOptions, OnMissingSeparator, append, cat_data,
    seekzstdsep_lib::inspect_with_opts, truncate,
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
    #[arg(short, long, default_value = "\n")]
    separator: String,
}

#[derive(Args, Debug)]
struct InspectArgs {
    #[arg(value_name = "FILE", required = true)]
    zstfile: PathBuf,
    #[arg(short, long, default_value = "\n")]
    separator: String,
    #[arg(short, long, default_value = "text")]
    format: String,
    #[arg(short, long, default_value_t = false)]
    no_fast_mode: bool,
}

#[derive(Args, Debug)]
struct TruncateArgs {
    #[arg(value_name = "FILE", required = true)]
    zstfile: PathBuf,
    /// Records to keep. The resulting length, not the number removed.
    #[arg(short, long, required = true)]
    records: u64,
    #[arg(short, long, default_value = "\n")]
    separator: String,
}

#[derive(Args, Debug)]
struct AppendArgs {
    #[arg(value_name = "FILE", required = true)]
    zstfile: PathBuf,
    /// Records to append (default: stdin)
    #[arg(value_name = "INPUT", required = false)]
    input: Option<PathBuf>,
    #[arg(short, long, default_value = "\n")]
    separator: String,
    /// Write a separator at the join when FILE ends in a fragment rather than in a record
    #[arg(long)]
    insert_separator: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Convert text to seekable zst
    Convert(ConvertArgs),
    Compress(ConvertArgs),
    /// Inspect zst file frames
    Inspect(InspectArgs),
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
            let outs = cat_data(
                args.input.expect("not found"),
                args.from,
                args.cnt,
                args.separator.as_bytes(),
            )?;

            let mut stdout = io::stdout().lock(); // ロックを取得すると高速
            stdout.write_all(&outs).expect("write fail"); // バイナリをそのまま書き出す
            stdout.flush().expect("failed to flush stdout");
        }
        Commands::Convert(args) | Commands::Compress(args) => {
            run_compress(&args, io::stdin().lock(), io::stdout())?;
        }
        Commands::Truncate(args) => {
            let mut file = File::options().read(true).write(true).open(&args.zstfile)?;
            truncate(&mut file, args.records, args.separator.as_bytes())?;
        }
        Commands::Append(args) => {
            let mut file = File::options().read(true).write(true).open(&args.zstfile)?;
            let data: Box<dyn Read> = match args.input {
                Some(ref path) => Box::new(File::open(path)?),
                None => Box::new(io::stdin().lock()),
            };
            let on_missing = if args.insert_separator {
                OnMissingSeparator::Insert
            } else {
                OnMissingSeparator::Refuse
            };
            append(
                &mut file,
                AppendInput::Records { data, on_missing },
                args.separator.as_bytes(),
            )?;
        }
        Commands::CopyRange(args) => {
            run_copy_range(&args, io::stdout().lock())?;
        }
        Commands::Inspect(args) => {
            let outs = inspect_with_opts(
                args.zstfile,
                args.separator.as_bytes(),
                InspectOptions {
                    fast_mode: !args.no_fast_mode,
                },
            )
            .expect("failed to inspect zst file");

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
