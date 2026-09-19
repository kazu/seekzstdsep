mod common;
use common::*;

use seekzstdsep::{
    AppendInput, OnMissingSeparator, RecordReader, append, append_records, append_records_with,
    find,
};
use std::fs::File;
use std::path::Path;

fn make_records(count: usize) -> Vec<Vec<u8>> {
    (0..count)
        .map(|i| format!("{i:08}\n").into_bytes())
        .collect()
}

fn append_count(
    path: &Path,
    data: &[u8],
    count: Option<usize>,
    on_missing: OnMissingSeparator,
    entry: usize,
) -> anyhow::Result<()> {
    let mut file = File::options().read(true).write(true).open(path)?;
    match entry {
        0 => append_records(&mut file, data, b"\n", on_missing, 0, count),
        1 => append_records_with(&mut file, data, find::by_fixed(9), on_missing, 0, count),
        2 => append(
            &mut file,
            AppendInput::Records {
                data,
                on_missing,
                level: 0,
                records_per_frame: count,
            },
            b"\n",
        ),
        _ => unreachable!(),
    }
}

fn assert_count_refused(path: &Path, count: Option<usize>, entry: usize) -> String {
    let before = std::fs::read(path).unwrap();
    let error = append_count(
        path,
        b"appended\n",
        count,
        OnMissingSeparator::Refuse,
        entry,
    )
    .expect_err("invalid append accepted");
    assert_eq!(
        std::fs::read(path).unwrap(),
        before,
        "refusal changed the target"
    );
    error.to_string()
}

#[test]
fn explicit_count_appends_small_files_through_every_entry() {
    for entry in 0..3 {
        for (layout, added, expected) in [
            (vec![255], 10, vec![255, 10]),
            (vec![200], 100, vec![255, 45]),
            (vec![255, 10], 250, vec![255, 255, 5]),
            (vec![255, 255, 10], 250, vec![255, 255, 255, 5]),
            (vec![300], 10, vec![255, 55]),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let initial: usize = layout.iter().sum();
            let records = make_records(initial + added);
            let mut at = 0;
            let groups: Vec<_> = layout
                .iter()
                .map(|&len| {
                    let group = records[at..at + len].concat();
                    at += len;
                    group
                })
                .collect();
            let path = compress_frames(dir.path(), "small", &groups);
            assert_framing(&path, &layout);
            let flags = frame_checksum_flags(&path);
            append_count(
                &path,
                &records[initial..].concat(),
                Some(255),
                OnMissingSeparator::Refuse,
                entry,
            )
            .unwrap();
            assert_framing(&path, &expected);
            assert_decompresses_to(&path, &records.concat());
            assert!(
                frame_checksum_flags(&path)
                    .iter()
                    .all(|flag| *flag == flags[0])
            );
            let mut reader = RecordReader::open(path.clone(), b"\n").unwrap();
            assert_eq!(reader.total_records().unwrap(), records.len());
            assert_eq!(
                reader
                    .into_records()
                    .collect::<anyhow::Result<Vec<_>>>()
                    .unwrap(),
                records
            );
            for i in 0..records.len() {
                assert_cat_returns(&path, &records, i, 1);
            }
        }
    }
}

#[test]
fn omitted_count_still_refuses_one_and_two_frames() {
    let dir = tempfile::tempdir().unwrap();
    for groups in [
        vec![b"00000000\n".to_vec()],
        vec![b"00000000\n".to_vec(); 2],
    ] {
        let path = compress_frames(dir.path(), "inferred", &groups);
        for entry in 0..3 {
            assert!(assert_count_refused(&path, None, entry).contains("fewer than three"));
        }
    }
}

#[test]
fn explicit_count_rejects_zero_and_mismatched_nonfinal_frames() {
    let dir = tempfile::tempdir().unwrap();
    let records = make_records(255).concat();
    for groups in [
        vec![records.clone()],
        vec![records.clone(); 2],
        vec![records.clone(); 3],
    ] {
        let path = compress_frames(dir.path(), "zero", &groups);
        for entry in 0..3 {
            assert!(assert_count_refused(&path, Some(0), entry).contains("greater than zero"));
        }
    }
    for counts in [vec![254, 10], vec![254, 255, 10], vec![255, 254, 10]] {
        let groups: Vec<_> = counts.iter().map(|&n| make_records(n).concat()).collect();
        let path = compress_frames(dir.path(), "mismatch", &groups);
        for entry in 0..3 {
            assert!(assert_count_refused(&path, Some(255), entry).contains("whole records"));
        }
    }
}

