//! NoSQL 表層の production ルータ（Issue #752・#753・#754・#758 の範囲。
//! TASK-171・TASK-179／HTTP-1・HTTP-2・HTTP-8。ポインタ:
//! `docs/spec/05-tasks.md` TASK-171・TASK-179・
//! `docs/spec/04-behavior/http-transport.md` HTTP-1, HTTP-2, HTTP-6, HTTP-8・
//! `docs/spec/04-behavior/nosql-surface.md` NOSQL-1）。
//!
//! [`crate::http::conn::handle_connection_with`]（Issue #747）へ注入する
//! [`crate::http::conn::RequestHandler`] 実装。[`resolve_target`] が
//! `target` を閉じた語彙 [`Route`] へ**バイト厳密一致のみ**で分類し
//! ディスパッチする（大文字小文字の畳み込み・パーセントデコード・
//! 正規化は行わない。`/V1/QUERY`・`//v1/query`・`/v1/query/`・
//! `/v1/session?x=1` 等は等しく [`Route::Unknown`] になる）。
//! `/v1/session`・`/v1/session/close`・`/v1/query` 以外はすべて
//! [`crate::http::conn::unknown_target_response`] により
//! [`crate::http::conn::PlaceholderRouter`] と同一バイト列（`08P01`）で
//! 拒否する。
//!
//! # 判定順序（契約の一部）
//!
//! 1. メソッド・要求行・ヘッダ・本文長・`Content-Type` の検証は
//!    接続ハンドラ（[`crate::http::conn`]）で完了済み（`08P01`／`54000`）。
//! 2. 本ルータ: [`resolve_target`] によるパス分類（**認証より前**。
//!    未知パスは `Authorization` ヘッダの有無・本文内容にかかわらず
//!    `08P01`。セッション枠を消費しない）。
//! 3. [`Endpoint::Query`] のみ
//!    [`crate::http::session::middleware::authenticate`]（Bearer 検証。
//!    失敗は `28000`）→ [`crate::http::query::gate::handle`]（Issue #754・
//!    HTTP-5・HTTP-6・HTTP-7）。
//!
//! # 優先規則（`/v1/query` 配下の tenant マーカーと未知パスの衝突）
//!
//! `/v1/query` 接頭辞配下にクライアント自己申告の `tenant_id` 相当が
//! 現れた場合（[`Route::QueryTenantMarker`]。Issue #754・HTTP-7）は、
//! 汎用の未知ターゲット `08P01`（HTTP-2・NOSQL-1）より**優先**して
//! `42601` を返す。両者の優先順位は本リポの実装上の判断であり、
//! spec 側での明文化は申し送り事項とする（Issue #758 実装記録参照）。
//!
//! op 許可リストの正式化（Issue #759）は完了済み。`search`（TASK-186・
//! NOSQL-2・Issue #764）・`scan`（TASK-186・NOSQL-3・Issue #766）・
//! `aggregate`（Issue #768）・`insert`（Issue #771・#772）の 4 op すべての
//! 束縛・実行は [`crate::http::query::gate::handle`] へ `engine` を渡すことで
//! 結線済み（`engine` 未接続の [`Router::new`] 経由時のみ全 op が暫定
//! `0A000`／501 を返す）。
//!
//! メソッド（`POST` 以外を拒否）は [`crate::http::conn`] が要求行パース時点で
//! 既に絞り込み済み（[`crate::http::request::Method`] は `Post` の 1 variant
//! のみを持つ閉じた語彙）だが、`Router::handle` 冒頭でも
//! `Method` の網羅 match により再検査する（実行時コストなし。将来
//! `Method` に variant が増えた場合にコンパイルエラーで気付ける
//! defense in depth）。

use std::time::{Instant, SystemTime};

use std::sync::Arc;

use crate::auth::UserStore;
use crate::http::conn::{unknown_target_response, Request, RequestHandler};
use crate::http::query::gate as query_gate;
use crate::http::request::Method;
use crate::http::response;
use crate::http::session::close as session_close;
use crate::http::session::issue as session_issue;
use crate::http::session::middleware;
use crate::http::session::store::SessionStore;
use engine::core::EngineCore;
use engine::error_format::ErrorClass;

/// `/v1/session` の要求ターゲット（バイト厳密一致のみ受理。クエリ文字列付き・
/// 末尾スラッシュ違いは不一致として [`Route::Unknown`]／`08P01` へ落ちる）。
const SESSION_TARGET: &str = "/v1/session";

/// `/v1/session/close` の要求ターゲット（[`SESSION_TARGET`] と同じ厳密一致
/// 方針）。
const SESSION_CLOSE_TARGET: &str = "/v1/session/close";

