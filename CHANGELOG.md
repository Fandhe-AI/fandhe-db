# Changelog

`crates/engine`（`fandhe-db-engine`）・`crates/wire-server`
（`fandhe-db-wire-server`）の公開 API に影響する変更を記録する。
両クレートは同一バージョンで同期して公開する（`crates/wire-server/Cargo.toml`
の `engine` path 依存が完全固定バージョンで参照するため）。

## 0.2.0

### Breaking Changes

- `TRUNCATE ... [CASCADE | RESTRICT]`（Issue #1644・SQL-48 (2)）: 従来 `42601` だった `CASCADE`／`RESTRICT` を
  受理する（省略時は RESTRICT）。RESTRICT は指定していない表の FK が参照する表の `TRUNCATE` を、参照元の
  行の有無・他テナントの行に関係なくカタログ宣言だけで `0A000` で拒否する（従来は行を消した後の事後検査で
  自テナントの子の行が残れば `23503`。子が空なら成功していた）。CASCADE は FK の参照元の表を推移的に
  対象へ加え、同じトランザクションで自テナントの行を削除する（参照アクションは発火しない）。公開 API は
  `ValidatedTruncate::behavior` の追加、`TruncateOutcome` の `Copy` 削除と `cascaded()` の追加、
  `TenantWriteError::TruncateReferencedByForeignKey` の追加。
- `CREATE VIEW v (a, b) AS ...`（Issue #1659・TABLE-41 (1)）: 従来 `42601` だった列別名リストを受理する。
  別名は出力列に先頭から名前を付け、短いリストの残りは導出名のまま。出力列より長いリストは `42601`、
  リスト内または最終名の重複は `42701`。別名付きビューが読む表への `RENAME COLUMN` は `0A000`、
  `DROP COLUMN`／`ALTER TYPE` は依存ありとして拒否する（保守側）。公開 API は `ViewDef.column_aliases` の追加
  （格納形式 v2。別名なしは従来と同一のバイト列）と `Storage::create_view_with_column_aliases`・
  `ValidatedCreateView::column_aliases` の追加。
- `ALTER COLUMN TYPE` の依存オブジェクト再構築（Issue #1637・TABLE-40 (3)(4)・TASK-278）:
  一般変換が、依存する索引宣言・PK／UNIQUE・CHECK・FK・DEFAULT／補完値を新しい型で再構築・再検証する
  （従来は一律 `0A000`。重複 `23505`・CHECK 違反 `23514`・FK の型照合 `42804`／参照整合性 `23503`・
  DEFAULT の変換不能 `42804`）。再検証は全テナント・全可視性の行で、拒否時は表不変・エラー文言に値を含めない。
  ビューが参照する列（`SELECT *`・連鎖を含む）は PostgreSQL と同じ `0A000` のまま。公開 API の変更なし。
- DROP の CASCADE／RESTRICT（Issue #1641・TABLE-37）: `DROP TABLE|VIEW` が `CASCADE`／`RESTRICT` を受理し、`CASCADE` は依存する FK 制約とビューを連鎖削除する。`ValidatedDropTable` に `pub behavior: DropBehavior` を追加、`DropTablesReport` に `cascaded` を追加、`DropTableOutcome`／`DropViewOutcome` から `Copy` を外し（`cascaded()` を追加）、`Storage::drop_tables` に `DropBehavior` 引数を追加した。`Storage::drop_view_with_behavior` を追加。`DROP INDEX|TYPE ... CASCADE` は `42601` のまま。
- 表の改名でビュー・台帳を追随（Issue #1625・TABLE-38）: `ALTER TABLE ... RENAME TO` が単純形ビューの
  定義を新名へ付け替え、`operation_id` 台帳・`last_op`・分割実行ジョブ記録を新名へ移す（従来は
  ビュー依存の表の改名を `0A000` で拒否し、台帳は削除していた）。評価後射影形ビューが読む表の改名は
  引き続き `0A000`。公開 API の型変更なし。
- 式索引の parser と式の決定性検査（Issue #1647・INDEX-10）: `CREATE INDEX` の列リストで
  `((<式>))` と関数呼び出し形 `(f(col))` を構文上受理し、非決定式を `42P17`、集計を `42803`、
  ウィンドウを `42P20`、部分クエリ・SRF・組み込み以外の関数を `0A000` で拒否する（従来はいずれも
  一律 `0A000`）。決定的な式も後続 Issue（#1648〜#1651）までは `0A000`（式索引は作成できない）。
  `((col))` は通常の列索引として作成できる。公開 API は `SqlSurfaceError`（`InvalidObjectDefinition`・
  `GroupingError`・`WindowingError`）・`ErrorClass`（同名 3 分類）への variant 追加と
  `ValidatedCreateIndex::expression_keys` の追加。
- `DROP TABLE a, b`（Issue #1639・TABLE-37 (1)）: 従来 `42601` だった複数テーブル指定を受理する。
  文全体を 1 つの write txn で原子的に実行し（不在・種類違い・依存のどれか 1 つでもあれば何も削除しない）、
  同じ文に並べた名前どうしの依存は数えない。`CASCADE`／`RESTRICT` は従来どおり `42601`。公開 API は
  `ValidatedDropTable.table_name: String` の `table_names: Vec<String>` への置き換えと
  `Storage::drop_tables`・`DropTablesReport` の追加。
