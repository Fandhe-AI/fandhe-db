//! SEQUENCE・関数単位の権限の判定部品（SQL-40・RLS-12 のポインタ、Issue #2235。設計記録:
//! `docs/design/role-privilege-model.md`「Issue #2235 実装記録」S1・S2・S7）。
//!
//! 役割: `schema_privilege.rs` と同じ流儀で、カタログ（`Storage`）に依存しない判定の唯一の分岐点を置く。
//! - [`check_any_of`]: SEQUENCE の判定（`nextval` は USAGE か UPDATE、`currval`・`lastval` は USAGE か SELECT、
//!   `setval` は UPDATE。いずれか 1 ビットで可）。ACL の読み取りは呼び出し元が渡す `lookup` に閉じる。
//! - [`FunctionAclScope`]: 関数 EXECUTE の拒否集合を運ぶスレッドローカルの RAII スコープ。`core.rs` が
//!   セッションの文を実行する区間の頭で、束縛ロールが EXECUTE を持たないカタログ由来の関数名の集合を
//!   1 回の読み取り txn で作って張る。参照点は式束縛（`udf_call::bind_call` の宣言的 UDF の展開）と
//!   `LANGUAGE sql|plpgsql` の関数の解決（`routine_fold`・`function_scan::plan`）で、入れ子の展開・ビュー本文・
//!   CHECK／DEFAULT・PL/pgSQL 本体内の呼び出しも同じ点を通るため、呼び出し元ロールで判定される
//!   （PostgreSQL と同じ）。
//!
//! 主体の扱い: ACL で判定すべき主体は `schema_privilege::acl_checked_principal`（束縛済みかつ所有者相当
//! でない主体）だけ。未束縛・所有者相当・セッションを持たない Rust API 経路は検査しない（#1971 で全セッション
//! が束縛された時点で全面適用になる。S1）。スコープが張られていない区間（内部経路）は検査しないが、公開の
//! セッション入口はすべてスコープを張る（`core.rs` の `function_acl_scope`）。
//!
//! 拒否は固定文言の `InsufficientPrivilege`（`42501`）で、名前・ID・ACL の断片を含めない。

use std::cell::RefCell;
use std::collections::BTreeSet;

use crate::sql::allowlist::SqlSurfaceError;

/// ACL で判定すべき主体 `role` が、`needed_any` のいずれかのビットを持つか検査する。`lookup(role_id)` は
/// そのロールが対象に持つ権限ビットの和（PUBLIC 込み）を返す。`role` が `None`（検査しない主体）なら
/// `lookup` を呼ばずに通す。読み取りの失敗は呼び出し元の `Err` のまま伝える（fail-closed）。
pub(crate) fn check_any_of(
    role: Option<u64>,
    needed_any: u16,
    lookup: &dyn Fn(u64) -> Result<u16, SqlSurfaceError>,
) -> Result<(), SqlSurfaceError> {
    let Some(role) = role else {
        return Ok(());
    };
    if lookup(role)? & needed_any != 0 {
        Ok(())
    } else {
        Err(SqlSurfaceError::InsufficientPrivilege)
    }
}

thread_local! {
    /// 現在の文の実行区間で EXECUTE を持たない関数名（小文字）の集合。`None` はスコープ外。
    static DENIED: RefCell<Option<BTreeSet<String>>> = const { RefCell::new(None) };
}

/// 関数 EXECUTE の拒否集合を張る RAII ガード。drop で直前の状態へ戻す（入れ子・panic でも他の文脈へ
/// 漏れない）。
pub(crate) struct FunctionAclScope {
    prev: Option<BTreeSet<String>>,
}

impl FunctionAclScope {
    /// 現スレッドへ拒否集合を張る（`denied` の名前は小文字）。
    pub(crate) fn enter(denied: BTreeSet<String>) -> Self {
        let prev = DENIED.with(|d| d.replace(Some(denied)));
        FunctionAclScope { prev }
    }

    /// スコープが既に張られているか（入れ子の入口が二重に読み取り txn を開かないための判定）。
    pub(crate) fn is_active() -> bool {
        DENIED.with(|d| d.borrow().is_some())
    }
}

impl Drop for FunctionAclScope {
    fn drop(&mut self) {
        let prev = self.prev.take();
        DENIED.with(|d| *d.borrow_mut() = prev);
    }
}

/// カタログ由来の関数 `name` の呼び出しを許すか検査する（解決点から呼ぶ。式束縛・ルーチン解決）。
/// スコープが無い区間・拒否集合に載らない名前は通す。
pub(crate) fn check_execute(name: &str) -> Result<(), SqlSurfaceError> {
    let denied = DENIED.with(|d| {
        d.borrow()
            .as_ref()
            .is_some_and(|set| set.contains(&name.to_ascii_lowercase()))
    });
    if denied {
        Err(SqlSurfaceError::InsufficientPrivilege)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unreachable_lookup(_: u64) -> Result<u16, SqlSurfaceError> {
        panic!("lookup must not run for this principal");
    }

    #[test]
    fn exempt_principal_skips_the_lookup() {
        assert!(check_any_of(None, 0b1, &unreachable_lookup).is_ok());
    }

    #[test]
    fn any_one_bit_is_enough() {
        let held = |bits: u16| move |_: u64| Ok(bits);
        assert!(check_any_of(Some(7), 0b101, &held(0b001)).is_ok());
        assert!(check_any_of(Some(7), 0b101, &held(0b100)).is_ok());
        let deny = check_any_of(Some(7), 0b101, &held(0b010)).expect_err("no bit");
        assert_eq!(deny.wire_code(), "42501");
        assert!(check_any_of(Some(7), 0b101, &held(0)).is_err());
    }

    #[test]
    fn lookup_failure_is_fail_closed() {
        let boom = |_: u64| {
            Err(SqlSurfaceError::Internal {
                detail: "x".to_string(),
            })
        };
        assert!(check_any_of(Some(7), 0b1, &boom).is_err());
    }

    #[test]
    fn scope_denies_only_listed_names_and_restores_on_drop() {
        assert!(!FunctionAclScope::is_active());
        assert!(check_execute("f").is_ok());
        {
            let _s = FunctionAclScope::enter(BTreeSet::from(["f".to_string()]));
            assert!(FunctionAclScope::is_active());
            assert_eq!(check_execute("F").expect_err("denied").wire_code(), "42501");
            assert!(check_execute("g").is_ok());
            {
                let _inner = FunctionAclScope::enter(BTreeSet::new());
                assert!(check_execute("f").is_ok());
            }
            assert!(check_execute("f").is_err());
        }
        assert!(!FunctionAclScope::is_active());
        assert!(check_execute("f").is_ok());
    }
}
