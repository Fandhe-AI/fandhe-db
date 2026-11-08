//! 読み取り系の文（SELECT・カーソルの DECLARE・EXPLAIN・検索）の SELECT 権限検査（SQL-40・RLS-12 ポインタ。
//! Issue #1967。設計は `docs/design/role-privilege-model.md` §12、経路一覧は
//! `docs/design/privilege-check-paths.md`）。
//!
//! 役割: 文が参照する relation（表・ビュー・matview）を集め、ACL を引いて共通ゲート
//! [`super::check`] へ渡す。判定そのもの（未束縛の素通し・運用者権限・ビット不足の `42501`）は
//! `privilege_gate.rs` だけが持ち、本ファイルは「何を・どの権限で・どの文種に」だけを決める。
//!
//! 呼び出し元: `core::EngineCore` の `execute_sql_in_session`・`execute_sql_in_txn`（構文解析の成功直後）、
//! `core/plpgsql_host`（`parse_tokens_gated` 経由）、`core/temp_route` の拡張クエリ Parse・Bind・Describe、
//! HTTP 表層の bound 読み取り API（`EngineCore::require_table_select_privilege`）。
//!
//! 契約:
//! - 現在ロール未束縛、または運用者権限のセッションは、トークン走査も ACL の照会もしない（従来と同一）。
//! - 判定の順序は「構文 `42601` → 名前解決 `42P01`／`3F000` → 権限 `42501`」。参照先の収集は構文解析が
//!   成功した後に、名前解決器で修飾を解決したトークン列に対して行う（他テナントの SCHEMA 配下の表は解決の
//!   段で不在と同じ応答になり、権限の分岐に入らない）。
//! - ビュー・matview は、利用者に対してはそれ自身の SELECT だけを判定する（基底表はビュー所有者の権限で
//!   読む扱い。ビューを作れるのは所有者相当の主体だけ。ADR §12）。
//! - カタログで relation として引けない名前（誤分類された識別子）は検査しない。ただし構文解析が
//!   カタログで実在を確認した名前（`RelationHits` の記録。ビュー展開前のビュー自身を含む）が ACL 照会の
//!   時点で見つからない場合は、解析後に DROP された参照先として拒否する（通常 SQL・準備済み文・明示 Tx の
//!   どの経路でも同じ）。余分な検査は拒否側にだけ倒れる。
//! - 参照数の上限を ACL の照会より前に検査する（untrusted 入力）。
//! - エラーは固定文言の `42501`。表名・ロール ID・テナントを含めない。
//! - 列単位の付与（Issue #2083。`columns.rs`）: 表単位のビットで満たせない `SELECT` だけを列で判定する。
//!   単一 relation の `SELECT`／集約／広域取得の形（[`ReadRequirement::SelectPrecise`]）は、文が名指しする列
//!   （上位近似）への列単位の付与で満たせる。それ以外の読み取り（JOIN・サブクエリ・CTE・集合演算・`*` 等）は
//!   全列、ビューは表単位、行ロック句の `UPDATE` は「いずれかの列」を要求する。

use crate::catalog::privilege::{
    relation_grants_in_txn, relation_shape_in_txn, RelationShape, PRIV_SELECT, PRIV_UPDATE,
};
use crate::core::ParsedSql;
use crate::sql::allowlist::{SqlSurfaceError, Statement};
use crate::sql::cursor::CursorStatement;
use crate::sql::lexer::Token;
use crate::sql::mode::SessionState;
use crate::sql::relation_name::SchemaResolver;
use crate::storage::Storage;

use super::columns::{self, ColumnSpec, Want};
use super::{check, Principal, PrivilegeTarget};

/// 1 文が参照できる relation 名の上限（重複除去後。ACL の照会より前に検査する）。
pub(crate) const MAX_READ_RELATION_REFS: usize = 256;

