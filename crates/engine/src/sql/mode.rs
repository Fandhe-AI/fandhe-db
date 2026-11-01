//! 取得モード（`recall`／`precision`）の構文解決コア（TASK-161、対象ビヘイビア: SQL-12。
//! ポインタ: docs/spec/05-tasks.md TASK-161・docs/spec/04-behavior/sql-surface.md SQL-12）。
//!
//! 責務境界: クエリ単位の `USING MODE` 句・セッション変数 `SET search_mode`
//! （いずれも [`allowlist`](crate::sql::allowlist) が構文として受理し、生のリテラル
//! 文字列を返す）から得られる値と、`query_planner.rs::QueryExpansion::mode_hint`
//! 由来のプランナー推定を [`resolve_mode_with_planner`] が決定的に解決する
//! （優先順位・fail-safe の解決契約は spec のビヘイビア定義〔TASK-164・PLAN-11〕を
//! 参照）。解決結果（[`ResolvedMode`]）は
//! [`parser::BoundStatement`](crate::sql::parser::BoundStatement)
//! （`bind_with_session`）が保持する。SQL 表層はプランナー推定の結線を持たない
//! （TASK-77/78 の管轄）ため、そちらは引き続き [`resolve_mode`]（2 引数版）を呼ぶ。
//!
//! `precision` の**実行契約**（確信度判定・空集合 fail-closed 応答）は本モジュールの
//! 管轄外（TASK-162・対象ビヘイビア SEARCH-9）。本モジュールは構文・優先順位の解決
//! までを担い、`recall`／`precision` いずれの値も対等に解決する（fail-open のような
//! 値の書き換えは行わない）。
//!
//! [`SessionState`] は wire 接続 1 本につき 1 個、呼び出し元（wire-server の接続
//! ハンドラ。TASK-73・TASK-165 の管轄）が所有する値型として設計する。`EngineCore`
//! 等の複数接続で共有される構造体には置かない（接続間でモードが混線しない構造を
//! 型で担保するため）。

use crate::sql::allowlist::SqlSurfaceError;

/// 取得モード（README「実装方針（要点）」で公開済みの `recall`〔既定・広域〕／
/// `precision`〔ピンポイント〕の 2 値）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    Recall,
    Precision,
}

impl SearchMode {
    /// `USING MODE`／`SET search_mode` のリテラル値を解決する（SQL-12）。
    ///
    /// 完全一致のみ受理する（大文字・前後空白・空文字はすべて拒否）。fail-closed
    /// 側に倒す方針（緩和は後から容易だが、逆〔一度緩めた受理範囲を狭める〕は
    /// 互換性破壊になるため）。不一致は [`SqlSurfaceError::InvalidInput`]
    /// （ERR-2: `22000`。構文上は文字列リテラルとして正しく受理されたが値が不正）。
    pub fn parse_literal(literal: &str) -> Result<Self, SqlSurfaceError> {
        match literal {
            "recall" => Ok(SearchMode::Recall),
            "precision" => Ok(SearchMode::Precision),
            other => Err(SqlSurfaceError::invalid_input(format!(
                "unknown search mode: {other}"
            ))),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            SearchMode::Recall => "recall",
            SearchMode::Precision => "precision",
        }
    }
}

/// 実効モードの指定元（4 値。TASK-164・PLAN-11 で `PlannerEstimate` を追加し確定。
/// `#[non_exhaustive]` により TASK-161 時点で確保していた拡張点を実際に使用した。
/// 優先順位の解決契約は spec のビヘイビア定義〔PLAN-11〕を参照）。
///
/// **TASK-161 で意図的に付与した破壊的変更（BREAKING CHANGE）**: `#[non_exhaustive]` を
/// 付与したため、クレート外で本 enum を網羅的に `match` していたコードは
/// `_ => ...` 等のワイルドカードアーム追加が必須になる（詳細は PR #188 の
/// Breaking Changes 節を参照）。
///
/// `#[non_exhaustive]`: TASK-164 で `PlannerEstimate` variant を追加した際、
/// クレート外の `match` 式が網羅性エラーでコンパイル不能になる破壊的変更を防いだ
/// （AGENTS.md「公開 API・エラー契約の互換性（P1）」。PR #188 レビュー指摘対応:
/// `BoundStatement`／`ValidatedStatement` と同種の拡張点保護）。クレート外で本 enum を
/// `match` する場合は `_ => ...` 等のワイルドカードアームが必須になる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ModeSource {
    QueryClause,
    SessionVariable,
    /// LLM クエリプランナー（`query_planner.rs`。TASK-110・PLAN-1）の展開結果に
    /// 含まれる `mode_hint` を採用した場合の指定元（TASK-164・PLAN-11。採用条件は
    /// [`resolve_mode_with_planner`] のドキュメント参照）。
    PlannerEstimate,
    Default,
}

impl ModeSource {
    /// `EXPLAIN` 等での可視化用の識別子（プログラム出力文字列は英語。
    /// TASK-78・SQL-6 が wire 応答へ載せる際の値として使う想定の安定した公開契約
    /// のため、一度出した値は今後変更しない）。
    pub fn as_str(&self) -> &'static str {
        match self {
            ModeSource::QueryClause => "query_clause",
            ModeSource::SessionVariable => "session_variable",
            ModeSource::PlannerEstimate => "planner_estimate",
            ModeSource::Default => "default",
        }
    }
}

