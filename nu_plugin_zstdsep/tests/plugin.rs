//! What the plugin does when nushell drives it.
mod common;

use std::path::{Path, PathBuf};

use common::{RECORDS, RECORDS_PER_FRAME, compress_fixture, eval, nu};
use nu_plugin_zstdsep::ZstdsepHandle;
use nu_protocol::Value;
use tempfile::{TempDir, tempdir};

/// A fixture and the nushell that reads it. The directory is returned because deleting it would
/// take the file with it.
fn fixture(name: &str) -> (TempDir, String) {
    let dir = tempdir().expect("Failed to create temp dir");
    let path = compress_fixture(dir.path(), name);
    (dir, path.to_string_lossy().to_string())
}

#[test]
fn inspect_reports_one_row_per_frame() {
    let (_dir, path) = fixture("events.jsonl.seek.zst");
    let mut nu = nu();

    let rows = eval(&mut nu, &format!("zstdsep inspect \"{path}\""))
        .expect("Failed to inspect")
        .into_list()
        .expect("inspect did not return a table");

    let frames = RECORDS.div_ceil(RECORDS_PER_FRAME);
    assert_eq!(rows.len(), frames, "one row per frame");

    let first = rows[0].as_record().expect("a row is not a record").clone();
    assert_eq!(
        first.columns().map(String::as_str).collect::<Vec<_>>(),
        vec![
            "index",
            "comp_start",
            "comp_end",
            "comp_size",
            "decomp_start",
            "decomp_end",
            "decomp_size",
            "records",
        ]
    );
    assert_eq!(
        first.get("records").and_then(|v| v.as_int().ok()),
        Some(RECORDS_PER_FRAME as i64)
    );
    assert!(
        matches!(first.get("comp_size"), Some(Value::Filesize { .. })),
        "sizes are filesizes"
    );
}

#[test]
fn a_handle_describes_itself_by_name() {
    let (_dir, path) = fixture("events.jsonl.seek.zst");
    let mut nu = nu();

    let described = eval(&mut nu, &format!("zstdsep open \"{path}\" | describe"))
        .expect("Failed to open")
        .into_string()
        .expect("describe did not return a string");

    assert_eq!(described, "zstdsep handle");
}

/// Displaying a handle summarises the file. Returning the file itself would materialise it at a
/// prompt, which is the footgun this avoids.
#[test]
fn displaying_a_handle_summarises_the_file() {
    let (_dir, path) = fixture("events.jsonl.seek.zst");
    let mut nu = nu();

    let summary = eval(&mut nu, &format!("(zstdsep open \"{path}\") | columns"))
        .expect("Failed to open")
        .into_list()
        .expect("the summary is not a record");

    let columns: Vec<String> = summary
        .iter()
        .map(|v| v.clone().into_string().expect("not a string"))
        .collect();
    assert_eq!(
        columns,
        vec![
            "path",
            "separator",
            "format",
            "frames",
            "records_per_frame",
            "records"
        ]
    );

    let records = eval(&mut nu, &format!("(zstdsep open \"{path}\").records"))
        .expect("Failed to read the summary")
        .as_int()
        .expect("records is not an integer");
    assert_eq!(records, RECORDS as i64);
}

#[test]
fn a_cell_path_returns_one_parsed_record() {
    let (_dir, path) = fixture("events.jsonl.seek.zst");
    let mut nu = nu();

    for index in [0, 1, RECORDS_PER_FRAME, RECORDS_PER_FRAME + 1, RECORDS - 1] {
        let seq = eval(
            &mut nu,
            &format!("let h = zstdsep open \"{path}\"; $h.{index}.seq"),
        )
        .unwrap_or_else(|e| panic!("Failed to read record {index}: {e}"))
        .as_int()
        .expect("seq is not an integer");
        assert_eq!(seq, index as i64, "record {index} came back as another one");
    }
}

/// The engine follows the rest of the path itself, which it can only do into a value it
/// understands. Parsing in the plugin is what makes that work.
#[test]
fn a_cell_path_continues_into_the_record() {
    let (_dir, path) = fixture("events.jsonl.seek.zst");
    let mut nu = nu();

    let n = eval(&mut nu, &format!("(zstdsep open \"{path}\").7.inner.n"))
        .expect("Failed to follow the path")
        .as_int()
        .expect("n is not an integer");

    assert_eq!(n, 14);
}

#[test]
fn get_is_the_same_as_a_cell_path() {
    let (_dir, path) = fixture("events.jsonl.seek.zst");
    let mut nu = nu();

    let seq = eval(
        &mut nu,
        &format!("zstdsep open \"{path}\" | get 42 | get seq"),
    )
    .expect("Failed to get record 42")
    .as_int()
    .expect("seq is not an integer");

    assert_eq!(seq, 42);
}

