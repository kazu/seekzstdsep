//! Separator-aware seekable Zstandard compression.
//!
//! Frames are cut at separator boundaries. With `is_same_separator_cnt` set, every frame holds the
//! same number of separators, which is what lets [`crate::RecordReader`] locate a record by division instead
//! of scanning. See `docs/format.md` for the format and the invariant.
//!
//! [`convert_to_seekable_zst_reader`] streams from any `Read` to any `Write`.
//! [`compress_to_seekable_zst_with_opts`] needs `Read + Seek` and writes to
//! [`CompressOptions::out_path`], but can retry when the framing derived from the first frame does
//! not fit.
use std::{
    io::{Read, Seek, SeekFrom, Write, copy},
    path::PathBuf,
};

use crate::record;

pub use zeekstd::CompressionLevel;
use zeekstd::{Decoder, EncodeOptions, Encoder, FrameSizePolicy, SEEKABLE_MAX_FRAME_SIZE};

use memchr::memmem::Finder;

const LIMIT_SEP_BUF_MULTIPLIER: usize = 4;

/// How much every reader in the crate takes from its source at a time.
pub(crate) const READ_BUF_SIZE: usize = 32768; // 大きなバッファでI/O削減

/// How much of a decompressed frame is held at once.
///
/// It is the whole of [`record::Reader`]'s window, which never grows: how large a frame is belongs
/// to the file rather than to this crate, and what a read holds should not follow it. A caller that
/// does need a frame whole — `edit::FrameReader` — starts here and grows to the frames it is asked
/// for.
///
/// Equal to [`READ_BUF_SIZE`] for now because nothing yet says it should differ, and separate from
/// it because the two answer different questions.
pub(crate) const READ_FRAME_BUF_SIZE: usize = READ_BUF_SIZE;

/// Shorthand for [`convert_to_seekable_zst_reader`] with `is_same_separator_cnt` set to `false`.
///
/// Frames are cut by size alone, so [`crate::RecordReader`] cannot locate records in the result.
pub fn convert_text_to_seekable_zst_reader<R: Read, W: Write>(
    reader: R,
    writer: W,
    frame_size: usize,
    separator: &[u8],
) -> anyhow::Result<()> {
    convert_to_seekable_zst_reader(reader, writer, frame_size, false, separator, None)
}

/// Options for [`compress_to_seekable_zst_with_opts`].
#[derive(Debug, Clone)]
pub struct CompressOptions {
    /// Separators per frame. `None` derives it from the first frame.
    pub max_of_separator: Option<usize>,
    /// Directory for the staging file. `None` uses the system temporary directory. Placing it on
    /// the same filesystem as `out_path` lets the final move use a reflink.
    pub out_dir: Option<PathBuf>,
    /// Destination path. Output is delivered here.
    pub out_path: Option<PathBuf>,
    /// Whether each frame ends with a 32-bit content checksum.
    pub checksum: bool,
    /// Zstandard compression level. 0 uses the zstd default.
    pub level: i32,
}

/// Everything derived or unset, except `checksum`, which is written unless a caller says otherwise.
impl Default for CompressOptions {
    fn default() -> Self {
        Self {
            max_of_separator: None,
            out_dir: None,
            out_path: None,
            checksum: true,
            level: CompressionLevel::default(),
        }
    }
}

/// Compresses `reader` to `writer`, cutting frames at `separator`.
///
/// Set `is_same_separator_cnt` to hold the separator count uniform across frames, which
/// [`crate::RecordReader`] requires. `frame_size` is a target in bytes, not a bound; see
/// [`convert_to_seekable_zst_reader_with_opts`] for framing and for the lower bound on
/// `frame_size * limit_multiplier`.
///
/// # Examples
///
/// ```
/// use seekzstdsep::convert_to_seekable_zst_reader;
///
/// let input: &[u8] = b"record 1\nrecord 2\nrecord 3\n";
/// let mut compressed = Vec::new();
///
/// convert_to_seekable_zst_reader(input, &mut compressed, 64 * 1024, true, b"\n", None)?;
/// assert!(!compressed.is_empty());
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn convert_to_seekable_zst_reader<R: Read, W: Write>(
    reader: R,
    writer: W,
    frame_size: usize,
    is_same_separator_cnt: bool,
    separator: &[u8],
    limit_multiplier: Option<usize>,
) -> anyhow::Result<()> {
    convert_to_seekable_zst_reader_with_opts(
        reader,
        writer,
        frame_size,
        is_same_separator_cnt,
        separator,
        limit_multiplier,
        None,
    )
}
#[derive(Debug)]
struct CompressErrorData {
    current_line_num_per_frame: i64,
}
impl std::fmt::Display for CompressErrorData {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "current_line_num_per_frame: {} ",
            self.current_line_num_per_frame
        )
    }
}

/// Blanket marker for `Read + Seek`, required by [`compress_to_seekable_zst_with_opts`].
pub trait ReadSeekable: Read + Seek {}

