mod common;

use common::*;
use seekzstdsep::{CopyMode, OnMissingSeparator, append_records, copy_and_replace, with_file_lock};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::Path,
    sync::{Arc, Barrier},
};

fn add(f: &mut File, data: &[u8]) -> anyhow::Result<()> {
    append_records(f, data, b"\n", OnMissingSeparator::Refuse, 0, None)
}

fn assert_clean(path: &Path) {
    let names: Vec<_> = fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names.len(), 2, "only target and persistent lock: {names:?}");
}

#[test]
fn copy_append_preserves_old_readers_and_publishes_complete_records() {
    let dir = tempfile::tempdir().unwrap();
    let path = compress_fixture(dir.path());
    let before = fs::read(&path).unwrap();
    let mut old = File::open(&path).unwrap();
    let added = fixture_records()[..10].concat();
    let mode = copy_and_replace(&path, |f| {
        add(f, &added)?;
        assert_eq!(fs::read(&path)?, before);
        Ok(())
    })
    .unwrap();
    assert_eq!(mode, CopyMode::Replaced);
    let mut held = Vec::new();
    old.read_to_end(&mut held).unwrap();
    assert_eq!(held, before);
    drop(old);
    assert_decompresses_to(&path, &[fixture_records().concat(), added].concat());
    assert_clean(&path);
}

