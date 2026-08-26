# seekzstdsep

**レコード (セパレータで区切られたデータ) を zstd ファイルに圧縮し、その一部だけを展開して
任意のレコードを取り出せるようにする圧縮/解凍ツールです。**

JSONL、CSV、TSV、logfmt — レコードが特定の文字列 (多くの場合は改行) で区切られた形式が対象です。
`seekzstdsep` はこれらを、**任意のレコード範囲を、その手前のデータを展開せずに読み出せる**形で
圧縮します。

コマンドラインツール `seekzstdsep` としても、Rust のライブラリ crate としても使えます。

出力は**独自フォーマットではありません**。ごく普通の [Zstandard Seekable Format][spec] のファイルで、
フレームをどこで切るかを意図的に選んでいるだけです。したがって通常の `zstd -d` でそのまま展開できます。

```sh
zstd -d -c events.jsonl.seek.zst   # 元データとバイト単位で一致
```

[spec]: https://github.com/rorosen/zeekstd/blob/main/seekable_format.md

## なぜ必要か

seekable zstd は元々、手前を全部展開することなく**バイト**オフセットへ飛べます。しかしレコード区切りの
データが指定されるのは**バイトではなくレコード**です。欲しいのがレコード 1,000,000..1,000,100 だと
分かっていても、それが何バイト目なのかは手前を全部展開しない限り分かりません。

`seekzstdsep` はこの隙間を1つのルールで埋めます — **すべてのフレームが同じ数のセパレータを持つ**。
レコード番号からフレーム番号への変換は単なる除算になり、デコーダは必要なフレームだけに触ります。
フォーマットが元々持っている seek table 以外に、インデックス構造は一切必要ありません。

素の gzip や zstd では、n 番目のレコードを取るのにその手前を全部展開することになり、n に比例して
遅くなります。`seekzstdsep` が展開するのはそのレコードが入っている 1 フレームだけなので、ファイルが
どれだけ大きくても、何番目のレコードでも、ほぼ定数時間で取り出せます。

50,000 レコードの JSONL (3.7 MB) は 55 フレームに分かれた 155 KB に圧縮されます。その真ん中から
3レコード読むときに展開されるのは1フレームであって、3.7 MB ではありません。

※ 「ほぼ」はファイルを開くときに seek table を読むぶんです。seek table はフレーム 1 つにつき
1 エントリなので、フレーム数が増えればそのぶん少しだけ増えます。

ディスク上のレイアウトと不変条件の詳細は `docs/format.md` にあります。

## インストール

### 前提環境

| | |
| --- | --- |
| Rust | 1.85 以降 (この crate は edition 2024 を使用) |
| C コンパイラ | 必須 — `zstd-sys` が同梱の libzstd 1.5.7 を C からビルドします |

システムに libzstd を入れておく必要も、`pkg-config` も不要です。C ライブラリは同梱されて静的リンク
されます。`zstd` コマンド自体も不要で、上で触れたのは出力が標準フォーマットであることを示すためだけ
です。

### ビルド

```sh
cargo install --path .
```

依存 crate は [`Cargo.toml`](./Cargo.toml) を参照してください。

## リポジトリの構成

- `src/` — ライブラリと CLI の本体。
- `tests/` — 統合テスト。fixture は `tests/fixtures/` にあります。
- `examples/` — ライブラリを使う最小の例 (`cat`、`compress`、`inspect`)。
- `benches/` — この crate の criterion ベンチ。
- `bench/` — ベンチマークハーネス `szbench`。自前の `[workspace]` を持つ別 crate で、測定対象である
  本体の `Cargo.toml` には手を触れません。`benches/` とは別物です。
- `nu_plugin_zstdsep/` — nushell plugin。
- `docs/` — 設計と測定の記録。
  - `docs/format.md` — ディスク上のレイアウト、セパレータ数均一の不変条件、そこに依存しているもの。
    **フレーム分割まわりを変更する前に必ず読んでください。**
  - `docs/benchmark.md` — ベンチマークで何を測り、何と比較し、どこで数字が狂うか。
  - `docs/bench/` — 実際に測った結果。baseline と生の JSON。
  - `docs/performances.md` — 実ファイルに対する測定値。
  - `docs/design/` — 検討中の変更に関する設計メモ。
  - `docs/bugs.md` — 既知の不具合と、直したものの記録。

## CLI

サブコマンドの一覧は `seekzstdsep -h`、各サブコマンドのオプションは `seekzstdsep <サブコマンド> -h`
で出ます。以下はそのうちよく使うものです。

