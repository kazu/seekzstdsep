//! What `zstdsep save` writes, and what it refuses to write.
mod common;

use std::path::PathBuf;

use common::{eval, nu};
use nu_plugin_test_support::PluginTest;
use tempfile::{TempDir, tempdir};

/// A directory to write into, and the nushell that writes there. The directory is returned because
/// deleting it would take the file with it.
fn target(name: &str) -> (TempDir, String, PluginTest) {
    let dir = tempdir().expect("Failed to create temp dir");
    let path = dir.path().join(name).to_string_lossy().to_string();
    (dir, path, nu())
}

/// The records a file holds, as strings, read back through the plugin's own reader.
fn records(nu: &mut PluginTest, path: &str) -> Vec<String> {
    records_with(nu, path, "")
}

/// The same, for a file that needs `flags` (a separator of its own) to be taken apart.
fn records_with(nu: &mut PluginTest, path: &str, flags: &str) -> Vec<String> {
    eval(
        nu,
        &format!("zstdsep open \"{path}\" --raw --no-partial {flags}"),
    )
    .expect("Failed to read the file back")
    .into_list()
    .expect("the records did not come back as a list")
    .into_iter()
    .map(|v| v.into_string().expect("a record is not a string"))
    .collect()
}

#[test]
fn a_table_is_written_through_the_extension_s_to_command() {
    let (_dir, path, mut nu) = target("rows.tsv.seek.zst");

    eval(
        &mut nu,
        &format!("[[a, b]; [1, x], [2, y]] | zstdsep save \"{path}\""),
    )
    .expect("Failed to save");

    let back = eval(&mut nu, &format!("zstdsep open \"{path}\" --no-partial"))
        .expect("Failed to read the file back")
        .into_list()
        .expect("the file did not come back as a table");
    assert_eq!(back.len(), 2, "one row per record, the header consumed");
    assert_eq!(
        back[1]
            .get_data_by_key("b")
            .and_then(|v| v.into_string().ok()),
        Some("y".to_string())
    );
}

#[test]
fn a_string_is_written_unchanged() {
    let (_dir, path, mut nu) = target("lines.seek.zst");

    eval(&mut nu, &format!("\"a\nb\nc\n\" | zstdsep save \"{path}\"")).expect("Failed to save");

    assert_eq!(records(&mut nu, &path), vec!["a", "b", "c"]);
}

/// A file whose last record has no separator is one that `save --append` refuses later, so the
/// separator is written even when the input ends without one.
#[test]
fn the_last_record_is_terminated() {
    let (_dir, path, mut nu) = target("lines.seek.zst");

    eval(
        &mut nu,
        &format!("\"a\nb\nc\nd\" | zstdsep save --records-per-frame 1 \"{path}\""),
    )
    .expect("Failed to save");

    assert_eq!(records(&mut nu, &path), vec!["a", "b", "c", "d"]);
    eval(&mut nu, &format!("\"e\n\" | zstdsep save -a \"{path}\""))
        .expect("Failed to append to the file it wrote");
    assert_eq!(records(&mut nu, &path), vec!["a", "b", "c", "d", "e"]);
}

#[test]
fn a_list_of_strings_is_one_record_each() {
    let (_dir, path, mut nu) = target("lines.seek.zst");

    eval(&mut nu, &format!("[a b c] | zstdsep save \"{path}\"")).expect("Failed to save");

    assert_eq!(records(&mut nu, &path), vec!["a", "b", "c"]);
}

/// Nothing names how to serialise a record, and guessing one would write a file the reader cannot
/// take apart again.
#[test]
fn a_table_without_a_format_is_refused() {
    let (_dir, path, mut nu) = target("rows.seek.zst");

    let err = eval(
        &mut nu,
        &format!("[[a, b]; [1, x]] | zstdsep save \"{path}\""),
    )
    .expect_err("a table was saved without a format");
    assert!(
        err.to_string().contains("format"),
        "the failure did not name the missing format: {err}"
    );
    assert!(
        !PathBuf::from(&path).exists(),
        "a refused save wrote a file"
    );
}

#[test]
fn a_format_with_no_to_command_in_scope_is_refused() {
    let (_dir, path, mut nu) = target("rows.logfmt.seek.zst");

    let err = eval(
        &mut nu,
        &format!("[[a, b]; [1, x]] | zstdsep save \"{path}\""),
    )
    .expect_err("a table was saved with no command to serialise it");
    assert!(
        err.to_string().contains("to logfmt"),
        "the failure did not name the command it looked for: {err}"
    );
}

#[test]
fn an_existing_file_is_kept_unless_forced() {
    let (_dir, path, mut nu) = target("lines.seek.zst");

    eval(&mut nu, &format!("\"a\n\" | zstdsep save \"{path}\"")).expect("Failed to save");

    let err = eval(&mut nu, &format!("\"b\n\" | zstdsep save \"{path}\""))
        .expect_err("an existing file was overwritten");
    assert!(
        err.to_string().contains("already exists"),
        "the failure was not about the existing file: {err}"
    );
    assert_eq!(records(&mut nu, &path), vec!["a"], "the file changed");

    eval(
        &mut nu,
        &format!("\"b\n\" | zstdsep save --force \"{path}\""),
    )
    .expect("Failed to overwrite");
    assert_eq!(records(&mut nu, &path), vec!["b"]);
}

