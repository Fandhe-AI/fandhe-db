//! WHERE の行値比較（簡易クエリ・拡張クエリ。対象ビヘイビア: SQL-9・RLS-10。親 Issue #3254・本 Issue #3257）の
//! テナント越境試験。`rls10_relational_paths.rs` の T2／T3 の流儀に従う。
//!
//! - T2: 他テナントの行（同じ `id`・同じ値を故意に持たせる）の有無で、結果（行・件数・集計）が変わらない。
//! - T3: エラー応答（`wire_code`・メッセージ）が他テナントの行の有無で変わらず、他テナントの情報を含めない。
//!
//! 比較は「自テナントの行だけの DB（baseline）」と「他テナントの行を足した DB（flooded）」で行い、
//! `PolicyContext::new`（Public のみ）と `with_visibilities`（Private 込み）の両モードで回す。
//! ポリシー（`ENABLE`／`FORCE ROW LEVEL SECURITY`）・利用者 WHERE 内の `OR`・`visible()`・述語形 DML も組み合わせる。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::legacy_table;
use crate::temp_db;
use legacy_table::LegacyExec as _;
use temp_db::{unique_db_path, CleanupGuard};

const TENANTS: [&str; 3] = ["tenant-a", "tenant-b", "tenant-c"];
const VIEWER: &str = "tenant-a";

fn ctx_for(tenant: &str, allow_private: bool) -> PolicyContext {
    if allow_private {
        PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
            .expect("valid tenant")
    } else {
        PolicyContext::new(tenant).expect("valid tenant")
    }
}

fn some(s: &str) -> Option<Vec<u8>> {
    Some(s.as_bytes().to_vec())
}

fn exec(
    core: &EngineCore,
    tenant: &str,
    allow_private: bool,
    sql: &str,
) -> Result<SqlOutcome, (&'static str, String)> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session_legacy(&ctx_for(tenant, allow_private), &mut s, sql)
        .map_err(|e| (e.wire_code(), e.to_string()))
}

fn must(core: &EngineCore, tenant: &str, sql: &str) {
    if let Err((c, m)) = exec(core, tenant, true, sql) {
        panic!("{sql}: {c} {m}");
    }
}

/// Prepared 経路（Parse → Describe → Bind → Execute）。
fn exec_prepared(
    core: &EngineCore,
    tenant: &str,
    allow_private: bool,
    sql: &str,
    values: &[Option<Vec<u8>>],
) -> Result<SqlOutcome, (&'static str, String)> {
    let err = |e: engine::sql::allowlist::SqlSurfaceError| (e.wire_code(), e.to_string());
    let prepared = core.parse_sql_prepared(sql).map_err(err)?;
    let session = SessionState::default();
    core.describe_prepared_in_session(&session, &prepared)
        .map_err(err)?;
    let bound = core.bind_prepared(&prepared, values).map_err(err)?;
    let mut s = SessionState::default();
    core.execute_parsed_in_session(&ctx_for(tenant, allow_private), &mut s, &bound)
        .map_err(err)
}

fn ids_of(o: &SqlOutcome) -> Vec<u64> {
    let SqlOutcome::Query(r) = o else {
        return Vec::new();
    };
    let mut v: Vec<u64> = r.rows.iter().map(|r| r.id).collect();
    v.sort_unstable();
    v
}

/// 自テナント（VIEWER）の行は `id` 1〜3、他テナントは同じ値で `id` 1〜3（衝突）と 11〜13 を持つ。
/// `policy` が真なら `ENABLE`／`FORCE ROW LEVEL SECURITY` と `USING (s = 'x')` を適用する。
fn build(label: &str, flood: bool, policy: bool) -> (CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let core = EngineCore::from_storage(
        Storage::open(&path).expect("open storage"),
        Box::new(CpuScalarProvider),
    );
    must(
        &core,
        VIEWER,
        "CREATE TABLE t (embedding VECTOR(2) NOT NULL, a INTEGER, b INTEGER, s TEXT, d DATE)",
    );
    let rows: [(u64, i32, i32, &str, &str); 3] = [
        (1, 1, 1, "x", "2024-01-01"),
        (2, 3, 5, "x", "2024-01-02"),
        (3, 3, 9, "y", "2024-01-03"),
    ];
    for tenant in TENANTS {
        if tenant != VIEWER && !flood {
            continue;
        }
        let offsets: &[u64] = if tenant == VIEWER { &[0] } else { &[0, 10] };
        for off in offsets {
            for (id, a, b, s, d) in rows {
                must(
                    &core,
                    tenant,
                    &format!(
                        "INSERT INTO t (id, embedding, a, b, s, d) VALUES ({}, '[0.1,0.2]', {a}, {b}, '{s}', '{d}') \
                         USING OPERATION_ID 'op-{tenant}-{}'",
                        id + off,
                        id + off
                    ),
                );
            }
        }
    }
    if policy {
        must(&core, VIEWER, "ALTER TABLE t ENABLE ROW LEVEL SECURITY");
        must(&core, VIEWER, "ALTER TABLE t FORCE ROW LEVEL SECURITY");
        must(&core, VIEWER, "CREATE POLICY p ON t USING (s = 'x')");
    }
    (guard, core)
}

