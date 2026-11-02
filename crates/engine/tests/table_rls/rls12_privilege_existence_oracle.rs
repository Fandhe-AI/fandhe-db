//! 権限エラーが他テナントのオブジェクトの存在オラクルにならないことの行列テスト（Issue #1972、
//! ポインタ: RLS-12・SQL-40。`docs/spec/04-behavior/` 配下。設計記録は
//! `docs/design/privilege-check-paths.md`「存在オラクルへの配慮」）。
//!
//! 既存の個別テスト（`sql40_*`・`sql39_*`・`table27_tenant_boundary`）は経路ごとに SQLSTATE を固定している。
//! 本ファイルはそれらを書き直さず、#1964 配下で権限ゲートが結線された後の「現在ロールを束縛した
//! セッション」を軸に、表・列・ロール名の 3 ケース（自テナントに在る・どこにも無い・他テナントだけに在る）を
//! 横断して固定する。守る不変条件は次の 2 つ。
//!
//! - **C≡B**: 束縛ロール（非所有者・付与なし）の応答は、他テナントだけに在る名前と、どこにも無い名前で
//!   完全一致する。「他テナントのオブジェクトが在る DB（X）」と「無い DB（Y）」の双子 DB で、観測側の
//!   応答（SQLSTATE・`client_message`・失敗した段）をバイト一致で比べる。
//! - **ゲート前置経路**: 権限ゲートが名前解決より前にある経路（未束縛／DDL 権限なしの GRANT・REVOKE・
//!   SET ROLE・OWNER TO・`DROP ROLE`・`ALTER ROLE`）では、3 ケース（A・B・C）が `42501` で完全一致する。
//!   SCHEMA 修飾の表 DDL は現状 SCHEMA の名前解決が先に走るため、自テナントの SCHEMA が在る (A) だけ
//!   `42501`、B と C は `3F000` になる。これもテナント内の区別で、B≡C だけを固定する
//!   （`gate_first_paths_are_identical_for_all_three_cases`）。
//!
//! 自テナントに在るが権限の無い名前（A）が束縛ロールで `42501`、どこにも無い名前（B）が `42P01` 等になる
//! 差は、テナント内の区別であり許容する（PostgreSQL と同じ。SQL-40 (6)(c) のポインタ）。本ファイルは
//! 許容する差を `in_tenant_denial_is_42501` で文書化するだけで、不変条件には含めない。
//!
//! 入口は autocommit・明示トランザクション・拡張クエリ（Parse／Describe／Bind／Execute）。
//! テナントは 3 つを回転させ、各プローブを 10 回以上繰り返し、試行総数を末尾で固定する。陽性対照
//! （付与で成功し取り消しで `42501` に戻る）で権限検査の空振りを防ぎ、検査器自身へ負の対照を置く。
//! 応答時間の粗い確認は単独バイナリ `tests/rls12_privilege_oracle_latency.rs` が担う。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TENANTS: [&str; 3] = ["tenant-a", "tenant-b", "tenant-c"];

/// 観測側が文に書かない、他テナントだけが知る名前。応答に現れたら存在の漏えい。
const WATCH: &[&str] = &[
    "zz_secret_s",
    "zz_secret_t",
    "zz_secret_c",
    "zz_secret_r",
    "zz_owner",
];

/// 観測ロール（束縛される主体）と同テナントの別ロールの ID（作成順）。他テナントのロールは 3。
const OBS: u64 = 1;
const PEER: u64 = 2;
const FOREIGN_OWNER: u64 = 3;

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

struct World {
    _g: CleanupGuard,
    core: EngineCore,
    /// 観測側テナント。
    tenant: &'static str,
    /// 他テナント（秘密の所有側）。
    owner_tenant: &'static str,
}

fn operator(tenant: &str) -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    assert!(s.bind_resolution_tenant(tenant));
    s
}

/// 現在ロールを束縛したセッション（`ddl` なら運用者権限つき）。
fn bound(tenant: &str, id: u64, ddl: bool) -> SessionState {
    let mut s = SessionState::default();
    s.bind_role_ids(id, id);
    assert!(s.bind_resolution_tenant(tenant));
    if ddl {
        s.allow_ddl();
    }
    s
}