/// 文が要求する読み取り側の権限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadRequirement {
    /// 本ゲートの対象外（DML・COPY・DDL・セッション制御等。DML の権限は `write.rs`、COPY は `copy.rs`〔#1969〕）。
    NotApplicable,
    /// 参照するすべての relation に SELECT（列単位の付与では全列を要求する）。
    Select,
    /// [`Self::Select`] のうち、単一 relation の `SELECT`／集約／広域取得の形。名指しした列への列単位の
    /// 付与で満たせる（`columns.rs` の上位近似。relation が複数なら全列）。
    SelectPrecise,
    /// 行ロック句つきの SELECT。SELECT に加えて UPDATE を要求する（PostgreSQL と同じく、ロック強度に
    /// よらず UPDATE 系の権限が要る。`OF` で絞られた表だけでなく参照先の全表に要求する側へ倒す）。
    SelectForUpdate,
}

impl ReadRequirement {
    fn bits(self) -> Option<u16> {
        match self {
            Self::NotApplicable => None,
            Self::Select | Self::SelectPrecise => Some(PRIV_SELECT),
            Self::SelectForUpdate => Some(PRIV_SELECT | PRIV_UPDATE),
        }
    }
}

/// 検証済みの文を読み取りの要求に分類する。ワイルドカード腕を置かない網羅 `match` で、`ParsedSql`・
/// `Statement` に variant が足されたときに分類の更新（読み取りか否か）をコンパイラが強制する。
pub(crate) fn classify(parsed: &ParsedSql) -> ReadRequirement {
    use ReadRequirement::{NotApplicable, Select, SelectForUpdate};
    match parsed {
        ParsedSql::Statement(s) => classify_statement(s),
        ParsedSql::LockingSelect(_) => SelectForUpdate,
        // Issue #2059: 包んだ `EXPLAIN` は中身と同じ検査を受ける（形式の違いで権限を緩めない）。
        ParsedSql::ExplainJson(inner) => classify(inner),
        // Issue #2054: 実行する内側の SELECT と同じく、参照する全 relation に SELECT 権限を実行前に要求する。
        ParsedSql::ExplainAnalyze(_) => Select,
        // Issue #2060: 中身と同じ検査（形式の違いで権限を緩めない）。
        ParsedSql::ExplainVerbose(inner) => classify(inner),
        ParsedSql::Cursor(c) => match c {
            CursorStatement::Declare { .. } => Select,
            // FETCH／CLOSE は DECLARE の時点で確定した結果を読むだけで、再検査しない（`sql-cursor.md` §1）。
            CursorStatement::Fetch { .. } | CursorStatement::Close { .. } => NotApplicable,
        },
        // 永続側の文はセッション経由で包まれていても同じ検査を受ける。一時表だけを読む文はセッションの
        // 所有物なので検査しない（混在は振り分けの段で `0A000`）。
        ParsedSql::SessionRouted(r) => {
            if r.is_persistent() {
                classify(r.inner())
            } else {
                NotApplicable
            }
        }
        // DML の読み取り側（SELECT 元・FROM／USING・サブクエリ）と書き込みの権限は `write.rs`（#1968）が
        // 本モジュールの収集・照会関数を再利用して判定する。COPY は `copy.rs`〔#1969〕。
        ParsedSql::Insert(_)
        | ParsedSql::InsertSelect(_)
        | ParsedSql::DataModifyingCte(_)
        | ParsedSql::ViewInsert(_)
        | ParsedSql::ViewDelete(_)
        | ParsedSql::ViewUpdate(_)
        | ParsedSql::Truncate(_)
        | ParsedSql::Delete(_)
        | ParsedSql::Update(_)
        | ParsedSql::UpdateFrom(_)
        | ParsedSql::DeleteUsing(_)
        | ParsedSql::Partitioned(_)
        // Issue #2051: INSERT 権限と元表の SELECT は書き込みゲート（`write.rs`）が判定する。
        | ParsedSql::ExplainInsert(_)
        // Issue #2050: 行を読まない。DELETE／UPDATE 権限は write gate が実行時と同じく要求する。
        | ParsedSql::ExplainDml(_)
        // Issue #2055: 実行する DML の権限（SELECT を含む）は write ゲートが判定する。
        | ParsedSql::ExplainAnalyzeDml(_)
        | ParsedSql::Copy(_)
        | ParsedSql::Lock(_) => NotApplicable,
        // トランザクション・セッション制御・DDL・関数呼び出し。DDL は運用者権限のゲートを通った主体だけが
        // 実行でき、CTAS・CREATE VIEW の読み取り元も所有者相当として扱う。DO の埋め込み SQL は
        // `plpgsql_host` が `parse_tokens_gated` を通るため、その文ごとに分類される。
        ParsedSql::Transaction(_)
        | ParsedSql::SetTransaction(_)
        | ParsedSql::DropTable(_)
        | ParsedSql::CreateTable(_)
        | ParsedSql::CreateTempTable(_)
        | ParsedSql::AlterTable(_)
        | ParsedSql::CreateView(_)
        | ParsedSql::DropView(_)
        | ParsedSql::DropMaterializedView(_)
        | ParsedSql::CreateMaterializedView(_)
        | ParsedSql::CreateTableAs(_)
        | ParsedSql::RefreshMaterializedView(_)
        | ParsedSql::CreateIndex(_)
        | ParsedSql::DropIndex(_)
        | ParsedSql::CreateType(_)
        | ParsedSql::DropType(_)
        | ParsedSql::CreateSequence(_)
        | ParsedSql::DropSequence(_)
        | ParsedSql::DropFunction(_)
        | ParsedSql::CreateTrigger(_)
        | ParsedSql::DropTrigger(_)
        | ParsedSql::CreatePolicy(_)
        | ParsedSql::AlterPolicy(_)
        | ParsedSql::DropPolicy(_)
        | ParsedSql::GrantRevoke(_)
        | ParsedSql::AlterDefaultPrivileges(_)
        | ParsedSql::CreateRole(_)
        | ParsedSql::DropRole(_)
        | ParsedSql::AlterRole(_)
        | ParsedSql::SetRole(_)
        | ParsedSql::Do(_)
        | ParsedSql::CreateSchema(_)
        | ParsedSql::DropSchema(_)
        | ParsedSql::AlterSchema(_)
        | ParsedSql::AlterOwner(_)
        | ParsedSql::AlterSequence(_)
        | ParsedSql::SequenceFunction(_) => NotApplicable,
    }
}

