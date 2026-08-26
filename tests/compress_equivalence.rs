//! The extracted compressor against the one it was extracted from.
//!
//! `old_convert_to_seekable_zst_reader_with_opts` is kept as the implementation was before the
//! buffering and the separator scan were taken out of it, and is the specification the extracted
//! one is measured against here: same bytes out, or the same error, for every input and every
//! combination of arguments below.
//!
//! **Below 2 MiB of records per frame.** The two no longer only differ in structure: the extracted
//! one asks for a frame size policy and the old one does not, so the old one splits a frame whose
//! records exceed zeekstd's default 2 MiB and the extracted one does not. That is the fix
//! `docs/bugs.md` recorded, and the last test here pins it. Everything the sweep and the random
//! rounds ask for stays under that, so the agreement they check is the agreement that is left.
//!
//! Reaching every branch is not claimed. The old implementation is the oracle precisely so that it
//! does not have to be. What is claimed is that the comparison can fail: putting a deliberate
//! difference into the extracted one — the size rule, the count rule, either limit check, the
//! checksum flag, the last frame's end_frame, the chunk end, the `cnt != max` skip — is caught here,
//! one at a time.
//!
//! The extracted stream's own decisions are caught the same way: where it says a record ends, how
//! much it counts as read past that, and how much of a read it keeps. So is the record boundary
//! itself, which the old one spells out rather than taking from `record`, since a copy that shares
//! the rule it measures cannot measure it.
//!
//! Three do not move it. `if is_same_separator_cnt && max_of_separator == -1 { continue }` is
//! redundant with the test after it, which continues for the same input. The drain at the end of
//! the read loop is not reached by anything here — neither a change to the size it triggers at nor
//! one to the stream method only it calls shows up.

use rand::{Rng, SeedableRng, rngs::StdRng};
use seekzstdsep::CompressOptions;
#[allow(deprecated)]
use seekzstdsep::seekzstdsep_lib::{
    new_convert_to_seekable_zst_reader_with_opts, old_convert_to_seekable_zst_reader_with_opts,
};

/// What a run of either compressor produced: the bytes, the error as it reads, or the panic.
type Outcome = Result<Vec<u8>, String>;

/// Runs `f`, turning a panic into an outcome so that the two can be compared on it too.
fn outcome(f: impl FnOnce() -> Outcome) -> Outcome {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(outcome) => outcome,
        Err(panic) => Err(format!(
            "panicked: {}",
            panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default()
        )),
    }
}

#[derive(Clone, Debug)]
struct Args {
    frame_size: usize,
    is_same_separator_cnt: bool,
    separator: Vec<u8>,
    limit_multiplier: Option<usize>,
    opts: Option<CompressOptions>,
}

fn run_old(input: &[u8], args: &Args) -> Outcome {
    outcome(|| {
        let mut out = Vec::new();
        #[allow(deprecated)]
        old_convert_to_seekable_zst_reader_with_opts(
            input,
            &mut out,
            args.frame_size,
            args.is_same_separator_cnt,
            &args.separator,
            args.limit_multiplier,
            args.opts.clone(),
        )
        .map(|()| out)
        .map_err(|e| e.to_string())
    })
}

fn run_new(input: &[u8], args: &Args) -> Outcome {
    outcome(|| {
        let mut out = Vec::new();
        new_convert_to_seekable_zst_reader_with_opts(
            input,
            &mut out,
            args.frame_size,
            args.is_same_separator_cnt,
            &args.separator,
            args.limit_multiplier,
            args.opts.clone(),
        )
        .map(|()| out)
        .map_err(|e| e.to_string())
    })
}

/// Fails the caller unless the two agree, naming the input and the arguments that separated them.
fn assert_agree(label: &str, input: &[u8], args: &Args) {
    let old = run_old(input, args);
    let new = run_new(input, args);
    match (&old, &new) {
        (Ok(a), Ok(b)) => assert_eq!(
            a,
            b,
            "{label}: {} bytes in, {args:?}, produced different output: {} against {} bytes",
            input.len(),
            a.len(),
            b.len()
        ),
        (Err(a), Err(b)) => assert_eq!(
            a,
            b,
            "{label}: {} bytes in, {args:?}, failed differently",
            input.len()
        ),
        _ => panic!(
            "{label}: {} bytes in, {args:?}, one succeeded and the other did not: old = {old:?}, \
             new = {new:?}",
            input.len()
        ),
    }
}

const SEPARATORS: [&[u8]; 4] = [b"\n", b"-=-", b"\n\n", b"\r\n"];

/// Every combination of the arguments that change how frames are cut.
fn argument_sweep() -> Vec<Args> {
    let mut sweep = Vec::new();
    for separator in SEPARATORS {
        for frame_size in [64usize, 1024, 16384, 65536] {
            for is_same_separator_cnt in [true, false] {
                for limit_multiplier in [None, Some(1), Some(4), Some(64)] {
                    for opts in [
                        None,
                        Some(CompressOptions::default()),
                        Some(CompressOptions {
                            checksum: false,
                            ..Default::default()
                        }),
                        Some(CompressOptions {
                            max_of_separator: Some(7),
                            ..Default::default()
                        }),
                    ] {
                        sweep.push(Args {
                            frame_size,
                            is_same_separator_cnt,
                            separator: separator.to_vec(),
                            limit_multiplier,
                            opts,
                        });
                    }
                }
            }
        }
    }
    sweep
}