#[test]
fn explicit_count_rejects_fragments_in_both_checked_frames() {
    let dir = tempfile::tempdir().unwrap();
    let whole = make_records(255).concat();
    let fragment = [whole.clone(), b"x".to_vec()].concat();
    for groups in [
        vec![fragment.clone(), whole.clone(), whole.clone()],
        vec![whole.clone(), fragment, whole.clone()],
    ] {
        let path = compress_frames(dir.path(), "fragment", &groups);
        for entry in 0..3 {
            assert!(assert_count_refused(&path, Some(255), entry).contains("whole records"));
        }
    }
}

#[test]
fn explicit_count_does_not_scan_intervening_frames() {
    let dir = tempfile::tempdir().unwrap();
    let groups: Vec<_> = [255, 254, 255, 10]
        .iter()
        .map(|&n| make_records(n).concat())
        .collect();
    let path = compress_frames(dir.path(), "unchecked", &groups);
    append_count(
        &path,
        b"appended\n",
        Some(255),
        OnMissingSeparator::Refuse,
        0,
    )
    .unwrap();
    assert_framing(&path, &[255, 254, 255, 11]);
    assert_decompresses_to(&path, &[groups.concat(), b"appended\n".to_vec()].concat());
}

#[test]
fn explicit_count_preserves_an_empty_append_and_handles_a_tail_fragment() {
    let dir = tempfile::tempdir().unwrap();
    for entry in [0, 2] {
        let path = compress_frames(dir.path(), "tail", &[b"record\nfragment".to_vec()]);
        let before = std::fs::read(&path).unwrap();
        append_count(&path, b"", Some(2), OnMissingSeparator::Refuse, entry).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(assert_count_refused(&path, Some(2), entry).contains("does not end"));
        append_count(
            &path,
            b"appended\n",
            Some(2),
            OnMissingSeparator::Insert,
            entry,
        )
        .unwrap();
        assert_framing(&path, &[2, 1]);
        assert_decompresses_to(&path, b"record\nfragment\nappended\n");
    }
    let path = compress_frames(dir.path(), "fixed-fragment", &[b"00000000\nx".to_vec()]);
    assert!(assert_count_refused(&path, Some(2), 1).contains("does not end"));
    let before = std::fs::read(&path).unwrap();
    assert!(
        append_count(&path, b"appended\n", Some(2), OnMissingSeparator::Insert, 1)
            .unwrap_err()
            .to_string()
            .contains("nothing to insert")
    );
    assert_eq!(std::fs::read(path).unwrap(), before);
}

#[test]
fn explicit_count_discards_trailing_empty_frames_but_does_not_validate_the_tail_as_full() {
    let dir = tempfile::tempdir().unwrap();
    for counts in [vec![200, 0], vec![255, 10, 0, 0]] {
        let groups: Vec<_> = counts.iter().map(|&n| make_records(n).concat()).collect();
        let path = compress_frames(dir.path(), "empty-tail", &groups);
        assert_framing(&path, &counts);
        let before = std::fs::read(&path).unwrap();
        append_count(&path, b"", Some(255), OnMissingSeparator::Refuse, 0).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), before);
        append_count(
            &path,
            &make_records(100).concat(),
            Some(255),
            OnMissingSeparator::Refuse,
            0,
        )
        .unwrap();
        let expected = if counts[0] == 200 {
            vec![255, 45]
        } else {
            vec![255, 110]
        };
        assert_framing(&path, &expected);
        assert_decompresses_to(
            &path,
            &[groups.concat(), make_records(100).concat()].concat(),
        );
    }
}

#[test]
fn explicit_count_refuses_zero_entries_and_only_empty_frames() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zero-entries.zst");
    let mut serializer = zeekstd::SeekTable::new().into_serializer();
    let mut bytes = vec![0; serializer.encoded_len()];
    let len = serializer.write_into(&mut bytes);
    std::fs::write(&path, &bytes[..len]).unwrap();
    for entry in 0..3 {
        assert!(assert_count_refused(&path, Some(255), entry).contains("no data frames"));
    }
    for groups in [vec![vec![]], vec![vec![]; 3]] {
        let path = compress_frames(dir.path(), "only-empty", &groups);
        for entry in 0..3 {
            assert!(assert_count_refused(&path, Some(255), entry).contains("no data frames"));
        }
    }
}
