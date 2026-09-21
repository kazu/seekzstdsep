mod common;

use common::{assert_decompresses_to, assert_framing, compress_frames};
use seekzstdsep::{OnMissingSeparator, append_records_with_finder_opts, append_records_with_opts};
use std::fs::File;
use std::path::Path;

fn append(path: &Path, count: Option<usize>, trust: bool, finder: bool) -> anyhow::Result<()> {
    let mut file = File::options().read(true).write(true).open(path)?;
    if finder {
        append_records_with_finder_opts(
            &mut file,
            &b"d\ne\n"[..],
            |data| {
                assert!(!data.contains(&b'a'), "read a frame before the tail");
                data.iter().position(|&b| b == b'\n').map(|i| i + 1)
            },
            OnMissingSeparator::Refuse,
            0,
            count,
            trust,
        )
    } else {
        append_records_with_opts(
            &mut file,
            &b"d\ne\n"[..],
            b"\n",
            OnMissingSeparator::Refuse,
            0,
            count,
            trust,
        )
    }
}

#[test]
fn trusted_append_reads_only_tail_and_preserves_records() {
    for finder in [false, true] {
        for groups in [
            vec![b"c\n".to_vec()],
            vec![b"a\na\n".to_vec(), b"c\n".to_vec()],
            vec![
                b"a\na\n".to_vec(),
                b"a\na\n".to_vec(),
                b"c\n".to_vec(),
                vec![],
            ],
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = compress_frames(dir.path(), "trusted", &groups);
            let mut expected = groups.concat();
            expected.extend_from_slice(b"d\ne\n");
            append(&path, Some(2), true, finder).unwrap();
            assert_decompresses_to(&path, &expected);
            let mut layout = vec![2; expected.len() / 4];
            layout.push(1);
            assert_framing(&path, &layout);
        }
    }
}

#[test]
fn trusted_append_rejects_oversized_tail_without_writing() {
    for finder in [false, true] {
        for groups in [
            vec![b"c\nc\nc\n".to_vec()],
            vec![b"a\na\n".to_vec(), b"c\nc\nc\n".to_vec(), vec![]],
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = compress_frames(dir.path(), "oversized", &groups);
            let before = std::fs::read(&path).unwrap();
            let error = append(&path, Some(2), true, finder).unwrap_err();
            assert!(error.to_string().contains("exceeds"), "{error}");
            assert_eq!(std::fs::read(path).unwrap(), before);
        }
    }
}

#[test]
fn trust_requires_positive_count() {
    for finder in [false, true] {
        for count in [None, Some(0)] {
            let dir = tempfile::tempdir().unwrap();
            let path = compress_frames(dir.path(), "invalid", &[b"c\n".to_vec()]);
            let before = std::fs::read(&path).unwrap();
            assert!(append(&path, count, true, finder).is_err());
            assert_eq!(std::fs::read(path).unwrap(), before);
        }
    }
}

#[test]
fn only_explicit_trust_skips_existing_frame_validation() {
    for trust in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = compress_frames(dir.path(), "mismatch", &[b"a\n".to_vec(), b"c\n".to_vec()]);
        let before = std::fs::read(&path).unwrap();
        let result = append(&path, Some(2), trust, false);
        assert_eq!(result.is_ok(), trust);
        if !trust {
            assert_eq!(std::fs::read(path).unwrap(), before);
        }
    }
}

#[test]
fn inserting_separator_cannot_exceed_trusted_count() {
    let dir = tempfile::tempdir().unwrap();
    let path = compress_frames(dir.path(), "fragment", &[b"c\nc\nfragment".to_vec()]);
    let before = std::fs::read(&path).unwrap();
    let mut file = File::options().read(true).write(true).open(&path).unwrap();
    let error = append_records_with_opts(
        &mut file,
        &b"d\n"[..],
        b"\n",
        OnMissingSeparator::Insert,
        0,
        Some(2),
        true,
    )
    .unwrap_err();
    assert!(error.to_string().contains("exceeds"), "{error}");
    assert_eq!(std::fs::read(path).unwrap(), before);
}

#[test]
fn cli_trust_is_explicit_and_only_for_raw_input() {
    use std::process::Command;
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("input");
    std::fs::write(&input, b"d\ne\n").unwrap();
    for args in [
        vec!["--records-per-frame", "2", "--trust-records-per-frame"],
        vec!["--records-per-frame", "2"],
        vec!["--trust-records-per-frame"],
        vec![
            "--records-per-frame",
            "2",
            "--trust-records-per-frame",
            "--input-seekable",
        ],
    ] {
        let path = compress_frames(dir.path(), "cli", &[b"a\n".to_vec(), b"c\n".to_vec()]);
        let before = std::fs::read(&path).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_seekzstdsep"))
            .arg("append")
            .arg(&path)
            .arg(&input)
            .args(&args)
            .output()
            .unwrap();
        let succeeds = args.len() == 3;
        assert_eq!(
            result.status.success(),
            succeeds,
            "{args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        if succeeds {
            assert_decompresses_to(&path, b"a\nc\nd\ne\n");
        } else {
            assert_eq!(std::fs::read(path).unwrap(), before);
        }
    }
}