/// A record of `len` bytes, separator included, distinguishable by `seq`.
fn record(seq: usize, len: usize, separator: &[u8]) -> Vec<u8> {
    let mut body = seq.to_string().into_bytes();
    body.resize(len.saturating_sub(separator.len()), b'x');
    body.extend_from_slice(separator);
    body
}

/// The inputs whose shape is what makes the two implementations able to disagree.
fn corpus(separator: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut inputs = vec![
        ("empty".into(), Vec::new()),
        ("no separator at all".into(), vec![b'x'; 5000]),
        (
            "shorter than one read".into(),
            (0..3).flat_map(|i| record(i, 40, separator)).collect(),
        ),
        (
            "one record longer than the buffer".into(),
            record(0, 100_000, separator),
        ),
    ];

    let whole: Vec<u8> = (0..400).flat_map(|i| record(i, 137, separator)).collect();
    inputs.push(("ends with a separator".into(), whole.clone()));

    let mut fragment = whole.clone();
    fragment.extend_from_slice(b"a fragment");
    inputs.push(("ends in a fragment".into(), fragment));

    // The separator lands across the 32768-byte boundary the compressor reads at.
    let len = 113;
    let straddling: Vec<u8> = (0..600).flat_map(|i| record(i, len, separator)).collect();
    inputs.push(("across a read boundary".into(), straddling));

    // Records of wildly differing lengths, which is what makes the count derived from the first
    // frame stop fitting.
    let mut rng = StdRng::seed_from_u64(20_260_824);
    let mut uneven = Vec::new();
    for i in 0..300 {
        uneven.extend_from_slice(&record(i, rng.random_range(8..4000), separator));
    }
    inputs.push(("uneven records".into(), uneven));

    inputs
}

#[test]
fn test_the_extracted_compressor_agrees_over_the_argument_sweep() {
    for args in argument_sweep() {
        for (label, input) in corpus(&args.separator) {
            assert_agree(&label, &input, &args);
        }
    }
}

#[test]
fn test_the_extracted_compressor_agrees_over_random_input() {
    let mut rng = StdRng::seed_from_u64(0x5EEC_2025);
    for round in 0..200 {
        let separator = SEPARATORS[rng.random_range(0..SEPARATORS.len())];
        let mut input = Vec::new();
        for i in 0..rng.random_range(0..400usize) {
            input.extend_from_slice(&record(i, rng.random_range(1..900), separator));
        }
        if rng.random_bool(0.25) {
            input.extend_from_slice(b"trailing fragment");
        }
        let args = Args {
            frame_size: 1 << rng.random_range(5..17),
            is_same_separator_cnt: rng.random_bool(0.5),
            separator: separator.to_vec(),
            limit_multiplier: match rng.random_range(0..4) {
                0 => None,
                n => Some(n),
            },
            opts: if rng.random_bool(0.5) {
                None
            } else {
                Some(CompressOptions {
                    max_of_separator: rng.random_bool(0.5).then(|| rng.random_range(1..20usize)),
                    checksum: rng.random_bool(0.5),
                    ..Default::default()
                })
            },
        };
        assert_agree(&format!("round {round}"), &input, &args);
    }
}

#[test]
fn test_the_extracted_compressor_agrees_on_an_empty_separator() {
    // Refused by both, but the refusal is not the only thing that has to match: what runs before
    // it does too. A frame size whose product with the multiplier does not fit says so.
    for frame_size in [64usize, 65536, usize::MAX / 2] {
        let args = Args {
            frame_size,
            is_same_separator_cnt: true,
            separator: Vec::new(),
            limit_multiplier: Some(4),
            opts: None,
        };
        assert_agree("empty separator", b"a\nb\n", &args);
    }
}

#[test]
fn test_the_extracted_compressor_stops_splitting_a_frame_at_two_mib() {
    use zeekstd::SeekTable;

    // Seven records of 400 KB in one frame: 2.8 MB, which the default frame size policy cuts at
    // 2 MiB wherever that lands.
    let sep: &[u8] = b"\n";
    let input: Vec<u8> = (0..7).flat_map(|i| record(i, 400_000, sep)).collect();
    let args = Args {
        frame_size: 65536,
        is_same_separator_cnt: true,
        separator: sep.to_vec(),
        limit_multiplier: Some(64),
        opts: Some(CompressOptions {
            max_of_separator: Some(7),
            ..Default::default()
        }),
    };

    let frames = |out: Vec<u8>| {
        SeekTable::from_seekable(&mut std::io::Cursor::new(out))
            .expect("no seek table")
            .num_frames()
    };
    let old = frames(run_old(&input, &args).expect("the old compressor failed"));
    let new = frames(run_new(&input, &args).expect("the extracted compressor failed"));

    assert!(
        old > 1,
        "the old compressor kept the records in one frame, so there is nothing here to have fixed"
    );
    assert_eq!(
        new, 1,
        "the extracted compressor split a frame of seven records, which is the 2 MiB default it is \
         meant to have turned off"
    );
}