/// `(ラベル, リテラル形 SQL, Prepared 形 SQL, 値)`。
type Case = (&'static str, &'static str, &'static str, [&'static str; 2]);

const READ_CASES: [Case; 7] = [
    (
        "range",
        "SELECT id FROM t WHERE (a, b) < (3, 9) LIMIT 10",
        "SELECT id FROM t WHERE (a, b) < ($1, $2) LIMIT 10",
        ["3", "9"],
    ),
    (
        "ge-and-scalar",
        "SELECT id FROM t WHERE (a, b) >= (3, 5) AND s = 'x' LIMIT 10",
        "SELECT id FROM t WHERE (a, b) >= ($1, $2) AND s = 'x' LIMIT 10",
        ["3", "5"],
    ),
    (
        "user-or",
        "SELECT id FROM t WHERE (a, b) < (3, 5) OR a IS NOT NULL LIMIT 10",
        "SELECT id FROM t WHERE (a, b) < ($1, $2) OR a IS NOT NULL LIMIT 10",
        ["3", "5"],
    ),
    (
        "count",
        "SELECT COUNT(*) AS n FROM t WHERE (a, b) > (0, 0)",
        "SELECT COUNT(*) AS n FROM t WHERE (a, b) > ($1, $2)",
        ["0", "0"],
    ),
    (
        "keyset-date",
        "SELECT id FROM t WHERE (d, id) > ('2024-01-01', 1) ORDER BY id LIMIT 10",
        "SELECT id FROM t WHERE (d, id) > ($1, $2) ORDER BY id LIMIT 10",
        ["2024-01-01", "1"],
    ),
    (
        "visible-and",
        "SELECT id FROM t WHERE visible() AND (a, b) = (3, 5) LIMIT 10",
        "SELECT id FROM t WHERE visible() AND (a, b) = ($1, $2) LIMIT 10",
        ["3", "5"],
    ),
    (
        "ne",
        "SELECT id FROM t WHERE (a, b) <> (3, 5) LIMIT 10",
        "SELECT id FROM t WHERE (a, b) <> ($1, $2) LIMIT 10",
        ["3", "5"],
    ),
];

/// T2: 他テナントの行（衝突する `id`・同じ値を含む）の有無で、リテラル形・Prepared 形とも結果が変わらず、
/// 自テナントの行だけが返る。ポリシーの有無・Public のみ／Private 込みの全組み合わせで確認する。
#[test]
fn row_compare_results_do_not_depend_on_other_tenants_rows() {
    for policy in [false, true] {
        let (_g1, base) = build("rls10-rc-base", false, policy);
        let (_g2, flooded) = build("rls10-rc-flood", true, policy);
        let mut nonempty = 0usize;
        for allow_private in [false, true] {
            for (label, lit, prep, vals) in READ_CASES {
                let want = exec(&base, VIEWER, allow_private, lit);
                let lit_flooded = exec(&flooded, VIEWER, allow_private, lit);
                let prep_flooded = exec_prepared(
                    &flooded,
                    VIEWER,
                    allow_private,
                    prep,
                    &[some(vals[0]), some(vals[1])],
                );
                let prep_base = exec_prepared(
                    &base,
                    VIEWER,
                    allow_private,
                    prep,
                    &[some(vals[0]), some(vals[1])],
                );
                let tag = format!("{label} policy={policy} private={allow_private}");
                assert_eq!(want, lit_flooded, "T2 literal: {tag}");
                assert_eq!(want, prep_base, "prepared parity (baseline): {tag}");
                assert_eq!(want, prep_flooded, "T2 prepared: {tag}");
                if let Ok(o) = &lit_flooded {
                    if label == "count" {
                        continue;
                    }
                    let ids = ids_of(o);
                    assert!(ids.iter().all(|i| (1..=3).contains(i)), "{tag}: {ids:?}");
                    if policy {
                        // ポリシー外の行（id3。s = 'y'）は、行値比較が真でも出ない。
                        assert!(!ids.contains(&3), "{tag}: {ids:?}");
                    }
                    nonempty += usize::from(!ids.is_empty());
                }
            }
        }
        assert!(nonempty > 0);
    }
}

