//! ポリシー式・関数・部分クエリの評価範囲が他テナントの行へ届かないことの回帰（Issue #2281、
//! ポインタ: `docs/spec/04-behavior/rls.md` RLS-13・補助 RLS-12・SQL-67。設計記録は
//! `docs/design/row-security-policy-composition.md`「非漏えい試験（#2281）」節）。
//!
//! 固定する契約（`rls13_policy_tenant_nonleak.rs` と同じ双子 DB 方式。X は他テナントが Private 行・
//! 同名の関数・表を持つ DB、Y は持たない DB）:
//! 1. ポリシー式に書いた部分クエリ・利用者定義関数・`current_user` は DDL 段で拒否され、拒否応答が
//!    参照名を他テナントが持つか否かに依存せず、他テナントの名前・値を含まない
//! 2. `SECURITY DEFINER` は構文段で `0A000`（定義者権限で他テナントの行へ届く関数を作れない）で、
//!    応答が同名の関数を他テナントが持つか否かに依存しない
//! 3. 呼び出し元権限の `LANGUAGE sql` 関数・`DO` ブロック・部分クエリ・CTE・ビューが適用表を読むと、
//!    「自テナントかつポリシーを通る行」だけを観測する。他テナントの同名・別本体の関数は結果に影響しない

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TENANTS: [&str; 3] = ["tenant-a", "tenant-b", "tenant-c"];

/// 他テナントだけが持つ名前・値。観測側の応答に現れたら存在の漏えい。
const WATCH: &[&str] = &["zz_secret_t", "zz_secret_f", "zz_secret_val"];

/// 観測側の行の `(id, lang)`。ポリシー `lang = 'ja'` を通るのは id 1・3。
const OBS_ROWS: [(u64, &str); 4] = [(1, "ja"), (2, "en"), (3, "ja"), (4, "fr")];
const VISIBLE_IDS: [u64; 2] = [1, 3];

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

struct World {
    _g: CleanupGuard,
    core: EngineCore,
    obs: &'static str,
}

fn seed(storage: &Storage, tenant: &str, id: u64, vis: Visibility, lang: &str) {
    engine::tenant::insert_typed_row(
        storage,
        "docs",
        &ctx(tenant),
        id,
        vis,
        &[
            Value::Vector(vec![1.0, id as f32 * 0.01]),
            Value::Text(lang.to_string()),
            Value::Text(format!("zz_secret_val{id}")),
        ],
        &OperationId::parse(&format!("seed-{tenant}-{id}")).expect("op id"),
    )
    .expect("seed row");
}

fn exec(w: &World, tenant: &str, sql: &str) -> Result<SqlOutcome, String> {
    let mut s = SessionState::default();
    s.allow_ddl();
    w.core
        .execute_sql_in_session(&ctx(tenant), &mut s, sql)
        .map_err(|e| format!("exec:{} {}", e.wire_code(), e.client_message()))
}

fn must(w: &World, tenant: &str, sql: &str) {
    if let Err(e) = exec(w, tenant, sql) {
        panic!("setup {tenant}: {sql}: {e}");
    }
}

/// 観測側の表・行・ポリシー・関数を先に作り（X/Y で内部 ID を揃える）、`foreign` のときだけ他テナントの
/// Private 行・同名の関数（別本体）・観測側に無い表・関数を置く。
fn world(label: &str, obs_idx: usize, foreign: bool) -> World {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("secret", ColumnType::Text, false),
            ],
        ))
        .expect("create docs");
    let obs = TENANTS[obs_idx];
    let frn = TENANTS[(obs_idx + 1) % 3];
    for (id, lang) in OBS_ROWS {
        let vis = if id % 2 == 0 {
            Visibility::Private
        } else {
            Visibility::Public
        };
        seed(&storage, obs, id, vis, lang);
    }
    if foreign {
        for id in [1, 6, 7] {
            seed(&storage, frn, id, Visibility::Private, "ja");
        }
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let w = World {
        _g: guard,
        core,
        obs,
    };
    must(&w, obs, "ALTER TABLE docs ENABLE ROW LEVEL SECURITY");
    must(&w, obs, "ALTER TABLE docs FORCE ROW LEVEL SECURITY");
    must(
        &w,
        obs,
        "CREATE POLICY pj ON docs AS PERMISSIVE FOR ALL USING (lang = 'ja')",
    );
    must(&w, obs, "CREATE TABLE probe (tag TEXT, v BIGINT)");
    must(
        &w,
        obs,
        "CREATE FUNCTION obs_cnt() RETURNS bigint LANGUAGE sql AS $$ SELECT count(*) FROM docs $$",
    );
    must(&w, obs, "CREATE VIEW dv AS SELECT id FROM docs");
    if foreign {
        must(&w, frn, "CREATE TABLE zz_secret_t (id BIGINT PRIMARY KEY)");
        must(
            &w,
            frn,
            "CREATE FUNCTION zz_secret_f() RETURNS bigint LANGUAGE sql AS $$ SELECT 999 $$",
        );
        // 観測側と同名・別本体の関数（呼ばれたら値が 999 になる）。
        must(
            &w,
            frn,
            "CREATE FUNCTION obs_cnt() RETURNS bigint LANGUAGE sql AS $$ SELECT 999 $$",
        );
        must(&w, frn, "ALTER TABLE docs ENABLE ROW LEVEL SECURITY");
        must(
            &w,
            frn,
            "CREATE POLICY zz_secret_pol ON docs AS PERMISSIVE FOR ALL USING (true)",
        );
    }
    w
}