/// 優先順位解決を終えた実効モードと、その指定元。
///
/// **TASK-161 で意図的に非公開化した破壊的変更（BREAKING CHANGE）**: 全フィールドを
/// `pub` から `pub(crate)` へ変更し `#[non_exhaustive]` を付与した。クレート外からの
/// 直接のフィールド参照・構造体リテラル構築は今後不可能。構築は [`ResolvedMode::new`]、
/// 読み取りは [`ResolvedMode::mode`]／[`ResolvedMode::source`] を使う（詳細は PR #188 の
/// Breaking Changes 節を参照。TASK-164 拡張点の前方互換確保とカプセル化のため）。
///
/// `#[non_exhaustive]`: 本構造体に将来フィールドを追加しても（例:
/// `ModeSource::PlannerEstimate` 導入時の付随情報）、既存の構造体リテラル構築コードが
/// 必須フィールド不足でコンパイル不能になる破壊的変更を防ぐ（PR #188 レビュー指摘
/// 対応: `BoundStatement`／`ValidatedStatement` と同種の拡張点保護）。フィールドは
/// カプセル化のため `pub(crate)` とし、クレート外からの構築は [`ResolvedMode::new`]、
/// 読み取りは [`ResolvedMode::mode`]／[`ResolvedMode::source`] を経由する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ResolvedMode {
    pub(crate) mode: SearchMode,
    pub(crate) source: ModeSource,
}

impl ResolvedMode {
    /// クレート外から解決済みモードを構築する constructor（`resolve_mode` を経ない
    /// テスト・拡張コード向け）。
    pub fn new(mode: SearchMode, source: ModeSource) -> Self {
        Self { mode, source }
    }

    /// 解決された実効モード。
    pub fn mode(&self) -> SearchMode {
        self.mode
    }

    /// 実効モードの指定元。
    pub fn source(&self) -> ModeSource {
        self.source
    }
}

/// クエリ句・セッション変数それぞれの解決済み値から実効モードを決定する
/// （副作用なし・決定的。同一入力には常に同一の結果を返す）。優先順位は
/// **クエリ句 > セッション変数 > 既定（`recall`）**（SQL-12）。
///
/// SQL 表層（`sql/parser.rs::bind_with_session` 等。TASK-77/78 が管轄する
/// `USING PLAN`／`EXPLAIN` 結線までは未実装）はプランナー推定を持たないため、
/// 引き続きこの 2 引数版を呼ぶ（[`resolve_mode_with_planner`] へ `planner: None`
/// を渡す後方互換の薄い委譲。TASK-164・PLAN-11）。
pub fn resolve_mode(query: Option<SearchMode>, session: Option<SearchMode>) -> ResolvedMode {
    resolve_mode_with_planner(query, session, None)
}

/// クエリ句・セッション変数・プランナー推定の 3 系統から実効モードを短絡順序で
/// 決定的に解決する（TASK-164・PLAN-11。解決契約〔優先順位・fail-safe 方針〕は
/// spec のビヘイビア定義〔PLAN-11〕を参照）。`planner`
/// （`query_planner.rs::QueryExpansion::mode_hint` 由来。不正値の丸めは
/// `query_planner::parse_expansion` 側が担う）は既に検証済みの
/// `Option<SearchMode>` を受け取るだけの契約とする。
pub fn resolve_mode_with_planner(
    query: Option<SearchMode>,
    session: Option<SearchMode>,
    planner: Option<SearchMode>,
) -> ResolvedMode {
    if let Some(mode) = query {
        return ResolvedMode {
            mode,
            source: ModeSource::QueryClause,
        };
    }
    if let Some(mode) = session {
        return ResolvedMode {
            mode,
            source: ModeSource::SessionVariable,
        };
    }
    if let Some(mode) = planner {
        return ResolvedMode {
            mode,
            source: ModeSource::PlannerEstimate,
        };
    }
    ResolvedMode {
        mode: SearchMode::Recall,
        source: ModeSource::Default,
    }
}

