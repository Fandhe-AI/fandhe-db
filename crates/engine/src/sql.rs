//! SQL 表層モジュールの入口（TASK-74・SQL-8 参照。docs/spec/05-tasks.md・
//! docs/spec/04-behavior/sql-surface.md）。
//!
//! 責務境界: 受信 SQL テキスト（wire プロトコル経由の untrusted 入力）に対する
//! **許可リスト形式の構造検証**（[`allowlist`]）から、束縛（[`parser`]）・評価順序
//! （[`plan`]、TASK-76・SQL-7）・実行（[`exec`]、TASK-75・SQL-1〜4）までを担う。
//! `EngineCore::execute_sql`（`core.rs`。TASK-75 で追加した固有メソッド。
//! `VectorCore` trait は不変）が本モジュールの公開 API を土台に SQL 文を実行する。
//!
//! 書き込み系 SQL 文（`INSERT`）は `EngineCore::execute_insert_sql`（TASK-80、
//! 対象ビヘイビア: SQL-10）が別エントリポイントとして扱う。文末専用句
//! `USING OPERATION_ID '<id>'`（[`using_operation_id`]）は本モジュールが構造
//! パース（省略・明示 `NULL` はいずれも `None`）のみを行い、必須化の判断
//! （省略を書き込みトランザクション開始前に `23502` で拒否するか否か）は
//! サーバー構成 [`crate::recovery::required_op_id::LedgerMode`] へ移した
//! （TASK-92・RECOVER-1。`allowlist::validate_insert` が `LedgerMode::require` へ
//! 委譲する）。
//!
//! 本モジュール配下は wire プロトコル入力と同じ untrusted 入力の扱い
//! （`.claude/rules/coding-rust.md`）に従う。
//!
//! 下位モジュール:
//! - [`lexer`][]: untrusted な SQL テキストの自作トークナイザ
//! - [`allowlist`][]: 許可リスト検証本体・`SqlSurfaceError`。`HINT ORDER(...)` の構造検証も含む（TASK-76）
//! - [`parser`][]: 許可リスト通過後の束縛（列名・型照合、ベクトルリテラル解析。TASK-75）
//! - [`plan`][]: `HINT ORDER(...)` の評価順序規則（RLS は暗黙事前フィルタ＋
//!   [`crate::rls::RlsSafetyNet`]（TASK-136）による最終安全網の二重適用を維持し、
//!   `HINT` で外せない。TASK-76・SQL-7・RLS-5）
//! - [`exec`][]: 実行計画（既定 RLS→SCALAR→DISTANCE、`HINT ORDER` 指定時は [`plan`] に従う）。
//!   [`exec::execute_insert`] は TASK-186（NOSQL-6）の前提として Issue #730 で公開 API へ
//!   昇格しており、engine クレート外からも呼べる（ファイル形 [`exec::execute_file_insert`]
//!   は対象外のまま `pub(crate)`）
//! - [`explain`][]: `EXPLAIN` 応答の構築（TASK-78・SQL-6。Issue #922・SQL-27 で
//!   対象を通常検索・集計・広域取得へ拡大した。`allowlist::ExplainTarget` 参照）。
//!   `build_explain_result`・`ExplainEngine`・[`AnnPlan`]・[`ScalarPlan`]・
//!   [`classify_ann_plan`]・[`classify_scalar_plan`] は TASK-186（NOSQL-10）の
//!   前提として Issue #730 で公開 API へ昇格しており、engine クレート外からも
//!   呼べる
//! - [`mode`][]: 取得モード（`recall`／`precision`）の優先順位解決・セッション状態
//!   （TASK-161・SQL-12）
//! - [`using_operation_id`][]: `USING OPERATION_ID '<id>'` 文末句の値型・検証（TASK-80）
//! - [`ddl`][]: `CREATE TABLE`／`DROP TABLE`／`ALTER TABLE ADD COLUMN`（SQL-23・
//!   TASK-202・TASK-203、Issue #899・#900・#902）の DDL 実行権限ゲート
//!   （`require_ddl_permission`。DDL 全般が通る単一の判定点）と実行本体
//! - [`using_plan`][]: `USING PLAN('<query>')` 文末句（`ORDER BY` の代替。SQL-5）の
//!   LLM クエリ展開結果 → 既存 C4 ハイブリッド実行形への束縛（TASK-77）
//! - [`aggregate`][]: 集計関数のみを結果列とする `GROUP BY` なし単一行 SELECT の
//!   実行（TASK-166・SQL-13）。`GROUP BY` ありの複数行実行は [`group_by`] へ委譲する。
//!   `bind_aggregate`／`execute_aggregate`／`BoundAggregate` は TASK-186（NOSQL-4・
//!   NOSQL-5）で公開 API へ昇格しており engine クレート外からも呼べる（`bind_aggregate`
//!   への到達は現状 SQL テキスト経由の [`allowlist::validate_sql`] のみで、
//!   `BoundScan::new` 相当の SQL テキスト非経由の直接構築 `BoundAggregate::new`
//!   は対象外のまま）
//! - [`group_by`][]: `GROUP BY <TEXT 列>` 集計の複数行実行（TASK-167・SQL-14）。
//!   グループ表の有界化（`MAX_GROUPS`・`MAX_GROUP_KEY_TOTAL_BYTES`）・`HAVING`・
//!   `ORDER BY`・`LIMIT` を担う
//! - [`sparse_cache`][]: `exec` の hybrid 実行が参照する `SparseIndex`（BM25 語彙・
//!   統計）のテーブル世代整合キャッシュ（Issue #357）。フィルタなし hybrid クエリに
//!   限り、同一世代内の連続クエリで疎索引の再構築を償却する
//! - [`scan`][]: ランキング段（`ORDER BY`／`USING PLAN`）を持たない広域取得
//!   （ソートなしのフィルタ取得。`SELECT ... [WHERE ...] LIMIT n`。Issue #454）の
//!   実行。[`aggregate`] と同じく `VectorArena` を経由しない redb 直接走査で、
//!   `VECTOR` 列を持たないテーブルでも動作する。`bind_scan`／`execute_scan`／
//!   `BoundScan` は TASK-186（NOSQL-3）で公開 API へ昇格しており、SQL テキストを
//!   経由しない直接束縛の入口として engine クレート外からも呼べる
//! - [`statement_splitter`][]: 簡易クエリプロトコル 1 メッセージに含まれる
//!   セミコロン区切りの複数 SQL 文の分割・文種別分類（WIRE-16・TASK-219）。
//!   `wire-server::simple_query` から呼ばれる唯一の公開経路
//! - [`relation`][]: 実行計画の複数テーブル対応基盤（SQL-28・RLS-10、TASK-212、
//!   Issue #924）その 1。複数テーブル参照スコープの束縛（修飾・非修飾列の解決、
//!   `42702` の曖昧列判定）。許可リストは本 Issue では JOIN・複数 FROM を
//!   引き続き拒否するため、本番実行経路からはまだ呼ばれない（結線は Issue #925
//!   以降）
//! - [`generation_key`][]: 同基盤その 2。複数テーブルの `(table, generation)`
//!   集合 + `PolicyContext` を鍵とする汎用の世代整合キャッシュ（既存の
//!   [`arena_cache`]・[`sparse_cache`]・[`scalar_index`]・[`visible_cache`]・
//!   [`hnsw_cache`] は単一テーブル専用のまま維持し、本 Issue では移行しない）
//! - [`relation_snapshot`][]: 同基盤その 3。テーブル単位の RLS 可視スナップショット
//!   （`(tenant_id, id)` 集合）を [`generation_key`] のキャッシュへ載せ、複数
//!   テーブルを同一 read トランザクション・同一 ctx から独立に解決する
//!
//! TASK-166（対象ビヘイビア: SQL-13）: `COUNT`/`SUM`/`AVG`/`MIN`/`MAX` のみを結果列
//! とする単一テーブル SELECT（C6a）を追加した。構文は [`allowlist`]（`Statement::Aggregate`）、
//! 意味論束縛は [`parser::bind_aggregate`]、実行は [`aggregate::execute_aggregate`]
//! が担う。RLS 適用順序（デコード前のヘッダ判定 → 可視行のみ完全デコード）は
//! 既存の検索 SELECT 実行経路（[`crate::arena`]）と同一の規約に揃え、`COUNT` 等の
//! 集計値から他テナント行の存在・件数を推測できないことを維持する（RLS-7・
//! RLS-8）。オーバーフロー（`u64`/`f64`）は `SqlSurfaceError::NumericOutOfRange`
//! （ERR-2 が新設する `22003`）で fail-closed に拒否する。`GROUP BY`／`HAVING` は
//! 引き続き許可リスト外（`42601`）。
//!
//! TASK-152（対象ビヘイビア: ERR-2）: `allowlist::SqlSurfaceError` の `wire_code` 写像は
//! [`crate::error_format`]（`ErrorClass`・`ClassifiedError` trait）へ委譲する。本モジュール
//! の公開シグネチャ・返値は変更しない（詳細は `error_format.rs` モジュールドキュメント
//! 参照）。
//!
//! TASK-161（対象ビヘイビア: SQL-12）: クエリ単位の専用句 `USING MODE '<literal>'`
//! （[`allowlist`]）とセッション変数 `SET search_mode = '<literal>'`（同）を追加し、
//! 優先順位（クエリ句 > セッション変数 > 既定）の解決を [`mode::resolve_mode`] に
//! 集約した。`core.rs::EngineCore::execute_sql_in_session` が接続単位の
//! [`mode::SessionState`] を受け取って呼び出す新しい公開 API で、既存の
//! `execute_sql`（セッションなし）は空のセッションでこれへ委譲する。
//!
//! TASK-80（対象ビヘイビア: SQL-10）: `INSERT ... USING OPERATION_ID '<id>'` の
//! 許可形状を追加した。実行は [`exec::execute_insert`] が担い、行の書き込みは
//! `tenant.rs` のガード付き API（`tenant::insert_typed_row`）経由に統一する
//! （TABLE-12・RLS-9）。
//!
//! TASK-147（対象ビヘイビア: EXT-3）: `WHERE` 句に前方一致条件
//! `<col> LIKE '<prefix>%'` を追加した（[`allowlist`] が構造を、
//! `crate::declarative_filter` が意味論を検証する。`LIKE` は末尾ちょうど 1 つの
//! `%` のみを許可し、`NOT LIKE`・`ILIKE`・中間 `%`・`_`・エスケープは拒否する）。
//! 既存の等価条件 `<col> = '<literal>'`（SQL-2）と合わせ、両者は
//! `crate::declarative_filter::MetadataFilter`（汎用 API。任意の `TEXT` 列に
//! 対する宣言的フィルタ）として一本化した（**BREAKING CHANGE**: 旧
//! `sql::parser::ScalarEq`・`BoundStatement::scalar_filters` を置換。詳細は
//! `declarative_filter.rs`・`sql/parser.rs` モジュールドキュメント参照）。

