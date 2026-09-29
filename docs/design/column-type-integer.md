# `INTEGER` / `BIGINT` 列型（Issue #881）

- ステータス: Accepted
- 対象ビヘイビア（ポインタ表記のみ。spec 本文は転記しない）: TABLE-13
  （スカラー型）・TABLE-6（型集合の拡大）・TASK-196
- 前提: Issue #880（`ColumnType` 拡張基盤・カタログ v2・`ScalarRef` 化。
  `docs/design/column-type-extension.md`）

## 背景・目的

`ColumnType` は `Text`／`Vector(u32)` の 2 値のみで、利用者は数値列を宣言
できなかった。本 Issue で符号付き整数の列型 `INTEGER`（i32）と `BIGINT`
（i64）を追加する。

## 決定

### D1: 型表現

`catalog::ColumnType` に `Integer`・`BigInt` を追加した（unit variant）。
`row_codec::Value` に `Integer(i32)`・`BigInt(i64)` を、`sql::exec::Cell` に
`SignedInteger(i64)`（`INTEGER`／`BIGINT` 共用。既存の `Cell::Integer(u64)`
〔`id`・`COUNT` 用〕とは意味が異なるため別 variant）を追加した。
`#[non_exhaustive]`・ワイルドカード腕は入れない（Issue #880 D1 の方針を継承）。

### D2: カタログのタグ・行ペイロードのバイト表現（固定幅）

カタログのタグは `integer`／`bigint`、`param` は `text` と同じく `-`
（パラメータを持つ宣言は `CatalogError::Invalid` で拒否）。

スカラーペイロード・全行コーデックの両方で、presence タグに続けて固定幅
（`INTEGER` は 4 バイト LE、`BIGINT` は 8 バイト LE）で書く。長さプレフィクス
は持たない。既存の `TEXT`/`VECTOR` のバイト列は不変（golden テストで固定）。

### D3: 走査 API の型付け（破壊的変更）

`scan_scalar_columns`／`scan_scalar_columns_masked` の戻り値を
`Vec<Option<&str>>` から `Vec<Option<ScalarRef<'a>>>`
（`enum ScalarRef<'a> { Text(&'a str), Integer(i32), BigInt(i64) }`）へ変更した
（Issue #880 が申し送った拡張点）。呼び出し元のうち、`TEXT` 列のみを扱う契約が
既に確立している箇所（`declarative_filter::matches_all`・`GROUP BY`・集計の
列参照）は `row_codec::scalar_refs_as_text` で `&[Option<&str>]` へ変換して
既存実装を再利用する。この変換は**束縛段が非 TEXT 列の参照を拒否する**という
契約に依存する fail-closed 設計であり、`scalar_refs_as_text` 自体は呼び出し元
インデックスの型を検証しない（`INTEGER`／`BIGINT` 列参照の解禁時は呼び出し元
の見直しが必要。関数のドキュメンテーションコメント参照）。投影段
（`sql/exec.rs`・`sql/scan.rs`・`sql/returning.rs`）は `ScalarRef` の型と列型が
一致しない場合を `XX000`（`SqlSurfaceError::Internal`）で fail-closed に拒否する
（黙って不一致・NULL 扱いにしない）。

### D4: SQL リテラルの束縛と範囲検査

`sql::allowlist::Parser::expect_literal` に単項マイナスの受理を追加した
（`-` の直後に `Number` トークンが続く形のみ。空白を挟む形も許容。
`- -1`・`-'x'`・`+1` は引き続き `42601`）。

**既存挙動の変化**: これまで `42601` だった「`TEXT`／`VECTOR` 列や `id` への
負数」は `22000`（型不一致）になる。`id` の値域・エラー分類自体は変えない。

`sql::parser::bind_integer_literal` で `INTEGER`／`BIGINT` の数値リテラルを
束縛する。`str::parse::<i32>`／`<i64>` の overflow は
`SqlSurfaceError::numeric_out_of_range`（`22003`）、それ以外の parse 失敗
（小数・16 進数等）と文字列リテラルは Issue #1187 で `22P02`
（`InvalidTextRepresentation`）。PG 互換の暗黙変換は行わない。範囲検査はすべて束縛段（対象行探索・台帳
記録・書き込みトランザクション開始より前）で行うため副作用ゼロで拒否できる。

### D5: 読み出し（投影）