/// `/v1/query` の要求ターゲット（[`SESSION_TARGET`] と同じ厳密一致方針）。
const QUERY_TARGET: &str = "/v1/query";

/// クライアント自己申告の `tenant_id` 相当がパス上に現れた場合の固定文言
/// （ヘッダの [`crate::http::session::middleware::TENANT_HEADER_MESSAGE`] と
/// 同じ設計意図。パス断片を echo しない）。
const QUERY_TARGET_TENANT_MARKER_MESSAGE: &str =
    "tenant context is derived from the session; per-request tenant path segments are not accepted";

/// 受理するエンドポイントの閉じた語彙（NOSQL-1）。バイト厳密一致した
/// ターゲットのみがここへ写像される。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Endpoint {
    Session,
    SessionClose,
    Query,
}

/// [`resolve_target`] の分類結果。`Router::handle` はこれを唯一の情報源
/// としてディスパッチする。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Endpoint(Endpoint),
    /// `/v1/query` 配下のパス・クエリ文字列に tenant_id 相当を含む
    /// （Issue #754。汎用の [`Route::Unknown`] より優先して `42601` を返す。
    /// モジュール doc「優先規則」節参照）。
    QueryTenantMarker,
    /// 上記以外すべて（`08P01`。応答バイト列は
    /// [`crate::http::conn::PlaceholderRouter`] と同一）。
    Unknown,
}

/// `target` を [`Route`] へバイト厳密一致で分類する純関数（Issue #752・
/// #754・#758 の判定を単一情報源へ統合）。
///
/// - 3 エンドポイント（[`SESSION_TARGET`]・[`SESSION_CLOSE_TARGET`]・
///   [`QUERY_TARGET`]）に厳密一致 → [`Route::Endpoint`]
/// - `/v1/query?...`／`/v1/query/...` の残余部分（ASCII 小文字化・`_`→`-`
///   正規化後）に `tenant-id` または `tenantid` を部分文字列として含む
///   → [`Route::QueryTenantMarker`]（`42601`。パス上の `tenant_id` は
///   要求自身の構文違反でありサーバー状態・テナント存在情報を含まないため、
///   認証前に判定してよい）
/// - それ以外（完全未知パス・クエリ文字列付き・末尾スラッシュ／余剰
///   セグメント・大文字小文字違い等）→ [`Route::Unknown`]（`08P01`）
fn resolve_target(target: &str) -> Route {
    if target == SESSION_TARGET {
        return Route::Endpoint(Endpoint::Session);
    }
    if target == SESSION_CLOSE_TARGET {
        return Route::Endpoint(Endpoint::SessionClose);
    }
    if target == QUERY_TARGET {
        return Route::Endpoint(Endpoint::Query);
    }
    let rest = target
        .strip_prefix("/v1/query?")
        .or_else(|| target.strip_prefix("/v1/query/"));
    if let Some(rest) = rest {
        let normalized: String = rest
            .chars()
            .map(|c| {
                if c == '_' {
                    '-'
                } else {
                    c.to_ascii_lowercase()
                }
            })
            .collect();
        if normalized.contains("tenant-id") || normalized.contains("tenantid") {
            return Route::QueryTenantMarker;
        }
    }
    Route::Unknown
}

/// production 入口のルータ。`users`（ユーザーストアの共有ハンドル）・
/// `sessions`（[`SessionStore`]。`Clone` で内部状態を共有する型のため、
/// `Router` 自身は `Arc` で包まず値として保持する）を束ねる。`engine` は
/// `/v1/query` の `search`（TASK-186・NOSQL-2・Issue #764）・`scan`
/// （TASK-186・NOSQL-3・Issue #766）・`aggregate`（Issue #768）・`insert`
/// （Issue #771・#772）の 4 op すべてを実行するための接続済み `EngineCore`
/// （`Router::new` 経由では `None` のまま。実行器なしで応答を偽装しない
/// fail-closed 設計）。
pub struct Router {
    users: Arc<UserStore>,
    sessions: SessionStore,
    engine: Option<Arc<EngineCore>>,
}

impl Router {
    /// `engine` 未接続の構築経路。既存呼び出し元・既存テストの契約
    /// （`scan`／`aggregate` も含め全 op が placeholder 応答）を維持する。
    pub fn new(users: Arc<UserStore>, sessions: SessionStore) -> Router {
        Router {
            users,
            sessions,
            engine: None,
        }
    }

