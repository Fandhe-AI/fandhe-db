# バイナリ形式の結果エンコーディング（WIRE-14）の設計判断

- ステータス: **Accepted（Phase A: エンコーダ層。Phase B の wire 経由結線は
  Issue #934 で実施済み——「Phase B 追記」節参照）**
- 対応: Issue #936（TASK-218・WIRE-14。ポインタ: `docs/spec/05-tasks.md`
  TASK-218・`docs/spec/04-behavior/wire-protocol.md` WIRE-14）
- 実装: `crates/wire-server/src/result_encoder.rs`
- テスト: `crates/wire-server/src/result_encoder.rs`（単体）・
  `crates/wire-server/tests/wire14_binary_format.rs`（結合）

## 背景

wire-server の `RowDescription`／`DataRow` は format code フィールドが 0
（テキスト）固定だった。PostgreSQL wire プロトコル v3 は結果列ごとにテキスト
／バイナリ形式を要求できるが、その要求は拡張クエリプロトコルの **Bind**
メッセージでのみ行われる。

## スコープ（Phase A / Phase B）

Bind／Execute（Issue #934）・Parse／Describe（Issue #933）は本 Issue 時点では
別 Issue の担当（未実装）。型 OID の拡張（Issue #895・WIRE-13）は OID 公告
自体を実施済みだが、新型のバイナリ表現の対応拡大は引き続き別 Issue の担当
（「申し送り」節参照）。

- **Phase A（本 Issue）**: `result_encoder.rs` に置く純関数のエンコーダ層
  （形式コードの解決・対応型の事前検査・型ごとのバイナリレイアウト・
  形式コード付きの `RowDescription`／`DataRow` 組み立て）。wire の送受信
  経路（`simple_query.rs`・`protocol_dispatch.rs`・`handshake.rs`）は変更
  しない。
- **Phase B（#934 以降）**: Bind の結果形式コードを本 API へ渡す結線、
  `0A000` のあと当該文だけを拒否して接続を維持する同期回復（WIRE-11）、
  3 クライアント（psycopg 3 `binary=True`・node pg `binary: true`・生バイト
  Rust クライアント）でのバイナリ受信 e2e。psql は拡張クエリプロトコル
  未対応かつバイナリ結果を要求できないため層 B の対象クライアントから外れる。

本 Issue の実装範囲は WIRE-14 のうちエンコーダ層（バイナリ形式で応答を
組み立てる部分）に限定し、wire 経由で実際にバイナリ形式を要求して値を
受け取るところまでは対象外とする（詳細は spec のビヘイビア定義 WIRE-14
参照）。

## Phase B 追記（Issue #934）

Bind（`handle_bind`）が `ResultFormats::resolve`／`validate_binary_formats`
を呼んで結果 format code を列ごとに解決・事前検査し、Describe(Portal) の
`RowDescription`（`encode_row_description_with_formats`）と Execute の
`DataRow`（`encode_data_row_into_with_formats`）が Bind 時点で確定した同じ
値を参照するよう結線した（`crates/wire-server/src/extended_query.rs`・
`docs/design/wire-extended-query-bind-execute-sync.md` 参照）。`0A000` 後の
同期回復（WIRE-11）は既存の `respond_error_and_await_sync` 経路をそのまま
使う。3 クライアントでのバイナリ受信 e2e（psycopg 3 `binary=True`・
node pg `binary: true`・生バイト Rust クライアント）は本 Issue の対象外の
まま別 Issue へ申し送る。

## 形式コードの列ごとの解決（`ResultFormats::resolve`）

PostgreSQL の Bind 規則に従う。

| 指定個数 | 解決 |
| --- | --- |
| 0 個 | 全列テキスト |
| 1 個 | 全列に同じ値を適用 |
| 列数と同数 | 列ごとに適用 |
| それ以外 | `BinaryFormatError::FormatCountMismatch`（`08P01`） |
| 値が {0, 1} 以外 | `BinaryFormatError::InvalidFormatCode`（`08P01`） |

不正な format code 値は本リポの既存エラー分類との整合上 `08P01` へ寄せた
（詳細は WIRE-14 参照）。

解決結果 `Vec<FormatCode>` の長さは列数（`i16` で有界）にのみ比例する。
`codes` スライス自体の長さ上限検証は、untrusted な Bind 本文を解析する側
（#934）の責務であり、`ResultFormats::resolve` は受け取ったスライス長に
比例した確保を行わない。

## 対応型の事前検査（`0A000`）: 1 バイトも送る前に判定する

`validate_binary_formats` は `RowDescription` を送る前に呼ぶ前提の純関数。
応答列の一部を送ってからエラーにするとフレームが崩壊するため、この順序は
必須。

判定は公告型（`ColumnMeta`／`WireType`）で静的に行い、実行時の `Cell` では
判定しない。

| 列 | 公告 OID | バイナリ可否 |
| --- | --- | --- |
| `ColumnMeta::Id` | numeric（1700） | **非対応 → `0A000`**（詳細は WIRE-14 参照） |
| `Scalar{ty: Text}` | text（25） | 対応（UTF-8 生バイト） |
| `Scalar{ty: Vector(_)}` | text（25） | **非対応 → `0A000`**（詳細は WIRE-14 参照） |
| `ColumnMeta::Computed` | text（25） | **非対応 → `0A000`**（fail-closed。詳細は WIRE-14 参照） |

