//! ポリシー有効な表での制約判定（一意性・参照整合性・`ON CONFLICT`）の範囲を固定する（Issue #2281、
//! ポインタ: `docs/spec/04-behavior/rls.md` RLS-13・補助 RLS-12。設計記録は
//! `docs/design/row-security-policy-composition.md`「非漏えい試験（#2281）」節）。
//!
//! 固定する契約:
//! - 制約はテナント内の全行で判定する。自テナントのポリシーで隠れた行とのキー衝突は `23505`、隠れた親を
//!   参照する子の挿入は成功、隠れた子を持つ親の削除は `23503`（PG と同じ。テナント内で隠れた行の存在が
//!   制約違反から推知できるのは既知の許容差であり、他テナントの行は含まない）
//! - 他テナントの行は判定に入らない。他テナントにだけ在るキー・id・親は「存在しない」と同じ扱いで、
//!   応答（SQLSTATE・`client_message`・失敗した段）が他テナントの行の有無（双子 DB の X/Y）に依存しない
//! - 制約違反の文言に他テナントの値・テナント ID・隠れた行の他列の値が現れない
//!
//! 検証は `rls13_policy_tenant_nonleak.rs` と同じ双子 DB 方式。期待する SQLSTATE はテスト側の独立した
//! 表で与え、production の判定は呼ばない。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::legacy_table::LegacyExec as _;
use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TENANTS: [&str; 3] = ["tenant-a", "tenant-b", "tenant-c"];

/// 他テナントだけが知る値。観測側の応答に現れたら存在の漏えい。
const WATCH: &[&str] = &["zz_secret", "zz_hidden_note"];

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

struct World {
    _g: CleanupGuard,
    core: EngineCore,
    obs: &'static str,
    frn: &'static str,
}

fn run(w: &World, tenant: &str, sql: &str) -> String {
    let mut s = SessionState::default();
    s.allow_ddl();
    match w
        .core
        .execute_sql_in_session_legacy(&ctx(tenant), &mut s, sql)
    {
        Ok(SqlOutcome::Query(q)) => format!("OK {:?}", q.rows),
        Ok(o) => format!("OK {o:?}"),
        Err(e) => format!("exec:{} {}", e.wire_code(), e.client_message()),
    }
}

fn must(w: &World, tenant: &str, sql: &str) {
    let r = run(w, tenant, sql);
    assert!(r.starts_with("OK"), "setup {tenant}: {sql}: {r}");
}

/// 観測側テナント `obs_idx`・他テナント `(obs_idx + 1) % 3` の双子 DB の片方。表・観測側の行・ポリシーを
/// 先に作り、`foreign` のときだけ他テナントの行・RLS・ポリシーを置く。
fn world(label: &str, obs_idx: usize, foreign: bool) -> World {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let w = World {
        _g: guard,
        core,
        obs: TENANTS[obs_idx],
        frn: TENANTS[(obs_idx + 1) % 3],
    };
    let o = w.obs;
    must(&w, o, "CREATE TABLE u (k TEXT UNIQUE, v TEXT)");
    must(&w, o, "CREATE TABLE par (name TEXT)");
    must(
        &w,
        o,
        "CREATE TABLE chi (pid BIGINT REFERENCES par, note TEXT)",
    );
    // 観測側の行（`show` は自分のポリシーを通る行、`hide` は隠れる行）。
    let rows = [
        "INSERT INTO u (id, k, v) VALUES (1, 'k1', 'show') USING OPERATION_ID 'o-u1'",
        "INSERT INTO u (id, k, v) VALUES (2, 'k2', 'hide') USING OPERATION_ID 'o-u2'",
        "INSERT INTO par (id, name) VALUES (1, 'show') USING OPERATION_ID 'o-p1'",
        "INSERT INTO par (id, name) VALUES (2, 'hide') USING OPERATION_ID 'o-p2'",
        "INSERT INTO par (id, name) VALUES (3, 'show') USING OPERATION_ID 'o-p3'",
        "INSERT INTO par (id, name) VALUES (4, 'show') USING OPERATION_ID 'o-p4'",
        "INSERT INTO chi (id, pid, note) VALUES (3, 3, 'hide') USING OPERATION_ID 'o-c3'",
    ];
    for sql in rows {
        must(&w, o, sql);
    }
    for t in ["u", "par", "chi"] {
        must(&w, o, &format!("ALTER TABLE {t} ENABLE ROW LEVEL SECURITY"));
        must(&w, o, &format!("ALTER TABLE {t} FORCE ROW LEVEL SECURITY"));
    }
    must(
        &w,
        o,
        "CREATE POLICY pu ON u FOR ALL USING (v = 'show') WITH CHECK (true)",
    );
    must(
        &w,
        o,
        "CREATE POLICY pp ON par FOR ALL USING (name = 'show') WITH CHECK (true)",
    );
    must(
        &w,
        o,
        "CREATE POLICY pc ON chi FOR ALL USING (note = 'show') WITH CHECK (true)",
    );
    if foreign {
        let f = w.frn;
        for sql in [
            // 観測側と同じキー・同じ id・観測側に無い id・観測側にだけ無い親。
            "INSERT INTO u (id, k, v) VALUES (1, 'k1', 'zz_secret') USING OPERATION_ID 'f-u1'",
            "INSERT INTO u (id, k, v) VALUES (9, 'kf', 'zz_secret') USING OPERATION_ID 'f-u9'",
            "INSERT INTO par (id, name) VALUES (9, 'zz_secret') USING OPERATION_ID 'f-p9'",
            "INSERT INTO par (id, name) VALUES (4, 'zz_secret') USING OPERATION_ID 'f-p4'",
            // 他テナントの子は観測側の親 4 の削除を妨げない。
            "INSERT INTO chi (id, pid, note) VALUES (4, 4, 'zz_hidden_note') USING OPERATION_ID 'f-c4'",
            "ALTER TABLE u ENABLE ROW LEVEL SECURITY",
            "CREATE POLICY zz_secret_pol ON u AS PERMISSIVE FOR ALL USING (true)",
        ] {
            must(&w, f, sql);
        }
    }
    w
}