// 2. Read と Seek を両方実装している全ての型に対し、自動的に ReadSeek を実装
impl<T: Read + Seek> ReadSeekable for T {}

/// [`compress_to_seekable_zst_with_opts`] without options.
///
/// Passing no options leaves [`CompressOptions::out_path`] unset, so the output goes to `owriter`.
pub fn compress_to_seekable_zst<R: ReadSeekable, W: Write>(
    mut reader: R,
    mut owriter: W,
    frame_size: usize,
    is_same_separator_cnt: bool,
    separator: &[u8],
    limit_multiplier: Option<usize>,
) -> anyhow::Result<()> {
    compress_to_seekable_zst_with_opts(
        &mut reader,
        &mut owriter,
        frame_size,
        is_same_separator_cnt,
        separator,
        limit_multiplier,
        None,
    )
}

/// Compresses to [`CompressOptions::out_path`], retrying when the derived framing does not fit.
///
/// The separators-per-frame count is taken from the first frame and can fail on later frames when
/// record sizes vary. This function then halves it, rewinds, and starts over, which is why `R` must
/// be `Read + Seek`.
///
/// Output is staged in a temporary file and cloned to [`CompressOptions::out_path`] with reflink,
/// avoiding a second copy of the data. `owriter` receives it when reflink fails, and when there is
/// no `out_path` to clone onto.
pub fn compress_to_seekable_zst_with_opts<R: ReadSeekable, W: Write>(
    mut reader: R,
    mut owriter: W,
    oframe_size: usize,
    is_same_separator_cnt: bool,
    separator: &[u8],
    limit_multiplier: Option<usize>,
    opt_args: Option<CompressOptions>,
) -> anyhow::Result<()> {
    let mut opts = opt_args.clone();
    let p_opts = &mut opts.clone();

    let tmp_dir = if p_opts.is_some() && p_opts.clone().unwrap().out_dir.is_some() {
        p_opts.clone().unwrap().out_dir.unwrap().to_path_buf()
    } else {
        use std::env;
        env::temp_dir()
    };

    let tmp_file = tempfile::NamedTempFile::new_in(tmp_dir)?;
    let tmp_path = tmp_file.path().to_path_buf();
    let mut writer = tmp_file.reopen()?;
    let mut frame_size = oframe_size;

    loop {
        match convert_to_seekable_zst_reader_with_opts(
            &mut reader,
            &mut writer,
            frame_size,
            is_same_separator_cnt,
            separator,
            limit_multiplier,
            opts.clone(),
        ) {
            Ok(data) => {
                writer.seek(SeekFrom::Start(0))?;
                // copy(&mut writer, &mut owriter)?;
                let shoud_fallback =
                    if p_opts.is_some() && p_opts.clone().unwrap().out_path.is_some() {
                        let path = p_opts.clone().unwrap().out_path.unwrap();
                        let _ = std::fs::remove_file(&path);
                        let r = reflink_copy::reflink_or_copy(&tmp_path, &path);
                        tracing::info!("reflink copy result: {:?}", r);
                        r.is_err()
                    } else {
                        // Nothing to reflink onto, so `owriter` is the only destination there is.
                        true
                    };
                // owriterがファイルの場合は、直接ファイルに書き込む
                if shoud_fallback {
                    copy(&mut writer, &mut owriter)?;
                } else {
                    let _ = std::fs::remove_file(&tmp_path);
                }

                return Ok(data);
            } // 成功したらループを抜ける
            Err(e) => {
                if let Some(compress_error_data) = e.downcast_ref::<CompressErrorData>() {
                    tracing::warn!(
                        "CompressErrorData: {}. Retrying with updated options.",
                        compress_error_data
                    );
                    let mut line_num_per_frame =
                        compress_error_data.current_line_num_per_frame as usize;
                    if line_num_per_frame < 2 {
                        frame_size = frame_size * 2; // フレームサイズを倍にする
                    } else {
                        line_num_per_frame = line_num_per_frame / 2;
                    }

                    opts = Some(CompressOptions {
                        max_of_separator: Some(line_num_per_frame),
                        ..p_opts.clone().unwrap_or_default()
                    });
                    reader
                        .seek(std::io::SeekFrom::Start(0))
                        .expect("fail to seek(start)"); // 読み込み位置をリセット

                    writer
                        .seek(std::io::SeekFrom::Start(0))
                        .expect("fail to seek(start)"); // 書き込み位置をリセット
                    writer.set_len(0).expect("fail to set_len(0)");
                    continue;
                } else {
                    return Err(e); // その他のエラーはそのまま返す
                }
            }
        }
    }
}

