mod common;

use common::{assert_decompresses_to, assert_framing, compress_frames};
use seekzstdsep::{AppendOptions, OnMissingSeparator, append_records_with_opts, find::by_fixed};
use std::fs::File;
use zeekstd::SeekTable;

#[test]
fn full_tail_is_preserved_and_results_can_be_chained() {
    let dir = tempfile::tempdir().unwrap();
    let path = compress_frames(
        dir.path(),
        "append-result",
        &[b"a\nb\n".to_vec(), b"c\nd\n".to_vec()],
    );
    let original = std::fs::read(&path).unwrap();
    let mut file = File::options().read(true).write(true).open(&path).unwrap();
    let original_table = SeekTable::from_seekable(&mut file).unwrap();
    let original_end = original_table.frame_end_comp(1).unwrap() as usize;

    let first = append_records_with_opts(
        &mut file,
        &b"e\nf\n"[..],
        b"\n",
        OnMissingSeparator::Refuse,
        0,
        Some(2),
        AppendOptions::new(),
    )
    .unwrap();
    assert_eq!(first.records_per_frame(), 2);
    assert_eq!(first.last_frame_records(), Some(2));
    assert_eq!(
        &std::fs::read(&path).unwrap()[..original_end],
        &original[..original_end]
    );

    let second = append_records_with_opts(
        &mut file,
        &b"g\n"[..],
        b"\n",
        OnMissingSeparator::Refuse,
        0,
        Some(2),
        AppendOptions::new().with_previous(&first),
    )
    .unwrap();
    assert_eq!(second.last_frame_records(), Some(1));
    assert_framing(&path, &[2, 2, 2, 1]);

    let third = append_records_with_opts(
        &mut file,
        &b"h\n"[..],
        b"\n",
        OnMissingSeparator::Refuse,
        0,
        Some(2),
        AppendOptions::new().with_previous(&second),
    )
    .unwrap();
    assert_eq!(third.last_frame_records(), Some(2));
    assert_framing(&path, &[2, 2, 2, 2]);
    assert_decompresses_to(&path, b"a\nb\nc\nd\ne\nf\ng\nh\n");
    assert_eq!(
        &std::fs::read(&path).unwrap()[..original_end],
        &original[..original_end]
    );
}

#[test]
fn stale_result_falls_back_to_normal_validation() {
    let dir = tempfile::tempdir().unwrap();
    let path = compress_frames(
        dir.path(),
        "stale-result",
        &[b"a\nb\n".to_vec(), b"c\nd\n".to_vec()],
    );
    let mut file = File::options().read(true).write(true).open(&path).unwrap();
    let first = append_records_with_opts(
        &mut file,
        &b"e\nf\n"[..],
        b"\n",
        OnMissingSeparator::Refuse,
        0,
        Some(2),
        AppendOptions::new(),
    )
    .unwrap();
    append_records_with_opts(
        &mut file,
        &b"g\nh\n"[..],
        b"\n",
        OnMissingSeparator::Refuse,
        0,
        Some(2),
        AppendOptions::new(),
    )
    .unwrap();
    append_records_with_opts(
        &mut file,
        &b"i\n"[..],
        b"\n",
        OnMissingSeparator::Refuse,
        0,
        Some(2),
        AppendOptions::new().with_previous(&first),
    )
    .unwrap();
    assert_framing(&path, &[2, 2, 2, 2, 1]);
    assert_decompresses_to(&path, b"a\nb\nc\nd\ne\nf\ng\nh\ni\n");
}

#[test]
fn inserting_a_missing_separator_rewrites_the_last_frame() {
    let dir = tempfile::tempdir().unwrap();
    let path = compress_frames(
        dir.path(),
        "insert-at-tail",
        &[b"a\nb\n".to_vec(), b"c\nfragment".to_vec()],
    );
    let mut file = File::options().read(true).write(true).open(&path).unwrap();
    let result = append_records_with_opts(
        &mut file,
        &b"d\n"[..],
        b"\n",
        OnMissingSeparator::Insert,
        0,
        Some(2),
        AppendOptions::new(),
    )
    .unwrap();
    assert_eq!(result.last_frame_records(), Some(1));
    assert_framing(&path, &[2, 2, 1]);
    assert_decompresses_to(&path, b"a\nb\nc\nfragment\nd\n");
}

#[test]
fn trusted_result_does_not_skip_later_validation() {
    let dir = tempfile::tempdir().unwrap();
    let path = compress_frames(
        dir.path(),
        "trusted-then-checked",
        &[b"a\n".to_vec(), b"b\nc\n".to_vec(), b"d\n".to_vec()],
    );
    let mut file = File::options().read(true).write(true).open(&path).unwrap();
    let previous = append_records_with_opts(
        &mut file,
        &b"e\n"[..],
        b"\n",
        OnMissingSeparator::Refuse,
        0,
        Some(2),
        AppendOptions::new().trust_records_per_frame(true),
    )
    .unwrap();
    let before = std::fs::read(&path).unwrap();
    let error = append_records_with_opts(
        &mut file,
        &b"f\n"[..],
        b"\n",
        OnMissingSeparator::Refuse,
        0,
        Some(2),
        AppendOptions::new().with_previous(&previous),
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("frame 0 must contain 2"),
        "{error}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn finder_uses_the_same_options_and_result() {
    let dir = tempfile::tempdir().unwrap();
    let path = compress_frames(
        dir.path(),
        "finder-result",
        &[b"a\nb\n".to_vec(), b"c\n".to_vec()],
    );
    let mut file = File::options().read(true).write(true).open(&path).unwrap();
    let first = append_records_with_opts(
        &mut file,
        &b"d\n"[..],
        b"\n",
        OnMissingSeparator::Refuse,
        0,
        Some(2),
        AppendOptions::new().with_finder(by_fixed(2)),
    )
    .unwrap();
    let second = append_records_with_opts(
        &mut file,
        &b"e\n"[..],
        b"",
        OnMissingSeparator::Refuse,
        0,
        Some(2),
        AppendOptions::new()
            .with_finder(by_fixed(2))
            .with_previous(&first),
    )
    .unwrap();
    assert_eq!(second.last_frame_records(), Some(1));
    assert_framing(&path, &[2, 2, 1]);
    assert_decompresses_to(&path, b"a\nb\nc\nd\ne\n");

    let before = std::fs::read(&path).unwrap();
    let error = append_records_with_opts(
        &mut file,
        &b"f\n"[..],
        b"",
        OnMissingSeparator::Insert,
        0,
        Some(2),
        AppendOptions::new()
            .with_finder(by_fixed(2))
            .with_previous(&second),
    )
    .unwrap_err();
    assert!(error.to_string().contains("nothing to insert"), "{error}");
    assert_eq!(std::fs::read(&path).unwrap(), before);
}
