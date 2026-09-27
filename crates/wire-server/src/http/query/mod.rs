//! `POST /v1/query` の op 別写像の親モジュール（TASK-175。対象ビヘイビア
//! NOSQL-1〜NOSQL-10。ポインタ: `docs/spec/04-behavior/nosql-surface.md`）。
//!
//! [`schema`] が構文解析（`engine::json::parse_json`）と意味的検証
//! （必須キー欠落・未知キー・型不一致 → `42601`）の橋渡しを担う。[`filter`]
//! は `filter` 配列の `op` 語彙（`eq`／`prefix`）を
//! `engine::declarative_filter::DeclarativeFilter` へ写像し、`bind_all` で
//! `TableSchema` へ束縛する（Issue #761・TASK-175・NOSQL-7）。[`ident`] は
//! `table`／列名等の識別子形状（SQL 表層の字句解析と同じ文字集合）を検査する
//! 共有ヘルパー（Issue #768。`search`／`scan`〔#763・#766〕からも再利用する
//! 想定）。[`op`] は `op` 名を閉じた語彙 6 値（`search`／`scan`／
//! `aggregate`／`insert`／`update`／`delete`）へ分類する許可リストで、語彙外
//! は `0A000` へ写像する（Issue #759・TASK-179・NOSQL-1・NOSQL-9。
//! `update`／`delete` の追加は Issue #875・NOSQL-12。束縛・実行結線は
//! Issue #876 で結線済み——`where`（単一行 `id` 指定形）のみ実行し、
//! `filter`（述語形）は Issue #871 の実行結線待ちのため `0A000`／501 のまま）。
//! [`dml_target`] は `update`／`delete` が共有する「対象行の指定形」判定
//! （`where`／`filter` の排他判定・`where.id` の読み取り。Issue #876）。
//! [`update`] は `update` op を `engine::sql::parser::bind_update`／
//! `EngineCore::execute_bound_update_in_session` へ束縛・実行する（Issue
//! #876・TASK-186・NOSQL-6・NOSQL-12）。[`delete`] は `delete` op を
//! `engine::sql::exec::execute_delete`／`EngineCore::
//! execute_bound_delete_in_session` へ束縛・実行する（Issue #876・
//! TASK-186・NOSQL-6・NOSQL-12）。[`aggregate`] は
//! `op: aggregate`（`group_by`／`having` を含む）を
//! SQL テキストを経由せずに `engine::sql::parser::BoundAggregate` へ束縛・
//! 実行する（Issue #768・TASK-177・NOSQL-4。`explain: true` は
//! [`aggregate::explain`] が別途扱う。Issue #948）。他 op の実行計画への写像は
//! 後続 Issue（#763・#771）がここへ追加する。[`response`] は
//! `search`／`scan`／`aggregate` 成功時の `engine::sql::exec::QueryResult`
//! → JSON 応答本文（`columns`／`rows`／`row_count`）への写像を担う
//! （Issue #762・NOSQL-11）。[`scan`] は `op: "scan"` を
//! `engine::sql::parser::BoundScan` へ束縛し `EngineCore` で実行する（Issue
//! #766・TASK-176・NOSQL-3。結線済み）。[`search`] は `op: search` の JSON
//! クエリオブジェクトを SQL 表層の `bind_in_session` と同一形の
//! `engine::sql::parser::BoundStatement` へ束縛し実行する（Issue #763・
//! #764・TASK-175・NOSQL-2。`vector`／`plan` 排他・`hybrid`／`mode`／
//! `columns` の意味論は SQL 表層の既存公開関数へ委譲する）。`plan` 指定は
//! LLM 展開が engine 内部 I/O を要するため、`EngineCore::
//! execute_bound_plan_search_in_session`（TASK-186）が SQL 表層
//! `USING PLAN` と同一の fail-closed 判定順序で実行する。[`gate`] は認証済み
//! 要求（`crate::http::session::middleware::SessionPrincipal`）に対する
//! `POST /v1/query` の入口本体で、`tenant_id` 相当ヘッダの拒否・op 許可
//! リスト判定・本文のスキーマ検証を行い、`engine` 接続済みの場合に限り
//! [`aggregate::handle`]（Issue #768）へ、`op: insert` を
//! [`insert::handle`]（Issue #772）へ、`op: search` を [`search::handle`]
//! （Issue #764）へそれぞれディスパッチする（Issue #754・#759。`engine`
//! 未接続時のみ暫定の `0A000`／501 応答が残る）。
//! [`insert`] は `insert` op を
//! `engine::sql::exec::execute_insert_batch`／
//! `EngineCore::execute_bound_insert_in_session` へ写像し、成功応答
//! `{"inserted","operation_id"}`（[`insert::encode_success_body`]）を
//! `gate.rs` の `Op::Insert` アームへ結線する
//! （Issue #771・#772・TASK-178・NOSQL-6・TABLE-12・RLS-9）。
//! [`explain`] は `op: search`・`explain: true` を検索本体を実行せず
//! `plan` 指定は `EngineCore::explain_bound_plan_in_session`
//! （Issue #765・TASK-186・NOSQL-10）、`vector` 指定は `EngineCore::
//! explain_bound_search_in_session`（Issue #948・NOSQL-16・SQL-27）へ写像し、
//! SQL 表層の対応する `EXPLAIN` 文と同一内容の `{"explain":[...]}` 応答を
//! 返す（`vector`・`plan` 両方欠落は `42601`）。`scan`・`aggregate` の
//! `explain: true` も同じく Issue #948 で受理し、[`scan::handle_explain`]・
//! [`aggregate::handle_explain`] が同様の写像を行う（`group_by`／`having`
//! 付き `aggregate` も対象）。
//! [`typed_json`] は `insert`／`update`／`filter` が共有する
//! 「JSON 値 → `engine::sql::allowlist::InsertLiteral`」写像を集約し、
//! `INTEGER`／`BIGINT`／`REAL`／`DOUBLE`／`NUMERIC`／`BOOLEAN`／`DATE`／
//! `TIMESTAMP`／`UUID`／`ARRAY` を含む全列型を SQL 表層と同一の束縛経路
//! （`engine::sql::parser::bind_insert`／`bind_update`）へ到達させる
//! （Issue #896・NOSQL-17）。
//! [`ddl`] は `create_table`／`alter_table`／`drop_table` op（Issue #910・
//! NOSQL-13・TASK-207）を SQL 表層の DDL（SQL-23）と同一の実行器
//! （`EngineCore::execute_parsed_in_session`）へトークン列として到達させる
//! （`gate.rs` の `Op::CreateTable`／`Op::AlterTable`／`Op::DropTable` アーム
//! から呼ばれる）。

pub mod aggregate;
pub mod base64_std;
pub mod ddl;
pub mod delete;
pub mod dml_target;
pub mod explain;
pub mod filter;
pub mod gate;
pub mod ident;
pub mod insert;
pub mod op;
pub mod response;
pub mod scan;
pub mod schema;
pub mod search;
pub mod typed_json;
pub mod update;