- `ALTER COLUMN TYPE` の全行書き換え（Issue #1636・TABLE-40 (1)(2)(5)・TASK-278）:
  `USING <式>` 付きの変換と、代入変換（SQL-47）のある組の `USING` 省略変換（BIGINT→INTEGER・
  NUMERIC の位取り変更・数値↔文字列等）を受理する。従来の `0A000`（USING 付き）・`42804`（縮小・異種）
  から変わる。全テナント・全可視性の行を 1 つの write txn で書き換え、1 行でも変換に失敗
  （`22P02`／`22003`／`22001`／`23502` 等）すれば表は不変で、エラー文言に値・テナント・行 id を含めない。
  PK／UNIQUE／FK／CHECK／索引宣言／ビューが参照する列と DEFAULT を持つ列への一般変換は、Issue #1637
  で再構築に置き換えた（下記。従来の受理経路は不変）。`USING <対象列>` は省略と同じ扱い。公開 API は
  `Storage::alter_table_alter_column_type_collated` の引数追加（crate 内部）で、外部公開面の変更なし。
- `ALTER TABLE ... ALTER COLUMN ... TYPE ... USING <式>` と `SET DATA TYPE`（Issue #1635・TABLE-40）:
  従来 `42601` だった構文を受理する。USING は旧スキーマで束縛・型検査し（未定義列 `42703`、
  代入変換不可 `42804`、組み込み以外の関数 `42601`）、通ったものは行の書き換え未実装のため
  `0A000` で拒否する。USING 省略で代入変換の無い組は `42804`。公開 API は
  `ValidatedAlterTableAlterColumnType.using` の追加。
- `ALTER TABLE ... ADD COLUMN IF NOT EXISTS` と複数アクション（Issue #1633・TABLE-36・TABLE-39 (5)）:
  従来 `42601` だった構文を受理する。同名の生存列があれば何もせず成功し、`,` 区切りの列単位アクション
  （ADD／DROP COLUMN・SET／DROP DEFAULT・SET／DROP NOT NULL）は 1 つの write txn で原子的に適用する
  （途中失敗で何も適用しない）。RENAME との混在は `42601`、制約・TYPE 変更との混在は `0A000`。
  公開 API は `ValidatedAlterTableAddColumn.if_not_exists`・`ValidatedAlterTable::Multiple`・
  `AlterTableAction::{AddColumnSkipped, Multiple}` の追加（既存の網羅的 `match` はアームの追加が必要）。
- `ALTER COLUMN DROP NOT NULL`（Issue #1632・TABLE-39 (4)）: 従来 `42601` だった構文を受理する。
  主キーの構成列は `42P16`、FK 参照先側の整合違反は `42830`。公開 enum `ValidatedAlterTable` に
  `AlterColumnDropNotNull`、`AlterTableAction` に同名の variant、`CatalogError`・`SqlSurfaceError` に
  `ColumnInPrimaryKey` を追加した（既存の網羅的 `match` はアームの追加が必要）。
- `ALTER COLUMN SET／DROP DEFAULT`（Issue #1628・TABLE-39）: 従来 `42601` だった構文を受理し、
  INSERT 既定値（`default_expr`）だけを変える（既存行の補完値 `fill_value` は変えない）。公開 enum
  `ValidatedAlterTable` に `AlterColumnSetDefault`・`AlterColumnDropDefault`、`AlterTableAction` に
  同名の variant を追加した（既存の網羅的 `match` はアームの追加が必要）。以後 `ColumnDef` の
  `fill_value` と `default_expr` は ALTER で食い違いうる（食い違うスキーマはカタログ v14 で書く）。
- 列定義の分離（Issue #1627・TABLE-39 の前提）: `engine::catalog::ColumnDef` の公開フィールド
  `default` を `default_expr`（`INSERT` 既定値）へ改名し、読み出し時補完値 `fill_value` を追加する。
  `ColumnDef::with_default` は両方を同じ値にする。カタログは補完値が食い違う列を持つ場合のみ
  新形式 v14 で書く（旧バイナリは v14 を拒否する）。

- `DROP TABLE|INDEX|VIEW|TYPE IF EXISTS`（Issue #1614・TABLE-36）: 従来 `42601` だった構文を受理し、
  対象不在は副作用ゼロの no-op で成功する。公開 API は `ValidatedDropTable.if_exists` の追加と
  `Drop{Table,View,Index,Type}Outcome::skipped()` の追加（構造体リテラルでの構築は不可に）。
- `CREATE TABLE`／名前付き `CREATE INDEX` の `IF NOT EXISTS`（Issue #1613・TABLE-36）: 従来 `42601` だった
  句を受理し、同名の表・ビュー・索引があれば何もせず成功する。公開構造体 `ValidatedCreateTable` に
  公開フィールド `if_not_exists` を追加した（構造体リテラルで作る外部コードは追従が必要）。
  `CREATE VIEW`／`CREATE TYPE`／名前なし `CREATE INDEX` の `IF NOT EXISTS` は `42601` のまま。
- 配列型 OID の公告（Issue #1606・TABLE-35）: 配列列と結果型が確定する配列式の `RowDescription` の
  型 OID が `text`（25）から要素型ごとの PostgreSQL 配列型 OID（`int4[]`=1007 など）に変わる。
  `ENUM[]` は 25 のまま。配列のバイナリ形式は従来どおり `0A000`。
- `EXTRACT`／`date_part`／`date_trunc` の INTERVAL 対応と `justify_*`（Issue #1582・TABLE-34）に伴う公開 API の変更:
  公開 enum `engine::sql::udf_call::BuiltinFn` に `IntervalPart`・`IntervalTrunc`・`JustifyHours`・
  `JustifyDays`・`JustifyInterval` を追加した（既存の網羅的 `match` はアームの追加が必要）。
  従来 `42804` だった `date_part(field, interval)` などが受理に変わる。`justify_hours`／
  `justify_days`／`justify_interval` は組み込み関数名として予約され、同名の UDF 定義は `42723` になる。
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
