mod common;

use common::{compress_body, compress_frames};
use seekzstdsep::{RecordChainExt, RecordReader};
use tempfile::tempdir;

#[test]
fn all_three_record_forms_use_the_same_positioned_chain() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let records: Vec<Vec<u8>> = (0..1_200)
        .map(|index| format!("record {index}\n").into_bytes())
        .collect();
    let frames: Vec<Vec<u8>> = records.chunks(100).map(|chunk| chunk.concat()).collect();
    let path = compress_frames(dir.path(), "record-chain", &frames);

    let mut reader = RecordReader::open(path.clone(), b"\n")?;
    let mut out = Vec::new();
    reader.records_from(1000).take(100).write_to(&mut out)?;
    assert_eq!(out, reader.records(1000, 100)?);

    let bytes = reader.records_from(1000).take(100).to_vec()?;
    assert_eq!(bytes, out);

    let per_record: Vec<Vec<u8>> = RecordReader::open(path, b"\n")?
        .into_records_from(1000)
        .take(100)
        .collect::<anyhow::Result<Vec<_>>>()?;
    assert_eq!(per_record, records[1000..1100]);
    assert_eq!(per_record.concat(), out);
    Ok(())
}

#[test]
fn borrowed_filter_copies_only_selected_records() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let path = compress_body(dir.path(), "filter", b"x1\ny1\nx2\ny2\n");
    let finder = memchr::memmem::Finder::new(b"x");
    let record_filter = |record: &[u8]| finder.find(record).is_some();
    let mut reader = RecordReader::open(path.clone(), b"\n")?;
    let kept: Vec<Vec<u8>> = reader
        .records_from(0)
        .filter(|record| record.as_ref().map_or(true, |record| record_filter(record)))
        .map(|record| record.map(|record| record.to_vec()))
        .collect::<anyhow::Result<_>>()?;
    assert_eq!(kept, [b"x1\n".to_vec(), b"x2\n".to_vec()]);

    let owned_kept = RecordReader::open(path, b"\n")?
        .into_records_from(0)
        .filter(|record| record.as_ref().map_or(true, |record| record_filter(record)))
        .collect::<anyhow::Result<Vec<_>>>()?;
    assert_eq!(owned_kept, kept);

    let hits = reader
        .records_from(0)
        .try_fold(0usize, |hits, record| -> anyhow::Result<_> {
            Ok(hits + usize::from(record?.starts_with(b"x")))
        })?;
    assert_eq!(hits, 2);

    let first = reader
        .records_from(0)
        .filter_map(|record| match record {
            Ok(record) if record.starts_with(b"x") => Some(Ok(record.to_vec())),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .take(1)
        .collect::<anyhow::Result<Vec<_>>>()?;
    assert_eq!(first, [b"x1\n".to_vec()]);
    Ok(())
}

#[test]
fn final_unterminated_record_is_returned_by_both_iterators() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let path = compress_body(dir.path(), "fragment", b"a\nb\ntrailing");
    let mut reader = RecordReader::open(path.clone(), b"\n")?;
    assert_eq!(reader.records(0, 10)?, b"a\nb\ntrailing");
    assert_eq!(reader.records_from(0).take(10).to_vec()?, b"a\nb\ntrailing");
    assert_eq!(reader.records_from(2).take(1).to_vec()?, b"trailing");
    assert_eq!(reader.total_records()?, 3);
    assert_eq!(reader.record(2)?, Some(b"trailing".to_vec()));
    let owned = RecordReader::open(path.clone(), b"\n")?
        .into_records()
        .collect::<anyhow::Result<Vec<_>>>()?;
    assert_eq!(
        owned,
        [b"a\n".to_vec(), b"b\n".to_vec(), b"trailing".to_vec()]
    );
    let tail = RecordReader::open(path, b"\n")?
        .into_records_from(2)
        .collect::<anyhow::Result<Vec<_>>>()?;
    assert_eq!(tail, [b"trailing".to_vec()]);
    Ok(())
}

#[test]
fn verified_final_unterminated_record_does_not_count_as_a_separator() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let path = compress_body(dir.path(), "verified-fragment", b"a\ntrailing");
    let mut borrowed = RecordReader::open(path.clone(), b"\n")?.verifying();
    assert_eq!(borrowed.records_from(0).to_vec()?, b"a\ntrailing");
    let owned = RecordReader::open(path, b"\n")?
        .verifying()
        .into_records()
        .collect::<anyhow::Result<Vec<_>>>()?;
    assert_eq!(owned, [b"a\n".to_vec(), b"trailing".to_vec()]);
    Ok(())
}

#[test]
fn owned_and_borrowed_iterators_reject_start_beyond_last_record() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let path = compress_frames(
        dir.path(),
        "beyond-last",
        &[b"a\n".repeat(10), b"b\n".repeat(3)],
    );
    let mut borrowed = RecordReader::open(path.clone(), b"\n")?;
    let borrowed_error = borrowed.records_from(14).to_vec().unwrap_err().to_string();
    let owned = RecordReader::open(path.clone(), b"\n")?
        .into_records_from(14)
        .collect::<anyhow::Result<Vec<_>>>();
    assert_eq!(owned.unwrap_err().to_string(), borrowed_error);
    let mut reversed = RecordReader::open(path.clone(), b"\n")?
        .into_records_from(14)
        .rev();
    let error = reversed
        .next()
        .expect("start beyond end must fail")
        .unwrap_err();
    assert_eq!(error.to_string(), borrowed_error);
    assert!(reversed.next().is_none());
    let at_end: Vec<_> = RecordReader::open(path, b"\n")?
        .into_records_from(13)
        .rev()
        .collect::<anyhow::Result<Vec<_>>>()?;
    assert!(at_end.is_empty());
    Ok(())
}

#[test]
fn zero_count_still_rejects_start_past_end() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let path = compress_body(dir.path(), "zero", b"a\n");
    let mut reader = RecordReader::open(path, b"\n")?;
    assert!(reader.records(100, 0).is_err());
    // Standard Take(0) does not poll its source, so it cannot report this error.
    assert!(reader.records_from(100).take(0).to_vec()?.is_empty());
    assert!(reader.records(1, 0).is_err());
    Ok(())
}

#[test]
fn unterminated_nonfinal_frame_is_rejected_by_both_verified_iterators() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let path = compress_frames(
        dir.path(),
        "middle-fragment",
        &[b"a\npartial".to_vec(), b"b\n".to_vec()],
    );
    let mut borrowed = RecordReader::open(path.clone(), b"\n")?.verifying();
    assert!(borrowed.records_from(0).to_vec().is_err());
    let owned = RecordReader::open(path, b"\n")?.verifying();
    assert!(
        owned
            .into_records()
            .collect::<anyhow::Result<Vec<_>>>()
            .is_err()
    );
    Ok(())
}

#[test]
fn unterminated_nonfinal_frame_is_rejected_without_verification() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let path = compress_frames(
        dir.path(),
        "middle-fragment-no-verify",
        &[b"a\npartial".to_vec(), b"b\n".to_vec()],
    );
    let mut borrowed = RecordReader::open(path.clone(), b"\n")?;
    assert!(borrowed.records_from(0).to_vec().is_err());
    let owned = RecordReader::open(path.clone(), b"\n")?
        .into_records()
        .collect::<anyhow::Result<Vec<_>>>();
    assert!(owned.is_err());
    let reversed = RecordReader::open(path, b"\n")?
        .into_records()
        .rev()
        .collect::<anyhow::Result<Vec<_>>>();
    assert!(reversed.is_err());
    Ok(())
}
