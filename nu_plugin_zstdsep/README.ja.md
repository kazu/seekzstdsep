# nu_plugin_zstdsep

[seekzstdsep](https://github.com/kazu/seekzstdsep) が書いた `.seek.zst` ファイルを扱う
[nushell](https://www.nushell.sh/) plugin です。

```text
> let h = zstdsep open events.jsonl.seek.zst
> $h.1999999.msg
```

これが読むのはファイル全体ではなくフレーム 1 つです。[コスト](#コスト)を参照してください。

## 専用コマンドである理由

`open events.jsonl.seek.zst | from zst` は lazy にできません。`open` は plugin に逐次的なバイト
ストリームを渡すだけでパスを教えず ([nushell#8030](https://github.com/nushell/nushell/issues/8030))、
seek table はファイルの末尾にあるので、ストリームを最後まで読まない限り届きません。パスを引数に取るのは
好みの問題ではなく、seek するための唯一の道です。

## インストール

```sh
cargo build --release -p nu_plugin_zstdsep
plugin add target/release/nu_plugin_zstdsep    # nushell 内で
plugin use zstdsep
```

`cargo install nu_plugin_zstdsep` でも入ります。plugin のプロトコルバージョンは動かす nushell と
一致している必要があるので、同じリリースに対してビルドしてください。

## コマンド

### `zstdsep inspect <path>`

フレームごとに 1 行。どこから始まり、圧縮後と展開後でどれだけの大きさで、何レコード持っているか。

```text
> zstdsep inspect events.jsonl.seek.zst
╭───┬────────────┬──────────┬───────────┬──────────────┬────────────┬─────────────┬─────────╮
│ # │ comp_start │ comp_end │ comp_size │ decomp_start │ decomp_end │ decomp_size │ records │
├───┼────────────┼──────────┼───────────┼──────────────┼────────────┼─────────────┼─────────┤
│ 0 │          0 │     7180 │    7.1 kB │            0 │      65634 │     65.6 kB │     463 │
│ 1 │       7180 │     9570 │    2.3 kB │        65634 │      85133 │     19.4 kB │     137 │
╰───┴────────────┴──────────┴───────────┴──────────────┴────────────┴─────────────┴─────────╯
```

内部のフレームについて、`records` はフレーム 0 からの推定値です。`--no-fast-mode` は全フレームを
数えます。インデックスが乗っている「レコード数が均一」という条件を壊しているフレームを見つける方法は、
これだけです。

### `zstdsep open <path>`

データではなく**ハンドル**を返します。

```text
> let h = zstdsep open events.jsonl.seek.zst
> $h.10              # そのフレームをレコードまでデコードし、レコード 1 つをパースする
> $h.10.user.name    # 残りのパスはエンジンが自分で辿る
> $h | get 10 11 12  # 呼び出しは 3 回、フレームは 1 つ、デコードは 1 回
> $h                 # サマリ: path, separator, format, frames, records_per_frame, records
> $h.records         # そのサマリの 1 フィールド
```

フラグ: `--separator` (デフォルトは改行 — ファイル自身はセパレータを記録していません)、`--format`、
`--raw`、`--no-partial`。

### `zstdsep save <path>`

入力をレコードとして書き、圧縮します。`--append` は既にレコードが入っているファイルに足します。

```text
> ls | zstdsep save listing.jsonl.seek.zst        # 拡張子から `to jsonl`
> open --raw access.log | zstdsep save access.log.seek.zst   # テキストは来たまま
> [a b c] | zstdsep save lines.seek.zst           # 1 要素が 1 レコード
> $new | zstdsep save --append listing.jsonl.seek.zst
```

テキストはそのまま書かれ、構造を持つものは内側の拡張子が名指すフォーマットで直列化されます。`--format`
がそれを上書きし、`--raw` は直列化を拒否します。入力がセパレータで終わっていなければ 1 つ足すので、
ファイルがレコードの途中で終わることはありません。

フラグ: `--append`/`-a`、`--force`/`-f` (付けなければ既存ファイルは残します)、`--separator`/`-s`、
`--format`、`--raw`/`-r`、`--insert-separator`、それに圧縮器自身の `--frame-size`、
`--records-per-frame`、`--limit-multiplier`、`--no-check`。

`--append` はライブラリの `seekzstdsep append` そのもので、その 2 つの拒否も受け継ぎます。フレームが
3 つ未満のファイル (セパレータを検証するには短すぎる) と、レコードの途中で終わっているファイル
(`--insert-separator` が先に閉じます) です。

`to` コマンドがヘッダ行を書くフォーマットを append すると、ファイルの途中に 2 つ目のヘッダが入ります。
nushell 自身の `save --append` も同じことをしますし、`to` コマンドにヘッダを書くかどうか尋ねる方法は
ありません。そういうものは自分で直列化して、テキストとして append してください。

```text
> $rows | to csv --noheaders | zstdsep save --append --raw rows.csv.seek.zst
```

### ハンドルと builtin

`first`、`last`、`skip`、`take`、`slice`、`length`、`where` はエンジンの中で動くので、ハンドルを
拒否します。

```text
> $h | length
Error: nu::shell::only_supports_this_input_type
  x Input type not supported.
   ,-[entry #1:1:1]
 1 | $h | length
   : ^|   ^^^|^^
   :  |      `-- only list, table, binary, and nothing input data is supported
   :  `-- input type: zstdsep handle
```

対処はフラグ 1 つです。

```text
> zstdsep open events.jsonl.seek.zst --no-partial | where lvl == error
```

これはファイル全体を読みます。`first n` なら途中で止まります — エンジンがストリームを捨て、plugin は
1 フレームずつ展開するためです。

## `open` と `save` を覆う

`nu/zstdsep-hook.nu` が builtin の `open` と `save` を覆います。`.seek.zst` のパスは plugin へ、
それ以外は builtin へ行きます。

```sh
nu nu_plugin_zstdsep/nu/install.nu              # ~/.config/nushell/autoload/ にリンクを張る
nu nu_plugin_zstdsep/nu/install.nu --uninstall
```

```text
> ls | save listing.jsonl.seek.zst                       # zstdsep save
> open listing.jsonl.seek.zst                            # zstdsep open: ハンドルが返る
> open listing.jsonl.seek.zst --no-partial | where type == file
> open Cargo.toml                                        # builtin、そのまま
```

どちらのコマンドも 2 つのシグネチャの和を持ち、もう一方に属するフラグは黙って捨てるのではなく拒否
します。`open notes.txt --no-partial` も `ls | save --progress listing.jsonl.seek.zst` もエラーです。
`core-open` と `core-save` は覆いを生き延びた builtin の別名で、`.seek.zst` をバイト列として読む
唯一の方法です。

制限が 2 つあります。autoload のファイルは REPL の起動時に読まれ、`nu script.nu` では読まれないので、
スクリプトには自前の `use .../nu/zstdsep-hook.nu *` が要ります。もう 1 つ、ハンドルは 1 つのファイルに
結びつくので、builtin なら連結していた `open a.seek.zst b.seek.zst` は拒否されます。

`nu nu_plugin_zstdsep/tests/run-hook.nu` がテストを走らせます。専用のディレクトリに登録した plugin に
対して実行されます。

## フォーマット

フォーマットは `.seek.zst` の内側の拡張子から決まります。`events.jsonl.seek.zst` なら jsonl です。
`--format <name>` がそれを上書きし、`--raw` が無効にします。

- **json、jsonl、ndjson** は plugin がパースします。パースの前にレコードへ分割するので、ここでは
  3 つとも同じ意味です — 1 レコードに JSON 値 1 つ。
- **それ以外**は*あなたのスコープ*で `from <name>` として解決されます。`use std formats *` すれば
  std が覆う範囲を覆い、logfmt plugin があれば logfmt を覆い、後から書かれたフォーマットもここへの
  変更なしで動きます。

後者は `--no-partial` では動き、セルパスでは**動きません**。nushell はカスタム値への操作を実行
コンテキストなしで処理するため、plugin はそこから `FindDecl` を呼べないからです。logfmt ファイルに
対する `$h.10` は行を文字列として返します。`--no-partial | get 10` ならパースされます。

`zstdsep save` も同じ分け方をしていて、理由がもう 1 つあります。plugin が動かせるのは nushell
*自身*の `to` コマンドだけです。nushell 内で定義されたものは `call_decl` が "can't run custom command
with 'run'" を返し、`to jsonl` と `to ndjson` はまさにそれです (`std formats` にあります)。そのため
json、jsonl、ndjson は plugin が書き — nushell 自身の変換を通して、1 レコードに JSON 値 1 つ —、
`to csv`、`to tsv`、`to yaml`、`to nuon` などは builtin なので動きます。

csv はどちらでパースするにせよ相性が良くありません。ヘッダ行がレコード 0 を占めて全インデックスを
ずらしますし、引用符で囲まれたフィールドの中の改行はセパレータの不変条件を真っ向から壊します。

## コスト

2,000,000 JSONL レコード、入力 215.5 MB、圧縮後 10.1 MB、640 レコードのフレームが 3125 個。
nushell 0.114.1、release ビルド、ページキャッシュは温めた状態、`timeit` を繰り返した中央値です。

| | |
| --- | ---: |
| `$h.1999999.seq` — キャッシュに無いフレーム | 380 µs |
| `$h.1999998.seq` — 同じフレームをもう一度 | 31 µs |
| `$h \| get 1999997` — 同じものをパイプライン経由で | 31 µs |
| `--no-partial \| first 3` | 417 µs |
| `--no-partial \| length` — 全レコード | 4.9 s |

作り直すには:

```text
> use std formats *
> 1..2_000_000 | each {|i| {
      seq: ($i - 1)
      lvl: ([info warn error] | get ($i mod 3))
      msg: ("" | fill --width (($i mod 101) + 20) --character "x")
  } } | to jsonl | save big.jsonl
> ^seekzstdsep compress big.jsonl big.jsonl.seek.zst
```

```text
> let h = zstdsep open big.jsonl.seek.zst
> timeit { $h.1999999.seq }
```

## まだ無いもの

`zstdsep slice`、`zstdsep first`、`zstdsep last`、`zstdsep len` — builtin がハンドルを拒否するために
専用コマンドが要る、範囲と個数の形です。`--no-partial` がこれらを代替しますが、ファイル全体を読む
代償が付きます。

## ライセンス

MIT ([LICENSE](../LICENSE))。