### 圧縮

```sh
seekzstdsep compress events.jsonl events.jsonl.seek.zst
```

`OUTPUT` を省略すると出力先は `INPUT` + `.seek.zst` になります。`INPUT` を省略すると標準入力から
読み、結果は標準出力に出ます。主なオプション:

| オプション | 意味 |
| --- | --- |
| `-s, --separator <S>` | レコードのセパレータ (デフォルト `"\n"`) |
| `--frame-size <N>` | フレームサイズの目標値、バイト単位 (デフォルト 65536) |
| `-c, --cnt-of-separator-per-frame <N>` | フレームあたりのレコード数を自動検出せず固定する |
| `-l, --limit-multiplier <N>` | `--frame-size` をどこまで超えてセパレータを探すか (デフォルト 4) |
| `--rm` | 変換成功後に入力ファイルを削除する |
| `--no-check` | フレームごとの内容チェックサムを書かない (デフォルトでは書く) |

`--frame-size` は上限ではなく目標値です。フレームは目標値を過ぎた次のセパレータで終わるため、
バイト長にはばらつきが出る一方、フレームあたりのレコード数は一定に保たれます。たいていの入力では
デフォルトのままで問題ありません。そうでない場合については `docs/format.md` を参照してください。

各フレームの末尾には内容チェックサムが付きます。読み出しが展開するそのフレームが、読みながら
検証されます。コストはフレームあたり 4 バイトで、実ファイルでの実測は `docs/performances.md` に
あります。`--no-check` で外せます。

### レコード範囲の読み出し

```sh
seekzstdsep cat events.jsonl.seek.zst --from 10000 --cnt 3
```

