//! 一時表（TABLE-44・Issue #1684。親 #1680）の文の振り分けと実行。
//!
//! # 責務境界
//!
//! `core.rs` の `EngineCore`（永続コア）が受けた SQL のうち、**一時表だけを参照する文**を、
//! セッション専用の一時コア（[`crate::sql::temp_rows::TempRowStore`] が持つ、一時 DB だけを
//! ストレージとする別の `EngineCore`）へ振り分ける。一時コアは永続コアと同じ実行器
//! （DML の各 `execute_*_form`・読み取り・制約・索引）をそのまま使うため、DML・`RETURNING`・
//! `ON CONFLICT`・`TRUNCATE`・`SELECT` 系・制約（`PRIMARY KEY`／`UNIQUE`／`CHECK`／`NOT NULL`／
//! `DEFAULT`）・索引の振る舞いが永続表と一致し、`self.storage` を直接引く既存の経路も
//! 一時 DB を引く（同名の永続表へ作用する経路が構造上存在しない。P0）。
//!
//! # 振り分け（parse 時）
//!
//! [`EngineCore::parse_tokens_gated`] が、一時カタログが空でないセッションで一時表の名前を含む
//! 文を次のとおり扱う（呼び出し元は `execute_sql_in_session`・`execute_sql_in_txn`）。
//!
//! - 一時カタログ優先の lookup で parse し、一時表に当たり永続の関係（表・ビュー）に当たらなければ
//!   一時コアで parse し直して [`ParsedSql::SessionRouted`] に包む。永続ビューの本文が一時表を
//!   引く場合も「永続に当たった」扱いになり混在として拒否される（永続ビューが一時表を読む
//!   fail-open を作らない）。
//! - 一時表と永続の関係の混在、振り分け対象外の文種（`ALTER`・ビュー・型・カーソル・
//!   `PARTITIONED` など）で一時表の名前が現れる文は `0A000`（固定文言。永続側の存否を含めない）。
//! - `USING OPERATION_ID` 付きの一時表への書き込みは `0A000`（TABLE-44 (5)。一時表の書き込みは
//!   `operation_id` 台帳の対象外）。一時コアの台帳モードは `CompareOnlyWithoutLedger` で、
//!   `--require-operation-id`（`LedgerMode::Ledgered`）の構成でも一時表への書き込みに id を
//!   要求せず、台帳表にも触れない。
//!
//! # 実行
//!
//! - 自動コミット: 書き込み文は一時の書き込みトランザクションの中で実行し、文の後・commit の前に
//!   総バイト数の上限を判定する（超過は abort して `54000`）。読み取り文は一時コアの通常の
//!   読み取り経路で実行する。索引 DDL は一時コアの DDL 実行本体を直接呼ぶ（DDL 権限ゲートの対象外。
//!   セッション内で閉じ、他セッション・他テナントへ影響しないため）。
//! - 明示・暗黙トランザクション: 最初の一時表の文で一時の書き込みトランザクションを開き
//!   （[`crate::sql::transaction::SessionTransaction::ensure_temp_txn`]）、COMMIT で永続側の
//!   commit 後に commit、ROLLBACK・失敗・drop で abort する。索引 DDL は明示トランザクション内で
//!   `0A000`（永続側の DDL と同じ）。
//!
//! # 準備済み文の陳腐化
//!
//! [`SessionRoutedSql`] は振り分け時の一時カタログの世代を持つ。一時表の作成・削除・索引 DDL の後に
//! 実行されると世代が食い違い `0A000` で拒否する（PostgreSQL なら再計画する場面。fail-closed）。

use std::sync::Arc;

use crate::policy::PolicyContext;
use crate::recovery::required_op_id::LedgerMode;
use crate::sql::allowlist::{DeleteStatement, SqlSurfaceError, Statement, ValidatedUpdateForm};
use crate::sql::mode::SessionState;
use crate::sql::temp_catalog::{SessionTempCatalog, TempRelationId};
use crate::sql::temp_rows::TempRowStore;
use crate::sql::transaction::SessionTransaction;
use crate::storage::{GatedWriteTxn, Storage};

use super::{parsed_operation_id, EngineCore, InsertSchemaLookup, ParsedSql};

/// 準備済み文（[`super::PreparedSql`]）の経路。
#[derive(Debug, Clone)]
pub(super) enum PreparedRoute {
    /// 永続コアで parse した文（セッションを持たない Parse、または一時表の名前を含まない文）。
    Plain,
    /// 一時コアで構造検証した、一時表だけを参照する文。Bind も同じ一時コアで行う。
    Temp(Arc<TempRowStore>),
}

/// 振り分け済みの経路。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    /// 一時表だけを参照する文。一時コアで実行する。
    Temp,
    /// 一時表を参照しないと確認した永続側の文。永続コアで従来どおり実行する（セッション対応の
    /// parse／Bind が、一時カタログが空でない間に作る。陳腐化の検出だけを担う）。
    Persistent,
}

/// セッションの一時カタログに対して振り分け済みの文（TABLE-44・Issue #1684）。
/// `ParsedSql::SessionRouted` が運ぶ。
///
/// 一時表の文を永続コアで実行する [`ParsedSql`] と型で区別するのは、一時表の文が誤って永続 DB へ
/// 作用する fail-open を、網羅 `match` のコンパイル時検査で防ぐため（`CreateTempTable` を別 variant に
/// した #1683 と同じ理由）。`Temp` 経路の `inner` は一時コアで parse した構文形で、永続コアの
/// 実行器へは渡さない。`Persistent` 経路は、振り分け時点で一時表の名前を含まないことを確かめた
/// 永続側の文で、実行時に一時カタログの世代が変わっていれば拒否する（準備済み文が、後から作られた
/// 同名の一時表に黙って別の意味で解決される誤りを防ぐ。PostgreSQL なら再計画する場面）。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRoutedSql {
    inner: ParsedSql,
    /// 振り分け時の一時カタログの世代（[`SessionTempCatalog::epoch`]）。
    epoch: u64,
    /// 参照する一時表の識別子（同名の再作成との区別。`Temp` 経路のみ）。
    relations: Vec<TempRelationId>,
    route: Route,
    /// 振り分け時に一時カタログが未変更（世代 0）だったか。`public.` 修飾つきの準備済み文が、
    /// 一時表を持たないセッションでも世代の検査を受けるための印（世代 0 は通常「未設定」の
    /// 予約値で拒否されるが、この印がある文だけは世代 0 の一致を現行とみなす）。
    fresh: bool,
    /// 包んだ時点で `public` が見えない `search_path` だったときの世代（TABLE-27・Issue #1715）。
    /// `public` が見えないセッションで実行するとき、永続側の文はこの世代が現在の `search_path` の
    /// 世代と一致していなければ陳腐化として拒否する（準備・Bind の後に `SET search_path` された文が、
    /// 古い解決結果のまま実行されない。PostgreSQL なら再計画する場面）。
    path_epoch: Option<u64>,
}

