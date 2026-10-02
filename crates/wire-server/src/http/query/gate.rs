//! `POST /v1/query` の認証後ゲート本体（Issue #754・TASK-175。対象ビヘイビア
//! HTTP-5・HTTP-6・HTTP-7・NOSQL-1〜NOSQL-10。ポインタ:
//! `docs/spec/05-tasks.md` TASK-175・`docs/spec/04-behavior/nosql-surface.md`）。
//!
//! 呼び出し文脈: [`crate::http::router::Router`] が
//! [`crate::http::session::middleware::authenticate`] を通過させた要求
//! （[`crate::http::session::middleware::SessionPrincipal`]）に対してのみ
//! [`handle`] を呼ぶ。本モジュールはヘッダ・JSON からテナントを読む経路を
//! シグネチャ上持たない（`principal` が唯一のテナント文脈の入口）。
//!
//! ## 手順（fail-closed。順序は契約の一部）
//!
//! 1. [`crate::http::session::middleware::reject_tenant_headers`]（`tenant_id`
//!    相当ヘッダの拒否。`42601`）
//! 2. 本文検証: [`crate::http::body::body_as_utf8`] → `engine::json::
//!    parse_json` → [`crate::http::query::schema::extract_op`]（本文の構文・
//!    `op` フィールドの形の検証。`42601`）
//! 3. [`crate::http::query::op::classify_op`]（`op` 名を閉じた語彙 6 値へ
//!    分類する許可リスト。DDL・UDF 呼び出し・トランザクション制御を含む
//!    語彙外はすべて `0A000`。Issue #759・TASK-179・NOSQL-1・NOSQL-9。
//!    `update`／`delete` の追加は Issue #875・NOSQL-12）
//! 4. `Op::schema().validate(...)`（必須欠落・未知キー・型不一致 →
//!    `42601`。`tenant_id` の JSON 自己申告・`HINT ORDER`／
//!    `SET search_mode` 相当フィールドはここで未知キーとして拒否される）
//! 5. `op` と `engine`（接続済み `EngineCore`。`Router::new` 経由では
//!    `None`・`Router::with_engine` 経由でのみ `Some`）の組でディスパッチ
//!    する。`(Op::Search, Some(engine))` かつ `explain: true` は
//!    [`super::explain::handle`]（`search` 本体〔`vector`・`plan` いずれも〕
//!    を実行せず `QUERY PLAN` を返す。`vector` は Issue #948・NOSQL-16・
//!    SQL-27、`plan` は TASK-186・NOSQL-10・Issue #765）、
//!    `(Op::Scan, Some(engine))` かつ `explain: true` は
//!    [`crate::http::query::scan::handle_explain`]（Issue #948）、
//!    `(Op::Aggregate, Some(engine))` かつ `explain: true` は
//!    [`super::aggregate::handle_explain`]（Issue #948）が、いずれも通常
//!    実行より必ず先に判定される。`explain` なしの `(Op::Scan, Some(engine))`
//!    は [`crate::http::query::scan::handle`]（TASK-186・NOSQL-3・
//!    Issue #766）、`(Op::Aggregate, Some(engine))` は [`super::aggregate::
//!    handle`]（Issue #768・TASK-177・NOSQL-4）、`explain` なしの
//!    `(Op::Search, Some(engine))` は [`super::search::handle`]（TASK-186・
//!    NOSQL-2・Issue #764）、
//!    `(Op::Insert, Some(engine))` は [`super::insert::handle`]（Issue
//!    #772・TASK-178・NOSQL-6）へ、`(Op::Update, Some(engine))` は
//!    [`super::update::handle`]（Issue #876・#1062・TASK-186・NOSQL-6・
//!    NOSQL-12）へ、`(Op::Delete, Some(engine))` は
//!    [`super::delete::handle`]（Issue #876・#1062・TASK-186・NOSQL-6・
//!    NOSQL-12）へそれぞれ束縛・実行を委譲する。`update`／`delete` は
//!    `where`（単一行 `id` 指定形）・`filter`（述語形。TASK-192・
//!    Issue #1062 で実行結線済み）のいずれも実行する（`filter: []` の
//!    空配列は `42601`。`super::dml_target::DmlTargetError::EmptyFilter`）。
//!    `engine` 未接続時の `Op::Scan`／`Op::Aggregate`／`Op::Insert`／
//!    `Op::Search`／`Op::Update`／`Op::Delete` はすべて
//!    [`PLACEHOLDER_MESSAGE`] へ落ちる。分割実行 DML（Issue #1130・NOSQL-12・
//!    RECOVER-11）は、`update`／`delete` の修飾（`mode`／`chunk`）が各 `handle`
//!    内部で engine の分割実行入口へ、`(Op::ShowPartitionedDml, Some(engine))`・
//!    `(Op::CancelPartitionedDml, Some(engine))` が [`super::partitioned::handle_show`]・
//!    [`super::partitioned::handle_cancel`] へ委譲する
//!
//! 手順 3（op 許可リスト）は手順 4（スキーマ検証）より前に行う。語彙外の
//! `op` にスキーマ検証由来の情報（未知キー等）が先に返ることはない
//! （fail-closed の判定順序も契約の一部）。
//!
//! 応答本文・ログにテナント ID・トークンを含めない
//! （`.claude/rules/security.md`「エラー・ログ経由で他テナントのデータ・
//! 存在情報を漏らさない」）。