/// 呼び出し専用セッション（DDL 権限なし・カタログから利用者定義関数をロード）で `sql` を実行し、
/// 比較用文字列を返す。
fn call(w: &World, sql: &str) -> String {
    call_with(w, sql, false)
}

/// `ddl` なら運用者権限つきのセッションで実行する（DDL 段の拒否応答を権限エラーで隠さないため）。
fn call_with(w: &World, sql: &str, ddl: bool) -> String {
    let c = ctx(w.obs);
    let mut s = SessionState::default();
    if ddl {
        s.allow_ddl();
    }
    if let Err(e) = w.core.load_session_udfs(&c, &mut s) {
        return format!("load:{} {}", e.wire_code(), e.client_message());
    }
    match w.core.execute_sql_in_session(&c, &mut s, sql) {
        Ok(SqlOutcome::Query(q)) => format!("OK {:?}", q.rows),
        Ok(o) => format!("OK {o:?}"),
        Err(e) => format!("exec:{} {}", e.wire_code(), e.client_message()),
    }
}

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
    None
}

fn ids_of(w: &World, sql: &str) -> Vec<u64> {
    let c = ctx(w.obs);
    let mut s = SessionState::default();
    w.core.load_session_udfs(&c, &mut s).expect("load udfs");
    match w.core.execute_sql_in_session(&c, &mut s, sql) {
        Ok(SqlOutcome::Query(q)) => {
            let mut v: Vec<u64> = q.rows.iter().map(|r| r.id).collect();
            v.sort_unstable();
            v
        }
        other => panic!("{sql}: {other:?}"),
    }
}

fn int(c: &Cell) -> i64 {
    match c {
        Cell::Integer(v) => i64::try_from(*v).expect("fits"),
        Cell::SignedInteger(v) => *v,
        other => panic!("expected integer cell, got {other:?}"),
    }
}

/// ポリシー式の部分クエリ・関数・`current_user`、および `SECURITY DEFINER` の拒否応答が X/Y で一致し、
/// 受理されない（ポリシーは作られない）。
#[test]
fn policy_expression_and_definer_rejections_are_identical() {
    let probes: &[(&str, &str)] = &[
        // 他テナントだけが持つ表・関数を名指す式と、どこにも無い名前を名指す式。
        (
            "CREATE POLICY e1 ON docs USING (id IN (SELECT id FROM zz_secret_t))",
            "",
        ),
        ("CREATE POLICY e2 ON docs USING (zz_secret_f() > 0)", ""),
        ("CREATE POLICY e3 ON docs USING (nowhere_f() > 0)", ""),
        ("CREATE POLICY e4 ON docs USING (lang = current_user)", ""),
        (
            "CREATE POLICY e5 ON docs USING (EXISTS (SELECT 1 FROM docs))",
            "",
        ),
        ("CREATE POLICY e6 ON docs USING (obs_cnt() > 0)", ""),
        (
            "CREATE FUNCTION zz_secret_f() RETURNS bigint LANGUAGE sql SECURITY DEFINER \
             AS $$ SELECT 1 $$",
            "0A000",
        ),
        (
            "CREATE FUNCTION nowhere_g() RETURNS bigint LANGUAGE sql SECURITY DEFINER \
             AS $$ SELECT 1 $$",
            "0A000",
        ),
    ];
    for obs_idx in 0..3 {
        let x = world("rls13-ex-px", obs_idx, true);
        let y = world("rls13-ex-py", obs_idx, false);
        for (sql, code) in probes {
            for _ in 0..10 {
                let rx = call_with(&x, sql, true);
                let ry = call_with(&y, sql, true);
                assert_eq!(rx, ry, "{sql}");
                assert!(rx.starts_with("exec:"), "{sql} must be rejected: {rx}");
                // 権限エラーでの空振りを防ぐ（DDL 段の機能拒否・構文拒否であること）。
                assert!(!rx.starts_with("exec:42501"), "{sql}: {rx}");
                if !code.is_empty() {
                    assert!(rx.starts_with(&format!("exec:{code} ")), "{sql}: {rx}");
                }
                if let Some(why) = leak_reason(sql, &rx) {
                    panic!("{sql}: {why}");
                }
            }
        }
        // 拒否されたポリシーは作られていない（観測側の可視集合は元のポリシーのまま）。
        assert_eq!(
            ids_of(&x, "SELECT id FROM docs LIMIT 100"),
            VISIBLE_IDS.to_vec()
        );
    }
}