/// The file is written one record per frame because the library refuses to append to a file of
/// fewer than three frames: it validates the separator by comparing two full frames.
#[test]
fn append_adds_records_to_an_existing_file() {
    let (_dir, path, mut nu) = target("lines.seek.zst");

    eval(
        &mut nu,
        &format!("[a b c] | zstdsep save --records-per-frame 1 \"{path}\""),
    )
    .expect("Failed to save");
    eval(
        &mut nu,
        &format!("[d e] | zstdsep save --append \"{path}\""),
    )
    .expect("Failed to append");

    assert_eq!(records(&mut nu, &path), vec!["a", "b", "c", "d", "e"]);
}

#[test]
fn append_to_a_missing_file_is_refused() {
    let (_dir, path, mut nu) = target("absent.seek.zst");

    let err = eval(&mut nu, &format!("\"a\n\" | zstdsep save -a \"{path}\""))
        .expect_err("appending to a missing file succeeded");
    assert!(
        err.to_string().contains("does not exist"),
        "the failure was not about the missing file: {err}"
    );
}

#[test]
fn force_and_append_together_are_refused() {
    let (_dir, path, mut nu) = target("lines.seek.zst");

    eval(&mut nu, &format!("\"a\n\" | zstdsep save \"{path}\"")).expect("Failed to save");

    let err = eval(
        &mut nu,
        &format!("\"b\n\" | zstdsep save --append --force \"{path}\""),
    )
    .expect_err("--append and --force were taken together");
    assert!(
        err.to_string().contains("--force"),
        "the failure did not name the flags: {err}"
    );
}

#[test]
fn records_end_with_the_separator_given() {
    let (_dir, path, mut nu) = target("lines.seek.zst");

    eval(
        &mut nu,
        &format!("[a b c] | zstdsep save --separator \";\" \"{path}\""),
    )
    .expect("Failed to save");

    assert_eq!(
        records_with(&mut nu, &path, "--separator \";\""),
        vec!["a", "b", "c"],
        "the file was not cut at the separator it was written with"
    );
}

/// The compressor's own knobs, which decide how much of a file one `get` has to decompress.
#[test]
fn records_per_frame_is_what_was_asked_for() {
    let (_dir, path, mut nu) = target("lines.seek.zst");

    eval(
        &mut nu,
        &format!("[a b c d e f g h i j] | zstdsep save --records-per-frame 4 \"{path}\""),
    )
    .expect("Failed to save");

    let per_frame = eval(
        &mut nu,
        &format!("(zstdsep open \"{path}\").records_per_frame"),
    )
    .expect("Failed to read the summary")
    .as_int()
    .expect("records_per_frame is not an integer");
    assert_eq!(per_frame, 4);
}

#[test]
fn raw_refuses_to_serialise() {
    let (_dir, path, mut nu) = target("rows.tsv.seek.zst");

    let err = eval(
        &mut nu,
        &format!("[[a, b]; [1, x]] | zstdsep save --raw \"{path}\""),
    )
    .expect_err("--raw serialised a table");
    assert!(
        err.to_string().contains("format"),
        "the failure did not name the missing format: {err}"
    );
}

#[test]
fn format_overrides_the_extension() {
    let (_dir, path, mut nu) = target("rows.seek.zst");

    eval(
        &mut nu,
        &format!("[[a, b]; [1, x]] | zstdsep save --format tsv \"{path}\""),
    )
    .expect("Failed to save");

    assert_eq!(records(&mut nu, &path), vec!["a\tb", "1\tx"]);
}

/// A list of 200 short records, as a nushell literal.
fn many_records() -> String {
    let items: Vec<String> = (0..200).map(|i| format!("line{i:04}")).collect();
    format!("[{}]", items.join(" "))
}

#[test]
fn frame_size_decides_how_many_records_a_frame_holds() {
    let (_dir, path, mut nu) = target("small.seek.zst");
    let (_other_dir, wide_path, _) = target("wide.seek.zst");
    let records = many_records();

    eval(
        &mut nu,
        &format!("{records} | zstdsep save --frame-size 256 \"{path}\""),
    )
    .expect("Failed to save");
    eval(
        &mut nu,
        &format!("{records} | zstdsep save \"{wide_path}\""),
    )
    .expect("Failed to save");

    let per_frame = |nu: &mut PluginTest, path: &str| {
        eval(nu, &format!("(zstdsep open \"{path}\").records_per_frame"))
            .expect("Failed to read the summary")
            .as_int()
            .expect("records_per_frame is not an integer")
    };
    assert!(
        per_frame(&mut nu, &path) < per_frame(&mut nu, &wide_path),
        "a smaller frame did not hold fewer records"
    );
}