/// (SQL, 観測側が期待する応答の接頭辞)。`{op}` は試行ごとの操作 ID。
const PROBES: &[(&str, &str)] = &[
    // 一意性: 隠れた行のキーは衝突（テナント内）、他テナントだけのキー・id は衝突しない。
    (
        "INSERT INTO u (id, k, v) VALUES (10, 'k2', 'show') USING OPERATION_ID '{op}'",
        "exec:23505",
    ),
    (
        "INSERT INTO u (id, k, v) VALUES (11, 'kf', 'show') USING OPERATION_ID '{op}'",
        "OK",
    ),
    (
        "INSERT INTO u (id, k, v) VALUES (12, 'k1', 'show') USING OPERATION_ID '{op}'",
        "exec:23505",
    ),
    (
        "UPDATE u SET k = 'k2' WHERE id = 1 USING OPERATION_ID '{op}'",
        "exec:23505",
    ),
    (
        "INSERT INTO u (id, k, v) VALUES (2, 'n2', 'show') USING OPERATION_ID '{op}'",
        "exec:23505",
    ),
    (
        "INSERT INTO u (id, k, v) VALUES (9, 'n9', 'show') USING OPERATION_ID '{op}'",
        "OK",
    ),
    // 参照整合性: 隠れた親への参照は成功、他テナントだけの親・存在しない親は同じ 23503。
    (
        "INSERT INTO chi (id, pid, note) VALUES (10, 2, 'show') USING OPERATION_ID '{op}'",
        "OK",
    ),
    (
        "INSERT INTO chi (id, pid, note) VALUES (11, 9, 'show') USING OPERATION_ID '{op}'",
        "exec:23503",
    ),
    (
        "INSERT INTO chi (id, pid, note) VALUES (12, 777, 'show') USING OPERATION_ID '{op}'",
        "exec:23503",
    ),
    // 隠れた子を持つ親の削除は 23503、他テナントの子しか持たない親は削除できる。
    (
        "DELETE FROM par WHERE id = 3 USING OPERATION_ID '{op}'",
        "exec:23503",
    ),
    (
        "DELETE FROM par WHERE id = 4 USING OPERATION_ID '{op}'",
        "OK",
    ),
    // ON CONFLICT: 衝突対象が隠れた自行なら既存行の USING に従う。他テナントだけの id は衝突しない。
    (
        "INSERT INTO u (id, k, v) VALUES (2, 'k2', 'show') ON CONFLICT (id) DO NOTHING \
         USING OPERATION_ID '{op}'",
        "OK",
    ),
    (
        "INSERT INTO u (id, k, v) VALUES (2, 'k2', 'show') ON CONFLICT (id) DO UPDATE SET v = 'show' \
         USING OPERATION_ID '{op}'",
        "exec:42501",
    ),
    (
        "INSERT INTO u (id, k, v) VALUES (9, 'q9', 'show') ON CONFLICT (id) DO UPDATE SET v = 'z' \
         USING OPERATION_ID '{op}'",
        "OK",
    ),
];

fn leak_reason(sql: &str, message: &str) -> Option<String> {
    if message.contains("tenant-") {
        return Some(format!("tenant id in {message:?}"));
    }
    if message.contains('#') && !sql.contains('#') {
        return Some(format!("internal key marker in {message:?}"));
    }
    for w in WATCH {
        if message.contains(w) && !sql.contains(w) {
            return Some(format!("watch value {w} in {message:?}"));
        }
    }
    // 隠れた自行の他列の値（v = 'hide'）を文言に出さない。
    if message.starts_with("exec:") && message.contains("hide") && !sql.contains("hide") {
        return Some(format!("hidden row value in {message:?}"));
    }
    None
}

