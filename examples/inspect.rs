//! Dump the frame layout of a compressed file and check the uniform-separator-count invariant.
//!
//! ```sh
//! cargo run --example inspect -- events.jsonl.seek.zst
//! ```
//!
//! Runs with `fast_mode: false` so every frame is actually counted. Fast mode extrapolates from the
//! first frame, which means it assumes the very invariant being checked here.

use std::path::PathBuf;

use seekzstdsep::{InspectOptions, seekzstdsep_lib::inspect_with_opts};

fn main() -> anyhow::Result<()> {
    let path = PathBuf::from(std::env::args().nth(1).expect("usage: inspect <FILE>"));

    let frames = inspect_with_opts(path, b"\n", InspectOptions { fast_mode: false })?;

    println!(
        "{:>5}  {:>10}  {:>12}  {:>8}",
        "frame", "comp", "decomp", "records"
    );
    for (i, f) in frames.iter().enumerate() {
        println!(
            "{:>5}  {:>10}  {:>12}  {:>8}",
            i, f.comp_size, f.decomp_size, f.cnt_of_sep
        );
    }

    // The invariant permits one partial frame at the end, plus the empty trailing frame that
    // zeekstd's encoder sometimes emits. Everything before that must share a single record count.
    let body: Vec<usize> = frames
        .iter()
        .filter(|f| f.decomp_size > 0)
        .map(|f| f.cnt_of_sep)
        .collect();

    match body.split_last() {
        None => println!("\nno data frames"),
        Some((last, head)) => {
            let expected = head.first().copied().unwrap_or(*last);
            let violations: Vec<usize> = (0..head.len()).filter(|&i| head[i] != expected).collect();

            println!(
                "\n{} data frames, {} records per frame",
                body.len(),
                expected
            );
            if violations.is_empty() {
                let total: usize = body.iter().sum();
                println!("invariant holds (final frame: {last} records, {total} records total)");
            } else {
                println!(
                    "INVARIANT VIOLATED in {} interior frame(s): {:?}",
                    violations.len(),
                    &violations[..violations.len().min(10)]
                );
                println!("record lookup via `cat` will return wrong records for this file");
            }
        }
    }

    Ok(())
}
