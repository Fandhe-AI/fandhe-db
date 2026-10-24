# 正規表現マッチャのパターン長・入力長・コンパイル後サイズ上限の起動時設定（CLI）

## ステータス・対象

- ステータス: 採用（Issue #1786・SQL-55 のポインタのみ。spec 本文は転記しない）
- 対象フラグ: `--max-regex-pattern-len`・`--max-regex-input-len`・`--max-regex-compiled-size`
- 先行: [jsonpath-limits-cli.md](jsonpath-limits-cli.md)（Issue #2184）と同型。写像・拒否の一覧は [regex-matcher.md](regex-matcher.md)

## 背景

正規表現マッチャ（`engine::regex_match`）は線形時間の `regex` クレートで評価する（[regex-crate-adoption.md](regex-crate-adoption.md)）。ReDoS による指数時間は起きないが、パターンが大きいほどコンパイルのコストとメモリが増え、入力が長いほど走査時間が延びる。パターン長・入力長・コンパイル後サイズを、運用者が `--max-jsonpath-steps` 等と同じ方式で起動時に下げられるようにする。

## 決定

| # | 内容 |
| - | ---- |
| D1 | ハード上限がそのまま既定値。パターン長 `4,096` バイト（`MAX_LIKE_PATTERN_LEN` と同値）・入力長 `4,194,304` バイト（1 値の最大ペイロード長と同値で、これより長い入力は表層に存在しない）・コンパイル後サイズ `10,485,760` バイト（`regex` の `size_limit` の既定と同値）。設定できるのは**下げる方向だけ**（範囲は `1..=` 各ハード上限）。引き上げは安全弁を外すことになるため対象外 |
| D2 | 超過は SQLSTATE `54000`（fail-closed）。パターン長は確保・解析の前、入力長は走査の前、コンパイル後サイズは `RegexBuilder::size_limit` で検査する。PG は複雑すぎるパターンを `2201B` にするが、本実装は上限超過を `54000` とする（意図的な差分） |
| D3 | 値の運搬は `jsonpath_limits` と同型。`engine::regex_limits::RegexLimits`（`Copy`・フィールド非公開・ハード上限で飽和）を `EngineCore::with_regex_limits` で起動時に 1 回注入する。範囲検証は engine の `validate_*` に一本化し、wire-server は CLI 文字列を厳密パース（先頭 `+`・空白・全角数字・空文字を弾く）して渡すだけ。環境変数で差し替える経路は作らない |
| D4 | フラグ名は `--max-regex-pattern-len <1-4096>`・`--max-regex-input-len <1-4194304>`・`--max-regex-compiled-size <1-10485760>`。範囲外・非数値・値欠落は起動エラー。2 回目以降の指定は fail-closed（last-wins にしない）。解決は bind・ユーザーストア読込より前 |
| D5 | 「ステップ上限」は**持たない**（ステップを数えるカウンタを置かない）。根拠は 2 点: `regex` が最悪 `O(m * n)` の終了を保証していること、コンパイル後サイズ上限がパターン側の `m` を、入力長上限が `n` を有界にすること。入れ子の深さ（括弧 64 段）と遅延 DFA のキャッシュ（2 MiB）は固定値で CLI では変えない。キャッシュ超過はエラーにならず低速なエンジンへ縮退するだけで、メモリの上限であり拒否の根拠ではない |

## 対象外

- ハード上限の引き上げ
- セッション単位の `SET` による変更
- SQL 表層への配線（`~`／`~*` は #1787、`SIMILAR TO` は #1788、jsonpath の `like_regex` は #2677 で配線済み）
