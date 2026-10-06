# `REAL`／`DOUBLE PRECISION` 列型（Issue #882）

- ステータス: Accepted
- 対象ビヘイビア（ポインタ表記のみ。spec 本文は転記しない）: TABLE-13・
  TASK-196（関連ポインタ: TABLE-1, TABLE-5, TABLE-6, TABLE-7）

## 背景・目的

`catalog::ColumnType` は Issue #880 で `Text`／`Vector(u32)` の 2 値からカタログ
v2 の基盤（型タグ↔`ColumnType` の往復集約・`#[non_exhaustive]` 不使用）を得た。
本 Issue はその上に `REAL`（f32）・`DOUBLE PRECISION`（f64）の 2 型を追加し、
値・型ともにカタログ・行エンコーダー・SQL 表層で往復させる。列として宣言・
保存できる浮動小数はこれが初めてで、式評価の中間値 `Cell::Float(f64)` とは
独立した永続表現になる。

## 決定

### F1: 型とタグ

`ColumnType::Real`・`ColumnType::Double` を追加する。カタログ v2 のタグは
`real`・`double`、`param` は `Text` と同じ「パラメータなし型」規約に従い
`-` 固定（それ以外は `CatalogError::Invalid`）。`#[non_exhaustive]` や
ワイルドカード腕は入れない（Issue #880 D1 を踏襲）。

### F2: 行ペイロード

`Value::Real(f32)`・`Value::Double(f64)` を追加する。スカラーペイロードは
presence(1) の後に固定長 LE ビット列（Real は 4 バイト、Double は 8 バイト）を
置き、長さプレフィクスは持たない（列の型自体が固定長を決めるため）。フレーム長
の定数 `SCALAR_REAL_ENTRY_LEN = 5`・`SCALAR_DOUBLE_ENTRY_LEN = 9` を
`row_codec.rs` に `pub(crate)` で定義し、`tenant::validate_set_assignments`
（SET 値の事前検証。対象行の探索より前に呼ばれる）と共有する（`TEXT` の
`SCALAR_TEXT_ENTRY_OVERHEAD` と同じ理由。事前検証と実エンコードが乖離すると、
行の存在有無で異なる応答を返す既存のテナント境界漏えいが再発するため）。
`ROW_CODEC_FORMAT_VERSION` は据え置く。既存 `TEXT`／`VECTOR` のバイト列は不変。

`scan_scalar_columns` 系の戻り値は `Vec<Option<&'a str>>` から
`Vec<Option<row_codec::ScalarRef<'a>>>` へ変更した（**BREAKING CHANGE**）。
`ScalarRef<'a>`（`Text(&'a str)`／`Real(f32)`／`Double(f64)`）は、`TEXT` 列を
借用のまま扱う既存の設計（Issue #56）を保ちつつ、`Real`／`Double` は固定長
ペイロードのため複製コストなしに値そのものを保持できる。`TEXT` 専用の既存
呼び出し元（宣言的フィルタ・スコアリングブースト・集計の `GROUP BY` キー等）は
`ScalarRef::as_text()`（`Real`／`Double` は `None`）で後方互換に扱う。

### F3: 非有限値

受け付けない（fail-closed）。

- SQL リテラル束縛: `scalar_float::parse_real`／`parse_double` の結果が
  非有限なら `22003`（`NumericOutOfRange`）で、書き込みトランザクション開始前に
  拒否する。
- `encode_row`／`encode_scalar_columns`／`merge_encode_scalar_columns`・
  `tenant::validate_set_assignments`: `Value::Real`／`Value::Double` が非有限
  なら `Err`（対象行の探索より前）。
- `decode_row`／`scan_scalar_columns_validated`: 永続バイトが非有限のビット列
  なら `Err`（TABLE-7 と同じく永続データを untrusted とみなす方針。未参照列・
  マスク外の列でも検証を弱めない）。

### F4: ゼロの扱い

`-0.0` はリテラル束縛時・エンコード時に `+0.0` へ正規化する
（`scalar_float::canonicalize_real`／`canonicalize_double`）。符号付きゼロは
保持しない。将来の索引キー（#891・#893）が見るゼロを 1 つに揃えるための判断。

### F5: 順序規約

`scalar_float::cmp_real`／`cmp_double` を唯一の情報源とする。`a == b`
（`-0.0 == +0.0` を含む IEEE 754 の等価性）を優先し、そうでなければ
`f32::total_cmp`／`f64::total_cmp` で決定的な全順序をつける。防御的に NaN が
来ても `total_cmp` の位置に置かれるため、どの入力でも全順序かつ決定的になる。
`NULL` の並び位置は呼び出し側の責務（本関数は非 NULL 値のみを扱う）。ソートは
安定ソートとの組み合わせを前提にし `sort_unstable*` は使わない
（`check_sort_determinism.sh` の方針に従う）。

### F6: テキスト表現