/// The core compressor. Every other compression entry point routes through this.
///
/// `frame_size` is a target, not a bound: a frame ends at the first separator at or after it. With
/// `is_same_separator_cnt`, only the first frame is cut that way, and the separator count it
/// happened to contain becomes the count every later frame must match. Byte sizes therefore drift
/// while the record count stays fixed.
///
/// `limit_multiplier` (default 4) bounds unprocessed data. The check runs after each read of the
/// 32768-byte internal buffer, so `frame_size * limit_multiplier` below 32768 fails on any input
/// larger than that limit.
///
/// Runs [`new_convert_to_seekable_zst_reader_with_opts`] or
/// [`old_convert_to_seekable_zst_reader_with_opts`] according to [`CURRENT_COMPRESSOR`]. The two have to
/// agree byte for byte, which `tests/compress_equivalence.rs` is where measures.
pub fn convert_to_seekable_zst_reader_with_opts<R: Read, W: Write>(
    reader: R,
    writer: W,
    frame_size: usize,
    is_same_separator_cnt: bool,
    separator: &[u8],
    limit_multiplier: Option<usize>,
    opt_args: Option<CompressOptions>,
) -> anyhow::Result<()> {
    if CURRENT_COMPRESSOR {
        new_convert_to_seekable_zst_reader_with_opts(
            reader,
            writer,
            frame_size,
            is_same_separator_cnt,
            separator,
            limit_multiplier,
            opt_args,
        )
    } else {
        #[allow(deprecated)]
        old_convert_to_seekable_zst_reader_with_opts(
            reader,
            writer,
            frame_size,
            is_same_separator_cnt,
            separator,
            limit_multiplier,
            opt_args,
        )
    }
}

/// Which of the two compressors [`convert_to_seekable_zst_reader_with_opts`] runs.
///
/// `old_` is the implementation the extraction started from, kept as it was so the extracted one
/// has something to be measured against. Turning this off runs it everywhere, which is how a
/// disagreement gets narrowed down and how the two are timed against each other.
///
/// The two no longer only differ in structure: the extracted one asks for a frame size policy and
/// the old one does not, so they cut differently once a frame's records exceed 2 MiB. See
/// `tests/compress_equivalence.rs`.
const CURRENT_COMPRESSOR: bool = true;

/// The compressor as it was before the buffering and the separator scan were taken out of it.
///
/// Kept as it was, calling its own copies of the two frame writers, so that it states what the
/// extracted one has to do. Nothing but the equivalence test should call it.
#[deprecated(
    since = "0.3.0",
    note = "the compressor before record::Stream was taken out of it, kept only so that tests/compress_equivalence.rs has something to measure the extracted one against"
)]
pub fn old_convert_to_seekable_zst_reader_with_opts<R: Read, W: Write>(
    mut reader: R,
    mut writer: W,
    frame_size: usize,
    is_same_separator_cnt: bool,
    separator: &[u8],
    limit_multiplier: Option<usize>,
    opt_args: Option<CompressOptions>,
) -> anyhow::Result<()> {
    let limit_multiplier = if limit_multiplier.is_none() {
        LIMIT_SEP_BUF_MULTIPLIER
    } else {
        limit_multiplier.unwrap()
    };
    let mut encoder: Encoder<'_, _> = EncodeOptions::new()
        .checksum_flag(opt_args.as_ref().is_none_or(|o| o.checksum))
        .into_encoder(&mut writer)?;
    let mut buf = Vec::with_capacity(frame_size * limit_multiplier); // 全データを連続配置
    let mut read_buf = [0u8; READ_BUF_SIZE];
    let mut frame_end = 0; // 現在のフレームの終端位置（セパレータ除く）
    let mut search_start = 0; // 未処理データの開始位置
    let mut max_of_separator: i64 = -1; // 同一セパレータの最大数（is_same_separator_cntがtrueの場合）
    let mut cnt_of_seprator: usize = 0;
    let mut written_frame = 0;

    if opt_args.is_some() {
        max_of_separator = opt_args
            .unwrap()
            .max_of_separator
            .map(|v| v as i64)
            .unwrap_or(-1);
    }

    if separator.is_empty() {
        return Err(anyhow::anyhow!("Separator must not be empty"));
    }
    let finder = Finder::new(separator);

    loop {
        let n = reader.read(&mut read_buf)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&read_buf[..n]); // read_bufからのコピーのみ

        // 未処理データが過度に増大するのを防ぐ
        let limit = frame_size.saturating_mul(limit_multiplier);
        if is_same_separator_cnt && buf.len() - search_start > limit {
            return Err(anyhow::anyhow!(CompressErrorData {
                current_line_num_per_frame: max_of_separator,
            }));
        }

        if buf.len() - search_start > limit {
            return Err(anyhow::anyhow!(
                "No separator was found before reaching the limit size ({} > {}). Please change the frame_size value or the limit_multiplier value. Current unprocessed data size: {}",
                buf.len() - search_start,
                limit,
                limit_multiplier,
            ));
        }

        let _buf_start = search_start;
        // セパレータを探してフレームを構築
        // Spelled out rather than taken from record, which the extraction reaches: a copy that
        // shares the rule it is measuring cannot measure it.
        while let Some(pos_after) = finder
            .find(&buf[search_start..])
            .map(|pos| pos + separator.len())
        {
            // while let Some(pos_after) = find_separator_after(&buf[search_start..], separator) {
            let absolute_pos_after = search_start + pos_after;
            let chunk_end = absolute_pos_after - separator.len(); // チャンクの終端（セパレータ除く）

            if is_same_separator_cnt {
                cnt_of_seprator += 1;
            }

            // フレームにチャンクを追加（データは既に連続配置されている）
            if frame_end == 0 {
                // 最初のチャンク
                frame_end = chunk_end;
            } else {
                // 2つ目以降のチャンク（セパレータを含めてframe_endを更新）
                frame_end = chunk_end;
            }

            search_start = absolute_pos_after;

            let result = if is_same_separator_cnt {
                old_encode_frame_on_same_separator_cnt(
                    &mut frame_end,
                    frame_size,
                    separator,
                    &mut encoder,
                    &mut buf,
                    &mut search_start,
                    &mut cnt_of_seprator,
                    &mut written_frame,
                    &mut max_of_separator,
                )
            } else {
                old_encode_frame_not_same_separator_cnt(
                    &mut frame_end,
                    frame_size,
                    separator,
                    &mut encoder,
                    &mut buf,
                    &mut search_start,
                    &mut cnt_of_seprator,
                    &mut written_frame,
                )
            };
            if result.is_err() {
                return Err(result.err().unwrap());
            }
        }
        if is_same_separator_cnt && max_of_separator == -1 {
            continue;
        }
        if is_same_separator_cnt && cnt_of_seprator as i64 != max_of_separator {
            continue;
        }

        // メモリ効率化: 処理済みデータを削除
        if frame_end > frame_size {
            buf.drain(..frame_end);
            search_start -= frame_end;
            frame_end = 0;
        }
    }

    // 残りの処理
    if !buf.is_empty() {
        if written_frame > 0 {
            encoder.end_frame()?;
        }
        encoder.compress(&buf)?;
    }

    encoder.finish()?;
    Ok(())
}

