//! 権限エラーの応答時間が存在オラクルにならないことの粗い確認（Issue #1972、ポインタ: RLS-12・SQL-40）。
//!
//! 応答の中身（SQLSTATE・文言）の一致は `tests/table_rls/rls12_privilege_existence_oracle.rs` が固定する。
//! 本ファイルは同じ失敗経路を「他テナントだけに在る名前 (C)」と「どこにも無い名前 (B)」で実行し、所要時間の
//! 代表値（バッチ時間の中央値）の比が緩い上限に収まることだけを確かめる。粗い確認であり統計的なゲートではない。
//! 閾値を緩めて通すことはアサーションの弱体化として禁止する。分布検定・専有環境での厳密な定量は対象外。
//!
//! 時間計測を含むため `table_rls` へは束ねず単独バイナリとし、他の計測系テストとの並走を
//! `timing_lock` で断つ（`catalog_latency.rs` と同じ型）。ラウンドごとに B と C の先後を入れ替えて
//! 周期的な負荷ノイズが片側に偏らないようにし、各ラウンドの応答が B と C で同じ形であることも検証する
//! （計測対象が同じ失敗経路であることの確認）。

use std::time::{Duration, Instant};

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

#[path = "../src/test_util/timing_lock.rs"]
mod timing_lock;
use timing_lock::acquire_timing_lock;

const OBSERVER: &str = "tenant-a";
const OWNER: &str = "tenant-b";
/// 観測ロール（束縛される主体）の ID。他テナントのロールは 2。
const OBS: u64 = 1;
const FOREIGN_OWNER: u64 = 2;

const WARMUP: usize = 20;
const ROUNDS: usize = 31;
/// 1 バッチあたりの呼び出し回数（1 回あたりが µs 級のため、タイマー分解能に負けないようまとめる）。
const BATCH: usize = 20;

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn bound(tenant: &str, id: u64, ddl: bool) -> SessionState {
    let mut s = SessionState::default();
    s.bind_role_ids(id, id);
    assert!(s.bind_resolution_tenant(tenant));
    if ddl {
        s.allow_ddl();
    }
    s
}

fn operator(tenant: &str) -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    assert!(s.bind_resolution_tenant(tenant));
    s
}

fn setup(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) {
    if let Err(e) = core.execute_sql_in_session(&ctx(tenant), s, sql) {
        panic!("setup {tenant}: {sql}: {e}");
    }
}

/// 応答を比較用の形にし、B と C の名前の差だけを潰す。
fn shape(core: &EngineCore, s: &mut SessionState, sql: &str) -> String {
    let r = core.execute_sql_in_session(&ctx(OBSERVER), s, sql);
    let t = match r {
        Ok(o) => format!("OK {o:?}"),
        Err(e) => format!("{} {}", e.wire_code(), e.client_message()),
    };
    t.replace("zz_secret_", "zz_x_")
        .replace("zz_absent_", "zz_x_")
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

#[test]
fn rls12_privilege_error_latency_does_not_reveal_foreign_objects() {
    let _timing_guard = acquire_timing_lock();

    let path = unique_db_path("rls12-latency");
    let _guard = CleanupGuard(path.clone());
    let core = EngineCore::from_storage(
        Storage::open(&path).expect("open storage"),
        Box::new(CpuScalarProvider),
    );
    let mut op = operator(OBSERVER);
    setup(&core, OBSERVER, &mut op, "CREATE ROLE obs");
    let mut op_o = operator(OWNER);
    setup(&core, OWNER, &mut op_o, "CREATE ROLE zz_owner");
    let mut obs_ddl = bound(OBSERVER, OBS, true);
    setup(
        &core,
        OBSERVER,
        &mut obs_ddl,
        "CREATE TABLE mine (id BIGINT PRIMARY KEY, n INTEGER)",
    );
    // 他テナントにだけ在るもの（C）。B 用の `zz_absent_*` はどこにも作らない。
    let mut o = bound(OWNER, FOREIGN_OWNER, true);
    setup(&core, OWNER, &mut o, "CREATE SCHEMA zz_secret_s");
    setup(
        &core,
        OWNER,
        &mut o,
        "CREATE TABLE zz_secret_s.zz_secret_t (id BIGINT PRIMARY KEY, zz_secret_c TEXT)",
    );
    setup(&core, OWNER, &mut op_o, "CREATE ROLE zz_secret_r");

    // (ラベル, C の文, B の文, 運用者権限つきか)
    let pairs: [(&str, &str, &str, bool); 5] = [
        (
            "table",
            "SELECT id FROM zz_secret_s.zz_secret_t",
            "SELECT id FROM zz_absent_s.zz_absent_t",
            false,
        ),
        (
            "column",
            "SELECT zz_secret_c FROM zz_secret_s.zz_secret_t",
            "SELECT zz_absent_c FROM zz_absent_s.zz_absent_t",
            false,
        ),
        (
            "grant-role",
            "GRANT SELECT ON mine TO zz_secret_r",
            "GRANT SELECT ON mine TO zz_absent_r",
            false,
        ),
        (
            "set-role",
            "SET ROLE zz_secret_r",
            "SET ROLE zz_absent_r",
            false,
        ),
        (
            "drop-role",
            "DROP ROLE zz_secret_r",
            "DROP ROLE zz_absent_r",
            true,
        ),
    ];

    for (label, c_sql, b_sql, ddl) in pairs {
        let mut s = bound(OBSERVER, OBS, ddl);
        // 同じ失敗経路であること（成功する文を計測していないこと）。
        let sc = shape(&core, &mut s, c_sql);
        let sb = shape(&core, &mut s, b_sql);
        assert_eq!(sc, sb, "{label}: response shape differs between C and B");
        assert!(!sc.starts_with("OK"), "{label}: probe must fail: {sc}");

        for _ in 0..WARMUP {
            let _ = shape(&core, &mut s, c_sql);
            let _ = shape(&core, &mut s, b_sql);
        }
        let mut c_times = Vec::with_capacity(ROUNDS);
        let mut b_times = Vec::with_capacity(ROUNDS);
        for round in 0..ROUNDS {
            let mut measure = |sql: &str, expected: &str| {
                let start = Instant::now();
                for _ in 0..BATCH {
                    let t = shape(&core, &mut s, sql);
                    assert_eq!(t, expected, "{label}: response changed during measurement");
                }
                start.elapsed()
            };
            // ラウンドごとに先後を入れ替える。
            if round % 2 == 0 {
                c_times.push(measure(c_sql, &sc));
                b_times.push(measure(b_sql, &sb));
            } else {
                b_times.push(measure(b_sql, &sb));
                c_times.push(measure(c_sql, &sc));
            }
        }
        let (mc, mb) = (median(c_times), median(b_times));
        let (hi, lo) = (mc.max(mb), mc.min(mb));
        let ratio = hi.as_secs_f64() / lo.as_secs_f64().max(1e-9);
        assert!(
            ratio < 5.0,
            "{label}: median latency differs between foreign-only (C) and absent (B) names: \
             c={mc:?}, b={mb:?}, ratio={ratio}"
        );
    }
}