//! TASK-77（対象ビヘイビア: SQL-5）: `USING PLAN('<query>')` を `ORDER BY` の代替
//! （相互排他）として追加した。構文は [`allowlist`]（`ValidatedStatement::
//! using_plan`）、展開後クエリ → 既存 C4 ハイブリッド実行形への束縛は
//! [`using_plan::bind_expansion`] が担う。LLM 展開（`core.rs::EngineCore::
//! plan_query`、TASK-110）→ 展開後テキストの再埋め込み（`Embedder`）→
//! [`using_plan::bind_expansion`] → [`exec::execute_statement`] という一意の
//! 経路へディスパッチし、`core.rs::EngineCore::execute_sql_in_session` が
//! `ValidatedStatement::using_plan` の有無で分岐する。
//!
//! Issue #869（対象ビヘイビア: SQL-19・TASK-192）: 述語つき `UPDATE ... WHERE`
//! （`lang = 'ja'` 等。単一行・id 指定形 `UPDATE`〔SQL-17・TASK-191〕とは別型）の
//! 許可リスト・束縛を追加型 API として実装した。構文は
//! [`allowlist::validate_update_form`]（[`allowlist::ValidatedUpdateForm`] を
//! 返す。既存 `validate_update` は id 指定形専用のまま無変更）、束縛は
//! [`parser::bind_update_form`]（`WHERE` 述語表現は [`parser::bind_where_predicates`]
//! を `SELECT`・集計 `SELECT`・広域取得 `SELECT` と共有し二重実装を作らない）。
//! 1 文あたりの影響行数上限は [`parser::MAX_DML_AFFECTED_ROWS`]／
//! [`parser::check_dml_affected_rows`] として用意し、実行結線（候補行の確定・
//! 一括適用・原子性）は別 Issue（#871）の担当。