use std::time::SystemTime;

use engine::core::EngineCore;
use engine::error_format::{ClassifiedError, ErrorClass};
use engine::json::parse_json;

use crate::http::query::op::{classify_op, Op};
use crate::http::query::schema::Validated;
use crate::http::query::{ddl, delete, insert, partitioned, scan, update};
use crate::http::session::middleware::{self, SessionPrincipal};
use crate::http::{body, response};

pub use crate::http::query::op::UNSUPPORTED_OP_MESSAGE;

/// 検証を通過したが実行結線が未接続の要求に返す暫定応答の文言。
/// `scan`・`aggregate`・`insert`・`search`（`explain: true` の `search` を
/// 含む）・`update`・`delete`（`where` 形・`filter` 形の両方。Issue #876・
/// #1062）は実行結線済み（Issue #766・#768・#772・#764・#765・#876・#1062）
/// のため `Router::new` 経由（`engine` 未接続）の場合にのみこの応答へ落ちる。
pub const PLACEHOLDER_MESSAGE: &str = "query execution not yet available";

/// `POST /v1/query` を処理し応答バイト列を返す（認証済み要求のみ）。
///
/// `principal` は [`crate::http::session::middleware::authenticate`] を通過
/// した要求のテナント文脈（唯一の入口）。`engine` は接続済みの
/// `EngineCore`（`Router::new` 経由では `None`。`Router::with_engine` 経由
/// でのみ `Some`）。`now_wall` は応答の `Date` ヘッダ（壁時計）に使う。
pub fn handle(
    principal: &SessionPrincipal,
    headers: &crate::http::headers::Headers<'_>,
    raw_body: &[u8],
    engine: Option<&EngineCore>,
    now_wall: SystemTime,
) -> Vec<u8> {
    // 手順 1: テナント指定ヘッダの拒否（本文検証より先）。
    if let Err(e) = middleware::reject_tenant_headers(headers) {
        return response::encode_error(e.error_class(), e.client_message(), now_wall);
    }

    // 手順 2: 本文検証（構文・`op` フィールドの形）。
    let text = match body::body_as_utf8(raw_body) {
        Ok(text) => text,
        Err(e) => return response::encode_error(e.error_class(), e.client_message(), now_wall),
    };
    let value = match parse_json(text) {
        Ok(value) => value,
        Err(e) => return response::encode_error(e.error_class(), &e.client_message(), now_wall),
    };
    let raw_op = match crate::http::query::schema::extract_op(&value) {
        Ok(raw_op) => raw_op,
        Err(e) => return response::encode_error(e.error_class(), &e.client_message(), now_wall),
    };

    // 手順 3: op 許可リスト判定（スキーマ検証より前。語彙外は 0A000）。
    let op = match classify_op(raw_op) {
        Ok(op) => op,
        Err(e) => return response::encode_error(e.error_class(), &e.client_message(), now_wall),
    };

    // 手順 4: スキーマ検証（必須欠落・未知キー・型不一致 → 42601）。
    let validated = match op.schema().validate(&value) {
        Ok(validated) => validated,
        Err(e) => return response::encode_error(e.error_class(), &e.client_message(), now_wall),
    };

    // 手順 5: op と engine 接続有無の組でディスパッチする（Issue #766・
    // #768・#772・#764・#876）。各アームは対応するモジュールへの 1 行委譲に
    // 留める。全 6 op が実行結線済みのため `(_, _)` は `engine` 未接続時
    // （`Router::new` 経由）にのみ到達する。
    match (op, engine) {
        // `explain: true` は各 op の通常実行より必ず先に判定する
        // （`search` は Issue #765・TASK-186・NOSQL-10、`scan`／`aggregate` は
        // Issue #948・NOSQL-16・SQL-27 で対象拡大）。`explain: true` が
        // 構造的に実行経路へ落ちないことを match の腕の順序自体で保証する
        // （各モジュールの `reject_explain`／`ExplainNotSupported` 拒否は
        // ゲートを迂回した場合に備える多層防御としてのみ残る）。
        (Op::Search, Some(engine)) if explain_requested(&validated) => {
            super::explain::handle(engine, principal, &validated, now_wall)
        }
        (Op::Scan, Some(engine)) if explain_requested(&validated) => {
            scan::handle_explain(engine, principal, &validated, now_wall)
        }
        (Op::Aggregate, Some(engine)) if explain_requested(&validated) => {
            super::aggregate::handle_explain(engine, principal, &validated, now_wall)
        }
        (Op::Scan, Some(engine)) => scan::handle(engine, principal, &validated, now_wall),
        (Op::Aggregate, Some(engine)) => {
            super::aggregate::handle(engine, principal, &validated, now_wall)
        }
        (Op::Insert, Some(engine)) => insert::handle(engine, principal, &validated, now_wall),
        (Op::Search, Some(engine)) => {
            super::search::handle(engine, principal, &validated, now_wall)
        }
        // `update`／`delete`（Issue #875・NOSQL-12）は Issue #876（`where`。
        // 単一行 `id` 指定形）・#1062（`filter`。述語形。TASK-192）で
        // いずれも束縛・実行結線済み。`filter: []`（空配列）のみ各モジュール
        // 内部（`super::dml_target::DmlTargetError::EmptyFilter`）で `42601`
        // へ fail-closed に落とす。
        (Op::Update, Some(engine)) => update::handle(engine, principal, &validated, now_wall),
        (Op::Delete, Some(engine)) => delete::handle(engine, principal, &validated, now_wall),
        // `create_table`／`alter_table`／`drop_table`（Issue #910・NOSQL-13・
        // TASK-207）は SQL 表層の DDL と同一の実行器（`super::ddl` モジュール
        // doc 参照）へ委譲する。DDL 実行権限（`42501`）はテナント境界とは
        // 別軸の判定で、`ddl::run_ddl` 内部の単一ゲート
        // （`engine::sql::ddl::require_ddl_permission`）が担う。
        (Op::CreateTable, Some(engine)) => {
            ddl::handle_create_table(engine, principal, &validated, now_wall)
        }
        (Op::AlterTable, Some(engine)) => {
            ddl::handle_alter_table(engine, principal, &validated, now_wall)
        }
        (Op::DropTable, Some(engine)) => {
            ddl::handle_drop_table(engine, principal, &validated, now_wall)
        }
        // 分割実行 DML の進捗照会・取り消し（Issue #1130・NOSQL-12・RECOVER-11）。
        (Op::ShowPartitionedDml, Some(engine)) => {
            partitioned::handle_show(engine, principal, &validated, now_wall)
        }
        (Op::CancelPartitionedDml, Some(engine)) => {
            partitioned::handle_cancel(engine, principal, &validated, now_wall)
        }
        // `engine` 未接続時（`Router::new` 経由）は全 op がこの暫定応答へ
        // 落ちる（実行器なしで応答を偽装しない）。
        (_, _) => response::encode_error(
            ErrorClass::FeatureNotSupported,
            PLACEHOLDER_MESSAGE,
            now_wall,
        ),
    }
}