#[test]
fn two_copies_of_one_version_cannot_lose_a_successful_append() {
    let dir = tempfile::tempdir().unwrap();
    let path = compress_fixture(dir.path());
    let barrier = Arc::new(Barrier::new(2));
    let results = std::thread::scope(|s| {
        let handles: Vec<_> = [b"winner a\n".as_slice(), b"winner b\n".as_slice()]
            .into_iter()
            .map(|data| {
                let barrier = barrier.clone();
                let path = &path;
                s.spawn(move || {
                    (
                        data,
                        copy_and_replace(path, |f| {
                            barrier.wait();
                            add(f, data)
                        }),
                    )
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(results.iter().filter(|(_, r)| r.is_ok()).count(), 1);
    let winner = results.iter().find(|(_, r)| r.is_ok()).unwrap().0;
    let err = results
        .iter()
        .find(|(_, r)| r.is_err())
        .unwrap()
        .1
        .as_ref()
        .unwrap_err();
    assert!(err.to_string().contains("conflict"), "{err:#}");
    assert_decompresses_to(
        &path,
        &[fixture_records().concat(), winner.to_vec()].concat(),
    );
    assert_clean(&path);
}

#[test]
fn locked_direct_append_causes_an_older_copy_to_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let path = compress_fixture(dir.path());
    let err = copy_and_replace(&path, |f| {
        with_file_lock(&path, |current| add(current, b"direct\n"))?;
        add(f, b"stale\n")
    })
    .unwrap_err();
    assert!(err.to_string().contains("conflict"), "{err:#}");
    assert_decompresses_to(
        &path,
        &[fixture_records().concat(), b"direct\n".to_vec()].concat(),
    );
    assert_clean(&path);
}

#[test]
fn truncate_callback_detects_intervening_append_and_truncate() {
    for direct in [false, true] {
        for append in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = compress_fixture(dir.path());
            let winner = |file: &mut File| {
                if append {
                    add(file, b"winner\n")
                } else {
                    seekzstdsep::truncate(file, 234, b"\n")
                }
            };
            let error = copy_and_replace(&path, |file| {
                if direct {
                    with_file_lock(&path, winner)?;
                } else {
                    copy_and_replace(&path, winner)?;
                }
                seekzstdsep::truncate(file, 117, b"\n")
            })
            .unwrap_err();
            assert!(error.to_string().contains("conflict"));
            let expected = if append {
                [fixture_records().concat(), b"winner\n".to_vec()].concat()
            } else {
                fixture_records_upto(234, true).concat()
            };
            assert_decompresses_to(&path, &expected);
            assert_clean(&path);
        }
    }
}

#[test]
fn failed_append_cleans_its_copy_without_changing_the_target() {
    let dir = tempfile::tempdir().unwrap();
    let path = compress_fixture(dir.path());
    let before = fs::read(&path).unwrap();
    let err = copy_and_replace(&path, |f| {
        f.write_all(b"broken")?;
        anyhow::bail!("input failed")
    })
    .unwrap_err();
    assert!(err.to_string().contains("input failed"));
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_clean(&path);
}

#[test]
fn empty_append_keeps_the_bytes_and_reuses_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = compress_fixture(dir.path());
    let before = fs::read(&path).unwrap();
    for _ in 0..2 {
        assert_eq!(
            copy_and_replace(&path, |f| add(f, b"")).unwrap(),
            CopyMode::Replaced
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_clean(&path);
    }
}

#[test]
fn explicit_count_finders_and_separator_insertion_match_direct_append() {
    use seekzstdsep::{append_records_with, find};
    for finder in [false, true] {
        for insert in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let group = b"record 1\nrecord 2\n".to_vec();
            let groups = if insert {
                vec![group, b"fragment".to_vec()]
            } else {
                vec![group]
            };
            let path = compress_frames(dir.path(), "target", &groups);
            let direct = dir.path().join("direct");
            fs::copy(&path, &direct).unwrap();
            let operation = |f: &mut File| {
                let missing = if insert {
                    OnMissingSeparator::Insert
                } else {
                    OnMissingSeparator::Refuse
                };
                if finder {
                    append_records_with(
                        f,
                        b"record 3\n".as_slice(),
                        find::by_fixed(9),
                        missing,
                        0,
                        Some(2),
                    )
                } else {
                    append_records(f, b"record 3\n".as_slice(), b"\n", missing, 0, Some(2))
                }
            };
            let expected = operation(
                &mut File::options()
                    .read(true)
                    .write(true)
                    .open(&direct)
                    .unwrap(),
            );
            let actual = copy_and_replace(&path, operation);
            match (expected, actual) {
                (Ok(()), Ok(CopyMode::Replaced)) => (),
                (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string()),
                other => panic!("different result: {other:?}"),
            }
            assert_eq!(fs::read(&path).unwrap(), fs::read(&direct).unwrap());
        }
    }
}

#[test]
fn frame_append_matches_the_existing_compressed_frame_entry() {
    use seekzstdsep::{RangeCheck, append_frames};
    let dir = tempfile::tempdir().unwrap();
    let records = fixture_records();
    let path = compress_frames(dir.path(), "target", &vec![records[..2].concat(); 3]);
    let input_path = compress_frames(dir.path(), "input", &vec![records[2..4].concat(); 3]);
    let input = File::open(&input_path).unwrap();
    let direct = dir.path().join("direct");
    fs::copy(&path, &direct).unwrap();
    let operation =
        |file: &mut File| append_frames(file, &input, 0, None, b"\n", RangeCheck::EveryFrame);
    operation(
        &mut File::options()
            .read(true)
            .write(true)
            .open(&direct)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        copy_and_replace(&path, operation).unwrap(),
        CopyMode::Replaced
    );
    assert_eq!(fs::read(path).unwrap(), fs::read(direct).unwrap());
}

#[test]
fn checksum_selection_is_preserved() {
    for checksum in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = compress_fixture_with_checksum(dir.path(), checksum);
        copy_and_replace(&path, |file| add(file, b"new record\n")).unwrap();
        assert!(
            frame_checksum_flags(&path)
                .iter()
                .all(|flag| *flag == checksum)
        );
        assert_decompresses_to(
            &path,
            &[fixture_records().concat(), b"new record\n".to_vec()].concat(),
        );
    }
}

#[test]
fn held_record_reader_can_still_read_the_old_tail_after_publication() {
    use seekzstdsep::RecordReader;
    let dir = tempfile::tempdir().unwrap();
    let path = compress_fixture(dir.path());
    let mut old = RecordReader::open(path.clone(), b"\n").unwrap();
    copy_and_replace(&path, |file| add(file, b"new record\n")).unwrap();
    assert_eq!(old.total_records().unwrap(), FIXTURE_RECORDS);
    let expected = fixture_records()[FIXTURE_RECORDS - 1].clone();
    assert_eq!(old.record(FIXTURE_RECORDS - 1).unwrap().unwrap(), expected);
    let mut new = RecordReader::open(path, b"\n").unwrap();
    assert_eq!(new.total_records().unwrap(), FIXTURE_RECORDS + 1);
}