/// The compressor, with the buffering and the separator scan taken out of it.
pub fn new_convert_to_seekable_zst_reader_with_opts<R: Read, W: Write>(
    reader: R,
    mut writer: W,
    frame_size: usize,
    is_same_separator_cnt: bool,
    separator: &[u8],
    limit_multiplier: Option<usize>,
    opt_args: Option<CompressOptions>,
) -> anyhow::Result<()> {
    let limit_multiplier = if limit_multiplier.is_none() {
        LIMIT_SEP_BUF_MULTIPLIER
    } else {
        limit_multiplier.unwrap()
    };
    let mut encoder = frame_encoder(
        &mut writer,
        opt_args.as_ref().is_none_or(|o| o.checksum),
        opt_args
            .as_ref()
            .map_or(CompressionLevel::default(), |o| o.level),
    )?;
    let buf = Vec::with_capacity(frame_size * limit_multiplier); // 全データを連続配置
    let mut frame_end = 0; // 現在のフレームの終端位置（セパレータ除く）
    let mut max_of_separator: i64 = -1; // 同一セパレータの最大数（is_same_separator_cntがtrueの場合）
    let mut cnt_of_seprator: usize = 0;
    let mut written_frame = 0;

    if opt_args.is_some() {
        max_of_separator = opt_args
            .unwrap()
            .max_of_separator
            .map(|v| v as i64)
            .unwrap_or(-1);
    }

    if separator.is_empty() {
        return Err(anyhow::anyhow!("Separator must not be empty"));
    }
    let finder = Finder::new(separator);
    let mut stream = record::Stream::from_buffer(reader, &finder, separator.len(), buf);

    loop {
        if !stream.fill()? {
            break;
        }

        // 未処理データが過度に増大するのを防ぐ
        let limit = frame_size.saturating_mul(limit_multiplier);
        if is_same_separator_cnt && stream.unscanned() > limit {
            return Err(anyhow::anyhow!(CompressErrorData {
                current_line_num_per_frame: max_of_separator,
            }));
        }

        if stream.unscanned() > limit {
            return Err(anyhow::anyhow!(
                "No separator was found before reaching the limit size ({} > {}). Please change the frame_size value or the limit_multiplier value. Current unprocessed data size: {}",
                stream.unscanned(),
                limit,
                limit_multiplier,
            ));
        }

        let _buf_start = stream.last_end();
        // セパレータを探してフレームを構築
        while let Some(absolute_pos_after) = stream.next_end() {
            let chunk_end = absolute_pos_after - separator.len(); // チャンクの終端（セパレータ除く）

            if is_same_separator_cnt {
                cnt_of_seprator += 1;
            }

            // フレームにチャンクを追加（データは既に連続配置されている）
            if frame_end == 0 {
                // 最初のチャンク
                frame_end = chunk_end;
            } else {
                // 2つ目以降のチャンク（セパレータを含めてframe_endを更新）
                frame_end = chunk_end;
            }

            let result = if is_same_separator_cnt {
                encode_frame_on_same_separator_cnt(
                    &mut frame_end,
                    frame_size,
                    separator,
                    &mut encoder,
                    &mut stream,
                    &mut cnt_of_seprator,
                    &mut written_frame,
                    &mut max_of_separator,
                )
            } else {
                encode_frame_not_same_separator_cnt(
                    &mut frame_end,
                    frame_size,
                    separator,
                    &mut encoder,
                    &mut stream,
                    &mut cnt_of_seprator,
                    &mut written_frame,
                )
            };
            if result.is_err() {
                return Err(result.err().unwrap());
            }
        }
        if is_same_separator_cnt && max_of_separator == -1 {
            continue;
        }
        if is_same_separator_cnt && cnt_of_seprator as i64 != max_of_separator {
            continue;
        }

        // メモリ効率化: 処理済みデータを削除
        if frame_end > frame_size {
            stream.drop_front(frame_end);
            frame_end = 0;
        }
    }

    // 残りの処理
    if !stream.buffered().is_empty() {
        if written_frame > 0 {
            encoder.end_frame()?;
        }
        encoder.compress(stream.buffered())?;
    }

    encoder.finish()?;
    Ok(())
}

