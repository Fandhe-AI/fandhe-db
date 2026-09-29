# RowDescription 型 OID 写像の拡張（WIRE-13・TASK-200）の設計判断

- ステータス: **Accepted（実装済み）**
- 対応: Issue #895（`feat(wire): RowDescription 型 OID 写像の拡張`。ポインタ:
  `docs/spec/05-tasks.md` TASK-200・`docs/spec/04-behavior/wire-protocol.md`
  WIRE-13）
- 実装: `crates/wire-server/src/result_encoder.rs`（`WireType`・
  `column_wire_type`）
- テスト: `crates/wire-server/src/result_encoder.rs`（単体。`wire_type_*`・
  `column_wire_type_matrix`・`row_description_encodes_new_scalar_types_with_
  builtin_oids`）・`crates/wire-server/src/http/query/response.rs`（単体）・
  `crates/wire-server/tests/wire13_type_oid.rs`（結合。実 wire 経路）

## 背景

`RowDescription`（pg wire）と NoSQL 表層の JSON 応答 `columns[].type`
（NOSQL-11）は `result_encoder::WireType` を単一情報源として型名を共有する。
Phase 2（親 #879）で engine の `ColumnType` に `BOOLEAN`／`REAL`／
`DOUBLE PRECISION`／`DATE`／`TIMESTAMP`／`BYTEA`／`UUID`／`JSON`／`JSONB`／
`NUMERIC`／`ARRAY`／`ENUM` が追加されたが、wire 側は `INTEGER`/`BIGINT`
（`int4`/`int8`）と `NUMERIC`（`numeric`）以外の新型を一律 `text`（OID 25）
で公告していた。このため psql・psycopg・node pg 等のドライバが bool／
float／日付／バイナリ／UUID／JSON 列をネイティブ型へ復元できなかった。

## 型写像表（PostgreSQL 組み込み値）

| `ColumnType` / `ColumnMeta` | `WireType` variant | OID | typlen | 型名 |
| --- | --- | --- | --- | --- |
| `Id` | `Numeric` | 1700 | -1 | `numeric` |
| `Text` | `Text` | 25 | -1 | `text` |
| `Vector(_)`／`Array(_)`／`Enum(_)`／`Computed` | `Text` | 25 | -1 | `text` |
| `Integer` | `Int4` | 23 | 4 | `int4` |
| `BigInt` | `Int8` | 20 | 8 | `int8` |
| `Numeric{..}` | `Numeric` | 1700 | -1 | `numeric` |
| `Boolean` | `Bool` | 16 | 1 | `bool` |
| `Real` | `Float4` | 700 | 4 | `float4` |
| `Double` | `Float8` | 701 | 8 | `float8` |
| `Date` | `Date` | 1082 | 4 | `date` |
| `Timestamp` | `Timestamp` | 1114 | 8 | `timestamp` |
| `Bytea` | `Bytea` | 17 | -1 | `bytea` |
| `Uuid` | `Uuid` | 2950 | 16 | `uuid` |
| `Json` | `Json` | 114 | -1 | `json` |
| `Jsonb` | `Jsonb` | 3802 | -1 | `jsonb` |

variant 名・型名は既存 `Int4`/`Int8` に倣い PostgreSQL の `typname` に揃えた。

## 単一情報源の設計

`WireType::ALL`（全 variant を列挙する固定長配列）を単一の起点とし、
`from_oid`（逆引き。単体テスト専用）は `ALL` を線形走査するだけで、逆引き
専用の第 2 の写像表を持たない。`ALL` は手動保守の配列ではなく、
`wire_type_enum!` マクロ（`result_encoder.rs`）が `WireType` の enum 定義と
同一の variant トークン列から自動生成する。配列サイズ注釈だけで
「`WireType` へ variant を追加すれば `ALL` への追加漏れがコンパイルエラーに
なる」と保証できるわけではない（variant 追加と配列長リテラルの据え置きは
両立してしまうため。PR #1037 codex-review 指摘）ことを踏まえ、マクロによる
単一トークン列からの同時生成でこの構造的な抜け道自体を無くしている。正引き（`oid()`／`typlen()`／`pg_type_name()`）はいずれも
`#[deny(clippy::wildcard_enum_match_arm)]` を付けた網羅 `match` のままとし、
`WireType`／`ColumnMeta` に variant が増えたときの決定漏れをコンパイル
エラーで検出する契約（`docs/design/wire-binary-format.md` から踏襲）を維持
した。