/// 接続（セッション）単位のモード状態。呼び出し元が 1 接続につき 1 個所有する
/// 値型（モジュールドキュメント参照）。意図的に `Copy` を実装しない
/// （`Copy` だと呼び出し元が値でコピーを保持したまま `set_search_mode` を
/// コピー側へ適用し、`SET` が見かけ上成功しつつ元のセッションへ反映されない
/// 事故を型で防げなくなるため。呼び出し元は `&mut SessionState` を経由して
/// 更新する契約とする）。
#[derive(Debug, Clone, Default)]
pub struct SessionState {
    search_mode: Option<SearchMode>,
    /// DDL（`CREATE TABLE`・`DROP TABLE` 等）実行権限（SQL-23・TASK-202・
    /// TASK-203、Issue #899・#902）。既定 `false`（fail-closed。
    /// `#[derive(Default)]` により未設定接続・NoSQL 表層・既存テストが使う
    /// `SessionState::default()` は構造上すべての DDL を実行できない）。
    ///
    /// [`crate::policy::PolicyContext`] はテナント ID と可視性のみを運び認証主体を
    /// 持たないため、DDL 権限はテナント境界とは別軸の権限として本フィールドが
    /// 担う（テーブル・カタログは全テナント共有であり、DDL 文はテナント
    /// スコープの操作ではない）。付与は認証層（wire-server の handshake）が
    /// 認証成功後に 1 回だけ行う契約——SQL 文経由で自身の権限を昇格する経路は
    /// 構造的に存在しない（[`Self::allow_ddl`] 以外に本フィールドを変更する
    /// 公開手段を持たない）。`sql::ddl::require_ddl_permission` が唯一の判定点。
    ddl_allowed: bool,
    /// TASK-79（SQL-9）: `CREATE FUNCTION` で登録した宣言的 UDF のセッション単位
    /// レジストリ。`SessionState` 自体が接続（＝認証済みテナント）単位の値型であるため、
    /// UDF 定義が他接続・他テナントへ漏れる経路は構造上存在しない。永続化しない
    /// （`crate::sql::udf_call` モジュールドキュメント参照）。
    udfs: crate::sql::udf_call::UdfRegistry,
    /// セッションの現在の TimeZone（TABLE-33・Issue #1575）。既定は UTC。接続単位の値型である
    /// ため他接続・他テナントへ影響する経路は構造上存在せず、`sql/transaction.rs` が
    /// ROLLBACK／abort で `SessionState` ごと巻き戻すため PG と同じく取り消しで値が戻る。
    time_zone: crate::datetime::SessionTimeZone,
    /// `SET TIME ZONE DEFAULT|LOCAL` の戻り先（reset 値）。StartupMessage の `TimeZone` が
    /// あればその値、無ければサーバー既定の UTC。
    time_zone_reset: crate::datetime::SessionTimeZone,
    /// セッション単位の一時カタログ（TABLE-44・Issue #1681）。接続単位の値型である
    /// ため他接続・他テナントへ漏れる経路は構造上存在せず、永続化もしない。ROLLBACK／abort
    /// の `SessionState` 巻き戻しに乗って取り消される。スキーマは `Arc` 共有のため
    /// BEGIN／メッセージ単位の `clone` は安い。行データは持たない（行は `temp_rows` の行ストアが持つ）。
    temp_tables: crate::sql::temp_catalog::SessionTempCatalog,
    /// 一時表の行ストア（TABLE-44・Issue #1682）。最初に行領域が要るとき
    /// （[`Self::temp_rows_or_init`]）まで `None`（使わない接続のコストはゼロ）。
    /// `Arc` を持つのは本構造体とその巻き戻し用 clone だけで、全部 drop されるとメモリごと
    /// 消える（切断時の破棄）。詳細は `sql::temp_rows` のモジュールドキュメント。
    temp_rows: Option<std::sync::Arc<crate::sql::temp_rows::TempRowStore>>,
    /// `currval` 用の状態（TABLE-24・Issue #1690）。接続単位の値型であるため他接続・他テナントへ
    /// 漏れる経路は構造上存在しない。`Arc` 共有なので、BEGIN 時のコピーを ROLLBACK／abort で
    /// 戻しても値は消えない（PostgreSQL の `currval` と同じ。`sql/transaction.rs` 参照）。
    sequences: crate::sql::sequence_scope::SharedSessionSequences,
    /// セッションの `search_path`（TABLE-27・Issue #1715）。既定は `"$user", public`。接続単位の値型
    /// であるため他接続へ漏れず、ROLLBACK／abort の `SessionState` 巻き戻しに乗って取り消される
    /// （ADR D5）。変更は検証成功後の [`Self::set_search_path`] だけで行う。
    search_path: std::sync::Arc<crate::sql::search_path::SearchPath>,
    /// `search_path` の変更ごとに進むプロセス全体で単調な世代（0 は未設定の予約値）。準備済み文・
    /// 解析済み文が、path が変わった後の実行で陳腐化していないかの照合に使う
    /// （[`crate::core::SessionRoutedSql`]）。値が等しければ path も等しい。
    search_path_epoch: u64,
    /// `"$user"` の展開に使う、接続した主体の username。認証層（wire-server の handshake）が認証成功後に
    /// 1 回だけ設定する契約で、SQL 文から変える経路は持たない（`allow_ddl` と同じ。ADR D6）。
    session_user: Option<std::sync::Arc<str>>,
    /// 拡張クエリの Parse が `schema.name` の修飾を解決するテナント（TABLE-27・Issue #1717、ADR D6）。
    /// Parse には認証コンテキスト（`PolicyContext`）が渡らないため、接続の認証済みテナントを
    /// 認証成功後に 1 回だけ束縛する（[`Self::bind_resolution_tenant`]）。SQL 文経由で変更する
    /// 手段は無い。未束縛（既定）の Parse はテナント SCHEMA の修飾を `3F000` にする（fail-closed）。
    ///
    /// `Box<str>`: `SessionState` を抱える `sql::transaction::TxnState` の enum 内サイズ差（clippy
    /// `large_enum_variant`）を増やさないため、`String` より小さい表現にする。
    resolution_tenant: Option<Box<str>>,
    /// `row_security = off` か（RLS-13。既定は `false` = on）。`#[derive(Default)]` で既定が on に
    /// なるよう、否定形で持つ。利用者ポリシーの適用可否だけに作用し、テナント境界には作用しない
    /// （判定は [`crate::sql::policy_applicability::decide`]）。ROLLBACK／abort の巻き戻しで取り消される。
    row_security_off: bool,
    /// `RESET row_security`／`SET row_security TO DEFAULT` の戻り先が off か（起動パラメータ由来）。
    row_security_reset_off: bool,
    /// 現在のロール ID（#1971 以降で認証層が認証成功後に 1 回だけ設定する契約。SQL 文から書き換える
    /// 経路は持たない）。`None`（現状の全セッション）は「BYPASSRLS なし・非所有者」として扱う
    /// （利用者ポリシーを適用する＝狭める側。fail-closed）。
    current_role_id: Option<u64>,
    /// セッションのロール ID（`SESSION_USER` 相当。設定契約は `current_role_id` と同じ）。
    session_role_id: Option<u64>,
}