impl SessionRoutedSql {
    fn temp(
        inner: ParsedSql,
        catalog: &SessionTempCatalog,
        relations: Vec<TempRelationId>,
    ) -> Self {
        Self {
            inner,
            epoch: catalog.epoch(),
            relations,
            route: Route::Temp,
            fresh: false,
            path_epoch: None,
        }
    }

    /// 永続側の文を、現在の一時カタログの世代つきで包む。
    pub(crate) fn persistent(inner: ParsedSql, catalog: &SessionTempCatalog) -> Self {
        Self {
            inner,
            epoch: catalog.epoch(),
            relations: Vec::new(),
            route: Route::Persistent,
            fresh: false,
            path_epoch: None,
        }
    }

    /// `public` が見えない `search_path` のセッションで、現在の `search_path` の世代を刻む
    /// （TABLE-27・Issue #1715）。既定の path のセッションでは何もしない。
    pub(crate) fn stamped_with_path(mut self, session: &SessionState) -> Self {
        if session.search_path_hides_public() {
            self.path_epoch = Some(session.search_path_epoch());
        }
        self
    }

    /// `public.` 修飾の表名を持つ準備済み文を、世代つきの永続側の文として包む（TABLE-27・
    /// Issue #1714）。Bind 後〜Execute 前に同名の一時表が作られると、`DROP TABLE` 等の免除文は
    /// 粗いゲートを素通りして一時表へ作用しうるため、世代の変化を実行時に拒否させる。
    pub(crate) fn persistent_guarded(inner: ParsedSql, catalog: &SessionTempCatalog) -> Self {
        Self {
            fresh: catalog.epoch() == 0 && catalog.is_empty(),
            ..Self::persistent(inner, catalog)
        }
    }

    /// 振り分け後の構文形。
    pub(crate) fn inner(&self) -> &ParsedSql {
        &self.inner
    }

    /// 永続側の文か（`false` は一時コアで実行する文）。
    pub(crate) fn is_persistent(&self) -> bool {
        self.route == Route::Persistent
    }

    /// 現在のセッションの一時カタログで、この文の前提（世代・参照する一時表）がまだ有効か。
    fn is_current(&self, catalog: &SessionTempCatalog) -> bool {
        (self.epoch != 0 || self.fresh)
            && self.epoch == catalog.epoch()
            && self.relations.iter().all(|id| {
                catalog
                    .names()
                    .any(|n| catalog.get(n).is_some_and(|e| e.id == *id))
            })
    }

    /// 前提が有効でなければ `0A000`（陳腐化。固定文言で永続側の存否を含まない）。
    pub(crate) fn require_current(&self, session: &SessionState) -> Result<(), SqlSurfaceError> {
        if !self.is_current(session.temp_tables()) {
            return Err(crate::sql::temp_catalog::temp_statement_stale());
        }
        // 永続側の文は、`public` が見えない `search_path` のもとで解決した世代と一致するときだけ通す
        // （一時表の経路の文は一時表だけを参照し path に依らない）。
        if self.route == Route::Persistent
            && session.search_path_hides_public()
            && !is_search_path_exempt(&self.inner)
            && self.path_epoch != Some(session.search_path_epoch())
        {
            return Err(search_path_stale());
        }
        Ok(())
    }
}

/// `search_path` の変更後に陳腐化した文に返す固定の拒否（`0A000`。TABLE-27・Issue #1715）。
/// 名前を判定していないため `42P01` にはせず、再 Parse／Bind での再検証を促す。
pub(crate) fn search_path_stale() -> SqlSurfaceError {
    SqlSurfaceError::feature_not_supported("statement must be re-prepared after search_path change")
}

/// 関係名を引かないため `search_path` の世代検査を免除する文か（トランザクション制御・セッション変数・
/// 一時表の作成・SCHEMA の作成削除・定数式・カーソルの `FETCH`／`CLOSE`・UDF 定義）。
pub(crate) fn is_search_path_exempt(parsed: &ParsedSql) -> bool {
    use crate::sql::cursor::CursorStatement;
    matches!(
        parsed,
        ParsedSql::Transaction(_)
            | ParsedSql::CreateTempTable(_)
            | ParsedSql::CreateSchema(_)
            | ParsedSql::DropSchema(_)
            | ParsedSql::DropFunction(_)
            | ParsedSql::Cursor(CursorStatement::Fetch { .. } | CursorStatement::Close { .. })
            | ParsedSql::Statement(
                Statement::SetSearchMode { .. }
                    | Statement::SetTimeZone { .. }
                    | Statement::ShowTimeZone
                    | Statement::SetSearchPath { .. }
                    | Statement::ShowSearchPath
                    | Statement::SetRowSecurity { .. }
                    | Statement::ShowRowSecurity
                    | Statement::CreateFunction { .. }
                    | Statement::ConstSelect(_)
            )
    )
}

/// 一時コアで実行できる文種か（`operation_id` を持たず、分割実行でもない）。
fn is_routable(parsed: &ParsedSql) -> bool {
    if parsed_operation_id(parsed).is_some() {
        return false;
    }
    match parsed {
        ParsedSql::Insert(_) | ParsedSql::Truncate(_) => true,
        ParsedSql::Delete(DeleteStatement::SingleRow(_)) => true,
        ParsedSql::Delete(DeleteStatement::Predicate(v)) => v.partitioned().is_none(),
        ParsedSql::Update(ValidatedUpdateForm::Single(_)) => true,
        ParsedSql::Update(ValidatedUpdateForm::Predicate(v)) => v.partitioned().is_none(),
        ParsedSql::Statement(
            Statement::Select(_)
            | Statement::Aggregate(_)
            | Statement::Scan(_)
            | Statement::Explain(_)
            | Statement::Join(_)
            | Statement::SetOperation(_),
        ) => true,
        // SQL-38・Issue #1957: 行ロック句付き SELECT は包んだ文が一時コアで実行できる形なら振り分け、
        // 実行段の行ロックゲート（暫定 `0A000`）に通す（振り分け不能の誤分類にしない）。
        ParsedSql::LockingSelect(l) => matches!(
            l.statement(),
            Statement::Select(_)
                | Statement::Aggregate(_)
                | Statement::Scan(_)
                | Statement::Join(_)
        ),
        ParsedSql::CreateIndex(_) | ParsedSql::DropIndex(_) => true,
        // SQL-53 (b)・Issue #1754: 一時表が関与する FROM／USING 形は一時コアの結合実行に未対応（`0A000`）。
        ParsedSql::UpdateFrom(_) | ParsedSql::DeleteUsing(_) => false,
        _ => false,
    }
}

/// 一時コアが書き込みトランザクションを要する文か（読み取り・索引 DDL 以外）。
/// 一時カタログが空でなくてもセッション対応の振り分けを要しない文（トランザクション制御・
/// `CREATE [TEMP] TABLE`・`DROP TABLE`。実行段が自セッションの一時カタログで名前を解決する）。
fn is_session_exempt(parsed: &ParsedSql) -> bool {
    matches!(
        parsed,
        ParsedSql::Transaction(_)
            | ParsedSql::CreateTable(_)
            | ParsedSql::CreateTempTable(_)
            | ParsedSql::DropTable(_)
    )
}

