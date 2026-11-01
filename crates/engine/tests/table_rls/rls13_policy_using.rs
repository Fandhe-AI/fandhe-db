//! 利用者ポリシーの `USING` を SELECT・UPDATE・DELETE の対象行選択へ適用する（RLS-13・Issue #2276）の
//! SQL 水準の結合テスト。ポインタ: `docs/spec/04-behavior/rls.md` の RLS-13、設計記録は
//! `docs/design/row-security-policy-composition.md`。
//!
//! 固定する契約:
//! 1. 合成規則（PERMISSIVE は OR・RESTRICTIVE は AND・適用される PERMISSIVE が無ければ全行拒否・コマンド別・
//!    `FOR ALL`・UPDATE／DELETE は既存行を読むので SELECT のポリシーも AND）。期待値はテスト側の独立した
//!    オラクル（Rust のクロージャ）で出し、production の判定関数を使わない
//! 2. 除かれた行は存在しない行と同じ（0 件・`RETURNING` に出ない・エラーにならない。利用者の式より先に評価）
//! 3. テナント境界は常に先で、ポリシーは内側で狭めるだけ（他テナントの有効化・定義は効かない）
//! 4. ポリシーを反映できない経路（ランキング付き検索）は適用表に限り `0A000` で拒否する（fail-closed）

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

/// シード行の真実（オラクル用。production の可視性・ポリシー判定は一切通さない）。
#[derive(Clone, Copy)]
struct Truth {
    id: u64,
    tenant: &'static str,
    vis: Visibility,
    lang: &'static str,
    n: i32,
    note: Option<&'static str>,
}

/// id 4（alice の Private・`n = 0`）は 0 除算カナリア。bob の Private（id 6）は alice に常に不可視。
const ROWS: [Truth; 7] = [
    Truth {
        id: 1,
        tenant: "alice",
        vis: Visibility::Public,
        lang: "ja",
        n: 1,
        note: Some("x"),
    },
    Truth {
        id: 2,
        tenant: "alice",
        vis: Visibility::Private,
        lang: "en",
        n: 2,
        note: None,
    },
    Truth {
        id: 3,
        tenant: "alice",
        vis: Visibility::Public,
        lang: "ja",
        n: 3,
        note: Some("y"),
    },
    Truth {
        id: 4,
        tenant: "alice",
        vis: Visibility::Private,
        lang: "fr",
        n: 0,
        note: None,
    },
    Truth {
        id: 5,
        tenant: "bob",
        vis: Visibility::Public,
        lang: "ja",
        n: 5,
        note: Some("x"),
    },
    Truth {
        id: 6,
        tenant: "bob",
        vis: Visibility::Private,
        lang: "ja",
        n: 6,
        note: Some("x"),
    },
    Truth {
        id: 7,
        tenant: "bob",
        vis: Visibility::Public,
        lang: "fr",
        n: 7,
        note: None,
    },
];

/// テナント境界だけで alice に見える行（独立オラクル）。
fn alice_visible(t: &Truth) -> bool {
    t.tenant == "alice" || matches!(t.vis, Visibility::Public)
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

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

fn open_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    for t in ROWS {
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx(t.tenant),
            t.id,
            t.vis,
            &[
                Value::Vector(vec![t.id as f32, 0.0]),
                Value::Text(t.lang.to_string()),
                Value::Integer(t.n),
                t.note.map_or(Value::Null, |s| Value::Text(s.to_string())),
                Value::Null,
            ],
            &OperationId::parse(&format!("seed-{}", t.id)).expect("op id"),
        )
        .expect("seed row");
    }
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn ddl_session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn run_as(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = ddl_session();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
}

