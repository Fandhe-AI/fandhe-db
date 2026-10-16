# ADR: DATE / TIMESTAMP 列型

- ステータス: Accepted
- 対象: TABLE-1・TABLE-7・TABLE-13・TASK-197（`docs/spec/05-tasks.md`・
  `docs/spec/04-behavior/data-model.md`）
- 関連: `column-type-extension.md`（BOOLEAN 列型・TASK-196。同じ拡張方式を
  踏襲）、ERR-6（`docs/spec/04-behavior/error-format.md`）

本ドキュメントは実装既定値・設計判断のみを記す。private spec 本文は転記せず
ビヘイビア ID・タスク ID のポインタ表記に限る（`.claude/rules/spec-confidentiality.md`）。

## 背景

列型は `TEXT`・`VECTOR(N)`・`BOOLEAN` のみで、日付・日時を列として宣言・
格納・読み出しできなかった。本 Issue で `DATE`／`TIMESTAMP` 列型を追加する。

## 決定

### D-1: `wire_code` の割り当て

- 範囲外・暦上不正（月 13、2/30、非閏年の 2/29、時 24、分 60、秒 60、
  年 0000、年 10000 以上等）は新設分類 `DatetimeFieldOverflow`（`22008`。
  ERR-6 ポインタ）へ写像する。