`sql/exec.rs`・`sql/scan.rs`・`sql/returning.rs` の投影に
`Cell::SignedInteger(i64::from(v))`（`INTEGER`）／`Cell::SignedInteger(v)`
（`BIGINT`）の腕を追加した。wire-server は `Cell` を網羅 match しているため
`result_encoder::cell_to_text`（text 形式）・
`http/query/response.rs::write_cell`（JSON 数値）に腕を追加するだけで済んだ。
`RowDescription` の OID 写像は当初 `ColumnMeta::Scalar` のまま `text` 固定と
していたが、PR レビュー指摘（psql・ドライバ・ORM・JSON クライアントが整数列を
文字列として扱ってしまう）を受け、`result_encoder.rs::column_wire_type` を
`ColumnType` ごとに分岐させ `INTEGER`→`int4`（OID 23）／`BIGINT`→`int8`
（OID 20）へ是正した（Issue #895 のうち整数型分を実装。`ColumnType::Boolean`
は引き続き `text`（OID 25）のまま Issue #895 の残課題とする）。

### D6: 後続 Issue 担当の機能を fail-closed に拒否する箇所

`ColumnType` を網羅 match しているため、コンパイラがすべての箇所を列挙した。
整数列は次のように扱う（いずれも `22000`）。

- `WHERE`・宣言的フィルタ・スコアブースト・本文列（`text_column_index`・
  `declarative_filter.rs`・`scoring_boost.rs`・`using_plan.rs`）: 「TEXT 列で
  はない」として拒否
- 式（`udf_call.rs`）: 「式で使えない列」として拒否。#891・TASK-199 は
  算術を持たない非数値型（DATE/TIMESTAMP/NUMERIC/UUID/BYTEA）の WHERE 等価・
  範囲比較のみを対象としたため、INTEGER/BIGINT を算術・WHERE 範囲比較で
  使う経路（レーン A）は引き続き別 Issue へ申し送り（詳細は
  `docs/design/scalar-types-predicates.md` 参照）
- 集計・`GROUP BY`（`sql/parser.rs::resolve_aggregate_input`・
  `resolve_group_by_column`）: 「TEXT 列でも VECTOR 列でもない」として拒否
  （#892 まで）
- スカラー列二次索引（`scalar_index.rs`）: 未索引（`Vector` 列と同じ）
  （#893 まで）
- NoSQL 表層（`http/query/update.rs`）: `SET integer column is not supported
  on the NoSQL surface yet` で拒否（#896 まで）。`http/query/insert.rs` は
  既存のワイルドカード腕でそのまま `22000` になる

### D7: 内容照合ハッシュ

`recovery/content_hash.rs::push_value` に `Value::Integer` をタグ 3（値の
4 バイト LE）、`Value::BigInt` をタグ 4（値の 8 バイト LE）として追加した。
既存タグ（Null=0／Text=1／Vector=2）の意味・並びは不変。

## バイト表現の不変範囲（golden で固定）

- `ROW_CODEC_FORMAT_VERSION = 1`、presence タグ `0x00`／`0x01`
- `Text` = `u32 LE 長 + 本文`、`Vector` = `u32 LE 次元 + f32 LE`（不変）
- `Integer` = presence(1) + `i32 LE`（5 バイト固定）
- `BigInt` = presence(1) + `i64 LE`（9 バイト固定）
- `recovery/content_hash.rs::push_value` のハッシュ入力タグ
  （Null=0／Text=1／Vector=2／**Integer=3／BigInt=4**）
- 整数列を一切含まないスキーマの `TEXT`／`VECTOR` バイト列は本 Issue の前後で
  完全に不変（golden テストで固定）

## スコープ外（後続 Issue の担当）

- `WHERE` 述語・式評価での整数列参照（レーン A。算術との組み合わせ）→
  #891 では対象外のまま別 Issue へ申し送り（`docs/design/
  scalar-types-predicates.md` 参照）
- 集計（`SUM`/`AVG`/`MIN`/`MAX`/`GROUP BY`）の整数列対応 → #892
- スカラー列二次索引の整数列対応 → #893。3 段階デコード tier の最適化 → #894
- `RowDescription` の型 OID 写像は `INTEGER`/`BIGINT` 分を実装済み（本節
  「D5」参照）。`BOOLEAN` 列の OID 写像は引き続き #895 の担当
- NoSQL の JSON 束縛（JSON 数値 → 整数列）→ #896
- `22P02`（形式不正）: Issue #1187 で解消（小数・16 進数・文字列リテラルの拒否を `22P02` へ変更。真偽値・ベクトルの種別不一致は `22000` のまま）
- SQL の DDL（`CREATE TABLE` 文）→ Phase 3（SQL-23）

## Issue #896 追記

NoSQL 表層の JSON 束縛（`insert`／`update`／`filter`）の型別対応・`columns[].type` の型名整備は Issue #896（NOSQL-17）で実施済み。詳細は `docs/design/nosql-typed-json-binding.md` 参照。