/// Deprecated: セパレータを効率的に検索する関数（セパレータの後の位置を返す）
fn _find_separator_after(data: &[u8], separator: &[u8]) -> Option<usize> {
    if separator.len() == 1 {
        // 単一バイトセパレータの場合は専用の最適化
        let sep_byte = separator[0];
        data.iter().position(|&b| b == sep_byte).map(|pos| pos + 1)
    } else if separator.len() > data.len() {
        // セパレータがデータより長い場合は見つからない
        None
    } else {
        // 複数バイトセパレータの場合
        let max_start = data.len() - separator.len();
        for i in 0..=max_start {
            if data[i..i + separator.len()] == *separator {
                return Some(i + separator.len());
            }
        }
        None
    }
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum FrameEncodeResult {
    Skipped,
    Encoded, // 圧縮されたフレームのサイズ
}

// !is_same_separator_cnt
fn encode_frame_not_same_separator_cnt<R: Read, W: Write>(
    frame_end: &mut usize,
    frame_size: usize,
    separator: &[u8],
    encoder: &mut Encoder<'_, &mut W>,
    stream: &mut record::Stream<'_, R>,
    cnt_of_seprator: &mut usize,
    written_frame: &mut usize,
) -> anyhow::Result<FrameEncodeResult> {
    if *frame_end < frame_size {
        return Ok(FrameEncodeResult::Skipped);
    }

    if *frame_end > separator.len() {
        // 直前のフレームはここで閉じる。最後のフレームだけは finish() に閉じさせる。
        if *written_frame > 0 {
            encoder.end_frame()?;
        }
        let n = encoder.compress(&stream.buffered()[..(*frame_end) + separator.len()])?;

        tracing::trace!(
            cnt_of_seprator = cnt_of_seprator,
            written = n,
            frame_idx = written_frame,
            "compressing frame with cnt_of_seprator",
        );
        *written_frame += 1;
    }
    stream.drop_to_last_end();
    *frame_end = 0;

    return Ok(FrameEncodeResult::Encoded);
}

/// [`old_convert_to_seekable_zst_reader_with_opts`]'s copy of [`encode_frame_not_same_separator_cnt`].
fn old_encode_frame_not_same_separator_cnt<W: Write>(
    frame_end: &mut usize,
    frame_size: usize,
    separator: &[u8],
    encoder: &mut Encoder<'_, &mut W>,
    buf: &mut Vec<u8>,
    search_start: &mut usize,
    cnt_of_seprator: &mut usize,
    written_frame: &mut usize,
) -> anyhow::Result<FrameEncodeResult> {
    if *frame_end < frame_size {
        return Ok(FrameEncodeResult::Skipped);
    }

    if *frame_end > separator.len() {
        // 直前のフレームはここで閉じる。最後のフレームだけは finish() に閉じさせる。
        if *written_frame > 0 {
            encoder.end_frame()?;
        }
        let n = encoder.compress(&buf[..(*frame_end) + separator.len()])?;

        tracing::trace!(
            cnt_of_seprator = cnt_of_seprator,
            written = n,
            frame_idx = written_frame,
            "compressing frame with cnt_of_seprator",
        );
        *written_frame += 1;
    }
    buf.drain(..(*search_start));
    *search_start = 0;
    *frame_end = 0;

    return Ok(FrameEncodeResult::Encoded);
}

fn encode_frame_on_same_separator_cnt<R: Read, W: Write>(
    frame_end: &mut usize,
    frame_size: usize,
    separator: &[u8],
    encoder: &mut Encoder<&mut W>,
    stream: &mut record::Stream<'_, R>,
    cnt_of_seprator: &mut usize,
    written_frame: &mut usize,
    max_of_separator: &mut i64,
) -> anyhow::Result<FrameEncodeResult> {
    let is_first_frame = *max_of_separator == -1;

    if is_first_frame {
        let result = encode_frame_not_same_separator_cnt(
            frame_end,
            frame_size,
            separator,
            encoder,
            stream,
            cnt_of_seprator,
            written_frame,
        );
        let copied_result = result
            .as_ref()
            .map(|res| *res)
            .map_err(|e| anyhow::anyhow!(e.to_string()));

        if copied_result.is_ok() && copied_result.unwrap() == FrameEncodeResult::Encoded {
            *max_of_separator = (*cnt_of_seprator) as i64;
            *cnt_of_seprator = 0;
        }

        return result;
    }
    let no_ref_max_of_separator: i64 = *max_of_separator;

    if no_ref_max_of_separator > (*cnt_of_seprator) as i64 {
        return Ok(FrameEncodeResult::Skipped);
    }

    // 直前のフレームはここで閉じる。最後のフレームだけは finish() に閉じさせる。
    if *written_frame > 0 {
        encoder.end_frame()?;
    }
    let n = encoder.compress(&stream.buffered()[..(*frame_end) + separator.len()])?;

    tracing::trace!(
        cnt_of_seprator = cnt_of_seprator,
        written = n,
        frame_idx = written_frame,
        "compressing frame with cnt_of_seprator",
    );
    *written_frame += 1;
    *cnt_of_seprator = 0;
    stream.drop_to_last_end();
    *frame_end = 0;
    return Ok(FrameEncodeResult::Encoded);
}

/// [`old_convert_to_seekable_zst_reader_with_opts`]'s copy of [`encode_frame_on_same_separator_cnt`].
fn old_encode_frame_on_same_separator_cnt<W: Write>(
    frame_end: &mut usize,
    frame_size: usize,
    separator: &[u8],
    encoder: &mut Encoder<&mut W>,
    buf: &mut Vec<u8>,
    search_start: &mut usize,
    cnt_of_seprator: &mut usize,
    written_frame: &mut usize,
    max_of_separator: &mut i64,
) -> anyhow::Result<FrameEncodeResult> {
    let is_first_frame = *max_of_separator == -1;

    if is_first_frame {
        let result = old_encode_frame_not_same_separator_cnt(
            frame_end,
            frame_size,
            separator,
            encoder,
            buf,
            search_start,
            cnt_of_seprator,
            written_frame,
        );
        let copied_result = result
            .as_ref()
            .map(|res| *res)
            .map_err(|e| anyhow::anyhow!(e.to_string()));

        if copied_result.is_ok() && copied_result.unwrap() == FrameEncodeResult::Encoded {
            *max_of_separator = (*cnt_of_seprator) as i64;
            *cnt_of_seprator = 0;
        }

        return result;
    }
    let no_ref_max_of_separator: i64 = *max_of_separator;

    if no_ref_max_of_separator > (*cnt_of_seprator) as i64 {
        return Ok(FrameEncodeResult::Skipped);
    }

    // 直前のフレームはここで閉じる。最後のフレームだけは finish() に閉じさせる。
    if *written_frame > 0 {
        encoder.end_frame()?;
    }
    let n = encoder.compress(&buf[..(*frame_end) + separator.len()])?;

    tracing::trace!(
        cnt_of_seprator = cnt_of_seprator,
        written = n,
        frame_idx = written_frame,
        "compressing frame with cnt_of_seprator",
    );
    *written_frame += 1;
    *cnt_of_seprator = 0;
    buf.drain(..(*search_start));
    *search_start = 0;
    *frame_end = 0;
    return Ok(FrameEncodeResult::Encoded);
}

/// Returns `cnt_of_sep` records of the region starting at decompressed offset `frame_start`,
/// beginning after its `start_sep_cnt`-th separator.
///
/// Both are counts of separators, not offsets into the region: `start_sep_cnt` is how many to skip
/// and `cnt_of_sep` how many to return, each record carrying its own separator. Fewer are returned
/// when the region holds fewer.
///
/// The region is decoded one fixed window at a time, so neither the region nor a record has to fit
/// in memory: a record longer than the window is written on in pieces.
pub fn records_between_by_separator_in_frame<'a>(
    decoder: &mut Decoder<'a, std::fs::File>,
    frame_start: u64,
    frame_len: u64,
    start_sep_cnt: u64,
    cnt_of_sep: u64,
    finder: &Finder,
    separator: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let reader = record::region(decoder, frame_start, frame_len)?;
    let mut out = Vec::new();
    reader
        .records(finder, separator.len())
        .skip_records(start_sep_cnt)?
        .take_records(cnt_of_sep)
        .write_to(&mut out)?;
    Ok(out)
}