/// 他テナント視点でも同様に、自分の行だけが見える（VIEWER の行は出ない）。
#[test]
fn row_compare_does_not_leak_viewer_rows_to_other_tenants() {
    let (_g, core) = build("rls10-rc-other", true, false);
    // tenant-b は id 1〜3・11〜13 を持つ。tenant-a の行（id 1〜3）と衝突するため件数で判定する。
    let b = exec(
        &core,
        "tenant-b",
        true,
        "SELECT id FROM t WHERE (a, b) >= (3, 5) LIMIT 20",
    )
    .expect("tenant-b select");
    assert_eq!(ids_of(&b), vec![2, 3, 12, 13]);
    let a = exec(
        &core,
        VIEWER,
        true,
        "SELECT id FROM t WHERE (a, b) >= (3, 5) LIMIT 20",
    )
    .expect("tenant-a select");
    assert_eq!(ids_of(&a), vec![2, 3]);
}

/// T3: エラー応答は他テナントの行の有無で変わらず、他テナント名・値を含まない。
#[test]
fn row_compare_errors_do_not_depend_on_other_tenants_rows() {
    let (_g1, base) = build("rls10-rc-err-base", false, false);
    let (_g2, flooded) = build("rls10-rc-err-flood", true, false);
    let literal_cases = [
        "SELECT id FROM t WHERE (a, b) < (NULL, 5) LIMIT 10",
        "SELECT id FROM t WHERE (zz, b) < (1, 5) LIMIT 10",
        "SELECT id FROM t WHERE (a, b) < (1, 'tenant-b') LIMIT 10",
        "SELECT id FROM t WHERE (d, a) < ('not-a-date', 1) LIMIT 10",
        "SELECT id FROM t WHERE (a, b) < (1, 2, 3) LIMIT 10",
    ];
    for allow_private in [false, true] {
        for sql in literal_cases {
            let a = exec(&base, VIEWER, allow_private, sql);
            let b = exec(&flooded, VIEWER, allow_private, sql);
            assert_eq!(a, b, "T3: {sql}");
            if let Err((_, msg)) = &b {
                assert!(
                    !msg.contains("tenant-b") || sql.contains("tenant-b"),
                    "{sql}: {msg}"
                );
            }
        }
        for (sql, vals) in [
            (
                "SELECT id FROM t WHERE (d, a) < ($1, $2) LIMIT 10",
                [some("not-a-date-SECRET"), some("1")],
            ),
            (
                "SELECT id FROM t WHERE (a, b) < ($1, $2) LIMIT 10",
                [None, some("5")],
            ),
            (
                "SELECT id FROM t WHERE (a, b) < ($1, $2) LIMIT 10",
                [some("1x"), some("5")],
            ),
        ] {
            let a = exec_prepared(&base, VIEWER, allow_private, sql, &vals);
            let b = exec_prepared(&flooded, VIEWER, allow_private, sql, &vals);
            assert_eq!(a, b, "T3 prepared: {sql}");
            let Err((_, msg)) = b else {
                panic!("{sql} must fail")
            };
            assert!(!msg.contains("SECRET"), "{msg}");
        }
    }
}

/// `visible()` を行値の成分に置いた形は、可視性の絞り込みを落として全行を返すことがない
/// （評価されるか fail-closed で拒否される。Issue #3257 の `rls_predicate_present` 伝播の回帰）。
#[test]
fn visible_inside_row_value_never_drops_visibility() {
    let (_g1, base) = build("rls10-rc-vis-base", false, false);
    let (_g2, flooded) = build("rls10-rc-vis-flood", true, false);
    for allow_private in [false, true] {
        for sql in [
            "SELECT id FROM t WHERE (visible(), a) = (true, 3) LIMIT 10",
            "SELECT id FROM t WHERE (a, visible()) = (3, true) LIMIT 10",
        ] {
            let a = exec(&base, VIEWER, allow_private, sql);
            let b = exec(&flooded, VIEWER, allow_private, sql);
            match (&a, &b) {
                (Ok(x), Ok(y)) => {
                    assert_eq!(x, y, "{sql}");
                    assert!(ids_of(y).iter().all(|i| (1..=3).contains(i)), "{sql}");
                }
                (Err(x), Err(y)) => {
                    assert_eq!(x, y, "{sql}");
                    assert!(matches!(x.0, "0A000" | "42601" | "42883"), "{sql}: {x:?}");
                }
                _ => panic!("{sql}: baseline {a:?} vs flooded {b:?}"),
            }
        }
    }
}

