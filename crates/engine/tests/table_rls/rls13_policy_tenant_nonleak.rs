//! 利用者定義ポリシー（RLS-13）が「テナント境界の内側で狭めるだけ」であり、他テナントの行の存在を
//! 応答から推知させないことの横断回帰（Issue #2281、ポインタ: `docs/spec/04-behavior/rls.md` RLS-13・
//! 補助 RLS-12・ERR-4。設計記録は `docs/design/row-security-policy-composition.md`
//! 「非漏えい試験（#2281）」節）。本ファイルは本番コードを変更せず、兄弟 Issue（#2273〜#2280）が
//! 経路ごとに入れた個別テストを書き直さない。守る不変条件は次の 2 つ。
//!
//! - **X≡Y**: 「他テナントが Private 行・自前のポリシー・RLS 状態を置いた DB（X）」と「置いていない
//!   DB（Y）」の双子 DB で、観測側テナントの応答（結果行・件数・スコア・順位・EXPLAIN・SQLSTATE・
//!   `client_message`・失敗した段）がバイト一致する。各プローブは 10 回以上反復し、反復間の安定性も固定する
//! - **狭めるだけ**: 観測側が見る行集合は、テスト側の独立オラクル（Rust の真実表。production の可視性判定は
//!   呼ばない）の「観測側の行 × ポリシー層」と一致し、他テナントの行は一度も現れず、一度も書き換わらない
//!
//! 組合せ軸は RLS 状態（無効・ENABLE・ENABLE+FORCE・ENABLE 後 DISABLE）× ポリシー集合 × `row_security`
//! （on・off）× 入口（autocommit・明示トランザクション・拡張クエリ）。COPY（`begin_copy`・
//! `commit_copy_in`）は別テストで同じ双子比較を行う。
//!
//! 到達不能な分岐の扱い: `BYPASSRLS`・所有者（`FORCE` 無しの所有者免除）の Skip 分岐は、現状 SQL から値を
//! 供給できない（`CREATE/ALTER ROLE ... BYPASSRLS` は構文段で `42501`、所有者は未結線）。その分岐の
//! 判定は `sql/policy_applicability.rs` の全組合せ単体テストが担い、本ファイルは SQL から到達できる
//! 拒否応答が X/Y で一致することだけを固定する（`bypassrls_grant_is_denied_identically`）。
//! テスト用のバイパスフックは `src/` に足さない（fail-open 経路になりうるため）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{CopyPlan, EngineCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TENANTS: [&str; 3] = ["tenant-a", "tenant-b", "tenant-c"];

/// 他テナントだけが知る値。観測側の応答（成功の結果も含む）に現れたら存在の漏えい。
const WATCH: &[&str] = &["zz_secret_note", "zz_secret_pol", "zz_secret_r"];

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ddl_session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

// ---------------------------------------------------------------------------------------------
// 観測側の行の真実（独立オラクル）
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Truth<'a> {
    id: u64,
    lang: &'static str,
    n: i32,
    note: &'a str,
}

/// 観測側の行。id 4 は `n = 0`（隠れた行にだけ置く 0 除算カナリア）。
const OBS_ROWS: [Truth<'static>; 4] = [
    Truth {
        id: 1,
        lang: "ja",
        n: 1,
        note: "alpha one",
    },
    Truth {
        id: 2,
        lang: "en",
        n: 2,
        note: "gamma",
    },
    Truth {
        id: 3,
        lang: "ja",
        n: 3,
        note: "alpha alpha",
    },
    Truth {
        id: 4,
        lang: "fr",
        n: 0,
        note: "delta",
    },
];

/// 他テナントの Private 行（X にだけ在る）。id 1 は観測側と同じ id。クエリ `[1,0]` に極めて近い位置・
/// クエリ語を多く含む本文に置き、漏れれば kNN・ハイブリッドの先頭に出る。
const FOREIGN_IDS: [u64; 3] = [1, 6, 7];

fn schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("n", ColumnType::Integer, false),
            ColumnDef::new("note", ColumnType::Text, true),
            ColumnDef::new("tag", ColumnType::Text, true),
        ],
    )
}

