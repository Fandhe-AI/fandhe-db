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
//! - カタログで relation として引けない名前（誤分類された識別子）は検査しない。存在しないものは読めない
//!   ので fail-open にならない。余分な検査は拒否側にだけ倒れる。
//! - 参照数の上限を ACL の照会より前に検査する（untrusted 入力）。
//! - エラーは固定文言の `42501`。表名・ロール ID・テナントを含めない。

use crate::catalog::privilege::{PRIV_SELECT, PRIV_UPDATE};
use crate::core::ParsedSql;
use crate::sql::allowlist::{SqlSurfaceError, Statement};
use crate::sql::cursor::CursorStatement;
use crate::sql::lexer::Token;
use crate::sql::mode::SessionState;
use crate::sql::relation_name::SchemaResolver;
use crate::storage::Storage;

use super::{check, Principal, PrivilegeTarget};

/// 1 文が参照できる relation 名の上限（重複除去後。ACL の照会より前に検査する）。
pub(crate) const MAX_READ_RELATION_REFS: usize = 256;

/// 文が要求する読み取り側の権限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadRequirement {
    /// 本ゲートの対象外（DML・COPY・DDL・セッション制御等。DML の権限は #1968、COPY は #1969）。
    NotApplicable,
    /// 参照するすべての relation に表単位の SELECT。
    Select,
    /// 行ロック句つきの SELECT。SELECT に加えて UPDATE を要求する（PostgreSQL と同じく、ロック強度に
    /// よらず UPDATE 系の権限が要る。`OF` で絞られた表だけでなく参照先の全表に要求する側へ倒す）。
    SelectForUpdate,
}

impl ReadRequirement {
    fn bits(self) -> Option<u16> {
        match self {
            Self::NotApplicable => None,
            Self::Select => Some(PRIV_SELECT),
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
        // DML の読み取り側（SELECT 元・FROM／USING・サブクエリ）と書き込みの権限は #1968 が本モジュールの
        // 収集関数を再利用して結線する。COPY は #1969。
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
        | ParsedSql::CreateRole(_)
        | ParsedSql::DropRole(_)
        | ParsedSql::Do(_)
        | ParsedSql::CreateSchema(_)
        | ParsedSql::DropSchema(_)
        | ParsedSql::AlterSchema(_)
        | ParsedSql::AlterSequence(_)
        | ParsedSql::SequenceFunction(_) => NotApplicable,
    }
}

fn classify_statement(s: &Statement) -> ReadRequirement {
    match s {
        Statement::Select(_)
        | Statement::Aggregate(_)
        | Statement::Explain(_)
        | Statement::Scan(_)
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
        );
    }
    let mut resolved = tokens.to_vec();
    crate::sql::relation_name::rewrite_relation_names_full(&mut resolved, resolver)?;
    require_relations(
        storage,
        tenant,
        &principal,
        required_bits,
        &crate::sql::search_path::resolved_relation_ref_names(&resolved),
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
        );
    }
    require_relations(
        storage,
        tenant,
        &principal,
        required_bits,
        &crate::sql::search_path::resolved_relation_ref_names(resolved_tokens),
    )
}

/// 解決済みの relation 名の並びに対する判定（表名を直接渡す HTTP 表層の入口と共有する）。
fn require_relations(
    storage: &Storage,
    tenant: &str,
    principal: &Principal,
    required_bits: u16,
    candidates: &[String],
) -> Result<(), SqlSurfaceError> {
    let mut names: Vec<String> = Vec::new();
    for name in candidates {
        if !names.contains(name) {
            names.push(name.clone());
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
    let role_id = principal.current_role_id().unwrap_or(0);
    let bits = storage
        .relation_grant_bits(tenant, &names, role_id)
        .map_err(crate::catalog::table_lookup_error)?;
    for granted in bits.into_iter().flatten() {
        check(
            principal,
            PrivilegeTarget::Relation {
                required_bits,
                granted_bits: granted,
            },
        )?;
    }
    Ok(())
}

/// 表名を直接受け取る入口（HTTP 表層の bound 読み取り API）用。`table` は解決済みの relation キー。
/// 現在ロール未束縛・運用者権限のセッションは何もしない。
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
        );
    }
    require_relations(
        storage,
        tenant,
        &principal,
        PRIV_SELECT,
        &[table.to_string()],
    )
}

#[cfg(test)]
mod tests;