#[test]
fn an_index_past_the_last_record_is_an_error_unless_optional() {
    let (_dir, path) = fixture("events.jsonl.seek.zst");
    let mut nu = nu();

    let err = eval(&mut nu, &format!("(zstdsep open \"{path}\").{RECORDS}"))
        .expect_err("reading past the end succeeded");
    assert!(
        err.to_string().to_lowercase().contains("row number"),
        "the failure was not an out-of-range one: {err}"
    );

    let optional = eval(&mut nu, &format!("(zstdsep open \"{path}\").{RECORDS}?"))
        .expect("the optional path failed");
    assert_eq!(optional, Value::test_nothing());
}

#[test]
fn no_partial_returns_every_record() {
    let (_dir, path) = fixture("events.jsonl.seek.zst");
    let mut nu = nu();

    let count = eval(
        &mut nu,
        &format!("zstdsep open \"{path}\" --no-partial | length"),
    )
    .expect("Failed to read the file")
    .as_int()
    .expect("length is not an integer");
    assert_eq!(count, RECORDS as i64);

    let errors = eval(
        &mut nu,
        &format!("zstdsep open \"{path}\" --no-partial | where lvl == error | length"),
    )
    .expect("Failed to filter")
    .as_int()
    .expect("length is not an integer");
    assert_eq!(errors, RECORDS.div_ceil(3) as i64);
}

#[test]
fn raw_returns_the_record_without_its_separator() {
    let (_dir, path) = fixture("events.jsonl.seek.zst");
    let mut nu = nu();

    let text = eval(&mut nu, &format!("(zstdsep open \"{path}\" --raw).0"))
        .expect("Failed to read record 0")
        .into_string()
        .expect("the record is not a string");

    assert_eq!(
        text,
        "{\"seq\":0,\"lvl\":\"error\",\"msg\":\"m0\",\"inner\":{\"n\":0}}"
    );
}

/// The format comes from the extension inside `.seek.zst`. A name that is not json's is left to
/// `from <name>`, which a cell path cannot reach, so the record arrives as a string.
#[test]
fn an_unknown_extension_leaves_records_unparsed_in_a_cell_path() {
    let (_dir, path) = fixture("events.logfmt.seek.zst");
    let mut nu = nu();

    let value =
        eval(&mut nu, &format!("(zstdsep open \"{path}\").0")).expect("Failed to read record 0");

    assert!(
        matches!(value, Value::String { .. }),
        "an unresolvable format parsed anyway: {value:?}"
    );
}

#[test]
fn an_extension_that_is_not_a_format_reads_as_raw() {
    let (_dir, path) = fixture("events.seek.zst");
    let mut nu = nu();

    let format = eval(&mut nu, &format!("(zstdsep open \"{path}\").format"))
        .expect("Failed to read the summary");

    assert_eq!(format, Value::test_nothing());
}

/// An empty separator ends no record, so nothing can be split on it.
///
/// `inspect` is the case that matters: it never builds a `RecordReader`, so the library's own
/// refusal is not behind it. Without this it would report a separator count of one per byte.
#[test]
fn an_empty_separator_is_refused() {
    let (_dir, path) = fixture("events.jsonl.seek.zst");
    let mut nu = nu();

    for command in ["zstdsep open", "zstdsep inspect"] {
        let err = eval(&mut nu, &format!("{command} \"{path}\" --separator ''"))
            .expect_err("an empty separator was accepted");
        assert!(
            err.to_string().to_lowercase().contains("separator"),
            "{command} failed for another reason: {err}"
        );
    }
}

#[test]
fn a_missing_file_is_reported_rather_than_panicking() {
    let (dir, _path) = fixture("events.jsonl.seek.zst");
    let missing = dir.path().join("nope.jsonl.seek.zst");
    let mut nu = nu();

    let err = eval(
        &mut nu,
        &format!("zstdsep open \"{}\"", missing.to_string_lossy()),
    )
    .expect_err("a missing file opened");
    assert!(
        err.to_string().contains("cannot read"),
        "the failure was not the open: {err}"
    );
}

/// A format the plugin does not parse itself is resolved as `from <name>` in the caller's scope,
/// which is what keeps the plugin ignorant of formats.
#[test]
fn a_stream_parses_with_the_from_command_in_scope() {
    let dir = tempdir().expect("Failed to create temp dir");
    let body = "a\tb\n1\t2\n3\t4\n".as_bytes().to_vec();
    let path = common::compress_body(dir.path(), "table.tsv.seek.zst", body);
    let mut nu = nu();

    let b = eval(
        &mut nu,
        &format!(
            "zstdsep open \"{}\" --no-partial | get 1.b",
            path.to_string_lossy()
        ),
    )
    .expect("Failed to read through `from tsv`")
    .as_int()
    .expect("b is not an integer");

    assert_eq!(b, 4);
}