/// Renamed to [`records_between_by_separator_in_frame`]: a separator is not a newline, so what
/// lies between two of them is a record rather than a line.
#[deprecated(
    since = "0.4.90",
    note = "renamed to records_between_by_separator_in_frame"
)]
pub fn lines_between_by_separator_in_frame<'a>(
    decoder: &mut Decoder<'a, std::fs::File>,
    frame_start: u64,
    frame_len: u64,
    start_sep_cnt: u64,
    cnt_of_sep: u64,
    finder: &Finder,
    separator: &[u8],
) -> anyhow::Result<Vec<u8>> {
    records_between_by_separator_in_frame(
        decoder,
        frame_start,
        frame_len,
        start_sep_cnt,
        cnt_of_sep,
        finder,
        separator,
    )
}

/// Superseded by [`records_between_by_separator_in_frame`], which fixes the name and takes a count
/// of separators in place of `end_sep_cnt`.
///
/// `end_sep_cnt` is inclusive, so this returns `end_sep_cnt - start_sep_cnt + 1` records. Kept for
/// compatibility, down to dropping the last byte when the closing separator is not found.
#[deprecated(
    since = "0.3.0",
    note = "renamed to records_between_by_separator_in_frame, which takes a count of separators instead of an inclusive end index"
)]
pub fn lines_betwee_by_separator_in_frame<'a>(
    decoder: &mut Decoder<'a, std::fs::File>,
    frame_start: u64,
    frame_len: u64,
    start_sep_cnt: u64,
    end_sep_cnt: u64,
    finder: &Finder,
    separator: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let cnt_of_sep = end_sep_cnt.saturating_sub(start_sep_cnt) + 1;
    let mut out = records_between_by_separator_in_frame(
        decoder,
        frame_start,
        frame_len,
        start_sep_cnt,
        cnt_of_sep,
        finder,
        separator,
    )?;

    // 終端の separator が見つからなかったとき末尾 1 バイトを落とすのが、この関数の従来の挙動。
    // 見つかったかどうかは、返ってきた separator の数が要求に届いたかで分かる。
    if (record::count(&out, finder) as u64) < cnt_of_sep {
        out.pop();
    }
    Ok(out)
}

