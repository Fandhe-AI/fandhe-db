//! カタログのロールの `CONNECTION LIMIT` を強制する接続数レジストリ（WIRE-18、Issue #2233）。
//!
//! 役割: 認証に成功したカタログのロールごとの同時接続数を数え、上限超過の接続を拒否する。
//! `handshake::run_authenticated_session` が認証成功の後に [`RoleConnectionRegistry::try_acquire`] を呼び、
//! 返ったガードをセッションの寿命の間だけ保持する（どの経路で抜けても drop で数が戻る）。
//! 全体の接続数上限（`limits::ConnectionLimiter`）とは独立で、その内側で働く。`UserStore` が
//! フィールドとして持つため、サーバー単位で共有され、複数のサーバーが混ざらない。
//!
//! fail-closed: 加算は checked で行い、`Mutex` が poisoned のときは拒否側に倒す。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// 同時接続数の台帳。キーは `(tenant_id, role_id)`。
#[derive(Debug, Default)]
pub struct RoleConnectionRegistry {
    counts: Arc<Mutex<HashMap<(String, u64), u32>>>,
}

/// [`RoleConnectionRegistry::try_acquire`] の失敗理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcquireError {
    /// 上限（`CONNECTION LIMIT`）を超える。呼び出し側は `53300` で拒否する。
    LimitExceeded,
    /// 台帳を安全に更新できない（poisoned・桁あふれ）。呼び出し側は内部エラーとして拒否する。
    Unavailable,
}

/// 接続数を 1 つ保持するガード。drop で数を戻す。
#[derive(Debug)]
pub struct RoleConnectionGuard {
    counts: Arc<Mutex<HashMap<(String, u64), u32>>>,
    key: (String, u64),
}

impl RoleConnectionRegistry {
    /// 空の台帳を作る。
    pub fn new() -> Self {
        Self::default()
    }

    /// ロールの接続数を 1 つ確保する。`limit` が負（-1）は無制限、0 はすべて拒否、それ以外は自分を足した
    /// 後の数が `limit` を超えたら拒否する。無制限でも数は数える（上限をあとから変えたとき整合させる）。
    pub fn try_acquire(
        &self,
        tenant_id: &str,
        role_id: u64,
        limit: i32,
    ) -> Result<RoleConnectionGuard, AcquireError> {
        let key = (tenant_id.to_string(), role_id);
        let mut map = self.counts.lock().map_err(|_| AcquireError::Unavailable)?;
        let current = map.get(&key).copied().unwrap_or(0);
        let next = current.checked_add(1).ok_or(AcquireError::Unavailable)?;
        if let Ok(max) = u32::try_from(limit) {
            if next > max {
                return Err(AcquireError::LimitExceeded);
            }
        }
        map.insert(key.clone(), next);
        drop(map);
        Ok(RoleConnectionGuard {
            counts: Arc::clone(&self.counts),
            key,
        })
    }

    /// 現在の接続数（テスト用。poisoned のときは 0）。
    #[cfg(test)]
    fn count(&self, tenant_id: &str, role_id: u64) -> u32 {
        self.counts
            .lock()
            .ok()
            .and_then(|m| m.get(&(tenant_id.to_string(), role_id)).copied())
            .unwrap_or(0)
    }
}

impl Drop for RoleConnectionGuard {
    fn drop(&mut self) {
        // poisoned でも数を戻す（into_inner で台帳を取り出す。戻し忘れは以後の接続を不当に拒否するため）。
        let mut map = match self.counts.lock() {
            Ok(m) => m,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(n) = map.get_mut(&self.key) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                map.remove(&self.key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_counts_and_releases_on_drop() {
        let r = RoleConnectionRegistry::new();
        let g1 = r.try_acquire("t", 1, 2).expect("first");
        let _g2 = r.try_acquire("t", 1, 2).expect("second");
        assert_eq!(
            r.try_acquire("t", 1, 2).expect_err("third"),
            AcquireError::LimitExceeded
        );
        drop(g1);
        assert_eq!(r.count("t", 1), 1);
        let _g3 = r.try_acquire("t", 1, 2).expect("after release");
    }

    #[test]
    fn zero_rejects_all_and_negative_is_unlimited() {
        let r = RoleConnectionRegistry::new();
        assert_eq!(
            r.try_acquire("t", 1, 0).expect_err("zero"),
            AcquireError::LimitExceeded
        );
        assert_eq!(r.count("t", 1), 0);
        let guards: Vec<_> = (0..50)
            .map(|_| r.try_acquire("t", 2, -1).expect("unlimited"))
            .collect();
        assert_eq!(r.count("t", 2), 50);
        drop(guards);
        assert_eq!(r.count("t", 2), 0);
    }

    #[test]
    fn roles_and_tenants_are_counted_separately() {
        let r = RoleConnectionRegistry::new();
        let _a = r.try_acquire("t1", 1, 1).expect("t1");
        let _b = r.try_acquire("t2", 1, 1).expect("t2 same id");
        let _c = r.try_acquire("t1", 2, 1).expect("t1 other role");
        assert!(r.try_acquire("t1", 1, 1).is_err());
    }
}