`DOUBLE PRECISION` の正準テキスト表現は `f64` の `Display`（最短往復表記・指数表記
なし）で、`scalar_float::format_double` として提供する。`f32` は必ず
`f32::from_str` で直接解析する（`f64` を経由すると二重丸めになるため）。

`REAL` のテキスト表現は Issue #1173（WIRE-13 のポインタ）で PostgreSQL の float4
出力形式に揃えた（`scalar_float::format_real`）。最短往復桁を使い、十進指数が
`-4 <= e < 6` なら固定小数、それ以外は `d[.ddd]e±XX`（例: `0.1f32` は `0.1`、
`1e6` は `1e+06`、`1234567` は `1.234567e+06`）。`Cell::Float` へは
`f64::from(v: f32)` の無損失拡大で運び、wire-server は `float4` 列（`REAL`・
`SUM(REAL)`）で f32 へ戻して整形する（f32 で表現できない値は fail-closed）。

指数表記は SQL リテラルの字句解析器（`lex_number`）が受理しないため、SELECT で得た
大きな／小さな `REAL` 値の文字列をそのまま SQL リテラルへ貼り戻すことはできない。
一方 `COPY ... FROM STDIN` は `COPY TO` の出力を再投入する往復契約（WIRE-17）のため、
指数表記を受理する `scalar_float::parse_real_text` を使う（SQL リテラルの
`parse_real` は従来の閉じた文法のまま）。

### F7: リテラル文法

INSERT・UPDATE・UPSERT のリテラル位置で `['-'] <Number>` を受け付ける
（`sql::allowlist::Parser::expect_literal` を拡張。既存の `HAVING` 述語の負号
処理と同じ形）。`scalar_float::parse_real`／`parse_double` は、`f32::from_str`
／`f64::from_str` を呼ぶ前に自前の文法（`^-?[0-9]+(\.[0-9]+)?$`）で検証する
（`from_str` は `inf`／`nan`／`infinity`／指数表記も受け付けてしまうため、
それを閉じる）。

- 範囲外（非有限化）は `22003`。
- 非ゼロ入力が 0 へアンダーフローした場合も `22003`（PostgreSQL と整合し、
  黙った桁落ちを防ぐ）。
- 文字列リテラル（`'1.5'`・`'NaN'`）は、既存の型不一致と同じ `22000`
  （`InvalidInput`）で拒否する（文字列からの暗黙変換はしない）。

`id` 疑似列への負リテラルは既存の応答コードを変えない（`allowlist.rs` の
`id = -1` 等の既存テストは無変更のまま green）。

### F8: `Cell` / wire

`sql::exec::Cell` は変更しない（公開 enum の追加を避け、兄弟型追加 Issue との
衝突面を減らす）。REAL 列は `Cell::Float(f64::from(v))`、DOUBLE 列は
`Cell::Float(v)` として投影する（`sql/exec.rs`・`sql/scan.rs`・
`sql/returning.rs` で同一の写像）。

### F9: content_hash タグ

`recovery/content_hash.rs::push_value` のタグを、TABLE-13 の列挙順で予約
割り当てする: Integer=3、BigInt=4、**Real=5、Double=6**、Boolean=7（本 Issue で
実装するのは 5・6 のみ）。ハッシュ入力は「タグ＋正規化後の LE ビット列」。
既存タグ（Null=0／Text=1／Vector=2）とは衝突しない。タグは台帳の `23505`／
`22023` 判定に使われ、着地後に番号を振り直せないため golden バイトテスト
（`content_hash::tests::for_typed_insert_real_and_double_tags_are_stable_and_distinct`）
で固定する。

### F10: 暫定拒否（fail-closed）

Issue #891〜#896 の担当範囲は、本 Issue では振る舞いを追加せず、拒否する腕だけを
追加する。

| 経路 | 拒否コード | 対応 Issue |
| --- | --- | --- |
| `WHERE`・フィルタ列の解決（`declarative_filter.rs`・`sql::udf_call`・`sql::using_plan`・`sql::scoring_boost`） | `22000`（既存の TEXT 列限定チェックへ合流） | #891（注記参照） |
| 集計の束縛（`sql::parser::resolve_aggregate_input`） | `22000`（`COUNT` を含む全関数） | #892 |
| `sql::scalar_index`（列単位索引構築） | 索引化しない（`per_column.push(None)`。VECTOR 列と同じ扱い） | #893 |
| NoSQL `insert`（`http/query/insert.rs`） | `22000`（既存の型不一致腕 `_ =>` へ合流。表層固有の新設腕なし） | #896 |

**#891 追記**: 算術を持たない非数値型（`DATE`／`TIMESTAMP`／`NUMERIC`／
`UUID`／`BYTEA`）の WHERE 等価・範囲比較述語は #891・TASK-199 で対応済み
（`declarative_filter::FilterOp::TypedCompare`。詳細は
`docs/design/scalar-types-predicates.md` 参照）。`REAL`／`DOUBLE` は算術と
組み合わせる式評価経路（レーン A）の対象のため、上表の拒否は本表の意味では
継続し、対応は別 Issue へ申し送り。
| NoSQL `update`（`http/query/update.rs`） | `22000`（`UpdateError::Set`。既存の TEXT/VECTOR 型不一致と同じ応答形） | #896 |