/// テスト側のポリシー定義（SQL と、同じ意味のクロージャ）。
struct Pol {
    name: &'static str,
    permissive: bool,
    cmd: &'static str,
    using: &'static str,
    f: fn(&Truth) -> bool,
}

impl Pol {
    fn create_sql(&self) -> String {
        format!(
            "CREATE POLICY {} ON docs AS {} FOR {} USING ({})",
            self.name,
            if self.permissive {
                "PERMISSIVE"
            } else {
                "RESTRICTIVE"
            },
            self.cmd,
            self.using
        )
    }
}

fn is_ja(t: &Truth) -> bool {
    t.lang == "ja"
}
fn not_fr(t: &Truth) -> bool {
    t.lang != "fr"
}

const P_JA: Pol = Pol {
    name: "pj",
    permissive: true,
    cmd: "ALL",
    using: "lang = 'ja'",
    f: is_ja,
};
const R_NOTFR: Pol = Pol {
    name: "rf",
    permissive: false,
    cmd: "ALL",
    using: "lang <> 'fr'",
    f: not_fr,
};
const P_SEL_JA: Pol = Pol {
    name: "sj",
    permissive: true,
    cmd: "SELECT",
    using: "lang = 'ja'",
    f: is_ja,
};

/// `(PERMISSIVE の OR) AND (RESTRICTIVE の AND)`。適用される PERMISSIVE が無ければ偽。
fn layer_ok(pols: &[&Pol], cmd: &str, t: &Truth) -> bool {
    let applicable = |p: &&&Pol| p.cmd == cmd || p.cmd == "ALL";
    let perm: Vec<&&Pol> = pols
        .iter()
        .filter(|p| p.permissive)
        .filter(applicable)
        .collect();
    if perm.is_empty() {
        return false;
    }
    perm.iter().any(|p| (p.f)(t))
        && pols
            .iter()
            .filter(|p| !p.permissive)
            .filter(applicable)
            .all(|p| (p.f)(t))
}

/// RLS 状態の軸。
#[derive(Clone, Copy, Debug)]
enum Rls {
    /// 有効化しない（テナント境界だけ）。
    Off,
    Enable,
    EnableForce,
    /// 有効化してから DISABLE（ポリシーは残るが適用しない）。
    EnableThenDisable,
}

impl Rls {
    fn applies(self) -> bool {
        matches!(self, Rls::Enable | Rls::EnableForce)
    }
}

struct Combo {
    rls: Rls,
    pols: Vec<&'static Pol>,
    row_security_on: bool,
}

fn combos() -> Vec<Combo> {
    let sets: Vec<Vec<&'static Pol>> =
        vec![vec![], vec![&P_JA], vec![&P_JA, &R_NOTFR], vec![&P_SEL_JA]];
    let mut v = Vec::new();
    for rls in [
        Rls::Off,
        Rls::Enable,
        Rls::EnableForce,
        Rls::EnableThenDisable,
    ] {
        for pols in &sets {
            for row_security_on in [true, false] {
                v.push(Combo {
                    rls,
                    pols: pols.clone(),
                    row_security_on,
                });
            }
        }
    }
    v
}

/// 観測側が SELECT で見る id（オラクル）。`None` は `row_security = off` で適用表のため全読み取りが拒否される。
fn expected_select(c: &Combo) -> Option<Vec<u64>> {
    if !c.rls.applies() {
        return Some(OBS_ROWS.iter().map(|t| t.id).collect());
    }
    if !c.row_security_on {
        return None;
    }
    Some(
        OBS_ROWS
            .iter()
            .filter(|t| layer_ok(&c.pols, "SELECT", t))
            .map(|t| t.id)
            .collect(),
    )
}

// ---------------------------------------------------------------------------------------------
// 双子 DB
// ---------------------------------------------------------------------------------------------

struct World {
    _g: CleanupGuard,
    core: EngineCore,
    obs: &'static str,
    frn: &'static str,
}

