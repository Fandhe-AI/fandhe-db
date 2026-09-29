# ADR: DATE / TIMESTAMP 列型

- ステータス: Accepted
- 対象: TABLE-1・TABLE-7・TABLE-13・TASK-197（`docs/spec/05-tasks.md`・
  `docs/spec/04-behavior/data-model.md`）
- 関連: `column-type-extension.md`（BOOLEAN 列型・TASK-196。同じ拡張方式を
  踏襲）、ERR-6（`docs/spec/04-behavior/error-format.md`。エラーコード管轄表）

本ドキュメントは実装既定値・設計判断のみを記す。private spec 本文は転記せず
ビヘイビア ID・タスク ID のポインタ表記に限る（`.claude/rules/spec-confidentiality.md`）。

## 背景

列型は `TEXT`・`VECTOR(N)`・`BOOLEAN` のみで、日付・日時を列として宣言・
格納・読み出しできなかった。本 Issue で `DATE`／`TIMESTAMP` 列型を追加する。

## 決定

### D-1: `wire_code` の割り当て

- 範囲外・暦上不正（月 13、2/30、非閏年の 2/29、時 24、分 60、秒 60、
  年 0000、年 10000 以上等）は新設分類 `DatetimeFieldOverflow`（`22008`。
  ERR-6 の管轄表にある行）へ写像する。
- 書式違反（区切り文字違い、桁数不足、TZ 接尾辞、小数 7 桁以上、前後空白、
  非 ASCII 数字、長さ超過等）は Issue #1187 で `InvalidDatetimeFormat`（`22007`）へ
  変更した（当初は `InvalidInput`〔`22000`〕）。
- （Issue #1187 で解消: 書式違反は `22007`（`InvalidDatetimeFormat`）へ変更）。当初の記述: `22P02`（`InvalidTextRepresentation`）の新設は行わない（TASK-227・ERR-6 へ
  申し送り。着手時点で兄弟 PR による追加も無かった）。

### D-2: BREAKING CHANGE として扱う

`ColumnType`・`row_codec::Value`・`row_codec::ScalarRef`・`sql::exec::Cell`・
`sql::allowlist::SqlSurfaceError`・`error_format::ErrorClass`・
`sql::parser::AggregateInput` はいずれも `#[non_exhaustive]` を付けない公開
enum のため、variant 追加は破壊的変更になる（BOOLEAN 列型 ADR の D1 と同じ
方針）。網羅 `match` している既存コードはコンパイラの指摘に従って型ごとの
正しい扱いを追加する（ワイルドカード腕での黙殺はしない）。

### D-3: 内部表現

| 型 | 内部値 | エポック起点 | 精度 | 行バイト表現（presence の後） | エントリ長定数 |
| --- | --- | --- | --- | --- | --- |
| `DATE` | `i32` | 1970-01-01 起点の日数 | 日 | 4 byte LE | `SCALAR_DATE_ENTRY_LEN = 5` |
| `TIMESTAMP` | `i64` | 1970-01-01 00:00:00 起点 | マイクロ秒 | 8 byte LE | `SCALAR_TIMESTAMP_ENTRY_LEN = 9` |

受理範囲（閉区間）:

- `DATE`: `0001-01-01` 〜 `9999-12-31`（`DATE_MIN_DAYS = -719162`・
  `DATE_MAX_DAYS = 2932896`）
- `TIMESTAMP`: `0001-01-01 00:00:00` 〜 `9999-12-31 23:59:59.999999`
  （`TIMESTAMP_MIN_MICROS`／`TIMESTAMP_MAX_MICROS`。`DATE` の範囲に日内
  マイクロ秒を加えた値）

暦は先発グレゴリオ暦（proleptic Gregorian）。日数と年月日の相互変換は
Howard Hinnant の `days_from_civil`／`civil_from_days` アルゴリズム
（`crate::datetime`）を自作実装し、`checked_*` 演算のみを使う。

decode 側（格納済みバイト列。TABLE-7 の untrusted 契約）も範囲を検証し、
範囲外の値は `RowCodecError::Invalid` で fail-closed に拒否する
（`0x00`／`0x01` 以外を拒否する BOOLEAN 列の decode と同じ方針）。

PG 互換の注記: PostgreSQL の内部エポックは 2000-01-01 で、本実装は
1970-01-01（Unix エポック）を採用する。バイナリ結果形式（#998・WIRE-14）で
PG のバイナリ表現を返す場合はオフセット変換が必要になる。

### D-4: タイムゾーン

`TIMESTAMP` はタイムゾーンなし（naive）の値のみを扱う。セッションの
タイムゾーン設定・変換・`AT TIME ZONE` は持たない。入力に TZ 接尾辞
（`Z`、`+hh:mm`、`-hh`）があれば閉じた文法の外として `22007` で拒否する。
値をどのタイムゾーンで解釈するかはクライアント側の責任とする。
`TIMESTAMPTZ` は対象外。

### D-5: リテラル文法（閉じた定義）

入力元は SQL の単一引用符文字列リテラル（`InsertLiteral::String`）を、
束縛時に列型で解釈する（`sql::parser::bind_datetime_literal`）。
`InsertLiteral` に新しい variant は追加しない。

- `DATE`: `Y{4,6}-MM-DD`。年は 4〜6 桁の ASCII 数字（1〜9999 の外は
  `22008`、7 桁以上の長さ超過は `22007`）、月・日は各 2 桁固定。
- `TIMESTAMP`: `<DATE 部><区切り 1 文字>HH:MM:SS[.f{1,6}]`。区切りは
  半角空白または `T` の 1 文字のみ。小数秒は 1〜6 桁（7 桁以上は丸めず
  `22007`）。日付だけの形（時刻部なし）は受け付けない。
  型付きリテラル式（`DATE '...'`・`TIMESTAMP '...'`）の書式違反も同じく `22007`。