- 書式違反（区切り文字違い、桁数不足、TZ 接尾辞、小数 7 桁以上、前後空白、
  非 ASCII 数字、長さ超過等）は Issue #1187 で `InvalidDatetimeFormat`（`22007`）へ
  変更した（当初は `InvalidInput`〔`22000`〕）。

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
`TIMESTAMPTZ` は本節の対象外で、UTC 保存の別型として追加した
（[Issue #1574 追記](#issue-1574-追記-timestamptz-列型)）。TZ データ源の承認資料は
[timestamptz-tz-data-adoption.md](timestamptz-tz-data-adoption.md)（承認済み・2026-10-16）。

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

## Issue #1569 追記: TIME 列型

`TIME [WITHOUT TIME ZONE]` を独立の `ColumnType::Time` として追加した（TABLE-32・親 #1568 のポインタ。第 1 段）。

- **内部表現**: 0 時起点のマイクロ秒 `i64`。受理範囲は `00:00:00`〜`24:00:00`（両端を含む。`24:00:00` は受理、`24:00:00.000001` は `22008`）。カタログタグは `time`（`param` は `-` 固定）。値は `Value::Time`／`ScalarRef::Time`／`Cell::Time` の専用 variant とし、TIMESTAMP の variant を流用しない（日時として誤投影・誤比較される fail-open を防ぐ）。行バイトは presence + `i64 LE`（9 バイト。`SCALAR_TIME_ENTRY_LEN`）で、encode・decode・scan のすべてで `validate_time_micros` により範囲を再検証する。`content_hash` は未使用のタグ 16 を採番した。
- **閉じた文法**: `HH:MM[:SS[.f+]]`。HH・MM・SS は 2 桁 ASCII 数字固定。前後空白・TZ 接尾辞・AM/PM・特殊語・非 ASCII・長さ超過は `22007`、範囲外は `22008`（DATE／TIMESTAMP と同じ写像）。
- **丸め**: 端数は 7 桁目で丸め（0.5 は 0 から遠い側）、8 桁目以降は数字確認のみで捨てる。丸めた後に範囲を判定する（`23:59:59.9999995` は `24:00:00` として受理）。PostgreSQL は浮動小数の丸めを使うため、0.5 ちょうどの境界は乖離しうる。
- **PostgreSQL との乖離**: 秒 60 を繰り上げず `22008` で拒否する（TIMESTAMP の D-5 と同じ）。TZ 接尾辞・AM/PM・1 桁の時は受理しない。
- **整形**: `HH:MM:SS[.ffffff]`（端数 0 なら省略、末尾の 0 は削る）。端数の整形は `format_timestamp` と共通ヘルパーを共有する。
- **型名**: `TIME`・`TIME WITHOUT TIME ZONE` は受理。`TIME(p)`・`TIME WITH TIME ZONE`・`TIMETZ`・`time[]` は構文段で `0A000`。`TIME` は綴りを保持する別名として返し、同名 ENUM との衝突を実行段が検出する。`time`・`timetz` は `CREATE_ONLY_RESERVED_TYPE_NAMES` へ追加した（`RESERVED_TYPE_NAMES` へは足さない）。
- **暫定挙動**: JOIN キー・PK／UNIQUE／FK・`ALTER COLUMN TYPE`（TIME との相互変換）は既存の拒否コードで fail-closed（述語・集計・並べ替え・索引・集合演算・SET 式・EXTRACT は Issue #1570 で解禁。下記）。wire の OID 1083・バイナリ入出力と NoSQL の insert／update／DDL の型束縛は #1571 で対応済み。NoSQL の filter は本 Issue の述語解禁後の別対応。
- **対象外**: NoSQL filter、`TIME(p)` の保存精度（兄弟 Issue に担当なし。要確認）、`time[]`（OID 1183）。

## Issue #1570 追記: TIME の述語・集計・索引・EXTRACT

TABLE-32・親 #1568 のポインタ（第 2 段）。#1569 で fail-closed にしていた問い合わせ側を解禁した。

- **比較・索引**: 述語は `TypedLiteral::Time`（レーン B の `TypedCompare`）、二次索引は `OrderedColumnIndex::I64`（`DATE`／`TIMESTAMP` と同じ構築・照会経路）。索引あり・全走査・Rust オラクルの 3 者一致を結合テストで固定する。`24:00:00` は有効な最大値で、`'24:00'` と同じ値になる。
- **並べ替え・グループ化・集計**: `OrderKind::SignedInt` を共有する。グループキーを `Cell::Time` へ戻す際は `24:00:00` を許容するため上端を含めて検査する。`MIN`／`MAX`／`COUNT`／`COUNT(DISTINCT)` を受理し、`SUM`／`AVG` は `42883`。`HAVING` で `MIN`／`MAX` の数値比較対象にすることは `DATE`／`TIMESTAMP` と同じく拒否する。
- **式層**: `TIME` 列参照と型付きリテラル `TIME '...'`（AST は解析済みの値を保持し、CHECK・ビューの描画は `format_time` の出力だけを使う）を受理する。`TIME` 同士の比較のみ許可し、`TIMESTAMP`／`DATE` との暗黙昇格は PostgreSQL に無いため行わない（`42883`）。型なし文字列リテラルは相手が `TIME` のとき TIME として解析する。算術（`time ± interval` 等）は未実装のまま拒否する。
- **EXTRACT／date_part**: 既存の `date_part` は暦計算で日付境界へ折り返し `24:00:00` が hour=0 になるため流用せず、`datetime_fn::time_part` を新設した。受理する field は `hour`・`minute`・`second`（小数含む）・`milliseconds`・`microseconds`・`epoch`（0 時からの秒数）。日付由来の field は `0A000`（束縛時に拒否）、`date_trunc(text, time)` は PostgreSQL に存在しないため `42883`、未知の field は DATE／TIMESTAMP と同じ `22000`。
- **残した fail-closed**: PK／UNIQUE／FK・`ALTER COLUMN TYPE`・JOIN キー（担当 Issue 未定）。wire OID 1083・バイナリ・NoSQL は #1571。

## Issue #1574 追記: TIMESTAMPTZ 列型

`TIMESTAMPTZ`／`TIMESTAMP WITH TIME ZONE` を独立の `ColumnType::TimestampTz` として追加した（TABLE-33・親 #1572 のポインタ。第 1 段）。

- **内部表現**: UTC の 1970-01-01 起点マイクロ秒 `i64`。受理範囲は UTC で `0001-01-01 00:00:00Z`〜`9999-12-31 23:59:59.999999Z`（`TIMESTAMP` と同じ）。カタログタグは `timestamptz`（`param` は `-` 固定）。値は `Value::TimestampTz`／`ScalarRef::TimestampTz`／`Cell::TimestampTz` の専用 variant とし、naive の `Timestamp` を流用しない（naive 値と UTC 値の取り違えによる fail-open を防ぐ）。行バイトは presence + `i64 LE`（9 バイト。`SCALAR_TIMESTAMPTZ_ENTRY_LEN`）で、encode・decode・scan のすべてで `validate_timestamptz_micros` により範囲を再検証する。`content_hash` は未使用のタグ 17 を採番した（UTC へ正規化した後の値をハッシュするため、`+09` と `Z` の同一時刻は同じハッシュになる）。
- **閉じた文法**: `<TIMESTAMP の D-5 文法>[空白 0〜1 個]<ゾーン>`。ゾーンは `Z`／`z`、数値 offset（`±HH`・`±HHMM`・`±HH:MM`・`±HH:MM:SS`。数字は 2 桁固定、時 0〜15、分・秒 0〜59）、IANA 名（bundled tzdb で解決できるもの）。日時部は `parse_timestamp` と同一の実装を共有する（日付だけ・秒なし・小数 7 桁以上は `22007`）。長さは解析前に `MAX_TIMESTAMPTZ_LITERAL_LEN` で拒否し、非 ASCII は `22007`。
- **正規化**: ゾーンのある入力は UTC へ変換して格納する。ゾーンの無い入力はセッション TimeZone の壁時計として解釈する。セッション TimeZone の保持は #1575 の担当のため、本 Issue では `SessionTimeZone::utc()` 固定（差し替え口は `parse_timestamptz` の `default_tz` 引数）。`SessionTimeZone` は jiff の型を公開 API へ出さない不透明型で、`TimeZoneDatabase::bundled()` から解決したものだけを保持する（`jiff::tz::db()`・`TimeZone::system()` は使わない）。TZ 名は長さ・許可文字（ASCII 英数と `/`・`_`・`+`・`-`）を検査してから解決し、`Etc/Unknown` は明示的に拒否する。
- **DST の解決**: 名前付き TZ の gap（存在しない時刻）は直前の offset、fold（重複する時刻）は後の offset で解決する（PG の `DetermineTimeZoneOffset` の既定と同じ見込み。例: NY の `2026-03-08 02:30` は `-05` として `07:30Z`、`2026-11-01 01:30` は `-05` として `06:30Z`）。jiff の既定に任せず自前で選んでいる。PG 17 の実測との照合は #1576 で完了し確定した（[timestamptz-conversion.md](timestamptz-conversion.md)）。
- **SQLSTATE**: 書式違反 `22007`、範囲外（UTC へ直して範囲外、offset の範囲外を含む）`22008`、解決できない TZ 名 `22023`（detail は静的文字列で入力を反射しない）。
- **整形**: `format_timestamptz(utc_micros, &SessionTimeZone)` は `YYYY-MM-DD HH:MM:SS[.ffffff]+HH[:MM[:SS]]`（分・秒が 0 でなければ付ける）。局所年が 0 以下になる場合は PG に合わせ年を `1 - year` とし offset の後ろに「 BC」（先頭に空白を伴う）を付ける。局所年 10000 は 5 桁のまま出す。LMT のような秒を含む offset も整形できる。現状の呼び出し元（投影・COPY TO・HTTP JSON）はすべて UTC 固定（`+00`）。
- **型名**: `TIMESTAMPTZ`・`TIMESTAMP WITH TIME ZONE` を受理（`TIMESTAMPTZ` は綴りを保持する別名として返し、同名 ENUM との衝突を実行段が検出する）。`TIMESTAMPTZ(p)`・`timestamptz[]` は構文段で `0A000`。`timestamptz` は `CREATE_ONLY_RESERVED_TYPE_NAMES` へ追加した。
- **PostgreSQL との乖離**: (1) 日付だけ・秒なし・小数 7 桁以上・1 桁の時の offset は受理しない。(2) 範囲は UTC で 0001〜9999 年。(3) offset の範囲外は PG では `22009` だが、Issue の写像に従い `22008`。(4) TZ 略称（`JST` 等）と POSIX 形式は #1575 まで `22023`。
- **暫定挙動（fail-closed）**: 述語・ORDER BY・GROUP BY・集計・window・集合演算・式・二次索引・EXTRACT／date_trunc は #1576 で解禁済み（[timestamptz-conversion.md](timestamptz-conversion.md)）。UDF 引数は引き続き拒否する。PK／UNIQUE／FK・JOIN キー・`ALTER COLUMN TYPE` は担当 Issue 未定。wire は暫定で text（OID 25）を公告してバイナリは非対応、NoSQL は型束縛・filter とも `TypeMismatch` で拒否する（OID 1184 とバイナリ・NoSQL は #1577）。JSON 応答の型名は `"timestamptz"`。
- **対象外**: セッション TimeZone（`SET TIME ZONE`／StartupMessage／`SHOW`）・POSIX 形式は #1575 で実装済み（[session-timezone.md](session-timezone.md)。TZ 略称は PG と同じく `SET` で `22023`）。`AT TIME ZONE`・他型との変換・述語・集計・索引・EXTRACT・DST 解決の PG 17 照合は #1576 で実装済み（[timestamptz-conversion.md](timestamptz-conversion.md)）。`now()`／`CURRENT_TIMESTAMP` の戻り値型の切り替え、`TIMESTAMPTZ(p)`、`timestamptz[]`（OID 1185）、特殊値 `infinity`・`epoch`・`now` は担当 Issue 未定。

## Issue #1577 追記: TIMESTAMPTZ の wire OID とバイナリ入出力

TABLE-33・WIRE-13・WIRE-14・NOSQL-13・NOSQL-17 のポインタ。#1574 の暫定状態（wire は text・OID 25、バイナリと NoSQL 型束縛は拒否）を解消した。

- **wire**: OID 1184・typlen 8 で公告し、バイナリ結果・バイナリ param に対応（詳細は [wire-binary-format.md](wire-binary-format.md)）。テキスト出力はセッション TimeZone で整形する（#1575）。
- **NoSQL**: insert／update は JSON 文字列を束縛し（数値・真偽値は `42601`）、書式・範囲・TZ 名の検査は engine の `parse_timestamptz`（`22007`／`22008`／`22023`）に委ねる。`create_table`／`add_column` の型名 `"timestamptz"` を受理する。
- **対象外**: NoSQL filter・述語（#1576 の後）、`timestamptz[]`（OID 1185）、COPY バイナリ、特殊値 `infinity`。

## Issue #1579 追記: INTERVAL 列型

`INTERVAL` を独立の `ColumnType::Interval` として追加した（TABLE-34・親 #1578 のポインタ。第 1 段。型の格納表現と入出力の書式だけを用意し、比較・算術・EXTRACT・wire OID は後続 Issue が担う）。

- **内部表現**: 月（`i32`）・日（`i32`）・マイクロ秒（`i64`）の三要素 `datetime::Interval`。三要素は独立で、`1 mon` と `30 days`、`24 hours` と `1 day` は別の格納値になる（SQL の比較は #1580 の正規化キー）。`PartialEq`／`Hash` は生の三要素の一致で codec・content_hash 専用。値は `Value::Interval`／`ScalarRef::Interval`／`Cell::Interval`。カタログタグは `interval`（`param` は `-` 固定）。行ペイロードは presence(1) ＋ micros(8, LE) ＋ days(4, LE) ＋ months(4, LE) の 17 バイト。`content_hash` のタグは 18（正規化しない生の三要素をハッシュする）。
- **無限大の番兵**: PostgreSQL 17 の予約値 `(i32::MAX, i32::MAX, i64::MAX)`・`(i32::MIN, i32::MIN, i64::MIN)` は有効な値として受理しない。入力 `infinity`／`-infinity` は `0A000`、解析結果が番兵と一致した場合は `22008`。encode・decode・scan・`tenant::validate_set_assignments` で多層に拒否する（将来 infinity を解禁しても番兵の意味が衝突しない）。
- **閉じた文法**（`datetime::parse_interval`。長さ上限 255 バイト、フィールド数上限 25、ASCII のみ、前後の空白と大小文字は許容）: (a) 単位句 `[@] 要素… [ago]`（要素は符号付き数値＋単位、または時刻部 `[±]H+:MM[:SS[.f]]`。単位なしの数値は秒、直後が時刻部なら日）、(b) SQL 標準形 `Y-M`・`D H:MM:SS`、(c) ISO 8601（記号形式 `P1Y2M3DT4H5M6S`、代替形式 `P0001-02-03T04:05:06`。`P`・単位は大文字）。単位の綴りは `microsecond(s)`・`us`・`msec`・`second(s)`・`sec`・`minute(s)`・`min`・`hour(s)`・`hr`・`day(s)`・`week(s)`・`month(s)`・`mon`・`year(s)`・`yr`・`decade(s)`・`century/centuries`・`millennium/millennia` などを受理する。
- **端数の繰り下げ**: PG17 と同じ規則。年・decade・century・millennium の端数は整数月へ偶数丸めし、残余は日へ繰り下げない。月の端数は 30 日単位で日へ、日の端数は µs へ（偶数丸め）。例: `1.5 years` は `1 year 6 mons`、`0.1 years` は `1 mon`、`1.5 months` は `1 mon 15 days`、`1.5 days` は `1 day 12:00:00`。累積は `years`・`months`・`days`（`i32`）と `micros`（`i64`）の中間値に `checked_*` で積み、最後に `年×12＋月` を合成する。
- **SQLSTATE**: 書式違反 `22007`、フィールド値の範囲外（数値が `i64` に収まらない・単位換算や累積があふれる）`22015`（`ErrorClass::IntervalFieldOverflow` を新設）、合成後の範囲外・番兵一致 `22008`、`infinity` `0A000`。detail は静的文字列で入力を反射しない。
- **整形**: `format_interval` は `IntervalStyle=postgres`（`EncodeInterval` と同じ規則）。年・月・日を順に `N year(s)`・`N mon(s)`・`N day(s)`（単数形は値が 1 のときだけ）、直前が負で今が正のときだけ `+` を付ける。時刻部は `HH:MM:SS[.ffffff]`（端数の末尾の 0 は削る。時は 2 桁以上）で、全要素 0 なら `00:00:00`。`parse_interval(format_interval(v)) == v` が番兵以外の全域で成り立つ。
- **型名**: `INTERVAL`（綴りを保持する別名として返し、同名 ENUM との衝突を実行段が検出する）。`interval` は `CREATE_ONLY_RESERVED_TYPE_NAMES` に追加した。`INTERVAL(p)`・フィールド制限付き（`INTERVAL YEAR TO MONTH` 等）・`interval[]` は構文段で `0A000`。
- **暫定挙動（fail-closed）**: #1579 時点で受理するのは DDL の列宣言、INSERT／UPDATE SET／UPSERT／COPY FROM／列 DEFAULT（文字列リテラル）、投影・RETURNING・COPY TO の整形、`IS NULL`。述語（`IS NULL` 以外）・ORDER BY・集計・索引宣言は #1580 で解禁した（下の「Issue #1580 追記」）。GROUP BY・SELECT DISTINCT・window・集合演算・サブクエリの値族・scoring_boost は担当 Issue 未定（代表値の規則の確定が必要）で拒否のまま。式中の列参照・型付きリテラル `INTERVAL '...'`・算術は #1581 まで拒否する。wire は暫定で text（OID 25）を公告し値は `format_interval` の文字列で出す。バイナリの結果列・パラメータは非対応、NoSQL の型束縛と filter は `42601`（JSON 応答の型名は `"interval"`）。いずれも #1583。PK／UNIQUE／FK・JOIN キー・`ALTER COLUMN TYPE`・ファイル形 INSERT は担当 Issue 未定のため既存の拒否コードで止める。
- **PostgreSQL との乖離**: (1) `infinity`／`-infinity` は未対応。(2) 単位語の「先頭 10 文字で照合する」癖は再現しない（完全一致のみ）。(3) `INTERVAL(p)` とフィールド制限は `0A000`。(4) `IntervalStyle` は `postgres` 固定（`SET IntervalStyle`・ParameterStatus は対象外）。
- **対象外**: `ExprType::Interval`・算術・型付きリテラル式（#1581）、EXTRACT／date_part（#1582）、OID 1186・バイナリ・NoSQL（#1583）、フィールド修飾子付きのリテラル（`'1' DAY`）、`interval[]`（OID 1187）、PK／UNIQUE／FK・JOIN キー・型変更（担当未定）。

## Issue #1580 追記: INTERVAL の比較・集計・索引

INTERVAL 列の述語・ORDER BY・集計・二次索引を解禁した（TABLE-34・親 #1578 のポインタ。第 2 段）。

- **比較キー**: `datetime::interval_cmp_key` が PG17 の `interval_cmp_value` と同じ換算（1 か月 = 30 日、1 日 = 24 時間）で 1 本のスカラーにする。`key = micros + (months * 30 + days) * 86_400_000_000` を **i128** で計算する。`months * 30 + days` は約 6.7e10 以内、日数 × 8.64e10 µs は約 5.8e21 以内で、i64 には収まらないが i128（約 1.7e38）には余裕で収まる。正規化等価は生の等価より粗く、`'1 mon'`・`'30 days'`・`'720 hours'` は同じキーになる。
- **`Ord` は実装しない**: `Interval` の `PartialEq`／`Eq`／`Hash` は生の三要素（codec・content_hash・`validate_interval` 専用）で、正規化した `Ord` を同居させると `Ord` と `Eq` の整合契約に反する。SQL の意味論（比較・並べ替え・DISTINCT・索引）は必ず `cmp_interval`／`interval_cmp_key` を通す。
- **WHERE**: `FilterOp::TypedCompare`／`InTyped`／`BetweenLiteral` に対応する。`TypedLiteral::Interval(i128)` は**正規化キー**を保持し、行側も `ScalarRef::as_interval` からキーへ変換して比べる（TIME の腕のように生の等価で比べると、`v IN ('30 days')` が格納済みの `'1 month'` を取りこぼす）。Describe 用のダミー（`Interval(0)`）を compare・in・between の 3 箇所に追加した。
- **ORDER BY**: `OrderKind::Interval`。キーは生の三要素を保持し比較は `cmp_interval`。同点の行は既存の副次順序（id → テナント）で決まる。`resolve_order_kind` は GROUP BY のキー解決と共有されるため、`resolve_group_by_key` が INTERVAL を `resolve_order_kind` の前に明示的に拒否する（SELECT DISTINCT も同じ関数を通る）。`NULLS FIRST/LAST` の明示指定は engine 全体で未対応のため対象外（PG 既定の NULL 位置のみ）。
- **集計**: `COUNT`・`COUNT(DISTINCT)`（正規化キー 16 バイトで重複除去）・`MIN`/`MAX`・`SUM`・`AVG`。`MIN`/`MAX` は正規化キーで比べ**元の三要素**を返す。キーが同点のときは PG17 の遷移関数（`interval_smaller`／`interval_larger`）と同じく後から観測した値を採る。PG は aggsortop により索引経由で求めることもあり、同点時の代表は実装依存になる点が乖離になりうる。`SUM` は三要素ごとの checked 加算（正規化しない。`'1 mon' + '30 days'` は `'1 mon 30 days'`）で、あふれは `22008`。`AVG` は和（あふれは `22008`）を `interval_div_f64`（PG17 `interval_div` 相当。#1581 の乗除でも再利用する）で割る。固定ベクタ: `AVG('1 mon','2 mons')` = `1 mon 15 days`、`AVG('1 day','2 days')` = `1 day 12:00:00`、`AVG('1 mon','00:00:00')` = `15 days`。全行 NULL・0 行は NULL（`COUNT(DISTINCT)` は 0）。結果型は INTERVAL。HAVING の数値比較対象と式内の参照は拒否のまま。
- **二次索引**: `OrderedColumnIndex::Interval(Vec<(i128, u32)>)`。`I128` は NUMERIC 専用（列 scale を持つ）のため流用せず、`(I128, Numeric)` の照会腕が INTERVAL 列から到達しないようにした。自動索引と `CREATE INDEX` の宣言（`is_declarable_scalar_index_type`）の両方で構築し、キーは評価側と同じ `interval_cmp_key` なので索引経路と全走査の結果は一致する。`InTyped` は他型と同じく索引未対応（全走査）。
- **fail-closed のまま（担当 Issue 未定）**: GROUP BY・SELECT DISTINCT・window（PARTITION BY／ORDER BY のキーと集計の入力）・集合演算・サブクエリの値族・scoring_boost。正規化等価な値を 1 グループにまとめるとき、どちらの三要素を代表値として出すかの規則（PG との整合）が未確定のため。PK／UNIQUE／FK・JOIN キー・`ALTER COLUMN TYPE` も現状維持。
- **対象外**: 式・算術・型付きリテラル `INTERVAL '...'`・列同士の比較（#1581）、EXTRACT（#1582）、OID 1186・バイナリ・NoSQL の filter／型束縛（#1583。下の「Issue #1583 追記」で対応）。

## Issue #1583 追記: INTERVAL の wire OID とバイナリ入出力・NoSQL

TABLE-34・WIRE-13・WIRE-14・NOSQL-13・NOSQL-14・NOSQL-17 のポインタ。#1579 の暫定状態（wire は text・OID 25、バイナリと NoSQL は拒否）を解消した。wire-server のみの変更で、engine は変更しない。

- **wire**: OID 1186・typlen 16・型名 `interval` で公告し、バイナリ結果（time i64・day i32・month i32 の 16 バイト BE）とバイナリ param に対応（詳細は [wire-binary-format.md](wire-binary-format.md)）。**契約変更**: INTERVAL 列の公告 OID が 25 から 1186 へ変わる。
- **NoSQL 型束縛・DDL**: insert／update は JSON 文字列を束縛し（数値・真偽値・配列・オブジェクトは `42601`）、書式・範囲の検査は engine の `parse_interval`（`22007`／`22015`／`22008`、`infinity` は `0A000`）に委ねる。`create_table`／`add_column` の型名 `"interval"` を受理する（`interval(p)` などの修飾付きは拒否のまま）。
- **NoSQL filter**: `eq`／`ne`／範囲比較／`between`／`in`／`is_null` を DATE／TIMESTAMP／UUID と同じ文字列レーンで解禁した（比較は engine の正規化キー。`1 mon` と `30 days` は等価）。述語形の update／delete の filter も同じ束縛点を共有する。`prefix`／`like` は TEXT 系専用のまま（`22000`）。
- **NoSQL sort**: `scan` の `sort` に INTERVAL 列を指定すると SQL の `ORDER BY` と同じ正規化キー順（同順位は id）になることをテストで固定した。変更は不要だった。
- **対象外**: `interval[]`（OID 1187）、`INTERVAL(p)`・フィールド修飾付きの列型、`infinity`／`-infinity` の受理、`SET IntervalStyle` と ParameterStatus、COPY バイナリ、GROUP BY・DISTINCT・PK／UNIQUE／FK・JOIN キー（担当 Issue 未定）、式・算術（#1581）、EXTRACT（#1582）。

## Issue #1581 追記: 日付時刻と INTERVAL の加減算・乗除

TABLE-34・親 #1578 のポインタ。#1579 の暫定状態（式中の INTERVAL 列参照・型付きリテラル・算術は拒否）を解消した。純関数は `sql/interval_arith.rs`、束縛は `udf_call::bind_binary`、評価は `eval_binary`（再帰評価と `expr_program` の双方が共有）。

- **式の型**: `ExprType::Interval`／`ExprValue::Interval`／`BoundExpr::{Interval, IntervalColumnRef}`／`Expr::IntervalLiteral`。結果列型は `ColumnType::Interval`（wire は #1583 で OID 1186・typlen 16・バイナリ入出力に対応済み）。
- **型規則**: `TIMESTAMP ± INTERVAL`・`INTERVAL + TIMESTAMP` は TIMESTAMP、`DATE ± INTERVAL` は TIMESTAMP（深夜 0 時へ昇格）、`TIMESTAMPTZ ± INTERVAL` は TIMESTAMPTZ（セッション TZ 依存のため CHECK では `0A000`）、`INTERVAL ± INTERVAL`・`INTERVAL * n`・`n * INTERVAL`・`INTERVAL / n` は INTERVAL、`TIMESTAMP(TZ) - TIMESTAMP(TZ)`（DATE・混在を含む）は INTERVAL、`DATE - DATE` は従来どおり日数。INTERVAL が絡む上記以外の組は `42883`、INTERVAL を伴わない既存の不一致（`TIMESTAMP + 1` 等）は従来どおり `42804`。
- **意味論（PG17 準拠）**: 月 → 日 → 時刻の順に加算し、月加算後の日は新しい月の末日へ切り詰める（`2024-01-31 + 1 mon` は `2024-02-29`、`+ '1 mon 1 day'` は `2024-03-01`）。`TIMESTAMPTZ` はセッション TZ の壁時計で月・日を加算し UTC へ戻す（gap は直前の offset、fold は後の offset。#1576 と同じ）。jiff の年範囲（±9999）を超える未来側の中間値は、暦が一致する 400 年窓（9200〜9599 年）へ写して tzdb の将来規則で offset を引く（2007 年以前の歴史的規則を拾わない。過去側は 2000〜2399 年）。減算は INTERVAL の各要素を符号反転して加算する（`i32::MIN`／`i64::MIN` は `22008`）。時点の差は日数と日内マイクロ秒に分け符号を揃える（`interval_justify_hours`）。乗除は PG17 の `interval_mul`／`interval_div` と同じ月 → 日 → 秒へのカスケード（端数は µs の偶数丸め）。
- **SQLSTATE**: 結果・中間値が範囲外（PG の timestamp 範囲を超える中間値を含む）、INTERVAL の要素あふれ、乗除の NaN は `22008`（±∞ の係数は infinity の interval を要するため `0A000`）。0 除算は `22012`。エラー文言は固定の英語で値を含めない。
- **#1580 との境界**: #1580 の比較・集計・索引は列参照が対象で、INTERVAL 式（算術結果）の比較・`NULLIF`・`ORDER BY`・集計（SUM/AVG/MIN/MAX）・GROUP BY は `42883`／`0A000` の fail-closed のまま。正規化キー比較の式への解禁は別途。
- **既知の差分**: (1) 本 DB の TIMESTAMP 範囲は 0001〜9999 年のため、PG では成功する範囲外の結果が `22008` になる。(2) 型なし文字列 `'2024-01-31' + INTERVAL …` は本 DB では TEXT 扱いで `42883`（PG は interval として解釈し `22007` 等で失敗する。いずれも成功しない）。(3) 単項マイナス `-INTERVAL`・CAST（`::`）は文法に無い。(4) `INTERVAL '1' DAY` などのフィールド修飾子・`INTERVAL(p)` は `0A000`。(5) `TIME ± INTERVAL`・`TIME - TIME` は `42883`。(6) INTERVAL 式を `jsonb` へ変換する形は `0A000`。
- **対象外**: `justify_*`／`age`／`make_interval`、EXTRACT／date_part／date_trunc の INTERVAL 対応（#1582）、OID 1186・バイナリ・NoSQL（#1583）、infinity の interval、`IntervalStyle` の切り替え。