/// 1 行をテナント `tenant` の `vis` でシードする（`row.id` が物理キーの一部。`vec2` は埋め込み）。
fn seed(storage: &Storage, tenant: &str, vis: Visibility, vec2: [f32; 2], row: &Truth<'_>) {
    engine::tenant::insert_typed_row(
        storage,
        "docs",
        &ctx(tenant),
        row.id,
        vis,
        &[
            Value::Vector(vec2.to_vec()),
            Value::Text(row.lang.to_string()),
            Value::Integer(row.n),
            Value::Text(row.note.to_string()),
            Value::Null,
        ],
        &OperationId::parse(&format!("seed-{tenant}-{}", row.id)).expect("op id"),
    )
    .expect("seed row");
}

fn setup_ok(core: &EngineCore, tenant: &str, sql: &str) {
    let mut s = ddl_session();
    if let Err(e) = core.execute_sql_in_session(&ctx(tenant), &mut s, sql) {
        panic!("setup {tenant}: {sql}: {} {e}", e.wire_code());
    }
}

/// 観測側テナント `obs_idx`・他テナント `(obs_idx + 1) % 3` の双子 DB の片方を作る。観測側の表・行・RLS を
/// 先に作り（内部 ID を X/Y で揃える）、`foreign` のときだけ他テナントの Private 行・RLS・ポリシー・ロールを置く。
fn world(label: &str, obs_idx: usize, foreign: bool, c: &Combo) -> World {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let obs = TENANTS[obs_idx];
    let frn = TENANTS[(obs_idx + 1) % 3];
    for t in OBS_ROWS {
        let vis = if t.id % 2 == 0 {
            Visibility::Private
        } else {
            Visibility::Public
        };
        seed(&storage, obs, vis, [1.0, 0.1 * t.id as f32], &t);
    }
    if foreign {
        for id in FOREIGN_IDS {
            let note = format!("alpha alpha alpha alpha zz_secret_note{id}");
            let row = Truth {
                id,
                lang: "ja",
                n: 100 + id as i32,
                note: &note,
            };
            seed(
                &storage,
                frn,
                Visibility::Private,
                [1.0, 0.0001 * id as f32],
                &row,
            );
        }
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    // 観測側の RLS 構成（X/Y で同一）。
    if !matches!(c.rls, Rls::Off) {
        setup_ok(&core, obs, "ALTER TABLE docs ENABLE ROW LEVEL SECURITY");
    }
    if matches!(c.rls, Rls::EnableForce) {
        setup_ok(&core, obs, "ALTER TABLE docs FORCE ROW LEVEL SECURITY");
    }
    for p in &c.pols {
        setup_ok(&core, obs, &p.create_sql());
    }
    if matches!(c.rls, Rls::EnableThenDisable) {
        setup_ok(&core, obs, "ALTER TABLE docs DISABLE ROW LEVEL SECURITY");
    }
    if foreign {
        // 他テナント自身の RLS とポリシー（観測側へ効かない）。
        setup_ok(&core, frn, "ALTER TABLE docs ENABLE ROW LEVEL SECURITY");
        setup_ok(
            &core,
            frn,
            "CREATE POLICY zz_secret_pol ON docs AS PERMISSIVE FOR ALL USING (true)",
        );
    }
    World {
        _g: guard,
        core,
        obs,
        frn,
    }
}

// ---------------------------------------------------------------------------------------------
// 実行とトレース
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Entry {
    Auto,
    Txn,
    Ext,
}

fn fmt_err(stage: &str, e: &SqlSurfaceError) -> String {
    format!("{stage}:{} {}", e.wire_code(), e.client_message())
}

fn session_for(w: &World, c: &Combo) -> SessionState {
    let mut s = ddl_session();
    if !c.row_security_on {
        setup_session(&w.core, w.obs, &mut s, "SET row_security = off");
    }
    s
}

fn setup_session(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) {
    if let Err(e) = core.execute_sql_in_session(&ctx(tenant), s, sql) {
        panic!("setup {tenant}: {sql}: {} {e}", e.wire_code());
    }
}

/// 文を 1 つの入口で実行し、段ごとの応答を 1 本の比較用文字列にまとめる。`{op}` は一意な操作 ID に置換する。
fn trace(w: &World, c: &Combo, entry: Entry, sql: &str, opn: usize) -> String {
    let sql = sql.replace("{op}", &format!("op-{opn}"));
    let cx = ctx(w.obs);
    let mut s = session_for(w, c);
    match entry {
        Entry::Auto => match w.core.execute_sql_in_session(&cx, &mut s, &sql) {
            Ok(o) => format!("OK {o:?}"),
            Err(e) => fmt_err("exec", &e),
        },
        Entry::Txn => {
            let mut txn = w.core.new_session_transaction();
            let mut out = Vec::new();
            for step in [
                "BEGIN",
                sql.as_str(),
                "SELECT id FROM docs LIMIT 100",
                "ROLLBACK",
            ] {
                out.push(
                    match w.core.execute_sql_in_txn(&cx, &mut s, &mut txn, step) {
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
                .parse_sql_prepared_with_declared_in_session(&s, &sql, &[])
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
            match w.core.execute_parsed_in_session(&cx, &mut s, &b) {
                Ok(o) => t + &format!("OK {o:?}"),
                Err(e) => t + &fmt_err("execute", &e),
            }
        }
    }
}

/// 応答に、観測側が文に書いていない他テナントだけの値・テナント ID・内部キーが現れたら理由を返す。
fn leak_reason(sql: &str, message: &str) -> Option<String> {
    if message.contains('#') && !sql.contains('#') {
        return Some(format!("internal key marker in {message:?}"));
    }
    if message.contains("tenant-") {
        return Some(format!("tenant id in {message:?}"));
    }
    for w in WATCH {
        if message.contains(w) && !sql.contains(w) {
            return Some(format!(
                "watch value {w} not written by caller in {message:?}"
            ));
        }
    }
    None
}

fn assert_no_leak(sql: &str, t: &str) {
    if let Some(why) = leak_reason(sql, t) {
        panic!("{sql}: {why}");
    }
}

// ---------------------------------------------------------------------------------------------
// プローブ
// ---------------------------------------------------------------------------------------------

const KNN: &str = "SELECT id FROM docs ORDER BY embedding <=> '[1,0]' LIMIT 3";
const HYBRID: &str =
    "SELECT id FROM docs ORDER BY HYBRID(embedding, '[1,0]', note, 'alpha') LIMIT 3";

/// 読み取り系プローブ（SELECT 形）。
const READ_PROBES: &[&str] = &[
    "SELECT id FROM docs ORDER BY id LIMIT 100",
    "SELECT id, lang, n, note FROM docs ORDER BY id LIMIT 100",
    "SELECT COUNT(*) FROM docs",
    "SELECT lang, COUNT(*) FROM docs GROUP BY lang",
    "SELECT id FROM docs WHERE id = 6",
    "SELECT id FROM docs WHERE id = 1",
    "SELECT id FROM docs WHERE id IN (SELECT id FROM docs) ORDER BY id LIMIT 100",
    "SELECT id FROM docs WHERE 100 / n > 0 ORDER BY id LIMIT 100",
    "EXPLAIN SELECT id FROM docs ORDER BY id LIMIT 100",
    KNN,
    HYBRID,
];

/// 書き込み系プローブ。`{op}` は試行ごとの操作 ID。
const WRITE_PROBES: &[&str] = &[
    "UPDATE docs SET tag = 'u' WHERE n >= 0 USING OPERATION_ID '{op}'",
    "UPDATE docs SET tag = 'u' WHERE id = 6 USING OPERATION_ID '{op}'",
    "UPDATE docs SET tag = 'u' WHERE id = 1 RETURNING id USING OPERATION_ID '{op}'",
    "DELETE FROM docs WHERE id = 7 USING OPERATION_ID '{op}'",
    "DELETE FROM docs WHERE n >= 100 RETURNING id USING OPERATION_ID '{op}'",
    "INSERT INTO docs (id, embedding, lang, n, note) VALUES (6, '[1,0]', 'ja', 6, 'new') \
     USING OPERATION_ID '{op}'",
    "INSERT INTO docs (id, embedding, lang, n, note) VALUES (50, '[1,0]', 'fr', 5, 'viol') \
     USING OPERATION_ID '{op}'",
    "INSERT INTO docs (id, embedding, lang, n, note) VALUES (50, '[1,0]', 'ja', 5, 'ok') \
     USING OPERATION_ID '{op}'",
];

/// 観測側が見る行の集合を（Auto 入口で）取り出す。
fn query_ids(w: &World, c: &Combo, sql: &str) -> Result<Vec<u64>, String> {
    let cx = ctx(w.obs);
    let mut s = session_for(w, c);
    match w.core.execute_sql_in_session(&cx, &mut s, sql) {
        Ok(SqlOutcome::Query(q)) => {
            let mut v: Vec<u64> = q.rows.iter().map(|r| r.id).collect();
            v.sort_unstable();
            Ok(v)
        }
        Ok(other) => panic!("{sql}: expected Query, got {other:?}"),
        Err(e) => Err(e.wire_code().to_string()),
    }
}

/// 双子 DB の全組合せ・全入口で、読み取り系プローブの応答が X/Y でバイト一致し、結果集合がオラクルに従う。
#[test]
fn reads_are_identical_with_and_without_foreign_tenant_rows() {
    let mut trials = 0usize;
    let mut nonempty = 0usize;
    let mut ok_probes = 0usize;
    let mut denied_probes = 0usize;
    for (ci, c) in combos().iter().enumerate() {
        let obs_idx = ci % 3;
        let x = world("rls13-nl-rx", obs_idx, true, c);
        let y = world("rls13-nl-ry", obs_idx, false, c);
        // 結果集合（オラクル）。
        match (
            expected_select(c),
            query_ids(&x, c, "SELECT id FROM docs LIMIT 100"),
        ) {
            (Some(want), Ok(got)) => {
                assert_eq!(got, want, "combo {ci}: SELECT ids");
                if !want.is_empty() {
                    nonempty += 1;
                }
            }
            (None, Err(code)) => assert_eq!(code, "42501", "combo {ci}: off on applied table"),
            (want, got) => panic!("combo {ci}: oracle {want:?} vs {got:?}"),
        }
        for entry in [Entry::Auto, Entry::Txn, Entry::Ext] {
            for (pi, sql) in READ_PROBES.iter().enumerate() {
                let first = trace(&x, c, entry, sql, 0);
                // 空振り防止: 制約の無い組合せでは全プローブが成功し、適用表の off では 42501 になる。
                if matches!(c.rls, Rls::Off | Rls::EnableThenDisable) {
                    // 0 除算カナリア（id 4・n = 0）は可視な間だけ評価され、22012 になる。
                    if sql.contains("100 / n") {
                        assert!(first.contains("22012"), "combo {ci}: {first}");
                    } else {
                        assert!(first.contains("OK"), "combo {ci}: {sql} must run: {first}");
                    }
                    ok_probes += 1;
                } else if c.row_security_on && sql.contains("100 / n") {
                    // 適用表では id 4（lang = fr）が隠れるポリシー集合に限り、カナリアは評価されず成功する。
                    let hidden4 = !expected_select(c).is_some_and(|v| v.contains(&4));
                    assert_eq!(first.contains("OK"), hidden4, "combo {ci}: {first}");
                    ok_probes += 1;
                } else if !c.row_security_on && !sql.starts_with("EXPLAIN") {
                    // EXPLAIN は行を読まないため off でも計画だけを返す（X/Y 一致は上で固定済み）。
                    assert!(first.contains("42501"), "combo {ci}: {sql}: {first}");
                    denied_probes += 1;
                }
                for r in 0..10 {
                    let rx = trace(&x, c, entry, sql, r);
                    let ry = trace(&y, c, entry, sql, r);
                    assert_eq!(rx, ry, "combo {ci} probe {pi} {sql} ({entry:?})");
                    assert_eq!(rx, first, "combo {ci} {sql} unstable");
                    assert_no_leak(sql, &rx);
                    trials += 2;
                }
            }
        }
        // x.frn は観測側へ一度も出ない（出れば Y と不一致になる）。
        assert_ne!(x.frn, x.obs);
    }
    assert_eq!(trials, combos().len() * 3 * READ_PROBES.len() * 10 * 2);
    assert!(
        nonempty >= 10,
        "oracle must exercise non-empty visible sets"
    );
    assert!(ok_probes > 0 && denied_probes > 0);
}

/// 書き込み系プローブ（述語形・id 形・`RETURNING`・他テナントと同じ id の INSERT・`WITH CHECK` 違反）の
/// 応答が X/Y で一致し、他テナントの Private 行が全プローブ後も不変。
#[test]
fn writes_are_identical_and_never_touch_foreign_rows() {
    let mut trials = 0usize;
    let mut violations = 0usize;
    let mut same_id_inserts = 0usize;
    for (ci, c) in combos().iter().enumerate() {
        let obs_idx = ci % 3;
        let x = world("rls13-nl-wx", obs_idx, true, c);
        let y = world("rls13-nl-wy", obs_idx, false, c);
        for entry in [Entry::Auto, Entry::Txn, Entry::Ext] {
            for (pi, sql) in WRITE_PROBES.iter().enumerate() {
                for r in 0..3 {
                    let opn = ci * 10_000 + pi * 100 + r * 3 + entry as usize;
                    let rx = trace(&x, c, entry, sql, opn);
                    let ry = trace(&y, c, entry, sql, opn);
                    assert_eq!(rx, ry, "combo {ci} probe {pi} {sql} ({entry:?})");
                    assert_no_leak(sql, &rx);
                    trials += 2;
                    // 空振り防止: WITH CHECK 違反（適用表・USING のみの PERMISSIVE ja）は 42501、
                    // 他テナントと同じ id の INSERT は成功する。
                    if r == 0 && matches!(entry, Entry::Auto) {
                        let applies = c.rls.applies() && c.row_security_on;
                        if pi == 6 && applies && c.pols.len() == 1 && c.pols[0].name == "pj" {
                            assert!(rx.starts_with("exec:42501 "), "{rx}");
                            violations += 1;
                        }
                        if pi == 5 && matches!(c.rls, Rls::Off) {
                            assert!(rx.starts_with("OK"), "{rx}");
                            same_id_inserts += 1;
                        }
                    }
                }
            }
        }
        // 他テナントの行は全プローブ後も初期値のまま（所有テナントとして RLS 無効状態で読む）。
        let mut s = ddl_session();
        setup_ok(
            &x.core,
            x.frn,
            "ALTER TABLE docs DISABLE ROW LEVEL SECURITY",
        );
        let q = match x
            .core
            .execute_sql_in_session(
                &ctx(x.frn),
                &mut s,
                "SELECT id, n, note, tag FROM docs ORDER BY id LIMIT 100",
            )
            .expect("foreign read")
        {
            SqlOutcome::Query(q) => q,
            other => panic!("{other:?}"),
        };
        let dump = format!("{:?}", q.rows);
        for id in FOREIGN_IDS {
            assert!(
                dump.contains(&format!("zz_secret_note{id}")),
                "combo {ci}: foreign row {id} must survive: {dump}"
            );
        }
        // 他テナントの行（本文に WATCH 値を持つ行）の tag は NULL のまま、n も初期値のまま。
        let frn_rows: Vec<String> = q
            .rows
            .iter()
            .map(|r| format!("{:?}", r.cells))
            .filter(|d| d.contains("zz_secret_note"))
            .collect();
        assert_eq!(frn_rows.len(), FOREIGN_IDS.len(), "combo {ci}: {dump}");
        for d in &frn_rows {
            assert!(d.ends_with("Null]"), "combo {ci}: foreign tag changed: {d}");
        }
    }
    assert_eq!(trials, combos().len() * 3 * WRITE_PROBES.len() * 3 * 2);
    assert!(violations > 0 && same_id_inserts > 0);
}

/// COPY（`begin_copy` の TO 形・FROM 形）も同じ双子比較で一致し、他テナントの行を出さない。
#[test]
fn copy_is_identical_with_and_without_foreign_tenant_rows() {
    let mut trials = 0usize;
    for (ci, c) in combos().iter().enumerate() {
        let obs_idx = ci % 3;
        let x = world("rls13-nl-cx", obs_idx, true, c);
        let y = world("rls13-nl-cy", obs_idx, false, c);
        let tos = [
            "COPY docs (id, lang, n) TO STDOUT",
            "COPY (SELECT id, lang FROM docs LIMIT 100) TO STDOUT",
        ];
        for sql in tos {
            for _ in 0..10 {
                let rx = copy_to_trace(&x, c, sql);
                let ry = copy_to_trace(&y, c, sql);
                assert_eq!(rx, ry, "combo {ci}: {sql}");
                assert_no_leak(sql, &rx);
                trials += 2;
            }
        }
        // FROM: 他テナントと同じ id(1) を含む成功バッチと、違反しうる行を含むバッチ。
        for (k, data) in [
            b"1001\tja\t1\n1002\tja\t2\n".as_slice(),
            b"6\tja\t6\n7\tfr\t7\n".as_slice(),
        ]
        .iter()
        .enumerate()
        {
            let rx = copy_from_trace(&x, c, &format!("cpx-{ci}-{k}"), data);
            let ry = copy_from_trace(&y, c, &format!("cpx-{ci}-{k}"), data);
            assert_eq!(rx, ry, "combo {ci}: COPY FROM batch {k}");
            assert_no_leak("COPY FROM", &rx);
            trials += 2;
        }
    }
    assert!(trials > 0);
}

fn copy_to_trace(w: &World, c: &Combo, sql: &str) -> String {
    let s = session_for(w, c);
    match w.core.begin_copy(&ctx(w.obs), &s, sql) {
        Ok(CopyPlan::To(_, r, _)) => {
            let mut ids: Vec<u64> = r.rows.iter().map(|row| row.id).collect();
            ids.sort_unstable();
            format!("OK {:?} {:?}", ids, r.rows)
        }
        Ok(_) => panic!("expected TO plan"),
        Err(e) => fmt_err("begin", &e),
    }
}

fn copy_from_trace(w: &World, c: &Combo, op: &str, data: &[u8]) -> String {
    let s = session_for(w, c);
    let sql = format!("COPY docs (id, lang, n) FROM STDIN USING OPERATION_ID '{op}'");
    let plan = match w.core.begin_copy(&ctx(w.obs), &s, &sql) {
        Ok(p) => p,
        Err(e) => return fmt_err("begin", &e),
    };
    let CopyPlan::From(mut cs) = plan else {
        panic!("expected FROM plan");
    };
    if let Err(e) = cs.feed(data) {
        return format!("feed:{e:?}");
    }
    let batch = match cs.finish() {
        Ok(b) => b,
        Err(e) => return format!("finish:{e:?}"),
    };
    match w.core.commit_copy_in(&ctx(w.obs), batch) {
        Ok(o) => format!("OK {}", o.rows_affected),
        Err(e) => format!("commit:{} {}", e.wire_code(), e.client_message()),
    }
}

/// `BYPASSRLS` の付与は SQL 表層で拒否され、拒否応答が他テナントの同名ロールの有無に依存しない
/// （Skip 分岐そのものは `policy_applicability.rs` の単体テストが担う）。
#[test]
fn bypassrls_grant_is_denied_identically() {
    for obs_idx in 0..3 {
        let c = Combo {
            rls: Rls::Off,
            pols: vec![],
            row_security_on: true,
        };
        let x = world("rls13-nl-bx", obs_idx, true, &c);
        let y = world("rls13-nl-by", obs_idx, false, &c);
        setup_ok(&x.core, x.frn, "CREATE ROLE zz_secret_r");
        for sql in [
            "CREATE ROLE zz_secret_r BYPASSRLS",
            "CREATE ROLE other_r BYPASSRLS",
            "ALTER ROLE zz_secret_r BYPASSRLS",
        ] {
            let rx = trace(&x, &c, Entry::Auto, sql, 0);
            let ry = trace(&y, &c, Entry::Auto, sql, 0);
            assert_eq!(rx, ry, "{sql}");
            assert!(!rx.starts_with("OK"), "{sql} must be denied: {rx}");
            assert_no_leak(sql, &rx);
        }
    }
}

/// 検査器自身への負の対照: 捏造した漏えい文字列を検出できる。
#[test]
fn leak_checker_detects_fabricated_violation() {
    assert!(leak_reason("SELECT 1", "row zz_secret_note1 found").is_some());
    assert!(leak_reason("SELECT 1", "tenant-b owns it").is_some());
    assert!(leak_reason("SELECT 1", "key #12.docs").is_some());
    assert!(leak_reason("SELECT zz_secret_note1", "zz_secret_note1 missing").is_none());
    assert!(leak_reason("SELECT 1", "permission denied for table docs").is_none());
}