/// `search_path` の世代の採番元。0 は採番されない。
static NEXT_SEARCH_PATH_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl SessionState {
    /// セッションの現在の `search_path`（TABLE-27・Issue #1715）。
    pub fn search_path(&self) -> &crate::sql::search_path::SearchPath {
        self.search_path.as_ref()
    }

    /// 検証済みの `search_path` を現在値にして世代を進める。呼び出し元（`core.rs` の `SetSearchPath`
    /// 実行腕）が検証成功後にのみ呼ぶ（失敗した `SET` はセッションを変えない）。世代の採番が尽きた
    /// 場合は `Internal`（fail-closed。到達しない想定）。
    pub(crate) fn set_search_path(
        &mut self,
        path: crate::sql::search_path::SearchPath,
    ) -> Result<(), SqlSurfaceError> {
        let epoch = NEXT_SEARCH_PATH_EPOCH
            .try_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |v| v.checked_add(1),
            )
            .map_err(|_| SqlSurfaceError::Internal {
                detail: "search_path epoch space exhausted".to_string(),
            })?;
        self.search_path = std::sync::Arc::new(path);
        self.search_path_epoch = epoch;
        Ok(())
    }

    /// `RESET search_path`／`SET search_path TO DEFAULT` の実体。既定値へ戻す。
    pub(crate) fn reset_search_path(&mut self) -> Result<(), SqlSurfaceError> {
        self.set_search_path(crate::sql::search_path::SearchPath::default())
    }

    /// `search_path` の現在の世代（変更ごとに進む。未変更は 0）。
    pub(crate) fn search_path_epoch(&self) -> u64 {
        self.search_path_epoch
    }

    /// 実効 `search_path` に `public` が含まれないか。真のセッションだけが未修飾名の参照ゲート・
    /// 作成先判定・準備済み文の再検証の対象になる（既定の path は常に偽で、追加コストを払わない）。
    pub fn search_path_hides_public(&self) -> bool {
        self.search_path.hides_public(self.session_user.as_deref())
    }

    /// `"$user"` の展開に使う username（未設定なら `None`）。
    pub(crate) fn session_user(&self) -> Option<&str> {
        self.session_user.as_deref()
    }

    /// 接続した主体の username を設定する。wire-server の handshake が認証成功後に 1 回だけ呼ぶ契約
    /// （SQL 文経由の経路は持たない。`allow_ddl` と同じ。TABLE-27・Issue #1715・ADR D6）。
    pub fn set_session_user(&mut self, username: &str) {
        self.session_user = Some(std::sync::Arc::from(username));
    }

    /// セッションの現在の TimeZone。
    pub fn time_zone(&self) -> &crate::datetime::SessionTimeZone {
        &self.time_zone
    }

    /// 検証済みの TimeZone を現在値にする。呼び出し元（`core.rs` の `SetTimeZone` 実行腕）が
    /// `SessionTimeZone::parse_setting` 成功後にのみ呼ぶ（失敗した `SET` はセッションを変えない）。
    pub fn set_time_zone(&mut self, tz: crate::datetime::SessionTimeZone) {
        self.time_zone = tz;
    }

    /// `SET TIME ZONE DEFAULT|LOCAL` の実体。現在値を reset 値へ戻す。
    pub fn reset_time_zone(&mut self) {
        self.time_zone = self.time_zone_reset.clone();
    }

    /// StartupMessage の `TimeZone` を現在値と reset 値の両方へ設定する。wire-server の
    /// handshake が認証成功後に 1 回だけ呼ぶ契約（SQL 文経由の経路は持たない）。
    pub fn apply_startup_time_zone(&mut self, tz: crate::datetime::SessionTimeZone) {
        self.time_zone_reset = tz.clone();
        self.time_zone = tz;
    }

    /// セッションの `row_security`（既定 on = `true`。RLS-13）。
    pub fn row_security(&self) -> bool {
        !self.row_security_off
    }

    /// 検証済みの `row_security` を現在値にする（`core.rs` の `SetRowSecurity` 実行腕が値の検証後にのみ呼ぶ）。
    pub(crate) fn set_row_security(&mut self, on: bool) {
        self.row_security_off = !on;
    }

    /// `RESET row_security`／`SET row_security TO DEFAULT` の実体。現在値を reset 値へ戻す。
    pub(crate) fn reset_row_security(&mut self) {
        self.row_security_off = self.row_security_reset_off;
    }

    /// StartupMessage の `row_security` を現在値と reset 値の両方へ設定する。wire-server の handshake が
    /// 認証成功後に 1 回だけ呼ぶ契約（SQL 文経由の経路は持たない）。
    pub fn apply_startup_row_security(&mut self, on: bool) {
        self.row_security_off = !on;
        self.row_security_reset_off = !on;
    }

    /// 現在のロール ID（未紐付けは `None`）。
    pub fn current_role_id(&self) -> Option<u64> {
        self.current_role_id
    }

    /// セッションのロール ID（未紐付けは `None`）。
    pub fn session_role_id(&self) -> Option<u64> {
        self.session_role_id
    }

    /// 認証主体のロール ID を束縛する。認証層が認証成功後に 1 回だけ呼ぶ契約（`set_session_user` と同じ。
    /// SQL 文経由の経路は持たない）。
    pub fn bind_role_ids(&mut self, current: u64, session: u64) {
        self.current_role_id = Some(current);
        self.session_role_id = Some(session);
    }

    pub fn search_mode(&self) -> Option<SearchMode> {
        self.search_mode
    }

    /// このセッションが一時表を 1 つでも持つか（TABLE-44・Issue #1684）。wire-server が、一時表を持つ
    /// セッションでは準備済み文を Bind のたびにセッション対応の経路（`bind_prepared_in_session`）で
    /// 検証し直すかどうかの判定に使う。一時表を持たないセッションの挙動は従来と同一。
    pub fn has_temp_tables(&self) -> bool {
        !self.temp_tables.is_empty()
    }

    /// `currval` 用の状態の共有ハンドル（INSERT 実行区間の `SequenceScope` が受け取る。
    /// TABLE-24・Issue #1690）。
    pub(crate) fn sequences_handle(&self) -> crate::sql::sequence_scope::SharedSessionSequences {
        std::sync::Arc::clone(&self.sequences)
    }

    /// このセッションの一時カタログ（読み取り専用。TABLE-44・Issue #1681）。
    pub(crate) fn temp_tables(&self) -> &crate::sql::temp_catalog::SessionTempCatalog {
        &self.temp_tables
    }

    /// このセッションの一時カタログ（更新用。`core.rs` の `CREATE TEMP TABLE`／一時表の
    /// `DROP TABLE` 実行腕が使う。TABLE-44・Issue #1683）。
    pub(crate) fn temp_tables_mut(&mut self) -> &mut crate::sql::temp_catalog::SessionTempCatalog {
        &mut self.temp_tables
    }

    /// このセッションの一時表ストア（生成済みなら）を `Arc` で返す。一時表の文の実行腕
    /// （`core/temp_route.rs`）が、セッションの借用と切り離して一時コアを使うために呼ぶ。
    pub(crate) fn temp_rows_arc(
        &self,
    ) -> Option<std::sync::Arc<crate::sql::temp_rows::TempRowStore>> {
        self.temp_rows.clone()
    }

    #[cfg(test)]
    pub(crate) fn temp_rows_arc_for_test(
        &self,
    ) -> Option<std::sync::Arc<crate::sql::temp_rows::TempRowStore>> {
        self.temp_rows.clone()
    }

    /// 一時表ストアを返す。未生成なら `tenant_id` 所有で生成する（遅延生成。`template` は構成を
    /// 引き継ぐ永続コア）。生成済みで所有テナントが違う場合は fail-closed で拒否する。
    /// `CREATE TEMP TABLE` の実行腕が呼ぶ（TABLE-44・Issue #1683）。
    pub(crate) fn temp_rows_or_init(
        &mut self,
        tenant_id: &str,
        max_total_bytes: u64,
        template: &crate::core::EngineCore,
    ) -> Result<&crate::sql::temp_rows::TempRowStore, crate::sql::allowlist::SqlSurfaceError> {
        let store = match self.temp_rows.take() {
            Some(s) => s,
            None => std::sync::Arc::new(crate::sql::temp_rows::TempRowStore::new(
                tenant_id,
                max_total_bytes,
                template,
            )?),
        };
        let mismatch = !store.owner_matches(tenant_id);
        let store = self.temp_rows.insert(store);
        if mismatch {
            return Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: "temporary storage error".to_string(),
            });
        }
        Ok(store)
    }

    /// commit 時の `ON COMMIT` 動作を適用する（TABLE-44・Issue #1683）。`DROP` の一時表は
    /// 一時 DB から削除して一時カタログから外し、`DELETE ROWS` の一時表は行を消す。
    /// 呼び出し元は自動コミットの文の成功直後（`core.rs`）と、明示・暗黙トランザクションの
    /// commit 成功直後（`sql::transaction`）。ROLLBACK・失敗した文では呼ばない。
    ///
    /// **一時の書き込みトランザクションが開いていない状態で呼ぶこと**（一時 DB の単一ライタを
    /// 取り直すため。開いていると `55P03` になる）。呼び出し元は先に一時側を commit する。
    /// 対象が無ければ何もしない（一時表を使わない接続のコストはゼロ）。一時 DB を先に更新し、
    /// 成功してからカタログを更新する（失敗してもカタログと実体が食い違わない）。失敗は
    /// 固定文言の `XX000` を返し詳細を渡さない。
    pub(crate) fn apply_temp_on_commit(
        &mut self,
    ) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
        use crate::sql::allowlist::TempOnCommit;
        let mut delete_rows = Vec::new();
        let mut dropped = Vec::new();
        for name in self.temp_tables.names() {
            let Some(entry) = self.temp_tables.get(name) else {
                continue;
            };
            match entry.on_commit {
                TempOnCommit::PreserveRows => {}
                TempOnCommit::DeleteRows => delete_rows.push(name.to_string()),
                TempOnCommit::Drop => dropped.push(name.to_string()),
            }
        }
        if delete_rows.is_empty() && dropped.is_empty() {
            return Ok(());
        }
        let Some(store) = self.temp_rows.clone() else {
            return Err(crate::sql::allowlist::SqlSurfaceError::Internal {
                detail: "temporary storage error".to_string(),
            });
        };
        store.apply_on_commit(&delete_rows, &dropped)?;
        for name in &dropped {
            self.temp_tables.remove(name);
        }
        Ok(())
    }

    /// このセッションに登録済みの UDF レジストリ（読み取り専用）。
    pub fn udfs(&self) -> &crate::sql::udf_call::UdfRegistry {
        &self.udfs
    }

    /// `core.rs::EngineCore::execute_sql_in_session` の `CreateFunction` 分岐から
    /// 呼ばれる。登録の検証自体は `udf_call::define_function` が担い、本メソッドは
    /// セッションが保持する `&mut UdfRegistry` を貸し出すだけの薄いアクセサ。
    pub fn udfs_mut(&mut self) -> &mut crate::sql::udf_call::UdfRegistry {
        &mut self.udfs
    }

    /// このセッションが DDL 実行権限（SQL-23、TASK-203、Issue #902）を持つか。
    /// [`crate::sql::ddl::require_ddl_permission`] が唯一の判定点として参照する。
    pub fn ddl_allowed(&self) -> bool {
        self.ddl_allowed
    }

    /// 拡張クエリの Parse が SCHEMA 修飾の解決に使うテナント（未束縛なら `None`）。
    pub fn resolution_tenant(&self) -> Option<&str> {
        self.resolution_tenant.as_deref()
    }

    /// 拡張クエリの Parse が SCHEMA 修飾を解決するテナントを束縛する。wire-server の handshake が
    /// 認証成功直後に 1 回だけ呼ぶ契約（[`Self::allow_ddl`] と同じ。SQL 文経由の経路は無い）。
    /// 既に別のテナントへ束縛済みなら変更せず `false` を返す（接続の途中でテナントを差し替えて
    /// 他テナントの SCHEMA を解決する経路を作らない）。
    pub fn bind_resolution_tenant(&mut self, tenant: &str) -> bool {
        match &self.resolution_tenant {
            None => {
                self.resolution_tenant = Some(tenant.into());
                true
            }
            Some(existing) => &**existing == tenant,
        }
    }

    /// このセッションへ DDL 実行権限を付与する。wire-server の handshake が
    /// 認証成功直後（`UserStore::is_ddl_allowed` が真の場合）に 1 回だけ呼ぶ
    /// 契約とし、SQL 文経由で呼ばれる経路は持たない（`Self::ddl_allowed`
    /// ドキュメント参照）。
    pub fn allow_ddl(&mut self) {
        self.ddl_allowed = true;
    }

    /// TASK-149（対象ビヘイビア: EXT-5, EXT-6）: 検証済みの `Arc<dyn WasmUdfBackend>`
    /// をこのセッションのレジストリへ登録する。名前空間の衝突検査は
    /// `udf_call::define_wasm_function` が担い、登録は宣言的 UDF と同じ
    /// 「セッション（＝認証済みテナントの接続単位）に閉じる・永続化しない」構造に
    /// 従う。モジュールのバイト列からの登録は [`Self::register_wasm_udf_from_module`]。
    /// SQL からの登録構文（`LANGUAGE wasm`）・wire 経由のモジュール搬送は範囲外。
    pub fn register_wasm_udf(
        &mut self,
        name: &str,
        backend: std::sync::Arc<dyn crate::wasm_udf::WasmUdfBackend>,
    ) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
        crate::sql::udf_call::define_wasm_function(&mut self.udfs, name, backend)
    }

    /// Issue #1926（EXT-9 ポインタ）: WASM モジュールのバイト列を宣言シグネチャと ABI の
    /// 照合・検証・コンパイルを経てこのセッションへ登録する。feature `wasm-udf` 無効の
    /// ビルドでは常に `0A000`。失敗時はレジストリを変えない。Rust の呼び出し元専用で、
    /// SQL・wire からは到達できない。
    pub fn register_wasm_udf_from_module(
        &mut self,
        name: &str,
        signature: &crate::wasm_udf::WasmUdfSignature,
        module_bytes: &[u8],
        entry: &str,
        limits: crate::wasm_udf::SandboxLimits,
    ) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
        crate::sql::udf_call::define_wasm_function_from_module(
            &mut self.udfs,
            name,
            signature,
            module_bytes,
            entry,
            limits,
        )
    }

    /// 検証済みの値のみを受け取る契約（呼び出し元 `core.rs::EngineCore::execute_sql_in_session`
    /// が `SearchMode::parse_literal` の検証成功後にのみ呼ぶ）。失敗した `SET` が
    /// セッションを変更しないことは、この「検証→代入」の呼び出し順序で保証する
    /// （部分更新＝黙った既定化と同種の fail-open を防ぐ。security.md 準拠）。
    pub fn set_search_mode(&mut self, mode: SearchMode) {
        self.search_mode = Some(mode);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_zone_defaults_to_utc_and_resets_to_startup_value() {
        use crate::datetime::SessionTimeZone;
        let mut session = SessionState::default();
        assert_eq!(session.time_zone().display(), "UTC");
        session.set_time_zone(SessionTimeZone::parse_setting("Asia/Tokyo").unwrap());
        assert_eq!(session.time_zone().display(), "Asia/Tokyo");
        session.reset_time_zone();
        assert_eq!(session.time_zone().display(), "UTC");

        session
            .apply_startup_time_zone(SessionTimeZone::parse_setting("America/New_York").unwrap());
        session.set_time_zone(SessionTimeZone::utc());
        let snapshot = session.clone();
        session.reset_time_zone();
        assert_eq!(session.time_zone().display(), "America/New_York");
        assert_eq!(snapshot.time_zone().display(), "UTC");
    }

    #[test]
    fn resolve_mode_defaults_to_recall_when_unset() {
        let resolved = resolve_mode(None, None);
        assert_eq!(resolved.mode, SearchMode::Recall);
        assert_eq!(resolved.source, ModeSource::Default);
    }

    #[test]
    fn resolve_mode_session_variable_wins_over_default() {
        let resolved = resolve_mode(None, Some(SearchMode::Precision));
        assert_eq!(resolved.mode, SearchMode::Precision);
        assert_eq!(resolved.source, ModeSource::SessionVariable);
    }

    #[test]
    fn resolve_mode_query_clause_wins_over_session_variable() {
        let resolved = resolve_mode(Some(SearchMode::Recall), Some(SearchMode::Precision));
        assert_eq!(resolved.mode, SearchMode::Recall);
        assert_eq!(resolved.source, ModeSource::QueryClause);
    }

    #[test]
    fn resolve_mode_query_clause_alone_wins_over_default() {
        let resolved = resolve_mode(Some(SearchMode::Precision), None);
        assert_eq!(resolved.mode, SearchMode::Precision);
        assert_eq!(resolved.source, ModeSource::QueryClause);
    }

    #[test]
    fn parse_literal_accepts_only_exact_lowercase_values() {
        assert_eq!(
            SearchMode::parse_literal("recall").expect("recall should parse"),
            SearchMode::Recall
        );
        assert_eq!(
            SearchMode::parse_literal("precision").expect("precision should parse"),
            SearchMode::Precision
        );
    }

    #[test]
    fn parse_literal_rejects_case_variants_whitespace_and_unknown_values() {
        assert!(SearchMode::parse_literal("RECALL").is_err());
        assert!(SearchMode::parse_literal("Recall").is_err());
        assert!(SearchMode::parse_literal(" recall").is_err());
        assert!(SearchMode::parse_literal("recall ").is_err());
        assert!(SearchMode::parse_literal("").is_err());
        assert!(SearchMode::parse_literal("fuzzy").is_err());
    }

    #[test]
    fn session_state_defaults_to_none_and_can_be_overwritten() {
        let mut session = SessionState::default();
        assert_eq!(session.search_mode(), None);
        session.set_search_mode(SearchMode::Precision);
        assert_eq!(session.search_mode(), Some(SearchMode::Precision));
        session.set_search_mode(SearchMode::Recall);
        assert_eq!(session.search_mode(), Some(SearchMode::Recall));
    }

    #[test]
    fn mode_source_as_str_is_stable() {
        // EXPLAIN 出力（TASK-78）が読む公開契約のため、4 値を明示的に固定する
        // （TASK-164・PLAN-11）。
        assert_eq!(ModeSource::QueryClause.as_str(), "query_clause");
        assert_eq!(ModeSource::SessionVariable.as_str(), "session_variable");
        assert_eq!(ModeSource::PlannerEstimate.as_str(), "planner_estimate");
        assert_eq!(ModeSource::Default.as_str(), "default");
    }

    // TASK-164・PLAN-11: query × session × planner の全 8 組合せで解決結果と
    // `source` を検査する（解決契約は spec のビヘイビア定義〔PLAN-11〕参照）。
    #[test]
    fn resolve_mode_with_planner_query_session_planner_all_set_query_wins() {
        let resolved = resolve_mode_with_planner(
            Some(SearchMode::Recall),
            Some(SearchMode::Precision),
            Some(SearchMode::Precision),
        );
        assert_eq!(resolved.mode, SearchMode::Recall);
        assert_eq!(resolved.source, ModeSource::QueryClause);
    }

    #[test]
    fn resolve_mode_with_planner_query_and_session_set_planner_unset_query_wins() {
        let resolved =
            resolve_mode_with_planner(Some(SearchMode::Recall), Some(SearchMode::Precision), None);
        assert_eq!(resolved.mode, SearchMode::Recall);
        assert_eq!(resolved.source, ModeSource::QueryClause);
    }

    #[test]
    fn resolve_mode_with_planner_query_and_planner_set_session_unset_query_wins() {
        let resolved =
            resolve_mode_with_planner(Some(SearchMode::Recall), None, Some(SearchMode::Precision));
        assert_eq!(resolved.mode, SearchMode::Recall);
        assert_eq!(resolved.source, ModeSource::QueryClause);
    }

    #[test]
    fn resolve_mode_with_planner_query_alone_wins() {
        let resolved = resolve_mode_with_planner(Some(SearchMode::Precision), None, None);
        assert_eq!(resolved.mode, SearchMode::Precision);
        assert_eq!(resolved.source, ModeSource::QueryClause);
    }

    #[test]
    fn resolve_mode_with_planner_session_and_planner_set_query_unset_session_wins() {
        let resolved =
            resolve_mode_with_planner(None, Some(SearchMode::Recall), Some(SearchMode::Precision));
        assert_eq!(resolved.mode, SearchMode::Recall);
        assert_eq!(resolved.source, ModeSource::SessionVariable);
    }

    #[test]
    fn resolve_mode_with_planner_session_alone_wins() {
        let resolved = resolve_mode_with_planner(None, Some(SearchMode::Precision), None);
        assert_eq!(resolved.mode, SearchMode::Precision);
        assert_eq!(resolved.source, ModeSource::SessionVariable);
    }

    #[test]
    fn resolve_mode_with_planner_planner_alone_wins_over_default() {
        let resolved = resolve_mode_with_planner(None, None, Some(SearchMode::Precision));
        assert_eq!(resolved.mode, SearchMode::Precision);
        assert_eq!(resolved.source, ModeSource::PlannerEstimate);
    }

    #[test]
    fn resolve_mode_with_planner_all_unset_defaults_to_recall() {
        let resolved = resolve_mode_with_planner(None, None, None);
        assert_eq!(resolved.mode, SearchMode::Recall);
        assert_eq!(resolved.source, ModeSource::Default);
    }

    #[test]
    fn resolve_mode_delegates_to_resolve_mode_with_planner_none() {
        // 後方互換の委譲そのものを検査する（TASK-164 で `resolve_mode` の外部
        // シグネチャは変えていないことの回帰テスト）。
        let resolved = resolve_mode(None, Some(SearchMode::Precision));
        assert_eq!(resolved.mode, SearchMode::Precision);
        assert_eq!(resolved.source, ModeSource::SessionVariable);
    }

    /// TABLE-27・Issue #1715: `search_path` の既定値・変更・世代・巻き戻し（clone）・`$user` の扱い。
    #[test]
    fn search_path_defaults_epoch_and_clone_independence() {
        use crate::sql::search_path::{SearchPath, SearchPathElem};
        let mut a = SessionState::default();
        assert_eq!(a.search_path().display(), "\"$user\", public");
        assert_eq!(a.search_path_epoch(), 0);
        assert!(!a.search_path_hides_public());
        let snapshot = a.clone();
        let only_x = SearchPath::new(vec![SearchPathElem::from_ident("x").unwrap()]).unwrap();
        a.set_search_path(only_x).unwrap();
        assert!(a.search_path_hides_public());
        let epoch_after_set = a.search_path_epoch();
        assert_ne!(epoch_after_set, 0);
        // 巻き戻し用 clone は独立で、世代も戻る（ROLLBACK で path が戻る契約）。
        assert!(!snapshot.search_path_hides_public());
        assert_eq!(snapshot.search_path_epoch(), 0);
        a.reset_search_path().unwrap();
        assert!(!a.search_path_hides_public());
        assert!(a.search_path_epoch() > epoch_after_set);
        // `$user` は username が `public` のときだけ public を見せる。
        let only_user = SearchPath::new(vec![SearchPathElem::User]).unwrap();
        a.set_search_path(only_user).unwrap();
        assert!(a.search_path_hides_public());
        a.set_session_user("alice");
        assert!(a.search_path_hides_public());
        a.set_session_user("PUBLIC");
        assert!(!a.search_path_hides_public());
    }

    #[test]
    fn session_state_instances_are_independent() {
        // TASK-161 設計判断: セッション状態は接続単位の値型。2 つの独立したインスタンス間で
        // 一方への `set_search_mode` が他方へ波及しないことを検査する（接続間漏えいの
        // 構造的排除。security.md「テナント境界」と同種の分離思想）。
        let mut session_a = SessionState::default();
        let session_b = SessionState::default();
        session_a.set_search_mode(SearchMode::Precision);
        assert_eq!(session_a.search_mode(), Some(SearchMode::Precision));
        assert_eq!(session_b.search_mode(), None);
    }
}