/// 呼び出し元権限の `LANGUAGE sql` 関数・`DO`・部分クエリ・CTE・ビューは、適用表を読むと自テナントかつ
/// ポリシーを通る行だけを観測し、X/Y で一致する（他テナントの同名・別本体の関数は結果に影響しない）。
#[test]
fn invoker_paths_observe_only_own_policy_visible_rows() {
    for obs_idx in 0..3 {
        let x = world("rls13-ex-ix", obs_idx, true);
        let y = world("rls13-ex-iy", obs_idx, false);
        // 関数: 観測側自身の本体が呼ばれ、結果は可視行数。
        for sql in [
            "SELECT obs_cnt()",
            "SELECT id FROM docs WHERE id IN (SELECT id FROM docs) ORDER BY id LIMIT 100",
            "WITH c AS (SELECT id FROM docs) SELECT id FROM c ORDER BY id LIMIT 100",
            "SELECT id FROM dv ORDER BY id LIMIT 100",
            "SELECT COUNT(*) FROM docs",
        ] {
            for _ in 0..10 {
                let rx = call(&x, sql);
                let ry = call(&y, sql);
                assert_eq!(rx, ry, "{sql}");
                assert!(rx.starts_with("OK"), "{sql}: {rx}");
                if let Some(why) = leak_reason(sql, &rx) {
                    panic!("{sql}: {why}");
                }
            }
        }
        let c = call(&x, "SELECT obs_cnt()");
        assert!(c.contains(&format!("{}", VISIBLE_IDS.len())), "{c}");
        assert!(!c.contains("999"), "foreign same-name function ran: {c}");
        for sql in [
            "SELECT id FROM docs WHERE id IN (SELECT id FROM docs) ORDER BY id LIMIT 100",
            "WITH c AS (SELECT id FROM docs) SELECT id FROM c ORDER BY id LIMIT 100",
            "SELECT id FROM dv ORDER BY id LIMIT 100",
        ] {
            assert_eq!(ids_of(&x, sql), VISIBLE_IDS.to_vec(), "{sql}");
        }
        // DO ブロック: 本体が適用表を読んだ件数を観測側の probe へ書く。
        for (w, tag) in [(&x, "x"), (&y, "y")] {
            let body = format!(
                "DECLARE c bigint; BEGIN SELECT count(*) INTO c FROM docs; \
                 INSERT INTO probe (tag, v) VALUES ('{tag}', c); END"
            );
            let sql = format!("DO '{}'", body.replace('\'', "''"));
            let r = call(w, &sql);
            assert!(r.starts_with("OK"), "{sql}: {r}");
        }
        for (w, tag) in [(&x, "x"), (&y, "y")] {
            let r = match exec(
                w,
                w.obs,
                &format!("SELECT v FROM probe WHERE tag = '{tag}'"),
            ) {
                Ok(SqlOutcome::Query(q)) => q,
                other => panic!("{other:?}"),
            };
            assert_eq!(r.rows.len(), 1);
            assert_eq!(int(&r.rows[0].cells[0]), VISIBLE_IDS.len() as i64);
        }
    }
}

/// 検査器自身への負の対照。
#[test]
fn leak_checker_detects_fabricated_violation() {
    assert!(leak_reason("", "exec:42P01 relation zz_secret_t does not exist").is_some());
    assert!(leak_reason("", "exec:0A000 tenant-b function").is_some());
    assert!(leak_reason("SELECT zz_secret_f()", "exec:42883 zz_secret_f").is_none());
}