`WireType::supports_binary`（`Id`／`Text` の型そのものの対応可否）と
`column_binary_support`（列種別を見た最終判定。`Vector`／`Computed` の
上書きを含む）を分離し、`#[deny(clippy::wildcard_enum_match_arm)]` を付けた
網羅 `match` にすることで、`ColumnMeta`／`WireType` に variant が増えたとき
（Issue #895 で実際に増えた）にバイナリ可否の決定漏れをコンパイルエラーで
検出する。

## 型ごとのバイナリ表現（`binary` サブモジュール）

PostgreSQL の send 関数と同じレイアウトで 8 型のバイナリ表現を組み立てる
純関数を用意した（`int4`／`int8`／`float4`／`float8`／`bool_`／`bytea`／
`uuid`／`text`）。本 Issue 時点で `WireType` 側に実際に結線されるのは `text`
のみで、他の型は Issue #895 で `WireType` へ OID 公告が追加された後も
バイナリ表現へは未結線のまま（`column_binary_support`／`supports_binary`
がいずれも `false` を返す。対応拡大は本 doc「申し送り」節）。golden バイト列
の単体テストで PostgreSQL 規約との一致を固定した（例: `int4(1)` →
`00 00 00 01`、`int8(-1)` → `ff` × 8、`float8(1.0)` → `3f f0 00 …`、
`bool_(true)` → `01`）。

`id`（`numeric`）のバイナリ対応拡大は spec 側で未策定のため引き続き未実施
（詳細は WIRE-13 参照）。

## 形式コードを受け取るエンコーダと既存出力の不変性

`encode_row_description_with_formats`／`encode_data_row_into_with_formats`
を新設し、既存の `encode_row_description`／`encode_data_row_into`／
`encode_data_row` は「全列テキスト」でこれらを呼ぶ薄いラッパーへ変更した。
生成バイト列は完全に同一であることを不変性テスト（`row_description_with_
all_text_formats_matches_legacy_encoder`・`data_row_with_all_text_formats_
matches_legacy_encoder`）で固定している。シグネチャは
変えていないため破壊的変更ではない。

`formats.len()` が列数／セル数と一致しない場合は呼び出し元の内部不整合と
みなし `EncodeError`（`XX000`）とする。untrusted 入力由来のバイナリ形式
検査自体は `validate_binary_formats` が別途 `RowDescription` 送出前に
済ませている前提であり、`encode_data_row_into_with_formats` はさらに
「バイナリ指定なのに非対応セル（`Cell::Integer`／`Vector`／`Float`／
`Bool`）が来た」場合も契約違反として fail-closed に拒否する（黙って別表現
へフォールバックしない）。

失敗時に `out` を呼び出し前の長さへ `truncate` する既存の契約（部分フレーム
を残さない）は据え置いた。長さはすべて `i32::try_from`／`checked_*` で
計算し、`as` キャストは使わない。

## セキュリティ（OWASP・AGENTS.md P0）

- **インジェクション**: 形式コードは列挙型へ閉じた写像にし、SQL や文字列の
  組み立てには一切使わない。
- **アクセス制御・テナント境界**: エンコーダは engine が RLS を適用した後の
  `QueryResult` だけを受け取り、可視性判定には関与しない。RLS の経路は
  変わらない。
- **fail-closed**: 非対応型は送出前に `0A000` で拒否する（中途半端な応答を
  送らない）。`Computed` はバイナリ表現が未策定のため拒否側に倒す（Issue #1173 以降は型 OID を公告するが、バイナリ対応の拡大は別途追跡）。`VECTOR`
  を text バイトとして黙って送らない。形式コードの不正は `08P01`。失敗時は
  部分フレームを残さない。
- **情報漏えい**: エラーメッセージに含める情報は最大でも要求者自身の列番号
  まで。テーブル名・他テナントの存在情報は含めない。
- **DoS**: 解決結果の確保量は列数（`i16`）で有界。形式コード列自体の長さ
  上限検証は Bind パーサ（#934）の責務。`unwrap`／`expect`／`[]` は使わない。
- **依存**: 追加なし（`Cargo.toml` は無変更）。

## 申し送り（Phase B 以降）

- Bind の結果形式コードを本 API へ渡す結線・Describe(portal) の
  `RowDescription` への反映 → **#934 で実施済み**（「Phase B 追記」節参照）
- `0A000` のあと当該文だけを拒否して接続を維持する同期回復 →
  **#934 で実施済み**（既存の `respond_error_and_await_sync` 経路をそのまま
  使う）
- パラメータのバイナリ復号 → Issue #1171 で text 系スロットのみ実装済み
  （UTF-8 バイト恒等。それ以外のスロットは `0A000`）。text 以外の型
  （int4・int8・float・bool・bytea・uuid）のバイナリ復号は後続（対象外のまま）
- 型 OID 拡張（`BOOLEAN`／`REAL`／`DOUBLE PRECISION`／`DATE`／`TIMESTAMP`／
  `BYTEA`／`UUID`／`JSON`／`JSONB`）→ **Issue #895 で実施済み**
  （`docs/design/wire-type-oid-mapping.md` 参照）。`id`・これら新型・
  集計列（`Computed`）のバイナリ対応拡大は spec 側で未策定のため引き続き
  未実施
- 3 クライアントのバイナリ受信モードでの値一致（層 B）→ #934 完了後の
  別 Issue（本 doc の対象外のまま）
- カーソル（WIRE-15）→ #937
