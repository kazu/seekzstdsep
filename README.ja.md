# seekzstdsep

[![Crates.io](https://img.shields.io/crates/v/seekzstdsep.svg)](https://crates.io/crates/seekzstdsep)
[![CI](https://img.shields.io/github/actions/workflow/status/kazu/seekzstdsep/rust.yml?branch=master)](https://github.com/kazu/seekzstdsep/actions/workflows/rust.yml)
[![nushell](https://img.shields.io/badge/dynamic/yaml?url=https%3A%2F%2Fraw.githubusercontent.com%2Fkazu%2Fseekzstdsep%2Fmaster%2F.github%2Fworkflows%2Frust.yml&query=%24.env.NU_VERSION&label=nushell&color=4E9A06)](https://github.com/kazu/seekzstdsep/tree/master/nu_plugin_zstdsep)

**圧縮されたファイルから、その手前を展開せずに任意のレコードを読み出します。**

| ブランチ | nushell | タグ |
| --- | --- | --- |
| `master` | 0.115 | `nu_v.0.115` |
| `0.114/nu` | 0.114 | `nu_v.0.114` |

JSONL、CSV、TSV、logfmt — レコードが特定の文字列で区切られた形式が対象です。コマンドラインツール
としても、Rust のライブラリ crate としても使えます。

出力は独自フォーマットではありません。ごく普通の [Zstandard Seekable Format][spec] のファイルで、
フレームをどこで切るかを選んでいるだけなので、`zstd -d` でそのまま元のバイト列に戻ります。

[BGZF][bgzf] + [tabix][tabix] と同じ考え方ですが、インデックスファイルが要りません。フレームの
切り方そのものがインデックスなので、データと一緒に持ち歩いたり失くしたりするサイドカーがありません。

![ファイル内の位置ごとの、1 レコードを読むのにかかる時間](docs/bench/read-latency.svg)

1,000,000 レコードの JSONL (74.2 MB) から 1 レコードを読んだ時間。10 回の最良値、1 CPU に pin:

| 読むレコード | `seekzstdsep cat` | `tail \| head` | `zstd -dc \| tail \| head` |
| ---: | ---: | ---: | ---: |
| 0 | 0.82 ms | 0.97 ms | 1.75 ms |
| 500,000 | 0.99 ms | 7.58 ms | 19.46 ms |
| 999,000 | 1.10 ms | 13.99 ms | 36.27 ms |

2 つのベースラインは位置のぶんだけ時間を払いますが、`seekzstdsep` は払いません。すべてのフレームが
同じ数のセパレータを持つので、レコード番号は除算だけでフレーム番号になり、展開されるのはその 1
フレームだけです。全体の表と測定条件は `docs/bench/` にあります。

## インストール

Rust 1.85 以降 (edition 2024) と C コンパイラが必要です。`zstd-sys` が同梱の libzstd 1.5.7 を
ソースからビルドするので、システムの libzstd も `pkg-config` も要りません。

```sh
cargo install --path .
```

## 使う

```sh
seekzstdsep compress events.jsonl                            # -> events.jsonl.seek.zst
seekzstdsep cat events.jsonl.seek.zst --from 10000 --cnt 3   # --from は 0 始まり
seekzstdsep inspect events.jsonl.seek.zst                    # フレームごとの範囲とレコード数
```

`truncate` はファイルをフレーム境界までその場で切り詰め、何も再エンコードしません。`append` は
その場で追記し、再エンコードするのは編集が落ちたフレームだけです。`append --input-seekable` で
別の seekable ファイルを継ぐときは 1 バイトも再エンコードしません。`copy-range` はレコード範囲を
別のファイルへ書き出し、フレームはそのままコピーします。
全サブコマンドとフラグは `docs/cli.md` にあります。

ライブラリとしては、任意の `Read`/`Write` の組に対して圧縮できます:

```rust
use seekzstdsep::convert_to_seekable_zst_reader;

let input: &[u8] = b"record 1\nrecord 2\nrecord 3\n";
let mut compressed: Vec<u8> = Vec::new();

convert_to_seekable_zst_reader(
    input,
    &mut compressed,
    64 * 1024, // 目標フレームサイズ (バイト)
    true,      // フレーム間でセパレータ数を一定に保つ
    b"\n",
    None,      // limit_multiplier, デフォルトは 4
)
.unwrap();

assert!(!compressed.is_empty());
```

4 番目の引数がこのツールの核心です。`false` にするとフレームはサイズだけで切られ、`cat` は
レコード番号を計算で解決できなくなります。

## nushell plugin

`nu_plugin_zstdsep/` は同じファイルを nushell から読みます。ファイルを開いたまま値として持ちます:

```text
> let h = zstdsep open events.jsonl.seek.zst
> $h.1999999.msg
```

2,000,000 レコードのファイルで 380 µs、全体を読むと 4.9 s かかるのに対してです。`nu/install.nu` は
nushell の autoload ディレクトリに hook を置くので、`open` と `save` が `.seek.zst` のパスを自動で
plugin へ回します。詳細は `nu_plugin_zstdsep/README.ja.md` にあります。

## ドキュメント

- `docs/format.md` — ファイルの実体と、参照を計算にする不変条件
- `docs/cli.md` — 全サブコマンドとフラグ
- `docs/library.md` — API の残り: 読み出し、`truncate`、`append`、`copy_range`、オプション
- `docs/benchmark.md` — ベンチマークが何を測っているか、避けている罠は何か
- `docs/bench/` — 測定結果そのもの
- `docs/bugs.md` — 既知の問題。`--cnt` の現在の意味論を含みます

## ライセンス

MIT ([LICENSE](./LICENSE))。

[spec]: https://github.com/rorosen/zeekstd/blob/main/seekable_format.md
[bgzf]: https://www.htslib.org/doc/bgzip.html
[tabix]: https://www.htslib.org/doc/tabix.html