#[test]
fn a_format_with_no_from_command_says_so() {
    let (_dir, path) = fixture("events.jsonl.seek.zst");
    let mut nu = nu();

    let err = eval(
        &mut nu,
        &format!("zstdsep open \"{path}\" --format nope --no-partial | length"),
    )
    .expect_err("an unresolvable format was accepted");

    assert!(
        err.to_string().contains("from nope"),
        "the failure did not name the missing command: {err}"
    );
}

/// A byte flipped in the last frame is only found by a read that reaches it. `first 1` succeeding
/// while `length` fails is what "one frame at a time" looks like from the outside.
#[test]
fn a_stream_reads_only_as_far_as_it_is_asked_to() {
    let (_dir, path) = fixture("events.jsonl.seek.zst");
    let mut nu = nu();

    let last_frame_start = eval(
        &mut nu,
        &format!("zstdsep inspect \"{path}\" | last 1 | get 0.comp_start"),
    )
    .expect("Failed to inspect")
    .as_int()
    .expect("comp_start is not an integer") as usize;

    let mut bytes = std::fs::read(&path).expect("Failed to read the fixture");
    bytes[last_frame_start] ^= 0xff;
    std::fs::write(&path, &bytes).expect("Failed to write the fixture");

    let first = eval(
        &mut nu,
        &format!("zstdsep open \"{path}\" --no-partial | first 1 | get 0.seq"),
    );
    assert!(
        matches!(&first, Ok(v) if v.as_int().ok() == Some(0)),
        "the first record was not readable: {first:?}"
    );

    let all = eval(
        &mut nu,
        &format!("zstdsep open \"{path}\" --no-partial | length"),
    )
    .expect("reading the whole file returned no value");
    assert!(
        all.as_int().is_err() || all.as_int().unwrap() < RECORDS as i64,
        "the corrupted last frame was read as if it were sound: {all:?}"
    );
}

/// Ids repeat across plugin processes, so an entry found under a handle's id may belong to another
/// file. Returning its records would be silent and wrong.
#[test]
fn a_handle_only_matches_the_file_it_was_made_for() {
    let handle = ZstdsepHandle {
        id: 0,
        path: PathBuf::from("/tmp/a.jsonl.seek.zst"),
        separator: "\n".to_string(),
        format: Some("json".to_string()),
    };

    assert!(handle.refers_to(Path::new("/tmp/a.jsonl.seek.zst"), "\n"));
    assert!(
        !handle.refers_to(Path::new("/tmp/b.jsonl.seek.zst"), "\n"),
        "another file matched"
    );
    assert!(
        !handle.refers_to(Path::new("/tmp/a.jsonl.seek.zst"), ";"),
        "another separator matched"
    );
    // The format decides how a record becomes a value, not which bytes are read.
    let raw = ZstdsepHandle {
        format: None,
        ..handle.clone()
    };
    assert!(raw.refers_to(Path::new("/tmp/a.jsonl.seek.zst"), "\n"));
}

/// A file does not record its own separator, so the wrong one is an ordinary mistake. It used to
/// open, report 0 records, and then fail every index with "Row number too large (max: 0)".
#[test]
fn a_separator_that_ends_no_record_is_refused_at_open() {
    let (_dir, path) = fixture("events.jsonl.seek.zst");
    let mut nu = nu();

    let err = eval(&mut nu, &format!("zstdsep open \"{path}\" --separator '|'"))
        .expect_err("a separator that occurs nowhere was accepted");

    assert!(
        err.to_string().contains("no record in"),
        "the failure did not name the separator as the cause: {err}"
    );
}

/// The inner extension names the format, and nothing else does. It is matched as written: a name
/// that is not a format's is still looked for as `from <name>`, and case is not folded.
#[test]
fn the_inner_extension_is_taken_as_written() {
    for (name, format) in [
        ("events.jsonl.seek.zst", Some("json")),
        ("events.ndjson.seek.zst", Some("json")),
        ("events.json.seek.zst", Some("json")),
        ("events.jsonl.zst", Some("json")),
        // Not folded, so this is `from JSONL` rather than the plugin's own parser.
        ("events.JSONL.seek.zst", Some("JSONL")),
        // Nothing says an inner extension names a format.
        ("events.2026.seek.zst", Some("2026")),
        ("events.seek.zst", None),
        ("events.zst", None),
    ] {
        let (_dir, path) = fixture(name);
        let mut nu = nu();

        let got = eval(&mut nu, &format!("(zstdsep open \"{path}\").format"))
            .unwrap_or_else(|e| panic!("{name}: failed to open: {e}"));
        let want = match format {
            Some(f) => Value::test_string(f),
            None => Value::test_nothing(),
        };
        assert_eq!(got, want, "{name} was read as the wrong format");
    }
}