pub mod aggregate;
pub mod allowlist;
pub(crate) mod arena_cache;
pub(crate) mod check_constraint;
pub mod copy;
pub(crate) mod cte;
pub mod cursor;
pub mod ddl;
pub(crate) mod ddl_column_type;
pub(crate) mod describe;
// `SELECT DISTINCT`・`COUNT(DISTINCT <expr>)`（SQL-25 (c)・TASK-209）が共有する
// 正準キー化・予算管理。`allowlist`（構文の脱糖先）と `aggregate`／`group_by`
// （`Accumulator::CountDistinct` の予算管理）の双方から参照される。
pub(crate) mod distinct;
pub mod exec;
pub mod explain;
pub(crate) mod expr_program;
pub mod generation_key;
pub mod group_by;
pub(crate) mod hnsw_cache;
pub(crate) mod hnsw_hybrid;
pub mod lexer;
pub mod mode;
pub mod params;
pub mod parser;
pub mod plan;
pub mod relation;
pub mod relation_snapshot;
pub mod returning;
pub(crate) mod scalar_index;
pub(crate) mod scalar_plan;
pub mod scan;
pub(crate) mod sparse_cache;
pub mod statement_splitter;
pub mod transaction;
pub mod udf_call;
pub(crate) mod view;
pub(crate) mod visible_cache;
pub(crate) mod where_tree;
pub(crate) mod window;