fn classify_statement(s: &Statement) -> ReadRequirement {
    match s {
        // 単一 relation の基本形。列の名指しは上位近似で精密化できる（relation が複数なら全列）。
        Statement::Select(_) | Statement::Aggregate(_) | Statement::Scan(_) => {
            ReadRequirement::SelectPrecise
        }
        Statement::Explain(_)
        | Statement::SetOperation(_)
        | Statement::Join(_)
        | Statement::BufferedView(_)
        | Statement::ConstSelect(_)
        | Statement::MaterializedView(_)
        | Statement::RecursiveCte(_)
        | Statement::FunctionScan(_) => ReadRequirement::Select,
        Statement::SetSearchMode { .. }
        | Statement::SetTimeZone { .. }
        | Statement::ShowTimeZone
        | Statement::SetSearchPath { .. }
        | Statement::ShowSearchPath
        | Statement::SetRowSecurity { .. }
        | Statement::ShowRowSecurity
        | Statement::ShowRestoreGeneration
        | Statement::SetStatementTimeout { .. }
        | Statement::ShowStatementTimeout
        | Statement::SetHnswEfSearch { .. }
        | Statement::ShowHnswEfSearch
        | Statement::SetHnswScanSetting { .. }
        | Statement::ShowHnswScanSetting(_)
        | Statement::CreateFunction { .. } => ReadRequirement::NotApplicable,
    }
}

