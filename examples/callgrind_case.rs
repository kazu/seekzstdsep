//! One read case, a fixed number of times, for counting instructions under callgrind.
//!
//! The fixture is built by `gen` in a separate run so that what callgrind counts is the read.
//!
//! ```sh
//! callgrind_case <path> gen
//! valgrind --tool=callgrind callgrind_case <path> records_to/512
//! ```
use std::hint::black_box;
use std::path::PathBuf;

use memchr::memmem::Finder;
use seekzstdsep::seekzstdsep_lib::{cnt_of_separetor_in_frame, seek_table_decomp_frames};
use seekzstdsep::{CompressOptions, RecordReader, compress_to_seekable_zst_with_opts};

const RECORDS: usize = 200_000;
const FRAME_SIZE: usize = 65536;
const SEPARATOR: &[u8] = b"\n";
const OPS: [&str; 8] = [
    "open", "read", "write", "close", "flush", "seek", "stat", "fsync",
];

fn body() -> Vec<u8> {
    let mut body = Vec::new();
    for i in 0..RECORDS {
        body.extend_from_slice(
            format!(
                "{{\"ts\":\"2026-08-24T00:00:{:02}Z\",\"lvl\":\"info\",\"seq\":{i},\"op\":\"{}\",\
                 \"path\":\"/var/log/app.log\",\"took_us\":{},\"msg\":\"done\"}}\n",
                i % 60,
                OPS[i % 8],
                i % 1000
            )
            .as_bytes(),
        );
    }
    body
}

/// The offsets the bench reads from: a stride, so each call starts in a different frame.
fn froms(cnt: usize) -> Vec<usize> {
    (0..40).map(|turn| turn * 7919 % (RECORDS - cnt)).collect()
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = PathBuf::from(args.next().expect("usage: callgrind_case <path> <case>"));
    let case = args.next().expect("usage: callgrind_case <path> <case>");

    if case == "gen" {
        let mut sink = std::io::sink();
        compress_to_seekable_zst_with_opts(
            &mut std::io::Cursor::new(body()),
            &mut sink,
            FRAME_SIZE,
            true,
            SEPARATOR,
            None,
            Some(CompressOptions {
                out_dir: path.parent().map(|dir| dir.to_path_buf()),
                out_path: Some(path.clone()),
                ..Default::default()
            }),
        )
        .expect("failed to compress the fixture");
        return;
    }

    let open = || RecordReader::open(path.clone(), SEPARATOR).expect("no reader");
    match case.as_str() {
        "records_to/512" => {
            let mut reader = open();
            for from in froms(512) {
                reader
                    .records_to(black_box(from), black_box(512), &mut std::io::sink())
                    .expect("failed to read");
            }
        }
        "records_to/as-read/512" => {
            let mut reader = open().verifying();
            for from in froms(512) {
                reader
                    .records_to(black_box(from), black_box(512), &mut std::io::sink())
                    .expect("failed to read");
            }
        }
        "records/512" => {
            let mut reader = open();
            for from in froms(512) {
                black_box(
                    reader
                        .records(black_box(from), black_box(512))
                        .expect("failed to read"),
                );
            }
        }
        "cnt_of_separetor_in_frame" => {
            let finder = Finder::new(SEPARATOR);
            let mut decoder =
                zeekstd::Decoder::new(std::fs::File::open(&path).expect("failed to open"))
                    .expect("no decoder");
            let frames = seek_table_decomp_frames(&decoder).expect("no frames");
            let (start, len) = frames[frames.len() / 2];
            for _ in 0..40 {
                black_box(
                    cnt_of_separetor_in_frame(&mut decoder, start, len, &finder, SEPARATOR)
                        .expect("failed to count"),
                );
            }
        }
        other => panic!("unknown case {other}"),
    }
}