fn is_write(parsed: &ParsedSql) -> bool {
    matches!(
        parsed,
        ParsedSql::Insert(_)
            | ParsedSql::InsertSelect(_)
            // SQL-63・Issue #1876: 振り分け対象外（`is_routable` が false で一時表が絡めば `0A000`）だが、
            // 書き込み文としての分類は他の DML と揃える。
            | ParsedSql::DataModifyingCte(_)
            | ParsedSql::Truncate(_)
            | ParsedSql::Delete(_)
            | ParsedSql::Update(_)
            | ParsedSql::UpdateFrom(_)
            | ParsedSql::DeleteUsing(_)
    )
}

fn internal() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "temporary storage error".to_string(),
    }
}

impl EngineCore {
    /// 一時表専用のコアを作る。`template`（永続コア）の各種上限だけを引き継ぎ、ストレージ・
    /// キャッシュ・embedder・query planner は共有しない（一時 DB だけを見る独立したコア）。
    /// 台帳モードは `CompareOnlyWithoutLedger` 固定（一時表の書き込みは台帳の対象外。TABLE-44 (5)）。
    pub(crate) fn for_temp_session(template: &EngineCore, storage: Storage) -> EngineCore {
        // 一時表は `VECTOR` 列を持てない（`0A000`）ためベクトル検索を実行しない。provider は実行時に
        // スレッドを起こさない単純な実装にする（既定の並列 provider は使わない）。
        let mut core = Self::assemble(storage, Box::new(crate::kernel::CpuScalarProvider), None);
        core.ledger_mode = LedgerMode::CompareOnlyWithoutLedger;
        core.precision_policy = template.precision_policy;
        // `--require-where-for-dml`（Issue #2210）。拡張クエリの Parse は一時コア上で構造検証されるため、
        // 引き継がないと一時表だけ WHERE 省略の UPDATE/DELETE が通る（fail-open）。
        core.require_where_for_dml = template.require_where_for_dml;
        core.batch_limits = template.batch_limits;
        core.transaction_limits = template.transaction_limits;
        core.dml_limits = template.dml_limits;
        core.schema_limits = template.schema_limits;
        core.scan_limits = template.scan_limits;
        core.group_by_limits = template.group_by_limits;
        core.temp_table_limits = template.temp_table_limits;
        core.jsonpath_limits = template.jsonpath_limits;
        core.regex_limits = template.regex_limits;
        core.nesting_limits = template.nesting_limits;
        core.window_limits = template.window_limits;
        core.join_limits = template.join_limits;
        core.result_limits = template.result_limits;
        core.partitioned_dml_limits = template.partitioned_dml_limits;
        core
    }

    /// このコアのストレージ（一時コアでは一時 DB）。`TempRowStore` が DDL 実体の作成・削除に使う。
    pub(crate) fn storage_handle(&self) -> &Storage {
        &self.storage
    }

    /// 一時カタログが空でないセッションで、一時表の名前を含む文を振り分ける
    /// （モジュールドキュメント「振り分け（parse 時）」）。`tokens` は一時表の名前を含むことが
    /// 確認済みの文。
    pub(super) fn route_temp_statement(
        &self,
        tokens: Vec<crate::sql::lexer::Token>,
        session: &SessionState,
        resolver: Option<crate::sql::relation_name::SchemaResolver<'_>>,
    ) -> Result<ParsedSql, SqlSurfaceError> {
        let temp = session.temp_tables();
        let hits = crate::sql::temp_catalog::RelationHits::default();
        // 分類用の parse は台帳モードを緩めて行う（一時表への書き込みは `operation_id` を
        // 要求しないため、永続コアの `Ledgered` で先に `MissingOperationId` にならないように）。
        // この parse 結果は分類にしか使わず、永続コアでは実行しない。
        let classified = self.parse_tokens_in(
            tokens.clone(),
            temp,
            &hits,
            LedgerMode::CompareOnlyWithoutLedger,
            resolver,
            None,
        )?;
        // 索引 DDL はカタログを引かず構造だけを検証するため、`hits` では判定できない。
        // 対象の表が一時表なら一時コアで実行し、そうでなければ（一時表の名前が現れる永続表の索引は）
        // 拒否する。
        if let ParsedSql::CreateIndex(stmt) = &classified {
            if let Some(entry) = temp.get(stmt.table()) {
                let relations = vec![entry.id];
                return Ok(ParsedSql::SessionRouted(Box::new(SessionRoutedSql::temp(
                    classified, temp, relations,
                ))));
            }
            return Err(crate::sql::temp_catalog::temp_table_unsupported());
        }
        if !hits.temp_hit() || hits.persistent_hit() {
            // 一時表の名前が関係として引かれていない（列名との衝突・サブクエリ等）、または
            // 永続の関係と混在している。いずれも一時表を扱えないため拒否する（fail-closed）。
            return Err(crate::sql::temp_catalog::temp_table_unsupported());
        }
        let store = session.temp_rows_arc().ok_or_else(internal)?;
        let core = store.core()?;
        let routed = core.parse_tokens(tokens)?;
        if !is_routable(&routed)
            || matches!(routed, ParsedSql::CreateIndex(_) | ParsedSql::DropIndex(_))
        {
            return Err(crate::sql::temp_catalog::temp_table_unsupported());
        }
        let relations: Vec<TempRelationId> = temp
            .names()
            .filter_map(|n| temp.get(n))
            .map(|e| e.id)
            .collect();
        Ok(ParsedSql::SessionRouted(Box::new(SessionRoutedSql::temp(
            routed, temp, relations,
        ))))
    }

    /// このコア（一時コア）に同名の索引があるか。`search_path` の参照ゲート
    /// （`sql::search_path`）が一時表の索引名を判定するために呼ぶ。照会失敗は `false`（fail-closed）。
    pub(crate) fn has_index_named(&self, name: &str) -> bool {
        self.storage.index_exists(name).unwrap_or(false)
    }

    /// `DROP INDEX` の振り分け。名前が一時 DB の索引なら一時コアで実行し（PostgreSQL が `pg_temp` を
    /// 先に探すのと同じく、同名の永続の索引より優先する）、そうでなければ従来の永続の経路
    /// （DDL 権限ゲートあり）へ回す。
    pub(super) fn route_drop_index(
        &self,
        tokens: Vec<crate::sql::lexer::Token>,
        session: &SessionState,
        resolver: Option<crate::sql::relation_name::SchemaResolver<'_>>,
    ) -> Result<ParsedSql, SqlSurfaceError> {
        let parsed = self.parse_tokens_r(tokens, resolver, None)?;
        if let (ParsedSql::DropIndex(stmt), Some(store)) = (&parsed, session.temp_rows_arc()) {
            let is_temp_index = store
                .core()?
                .storage
                .index_exists(stmt.name())
                .map_err(|_| internal())?;
            if is_temp_index {
                return Ok(ParsedSql::SessionRouted(Box::new(SessionRoutedSql::temp(
                    parsed,
                    session.temp_tables(),
                    Vec::new(),
                ))));
            }
        }
        Ok(parsed)
    }