/// `validated`（`Op::Search`／`Op::Scan`／`Op::Aggregate` いずれかのスキーマ
/// 検証済み要求）が `explain: true` を伴うかを判定する（`search` は Issue
/// #765・TASK-186・NOSQL-10、`scan`／`aggregate` は Issue #948・NOSQL-16・
/// SQL-27）。op に依存しない判定のため 3 op のいずれの呼び出し元
/// （[`handle`] の match アーム）からも共有される。`optional_bool` の
/// `Err`（多層防御の域。各 op のスキーマ検証を既に通過しているため通常
/// 到達しない）は `false` 扱いにせず、後段のスキーマ検証と同じ判定経路
/// （`(_, _)` アーム）へ委ねる意図で `matches!` により厳密に
/// `Ok(Some(true))` のみを真とする。
fn explain_requested(validated: &Validated<'_>) -> bool {
    matches!(validated.optional_bool("explain"), Ok(Some(true)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::headers::{parse_headers, HeaderParse};
    use crate::http::session::store::SessionStore;
    use engine::policy::PolicyContext;
    use std::time::Instant;

    fn leak(v: Vec<u8>) -> &'static [u8] {
        Box::leak(v.into_boxed_slice())
    }

    /// テストごとに一意な一時 DB ファイルパスを払い出す（`EngineCore::open`
    /// に渡す前提。ファイル自体は作成しない）。`gate.rs` は `mod gate;` 経由
    /// で読み込まれる非ルートファイルのため、`#[path]` によるエンジン側
    /// `test_util/temp_db.rs` の取り込みは、ネストしたモジュール宣言に
    /// 対応する仮想ディレクトリ（`query/gate/`・`query/gate/tests/`）が
    /// 実ファイルシステム上に存在せず `..` を辿れない（OS のパス解決は
    /// 中間コンポーネントの実在を要求する）ため使えない。本ヘルパーは
    /// その代わりに最小限の一意名生成のみをその場で行う（`std` のみに依存。
    /// dependency-policy 準拠）。
    fn unique_temp_db_path(label: &str) -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "vector-db-wire-server-query-gate-{label}-{}-{seq}.redb",
            std::process::id()
        ));
        path
    }

    /// [`unique_temp_db_path`] で払い出したパスのファイルを、値が drop
    /// されるタイミングで削除する RAII ガード（`engine::test_util::temp_db::
    /// CleanupGuard` と同じ意図の最小版）。
    struct CleanupGuard(std::path::PathBuf);

    impl Drop for CleanupGuard {
        fn drop(&mut self) {
            // 削除失敗はテストを失敗させず（Drop 内 panic は二重 panic の恐れがある）、
            // パスとエラーだけを stderr へ出す（Issue #1303）。
            if let Err(e) = std::fs::remove_file(&self.0) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    eprintln!(
                        "warning: failed to remove temp entry {}: {e}",
                        self.0.display()
                    );
                }
            }
        }
    }

    /// テーブルを一切作らないスローアウェイ `EngineCore`。`table: "docs"` への
    /// `scan` はスキーマ取得の時点で `SqlSurfaceError::UndefinedTable`
    /// （`42P01`／404）となり、「認証 → op 許可リスト → スキーマ検証 →
    /// engine 呼び出し」がすべて走ったことの非 vacuous な証跡になる
    /// （`http_common::assert_reached_query_gate` と同じ判断）。
    fn empty_core() -> (EngineCore, CleanupGuard) {
        let path = unique_temp_db_path("query-gate");
        let guard = CleanupGuard(path.clone());
        let core = EngineCore::open(&path).expect("open throwaway engine core");
        (core, guard)
    }

    /// `docs`（`id`／`lang` の 2 列。`VECTOR` 列なし）を作成済みの
    /// `EngineCore`（Issue #948。`explain: true` の scan／aggregate が
    /// `42P01` ではなく `QUERY PLAN` 応答を返すことを確認するために、
    /// 既知テーブルが必要な回帰テストが使う）。
    fn core_with_docs_table() -> (EngineCore, CleanupGuard) {
        let path = unique_temp_db_path("query-gate-docs-table");
        let guard = CleanupGuard(path.clone());
        {
            let storage = engine::storage::Storage::open(&path).expect("open storage");
            storage
                .create_table(&engine::catalog::TableSchema::new(
                    "docs",
                    vec![engine::catalog::ColumnDef::new(
                        "lang",
                        engine::catalog::ColumnType::Text,
                        false,
                    )],
                ))
                .expect("create docs table");
        }
        let core = EngineCore::open(&path).expect("open engine core");
        (core, guard)
    }

    fn headers_with_body(body: &[u8], extra: &[&str]) -> crate::http::headers::Headers<'static> {
        let mut input = Vec::new();
        for line in extra {
            input.extend_from_slice(line.as_bytes());
        }
        input.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
        match parse_headers(leak(input)).expect("header parse should succeed") {
            HeaderParse::Complete { headers, .. } => headers,
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    fn principal() -> SessionPrincipal {
        let sessions = SessionStore::new();
        let now = Instant::now();
        let ctx = PolicyContext::new("tenant-a").expect("valid ctx");
        let token = sessions.issue(ctx, now).expect("issue");
        let raw = format!("Authorization: Bearer {}\r\n", token.encoded());
        let mut input = raw.into_bytes();
        input.extend_from_slice(b"Content-Length: 0\r\n\r\n");
        let headers = match parse_headers(leak(input)).expect("header parse") {
            HeaderParse::Complete { headers, .. } => headers,
            other => panic!("expected Complete, got {other:?}"),
        };
        middleware::authenticate(&sessions, &headers, move || now).expect("authenticate")
    }

    /// `engine` 未接続（`Router::new` 経由）の従来経路。全 op がプレース
    /// ホルダー応答へ落ちることを検証する既存テスト群が使う。
    fn run(body: &[u8], extra_headers: &[&str]) -> Vec<u8> {
        let p = principal();
        let headers = headers_with_body(body, extra_headers);
        handle(&p, &headers, body, None, std::time::SystemTime::now())
    }

    /// `engine` 接続済み（`Router::with_engine` 相当）の経路。`scan` op の
    /// 実行結線（TASK-186・NOSQL-3・Issue #766）を検証するテストが使う。
    fn run_with_engine(core: &EngineCore, body: &[u8], extra_headers: &[&str]) -> Vec<u8> {
        let p = principal();
        let headers = headers_with_body(body, extra_headers);
        handle(&p, &headers, body, Some(core), std::time::SystemTime::now())
    }

    #[test]
    fn valid_scan_is_dispatched_to_the_engine_and_reports_undefined_table() {
        // `scan` は TASK-186・NOSQL-3 で実行結線済みのため、`engine` 接続済み
        // であれば他 op のような暫定 placeholder（`0A000`／501）はもう
        // 返らない。存在しないテーブルへの `scan` が `42P01`／404（SQL 経路と
        // 同一分類）になることで、「認証 → op 許可リスト → スキーマ検証 →
        // engine 呼び出し」がすべて走ったことを非 vacuous に確認する。
        let (core, _guard) = empty_core();
        let body = br#"{"op":"scan","table":"docs","limit":1}"#;
        let response = run_with_engine(&core, body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 404 "), "got: {text}");
        assert!(text.contains("42P01"), "got: {text}");
        assert!(!text.contains(PLACEHOLDER_MESSAGE), "got: {text}");
    }

    #[test]
    fn valid_scan_reaches_placeholder_response_when_engine_is_not_connected() {
        // `engine` 未接続（`Router::new` 経由）では `scan` も従来どおり
        // placeholder のまま（実行器なしで応答を偽装しない。
        // `router.rs::Router::new` の既定・`nosql9_op_allowlist.rs` と
        // 同じ契約）。
        let body = br#"{"op":"scan","table":"docs","limit":1}"#;
        let response = run(body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 501 "), "got: {text}");
        assert!(text.contains("0A000"), "got: {text}");
        assert!(text.contains(PLACEHOLDER_MESSAGE), "got: {text}");
    }

    #[test]
    fn valid_search_reaches_placeholder_response_when_engine_is_not_connected() {
        // `engine` 未接続（`Router::new` 経由）では `search` も従来どおり
        // placeholder のまま（実行器なしで応答を偽装しない。`scan`・
        // `aggregate` と同じ契約）。
        let body = br#"{"op":"search","table":"docs","limit":1}"#;
        let response = run(body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 501 "), "got: {text}");
    }

    #[test]
    fn valid_aggregate_reaches_placeholder_response_when_engine_is_not_connected() {
        // `engine` 未接続（`Router::new` 経由）では `aggregate` も従来どおり
        // placeholder のまま（実行器なしで応答を偽装しない。
        // `router.rs::Router::new` の既定・`nosql9_op_allowlist.rs` と
        // 同じ契約）。
        let body =
            br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}]}"#;
        let response = run(body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 501 "), "got: {text}");
        assert!(text.contains(PLACEHOLDER_MESSAGE), "got: {text}");
    }

    #[test]
    fn valid_insert_reaches_placeholder_response() {
        let body = br#"{"op":"insert","table":"docs","rows":[]}"#;
        let response = run(body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 501 "), "got: {text}");
    }

    #[test]
    fn valid_insert_is_dispatched_to_the_engine_and_reports_undefined_table() {
        // `insert` は Issue #772 で実行結線済みのため、`engine` 接続済み
        // であれば `scan`／`aggregate` と同様もう暫定 placeholder
        // （`0A000`／501）を返さない。存在しないテーブルへの `insert` が
        // `42P01`／404（SQL 経路と同一分類）になることで、「認証 → op
        // 許可リスト → スキーマ検証 → engine 呼び出し」がすべて走った
        // ことを非 vacuous に確認する（`valid_scan_is_dispatched_to_...`
        // と同型）。
        let (core, _guard) = empty_core();
        let body = br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[1,0,0]}],"operation_id":"op-gate-1"}"#;
        let response = run_with_engine(&core, body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 404 "), "got: {text}");
        assert!(text.contains("42P01"), "got: {text}");
        assert!(!text.contains(PLACEHOLDER_MESSAGE), "got: {text}");
    }

    #[test]
    fn valid_search_is_dispatched_to_the_engine_and_reports_undefined_table() {
        // `search` は TASK-186・NOSQL-2（Issue #764）で実行結線済みのため、
        // `engine` 接続済みであれば `scan`／`aggregate`／`insert` と同様もう
        // 暫定 placeholder（`0A000`／501）を返さない。存在しないテーブルへの
        // `search` が `42P01`／404（SQL 経路と同一分類）になることで、
        // 「認証 → op 許可リスト → スキーマ検証 → engine 呼び出し」が
        // すべて走ったことを非 vacuous に確認する
        // （`valid_scan_is_dispatched_to_...` と同型）。
        let (core, _guard) = empty_core();
        let body = br#"{"op":"search","table":"docs","limit":1}"#;
        let response = run_with_engine(&core, body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 404 "), "got: {text}");
        assert!(text.contains("42P01"), "got: {text}");
        assert!(!text.contains(PLACEHOLDER_MESSAGE), "got: {text}");
    }

    #[test]
    fn scan_and_aggregate_explain_true_are_dispatched_to_the_explain_arm_not_the_execute_arm() {
        // Issue #948（NOSQL-16・SQL-27）: `explain: true` を伴う `scan`／
        // `aggregate` は `(Op::Scan, Some(engine)) if explain_requested(...)`／
        // `(Op::Aggregate, Some(engine)) if explain_requested(...)` の腕へ
        // 振り分けられ、後続の実行アーム（`scan::handle`／
        // `aggregate::handle`）へは落ちない。両アームは match の腕の順序上
        // 実行アームより先に置かれている必要がある——本テストは腕の順序が
        // 入れ替わった場合に検出する回帰確認（存在しないテーブルへの要求が
        // `QUERY PLAN` 応答〔`200`〕になることで、実行アームの `42P01` では
        // なく `explain` アームへ到達したことを非 vacuous に確認する。
        // `EXPLAIN` はテーブルのスキーマ解決自体は行うため未知テーブルは
        // 依然 `42P01` になる点に注意し、既存テーブルへの要求で判定する）。
        let (core, _guard) = core_with_docs_table();
        let scan_body = br#"{"op":"scan","table":"docs","limit":1,"explain":true}"#;
        let response = run_with_engine(&core, scan_body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 200 "), "got: {text}");
        assert!(text.contains("\"explain\""), "got: {text}");
        assert!(!text.contains(PLACEHOLDER_MESSAGE), "got: {text}");

        let aggregate_body = br#"{"op":"aggregate","table":"docs",
            "aggregates":[{"fn":"count","column":"*"}],"explain":true}"#;
        let response = run_with_engine(&core, aggregate_body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 200 "), "got: {text}");
        assert!(text.contains("\"explain\""), "got: {text}");
        assert!(!text.contains(PLACEHOLDER_MESSAGE), "got: {text}");
    }

    #[test]
    fn valid_update_and_delete_where_form_are_dispatched_to_the_engine_and_report_undefined_table()
    {
        // `update`／`delete`（Issue #875・NOSQL-12）は Issue #876 で束縛・
        // 実行結線済みのため、`engine` 接続済みであれば `scan`／`insert`／
        // `search` と同様もう暫定 placeholder（`0A000`／501）を返さない。
        // 存在しないテーブルへの要求が `42P01`／404（SQL 経路と同一分類）に
        // なることで、「認証 → op 許可リスト → スキーマ検証 → engine 呼び
        // 出し」がすべて走ったことを非 vacuous に確認する。
        let (core, _guard) = empty_core();
        let cases: [&[u8]; 2] = [
            br#"{"op":"update","table":"docs","set":{"lang":"en"},"where":{"id":1},"operation_id":"gate-update-1"}"#,
            br#"{"op":"delete","table":"docs","where":{"id":1},"operation_id":"gate-delete-1"}"#,
        ];
        for body in cases {
            let response = run_with_engine(&core, body, &[]);
            let text = String::from_utf8(response).expect("utf-8 response");
            assert!(text.starts_with("HTTP/1.1 404 "), "got: {text}");
            assert!(text.contains("42P01"), "got: {text}");
            assert!(!text.contains(PLACEHOLDER_MESSAGE), "got: {text}");
        }
    }

    #[test]
    fn empty_update_and_delete_filter_form_reject_with_42601_without_reaching_engine() {
        // `filter: []`（空配列）は SQL 表層の `WHERE` 句省略とのパリティで
        // `42601`（Issue #1062）。engine を一切呼ばないため、テーブル不存在
        // （`42P01`）にはならない。
        let (core, _guard) = empty_core();
        let cases: [&[u8]; 2] = [
            br#"{"op":"update","table":"docs","set":{"lang":"en"},"filter":[]}"#,
            br#"{"op":"delete","table":"docs","filter":[]}"#,
        ];
        for body in cases {
            let response = run_with_engine(&core, body, &[]);
            let text = String::from_utf8(response).expect("utf-8 response");
            assert!(text.starts_with("HTTP/1.1 400 "), "got: {text}");
            assert!(text.contains("42601"), "got: {text}");
            assert!(!text.contains("42P01"), "got: {text}");
        }
    }

    #[test]
    fn non_empty_update_and_delete_filter_form_reach_the_engine_and_report_undefined_table() {
        // 述語形（`filter` 非空。TASK-186・NOSQL-12、Issue #1062 で実行結線
        // 済み）は `engine` 接続済みであれば実行され、存在しないテーブルへの
        // 要求は単一行形・`scan` と同じ `42P01`／404 になる（実行器へ到達した
        // ことの非 vacuous な証跡）。
        let (core, _guard) = empty_core();
        let cases: [&[u8]; 2] = [
            br#"{"op":"update","table":"docs","set":{"lang":"en"},"filter":[{"column":"lang","op":"eq","value":"ja"}],"operation_id":"gate-pred-update-1"}"#,
            br#"{"op":"delete","table":"docs","filter":[{"column":"lang","op":"eq","value":"ja"}],"operation_id":"gate-pred-delete-1"}"#,
        ];
        for body in cases {
            let response = run_with_engine(&core, body, &[]);
            let text = String::from_utf8(response).expect("utf-8 response");
            assert!(text.starts_with("HTTP/1.1 404 "), "got: {text}");
            assert!(text.contains("42P01"), "got: {text}");
            assert!(!text.contains(PLACEHOLDER_MESSAGE), "got: {text}");
        }
    }

    #[test]
    fn valid_update_and_delete_reach_placeholder_when_engine_is_not_connected() {
        // `engine` 未接続（`Router::new` 経由）では `update`／`delete` も
        // 従来どおり placeholder のまま（実行器なしで応答を偽装しない）。
        let cases: [&[u8]; 2] = [
            br#"{"op":"update","table":"docs","set":{"lang":"en"},"where":{"id":1}}"#,
            br#"{"op":"delete","table":"docs","where":{"id":1}}"#,
        ];
        for body in cases {
            let response = run(body, &[]);
            let text = String::from_utf8(response).expect("utf-8 response");
            assert!(text.starts_with("HTTP/1.1 501 "), "got: {text}");
            assert!(text.contains("0A000"), "got: {text}");
            assert!(text.contains(PLACEHOLDER_MESSAGE), "got: {text}");
        }
    }

    #[test]
    fn update_missing_set_rejects_with_42601() {
        let body = br#"{"op":"update","table":"docs","where":{"id":1}}"#;
        let response = run(body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 400 "), "got: {text}");
        assert!(text.contains("42601"), "got: {text}");
    }

    #[test]
    fn update_with_hint_order_rejects_with_42601_not_0a000() {
        // op 許可リストを通過した（`0A000` ではない）うえでスキーマ検証段の
        // 未知キーとして `42601` が返ることを固定する（手順の順序が契約で
        // あることの `update` 版回帰確認。`op_allowlist_check_precedes_
        // schema_validation` と対をなす）。
        let body = br#"{"op":"update","table":"docs","set":{"lang":"en"},"hint_order":["path"]}"#;
        let response = run(body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 400 "), "got: {text}");
        assert!(text.contains("42601"), "got: {text}");
        assert!(!text.contains("0A000"), "got: {text}");
    }

    #[test]
    fn nested_where_unknown_key_rejects_with_42601() {
        let body = br#"{"op":"delete","table":"docs","where":{"id":1,"extra":1}}"#;
        let response = run(body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 400 "), "got: {text}");
        assert!(text.contains("42601"), "got: {text}");
    }

    #[test]
    fn tenant_id_in_json_is_rejected_for_all_eleven_ops() {
        let cases: [&[u8]; 11] = [
            br#"{"op":"search","table":"docs","limit":1,"tenant_id":"evil"}"#,
            br#"{"op":"scan","table":"docs","limit":1,"tenant_id":"evil"}"#,
            br#"{"op":"aggregate","table":"docs","aggregates":[],"tenant_id":"evil"}"#,
            br#"{"op":"insert","table":"docs","rows":[],"tenant_id":"evil"}"#,
            br#"{"op":"update","table":"docs","set":{"lang":"en"},"tenant_id":"evil"}"#,
            br#"{"op":"delete","table":"docs","where":{"id":1},"tenant_id":"evil"}"#,
            br#"{"op":"create_table","table":"docs","columns":[{"name":"a","type":"text"}],"tenant_id":"evil"}"#,
            br#"{"op":"alter_table","table":"docs","add_column":{"name":"a","type":"text"},"tenant_id":"evil"}"#,
            br#"{"op":"drop_table","table":"docs","tenant_id":"evil"}"#,
            br#"{"op":"show_partitioned_dml","table":"docs","operation_id":"x","tenant_id":"evil"}"#,
            br#"{"op":"cancel_partitioned_dml","table":"docs","operation_id":"x","tenant_id":"evil"}"#,
        ];
        for body in cases {
            let response = run(body, &[]);
            let text = String::from_utf8(response).expect("utf-8 response");
            assert!(text.starts_with("HTTP/1.1 400 "), "got: {text}");
            assert!(text.contains("42601"), "got: {text}");
        }
    }

    #[test]
    fn unsupported_op_rejects_with_0a000() {
        let body = br#"{"op":"select","table":"docs"}"#;
        let response = run(body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 501 "), "got: {text}");
        assert!(text.contains("0A000"), "got: {text}");
        assert!(text.contains(UNSUPPORTED_OP_MESSAGE), "got: {text}");
    }

    #[test]
    fn ddl_udf_transaction_ops_reject_with_0a000() {
        // UDF 呼び出し・トランザクション制御・NOSQL-13 対象外の DDL 相当
        // （index／view 系）・表記揺れは、いずれも許可リスト（Op::parse の
        // 11 値）に無いため 0A000 に落ちる（拒否リストを別途持たない設計の
        // 回帰確認。`update`／`delete` は Issue #875、`create_table`／
        // `alter_table`／`drop_table` は Issue #910 で語彙へ加わったため
        // 本テストの対象から除外した）。
        let ops = [
            "create_index",
            "drop_index",
            "create_view",
            "drop_view",
            "call",
            "udf",
            "begin",
            "commit",
            "rollback",
            "explain",
            "set",
            "SEARCH",
            " search",
            "",
        ];
        for op in ops {
            let body = format!(r#"{{"op":"{op}","table":"docs"}}"#);
            let response = run(body.as_bytes(), &[]);
            let text = String::from_utf8(response).expect("utf-8 response");
            assert!(text.starts_with("HTTP/1.1 501 "), "op {op:?} got: {text}");
            assert!(text.contains("0A000"), "op {op:?} got: {text}");
            assert!(
                text.contains(UNSUPPORTED_OP_MESSAGE),
                "op {op:?} got: {text}"
            );
        }
    }

    #[test]
    fn op_allowlist_check_precedes_schema_validation() {
        // 語彙外 op に未知キー（本来ならスキーマ検証で 42601）が同時に
        // 付与されていても、op 許可リスト判定（0A000）が先に効く
        // （手順の順序が契約であることの回帰確認。`drop_table` は Issue #910
        // で語彙に加わったため、引き続き語彙外の `drop_index` を使う）。
        let body = br#"{"op":"drop_index","table":"docs","hint_order":["path"]}"#;
        let response = run(body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 501 "), "got: {text}");
        assert!(text.contains("0A000"), "got: {text}");
        assert!(text.contains(UNSUPPORTED_OP_MESSAGE), "got: {text}");
        assert!(!text.contains("42601"), "got: {text}");
    }

    #[test]
    fn missing_op_rejects_with_42601() {
        let body = br#"{"table":"docs"}"#;
        let response = run(body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 400 "), "got: {text}");
        assert!(text.contains("42601"), "got: {text}");
    }

    #[test]
    fn non_object_body_rejects_with_42601() {
        let body = br#"[]"#;
        let response = run(body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 400 "), "got: {text}");
        assert!(text.contains("42601"), "got: {text}");
    }

    #[test]
    fn invalid_json_rejects_with_42601() {
        let body = b"not json";
        let response = run(body, &[]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 400 "), "got: {text}");
        assert!(text.contains("42601"), "got: {text}");
    }

    #[test]
    fn non_utf8_body_rejects_with_42601() {
        let body: &[u8] = &[0xff, 0xfe, 0xfd];
        let response = run(body, &[]);
        let text = String::from_utf8_lossy(&response);
        assert!(text.starts_with("HTTP/1.1 400 "), "got: {text}");
        assert!(text.contains("42601"), "got: {text}");
    }

    #[test]
    fn tenant_header_rejects_before_body_validation() {
        // 本文が不正でも tenant ヘッダの拒否文言が先に返る（順序: ヘッダ検査
        // が本文検証より前）。
        let body = b"not json at all";
        let response = run(body, &["X-Tenant-Id: other\r\n"]);
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 400 "), "got: {text}");
        assert!(text.contains("42601"), "got: {text}");
        assert!(
            text.contains("tenant context is derived from the session"),
            "got: {text}"
        );
    }

    #[test]
    fn response_does_not_leak_tenant_id() {
        let body = br#"{"op":"scan","table":"docs","limit":1}"#;
        let response = run(body, &[]);
        let text = String::from_utf8_lossy(&response);
        assert!(!text.contains("tenant-a"));
    }
}