/// 文が参照する relation に `requirement` の権限があるかを判定する（権限不足は `42501`）。
///
/// `tokens` は構文解析へ渡した字句列（名前解決前。`resolver` で修飾を解決してから走査する）。呼び出し元は
/// 構文解析の成功後に呼ぶ。現在ロール未束縛・運用者権限のセッションは何もしない。
pub(crate) fn require_read_privileges(
    storage: &Storage,
    tenant: &str,
    session: &SessionState,
    tokens: &[Token],
    resolver: Option<SchemaResolver<'_>>,
    requirement: ReadRequirement,
    snapshot: ParsedScope<'_>,
) -> Result<(), SqlSurfaceError> {
    let Some(required_bits) = requirement.bits() else {
        return Ok(());
    };
    let principal = Principal::from_session(session);
    if !principal.needs_acl_lookup() {
        // 結果が ACL に依存しない主体（未束縛・運用者権限）。`Some(0)` の拒否だけがここで決まる。
        return check(
            &principal,
            PrivilegeTarget::Relation {
                required_bits,
                granted_bits: 0,
            },
        )
        .map(|_| ());
    }
    let mut resolved = tokens.to_vec();
    crate::sql::relation_name::rewrite_relation_names_full(&mut resolved, resolver)?;
    let names = gate_relation_names(&resolved, snapshot.parsed_relations);
    require_relations_bits(
        storage,
        tenant,
        &principal,
        &read_wants(requirement, &names),
        &resolved,
        snapshot.read_snap,
        snapshot.parsed_relations,
    )
}

/// [`require_read_privileges`] の、名前解決済みのトークン列を受け取る版（拡張クエリの Parse・Bind・
/// Describe。`PreparedSql` は Parse 時に修飾を解決したトークン列を保持している）。
pub(crate) fn require_read_privileges_resolved(
    storage: &Storage,
    tenant: &str,
    session: &SessionState,
    resolved_tokens: &[Token],
    requirement: ReadRequirement,
    scope: ParsedScope<'_>,
) -> Result<(), SqlSurfaceError> {
    let Some(required_bits) = requirement.bits() else {
        return Ok(());
    };
    let principal = Principal::from_session(session);
    if !principal.needs_acl_lookup() {
        return check(
            &principal,
            PrivilegeTarget::Relation {
                required_bits,
                granted_bits: 0,
            },
        )
        .map(|_| ());
    }
    let names = gate_relation_names(resolved_tokens, scope.parsed_relations);
    require_relations_bits(
        storage,
        tenant,
        &principal,
        &read_wants(requirement, &names),
        resolved_tokens,
        scope.read_snap,
        scope.parsed_relations,
    )
}

/// 読み取りの要求を relation ごとの [`Want`] へ展開する。
fn read_wants(requirement: ReadRequirement, names: &[String]) -> Vec<Want> {
    let mut out = Vec::new();
    for n in names {
        match requirement {
            ReadRequirement::NotApplicable => {}
            ReadRequirement::Select => out.extend(Want::all(n, PRIV_SELECT)),
            ReadRequirement::SelectPrecise => {
                out.extend(Want::split(n, PRIV_SELECT, ColumnSpec::Scan))
            }
            ReadRequirement::SelectForUpdate => {
                out.extend(Want::all(n, PRIV_SELECT));
                // 行ロックは書き込み系の権限。列の指定を持たないので「いずれかの列」への UPDATE で足りる側に
                // する（PostgreSQL と同じ。SELECT 側が全列を要求するので読める範囲は広げない）。
                out.extend(Want::split(n, PRIV_UPDATE, ColumnSpec::Any));
            }
        }
    }
    out
}

/// 通常 SQL の構文解析の結果として権限ゲートへ渡す、参照先の確認の文脈。
#[derive(Debug, Clone, Copy)]
pub(crate) struct ParsedScope<'a> {
    /// 明示 Tx の読み取り源（`BEGIN` 時点のスナップショット）。無ければ最新の確定カタログ。
    pub(crate) read_snap: Option<&'a redb::ReadTransaction>,
    /// 構文解析がカタログで実在を確認した relation 名（ビュー展開前のビュー自身を含む）。
    pub(crate) parsed_relations: &'a [String],
}