/// An encoder that ends a frame only where it is told to.
///
/// The default frame size policy ends one at 2 MiB of uncompressed data, wherever that lands, so a
/// frame holding more than that is split at a byte count rather than at a record count and the file
/// gets frames of differing record counts in its interior. `SEEKABLE_MAX_FRAME_SIZE` is the highest
/// a policy can ask for, and it cannot be raised: the check is `MAX_FRAME_SIZE.min(limit)`.
pub(crate) fn frame_encoder<W: Write>(
    writer: W,
    checksum: bool,
    level: i32,
) -> anyhow::Result<Encoder<'static, W>> {
    Ok(EncodeOptions::new()
        .checksum_flag(checksum)
        .compression_level(level)
        .frame_size_policy(FrameSizePolicy::Uncompressed(
            SEEKABLE_MAX_FRAME_SIZE as u32,
        ))
        .into_encoder(writer)?)
}

/// The decompressed bytes in `[start, start + len)`.
///
/// One `read`, as both of the sites this was taken from did. `Decoder::read` may return fewer bytes
/// than the buffer holds without that being an error, and the unfilled tail is then NUL; see
/// `docs/bugs.md`. The two sites differed in what they did with a read error, one panicking and one
/// returning it — a shared body can only do one, and it returns it.
pub(crate) fn decompressed_range<S: zeekstd::Seekable>(
    decoder: &mut Decoder<'_, S>,
    start: u64,
    len: u64,
) -> anyhow::Result<Vec<u8>> {
    let mut data = Vec::new();
    decompressed_range_into(decoder, start, len, &mut data)?;
    Ok(data)
}

/// [`decompressed_range`] into a buffer the caller owns, for one that reads frame after frame and
/// would otherwise allocate for each.
///
/// `data` is emptied and filled with NUL first, so what a short `read` leaves behind is the NUL the
/// caller allocating a fresh buffer would have got, not the frame read before it.
pub(crate) fn decompressed_range_into<S: zeekstd::Seekable>(
    decoder: &mut Decoder<'_, S>,
    start: u64,
    len: u64,
    data: &mut Vec<u8>,
) -> anyhow::Result<()> {
    decoder.seek(SeekFrom::Start(start))?;
    data.clear();
    data.resize(len as usize, 0);
    let _n = decoder.read(&mut data[..])?;
    Ok(())
}

/// Counts separators in the decompressed region `[start, start + len)`.
///
/// The region is decoded one fixed window at a time and dropped as it is scanned, so a frame does
/// not have to fit in memory to be counted, whatever it holds.
pub fn cnt_of_separetor_in_frame<'a>(
    decoder: &mut Decoder<'a, std::fs::File>,
    start: u64,
    len: u64,
    finder: &Finder,
    separator: &[u8],
) -> anyhow::Result<usize> {
    if len == 0 {
        return Ok(0);
    }

    record::region(decoder, start, len)?
        .records(finder, separator.len())
        .count_records()
}

/// Counts separators in an already-decoded buffer. `finder` must match `_separator`, which is unused.
pub fn cnt_of_separetor_in_frame_via_buf(
    data: &[u8],
    finder: &Finder,
    _separator: &[u8], // start, len
) -> anyhow::Result<usize> {
    Ok(record::count(data, finder))
}