fn setup(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) {
    if let Err(e) = core.execute_sql_in_session(&ctx(tenant), s, sql) {
        panic!("setup {tenant}: {sql}: {e}");
    }
}

/// 観測側テナント `obs_idx`、所有側テナント `(obs_idx + 1) % 3` の双子 DB の片方を作る。
/// `foreign` なら所有側に秘密（SCHEMA・表・列・ロール・共有表の ACL）を置く。観測側の ID（1・2）を
/// 揃えるため、ロールは両 world とも観測側 2 件 → 所有側 1 件の順に作り、秘密はその後に作る。
fn world(label: &str, obs_idx: usize, foreign: bool) -> World {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let tenant = TENANTS[obs_idx];
    let owner_tenant = TENANTS[(obs_idx + 1) % 3];
    let mut op = operator(tenant);
    setup(&core, tenant, &mut op, "CREATE ROLE obs");
    setup(&core, tenant, &mut op, "CREATE ROLE peer");
    let mut op_o = operator(owner_tenant);
    setup(&core, owner_tenant, &mut op_o, "CREATE ROLE zz_owner");
    // 観測ロールの自前の表（観測ロール自身が所有）と同テナントの別ロールの表（A ケース）。
    let mut obs_ddl = bound(tenant, OBS, true);
    setup(
        &core,
        tenant,
        &mut obs_ddl,
        "CREATE TABLE mine (id BIGINT PRIMARY KEY, n INTEGER)",
    );
    setup(
        &core,
        tenant,
        &mut obs_ddl,
        "INSERT INTO mine (id, n) VALUES (1, 10)",
    );
    let mut peer_ddl = bound(tenant, PEER, true);
    setup(
        &core,
        tenant,
        &mut peer_ddl,
        "CREATE TABLE peer_t (id BIGINT PRIMARY KEY, n INTEGER)",
    );
    setup(
        &core,
        tenant,
        &mut peer_ddl,
        "INSERT INTO peer_t (id, n) VALUES (1, 10)",
    );
    if foreign {
        let mut o = bound(owner_tenant, FOREIGN_OWNER, true);
        setup(&core, owner_tenant, &mut o, "CREATE SCHEMA zz_secret_s");
        setup(
            &core,
            owner_tenant,
            &mut o,
            "CREATE TABLE zz_secret_s.zz_secret_t (id BIGINT PRIMARY KEY, zz_secret_c TEXT)",
        );
        setup(
            &core,
            owner_tenant,
            &mut o,
            "CREATE TABLE pub_owned (id BIGINT PRIMARY KEY, n INTEGER)",
        );
        setup(
            &core,
            owner_tenant,
            &mut o,
            "GRANT SELECT ON pub_owned TO zz_owner",
        );
        setup(
            &core,
            owner_tenant,
            &mut o,
            "GRANT SELECT (n) ON pub_owned TO zz_owner",
        );
        let mut op_o = operator(owner_tenant);
        setup(&core, owner_tenant, &mut op_o, "CREATE ROLE zz_secret_r");
    } else {
        // 同名の共有表を観測側テナントの別ロールが所有する（所有者のテナントが応答に出ないことの対照）。
        setup(
            &core,
            tenant,
            &mut peer_ddl,
            "CREATE TABLE pub_owned (id BIGINT PRIMARY KEY, n INTEGER)",
        );
    }
    World {
        _g: guard,
        core,
        tenant,
        owner_tenant,
    }
}

/// 入口。
#[derive(Clone, Copy, Debug)]
enum Entry {
    Auto,
    Txn,
    Ext,
}

fn fmt_err(stage: &str, e: &SqlSurfaceError) -> String {
    format!("{stage}:{} {}", e.wire_code(), e.client_message())
}