    /// `COPY` の一時表への振り分け。一時カタログが空、または文が一時表の名前を含まなければ `Ok(None)`
    /// （従来の永続の経路へ進む）。一時表の名前を含む文は、一時 DB のカタログで構造検証し、
    /// 一時表向けなら一時コアで受け入れ（`FROM STDIN` は [`TempCopyTarget`] の印を付ける）、
    /// 一時 DB に無い表（永続表・混在）は `0A000` で拒否する（固定文言）。`USING OPERATION_ID` 付きは
    /// `0A000`（TABLE-44 (5)）。明示トランザクション内の `COPY ... TO` は一時側の書き込み
    /// トランザクションを読み取り源にする（自トランザクションの未 commit の変更が見える）。
    pub(super) fn begin_copy_temp<'e>(
        &self,
        ctx: &PolicyContext,
        session: &SessionState,
        txn: Option<&mut SessionTransaction<'e>>,
        sql: &str,
    ) -> Result<Option<super::CopyPlan>, SqlSurfaceError> {
        let temp = session.temp_tables();
        if temp.is_empty() {
            return Ok(None);
        }
        // 字句解析に失敗する入力は通常の検証へ任せる。
        let Ok(tokens) = crate::sql::lexer::tokenize_marked(sql) else {
            return Ok(None);
        };
        crate::sql::temp_catalog::reject_public_qualified_temp(&tokens, temp)?;
        if !crate::sql::temp_catalog::tokens_reference_temp(&tokens, temp) {
            return Ok(None);
        }
        let store = session.temp_rows_arc().ok_or_else(internal)?;
        store.check_tenant(ctx.tenant_id())?;
        let core = store.core()?;
        let stmt = match crate::sql::allowlist::validate_copy(
            sql,
            &core.storage,
            LedgerMode::CompareOnlyWithoutLedger,
        ) {
            Ok(stmt) => stmt,
            // 一時 DB に無い表（永続表・混在）。永続側の存否を観測させない固定の拒否にする。
            Err(SqlSurfaceError::UndefinedTable { .. }) => {
                return Err(crate::sql::temp_catalog::temp_table_unsupported())
            }
            Err(e) => return Err(e),
        };
        let epoch = temp.epoch();
        match stmt {
            crate::sql::allowlist::CopyStatement::From(v) => {
                // Issue #1934・SQL-38 (3): 読み取り専用 Tx は一時表への `COPY FROM` も拒否する
                // （fail-closed。PostgreSQL と異なり一時表を除外しない）。
                if txn.as_ref().and_then(|t| t.access_mode())
                    == Some(crate::sql::transaction::TxnAccessMode::ReadOnly)
                {
                    return Err(SqlSurfaceError::ReadOnlySqlTransaction);
                }
                if v.operation_id.is_some() {
                    return Err(crate::sql::temp_catalog::temp_table_unsupported());
                }
                let txn_start = txn.as_ref().and_then(|t| t.txn_start_micros());
                let plan = core.begin_copy_validated(
                    ctx,
                    session,
                    crate::sql::allowlist::CopyStatement::From(v),
                    txn_start,
                )?;
                Ok(Some(match plan {
                    super::CopyPlan::From(copy) => super::CopyPlan::From(copy.with_temp(
                        crate::sql::temp_rows::TempCopyTarget {
                            store: Arc::clone(&store),
                            epoch,
                        },
                    )),
                    other => other,
                }))
            }
            crate::sql::allowlist::CopyStatement::To(v) => match txn {
                Some(txn) if txn.is_active() => {
                    // 通常の読み取り文と同じ文実行前チェック（文数・持続時間の上限）。
                    txn.check_and_register_statement(None)?;
                    txn.ensure_temp_txn(&store)?;
                    let write_txn: &redb::WriteTransaction =
                        txn.temp_write_txn().ok_or_else(internal)?;
                    let schema = super::table_schema_in(write_txn, v.inner.table_name())?;
                    let result = core.run_copy_to_scan(write_txn, ctx, session, &schema, &v)?;
                    Ok(Some(super::CopyPlan::To(v.format, result, v.options)))
                }
                _ => Ok(Some(core.begin_copy_validated(
                    ctx,
                    session,
                    crate::sql::allowlist::CopyStatement::To(v),
                    None,
                )?)),
            },
        }
    }

    /// `COPY FROM STDIN` の完了処理（セッション対応版）。`wire-server` の COPY 完了処理が、接続の
    /// [`SessionState`]・[`SessionTransaction`] とともに呼ぶ唯一の入口。
    ///
    /// - 永続表向けのバッチ: [`Self::commit_copy_in_txn`] と同一。自動コミットで成功したときは、
    ///   他の自動コミットの文と同じく一時表の `ON COMMIT` 動作を適用する（#1683 の申し送り）。
    /// - 一時表向けのバッチ（[`TempCopyTarget`] の印付き）: 一時コアが一時 DB へ書き込む。自動コミットは
    ///   一時の書き込みトランザクションの中で実行して総バイト数の上限を判定してから commit し、
    ///   続けて `ON COMMIT` を適用する。明示・暗黙トランザクション内は一時側の書き込みトランザクションに
    ///   書き、失敗時はトランザクションを `Failed` にする。
    pub fn commit_copy_in_txn_in_session<'e>(
        &'e self,
        ctx: &PolicyContext,
        session: &mut SessionState,
        txn: &mut SessionTransaction<'e>,
        batch: crate::sql::copy::CopyInBatch,
    ) -> Result<crate::sql::exec::InsertOutcome, SqlSurfaceError> {
        use crate::sql::transaction::TransactionStatus;
        let Some(target) = batch.temp.clone() else {
            let was_idle = txn.status() == TransactionStatus::Idle;
            // 移行済み表への `id` 省略 COPY（TABLE-24・Issue #1700）は所有 SEQUENCE から採番するため、
            // INSERT 文と同じ採番文脈（アロケータとセッションの `currval` 状態）を張る。
            // 省略しない COPY には張らない（従来の挙動を変えない。セッションなしの公開入口
            // `commit_copy_in_txn` は明示 txn で採番文脈が無く `0A000` で fail-closed になる）。
            let _sequence_scope = batch.bounds.iter().any(|b| b.id_omitted).then(|| {
                crate::sql::sequence_scope::SequenceScope::enter(
                    Arc::clone(self.storage.sequence_allocator()),
                    session.sequences_handle(),
                )
            });
            let outcome = self.commit_copy_in_txn(ctx, txn, batch)?;
            if was_idle {
                session.apply_temp_on_commit()?;
            }
            return Ok(outcome);
        };
        match txn.status() {
            TransactionStatus::Failed => Err(txn.take_failed_error()),
            TransactionStatus::Idle => {
                let outcome = self.commit_temp_copy_autocommit(ctx, session, &target, batch)?;
                session.apply_temp_on_commit()?;
                Ok(outcome)
            }
            TransactionStatus::InTransaction => {
                let result = self.commit_temp_copy_in_txn(ctx, session, txn, &target, batch);
                if result.is_err() {
                    txn.fail();
                }
                result
            }
        }
    }

    /// 一時表向け `COPY` の前提（準備時からの一時カタログの変化・テナント）を確かめる。
    fn check_temp_copy_target(
        &self,
        ctx: &PolicyContext,
        session: &SessionState,
        target: &crate::sql::temp_rows::TempCopyTarget,
    ) -> Result<(), SqlSurfaceError> {
        if session.temp_tables().epoch() != target.epoch || target.epoch == 0 {
            return Err(crate::sql::temp_catalog::temp_statement_stale());
        }
        // セッションの一時表ストアと同一でなければ拒否する（別セッション由来のバッチを通さない）。
        match session.temp_rows_arc() {
            Some(current) if Arc::ptr_eq(&current, &target.store) => {}
            _ => return Err(internal()),
        }
        target.store.check_tenant(ctx.tenant_id())
    }

    fn commit_temp_copy_autocommit(
        &self,
        ctx: &PolicyContext,
        session: &SessionState,
        target: &crate::sql::temp_rows::TempCopyTarget,
        batch: crate::sql::copy::CopyInBatch,
    ) -> Result<crate::sql::exec::InsertOutcome, SqlSurfaceError> {
        self.check_temp_copy_target(ctx, session, target)?;
        let store = &target.store;
        let core = store.core()?;
        let gated = store.begin_write()?;
        let write_txn: &redb::WriteTransaction = &gated;
        let outcome = core.commit_copy_in_with_target(
            crate::tenant::WriteTarget::InTxn(write_txn),
            ctx,
            batch,
        )?;
        store.enforce_budget(write_txn)?;
        store.commit(gated)?;
        Ok(outcome)
    }

    fn commit_temp_copy_in_txn<'e>(
        &self,
        ctx: &PolicyContext,
        session: &SessionState,
        txn: &mut SessionTransaction<'e>,
        target: &crate::sql::temp_rows::TempCopyTarget,
        batch: crate::sql::copy::CopyInBatch,
    ) -> Result<crate::sql::exec::InsertOutcome, SqlSurfaceError> {
        self.check_temp_copy_target(ctx, session, target)?;
        txn.check_and_register_statement(None)?;
        let store = &target.store;
        let core = store.core()?;
        txn.ensure_temp_txn(store)?;
        let write_txn: &redb::WriteTransaction = txn.temp_write_txn().ok_or_else(internal)?;
        let outcome = core.commit_copy_in_with_target(
            crate::tenant::WriteTarget::InTxn(write_txn),
            ctx,
            batch,
        )?;
        store.enforce_budget(write_txn)?;
        Ok(outcome)
    }

    /// 準備済み文を、セッションの一時カタログに照らして検証する（Describe 用）。一時表向けの文は
    /// 世代の検査（[`SessionRoutedSql::require_current`]）、それ以外は一時カタログが空でなければ
    /// 一時表の名前を含まないことの確認（含めば陳腐化＝`0A000`。後から作られた同名の一時表に
    /// 黙って別の意味で解決されない）。トランザクション制御・`CREATE [TEMP] TABLE`・`DROP TABLE` は
    /// 実行段が一時カタログで名前を解決するため通す。
    pub(super) fn check_prepared_for_session(
        session: &SessionState,
        prepared: &super::PreparedSql,
    ) -> Result<(), SqlSurfaceError> {
        match &prepared.route {
            PreparedRoute::Temp(_) => Self::coarse_temp_gate(session, &prepared.dummy_parsed),
            PreparedRoute::Plain => {
                let temp = session.temp_tables();
                // `public` が見えない `search_path` では、一時表以外の未修飾名を `42P01` にする
                // （TABLE-27・Issue #1715。既定の path では何もしない）。
                crate::sql::search_path::check_unqualified_refs(
                    session,
                    &prepared.tokens,
                    &prepared.qualified_positions,
                )?;
                // 免除文（`DROP TABLE` 等）でも、Parse 後に作られた同名の一時表へ `public.` 修飾の
                // 表名が化けないよう先に拒否する（TABLE-27・Issue #1714）。
                crate::sql::temp_catalog::reject_public_names_temp(
                    &prepared.public_qualified,
                    temp,
                )?;
                if temp.is_empty() || is_session_exempt(&prepared.dummy_parsed) {
                    return Ok(());
                }
                if crate::sql::temp_catalog::tokens_reference_temp(&prepared.tokens, temp) {
                    return Err(crate::sql::temp_catalog::temp_statement_stale());
                }
                Ok(())
            }
        }
    }

    /// [`Self::parse_sql_prepared_with_declared`] のセッション対応版（拡張クエリの Parse。TABLE-44・
    /// Issue #1684）。一時カタログが空、または文が一時表の名前を含まなければ従来と同一。一時表の
    /// 名前を含む文は一時コアで構造検証し（一時表だけを参照する DML・読み取り・索引 DDL のみ。
    /// 混在・対象外の文種は `0A000`）、結果を一時表の経路として印付けする。
    pub fn parse_sql_prepared_with_declared_in_session(
        &self,
        session: &SessionState,
        sql: &str,
        declared: &[Option<crate::sql::params::DeclaredParamType>],
    ) -> Result<super::PreparedSql, SqlSurfaceError> {
        let result = self.parse_sql_prepared_in_session_inner(session, sql, declared);
        // TABLE-27・Issue #1715: `public` が見えない `search_path` のセッションは、一時表以外の未修飾の
        // テーブル参照を `42P01` にする。判定順は構文エラー `42601` → 参照ゲート → その他の検証エラー。
        // 既定の path のセッションは何も足さない。
        if !session.search_path_hides_public() {
            return result;
        }
        if matches!(&result, Err(e) if e.wire_code() == "42601") {
            return result;
        }
        if let Ok(tokens) = crate::sql::lexer::tokenize_with_params(sql) {
            crate::sql::search_path::check_unqualified_refs(session, &tokens, &[])?;
        }
        result
    }

    /// [`Self::parse_sql_prepared_with_declared_in_session`] の一時カタログ対応の本体。
    fn parse_sql_prepared_in_session_inner(
        &self,
        session: &SessionState,
        sql: &str,
        declared: &[Option<crate::sql::params::DeclaredParamType>],
    ) -> Result<super::PreparedSql, SqlSurfaceError> {
        let temp = session.temp_tables();
        if temp.is_empty() {
            return self.parse_sql_prepared_for_tenant(sql, declared, session.resolution_tenant());
        }
        let tokens = crate::sql::lexer::tokenize_with_params(sql)?;
        // `public.<一時表の名前>` は解決より前に拒否する（TABLE-27・Issue #1714）。
        crate::sql::temp_catalog::reject_public_qualified_temp(&tokens, temp)?;
        // トランザクション制御・`CREATE [TEMP] TABLE`・`DROP TABLE` は実行段が一時カタログで名前を解決する。
        let txn_control_head = crate::sql::transaction::is_transaction_control_head(&tokens);
        let is_drop_table = matches!(tokens.first(), Some(crate::sql::lexer::Token::Ident(a)) if a.eq_ignore_ascii_case("DROP"))
            && matches!(tokens.get(1), Some(crate::sql::lexer::Token::Ident(b)) if b.eq_ignore_ascii_case("TABLE"));
        if txn_control_head
            || is_drop_table
            // CTAS は内側の問い合わせが一時表を読みうるため免除しない（simple-query 経路と同じ。`0A000`）。
            || (crate::sql::allowlist::is_create_table_statement(&tokens)
                && !crate::sql::allowlist::is_create_table_as_statement(&tokens))
            || crate::sql::allowlist::is_create_temp_table_statement(&tokens)
        {
            return self.parse_sql_prepared_for_tenant(sql, declared, session.resolution_tenant());
        }
        // `DROP INDEX <名前>` は一時表の名前が現れない。一時 DB の索引なら一時コアで検証する。
        if crate::sql::allowlist::is_drop_index_statement(&tokens) {
            let plain =
                self.parse_sql_prepared_for_tenant(sql, declared, session.resolution_tenant())?;
            if let (ParsedSql::DropIndex(stmt), Some(store)) =
                (&plain.dummy_parsed, session.temp_rows_arc())
            {
                let is_temp_index = store
                    .core()?
                    .storage
                    .index_exists(stmt.name())
                    .map_err(|_| internal())?;
                if is_temp_index {
                    let prepared = store
                        .core()?
                        .parse_sql_prepared_with_declared(sql, declared)?;
                    return Self::into_temp_prepared(prepared, store, temp);
                }
            }
            return Ok(plain);
        }
        if !crate::sql::temp_catalog::tokens_reference_temp(&tokens, temp) {
            return self.parse_sql_prepared_for_tenant(sql, declared, session.resolution_tenant());
        }
        let store = session.temp_rows_arc().ok_or_else(internal)?;
        match store
            .core()?
            .parse_sql_prepared_with_declared(sql, declared)
        {
            Ok(prepared) => Self::into_temp_prepared(prepared, store, temp),
            // 一時 DB に無い表（永続表・混在）。永続側の存否を観測させない固定の拒否にする。
            Err(SqlSurfaceError::UndefinedTable { .. }) => {
                Err(crate::sql::temp_catalog::temp_table_unsupported())
            }
            Err(e) => Err(e),
        }
    }

    /// 一時コアで構造検証した準備済み文を、一時表の経路として印付けする。
    fn into_temp_prepared(
        mut prepared: super::PreparedSql,
        store: Arc<TempRowStore>,
        temp: &SessionTempCatalog,
    ) -> Result<super::PreparedSql, SqlSurfaceError> {
        if !is_routable(&prepared.dummy_parsed) {
            return Err(crate::sql::temp_catalog::temp_table_unsupported());
        }
        let relations: Vec<TempRelationId> = temp
            .names()
            .filter_map(|n| temp.get(n))
            .map(|e| e.id)
            .collect();
        let inner = std::mem::replace(
            &mut prepared.dummy_parsed,
            ParsedSql::Statement(Statement::ShowTimeZone),
        );
        prepared.dummy_parsed =
            ParsedSql::SessionRouted(Box::new(SessionRoutedSql::temp(inner, temp, relations)));
        prepared.route = PreparedRoute::Temp(store);
        Ok(prepared)
    }

    /// [`Self::bind_prepared`] のセッション対応版（拡張クエリの Bind。TABLE-44・Issue #1684）。
    ///
    /// - 一時表の経路で準備した文: 準備時から一時カタログが変わっていなければ（世代・参照する一時表・
    ///   セッションの一時表ストア）、一時コアで値を束縛して一時表の経路の [`ParsedSql`] を返す。
    ///   変わっていれば `0A000`。
    /// - それ以外: 従来の [`Self::bind_prepared`]。一時カタログが空でなければ、文が一時表の名前を
    ///   含まないことを確かめてから永続側の文として世代つきで包む（含めば陳腐化＝`0A000`）。
    pub fn bind_prepared_in_session(
        &self,
        session: &SessionState,
        prepared: &super::PreparedSql,
        values: &[Option<Vec<u8>>],
    ) -> Result<ParsedSql, SqlSurfaceError> {
        self.bind_prepared_in_session_snap(session, prepared, values, None)
    }

    /// [`Self::bind_prepared_in_session`] の明示トランザクション対応版（拡張クエリの Bind。Issue #2403）。
    /// writer 取得前のトランザクションでは、永続側の再 parse（検証・ビュー展開）を `BEGIN` 時点の
    /// スナップショットのカタログで行い、簡易クエリ経路と検証の参照時点を揃える。
    pub fn bind_prepared_in_txn(
        &self,
        session: &SessionState,
        txn: &crate::sql::transaction::SessionTransaction<'_>,
        prepared: &super::PreparedSql,
        values: &[Option<Vec<u8>>],
    ) -> Result<ParsedSql, SqlSurfaceError> {
        let read_snap = if txn.status() == crate::sql::transaction::TransactionStatus::InTransaction
        {
            match txn.read_source()? {
                crate::sql::transaction::TxnReadSource::BeginSnapshot(snap) => Some(snap),
                _ => None,
            }
        } else {
            None
        };
        self.bind_prepared_in_session_snap(session, prepared, values, read_snap)
    }

    /// 明示トランザクションが writer 取得前（`BEGIN` 時点のスナップショットで読む）か。
    /// wire 層が、Parse 時の解決結果を使い回さず Bind で再検証すべきかの判定に使う（Issue #2403）。
    pub fn txn_reads_begin_snapshot(txn: &crate::sql::transaction::SessionTransaction<'_>) -> bool {
        txn.status() == crate::sql::transaction::TransactionStatus::InTransaction
            && matches!(
                txn.read_source(),
                Ok(crate::sql::transaction::TxnReadSource::BeginSnapshot(_))
            )
    }

    fn bind_prepared_in_session_snap(
        &self,
        session: &SessionState,
        prepared: &super::PreparedSql,
        values: &[Option<Vec<u8>>],
        read_snap: Option<&redb::ReadTransaction>,
    ) -> Result<ParsedSql, SqlSurfaceError> {
        let temp = session.temp_tables();
        match &prepared.route {
            PreparedRoute::Temp(store) => {
                let ParsedSql::SessionRouted(prepared_routed) = &prepared.dummy_parsed else {
                    return Err(internal());
                };
                prepared_routed.require_current(session)?;
                match session.temp_rows_arc() {
                    Some(current) if Arc::ptr_eq(&current, store) => {}
                    _ => return Err(internal()),
                }
                let inner = store.core()?.bind_prepared(prepared, values)?;
                if !is_routable(&inner) {
                    return Err(crate::sql::temp_catalog::temp_table_unsupported());
                }
                Ok(ParsedSql::SessionRouted(Box::new(SessionRoutedSql {
                    inner,
                    epoch: prepared_routed.epoch,
                    relations: prepared_routed.relations.clone(),
                    route: Route::Temp,
                    fresh: false,
                    path_epoch: None,
                })))
            }
            PreparedRoute::Plain => {
                crate::sql::temp_catalog::reject_public_names_temp(
                    &prepared.public_qualified,
                    temp,
                )?;
                // `public` が見えない `search_path` では、Parse 後に変わった path でも一時表以外の
                // 未修飾名を `42P01` にする（TABLE-27・Issue #1715。既定の path では何もしない）。
                crate::sql::search_path::check_unqualified_refs(
                    session,
                    &prepared.tokens,
                    &prepared.qualified_positions,
                )?;
                let parsed = self.bind_prepared_snap(prepared, values, read_snap)?;
                // `COPY`（Issue #1760）は Execute で元 SQL を現在のカタログへ再解決する
                // （`begin_copy_temp`）ため、Bind 後に一時表が作成・削除されると宛先が永続表から
                // 一時表（または逆）へ化ける。一時カタログの世代つきで包み、Execute 前の
                // `require_current` で陳腐化を `0A000` として拒否させる（fail-closed）。
                if matches!(parsed, ParsedSql::Copy(_)) {
                    // Parse 後に同名の一時表が作られていれば、他の文と同じく Bind 時点で陳腐化として
                    // 拒否する（current と刻印して Execute で一時表へ化けさせない。fail-closed）。
                    if crate::sql::temp_catalog::tokens_reference_temp(&prepared.tokens, temp) {
                        return Err(crate::sql::temp_catalog::temp_statement_stale());
                    }
                    return Ok(ParsedSql::SessionRouted(Box::new(
                        SessionRoutedSql::persistent_guarded(parsed, temp)
                            .stamped_with_path(session),
                    )));
                }
                // `DROP INDEX <名前>` は一時表の名前トークンが現れない。準備後に同名の一時索引が
                // 作られていれば、通常の Parse（`route_drop_index`）なら一時索引に当たるため、
                // 永続の索引を黙って削除せず陳腐化として拒否する（fail-closed）。
                if let (ParsedSql::DropIndex(stmt), Some(store)) =
                    (&parsed, session.temp_rows_arc())
                {
                    let is_temp_index = store
                        .core()?
                        .storage
                        .index_exists(stmt.name())
                        .map_err(|_| internal())?;
                    if is_temp_index {
                        return Err(crate::sql::temp_catalog::temp_statement_stale());
                    }
                }
                if temp.is_empty() || is_session_exempt(&parsed) {
                    if prepared.public_qualified.is_empty() && !session.search_path_hides_public() {
                        return Ok(parsed);
                    }
                    // `public.` 修飾つきの文は、実行までに同名の一時表が作られても一時表へ
                    // 化けないよう世代を持たせる（Execute の粗いゲートが検査する）。`public` が
                    // 見えない `search_path` では、path の世代も刻む（Bind 後の `SET search_path` で
                    // 陳腐化した文を実行しない。TABLE-27・Issue #1715）。
                    return Ok(ParsedSql::SessionRouted(Box::new(
                        SessionRoutedSql::persistent_guarded(parsed, temp)
                            .stamped_with_path(session),
                    )));
                }
                if crate::sql::temp_catalog::tokens_reference_temp(&prepared.tokens, temp) {
                    return Err(crate::sql::temp_catalog::temp_statement_stale());
                }
                Ok(ParsedSql::SessionRouted(Box::new(
                    SessionRoutedSql::persistent(parsed, temp).stamped_with_path(session),
                )))
            }
        }
    }

    /// 振り分け済みの文の前提（世代・テナント）を確かめ、一時表ストアを返す。
    fn temp_store_for(
        &self,
        ctx: &PolicyContext,
        session: &SessionState,
        routed: &SessionRoutedSql,
    ) -> Result<Arc<TempRowStore>, SqlSurfaceError> {
        if !routed.is_current(session.temp_tables()) {
            return Err(crate::sql::temp_catalog::temp_statement_stale());
        }
        let store = session.temp_rows_arc().ok_or_else(internal)?;
        store.check_tenant(ctx.tenant_id())?;
        Ok(store)
    }

    /// 自動コミットでの一時表の文の実行（`execute_parsed_statement` の `SessionRouted` 腕）。
    pub(super) fn execute_temp_routed_autocommit(
        &self,
        ctx: &PolicyContext,
        session: &mut SessionState,
        routed: &SessionRoutedSql,
    ) -> Result<crate::sql::SqlOutcome, SqlSurfaceError> {
        let store = self.temp_store_for(ctx, session, routed)?;
        let core = store.core()?;
        let inner = routed.inner();
        match inner {
            ParsedSql::CreateIndex(stmt) => {
                // `IF NOT EXISTS` で既存索引に当たる no-op を区別する（戻す対象は今回作った索引だけ）。
                let existed = core
                    .storage
                    .relation_name_exists(stmt.name())
                    .map_err(|_| internal())?;
                let outcome = crate::sql::ddl::execute_create_index(&core.storage, stmt)?;
                // 確定後に容量上限を検査し、超過なら今回作った索引を戻して `54000`（DDL も上限を
                // 迂回させない）。既存索引への no-op 成功では何も戻さない。
                if !existed {
                    if let Err(e) = store.enforce_budget_after_ddl() {
                        let _ = core.storage.drop_index(stmt.name());
                        return Err(e);
                    }
                }
                session.temp_tables_mut().bump_epoch()?;
                Ok(crate::sql::SqlOutcome::CreateIndex(outcome))
            }
            ParsedSql::DropIndex(stmt) => {
                let outcome = crate::sql::ddl::execute_drop_index(&core.storage, stmt)?;
                session.temp_tables_mut().bump_epoch()?;
                Ok(crate::sql::SqlOutcome::DropIndex(outcome))
            }
            // SQL-38・Issue #1957: 行ロック句付き SELECT は永続側と同じ暫定ゲート（ロックを取らずに
            // 素の SELECT として実行しない）。
            ParsedSql::LockingSelect(_) => Err(super::row_lock_not_supported()),
            ParsedSql::Statement(stmt) => {
                core.execute_validated_in_session(ctx, session, stmt.clone())
            }
            _ if is_write(inner) => {
                let gated = store.begin_write()?;
                // `Err` を返すと `gated` が drop され abort する（部分書き込みを残さない）。
                let outcome = core.temp_dispatch(ctx, session, &gated, inner)?;
                store.enforce_budget(&gated)?;
                store.commit(gated)?;
                Ok(outcome)
            }
            _ => Err(internal()),
        }
    }

    /// 明示・暗黙トランザクション内での一時表の文の実行（`execute_in_active_txn` の `SessionRouted` 腕）。
    pub(super) fn execute_temp_routed_in_txn<'e>(
        &self,
        ctx: &PolicyContext,
        session: &mut SessionState,
        txn: &mut SessionTransaction<'e>,
        routed: &SessionRoutedSql,
    ) -> Result<crate::sql::SqlOutcome, SqlSurfaceError> {
        let inner = routed.inner();
        if matches!(inner, ParsedSql::CreateIndex(_) | ParsedSql::DropIndex(_)) {
            // 永続側の DDL と同じく、明示トランザクション内の DDL は対象外。
            return Err(SqlSurfaceError::transaction_feature_not_supported(
                "this statement is not supported inside an explicit transaction",
            ));
        }
        // 一時表はセッションローカルで他セッションの commit の影響を受けないため、この区間の読み取りは
        // 明示トランザクションの読み取り集合へ記録しない（TABLE-44・SQL-38・Issue #2404）。
        let _read_set_suspend = crate::sql::read_set::SuspendGuard::enter();
        let store = self.temp_store_for(ctx, session, routed)?;
        let core = store.core()?;
        txn.ensure_temp_txn(&store)?;
        let gated = txn.temp_write_txn().ok_or_else(internal)?;
        let outcome = core.temp_dispatch(ctx, session, gated, inner)?;
        if is_write(inner) {
            store.enforce_budget(gated)?;
        }
        Ok(outcome)
    }

    /// Describe（結果列メタデータの導出）。一時コアの導出本体へ委譲する。
    pub(super) fn describe_temp_routed(
        &self,
        session: &SessionState,
        routed: &SessionRoutedSql,
        skip_vector_literal_validation: bool,
        dummy_equality_flags: &[bool],
        limits: &crate::result_limits::ResultLimits,
    ) -> Result<Option<Vec<crate::sql::exec::ColumnMeta>>, SqlSurfaceError> {
        if !routed.is_current(session.temp_tables()) {
            return Err(crate::sql::temp_catalog::temp_statement_stale());
        }
        let store = session.temp_rows_arc().ok_or_else(internal)?;
        store.core()?.describe_parsed_in_session_impl(
            session,
            routed.inner(),
            skip_vector_literal_validation,
            dummy_equality_flags,
            limits,
        )
    }

    /// 一時コア（`self`）で、開いている一時の書き込みトランザクション `gated` の上に 1 文を
    /// 実行する。`self.storage` は一時 DB のため、フォーム関数（永続表と共通）が引くスキーマ・
    /// 行・索引・世代はすべて一時 DB のものになる。commit／abort は呼び出し元が行う。
    /// 時刻関数のスコープは呼び出し元（`execute_parsed_statement`／`execute_in_active_txn`）が
    /// 文頭で張り済み（`SessionRouted` の中身を見て決める）。
    fn temp_dispatch(
        &self,
        ctx: &PolicyContext,
        session: &mut SessionState,
        gated: &GatedWriteTxn,
        inner: &ParsedSql,
    ) -> Result<crate::sql::SqlOutcome, SqlSurfaceError> {
        let write_txn: &redb::WriteTransaction = gated;
        let target = crate::tenant::WriteTarget::InTxn(write_txn);
        // 外側（永続コア）が張った SEQUENCE の採番文脈を一時表の書き込みへ持ち込まない
        // （一時 DB は永続の SEQUENCE を解決できず、カーソルを一時 DB へ書いてしまうため。TABLE-24）。
        let _no_sequence_scope = crate::sql::sequence_scope::SequenceScope::suspend();
        match inner {
            ParsedSql::Insert(stmt) => {
                let lookup = InsertSchemaLookup::new(&self.storage);
                if stmt.returning.is_some() {
                    Ok(crate::sql::SqlOutcome::Returning(
                        self.execute_insert_returning_form(target, ctx, stmt, &lookup)?,
                    ))
                } else {
                    Ok(crate::sql::SqlOutcome::Insert(
                        self.execute_insert_form(target, ctx, stmt, &lookup)?,
                    ))
                }
            }
            ParsedSql::Delete(DeleteStatement::SingleRow(v)) => {
                if v.returning.is_some() {
                    Ok(crate::sql::SqlOutcome::Returning(
                        self.execute_delete_returning_form(target, ctx, v)?,
                    ))
                } else {
                    Ok(crate::sql::SqlOutcome::Delete(
                        self.execute_delete_form(target, ctx, v)?,
                    ))
                }
            }
            ParsedSql::Delete(DeleteStatement::Predicate(v)) => {
                if v.returning().is_some() {
                    Ok(crate::sql::SqlOutcome::Returning(
                        self.execute_predicate_delete_returning_form(target, ctx, session, v)?,
                    ))
                } else {
                    Ok(crate::sql::SqlOutcome::Delete(
                        self.execute_predicate_delete_form(target, ctx, session, v)?,
                    ))
                }
            }
            ParsedSql::Update(stmt) => {
                let has_returning = match stmt {
                    ValidatedUpdateForm::Single(v) => v.returning.is_some(),
                    ValidatedUpdateForm::Predicate(v) => v.returning().is_some(),
                };
                if has_returning {
                    Ok(crate::sql::SqlOutcome::Returning(
                        self.execute_update_returning_form(target, ctx, session, stmt)?,
                    ))
                } else {
                    Ok(crate::sql::SqlOutcome::Update(
                        self.execute_predicate_update_form(target, ctx, session, stmt)?,
                    ))
                }
            }
            ParsedSql::Truncate(stmt) => Ok(crate::sql::SqlOutcome::Truncate(
                crate::sql::exec::execute_truncate_in(
                    target,
                    ctx,
                    stmt,
                    self.ledger_mode,
                    Some(&self.trigger_runtime),
                )?,
            )),
            // 読み取り系は開いている書き込みトランザクションを読み取り源にする（自トランザクションの
            // 未 commit の変更を読める。キャッシュは使われない brute-force 経路）。
            ParsedSql::LockingSelect(_) => Err(super::row_lock_not_supported()),
            ParsedSql::Statement(stmt) => match stmt {
                Statement::Select(v) if v.using_plan().is_some() => {
                    self.execute_validated_with_dict(ctx, session, stmt.clone(), Some(write_txn))
                }
                Statement::Explain(_) => {
                    self.execute_validated_with_dict(ctx, session, stmt.clone(), Some(write_txn))
                }
                Statement::Select(_)
                | Statement::Aggregate(_)
                | Statement::Scan(_)
                | Statement::SetOperation(_)
                | Statement::Join(_) => {
                    self.execute_read_statement(ctx, session, stmt.clone(), write_txn)
                }
                _ => Err(internal()),
            },
            _ => Err(internal()),
        }
    }
}
