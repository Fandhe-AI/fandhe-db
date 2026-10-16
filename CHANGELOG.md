# Changelog

`crates/engine`（`fandhe-db-engine`）・`crates/wire-server`
（`fandhe-db-wire-server`）の公開 API に影響する変更を記録する。
両クレートは同一バージョンで同期して公開する（`crates/wire-server/Cargo.toml`
の `engine` path 依存が完全固定バージョンで参照するため）。

## 0.2.0

### Breaking Changes

- 日付時刻と INTERVAL の加減算・乗除（Issue #1581・TABLE-34）に伴う公開 API の変更:
  公開 enum `engine::sql::udf_call::Expr` に `IntervalLiteral(Interval)`、`BoundExpr` に
  `Interval`・`IntervalColumnRef`、`ExprType`／`ExprValue` に `Interval` を追加した
  （既存の網羅的 `match` はアームの追加が必要）。従来 `42804` か「列を式で使えない」
  エラーだった `TIMESTAMP ± INTERVAL`・`DATE ± INTERVAL`・`INTERVAL * n`・
  `TIMESTAMP - TIMESTAMP` などの INTERVAL 算術と、INTERVAL 列の式内参照・型付きリテラル
  `INTERVAL '…'` が受理に変わる。INTERVAL が絡む未対応の演算子の組は `42883` で拒否する。
- `AT TIME ZONE`・TIMESTAMPTZ の変換と問い合わせ（Issue #1576・TABLE-33）に伴う公開 API の変更:
  公開 enum `engine::sql::udf_call::Expr` に `TimestampTzLiteral { micros, session_local }`、
  `BoundExpr` に `TimestampTz`・`TimestampTzColumnRef`、`ExprType`／`ExprValue` に `TimestampTz`、
  `BuiltinFn` に TZ 変換・`DatePartTz`・`DateTruncTz` 系の variant、
  `datetime_fn::DatePartField` に `Timezone`・`TimezoneHour`・`TimezoneMinute`、
  `declarative_filter::TypedLiteral` に `TimestampTz` を追加した（既存の網羅的 `match` は
  アームの追加が必要）。`timezone` を組み込み関数名として予約したため、同名の UDF は
  作れない（`42723`）。TIMESTAMPTZ の述語・集計・二次索引・集合演算は従来の `0A000`／`42883`
  拒否から受理へ変わる。
- セッション TimeZone（Issue #1575・TABLE-33）に伴う公開 API の変更: 公開 enum
  `engine::sql::allowlist::Statement` に `SetTimeZone { value: TimeZoneSetting }`・`ShowTimeZone`、
  公開 enum `engine::sql::SqlOutcome` に `SetTimeZone`・`Show(QueryResult)` を追加した
  （既存の網羅的 `match` はアームの追加が必要）。`engine::datetime::SessionTimeZone` は表示値を
  持つ構造体になり、`SessionState::time_zone`・`SessionTimeZone::parse_setting` を追加した。
  pg wire の起動時 ParameterStatus に `TimeZone`（既定 `UTC`）が増え、StartupMessage の
  `TimeZone` を受理する（不正値は認証後に FATAL `22023`）。
- crate 名を `fandhe-vector-db-engine` / `fandhe-vector-db-wire-server` から
  `fandhe-db-engine` / `fandhe-db-wire-server` へ変更した（プロジェクト名を
  vector-db から fandhe-db へ改めたため）。0.2.0 は新しい crate 名での初公開版となる。
  旧 crate 名の 0.1.0 は crates.io に残すが、以後は更新しない。移行は `Cargo.toml` の
  依存名を新しい crate 名へ置き換える（lib 名 `engine` / `wire_server` は不変のため
  `use` 文の変更は不要）。
- バッチ上限の環境変数を `VECTOR_DB_BATCH_MAX_FILES` / `VECTOR_DB_BATCH_MAX_TOTAL_BYTES` /
  `VECTOR_DB_BATCH_MAX_CHUNKS` から `FANDHE_DB_BATCH_MAX_FILES` /
  `FANDHE_DB_BATCH_MAX_TOTAL_BYTES` / `FANDHE_DB_BATCH_MAX_CHUNKS` へ改名した。旧名は
  読まれず（設定しても既定の上限が適用される）、設定されていれば起動時に stderr へ
  警告が出る。移行は環境変数名を新名へ置き換える。
- `engine::sql::allowlist::ValidatedInsert` の公開フィールド
  `pub values: Vec<InsertLiteral>` を `pub rows: Vec<Vec<InsertLiteral>>` へ
  変更した。単一行 INSERT は `rows.len() == 1` として同じ内容を保持するため、
  移行は `stmt.values` を `stmt.rows[0]`（単一行前提のコードの場合）へ
  読み替えるか、複数行を扱う場合は `rows` を反復する形へ書き換える。
- 書き込み文の現在時刻関数（Issue #2188・SQL-37）に伴う公開 API の変更: 公開 enum
  `engine::sql::allowlist::InsertLiteral` に `CurrentTime(TimeDefault)`、公開 enum
  `engine::catalog::TimeDefault` に `StatementTimestamp`・`ClockTimestamp`・`LocalTime(Option<u8>)`
  を追加した（網羅 `match` は更新が必要）。`engine::sql::parser::BoundInsert::volatile_default_columns`
  の型を `Vec<usize>` から `Vec<VolatileColumn>`（新しい公開 enum）へ変更した。移行は
  `VolatileColumn::Default(i)` を従来の位置 `i` として読み替える。
- `operation_id` 台帳に値フォーマット v3（ハッシュ＋評価に用いたトランザクション開始時刻）を追加した。
  時刻関数を評価した書き込みだけが v3 を書く。v3 を書いた DB を旧バイナリで開くと、該当キーの再送照合が
  `XX000`（fail-closed）になる（ダウングレード不可の既知の制約。移行処理は無い）。
- `engine::sql::parser::BoundInsertForm` に新しい variant `RowBatch(Vec<BoundInsert>)`
  を追加した。この enum を網羅的に `match` していた利用側コードは、新 variant
  への対応を追加しないとコンパイルできない。
- いずれも SQL 表層の複数行 `VALUES (...), (...)` 構文サポート（ビヘイビア ID:
  SQL-16、TASK-190）のための変更であり、単一行 `INSERT` の外部観測可能な挙動
  （wire プロトコル応答・`wire_code`）は不変。