/// 全プローブを X/Y の同じ状態遷移で流し、応答がバイト一致し、観測側の期待（独立な表）に従い、
/// 他テナントの行が不変であることを固定する。
#[test]
fn constraint_verdicts_ignore_foreign_rows_and_stay_within_tenant() {
    let mut trials = 0usize;
    for obs_idx in 0..3 {
        let x = world("rls13-cs-x", obs_idx, true);
        let y = world("rls13-cs-y", obs_idx, false);
        for (i, (sql, want)) in PROBES.iter().enumerate() {
            let sql = sql.replace("{op}", &format!("op-{obs_idx}-{i}"));
            let rx = run(&x, x.obs, &sql);
            let ry = run(&y, y.obs, &sql);
            assert_eq!(rx, ry, "probe {i}: {sql}");
            assert!(
                rx.starts_with(want),
                "probe {i}: {sql}: expected {want}, got {rx}"
            );
            if let Some(why) = leak_reason(&sql, &rx) {
                panic!("probe {i}: {why}");
            }
            trials += 2;
        }
        // 他テナントにだけ在る親（9）と存在しない親（777）の拒否応答は完全一致（値・テナントを含まない）。
        let a = run(
            &x,
            x.obs,
            "INSERT INTO chi (id, pid, note) VALUES (21, 9, 'show') USING OPERATION_ID 'cmp-a'",
        );
        let b = run(
            &x,
            x.obs,
            "INSERT INTO chi (id, pid, note) VALUES (22, 777, 'show') USING OPERATION_ID 'cmp-b'",
        );
        assert!(a.starts_with("exec:23503"), "{a}");
        assert_eq!(a, b, "other-tenant parent must read as missing");
        // 他テナントの行は全プローブ後も不変（所有テナントとして RLS 無効化して読む）。
        must(&x, x.frn, "ALTER TABLE u DISABLE ROW LEVEL SECURITY");
        let dump = run(&x, x.frn, "SELECT id, k, v FROM u ORDER BY id LIMIT 100");
        let frn_rows = dump.matches("zz_secret").count();
        assert_eq!(frn_rows, 2, "foreign u rows must survive unchanged: {dump}");
        assert!(dump.contains("\"kf\""), "{dump}");
        let pars = run(&x, x.frn, "SELECT id, name FROM par ORDER BY id LIMIT 100");
        assert_eq!(pars.matches("zz_secret").count(), 2, "{pars}");
        let kids = run(
            &x,
            x.frn,
            "SELECT id, pid, note FROM chi ORDER BY id LIMIT 100",
        );
        assert_eq!(kids.matches("zz_hidden_note").count(), 1, "{kids}");
    }
    assert_eq!(trials, 3 * PROBES.len() * 2);
}

/// 一意性・`ON CONFLICT` の経路のうち COPY FROM（INSERT と同じ検査点）でも、隠れた自行のキーは `23505`、
/// 他テナントだけのキーは成功で、応答が X/Y で一致する。
#[test]
fn copy_from_uniqueness_matches_insert_scope() {
    use engine::core::CopyPlan;
    for obs_idx in 0..3 {
        let x = world("rls13-cs-cx", obs_idx, true);
        let y = world("rls13-cs-cy", obs_idx, false);
        let mut out: Vec<String> = Vec::new();
        for w in [&x, &y] {
            let s = SessionState::default();
            let mut traces = Vec::new();
            for (k, (data, want)) in [
                (b"31\tk2\tshow\n".as_slice(), "23505"),
                (b"32\tkf\tshow\n".as_slice(), "OK"),
                (b"2\tn2\tshow\n".as_slice(), "23505"),
            ]
            .iter()
            .enumerate()
            {
                let sql =
                    format!("COPY u (id, k, v) FROM STDIN USING OPERATION_ID 'cp-{obs_idx}-{k}'");
                let CopyPlan::From(mut cs) = w
                    .core
                    .begin_copy(&ctx(w.obs), &s, &sql)
                    .expect("begin_copy")
                else {
                    panic!("expected FROM plan");
                };
                cs.feed(data).expect("feed");
                let batch = cs.finish().expect("finish");
                let r = match w.core.commit_copy_in(&ctx(w.obs), batch) {
                    Ok(o) => format!("OK {}", o.rows_affected),
                    Err(e) => format!("{} {}", e.wire_code(), e.client_message()),
                };
                assert!(r.starts_with(want), "copy {k}: expected {want}, got {r}");
                assert!(leak_reason("", &format!("exec:{r}")).is_none(), "{r}");
                traces.push(r);
            }
            out.push(traces.join(" | "));
        }
        assert_eq!(
            out[0], out[1],
            "COPY FROM verdicts must match (obs={obs_idx})"
        );
    }
}

/// 検査器自身への負の対照。
#[test]
fn leak_checker_detects_fabricated_violation() {
    assert!(leak_reason("", "exec:23505 key zz_secret exists").is_some());
    assert!(leak_reason("", "exec:23503 tenant-b parent").is_some());
    assert!(leak_reason("", "exec:23505 row hide").is_some());
    assert!(leak_reason(
        "",
        "exec:23505 duplicate key value violates unique constraint"
    )
    .is_none());
}