/// Superseded by [`cnt_of_separetor_in_frame_via_buf`].
// FIXME: `data.len() - sep_len` underflows and panics when data is shorter than separator.
pub fn old_cnt_of_separetor_in_frame_via_buf(
    data: &[u8],
    separator: &[u8], // start, len
) -> anyhow::Result<usize> {
    let mut search_start = 0;
    let sep_len = separator.len();
    let max_pos = data.len() - sep_len;
    let mut total_count = 0;

    if separator.len() == 1 {
        let sep_byte = separator[0];
        return Ok(data.iter().filter(|&&byte| byte == sep_byte).count());
    }

    while search_start <= max_pos {
        if data[search_start..search_start + sep_len] == *separator {
            total_count += 1;
            search_start += sep_len;
        } else {
            search_start += 1;
        }
    }
    Ok(total_count)
}

/// Returns each frame's `(decompressed_start, decompressed_length)`, or `None` if there are none.
pub fn seek_table_decomp_frames<'a>(
    decoder: &Decoder<'a, std::fs::File>,
) -> Option<Vec<(u64, u64)>> {
    let seek_table = decoder.seek_table();
    let n_u32 = seek_table.num_frames();
    if n_u32 == 0 {
        return None;
    }
    let n = n_u32 as usize;
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(n);
    let mut prev_end: u64 = 0;
    for i in 0..n_u32 {
        let end = match seek_table.frame_end_decomp(i) {
            Ok(v) => v,
            Err(_) => return None,
        };
        let start = prev_end;
        let len = end.saturating_sub(start);
        out.push((start, len));
        prev_end = end;
    }
    Some(out)
}

use serde::{Deserialize, Serialize};

/// Layout of one frame, as reported by [`inspect`].
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct InspectResult {
    /// Compressed offset of the frame.
    pub comp_start: u64,
    /// Compressed offset just past the frame.
    pub comp_end: u64,
    /// Compressed size in bytes.
    pub comp_size: u64,
    /// Decompressed offset of the frame.
    pub decomp_start: u64,
    /// Decompressed offset just past the frame.
    pub decomp_end: u64,
    /// Decompressed size in bytes. 0 for the empty trailing frame the encoder sometimes emits.
    pub decomp_size: u64,
    /// The separator the count was measured with.
    pub sep: Vec<u8>,
    /// Separators in this frame. Extrapolated, not measured, under [`InspectOptions::fast_mode`].
    pub cnt_of_sep: usize,
}

use std::cell::RefCell;

/// Options for [`inspect_with_opts`].
#[derive(Debug, Clone)]
pub struct InspectOptions {
    /// Measure separators only in frame 0 and the last two frames, assuming that count for the
    /// rest. Set to `false` to count every frame, which is the only way to detect a frame in the
    /// interior that breaks the uniform count.
    pub fast_mode: bool,
}

/// [`inspect_with_opts`] with `fast_mode: true`.
pub fn inspect(input: PathBuf, separator: &[u8]) -> anyhow::Result<Vec<InspectResult>> {
    inspect_with_opts(input, separator, InspectOptions { fast_mode: true })
}

/// Reports the frame layout of a compressed file. Cost depends on [`InspectOptions::fast_mode`].
pub fn inspect_with_opts(
    input: PathBuf,
    separator: &[u8],
    opts: InspectOptions,
) -> anyhow::Result<Vec<InspectResult>> {
    let file = std::fs::File::open(&input).expect("fail open");
    let decoder = Decoder::new(file).expect("cannot open decoder");
    let decoder_cell = RefCell::new(decoder);

    let frames =
        seek_table_decomp_frames(&mut *decoder_cell.borrow_mut()).expect("cannot get frames");

    let frame_len = frames.len();
    let mut cache_cnt_of_sep: usize = 0;
    let finder = Finder::new(separator);
    let results = frames
        .iter()
        .enumerate()
        .map(|(i, (start, len))| {
            let i_u32 = i as u32;
            let table_info = {
                let borrow = decoder_cell.borrow();
                let seek_table = borrow.seek_table();
                (
                    seek_table.frame_start_comp(i_u32).unwrap_or(0),
                    seek_table.frame_end_comp(i_u32).unwrap_or(0),
                    seek_table.frame_size_comp(i_u32).unwrap_or(0),
                    seek_table.frame_end_decomp(i_u32).unwrap_or(0),
                    seek_table.frame_size_decomp(i_u32).unwrap_or(0),
                )
            };
            let (comp_start, comp_end, comp_size, decomp_end, decomp_size) = table_info;
            let cnt_of_sep = if !opts.fast_mode || i == 0 || i > frame_len - 3 {
                cnt_of_separetor_in_frame(
                    &mut *decoder_cell.borrow_mut(),
                    *start,
                    *len,
                    &finder,
                    separator,
                )
                // FIXME: an undecodable frame aborts the process here. See `docs/bugs.md`.
                .expect("failt to get count")
            } else {
                cache_cnt_of_sep
            };
            if opts.fast_mode && i == 0 {
                cache_cnt_of_sep = cnt_of_sep;
            }

            InspectResult {
                comp_start,
                comp_end,
                comp_size,
                decomp_start: *start,
                decomp_end,
                decomp_size,
                sep: separator.to_vec(),
                cnt_of_sep,
            }
        })
        .collect();
    Ok(results)
}