`--from` は 0 始まりのレコード番号です。現状の `--cnt` の挙動については[既知の問題](#既知の問題)を
参照してください。

### フレーム構成の確認

```sh
seekzstdsep inspect events.jsonl.seek.zst
seekzstdsep inspect events.jsonl.seek.zst --format json
```

フレームごとの圧縮前後の範囲とセパレータ数を表示します。あるファイルで実際に不変条件が保たれているかを
確認する一番早い方法です。デフォルトでは最初と最後の数フレームだけを実測して残りはそこから推定するので、
全フレームを数えるには `-n, --no-fast-mode` を渡してください。

### 切り詰め

ファイルをその場で先頭 `--records` レコードに切り詰めます。切れ目は必ずレコード境界です。渡す数を
決めるにはファイル全体のレコード数が要りますが、`inspect` がフレームごとのレコード数を出すので、
その合計がそれにあたります。

数え方はシェルによります。bash / zsh なら `jq` で:

```sh
seekzstdsep inspect events.jsonl.seek.zst --format json | jq '[.[].cnt_of_sep] | add'
# => 50000
```

nushell なら外部コマンドなしで:

```nu
seekzstdsep inspect events.jsonl.seek.zst --format json | from json | get cnt_of_sep | math sum
# => 50000
```

その数を見て切り詰めます。

```sh
seekzstdsep truncate events.jsonl.seek.zst --records 10000
```

再エンコードされるのは切れ目が入るフレームだけで、それより前は読みも書きもしません。seek table だけは
全体を作り直すので、そこはフレーム数に比例します。

破壊的です。元のファイルが必要なら先に複製してください。対応するファイルシステムなら
`cp --reflink=auto` が 1 ミリ秒程度で済ませます。何かを書き込む前にセパレータがそのファイルのものか
検証しますが、これには 3 フレーム以上が必要なため、ごく小さいファイルは拒否されます。

### 追記

```sh
seekzstdsep append events.jsonl.seek.zst more.jsonl
cat more.jsonl | seekzstdsep append events.jsonl.seek.zst
```

レコードをファイル末尾にその場で足します。ファイルが終わっているフレームは普通ほかより少ないレコード
しか持たないので、その後ろに足すと内部に短いフレームが残り、レコード検索が成り立たない除算をすることに
なります。そうはせず、そのフレームを復号して新しいレコードと一緒に切り直すので、最終フレーム以外は
ファイルが作られたときのレコード数に戻ります。それより前は読みも書きもしません。

最後のバイトがセパレータでないファイルはレコードではなく断片で終わっており、そのまま繋ぐと断片と
最初の追記レコードがひとつのレコードに融合します。`append` は拒否します。`--insert-separator` を
渡すと継ぎ目にセパレータを 1 個書き、断片を独立したレコードにします。セパレータを 1 個書いても
レコードにならない場合 — `\n\n` のように自分自身と重なるセパレータで起きます — も拒否します。

破壊的で、書き込む前に検証する点は上の `truncate` と同じです。

## ライブラリとして

圧縮は任意の `Read`/`Write` の組で動きます。

```rust
use seekzstdsep::convert_to_seekable_zst_reader;

let input: &[u8] = b"record 1\nrecord 2\nrecord 3\n";
let mut compressed: Vec<u8> = Vec::new();

convert_to_seekable_zst_reader(
    input,
    &mut compressed,
    64 * 1024, // フレームサイズの目標値 (バイト)
    true,      // フレームあたりのセパレータ数を揃える
    b"\n",
    None,      // limit_multiplier、デフォルトは 4
)
.unwrap();

assert!(!compressed.is_empty());
```

4番目の引数がこの crate の肝です。`false` にするとフレームはサイズだけで切られ、`cat` はレコード番号を
算術で解決できなくなります。`convert_text_to_seekable_zst_reader` は `false` を渡す短縮形です。

読み出しは seek するので実ファイルが必要です。

```rust,no_run
use seekzstdsep::{cat_data, inspect};
use std::path::PathBuf;

let path = PathBuf::from("events.jsonl.seek.zst");

// レコード番号 10000 から。
let records: Vec<u8> = cat_data(path.clone(), 10000, 3, b"\n").unwrap();
print!("{}", String::from_utf8_lossy(&records));

// フレームごとの構成。
for frame in inspect(path, b"\n").unwrap() {
    println!("{} records in {} compressed bytes", frame.cnt_of_sep, frame.comp_size);
}
```

`truncate` はファイルをその場で切り詰めます。読み書き両方で開いておく必要があります。

```rust,no_run
use seekzstdsep::truncate;
use std::fs::File;

let mut f = File::options()
    .read(true)
    .write(true)
    .open("events.jsonl.seek.zst")
    .unwrap();

truncate(&mut f, 10_000, b"\n").unwrap();
```

`append` はファイル末尾にレコードを足します。開き方は同じです。

```rust,no_run
use seekzstdsep::{OnMissingSeparator, append};
use std::fs::File;

let mut f = File::options()
    .read(true)
    .write(true)
    .open("events.jsonl.seek.zst")
    .unwrap();

append(&mut f, File::open("more.jsonl").unwrap(), b"\n", OnMissingSeparator::Refuse).unwrap();
```

追記するレコードは任意の `Read` から取れます。断片で終わるファイルを拒否する代わりに継ぎ目へ
セパレータを書かせるのが `OnMissingSeparator::Insert` です。

`compress_to_seekable_zst_with_opts` はより上位のエントリポイントです。`Read + Seek` な入力を取り、
自動検出したフレームあたりレコード数が途中で収まらないと分かった時点でフレーム分割をやり直します
(ストリーミング系のエントリポイントにはできない芸当です)。出力は一時ファイルに書かれたあと reflink で
`CompressOptions::out_path` に複製されます。データを二度コピーせずに済ませるためで、writer 引数に
届くのは reflink が使えない場合のフォールバック時だけです。したがって出力を得るには `out_path` を
設定してください。`compress_to_seekable_zst` はオプションを取らないため出力先がありません。

## nushell plugin

`nu_plugin_zstdsep/` は同じファイルを読む nushell plugin です。`zstdsep open f | get 10` は
ファイル全体ではなくフレーム 1 つを展開します。

```text
> let h = zstdsep open events.jsonl.seek.zst
> $h.1999999.msg
```

2,000,000 レコードのファイルで 380 µs、全体を読むと 4.9 秒です。

`nu_plugin_zstdsep/nu/install.nu` は nushell の autoload に hook を張り、`open` と `save` を
覆います。`.seek.zst` は plugin に、それ以外は builtin に届きます。詳細は
[nu_plugin_zstdsep/README.md](./nu_plugin_zstdsep/README.md) を参照してください。

## 既知の問題

- `frame_size * limit_multiplier` は内部読み込みバッファのサイズである 32768 以上が望ましいです。
  下回ると、limit を超える大きさの入力は、それがセパレータだらけであっても "No separator was found
  before reaching the limit size" で失敗します。limit より小さい入力は判定に到達しないため通ります。なおこのメッセージは "Current unprocessed data size" というラベルで
  `limit_multiplier` の値を表示します。

## ライセンス

MIT ([LICENSE](./LICENSE))。