/// 述語形 UPDATE／DELETE の副作用は自テナントの（ポリシー可視の）該当行に限られ、他テナントの行は変化しない。
#[test]
fn predicate_dml_with_row_compare_stays_inside_the_tenant_and_policy() {
    for policy in [false, true] {
        let (_g, core) = build("rls10-rc-dml", true, policy);
        // リテラル形の UPDATE と Prepared 形の UPDATE の両方。
        must(
            &core,
            VIEWER,
            "UPDATE t SET s = 'x' WHERE id = 3 USING OPERATION_ID 'prep-reset'",
        );
        let upd = core
            .parse_sql_prepared(
                "UPDATE t SET d = '2030-01-01' WHERE (a, b) >= ($1, $2) USING OPERATION_ID $3",
            )
            .expect("parse update");
        let bound = core
            .bind_prepared(&upd, &[some("3"), some("5"), some("op-rc-upd")])
            .expect("bind");
        let mut s = SessionState::default();
        core.execute_parsed_in_session(&ctx_for(VIEWER, true), &mut s, &bound)
            .expect("execute update");
        let changed = exec(
            &core,
            VIEWER,
            true,
            "SELECT id FROM t WHERE d = '2030-01-01' LIMIT 20",
        )
        .expect("select changed");
        // `id = 3` はポリシー有りでは先の `s = 'x'` への UPDATE が可視外のため `y` のまま（policy 時は拒否側）。
        let ids = ids_of(&changed);
        assert!(ids.contains(&2), "policy={policy}: {ids:?}");
        // 他テナントの行は 1 行も変わっていない。
        for tenant in ["tenant-b", "tenant-c"] {
            let theirs = exec(
                &core,
                tenant,
                true,
                "SELECT id FROM t WHERE d = '2030-01-01' LIMIT 20",
            )
            .expect("select other");
            assert!(ids_of(&theirs).is_empty(), "{tenant} policy={policy}");
        }
        // DELETE（Prepared）も他テナントの同値行を消さない。
        let del = core
            .parse_sql_prepared("DELETE FROM t WHERE (a, b) = ($1, $2) USING OPERATION_ID $3")
            .expect("parse delete");
        let bound = core
            .bind_prepared(&del, &[some("1"), some("1"), some("op-rc-del")])
            .expect("bind");
        core.execute_parsed_in_session(&ctx_for(VIEWER, true), &mut s, &bound)
            .expect("execute delete");
        for tenant in ["tenant-b", "tenant-c"] {
            let theirs = exec(&core, tenant, true, "SELECT id FROM t WHERE a = 1 LIMIT 20")
                .expect("select other");
            assert_eq!(ids_of(&theirs), vec![1, 11], "{tenant} policy={policy}");
        }
    }
}

/// EXPLAIN（ANALYZE を含む）の出力は他テナントの行に影響されない。ANALYZE の実測件数は可視行だけを反映する。
#[test]
fn explain_output_does_not_depend_on_other_tenants_rows() {
    let (_g1, base) = build("rls10-rc-explain-base", false, false);
    let (_g2, flooded) = build("rls10-rc-explain-flood", true, false);
    let sql = "SELECT id FROM t WHERE (a, b) >= (3, 5) LIMIT 10";
    for allow_private in [false, true] {
        let plan = |core: &EngineCore, prefix: &str| {
            let o = exec(core, VIEWER, allow_private, &format!("{prefix}{sql}")).expect("explain");
            let SqlOutcome::Explain(r) = o else {
                panic!("not an explain outcome")
            };
            r.rows
                .iter()
                .map(|row| format!("{:?}", row.cells))
                .filter(|l| !l.contains("time_ms"))
                .collect::<Vec<_>>()
        };
        assert_eq!(plan(&base, "EXPLAIN "), plan(&flooded, "EXPLAIN "));
        assert_eq!(
            plan(&base, "EXPLAIN ANALYZE "),
            plan(&flooded, "EXPLAIN ANALYZE ")
        );
    }
}