## 対象外（申し送り）

以下は本 Issue では実装しない。

- `DOUBLE PRECISION` 出力の PostgreSQL 形式化（`1e+20` 形式）、SQL リテラルでの
  指数表記の受理（`REAL` の出力形式化・COPY FROM の指数表記受理は #1173 で対応済み。
  RowDescription OID 700/701 は #895）
- 新型の `WHERE` 述語・式評価 → #891
- 集計 `SUM`/`AVG`/`MIN`/`MAX`/`COUNT` → #892
- スカラー索引 → #893
- `DecodeTier` の最適化 → #894
- NoSQL JSON 束縛（`columns[].type` を含む）→ #896
- 回帰の統合（`scalar_types_roundtrip.rs` への集約）→ #897
- 文字列リテラルからの暗黙変換（`22P02` の新設と指数表記の受理は Issue #1187 で解消）
- `ALTER COLUMN TYPE` による REAL→DOUBLE の拡大変換（TABLE-19・#901）

## SET 事前検証とテナント境界（codex-review 指摘・PR #1007）

`tenant.rs::validate_set_assignments`（対象行探索より前の SET 値検証）は
REAL／DOUBLE PRECISION の固定長エントリ（presence 込みで REAL 5 bytes・
DOUBLE 9 bytes）を累計スカラーペイロード上限へ加算する（Issue #882 レビュー
指摘）。一方、nullable な REAL／DOUBLE 列の `NULL`→値ありという presence
バイト数の増分（REAL: 1→5 bytes・DOUBLE: 1→9 bytes）自体は、対象行に
**既に格納されている未変更列**の payload と組み合わさって初めて上限を
超えるケースであり、これは単一行 UPDATE の「未変更 TEXT 列との組み合わせで
上限超過（判断 D 再改訂・`docs/design/update-single-row.md`）」と同じ内容
依存クラスに属する。既存の受け入れ済み契約どおり、内容依存の処理
（`merge_encode_scalar_columns` を含む）は `is_owner && is_visible` を満たす
行にのみ実行され、それ以外（RLS 不可視・未存在・他テナント所有）は内容に
触れず `UPDATE 0` と区別できない。述語 UPDATE（`update_rows_where_unchecked`）
の候補列挙スコープはテナント所有のみ（RLS 可視性フィルタではない）ため、
他テナント行はそもそも候補にならない。回帰は
`tenant.rs::update_row_columns_nullable_real_null_to_value_transition_
overflow_matches_visibility_parity`・
`update_rows_where_unchecked_nullable_real_null_to_value_transition_overflow`
参照。

ファイル形 `INSERT`（`path`／`body` の TEXT 列専用チャンク化・埋め込み経路）
は他の追加スカラー型（BOOLEAN・DATE・ARRAY・BYTEA・JSON・ENUM・NUMERIC）と
同様に REAL／DOUBLE PRECISION 列も対象外として拒否する
（`sql::parser::bind_file_insert`）。

## 破壊的変更

`row_codec::scan_scalar_columns`／`scan_scalar_columns_masked` の戻り値型が
`Vec<Option<&str>>` から `Vec<Option<ScalarRef<'_>>>` へ変わった
（`row_codec::ScalarRef` 新設）。`Value`（`row_codec::Value`）へ `Real`／
`Double` variant を追加した。いずれも公開 API の破壊的変更として
`feat(engine)!:` コミットへ記録する。

## Issue #896 追記

NoSQL 表層の JSON 束縛（`insert`／`update`／`filter`）の型別対応・`columns[].type` の型名整備は Issue #896（NOSQL-17）で実施済み。詳細は `docs/design/nosql-typed-json-binding.md` 参照。

## Issue #1438 追記: 浮動小数列と大きな数値リテラルの比較

- 対象ビヘイビア: SQL-24・TABLE-13（関連: ERR-2・ERR-4）。
- `REAL`／`DOUBLE PRECISION` 列と数値リテラル（数値形・文字列形）の比較は float8 同士の比較として束縛し、整数列・疑似列 `id` 用の 2^53 exactness 判定（`parse_number_literal`）をかけない。`1e21`・`1e39` のような f32 範囲外でも f64 範囲内なら受理する（REAL 列も f64 範囲で比較する）。
- float8 範囲外（オーバーフロー・非ゼロのアンダーフロー）は `22003`、書式不正は `22P02`。エラー文言にリテラル本文は含めない。
- 整数列（`INTEGER`／`BIGINT`）・疑似列 `id`・式の結果（`d + 0 > 1e21` 等）・HAVING リテラルは従来どおり exactness 判定を維持する。書き込み経路（INSERT／UPDATE／COPY）の REAL 範囲検査も変更しない。