- リテラル長は `MAX_DATETIME_LITERAL_LEN`（40 byte）を超えたらパース前に
  拒否する。

出力形式（投影テキスト）は PostgreSQL の既定出力に合わせる:

- `DATE`: `YYYY-MM-DD`（年は常に 4 桁ゼロ埋め）。
- `TIMESTAMP`: `YYYY-MM-DD HH:MM:SS`。小数秒が 0 でなければ `.ffffff` を
  付け、末尾の 0 は削る。

## 実装範囲

- `catalog.rs`: `ColumnType::Date`／`ColumnType::Timestamp`（カタログ型
  タグ `date`／`timestamp`、param は `-`）。
- `row_codec.rs`: `Value::Date(i32)`／`Value::Timestamp(i64)`、
  `ScalarRef::Date(i32)`／`ScalarRef::Timestamp(i64)`。encode/decode/scan/
  merge の各経路を固定長ペイロードとして実装し、decode 側で範囲外を拒否。
- `error_format.rs`: `ErrorClass::DatetimeFieldOverflow`（`22008`）。
- `sql/allowlist.rs`: `SqlSurfaceError::DatetimeFieldOverflow`。
- `sql/parser.rs`: `bind_datetime_literal`（INSERT／UPDATE SET／UPSERT の
  3 経路が共有）、`AggregateInput::DatetimeColumn`（`COUNT` のみ受理）。
  ファイル形 INSERT は DATE／TIMESTAMP 列を拒否する。
- `tenant.rs`: `validate_set_assignments` に固定長（4／8 byte）の累計検証を
  追加。
- `recovery/content_hash.rs`: `Value::Date` → タグ 8、`Value::Timestamp` →
  タグ 9（TABLE-13 の宣言順規則。BOOLEAN のタグ 7 の続き）。
- `sql/exec.rs`・`sql/scan.rs`・`sql/returning.rs`・`sql/aggregate.rs`・
  `sql/group_by.rs`・`sql/scalar_index.rs`・`sql/udf_call.rs`・
  `sql/using_plan.rs`・`scoring_boost.rs`・`core.rs`: 網羅 `match` へ型ごと
  の扱いを追加（`Cell::Date`／`Cell::Timestamp` の投影を含む）。
- wire-server: `result_encoder.rs`・`http/query/response.rs` は ISO テキスト
  へ整形（`engine::datetime::format_date`／`format_timestamp`）。
  `http/query/update.rs` の NoSQL `update` op は JSON 文字列（ISO テキスト）
  の `SET` を受理する。`http/status.rs`・`err4_http_projection.rs`・
  `docs/nosql-api.md` のエラー射影表・分類数を 16 → 17 へ更新。

## Issue #893 追記: スカラー二次索引

`DATE`／`TIMESTAMP` 列を `i64` へ昇格した順序索引
（`sql::scalar_index::OrderedColumnIndex::I64`。`DATE` は日数、`TIMESTAMP`
はマイクロ秒。`TIMESTAMP` の絶対値が `2^53` を超える値を 1 件でも含む列は
列単位で索引対象外へ縮退する。`DATE` は値域が `i32` のため構造的にこの上限
に収まる）で索引化し、WHERE の比較・等価述語（#891・TASK-199 で結線済み。
下記「対象外（申し送り）」参照）から実際に候補削減へ消費される production
結線まで完了した（`sql::scalar_index::ScalarIndex::candidates_for` が
`FilterOp::TypedCompare` を直接消費し、`EXPLAIN` の `scalar_plan:` トークンに
`index_typed_range` を追加。codex-review 指摘・PR #1032）。`ScalarIndex::
build`（production の既定入口）は `DATE`／`TIMESTAMP` を常に構築する。
詳細は `docs/design/scalar-index-prune.md`「Issue #893」節・「レビュー対応」
節参照。

## 対象外（申し送り）

- WHERE の比較・等価述語（#891・TASK-199 で対応済み。範囲・設計は
  `docs/design/scalar-types-predicates.md` 参照。`DATE`／`TIMESTAMP` 列は
  `declarative_filter::FilterOp::TypedCompare` へ束縛される）。式（算術・
  関数引数。`sql::udf_call::bind_expr`）中の参照は引き続き対象外のまま
  （`22000`）。
- `MIN`／`MAX` などの集計拡張（#892）。`COUNT` のみ受理。
- 3 段階デコード tier の最適化（#894）。
- RowDescription の OID `1082`（date）／`1114`（timestamp）の公告（#895・
  WIRE-13）。現状は他の非 VECTOR 列と同じ既定 OID のまま。
- NoSQL `insert` op での JSON 文字列束縛（#896・NOSQL-17）。既存の
  ワイルドカード腕による fail-closed 拒否のまま（`update` との表層間
  非対称は BOOLEAN 列と同じ既知の制約）。
- 回帰テストの拡充（#897・TASK-201）。
- `22P02` の新設（TASK-227・ERR-6）。Issue #1187 で解消済み（書式違反は `22007`）。
- SQL `CREATE TABLE` による宣言（SQL-23）。Rust API（`TableSchema`）での
  宣言までが対象。
- `DATE '...'`／`TIMESTAMP '...'` の型付きリテラル接頭辞。
- `TIMESTAMPTZ`。
- PG バイナリ結果形式のエポック変換（#998）。

## Issue #896 追記

NoSQL 表層の JSON 束縛（`insert`／`update`／`filter`）の型別対応・`columns[].type` の型名整備は Issue #896（NOSQL-17）で実施済み。詳細は `docs/design/nosql-typed-json-binding.md` 参照。
