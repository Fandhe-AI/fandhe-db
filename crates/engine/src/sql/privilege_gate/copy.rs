//! `COPY` 文の権限検査（SQL-40・Issue #1969。経路一覧は `docs/design/privilege-check-paths.md`）。
//!
//! 共通ゲート（[`super::check`]）と ACL の照会点（[`super::read::require_relations_bits`]）を再利用し、
//! COPY 専用の判定ロジックは持たない（第 2 の判定点を作らない）。呼び出し元は `core.rs` の
//! `begin_copy*`（簡易クエリ・明示 Tx。検証済みの構文木と名前解決済みトークンを渡す）と
//! `core/temp_route.rs` の拡張クエリ入口（Parse・Bind・Describe）。
//!
//! 契約:
//! - `COPY ... FROM STDIN` は対象表への INSERT、`COPY ... TO STDOUT` は読む relation への SELECT を要求する。
//! - `FROM STDIN` 形はトークン走査をしない（`STDIN` を relation と誤認しないため）。対象表は構文木から取る。
//! - `TO` 形はトークン走査の参照先に `inner.table_name()` を加える（テーブル形は `FROM` を持たないため）。
//! - 現在ロール未束縛・運用者権限のセッションは何もしない（従来と同一）。`Some(0)` は拒否する。
//! - 一時表（セッションの所有物）は呼び出し元が本ゲートの手前で分岐するので対象外。
//! - ACL は明示 Tx では `BEGIN` 時のスナップショットで照会し（`read_snap`）、検証済みの対象表が照会で
//!   見つからなければ `42P01` で拒否する（`read.rs` の `require_relations_bits` の規則をそのまま使う）。
//! - 列単位の付与（Issue #2083。`columns.rs`）: `FROM` は列リストの列への INSERT、`TO` のテーブル形は列リスト
//!   （省略時は全列）への SELECT、問い合わせ形は単一 relation なら名指しした列への SELECT（それ以外は全列）。
//!   従来方式の表の疑似列 `id`（`FROM` の列リストは必ず含む）は表単位でしか満たせない。
//! - 拒否は固定文言の `42501`。表名・ロール ID・テナントを含めない。

use crate::catalog::privilege::{PRIV_INSERT, PRIV_SELECT};
use crate::sql::allowlist::{CopyStatement, Projection, SqlSurfaceError};
use crate::sql::lexer::Token;
use crate::sql::mode::SessionState;
use crate::storage::Storage;

use super::columns::{ColumnSpec, Want};
use super::read::require_relations_bits;
use super::{check, Principal, PrivilegeTarget};

/// COPY 文が要求する権限ビット（From は INSERT、To は SELECT）。
pub(crate) fn copy_required_bits(stmt: &CopyStatement) -> u16 {
    match stmt {
        CopyStatement::From(_) => PRIV_INSERT,
        CopyStatement::To(_) => PRIV_SELECT,
    }
}

/// COPY 文の対象 relation に必要な権限があるかを判定する（不足は `42501`）。`resolved_tokens` は
/// 名前解決済みの字句列。呼び出し元は構文検証（`42601`）と名前解決（`42P01`／`3F000`）の後に呼ぶ。
pub(crate) fn require_copy_privileges(
    storage: &Storage,
    tenant: &str,
    session: &SessionState,
    stmt: &CopyStatement,
    resolved_tokens: &[Token],
    read_snap: Option<&redb::ReadTransaction>,
) -> Result<(), SqlSurfaceError> {
    let required_bits = copy_required_bits(stmt);
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
    let mut names: Vec<String> = Vec::new();
    match stmt {
        CopyStatement::From(v) => names.push(v.table_name.clone()),
        CopyStatement::To(v) => {
            names.extend(crate::sql::search_path::resolved_relation_ref_names(
                resolved_tokens,
            ));
            names.push(v.inner.table_name().to_string());
        }
    }
    // 構文検証（`validate_copy_tokens`）がカタログで実在を確認した対象表。ACL の照会で見つからなければ、
    // 検証後に DROP された参照先として拒否する（fail-closed。SELECT・DML ゲートと同じ規則）。
    let verified = vec![match stmt {
        CopyStatement::From(v) => v.table_name.clone(),
        CopyStatement::To(v) => v.inner.table_name().to_string(),
    }];
    let mut wants: Vec<Want> = Vec::new();
    match stmt {
        CopyStatement::From(v) => wants.extend(Want::split(
            &v.table_name,
            required_bits,
            ColumnSpec::Names(v.columns.clone()),
        )),
        CopyStatement::To(v) => {
            let target = v.inner.table_name();
            let target_spec = match (&v.inner.projection, v.is_table_form()) {
                (Projection::Columns(cols), true) => ColumnSpec::Names(cols.clone()),
                (_, true) => ColumnSpec::All,
                (_, false) => ColumnSpec::Scan,
            };
            for n in names {
                if n == target {
                    wants.extend(Want::split(&n, required_bits, target_spec.clone()));
                } else {
                    wants.extend(Want::all(&n, required_bits));
                }
            }
        }
    }
    require_relations_bits(
        storage,
        tenant,
        &principal,
        &wants,
        resolved_tokens,
        read_snap,
        &verified,
    )
}