fn ok_as(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    run_as(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {e}"))
}

fn code_as(core: &EngineCore, tenant: &str, sql: &str) -> &'static str {
    match run_as(core, tenant, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

fn query_as(core: &EngineCore, tenant: &str, sql: &str) -> QueryResult {
    match ok_as(core, tenant, sql) {
        SqlOutcome::Query(q) => q,
        other => panic!("expected Query for {sql}, got {other:?}"),
    }
}

fn ids_as(core: &EngineCore, tenant: &str, sql: &str) -> Vec<u64> {
    let mut v: Vec<u64> = query_as(core, tenant, sql)
        .rows
        .iter()
        .map(|r| r.id)
        .collect();
    v.sort_unstable();
    v
}

fn affected(outcome: SqlOutcome) -> u64 {
    match outcome {
        SqlOutcome::Update(o) => o.rows_affected,
        SqlOutcome::Delete(o) => o.rows_affected,
        SqlOutcome::Returning(r) => r.rows_affected,
        other => panic!("expected Update/Delete/Returning, got {other:?}"),
    }
}

fn int_cell(cell: &Cell) -> i64 {
    match cell {
        Cell::Integer(v) => i64::try_from(*v).expect("fits"),
        Cell::SignedInteger(v) => *v,
        other => panic!("expected integer cell, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// 独立オラクル
// ---------------------------------------------------------------------

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

fn ja(t: &Truth) -> bool {
    t.lang == "ja"
}
fn n_ge3(t: &Truth) -> bool {
    t.n >= 3
}
fn not_fr(t: &Truth) -> bool {
    t.lang != "fr"
}
fn note_x(t: &Truth) -> bool {
    t.note == Some("x")
}

const P_JA: Pol = Pol {
    name: "pj",
    permissive: true,
    cmd: "ALL",
    using: "lang = 'ja'",
    f: ja,
};
const P_N3: Pol = Pol {
    name: "pn",
    permissive: true,
    cmd: "ALL",
    using: "n >= 3",
    f: n_ge3,
};
const R_NOTFR: Pol = Pol {
    name: "rf",
    permissive: false,
    cmd: "ALL",
    using: "lang <> 'fr'",
    f: not_fr,
};
const R_SEL_N3: Pol = Pol {
    name: "rn",
    permissive: false,
    cmd: "SELECT",
    using: "n >= 3",
    f: n_ge3,
};
const P_SEL_JA: Pol = Pol {
    name: "sj",
    permissive: true,
    cmd: "SELECT",
    using: "lang = 'ja'",
    f: ja,
};
const P_UPD_N3: Pol = Pol {
    name: "un",
    permissive: true,
    cmd: "UPDATE",
    using: "n >= 3",
    f: n_ge3,
};
const P_DEL_NOTFR: Pol = Pol {
    name: "df",
    permissive: true,
    cmd: "DELETE",
    using: "lang <> 'fr'",
    f: not_fr,
};
const P_NOTE_X: Pol = Pol {
    name: "px",
    permissive: true,
    cmd: "ALL",
    using: "note = 'x'",
    f: note_x,
};

/// 1 コマンド分の層（`(PERMISSIVE の OR) AND (RESTRICTIVE の AND)`。適用される PERMISSIVE が無ければ偽）。
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

fn expected_select(pols: &[&Pol]) -> Vec<u64> {
    ROWS.iter()
        .filter(|t| alice_visible(t) && layer_ok(pols, "SELECT", t))
        .map(|t| t.id)
        .collect()
}

/// UPDATE・DELETE は既存行を読むため、対象のコマンドの層に SELECT の層も AND する。
fn expected_write(pols: &[&Pol], cmd: &str) -> Vec<u64> {
    ROWS.iter()
        .filter(|t| t.tenant == "alice" && layer_ok(pols, cmd, t) && layer_ok(pols, "SELECT", t))
        .map(|t| t.id)
        .collect()
}

fn cases() -> Vec<Vec<&'static Pol>> {
    vec![
        vec![],
        vec![&P_JA],
        vec![&P_JA, &P_N3],
        vec![&P_JA, &R_NOTFR],
        vec![&R_NOTFR],
        vec![&P_SEL_JA],
        vec![&P_SEL_JA, &P_UPD_N3, &P_DEL_NOTFR],
        vec![&P_JA, &R_SEL_N3],
        vec![&P_N3, &P_NOTE_X],
        vec![&P_JA, &P_NOTE_X, &R_NOTFR, &R_SEL_N3],
    ]
}

/// 表 `docs` に RLS を有効化し、`pols` を alice として作る。
fn setup_policies(core: &EngineCore, pols: &[&Pol]) {
    ok_as(core, "alice", "ALTER TABLE docs ENABLE ROW LEVEL SECURITY");
    for p in pols {
        ok_as(core, "alice", &p.create_sql());
    }
}

/// 利用者ポリシーを外してテナント境界だけで読み、対象の更新・削除結果を確認する。
fn disable_rls(core: &EngineCore) {
    ok_as(core, "alice", "ALTER TABLE docs DISABLE ROW LEVEL SECURITY");
}

fn sorted(mut v: Vec<u64>) -> Vec<u64> {
    v.sort_unstable();
    v
}

// ---------------------------------------------------------------------
// 1. 合成規則（SELECT）
// ---------------------------------------------------------------------

/// 組合せ行列ごとに、SELECT の見える行がオラクルと一致する。件数・集計・GROUP BY・窓関数・部分クエリも同じ集合に従う。
#[test]
fn select_matches_oracle_for_every_policy_combination() {
    for (i, pols) in cases().iter().enumerate() {
        let (core, _g) = open_core(&format!("rls13-using-sel-{i}"));
        // 有効化前はテナント境界だけ（導入前とビット同一）。
        assert_eq!(
            ids_as(&core, "alice", "SELECT id FROM docs LIMIT 100"),
            sorted(
                ROWS.iter()
                    .filter(|t| alice_visible(t))
                    .map(|t| t.id)
                    .collect()
            ),
            "case {i}: before ENABLE"
        );
        setup_policies(&core, pols);
        let expected = expected_select(pols);
        assert_eq!(
            ids_as(&core, "alice", "SELECT id FROM docs LIMIT 100"),
            expected,
            "case {i}: SELECT *"
        );
        assert_eq!(
            ids_as(
                &core,
                "alice",
                "SELECT id FROM docs WHERE n >= 0 ORDER BY id LIMIT 100"
            ),
            expected,
            "case {i}: WHERE + ORDER BY"
        );
        let count = query_as(&core, "alice", "SELECT COUNT(*) FROM docs");
        assert_eq!(
            int_cell(&count.rows[0].cells[0]),
            expected.len() as i64,
            "case {i}: COUNT(*)"
        );
        // 連続して 2 回呼んでも（キャッシュ・高速経路の相乗りがあっても）同じ。
        let count2 = query_as(&core, "alice", "SELECT COUNT(*) FROM docs");
        assert_eq!(
            int_cell(&count2.rows[0].cells[0]),
            expected.len() as i64,
            "case {i}: COUNT(*) repeat"
        );
        let grouped = query_as(
            &core,
            "alice",
            "SELECT lang, COUNT(*) FROM docs GROUP BY lang",
        );
        let total: i64 = grouped.rows.iter().map(|r| int_cell(&r.cells[1])).sum();
        assert_eq!(total, expected.len() as i64, "case {i}: GROUP BY total");
        let mut langs: Vec<String> = grouped
            .rows
            .iter()
            .map(|r| match &r.cells[0] {
                Cell::Text(s) => s.clone(),
                other => panic!("lang cell: {other:?}"),
            })
            .collect();
        langs.sort();
        let mut expected_langs: Vec<String> = ROWS
            .iter()
            .filter(|t| expected.contains(&t.id))
            .map(|t| t.lang.to_string())
            .collect();
        expected_langs.sort();
        expected_langs.dedup();
        assert_eq!(langs, expected_langs, "case {i}: GROUP BY keys");
        let windowed = query_as(
            &core,
            "alice",
            "SELECT id, ROW_NUMBER() OVER (ORDER BY id) FROM docs LIMIT 100",
        );
        assert_eq!(windowed.rows.len(), expected.len(), "case {i}: window");
        let sub = ids_as(
            &core,
            "alice",
            "SELECT id FROM docs WHERE id IN (SELECT id FROM docs) LIMIT 100",
        );
        assert_eq!(sub, expected, "case {i}: IN subquery");
        let joined = query_as(
            &core,
            "alice",
            "SELECT docs.id FROM docs JOIN docs d2 ON docs.id = d2.id LIMIT 100",
        );
        assert_eq!(joined.rows.len(), expected.len(), "case {i}: self join");
        // PK 形ではない id 等価（点取得の経路）も同じ。
        for t in ROWS.iter().filter(|t| alice_visible(t)) {
            let one = ids_as(
                &core,
                "alice",
                &format!("SELECT id FROM docs WHERE id = {}", t.id),
            );
            assert_eq!(
                one.len(),
                usize::from(expected.contains(&t.id)),
                "case {i}: id = {}",
                t.id
            );
        }
    }
}

/// DISABLE でテナント境界だけに戻る（ポリシーが残っていても適用しない）。境界を越えて広がらない。
#[test]
fn disable_restores_tenant_boundary_only() {
    let (core, _g) = open_core("rls13-using-disable");
    setup_policies(&core, &[&P_JA]);
    assert_eq!(
        ids_as(&core, "alice", "SELECT id FROM docs LIMIT 100"),
        vec![1, 3, 5]
    );
    disable_rls(&core);
    assert_eq!(
        ids_as(&core, "alice", "SELECT id FROM docs LIMIT 100"),
        vec![1, 2, 3, 4, 5, 7]
    );
    // bob の Private（id 6）は alice に一度も見えない。
    assert!(!ids_as(&core, "alice", "SELECT id FROM docs LIMIT 100").contains(&6));
}

// ---------------------------------------------------------------------
// 1. 合成規則（UPDATE・DELETE）
// ---------------------------------------------------------------------

/// 述語形 UPDATE の影響行とその集合がオラクルと一致する。
#[test]
fn predicate_update_matches_oracle() {
    for (i, pols) in cases().iter().enumerate() {
        let (core, _g) = open_core(&format!("rls13-using-upd-{i}"));
        setup_policies(&core, pols);
        let expected = expected_write(pols, "UPDATE");
        let out = ok_as(
            &core,
            "alice",
            "UPDATE docs SET tag = 'u1' WHERE n >= 0 USING OPERATION_ID 'op-u'",
        );
        assert_eq!(affected(out), expected.len() as u64, "case {i}: rows");
        disable_rls(&core);
        assert_eq!(
            ids_as(
                &core,
                "alice",
                "SELECT id FROM docs WHERE tag = 'u1' LIMIT 100"
            ),
            expected,
            "case {i}: updated set"
        );
    }
}

/// 述語形 DELETE の影響行と残りの集合がオラクルと一致する。
#[test]
fn predicate_delete_matches_oracle() {
    for (i, pols) in cases().iter().enumerate() {
        let (core, _g) = open_core(&format!("rls13-using-del-{i}"));
        setup_policies(&core, pols);
        let expected = expected_write(pols, "DELETE");
        let out = ok_as(
            &core,
            "alice",
            "DELETE FROM docs WHERE n >= 0 USING OPERATION_ID 'op-d'",
        );
        assert_eq!(affected(out), expected.len() as u64, "case {i}: rows");
        disable_rls(&core);
        let remaining = ids_as(&core, "alice", "SELECT id FROM docs LIMIT 100");
        let want: Vec<u64> = ROWS
            .iter()
            .filter(|t| alice_visible(t) && !expected.contains(&t.id))
            .map(|t| t.id)
            .collect();
        assert_eq!(remaining, sorted(want), "case {i}: remaining");
    }
}

/// id 形（`WHERE id = n`）の UPDATE・DELETE も同じ選択になる。
#[test]
fn id_form_update_and_delete_match_oracle() {
    for (i, pols) in cases().iter().enumerate() {
        let (core, _g) = open_core(&format!("rls13-using-idu-{i}"));
        setup_policies(&core, pols);
        let expected = expected_write(pols, "UPDATE");
        for t in ROWS.iter().filter(|t| alice_visible(t)) {
            let out = ok_as(
                &core,
                "alice",
                &format!(
                    "UPDATE docs SET tag = 'u2' WHERE id = {} USING OPERATION_ID 'op-iu-{}'",
                    t.id, t.id
                ),
            );
            assert_eq!(
                affected(out),
                u64::from(expected.contains(&t.id)),
                "case {i}: update id {}",
                t.id
            );
        }
        disable_rls(&core);
        assert_eq!(
            ids_as(
                &core,
                "alice",
                "SELECT id FROM docs WHERE tag = 'u2' LIMIT 100"
            ),
            expected,
            "case {i}: id-form updated set"
        );

        let (core, _g2) = open_core(&format!("rls13-using-idd-{i}"));
        setup_policies(&core, pols);
        let expected = expected_write(pols, "DELETE");
        for t in ROWS.iter().filter(|t| alice_visible(t)) {
            let out = ok_as(
                &core,
                "alice",
                &format!(
                    "DELETE FROM docs WHERE id = {} USING OPERATION_ID 'op-id-{}'",
                    t.id, t.id
                ),
            );
            assert_eq!(
                affected(out),
                u64::from(expected.contains(&t.id)),
                "case {i}: delete id {}",
                t.id
            );
        }
        disable_rls(&core);
        let remaining = ids_as(&core, "alice", "SELECT id FROM docs LIMIT 100");
        let want: Vec<u64> = ROWS
            .iter()
            .filter(|t| alice_visible(t) && !expected.contains(&t.id))
            .map(|t| t.id)
            .collect();
        assert_eq!(remaining, sorted(want), "case {i}: id-form remaining");
    }
}

/// 分割実行 DML（`PARTITIONED`）も同じ対象選択になる。
#[test]
fn partitioned_dml_applies_the_policy() {
    for (i, pols) in cases().iter().enumerate() {
        let (core, _g) = open_core(&format!("rls13-using-part-{i}"));
        setup_policies(&core, pols);
        let expected = expected_write(pols, "UPDATE");
        run_as(
            &core,
            "alice",
            "UPDATE docs SET tag = 'p1' WHERE n >= 0 USING OPERATION_ID 'op-p' PARTITIONED CHUNK 2",
        )
        .unwrap_or_else(|e| panic!("case {i}: partitioned update: {e}"));
        disable_rls(&core);
        assert_eq!(
            ids_as(
                &core,
                "alice",
                "SELECT id FROM docs WHERE tag = 'p1' LIMIT 100"
            ),
            expected,
            "case {i}: partitioned updated set"
        );
    }
}

/// `RETURNING` には通過した行だけが現れる。
#[test]
fn returning_contains_only_rows_that_pass_the_policy() {
    let (core, _g) = open_core("rls13-using-returning");
    setup_policies(&core, &[&P_JA]);
    // 通過する行（id 1）。
    let out = ok_as(
        &core,
        "alice",
        "UPDATE docs SET tag = 'r' WHERE id = 1 RETURNING id USING OPERATION_ID 'op-r1'",
    );
    match out {
        SqlOutcome::Returning(r) => {
            assert_eq!(r.rows_affected, 1);
            assert_eq!(r.result.rows.len(), 1);
        }
        other => panic!("expected Returning, got {other:?}"),
    }
    // 除かれた行（id 2: en）は 0 件・`RETURNING` も空・エラーなし。
    let out = ok_as(
        &core,
        "alice",
        "UPDATE docs SET tag = 'r' WHERE id = 2 RETURNING id USING OPERATION_ID 'op-r2'",
    );
    match out {
        SqlOutcome::Returning(r) => {
            assert_eq!(r.rows_affected, 0);
            assert!(r.result.rows.is_empty());
        }
        other => panic!("expected Returning, got {other:?}"),
    }
    let out = ok_as(
        &core,
        "alice",
        "DELETE FROM docs WHERE n >= 0 RETURNING id USING OPERATION_ID 'op-r3'",
    );
    match out {
        SqlOutcome::Returning(r) => {
            // 書き込みの対象は自テナントの行だけ（他テナントの Public 行は読めても書けない）。ja の id 1・3。
            assert_eq!(r.rows_affected, 2);
        }
        other => panic!("expected Returning, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// 2. 存在の非漏えい
// ---------------------------------------------------------------------

/// 隠れた行にだけ 0 除算を起こす値（id 4: `n = 0`）を置いても、利用者の式は隠れた行に対して評価されない。
/// ポリシーを外すと同じ式が 0 除算で失敗する（カナリアが実在する対照）。
#[test]
fn user_expression_is_not_evaluated_on_rows_hidden_by_the_policy() {
    let (core, _g) = open_core("rls13-using-canary");
    setup_policies(&core, &[&P_JA]);
    // SELECT: ja の行（n = 1・3・5）だけが評価される。
    assert_eq!(
        ids_as(
            &core,
            "alice",
            "SELECT id FROM docs WHERE 10 / n > 1 LIMIT 100"
        ),
        vec![1, 3, 5]
    );
    // UPDATE・DELETE（述語形）もエラーにならない。
    let out = ok_as(
        &core,
        "alice",
        "UPDATE docs SET tag = 'c' WHERE 10 / n > 1 USING OPERATION_ID 'op-c1'",
    );
    assert_eq!(affected(out), 2);
    let out = ok_as(
        &core,
        "alice",
        "DELETE FROM docs WHERE 10 / n > 1 USING OPERATION_ID 'op-c2'",
    );
    assert_eq!(affected(out), 2);

    // 対照: ポリシーを外すと、テナント境界の内側の全行（カナリア込み）に対して評価され 0 除算になる。
    let (core2, _g2) = open_core("rls13-using-canary-control");
    assert_eq!(
        code_as(
            &core2,
            "alice",
            "SELECT id FROM docs WHERE 10 / n > 1 LIMIT 100"
        ),
        "22012"
    );
}

/// 隠れた行だけを対象にした UPDATE・DELETE は 0 件で、エラーにならない（存在しない行と同じ応答）。
#[test]
fn writes_against_hidden_rows_are_indistinguishable_from_missing_rows() {
    let (core, _g) = open_core("rls13-using-hidden");
    setup_policies(&core, &[&P_JA]);
    for id in [2u64, 4] {
        let out = ok_as(
            &core,
            "alice",
            &format!("UPDATE docs SET tag = 'h' WHERE id = {id} USING OPERATION_ID 'op-hu-{id}'"),
        );
        assert_eq!(affected(out), 0, "hidden id {id}");
        let out = ok_as(
            &core,
            "alice",
            &format!("DELETE FROM docs WHERE id = {id} USING OPERATION_ID 'op-hd-{id}'"),
        );
        assert_eq!(affected(out), 0, "hidden id {id}");
    }
    // 実在しない id と同じ応答。
    let out = ok_as(
        &core,
        "alice",
        "DELETE FROM docs WHERE id = 999 USING OPERATION_ID 'op-hd-999'",
    );
    assert_eq!(affected(out), 0);
    disable_rls(&core);
    assert_eq!(
        ids_as(
            &core,
            "alice",
            "SELECT id FROM docs WHERE tag = 'h' LIMIT 100"
        ),
        Vec::<u64>::new()
    );
    assert_eq!(
        ids_as(&core, "alice", "SELECT id FROM docs LIMIT 100"),
        vec![1, 2, 3, 4, 5, 7]
    );
}

// ---------------------------------------------------------------------
// 3. テナント境界は常に先
// ---------------------------------------------------------------------

/// 他テナントの有効化・ポリシーは自テナントに効かない。自テナントのポリシーは、自テナントに見える
/// 他テナントの Public 行にも掛かる。
#[test]
fn policies_are_scoped_per_tenant_and_narrow_inside_the_tenant_boundary() {
    let (core, _g) = open_core("rls13-using-tenants");
    // bob だけが有効化・ポリシーを持つ。
    ok_as(&core, "bob", "ALTER TABLE docs ENABLE ROW LEVEL SECURITY");
    ok_as(
        &core,
        "bob",
        "CREATE POLICY bp ON docs FOR ALL USING (lang = 'en')",
    );
    // alice は影響を受けない（導入前と同じ）。
    assert_eq!(
        ids_as(&core, "alice", "SELECT id FROM docs LIMIT 100"),
        vec![1, 2, 3, 4, 5, 7]
    );
    // bob には bob の境界（自分の行＋alice の Public 行）の内側で、en の行は無い（alice の id 2 は Private）。
    assert_eq!(
        ids_as(&core, "bob", "SELECT id FROM docs LIMIT 100"),
        Vec::<u64>::new()
    );
    // alice の有効化後、alice のポリシーは bob の Public 行（id 5・7）にも掛かる。
    setup_policies(&core, &[&P_JA]);
    assert_eq!(
        ids_as(&core, "alice", "SELECT id FROM docs LIMIT 100"),
        vec![1, 3, 5]
    );
    // bob の Private（id 6）はポリシーが何であれ alice に見えない。
    ok_as(&core, "alice", "DROP POLICY pj ON docs");
    ok_as(
        &core,
        "alice",
        "CREATE POLICY open ON docs FOR ALL USING (true)",
    );
    assert!(!ids_as(&core, "alice", "SELECT id FROM docs LIMIT 100").contains(&6));
}

// ---------------------------------------------------------------------
// 規則の細目
// ---------------------------------------------------------------------

/// 有効化しただけでポリシーが 0 件なら全行拒否（既定拒否）。式を省略した PERMISSIVE は TRUE の分岐になる。
#[test]
fn default_deny_and_policy_without_using() {
    let (core, _g) = open_core("rls13-using-deny");
    ok_as(&core, "alice", "ALTER TABLE docs ENABLE ROW LEVEL SECURITY");
    assert!(ids_as(&core, "alice", "SELECT id FROM docs LIMIT 100").is_empty());
    let out = ok_as(
        &core,
        "alice",
        "UPDATE docs SET tag = 'd' WHERE n >= 0 USING OPERATION_ID 'op-deny-u'",
    );
    assert_eq!(affected(out), 0);
    let out = ok_as(
        &core,
        "alice",
        "DELETE FROM docs WHERE n >= 0 USING OPERATION_ID 'op-deny-d'",
    );
    assert_eq!(affected(out), 0);
    // 式を省略した PERMISSIVE は TRUE として合成される（書き込み側と同じ意味。既定拒否とは区別する）。
    ok_as(&core, "alice", "CREATE POLICY nu ON docs FOR ALL");
    assert!(!ids_as(&core, "alice", "SELECT id FROM docs LIMIT 100").is_empty());
}

/// 式が NULL になる行は偽として扱う（`note` が NULL の行は `note = 'x'` を通らない）。
#[test]
fn null_policy_result_is_treated_as_false() {
    let (core, _g) = open_core("rls13-using-null");
    setup_policies(&core, &[&P_NOTE_X]);
    // note = 'x' の行は id 1・5（alice 可視）。NULL（id 2・4・7）と 'y'（id 3）は通らない。
    assert_eq!(
        ids_as(&core, "alice", "SELECT id FROM docs LIMIT 100"),
        vec![1, 5]
    );
}

/// `ALTER POLICY` による式の差し替えは次の文から効く。
#[test]
fn altered_policy_takes_effect_on_the_next_statement() {
    let (core, _g) = open_core("rls13-using-alter");
    setup_policies(&core, &[&P_JA]);
    assert_eq!(
        ids_as(&core, "alice", "SELECT id FROM docs LIMIT 100"),
        vec![1, 3, 5]
    );
    ok_as(&core, "alice", "ALTER POLICY pj ON docs USING (n >= 5)");
    assert_eq!(
        ids_as(&core, "alice", "SELECT id FROM docs LIMIT 100"),
        vec![5, 7]
    );
}

/// 既存行を読まない文（`WHERE` 無し・`RETURNING` 無し・リテラルだけの `SET`）は UPDATE／DELETE の
/// ポリシーだけで対象を選ぶ。`WHERE` が列を参照する・`RETURNING` がある文は SELECT のポリシーも AND する
/// （PostgreSQL 17 と同じ）。
#[test]
fn select_policy_is_added_only_when_the_statement_reads_existing_rows() {
    const P_UPD_NOTFR: Pol = Pol {
        name: "uf",
        permissive: true,
        cmd: "UPDATE",
        using: "lang <> 'fr'",
        f: not_fr,
    };
    let pols: [&Pol; 3] = [&P_SEL_JA, &P_UPD_NOTFR, &P_DEL_NOTFR];
    // 自テナント（alice）の行: id 1 ja・2 en・3 ja・4 fr。UPDATE／DELETE のポリシーだけなら fr 以外の 1・2・3。
    // SELECT の `lang = 'ja'` も AND すると 1・3。
    let only_write_layer: Vec<u64> = ROWS
        .iter()
        .filter(|t| t.tenant == "alice" && layer_ok(&pols, "UPDATE", t))
        .map(|t| t.id)
        .collect();
    let with_select_layer = expected_write(&pols, "UPDATE");
    assert_eq!(only_write_layer, vec![1, 2, 3]);
    assert_eq!(with_select_layer, vec![1, 3]);

    // 既存行を読まない UPDATE（`WHERE` 無し）。
    let (core, _g) = open_core("rls13-using-reads-upd");
    setup_policies(&core, &pols);
    let out = ok_as(
        &core,
        "alice",
        "UPDATE docs SET tag = 'nw' USING OPERATION_ID 'op-nw'",
    );
    assert_eq!(affected(out), only_write_layer.len() as u64);
    // 列を参照する `WHERE` は SELECT の層も AND する。
    let out = ok_as(
        &core,
        "alice",
        "UPDATE docs SET tag = 'w' WHERE n >= 0 USING OPERATION_ID 'op-w'",
    );
    assert_eq!(affected(out), with_select_layer.len() as u64);

    // DELETE も同じ。`RETURNING` があれば既存行を読む扱い。
    let (core, _g) = open_core("rls13-using-reads-del");
    setup_policies(&core, &pols);
    let out = ok_as(
        &core,
        "alice",
        "DELETE FROM docs RETURNING id USING OPERATION_ID 'op-dr'",
    );
    assert_eq!(affected(out), with_select_layer.len() as u64);
    // 残りの行（SELECT の層で落ちる id 2・4 のうち、DELETE の層を通る id 2）が WHERE 無しなら消える。
    let out = ok_as(
        &core,
        "alice",
        "DELETE FROM docs USING OPERATION_ID 'op-dn'",
    );
    assert_eq!(affected(out), 1);
}

// ---------------------------------------------------------------------
// 4. ポリシーを反映できない経路は fail-closed
// ---------------------------------------------------------------------

/// ランキング付き検索は、適用表に限り固定文言の `0A000` で拒否する。適用していない表・DISABLE 後は従来どおり成功する。
#[test]
fn ranked_search_is_rejected_only_for_tables_where_policies_apply() {
    let (core, _g) = open_core("rls13-using-ranked");
    let sql = "SELECT id FROM docs ORDER BY embedding <=> '[1,0]' LIMIT 3";
    // 未有効化: 成功。
    assert!(!query_as(&core, "alice", sql).rows.is_empty());
    setup_policies(&core, &[&P_JA]);
    match run_as(&core, "alice", sql) {
        Err(e) => {
            assert_eq!(e.wire_code(), "0A000");
            let msg = e.client_message().to_string();
            assert!(!msg.contains("docs"), "表名を含めない: {msg}");
            assert!(!msg.contains("pj"), "ポリシー名を含めない: {msg}");
        }
        Ok(o) => panic!("ranked search must be rejected, got {o:?}"),
    }
    // 他テナントの有効化はこのテナントの検索を妨げない。
    assert!(!query_as(&core, "bob", sql).rows.is_empty());
    disable_rls(&core);
    assert!(!query_as(&core, "alice", sql).rows.is_empty());
}