/// [`scalar_plan::ScalarShapeInput`]（`pub`）の `or_filters` フィールドの要素型を
/// 外部から名前解決可能にするための再エクスポート（TASK-208・SQL-24、
/// Issue #912）。`where_tree` モジュール自体は内部実装として `pub(crate)` の
/// まま維持する（`ScalarIndexCacheStats`・`AnnPlan` 等と同方針）。
pub use where_tree::BoundOrGroup;

/// `EngineCore::sparse_index_cache_stats`（`pub`）の戻り値型を外部から
/// 名前解決可能にするための再エクスポート。`sparse_cache` モジュール自体は
/// 内部実装として `pub(crate)` のまま維持する（codex-review 指摘対応）。
pub use scalar_index::ScalarIndexCacheStats;
pub use sparse_cache::SparseIndexCacheStats;

/// [`explain::build_explain_result`]（TASK-186・NOSQL-10 の前提。Issue #730）が
/// 要求する入力型・分類関数を外部から名前解決可能にするための再エクスポート。
/// `hnsw_cache`（Issue #408・#409・#410 の索引キャッシュ実装本体）・
/// `scalar_plan` モジュール自体は内部実装として `pub(crate)` のまま維持する
/// （`ScalarIndexCacheStats`／`SparseIndexCacheStats` と同方針）。
pub use hnsw_cache::{classify_ann_plan, AnnPlan, AnnShapeInput};
pub use scalar_plan::{classify_scalar_plan, ScalarPlan, ScalarShapeInput};
pub mod using_operation_id;

/// `allowlist::ValidatedAlterTableAddColumn::column_type`（TASK-202・SQL-23。
/// Issue #900）を外部から名前解決可能にするための再エクスポート。
/// `ddl_column_type` モジュール自体は内部実装として `pub(crate)` のまま維持する
/// （`ScalarIndexCacheStats`・`AnnPlan` 等と同方針）。`SqlOutcome::AlterTable` の
/// 中身（`AlterTableOutcome`）は公開モジュール [`ddl`] から直接参照できる
/// （`CreateTableOutcome`／`DropTableOutcome` と同じ扱い）。
pub use ddl_column_type::SqlColumnTypeName;
pub(crate) mod using_plan;

/// `EngineCore::visible_bitmap_cache_stats`（`pub`）の戻り値型を外部から
/// 名前解決可能にするための再エクスポート。`visible_cache` モジュール自体は
/// 内部実装として `pub(crate)` のまま維持する（`SparseIndexCacheStats` と同方針。
/// codex-review 指摘対応）。
pub use visible_cache::VisibleBitmapCacheStats;

