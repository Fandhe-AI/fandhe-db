//! SCHEMA 単位の権限（USAGE・CREATE）の判定（SQL-40・RLS-12 のポインタ、Issue #2234。設計記録:
//! `docs/design/role-privilege-model.md`「Issue #2234 実装記録」K1・K3・K5）。
//!
//! 役割: `core.rs` の SCHEMA 解決器（修飾名の USAGE 検査）・`search_path` の probe（USAGE の無い SCHEMA を
//! 「見えないもの」として飛ばす）・作成系 DDL の作成先検査（CREATE）が共有する、判定の唯一の分岐点。
//! カタログは引かず、ACL の読み取りは呼び出し元が渡す `lookup` に閉じる（本モジュールは `Storage` に依存しない）。
//!
//! 主体の扱い（段階導入。#1966 の共通ゲートへの統合・#1971 の全セッション束縛までの暫定）:
//! - 現在ロール未束縛のセッション（認証層がまだ束縛していない既存の全入口）は従来どおり検査しない。
//! - DDL 権限ゲートを通過したセッション（`ddl_allowed`）は所有者相当で、自テナントの全 SCHEMA に USAGE・
//!   CREATE を持つとみなす（所有者の機構〔#2236〕が入るまでの代替。`grant_ddl` と同じ規約）。
//! - 束縛ロールを持つ上記以外の主体は、ACL（直接の行と PUBLIC の行）で判定する。
//!
//! 順序の契約（D10）: 呼び出し元は「自テナント内の名前解決 → 本判定」の順で呼ぶ。他テナントの SCHEMA・
//! 不在は解決器が先に `None`（`3F000`）にするため、本判定の分岐は他テナントの SCHEMA では走らない
//! （権限の有無が存在オラクルにならない）。拒否は固定文言の `InsufficientPrivilege`（`42501`）で、
//! SCHEMA 名・ID・ACL の断片を含めない。

use crate::catalog::privilege::{PRIV_CREATE, PRIV_USAGE};
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::mode::SessionState;

/// ACL で判定すべき主体（束縛ロールの ID）。判定を飛ばす主体（未束縛・所有者相当）は `None`。
pub(crate) fn acl_checked_principal(session: &SessionState) -> Option<u64> {
    if session.ddl_allowed() {
        return None;
    }
    match session.current_role_id() {
        Some(id) if id != 0 => Some(id),
        _ => None,
    }
}

/// `session` が SCHEMA に対して `needed`（権限ビットの全部）を持つか検査する。`lookup(role_id)` は、
/// ACL で判定すべき主体にだけ呼ばれ、そのロールが SCHEMA に持つ権限ビットの和（PUBLIC 込み）を返す。
pub(crate) fn check_schema_privilege(
    session: &SessionState,
    needed: u16,
    lookup: &dyn Fn(u64) -> Result<u16, SqlSurfaceError>,
) -> Result<(), SqlSurfaceError> {
    let Some(role) = acl_checked_principal(session) else {
        return Ok(());
    };
    if lookup(role)? & needed == needed {
        Ok(())
    } else {
        Err(SqlSurfaceError::InsufficientPrivilege)
    }
}

/// USAGE の検査（修飾名の解決後・Bind の再検査）。
pub(crate) fn check_schema_usage(
    session: &SessionState,
    lookup: &dyn Fn(u64) -> Result<u16, SqlSurfaceError>,
) -> Result<(), SqlSurfaceError> {
    check_schema_privilege(session, PRIV_USAGE, lookup)
}

/// CREATE の検査（作成系 DDL の作成先が SCHEMA のとき。結線点は `core.rs` の `require_creation_target`）。
///
/// 現時点では `ServerDdl` ゲートが必要条件として先に通るため、ここへ来る主体は所有者相当（`ddl_allowed`）
/// で、観測上の挙動は変わらない。`ServerDdl` を CREATE へ置き換えるのは所有者の機構（#2236）と認証主体の
/// 束縛（#1971）の後（ロールの束縛だけで DDL 権限が広がる経路を作らない）。
pub(crate) fn check_schema_create(
    session: &SessionState,
    lookup: &dyn Fn(u64) -> Result<u16, SqlSurfaceError>,
) -> Result<(), SqlSurfaceError> {
    check_schema_privilege(session, PRIV_CREATE, lookup)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bound(id: u64, owner: bool) -> SessionState {
        let mut s = SessionState::default();
        s.bind_role_ids(id, id);
        if owner {
            s.allow_ddl();
        }
        s
    }

    fn grants(bits: u16) -> impl Fn(u64) -> Result<u16, SqlSurfaceError> {
        move |_| Ok(bits)
    }

    fn unreachable_lookup(_: u64) -> Result<u16, SqlSurfaceError> {
        panic!("lookup must not run for this principal");
    }

    #[test]
    fn unbound_and_owner_equivalent_skip_the_acl() {
        let unbound = SessionState::default();
        assert!(check_schema_usage(&unbound, &unreachable_lookup).is_ok());
        let mut ddl_only = SessionState::default();
        ddl_only.allow_ddl();
        assert!(check_schema_create(&ddl_only, &unreachable_lookup).is_ok());
        assert!(check_schema_usage(&bound(7, true), &unreachable_lookup).is_ok());
    }

    #[test]
    fn bound_role_is_judged_by_the_acl() {
        let s = bound(7, false);
        let deny = check_schema_usage(&s, &grants(0)).expect_err("no usage");
        assert_eq!(deny.wire_code(), "42501");
        assert!(check_schema_usage(&s, &grants(PRIV_USAGE)).is_ok());
        // USAGE だけでは CREATE は通らない（逆も同じ）。
        assert!(check_schema_create(&s, &grants(PRIV_USAGE)).is_err());
        assert!(check_schema_create(&s, &grants(PRIV_CREATE)).is_ok());
        assert!(check_schema_usage(&s, &grants(PRIV_CREATE)).is_err());
        assert!(check_schema_privilege(&s, PRIV_USAGE | PRIV_CREATE, &grants(PRIV_USAGE)).is_err());
        assert!(check_schema_privilege(
            &s,
            PRIV_USAGE | PRIV_CREATE,
            &grants(PRIV_USAGE | PRIV_CREATE)
        )
        .is_ok());
    }

    #[test]
    fn lookup_failure_is_fail_closed() {
        let s = bound(7, false);
        let boom = |_: u64| -> Result<u16, SqlSurfaceError> {
            Err(SqlSurfaceError::Internal {
                detail: "x".to_string(),
            })
        };
        assert!(check_schema_usage(&s, &boom).is_err());
    }
}