/// 権限検査の対象にする relation 名の合成（トークン収集器の候補 ∪ 解析が外側の文で直接解決した名前）。
/// 収集器は字句の推測で構文語・別名を除くため取りこぼし（偽陰性。実表 `"if"`・`"only"` を構文語と誤認する
/// 等）がありうる。解析（`RelationHits`）が深さ 0 で relation として解決した名前は、収集器が除外したかに
/// かかわらず必ず加える。ビューの展開中に引かれた基底表は記録の段で除かれており（ビュー所有者の権限で読む
/// 扱い。ADR §12）、基底表と同名の別名・列名があっても検査対象に戻らない。
pub(super) fn gate_relation_names(
    resolved_tokens: &[Token],
    direct_relations: &[String],
) -> Vec<String> {
    let mut names = crate::sql::search_path::resolved_relation_ref_names(resolved_tokens);
    for rel in direct_relations {
        if !names.contains(rel) {
            names.push(rel.clone());
        }
    }
    names
}

/// 解決済みの relation 名の並びに対する判定（全列を要求する版。HTTP 表層の入口とテストが共有する）。
/// 全名に同じ `required_bits` を課す。
pub(super) fn require_relations(
    storage: &Storage,
    tenant: &str,
    principal: &Principal,
    required_bits: u16,
    candidates: &[String],
    read_snap: Option<&redb::ReadTransaction>,
    parsed_relations: &[String],
) -> Result<(), SqlSurfaceError> {
    let wants: Vec<Want> = candidates
        .iter()
        .flat_map(|n| Want::all(n, required_bits))
        .collect();
    require_relations_bits(
        storage,
        tenant,
        principal,
        &wants,
        &[],
        read_snap,
        parsed_relations,
    )
}

/// relation ごと・権限ビットごとに列の仕様が異なる版の判定（#1968 の書き込み系・COPY ゲートと共有する
/// 唯一の ACL 照会点）。relation 名の種類数の上限と列名の数の上限を ACL の照会より前に検査する（untrusted 入力）。
/// `tokens` は `Scan` の列集合に使う名前解決済みの字句列（持たない入口は空）。
pub(super) fn require_relations_bits(
    storage: &Storage,
    tenant: &str,
    principal: &Principal,
    wants: &[Want],
    tokens: &[Token],
    read_snap: Option<&redb::ReadTransaction>,
    parsed_relations: &[String],
) -> Result<(), SqlSurfaceError> {
    let mut names: Vec<String> = Vec::new();
    for w in wants {
        if !names.contains(&w.relation) {
            names.push(w.relation.clone());
        }
        if names.len() > MAX_READ_RELATION_REFS {
            return Err(SqlSurfaceError::payload_too_large(
                "too many relation references",
            ));
        }
    }
    if names.is_empty() {
        return Ok(());
    }
    columns::check_explicit_count(wants)?;
    // 明示 Tx（BEGIN 時のスナップショットで解析・実行する経路）では、ACL と relation の存在確認も同じ
    // スナップショットで引く。最新カタログで引くと、BEGIN 後に別セッションが DROP した表が `None`
    // （存在しない扱い）になり、旧スナップショットで読めてしまう認可バイパスになる。
    match read_snap {
        Some(snap) => evaluate_in_txn(
            principal,
            snap,
            tenant,
            &names,
            wants,
            tokens,
            parsed_relations,
        ),
        None => {
            use redb::ReadableDatabase;
            let rt = storage
                .db()
                .begin_read()
                .map_err(|e| crate::catalog::table_lookup_error(e.into()))?;
            evaluate_in_txn(
                principal,
                &rt,
                tenant,
                &names,
                wants,
                tokens,
                parsed_relations,
            )
        }
    }
}