/// `EngineCore::execute_sql_in_session`（TASK-161）の成功応答。`SELECT` は
/// [`exec::QueryResult`] を、`SET search_mode` は解決前の設定値
/// （[`mode::SearchMode`]）そのものを返す。TASK-79（SQL-9）で `CREATE FUNCTION` の
/// 応答として `CreateFunction` を追加した（**BREAKING CHANGE**: 既存の網羅的
/// `match` はワイルドカードアームの追加が必要）。
///
/// **TASK-78（SQL-6）で追加した破壊的変更（BREAKING CHANGE）**: `Explain`
/// variant を追加した（既存の網羅的 `match` はワイルドカードアームの追加が
/// 必要）。
///
/// **TASK-82（SQL-10）で追加した破壊的変更（BREAKING CHANGE）**: `Insert`
/// variant を追加した（既存の網羅的 `match` はワイルドカードアームの追加が
/// 必要）。
///
/// **TASK-195（SQL-22）で追加した破壊的変更（BREAKING CHANGE）**: `Truncate`
/// variant を追加した（既存の網羅的 `match` はワイルドカードアームの追加が
/// 必要）。
///
/// **TASK-191（SQL-18・#867）で追加した破壊的変更（BREAKING CHANGE）**: `Delete`
/// variant を追加した（既存の網羅的 `match` はワイルドカードアームの追加が
/// 必要）。
///
/// **Issue #873（SQL-21）で追加した破壊的変更（BREAKING CHANGE）**: `Returning`
/// variant を追加した（既存の網羅的 `match` はワイルドカードアームの追加が
/// 必要）。
///
/// **Issue #865（SQL-17・TASK-191）で追加した破壊的変更（BREAKING CHANGE）**:
/// `Update` variant を追加した（既存の網羅的 `match` はワイルドカードアームの
/// 追加が必要）。
///
/// **Issue #900（TASK-202・SQL-23）で追加した破壊的変更（BREAKING CHANGE）**:
/// `AlterTable` variant を追加した（既存の網羅的 `match` はワイルドカードアームの
/// 追加が必要）。
#[derive(Debug, Clone, PartialEq)]
pub enum SqlOutcome {
    Query(exec::QueryResult),
    SetSearchMode(mode::SearchMode),
    /// `CREATE FUNCTION <name>(...) AS <expr>`（TASK-79・SQL-9）がセッションへの
    /// 登録に成功したことを示す応答。登録された関数名を保持する。
    CreateFunction {
        name: String,
    },
    /// `EXPLAIN SELECT ... USING PLAN(...)`（TASK-78・SQL-6）の応答。検索本体は
    /// 実行せず、LLM クエリ展開・モード解決結果を可視化する `QUERY PLAN` 単一列の
    /// [`exec::QueryResult`]（`sql::explain` モジュールが構築）を返す。
    Explain(exec::QueryResult),
    /// `INSERT INTO <table> (...) VALUES (...) USING OPERATION_ID '<id>'`
    /// （TASK-82・SQL-10）がセッション経由の実行経路
    /// （[`crate::core::EngineCore::execute_sql_in_session`]）で成功したことを
    /// 示す応答。検証・実行本体は既存の
    /// [`crate::core::EngineCore::execute_insert_sql`]（TASK-80）に委譲しており、
    /// 本 variant はその [`exec::InsertOutcome`] をそのまま運ぶ薄いラッパー。
    Insert(exec::InsertOutcome),
    /// `TRUNCATE TABLE <table> USING OPERATION_ID '<id>'`（TASK-195・SQL-22）が
    /// セッション経由の実行経路で成功したことを示す応答。検証・実行本体は
    /// [`crate::core::EngineCore::execute_truncate_sql`] に委譲しており、本
    /// variant はその [`exec::TruncateOutcome`] をそのまま運ぶ薄いラッパー
    /// （`Insert` と同じ設計）。
    Truncate(exec::TruncateOutcome),
    /// `DELETE FROM <table> WHERE id = <n> USING OPERATION_ID '<id>'`
    /// （SQL-18・TASK-191・#867）がセッション経由の実行経路で成功したことを
    /// 示す応答。検証・実行本体は
    /// [`crate::core::EngineCore::execute_delete_sql`] に委譲しており、本
    /// variant はその [`exec::DeleteOutcome`] をそのまま運ぶ薄いラッパー
    /// （`Insert`・`Truncate` と同じ設計）。
    Delete(exec::DeleteOutcome),
    /// `RETURNING` 句（Issue #873・SQL-21）付きの `INSERT`／`DELETE` がセッション
    /// 経由の実行経路で成功したことを示す応答。`INSERT`／`DELETE` 単独の
    /// `Insert`／`Delete` variant とは別 variant として保持する
    /// （`RowDescription` を伴う応答形が異なるため。`wire-server::simple_query`
    /// は `result` から `RowDescription`／`DataRow`* を、`command`・
    /// `rows_affected` から `CommandComplete` タグ〔`INSERT 0 <n>`／
    /// `DELETE <n>`／`UPDATE <n>`〕を組み立てる）。検証・実行本体は
    /// [`exec::execute_insert_returning`]／[`exec::execute_delete_returning`]
    /// に委譲しており、本 variant はその [`exec::ReturningOutcome`] をそのまま
    /// 運ぶ薄いラッパー（`Insert`・`Delete` と同じ設計）。
    Returning(exec::ReturningOutcome),
    /// `UPDATE` がセッション経由の実行経路で成功したことを示す応答。単一行・
    /// `id` 完全一致形（`UPDATE <table> SET <col> = <lit>[, ...] WHERE id = <n>
    /// USING OPERATION_ID '<id>'`。SQL-17・TASK-191、Issue #865）は
    /// [`crate::core::EngineCore::execute_update_sql`] に、述語形
    /// （`UPDATE <table> SET ... WHERE ... USING OPERATION_ID '<id>'`。
    /// SQL-19・TASK-192、Issue #871）は
    /// [`crate::core::EngineCore::execute_sql_in_session`] の `UPDATE` 分岐に
    /// それぞれ委譲しており、本 variant はいずれの場合もその
    /// [`exec::UpdateOutcome`] をそのまま運ぶ薄いラッパー（`Insert`・
    /// `Truncate`・`Delete` と同じ設計）。
    ///
    /// **BREAKING CHANGE**（Issue #865）: 本 variant の追加により `SqlOutcome`
    /// を網羅的にマッチする既存コード（`crate::core::EngineCore`・
    /// `wire-server::simple_query`）はすべて更新済み。クレート外で `SqlOutcome`
    /// を網羅的にマッチするコードがあれば追随が必要。
    Update(exec::UpdateOutcome),
    /// `BEGIN [WORK|TRANSACTION]`（SQL-31・TASK-221）が成功したことを示す応答。
    /// `wire-server::simple_query` は `CommandComplete` タグ `BEGIN` を返す。
    ///
    /// **BREAKING CHANGE**（Issue #942）: 本 variant の追加により `SqlOutcome`
    /// を網羅的にマッチする既存コードは更新が必要。
    Begin,
    /// `COMMIT [WORK|TRANSACTION]`（SQL-31・TASK-221）が成功したことを示す応答。
    /// `wire-server::simple_query` は `CommandComplete` タグ `COMMIT` を返す。
    Commit,
    /// `ROLLBACK [WORK|TRANSACTION]`（SQL-31・TASK-221）が成功したことを示す応答。
    /// `wire-server::simple_query` は `CommandComplete` タグ `ROLLBACK` を返す。
    Rollback,
    /// `CREATE TABLE <table> (<col> <type>[, ...]) [;]`（SQL-23・TASK-85・
    /// TASK-202、Issue #899）がセッション経由の実行経路
    /// （`crate::core::EngineCore::execute_parsed_in_session`）で成功したことを
    /// 示す応答。DDL 実行権限ゲート（[`ddl::require_ddl_permission`]）を通過
    /// したセッションに限り到達する。本 variant はその
    /// [`ddl::CreateTableOutcome`] をそのまま運ぶ薄いラッパー（`Insert`・
    /// `Truncate`・`Delete`・`Update` と同じ設計）。
    ///
    /// **BREAKING CHANGE**（Issue #899）: 本 variant の追加により `SqlOutcome`
    /// を網羅的にマッチする既存コード（`crate::core::EngineCore`・
    /// `wire-server::simple_query`）はすべて更新済み。クレート外で `SqlOutcome`
    /// を網羅的にマッチするコードがあれば追随が必要。
    CreateTable(ddl::CreateTableOutcome),
    /// `DROP TABLE <table>`（SQL-23・TASK-203、Issue #902）がセッション経由の
    /// 実行経路（`crate::core::EngineCore::execute_parsed_in_session`）で
    /// 成功したことを示す応答。DDL 実行権限ゲート（`ddl::require_ddl_permission`）を
    /// 通過したセッションに限り到達する。本 variant はその [`ddl::DropTableOutcome`]
    /// をそのまま運ぶ薄いラッパー（`Insert`・`Truncate`・`Delete`・`Update` と
    /// 同じ設計）。
    ///
    /// **BREAKING CHANGE**（Issue #902）: 本 variant の追加により `SqlOutcome`
    /// を網羅的にマッチする既存コード（`crate::core::EngineCore`・
    /// `wire-server::simple_query`）はすべて更新済み。クレート外で `SqlOutcome`
    /// を網羅的にマッチするコードがあれば追随が必要。
    DropTable(ddl::DropTableOutcome),
    /// `DECLARE <name> CURSOR FOR <SELECT>`（WIRE-15・TASK-218）がセッション
    /// 経由の実行経路（`crate::core::EngineCore::execute_sql_in_txn`。カーソルは
    /// 明示トランザクション内でのみ有効）で成功したことを示す応答。
    /// `wire-server::simple_query` は `CommandComplete` タグ `DECLARE CURSOR`
    /// （件数を持たない固定タグ）を返す。
    ///
    /// **BREAKING CHANGE**（WIRE-15・TASK-218）: 本 variant の追加により
    /// `SqlOutcome` を網羅的にマッチする既存コード（`crate::core::EngineCore`・
    /// `wire-server::simple_query`）はすべて更新済み。クレート外で `SqlOutcome`
    /// を網羅的にマッチするコードがあれば追随が必要。
    DeclareCursor,
    /// `FETCH [FORWARD] <n> FROM <name>`（WIRE-15・TASK-218）が成功したことを
    /// 示す応答。内側 `DECLARE` 時点で確定済みの行から未取得分を払い出すのみで
    /// 検索本体は再実行しない。`wire-server::simple_query` は `RowDescription`／
    /// `DataRow`* に続けて `CommandComplete` タグ `FETCH <実際に送出した行数>`
    /// （`TagShape::Dynamic("FETCH")`）を送出する。
    Fetch(exec::QueryResult),
    /// `CLOSE <name>`（WIRE-15・TASK-218）が成功したことを示す応答。
    /// `wire-server::simple_query` は `CommandComplete` タグ `CLOSE CURSOR`
    /// （件数を持たない固定タグ）を返す。
    CloseCursor,
    /// `ALTER TABLE <table> ADD COLUMN <column> <type>`（TASK-202・SQL-23。
    /// Issue #900）がセッション経由の実行経路で成功したことを示す応答。DDL
    /// 権限ゲート（`sql::ddl::require_ddl_permission`）・実行本体
    /// （`sql::ddl::execute_alter_table_add_column`）は
    /// [`crate::core::EngineCore::execute_parsed_in_session`] の `AlterTable`
    /// 分岐に委譲しており、本 variant はその [`ddl::AlterTableOutcome`] を
    /// そのまま運ぶ薄いラッパー（`Insert`・`Truncate` と同じ設計）。
    AlterTable(ddl::AlterTableOutcome),
    /// `CREATE VIEW <name> AS <body>`（TABLE-18・SQL-23・TASK-205、Issue #909）
    /// がセッション経由の実行経路で成功したことを示す応答。`DropTable` と
    /// 同じ設計で、本 variant はその [`ddl::CreateViewOutcome`] をそのまま
    /// 運ぶ薄いラッパー。
    ///
    /// **BREAKING CHANGE**（Issue #909）: 本 variant の追加により `SqlOutcome`
    /// を網羅的にマッチする既存コードはすべて更新済み。
    CreateView(ddl::CreateViewOutcome),
    /// `DROP VIEW <name>`（TABLE-18・SQL-23・TASK-205、Issue #909）がセッション
    /// 経由の実行経路で成功したことを示す応答。
    ///
    /// **BREAKING CHANGE**（Issue #909）: 本 variant の追加により `SqlOutcome`
    /// を網羅的にマッチする既存コードはすべて更新済み。
    DropView(ddl::DropViewOutcome),
    /// `CREATE INDEX <name> ON <table> [USING hnsw] (<col>[, ...])`（TASK-206・
    /// INDEX-7・SQL-23、Issue #908）がセッション経由の実行経路で成功したことを
    /// 示す応答（[`ddl::CreateIndexOutcome`] を運ぶ薄いラッパー）。
    ///
    /// **BREAKING CHANGE**（Issue #908）: 本 variant の追加により `SqlOutcome`
    /// を網羅的にマッチする既存コードはすべて更新済み。
    CreateIndex(ddl::CreateIndexOutcome),
    /// `DROP INDEX <name>`（TASK-206・INDEX-7・SQL-23、Issue #908）がセッション
    /// 経由の実行経路で成功したことを示す応答。
    ///
    /// **BREAKING CHANGE**（Issue #908）: 本 variant の追加により `SqlOutcome`
    /// を網羅的にマッチする既存コードはすべて更新済み。
    DropIndex(ddl::DropIndexOutcome),
}