## 据え置き判断（本 Issue の受入基準外）

| 項目 | 判断 | 理由 |
| ---- | ---- | ---- |
| `id` 列 | `numeric`（OID 1700, typlen -1）のまま | engine の行 ID は `u64` 全域（`u64::MAX` を含む）を有効値とし、符号付き 64bit の `int8`（OID 20）ではこれを表現できない（PR #210 レビュー指摘）。受入基準 3（既存の `id` 公告不変）とも整合する |
| `VECTOR(N)`／`ARRAY`／`ENUM` | `text`（OID 25, typlen -1）のまま | 専用 OID を持たない実装既定値。値そのものはテキスト表現（`[v1,v2,...]`・`{a,b}`・ラベル文字列）のまま変わらない |
| `ColumnMeta::Computed`（式・集計結果） | `text`（OID 25）のまま | 実行時に決まる型であり静的な型情報を持たないため、専用 OID を公告する型付けには別途の設計判断（engine 側の変更）が要る |
| バイナリ形式（WIRE-14） | `INTEGER`／`BIGINT`／`REAL`／`DOUBLE PRECISION`／`BOOLEAN`／`BYTEA`／`UUID` は Issue #1172 でバイナリ対応（`docs/design/wire-binary-format.md`「Issue #1172 追記」参照）。`DATE`／`TIMESTAMP`／`JSON`／`JSONB`／`NUMERIC`／`id`／配列／`ENUM`／`VECTOR`／`Computed` は引き続き `supports_binary` / `column_binary_support` が `false`（fail-closed・`0A000`） | 対応拡大は WIRE-14・TASK-218・Issue #1172 で実施済み。float4 の縮小は無損失検査つき |
| typmod | 全列 `-1` のまま | `NUMERIC(p,s)` の typmod 符号化は本 Issue で採用しない |
| REAL のテキスト表現 | 変更しない | `Cell::Float(f64::from(f32))` の `to_string()` により `0.1f32` は `0.10000000149011612` になる既知挙動（`crates/wire-server/tests/wire_float_columns.rs` 参照）。OID 700 を公告してもドライバの float 変換は成立するが、PostgreSQL 既定出力との精度表記の差は残る |

## 値表現は不変

本 Issue で変わるのは `RowDescription` の OID・typlen と NoSQL `columns[].type`
の型名のみで、`DataRow` の値・行集合はいずれも一切変更しない（`Cell` から
テキストへの変換関数 `cell_to_text`／`http::query::response::write_cell` は
無変更）。`crates/wire-server/tests/wire13_type_oid.rs` の
`data_row_text_values_are_unchanged_by_new_oid_announcements` がこの契約を
実 wire 経路で固定する。

## psql での観測手段の制約

psql は通常出力に列の型 OID・型名を表示せず、`\gdesc` は `pg_catalog.
format_type` 呼び出しをサーバーへ要求するため本サーバー（`pg_catalog` 非実装）
では使えない。psql から観測できる差分は aligned 出力での数値列の右寄せ
（`bool`／`int4`／`int8`／`float4`／`float8`／`numeric` 等の数値系型として
扱われることに由来）に限られる。ドライバレベルの型 OID 観測（psycopg
`cursor.description[i].type_code`・node pg `result.fields[i].dataTypeID`）
は層 B（3 クライアント統合検証）の対象だが、本 Issue の時点では実装しない
（下記「申し送り」参照）。

## 申し送り

- 新型のバイナリ形式対応拡大 → WIRE-14・TASK-218 の後続として別途追跡
- `id`（`numeric`）のバイナリ対応拡大 → spec 側で未策定のため引き続き未実施
- `Computed`（集計・式列）への型情報付与とそれに伴う OID 公告 → engine 側の
  設計判断を要するため対象外
- 層 B（psycopg・node pg の opt-in 型 OID 観測スクリプト拡張・
  `three_client_e2e.rs` への `#[ignore]` テスト追加）→ 本 Issue の時点では
  未実施。`crates/wire-server/tests/wire13_type_oid.rs`（層 A・実 wire 経路）
  が RowDescription バイト列・値の不変・RLS 境界を機械検証しており、
  `DataRow` のテキスト表現自体が不変（ドライバのパース経路には影響しない）
  ことから、層 B 追加は独立の後続タスクとして扱う