/// 文を 1 つの入口で実行し、段ごとの応答を 1 本の文字列（比較用の形）にまとめる。成功は "OK ..."。
fn trace(w: &World, entry: Entry, mk: &dyn Fn() -> SessionState, sql: &str) -> String {
    let c = ctx(w.tenant);
    let mut s = mk();
    match entry {
        Entry::Auto => match w.core.execute_sql_in_session(&c, &mut s, sql) {
            Ok(o) => format!("OK {o:?}"),
            Err(e) => fmt_err("exec", &e),
        },
        Entry::Txn => {
            let mut txn = w.core.new_session_transaction();
            let mut out = Vec::new();
            for step in ["BEGIN", sql, "SELECT id FROM mine LIMIT 1", "ROLLBACK"] {
                out.push(
                    match w.core.execute_sql_in_txn(&c, &mut s, &mut txn, step) {
                        Ok(o) => format!("OK {o:?}"),
                        Err(e) => fmt_err("txn", &e),
                    },
                );
            }
            out.join(" | ")
        }
        Entry::Ext => {
            let p = match w
                .core
                .parse_sql_prepared_with_declared_in_session(&s, sql, &[])
            {
                Ok(p) => p,
                Err(e) => return fmt_err("parse", &e),
            };
            let mut t = String::new();
            if let Err(e) = w.core.describe_prepared_in_session(&s, &p) {
                t.push_str(&fmt_err("describe", &e));
                t.push(';');
            }
            let b = match w.core.bind_prepared_in_session(&s, &p, &[]) {
                Ok(b) => b,
                Err(e) => return t + &fmt_err("bind", &e),
            };
            match w.core.execute_parsed_in_session(&c, &mut s, &b) {
                Ok(o) => t + &format!("OK {o:?}"),
                Err(e) => t + &fmt_err("execute", &e),
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// プローブ行列
// ---------------------------------------------------------------------------------------------

/// 1 つのプローブ。`ddl` は観測ロールに運用者権限を与えるか、`code` は autocommit で期待する SQLSTATE
/// （空振り＝構文エラー等で名前解決まで届かない状態の防止）。
struct Probe {
    sql: &'static str,
    ddl: bool,
    code: &'static str,
}

const fn p(sql: &'static str, code: &'static str) -> Probe {
    Probe {
        sql,
        ddl: false,
        code,
    }
}

const fn pd(sql: &'static str, code: &'static str) -> Probe {
    Probe {
        sql,
        ddl: true,
        code,
    }
}

/// 表（他テナントの SCHEMA 配下の表を観測側が修飾名で参照する。C: X に在る／B: Y には無い）。
const TABLE_PROBES: &[Probe] = &[
    p("SELECT id FROM zz_secret_s.zz_secret_t", "3F000"),
    p("SELECT COUNT(*) FROM zz_secret_s.zz_secret_t", "3F000"),
    p(
        "SELECT id FROM mine WHERE id IN (SELECT id FROM zz_secret_s.zz_secret_t)",
        "3F000",
    ),
    p(
        "WITH c AS (SELECT id FROM zz_secret_s.zz_secret_t) SELECT id FROM c",
        "3F000",
    ),
    p("EXPLAIN SELECT id FROM zz_secret_s.zz_secret_t", "3F000"),
    p(
        "DECLARE c CURSOR FOR SELECT id FROM zz_secret_s.zz_secret_t",
        "3F000",
    ),
    p("SELECT id FROM zz_secret_s.zz_secret_t FOR UPDATE", "3F000"),
    p(
        "INSERT INTO zz_secret_s.zz_secret_t (id) VALUES (1)",
        "3F000",
    ),
    p("UPDATE zz_secret_s.zz_secret_t SET id = 2", "3F000"),
    p("DELETE FROM zz_secret_s.zz_secret_t", "3F000"),
    p("TRUNCATE zz_secret_s.zz_secret_t", "3F000"),
    p("GRANT SELECT ON zz_secret_s.zz_secret_t TO obs", "3F000"),
    p("REVOKE SELECT ON zz_secret_s.zz_secret_t FROM obs", "3F000"),
    p("COPY zz_secret_s.zz_secret_t TO STDOUT", "3F000"),
    p("COPY zz_secret_s.zz_secret_t (id) FROM STDIN", "3F000"),
    pd("DROP TABLE zz_secret_s.zz_secret_t", "3F000"),
    pd(
        "ALTER TABLE zz_secret_s.zz_secret_t ADD COLUMN x INTEGER",
        "3F000",
    ),
    pd(
        "CREATE INDEX zz_ix ON zz_secret_s.zz_secret_t (id)",
        "3F000",
    ),
    pd("ALTER TABLE zz_secret_s.zz_secret_t OWNER TO obs", "3F000"),
    // 他テナントが所有する共有表（`pub_owned`）。Y では観測側の別ロールが所有する。所有者のテナントは
    // 応答に出ない。
    p("SELECT n FROM pub_owned", "42501"),
    p("SELECT id FROM pub_owned", "42501"),
    p("GRANT SELECT ON pub_owned TO peer", "42501"),
    p("INSERT INTO pub_owned (id, n) VALUES (9, 9)", "42501"),
];

/// 列（他テナントの SCHEMA 配下の表の列・列 ACL の付いた共有表の列）。
const COLUMN_PROBES: &[Probe] = &[
    p("SELECT zz_secret_c FROM zz_secret_s.zz_secret_t", "3F000"),
    p(
        "UPDATE zz_secret_s.zz_secret_t SET zz_secret_c = 'x'",
        "3F000",
    ),
    p(
        "INSERT INTO zz_secret_s.zz_secret_t (zz_secret_c) VALUES ('x')",
        "3F000",
    ),
    p(
        "GRANT SELECT (zz_secret_c) ON zz_secret_s.zz_secret_t TO obs",
        "3F000",
    ),
    pd(
        "ALTER TABLE zz_secret_s.zz_secret_t RENAME COLUMN zz_secret_c TO y",
        "3F000",
    ),
    // 自テナントの表に対する不在列は名前解決が権限判定より前（テナント内の区別）。両 world で同じ。
    p("SELECT zz_secret_c FROM mine", "42703"),
    p("UPDATE mine SET zz_secret_c = 'x'", "42703"),
    // 他テナントが列 ACL を付けた共有表の列。観測側の応答は ACL の有無（X/Y）に依存しない。
    p("SELECT zz_secret_c FROM pub_owned", "42501"),
    p("SELECT n FROM pub_owned WHERE n > 0", "42501"),
];

/// ロール名（他テナントのロール `zz_secret_r`。X にだけ在る）。観測ロールは `mine` の所有者。
const ROLE_PROBES: &[Probe] = &[
    p("GRANT SELECT ON mine TO zz_secret_r", "42704"),
    p("REVOKE SELECT ON mine FROM zz_secret_r", "42704"),
    p("GRANT zz_secret_r TO obs", "42704"),
    p("REVOKE zz_secret_r FROM obs", "42704"),
    p("SET ROLE zz_secret_r", "42704"),
    p("ALTER TABLE mine OWNER TO zz_secret_r", "42704"),
    p(
        "ALTER DEFAULT PRIVILEGES FOR ROLE zz_secret_r GRANT SELECT ON TABLES TO obs",
        "42704",
    ),
    p(
        "ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO zz_secret_r",
        "42704",
    ),
    pd("DROP ROLE zz_secret_r", "42704"),
    pd("ALTER ROLE zz_secret_r NOLOGIN", "42704"),
];

fn all_probes() -> Vec<&'static Probe> {
    TABLE_PROBES
        .iter()
        .chain(COLUMN_PROBES)
        .chain(ROLE_PROBES)
        .collect()
}

/// 応答に、観測側が文に書いていない他テナントだけの名前・テナント ID・内部キーが現れたら理由を返す。
/// 違反があれば理由を返す（`leak_checker_detects_fabricated_violation` が捏造した違反を与える）。
fn leak_reason(sql: &str, message: &str) -> Option<String> {
    if message.contains('#') {
        return Some(format!("internal key marker in {message:?}"));
    }
    if message.contains("tenant-") {
        return Some(format!("tenant id in {message:?}"));
    }
    for w in WATCH {
        if message.contains(w) && !sql.contains(w) {
            return Some(format!(
                "watch name {w} not written by caller in {message:?}"
            ));
        }
    }
    None
}

/// 対象の文が失敗していること（`Txn` は BEGIN の次の段）。
fn stmt_failed(entry: Entry, t: &str) -> bool {
    match entry {
        Entry::Txn => t.split(" | ").nth(1).is_some_and(|s| !s.starts_with("OK")),
        _ => !t.contains("OK "),
    }
}

fn assert_no_leak(sql: &str, t: &str) {
    if let Some(why) = leak_reason(sql, t) {
        panic!("{sql}: {why}");
    }
}

/// 同じ SQL を観測側の束縛ロールで X・Y の双方に `repeats` 回流し、全応答がバイト一致することを検査する。
/// 返り値は実行した試行数。
fn compare_probes(
    probes: &[&Probe],
    entry: Entry,
    repeats: usize,
    check: impl Fn(&Probe, &str),
) -> usize {
    let mut trials = 0;
    for obs_idx in 0..3 {
        let x = world("rls12-x", obs_idx, true);
        let y = world("rls12-y", obs_idx, false);
        for pr in probes {
            let mk = || bound(x.tenant, OBS, pr.ddl);
            let first = trace(&x, entry, &mk, pr.sql);
            for _ in 0..repeats {
                let rx = trace(&x, entry, &mk, pr.sql);
                let ry = trace(&y, entry, &mk, pr.sql);
                assert_eq!(rx, ry, "{} ({entry:?}, obs={obs_idx})", pr.sql);
                assert_eq!(rx, first, "{} unstable across repeats", pr.sql);
                if !matches!(entry, Entry::Txn) {
                    assert!(!rx.contains("OK "), "{} must fail: {rx}", pr.sql);
                }
                assert_no_leak(pr.sql, &rx);
                check(pr, &rx);
                trials += 2;
            }
        }
    }
    trials
}

fn assert_code_autocommit(pr: &Probe, t: &str) {
    assert!(
        t.starts_with(&format!("exec:{} ", pr.code)),
        "{}: expected {} got {t}",
        pr.sql,
        pr.code
    );
}

#[test]
fn tables_cross_tenant_matches_absent_autocommit() {
    let probes: Vec<&Probe> = TABLE_PROBES.iter().collect();
    let trials = compare_probes(&probes, Entry::Auto, 10, assert_code_autocommit);
    assert_eq!(
        trials,
        3 * TABLE_PROBES.len() * 10 * 2,
        "trial count drifted"
    );
}

#[test]
fn columns_cross_tenant_matches_absent_autocommit() {
    let probes: Vec<&Probe> = COLUMN_PROBES.iter().collect();
    let trials = compare_probes(&probes, Entry::Auto, 10, assert_code_autocommit);
    assert_eq!(
        trials,
        3 * COLUMN_PROBES.len() * 10 * 2,
        "trial count drifted"
    );
}

#[test]
fn role_names_cross_tenant_matches_absent_autocommit() {
    let probes: Vec<&Probe> = ROLE_PROBES.iter().collect();
    let trials = compare_probes(&probes, Entry::Auto, 10, assert_code_autocommit);
    assert_eq!(
        trials,
        3 * ROLE_PROBES.len() * 10 * 2,
        "trial count drifted"
    );
}

/// 明示トランザクション: 文の失敗が X/Y で同じ応答になり、後続の文は中断済み（fail-closed）、
/// ROLLBACK で解ける。
#[test]
fn cross_tenant_matches_absent_in_explicit_transaction() {
    let probes = all_probes();
    let trials = compare_probes(&probes, Entry::Txn, 10, |pr, t| {
        let steps: Vec<&str> = t.split(" | ").collect();
        assert_eq!(steps.len(), 4, "{}: {t}", pr.sql);
        assert!(steps[0].starts_with("OK"), "{}: {t}", pr.sql);
        assert!(!steps[1].starts_with("OK"), "{}: {t}", pr.sql);
        assert!(!steps[2].starts_with("OK"), "{}: {t}", pr.sql);
        assert!(steps[3].starts_with("OK"), "{}: {t}", pr.sql);
    });
    assert_eq!(trials, 3 * probes.len() * 10 * 2, "trial count drifted");
}

/// 拡張クエリ: Parse／Describe／Bind／Execute のどの段で失敗するかも含めて X/Y が一致する。
#[test]
fn cross_tenant_matches_absent_in_extended_query() {
    let probes = all_probes();
    let trials = compare_probes(&probes, Entry::Ext, 10, |pr, t| {
        assert!(
            t.contains(pr.code),
            "{}: expected {} in {t}",
            pr.sql,
            pr.code
        );
    });
    assert_eq!(trials, 3 * probes.len() * 10 * 2, "trial count drifted");
}

/// COPY の入口（`begin_copy`・`begin_copy_in_txn`）でも X/Y が一致する。
#[test]
fn cross_tenant_matches_absent_in_copy() {
    let mut trials = 0;
    for obs_idx in 0..3 {
        let x = world("rls12-cx", obs_idx, true);
        let y = world("rls12-cy", obs_idx, false);
        let c = ctx(x.tenant);
        let s = bound(x.tenant, OBS, false);
        for sql in [
            "COPY zz_secret_s.zz_secret_t (id, zz_secret_c) FROM STDIN",
            "COPY zz_secret_s.zz_secret_t TO STDOUT",
            "COPY (SELECT id FROM zz_secret_s.zz_secret_t) TO STDOUT",
            "COPY pub_owned (id, n) FROM STDIN",
            "COPY pub_owned TO STDOUT",
        ] {
            let auto = |w: &World| {
                w.core
                    .begin_copy(&c, &s, sql)
                    .map(|_| ())
                    .map_err(|e| fmt_err("copy", &e))
            };
            let (ax, ay) = (auto(&x), auto(&y));
            assert!(ax.is_err(), "{sql}");
            assert_eq!(format!("{ax:?}"), format!("{ay:?}"), "{sql}");
            assert_no_leak(sql, &format!("{ax:?}"));
            trials += 1;
            let in_txn = |w: &World| {
                let mut txn = w.core.new_session_transaction();
                let mut sx = bound(w.tenant, OBS, false);
                w.core
                    .execute_sql_in_txn(&c, &mut sx, &mut txn, "BEGIN")
                    .expect("begin");
                w.core
                    .begin_copy_in_txn(&c, &s, &mut txn, sql)
                    .map(|_| ())
                    .map_err(|e| fmt_err("copy", &e))
            };
            let (bx, by) = (in_txn(&x), in_txn(&y));
            assert!(bx.is_err(), "{sql}");
            assert_eq!(format!("{bx:?}"), format!("{by:?}"), "{sql}");
            trials += 1;
        }
    }
    assert_eq!(trials, 3 * 5 * 2, "trial count drifted");
}

// ---------------------------------------------------------------------------------------------
// ゲート前置経路: 自テナントに在る (A)・どこにも無い (B)・他テナントだけに在る (C) が完全一致
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Kind {
    A,
    B,
    C,
}

/// 同じ名前（`g_s.g_t`・`g_r`）を、A は観測側テナント・C は他テナントに作り、B は作らない。
fn gate_world(obs_idx: usize, kind: Kind) -> World {
    let w = world("rls12-gate", obs_idx, false);
    let place = match kind {
        Kind::A => Some(w.tenant),
        Kind::B => None,
        Kind::C => Some(w.owner_tenant),
    };
    if let Some(t) = place {
        let mut op = operator(t);
        setup(&w.core, t, &mut op, "CREATE SCHEMA g_s");
        setup(
            &w.core,
            t,
            &mut op,
            "CREATE TABLE g_s.g_t (id BIGINT PRIMARY KEY, c TEXT)",
        );
        setup(&w.core, t, &mut op, "CREATE ROLE g_r");
    }
    w
}

fn unbound_session(t: &str) -> SessionState {
    let mut s = SessionState::default();
    assert!(s.bind_resolution_tenant(t));
    s
}

#[test]
fn gate_first_paths_are_identical_for_all_three_cases() {
    // 第 2 要素が true の文は A≡B≡C（権限ゲートが名前解決より前）。false は SCHEMA 修飾の表 DDL で、
    // 現状は SCHEMA の名前解決が先に走るため、自テナントに SCHEMA が在る (A) だけ `42501`、B と C は
    // `3F000` になる。これはテナント内の区別（他テナントの存在は B と同一）なので B≡C だけを固定する。
    // 未束縛（運用者権限なし）: GRANT／REVOKE／SET ROLE／OWNER TO・ロール DDL は存在判定より前に `42501`。
    let unbound_sqls: [(&str, bool); 10] = [
        ("GRANT SELECT ON g_s.g_t TO g_r", true),
        ("REVOKE SELECT ON g_s.g_t FROM g_r", true),
        ("GRANT g_r TO obs", true),
        ("SET ROLE g_r", true),
        ("ALTER TABLE g_s.g_t OWNER TO g_r", true),
        ("DROP TABLE g_s.g_t", false),
        ("ALTER TABLE g_s.g_t DROP COLUMN c", false),
        ("CREATE INDEX g_ix ON g_s.g_t (c)", false),
        ("DROP ROLE g_r", true),
        ("ALTER ROLE g_r NOLOGIN", true),
    ];
    // 束縛ロール（運用者権限なし）のロール DDL も、カタログ照会より前に `42501`。
    let bound_sqls: [(&str, bool); 5] = [
        ("DROP TABLE g_s.g_t", false),
        ("ALTER TABLE g_s.g_t DROP COLUMN c", false),
        ("CREATE INDEX g_ix ON g_s.g_t (c)", false),
        ("DROP ROLE g_r", true),
        ("ALTER ROLE g_r NOLOGIN", true),
    ];
    let mut trials = 0;
    for obs_idx in 0..3 {
        let worlds = [Kind::A, Kind::B, Kind::C].map(|k| gate_world(obs_idx, k));
        for (label, sqls, is_bound) in [
            ("unbound", &unbound_sqls[..], false),
            ("bound", &bound_sqls[..], true),
        ] {
            for &(sql, strict) in sqls {
                for entry in [Entry::Auto, Entry::Txn, Entry::Ext] {
                    let traces: Vec<String> = worlds
                        .iter()
                        .map(|w| {
                            trace(
                                w,
                                entry,
                                &|| {
                                    if is_bound {
                                        bound(w.tenant, OBS, false)
                                    } else {
                                        unbound_session(w.tenant)
                                    }
                                },
                                sql,
                            )
                        })
                        .collect();
                    // 他テナント (C) とどこにも無い (B) は常に一致する。
                    assert_eq!(
                        traces[1], traces[2],
                        "{label} {sql} ({entry:?}): {traces:?}"
                    );
                    assert!(stmt_failed(entry, &traces[1]), "{label} {sql}: {traces:?}");
                    assert_no_leak(sql, &traces[1]);
                    // 拡張クエリの Parse 段は名前解決を伴うため、strict な文でも A の区別が出る
                    // （テナント内の区別で許容）。
                    if strict && !matches!(entry, Entry::Ext) {
                        assert_eq!(
                            traces[0], traces[1],
                            "{label} {sql} ({entry:?}): {traces:?}"
                        );
                        assert!(traces[0].contains("42501"), "{label} {sql}: {}", traces[0]);
                    }
                    trials += 3;
                }
            }
        }
    }
    assert_eq!(
        trials,
        3 * (unbound_sqls.len() + bound_sqls.len()) * 3 * 3,
        "trial count drifted"
    );
}

// ---------------------------------------------------------------------------------------------
// 契約の文書化・陽性対照・副作用なし・負の対照
// ---------------------------------------------------------------------------------------------

/// 自テナントに在るが権限の無い表・列・ロール（A）は束縛ロールで `42501`。B との差はテナント内の区別で
/// 許容する（不変条件ではなく契約の明示。SQL-40 (6)(c) のポインタ）。
#[test]
fn in_tenant_denial_is_42501() {
    let w = world("rls12-a", 0, false);
    let mut n = 0;
    for sql in [
        "SELECT id FROM peer_t",
        "SELECT n FROM peer_t",
        "INSERT INTO peer_t (id, n) VALUES (2, 2)",
        "UPDATE peer_t SET n = 1",
        "DELETE FROM peer_t",
        "TRUNCATE peer_t",
        "GRANT SELECT ON peer_t TO obs",
        "ALTER TABLE peer_t OWNER TO obs",
        "SET ROLE peer",
    ] {
        let t = trace(&w, Entry::Auto, &|| bound(w.tenant, OBS, false), sql);
        assert!(t.starts_with("exec:42501 "), "{sql}: {t}");
        n += 1;
    }
    assert_eq!(n, 9, "trial count drifted");
}

/// 陽性対照: 付与で成功し、取り消しで `42501` に戻る（権限検査が生きており、空振りでないこと）。
#[test]
fn positive_control_grant_makes_probes_succeed() {
    let w = world("rls12-pos", 0, true);
    let t = w.tenant;
    let run = |s: &mut SessionState, sql: &str| w.core.execute_sql_in_session(&ctx(t), s, sql);
    let code = |s: &mut SessionState, sql: &str| run(s, sql).expect_err(sql).wire_code();
    let mut peer = bound(t, PEER, false);
    let mut o = bound(t, OBS, false);
    assert_eq!(code(&mut o, "SELECT id FROM peer_t"), "42501");
    run(&mut peer, "GRANT SELECT ON peer_t TO obs").expect("grant");
    run(&mut o, "SELECT id FROM peer_t").expect("granted select succeeds");
    run(&mut peer, "REVOKE SELECT ON peer_t FROM obs").expect("revoke");
    assert_eq!(code(&mut o, "SELECT id FROM peer_t"), "42501");
    // 他テナントが共有表へ付けた ACL（zz_owner 宛て）は観測ロールに効かない。
    assert_eq!(code(&mut o, "SELECT n FROM pub_owned"), "42501");
}

/// 全プローブの前後で、他テナントの観測（所有側から見える状態）が不変（副作用ゼロ）。
#[test]
fn attempts_leave_foreign_state_unchanged() {
    let snapshot = |w: &World| {
        let mut o = bound(w.owner_tenant, FOREIGN_OWNER, false);
        let mut out = Vec::new();
        for sql in [
            "SELECT id FROM zz_secret_s.zz_secret_t",
            "SELECT zz_secret_c FROM zz_secret_s.zz_secret_t",
            "SELECT n FROM pub_owned",
            "SET ROLE zz_secret_r",
        ] {
            out.push(
                match w
                    .core
                    .execute_sql_in_session(&ctx(w.owner_tenant), &mut o, sql)
                {
                    Ok(r) => format!("OK {r:?}"),
                    Err(e) => fmt_err("exec", &e),
                },
            );
        }
        out
    };
    let probed = world("rls12-st-x", 1, true);
    let fresh = world("rls12-st-f", 1, true);
    let before = snapshot(&probed);
    for pr in all_probes() {
        for entry in [Entry::Auto, Entry::Txn, Entry::Ext] {
            let _ = trace(
                &probed,
                entry,
                &|| bound(probed.tenant, OBS, pr.ddl),
                pr.sql,
            );
        }
    }
    assert_eq!(before, snapshot(&probed));
    assert_eq!(snapshot(&fresh), snapshot(&probed));
    // 観測側の自前のデータも不変。
    let mut o = bound(probed.tenant, OBS, false);
    let r = probed
        .core
        .execute_sql_in_session(&ctx(probed.tenant), &mut o, "SELECT id, n FROM mine")
        .expect("own table readable");
    assert!(format!("{r:?}").contains("SignedInteger(10)"));
}

/// 検査器自身への負の対照: 捏造した違反を検出できる。
#[test]
fn leak_checker_detects_fabricated_violation() {
    assert!(leak_reason("SELECT 1", "relation zz_secret_t exists").is_some());
    assert!(leak_reason("SELECT 1", "owned by tenant-b").is_some());
    assert!(leak_reason("SELECT 1", "key #3.x").is_some());
    // 観測側が自分で書いた名前は漏えいではない。
    assert!(leak_reason("SELECT id FROM zz_secret_t", "relation zz_secret_t missing").is_none());
}