    /// `engine` 接続済みの構築経路（Issue #766・#768・#772・#876。
    /// `main.rs::run_server` の nosql 分岐から呼ばれる）。`scan`／
    /// `aggregate`／`insert`／`update`／`delete` op は
    /// [`super::query::scan::handle`]／[`super::query::aggregate::handle`]／
    /// [`super::query::insert::handle`]／[`super::query::update::handle`]／
    /// [`super::query::delete::handle`] へ結線され実行可能になる
    /// （`update`／`delete` は `where` 形のみ。`filter` 形は Issue #871 の
    /// 実行結線待ち）。
    pub fn with_engine(
        users: Arc<UserStore>,
        sessions: SessionStore,
        engine: Arc<EngineCore>,
    ) -> Router {
        Router {
            users,
            sessions,
            engine: Some(engine),
        }
    }
}

impl RequestHandler for Router {
    fn handle(&self, req: &Request<'_>) -> Vec<u8> {
        // `Method` は `Post` の 1 variant のみを持つ閉じた語彙（要求行
        // パース時点で既に絞り込み済み）。この irrefutable な分配束縛は
        // 将来 variant が増えた場合にコンパイルエラーで気付ける
        // defense in depth であり、実行時の再検査コストは発生しない
        // （モジュール doc 参照）。
        let Method::Post = req.line.method;

        match resolve_target(req.line.target) {
            Route::Endpoint(Endpoint::Session) => session_issue::handle_with_engine(
                &self.users,
                self.engine.as_deref(),
                &self.sessions,
                req.body,
                Instant::now,
                SystemTime::now(),
            ),
            Route::Endpoint(Endpoint::SessionClose) => session_close::handle(
                &self.sessions,
                &req.headers,
                req.body,
                Instant::now,
                SystemTime::now(),
            ),
            Route::Endpoint(Endpoint::Query) => {
                match middleware::authenticate(&self.sessions, &req.headers, Instant::now) {
                    Ok(principal) => query_gate::handle(
                        &principal,
                        &req.headers,
                        req.body,
                        self.engine.as_deref(),
                        SystemTime::now(),
                    ),
                    Err(e) => response::encode_error(
                        e.error_class(),
                        e.client_message(),
                        SystemTime::now(),
                    ),
                }
            }
            Route::QueryTenantMarker => response::encode_error(
                ErrorClass::UnsupportedSqlSyntax,
                QUERY_TARGET_TENANT_MARKER_MESSAGE,
                SystemTime::now(),
            ),
            Route::Unknown => unknown_target_response(SystemTime::now()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_target_classifies_the_three_endpoints() {
        assert_eq!(
            resolve_target(SESSION_TARGET),
            Route::Endpoint(Endpoint::Session)
        );
        assert_eq!(
            resolve_target(SESSION_CLOSE_TARGET),
            Route::Endpoint(Endpoint::SessionClose)
        );
        assert_eq!(
            resolve_target(QUERY_TARGET),
            Route::Endpoint(Endpoint::Query)
        );
    }

    #[test]
    fn resolve_target_classifies_query_tenant_marker_variants() {
        assert_eq!(
            resolve_target("/v1/query?tenant_id=other"),
            Route::QueryTenantMarker
        );
        assert_eq!(
            resolve_target("/v1/query/tenant_id/other"),
            Route::QueryTenantMarker
        );
        assert_eq!(
            resolve_target("/v1/query?TENANT-ID=other"),
            Route::QueryTenantMarker
        );
        assert_eq!(
            resolve_target("/v1/query/tenantid/other"),
            Route::QueryTenantMarker
        );
    }

    /// 未知ターゲット（完全未知・既知エンドポイント＋クエリ文字列／末尾
    /// スラッシュ／余剰セグメント・大文字小文字違い・スラッシュ二重化・
    /// パーセントエンコード・フラグメント）が漏れなく [`Route::Unknown`]
    /// になることを固定する（Issue #758・NOSQL-1・HTTP-2）。
    #[test]
    fn resolve_target_classifies_unknown_targets() {
        let unknown_targets = [
            // (a) 完全未知
            "/",
            "/definitely/unknown",
            "/v1",
            "/v1/sessions",
            // (b) 既知エンドポイント＋クエリ文字列
            "/v1/session?x=1",
            "/v1/session/close?x=1",
            "/v1/query?x=1",
            // (c) 既知エンドポイント＋末尾スラッシュ／余剰セグメント
            "/v1/session/",
            "/v1/session/close/",
            "/v1/query/",
            "/v1/query/extra",
            // 追加: 大文字小文字・スラッシュ二重化・パーセントエンコード・
            // フラグメント（バイト厳密一致のみで判定するため正規化しない）
            "/V1/QUERY",
            "//v1/query",
            "/v1/query%2F",
            "/v1/query#frag",
        ];
        for target in unknown_targets {
            assert_eq!(
                resolve_target(target),
                Route::Unknown,
                "target {target:?} should resolve to Route::Unknown"
            );
        }
    }
}