/// 同一スナップショット `src` での ACL の取得と、`wants` の判定（読み取り・書き込み・COPY・分割実行の
/// チャンク再判定が共有する）。表単位のビットで満たせる要求は列を見ない（高速経路）。満たせない列で
/// 満たせるビットだけ、relation の形を引いて列の仕様を解決し、列単位の付与で判定する。
pub(super) fn evaluate_in_txn<R: crate::storage::read_source::ReadSource>(
    principal: &Principal,
    src: &R,
    tenant: &str,
    names: &[String],
    wants: &[Want],
    tokens: &[Token],
    parsed_relations: &[String],
) -> Result<(), SqlSurfaceError> {
    let role_id = principal.current_role_id().unwrap_or(0);
    let grants = relation_grants_in_txn(src, tenant, names, role_id)
        .map_err(crate::catalog::table_lookup_error)?;
    for (name, granted) in names.iter().zip(&grants) {
        if granted.is_none() {
            // 準備済み文（Parse 済みで名前解決済みのトークン列）の参照先が消えている場合は、別セッションの
            // DROP 後に展開済みの文だけが残った状態であり、検査を省略すると SET ROLE 後のロールが
            // 権限なしで基底表を読める認可バイパスになる。fail-closed で拒否する（fail-open にしない）。
            // 通常 SQL でも、構文解析がカタログで実在を確認した名前（`parsed_relations`。ビュー展開前の
            // ビュー自身を含む）が ACL 照会の時点で見つからないなら、解析後に別セッションが DROP した
            // 参照先であり、検査を省略すると展開済みの基底表を権限なしで読めてしまう。拒否する。
            // 解析で実在を確認できなかった名前（構文語・別名などの誤分類）だけを読み飛ばす。
            if parsed_relations.iter().any(|p| p == name) {
                // 内部キー（`#<schema_id>.<name>`）は利用者へ出さない（ADR D2）。公開名の名前部だけを返す。
                return Err(SqlSurfaceError::undefined_table(
                    crate::catalog::namespace::relation_name_part(name),
                ));
            }
        }
    }
    let existing = grants.iter().filter(|g| g.is_some()).count();
    let mut shapes: Vec<Option<RelationShape>> = vec![None; names.len()];
    for want in wants {
        let Some(idx) = names.iter().position(|n| *n == want.relation) else {
            continue;
        };
        let Some(Some(g)) = grants.get(idx) else {
            continue;
        };
        // 表単位で満たせない要求のうち、列単位の付与で満たせうるものだけ列を評価する。
        let needs_columns = want.bits & columns::COLUMN_PRIVILEGES & !g.table_bits != 0
            && !g.column_bits.is_empty();
        let mut cover = 0u16;
        if needs_columns {
            if shapes.get(idx).is_some_and(|s| s.is_none()) {
                let shape = relation_shape_in_txn(src, &want.relation)
                    .map_err(crate::catalog::table_lookup_error)?;
                if let Some(slot) = shapes.get_mut(idx) {
                    *slot = Some(shape);
                }
            }
            if let Some(Some(shape)) = shapes.get(idx) {
                let demand = columns::resolve(&want.cols, shape, tokens, existing <= 1)?;
                cover =
                    columns::cover_bits(want.bits, g.table_bits, &g.column_bits, &demand, shape);
            }
        }
        check(
            principal,
            PrivilegeTarget::RelationColumns {
                required_bits: want.bits,
                granted_bits: g.table_bits,
                column_cover_bits: cover,
            },
        )?;
    }
    Ok(())
}

/// 表名を直接受け取る入口（HTTP 表層の bound 読み取り API）用。`table` は解決済みの relation キー。
/// 現在ロール未束縛・運用者権限のセッションは何もしない。束縛前なので列は特定できず、全列への SELECT
/// （表単位、または全列への列単位の付与）を要求する。
pub(crate) fn require_table_select(
    storage: &Storage,
    tenant: &str,
    session: &SessionState,
    table: &str,
) -> Result<(), SqlSurfaceError> {
    let principal = Principal::from_session(session);
    if !principal.needs_acl_lookup() {
        return check(
            &principal,
            PrivilegeTarget::Relation {
                required_bits: PRIV_SELECT,
                granted_bits: 0,
            },
        )
        .map(|_| ());
    }
    require_relations(
        storage,
        tenant,
        &principal,
        PRIV_SELECT,
        &[table.to_string()],
        None,
        &[],
    )
}

#[cfg(test)]
mod tests;