#[test]
fn no_check_leaves_the_checksum_out() {
    let (_dir, path, mut nu) = target("checked.seek.zst");
    let (_other_dir, unchecked_path, _) = target("unchecked.seek.zst");
    let records = many_records();

    eval(
        &mut nu,
        &format!("{records} | zstdsep save --frame-size 256 \"{path}\""),
    )
    .expect("Failed to save");
    eval(
        &mut nu,
        &format!("{records} | zstdsep save --frame-size 256 --no-check \"{unchecked_path}\""),
    )
    .expect("Failed to save");

    let size = |path: &str| {
        std::fs::metadata(path)
            .expect("Failed to stat the file")
            .len()
    };
    assert!(
        size(&unchecked_path) < size(&path),
        "the file with no checksums is not the smaller one"
    );
}

/// A file whose last frame ends mid-record, which is what `--insert-separator` exists for. It is
/// written by the library rather than by `save`, which always terminates what it writes.
fn fragment_fixture() -> (TempDir, String) {
    let dir = tempdir().expect("Failed to create temp dir");
    let mut body: Vec<u8> = (0..44)
        .map(|i| format!("line{i:04}\n"))
        .collect::<String>()
        .into();
    body.extend_from_slice(b"cut off here");
    let path = common::compress_body(dir.path(), "fragment.seek.zst", body);
    (dir, path.to_string_lossy().to_string())
}

#[test]
fn appending_to_a_file_that_ends_mid_record_is_refused() {
    let (_dir, path) = fragment_fixture();
    let mut nu = nu();

    let err = eval(&mut nu, &format!("[next] | zstdsep save -a \"{path}\""))
        .expect_err("records were added after a fragment");
    // The reason is the library's and lands in the label, which `to_string` leaves out and
    // nushell shows.
    assert!(
        format!("{err:?}").contains("whole record"),
        "the failure was not about the fragment: {err:?}"
    );
}

#[test]
fn insert_separator_closes_the_fragment_first() {
    let (_dir, path) = fragment_fixture();
    let mut nu = nu();

    eval(
        &mut nu,
        &format!("[next] | zstdsep save -a --insert-separator \"{path}\""),
    )
    .expect("Failed to append");

    let back = records(&mut nu, &path);
    assert_eq!(back.len(), 46, "the fragment became a record of its own");
    assert_eq!(back[44], "cut off here");
    assert_eq!(back[45], "next");
}

/// jsonl and ndjson are `std formats` commands, which are written in nushell and which a plugin
/// cannot call: `call_decl` answers one with "can't run custom command with 'run'". So the plugin
/// writes JSON itself, as it already reads it. Nothing is in scope here, which is the point.
#[test]
fn json_is_written_by_the_plugin_itself() {
    let (_dir, path, mut nu) = target("rows.jsonl.seek.zst");

    eval(
        &mut nu,
        &format!("[[seq, lvl]; [0, info], [1, error]] | zstdsep save \"{path}\""),
    )
    .expect("Failed to save");

    assert_eq!(
        records(&mut nu, &path),
        vec![r#"{"seq":0,"lvl":"info"}"#, r#"{"seq":1,"lvl":"error"}"#],
        "one JSON value per record, in the column order of the table"
    );
    let lvl = eval(&mut nu, &format!("(zstdsep open \"{path}\").1.lvl"))
        .expect("Failed to read the record back")
        .into_string()
        .expect("lvl is not a string");
    assert_eq!(lvl, "error", "what save wrote is what open reads");
}

/// `to tsv` writes a header row at the front of whatever it is handed, so appending through it
/// writes a second one into the middle of the file. nushell's own `save --append` does exactly
/// this, and there is no way to ask a `to` command whether it writes a header; matching `save`
/// beats guessing which formats have one.
#[test]
fn appending_a_format_that_writes_a_header_writes_it_again() {
    let (_dir, path, mut nu) = target("rows.tsv.seek.zst");

    eval(
        &mut nu,
        &format!(
            "[[a, b]; [1, x], [2, y], [3, z]] | zstdsep save --records-per-frame 1 \"{path}\""
        ),
    )
    .expect("Failed to save");
    eval(
        &mut nu,
        &format!("[[a, b]; [4, w]] | zstdsep save --append \"{path}\""),
    )
    .expect("Failed to append");

    assert_eq!(
        records(&mut nu, &path),
        vec!["a\tb", "1\tx", "2\ty", "3\tz", "a\tb", "4\tw"]
    );
}

/// The way around it: serialise it yourself and append the text.
#[test]
fn a_header_less_serialisation_can_be_appended_as_text() {
    let (_dir, path, mut nu) = target("rows.tsv.seek.zst");

    eval(
        &mut nu,
        &format!(
            "[[a, b]; [1, x], [2, y], [3, z]] | zstdsep save --records-per-frame 1 \"{path}\""
        ),
    )
    .expect("Failed to save");
    eval(
        &mut nu,
        &format!("[[a, b]; [4, w]] | to tsv --noheaders | zstdsep save --append --raw \"{path}\""),
    )
    .expect("Failed to append");

    assert_eq!(
        records(&mut nu, &path),
        vec!["a\tb", "1\tx", "2\ty", "3\tz", "4\tw"]
    );
}
