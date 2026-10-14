//! `UPDATE ... SET <col> = <式>` の `operation_id` 再送判定（RECOVER-10・SQL-45・
//! Issue #1545）の結合テスト。ポインタ: `docs/design/multi-row-dml-operation-id.md`・
//! `docs/design/update-single-row.md`・`docs/design/implementation-status.md`。
//!
//! 同じ文の再送は `23505`（適用済み・二重適用しない）、式が違う文は `22023`（内容不一致・
//! 副作用ゼロ）に振り分けられることを、単一行形・述語形・分割実行・`RETURNING`・
//! 0 行更新・テナント独立・再オープンの各経路で固定する。式違いを `23505` にすると
//! 再送が黙って成功扱いになるため（fail-closed の要）、網羅の本体は
//! `recovery::content_hash` の単体テストと本ファイルの SQL 経路マトリクスが担う。
//! `sql45_update_set_expr.rs` と同じ流儀（`unique_db_path`／`CleanupGuard`）。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn core_at(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    run(core, tenant, sql)
        .map(|o| panic!("{sql} must fail, got {o:?}"))
        .unwrap_err()
        .wire_code()
        .to_string()
}

/// 成功なら `"ok"`、失敗なら `wire_code`（応答が行の存在に依存しないことの比較用）。
fn outcome_code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    match run(core, tenant, sql) {
        Ok(_) => "ok".to_string(),
        Err(e) => e.wire_code().to_string(),
    }
}

fn int_of(core: &EngineCore, tenant: &str, col: &str, id: u64) -> i64 {
    match ok(
        core,
        tenant,
        &format!("SELECT {col} FROM docs WHERE id = {id} LIMIT 10"),
    ) {
        SqlOutcome::Query(r) => match &r.rows[0].cells[0] {
            Cell::SignedInteger(v) => *v,
            other => panic!("expected integer cell, got {other:?}"),
        },
        other => panic!("expected Query, got {other:?}"),
    }
}

/// alice に id 1〜3（n=10/20/30・m=1/2/3）、bob に id 4（n=100・m=4）。
fn setup(label: &str) -> (EngineCore, CleanupGuard, std::path::PathBuf) {
    let path = unique_db_path(label);
    let core = core_at(&path);
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx("sys"),
        &mut session,
        "CREATE TABLE docs (n INTEGER, m INTEGER, t TEXT, d DATE)",
    )
    .expect("create table");
    for (id, tenant, n, m) in [
        (1, "alice", 10, 1),
        (2, "alice", 20, 2),
        (3, "alice", 30, 3),
        (4, "bob", 100, 4),
    ] {
        ok(
            &core,
            tenant,
            &format!(
                "INSERT INTO docs (id, n, m, t) VALUES ({id}, {n}, {m}, 'a') \
                 USING OPERATION_ID 'seed-{id}'"
            ),
        );
    }
    (core, CleanupGuard(path.clone()), path)
}

/// 経路ごとの文を組み立てる。`set` は SET 句、`op` は operation_id。
fn stmt(kind: &str, set: &str, op: &str) -> String {
    match kind {
        "single" => format!("UPDATE docs SET {set} WHERE id = 1 USING OPERATION_ID '{op}'"),
        "pred" => format!("UPDATE docs SET {set} WHERE m > 0 USING OPERATION_ID '{op}'"),
        "returning" => {
            format!("UPDATE docs SET {set} WHERE m > 0 RETURNING n USING OPERATION_ID '{op}'")
        }
        "part" => format!(
            "UPDATE docs SET {set} WHERE m > 0 USING OPERATION_ID '{op}' PARTITIONED CHUNK 1"
        ),
        other => panic!("unknown kind {other}"),
    }
}

const KINDS: [&str; 4] = ["single", "pred", "returning", "part"];

/// 式違いの SET 句（基準は `n = n + 1`）。いずれも同じ operation_id では `22023`。
const DIFFERENT_FROM_BASE: &[&str] = &[
    "n = m + 1",
    "n = n - 1",
    "n = n * 1",
    "n = n + 2",
    "n = 1 + n",
    "n = abs(n)",
    "n = n + 1, t = 'x'",
    "n = 5",
    "n = n + 1 + 0",
];

#[test]
fn same_statement_resend_is_idempotent_on_every_path() {
    for kind in KINDS {
        let (core, _g, _p) = setup(&format!("sql45-resend-{kind}"));
        let sql = stmt(kind, "n = n + 1", "op-1");
        ok(&core, "alice", &sql);
        let after = int_of(&core, "alice", "n", 1);
        assert_eq!(after, 11, "{kind}: first application");
        assert_eq!(code(&core, "alice", &sql), "23505", "{kind}: resend");
        assert_eq!(
            int_of(&core, "alice", "n", 1),
            after,
            "{kind}: resend must not re-apply"
        );
        assert_eq!(int_of(&core, "bob", "n", 4), 100, "{kind}: other tenant");
    }
}

#[test]
fn different_expression_is_a_content_mismatch_with_zero_side_effects() {
    for kind in KINDS {
        let (core, _g, _p) = setup(&format!("sql45-mismatch-{kind}"));
        ok(&core, "alice", &stmt(kind, "n = n + 1", "op-1"));
        let after = int_of(&core, "alice", "n", 1);
        for set in DIFFERENT_FROM_BASE {
            assert_eq!(
                code(&core, "alice", &stmt(kind, set, "op-1")),
                "22023",
                "{kind}: `{set}` must mismatch"
            );
            assert_eq!(
                int_of(&core, "alice", "n", 1),
                after,
                "{kind}: `{set}` must have no side effects"
            );
        }
    }
}

#[test]
fn integer_and_decimal_literals_are_distinct_content() {
    for kind in KINDS {
        let (core, _g, _p) = setup(&format!("sql45-intdec-{kind}"));
        ok(&core, "alice", &stmt(kind, "n = n / 2", "op-1"));
        assert_eq!(
            code(&core, "alice", &stmt(kind, "n = n / 2", "op-1")),
            "23505"
        );
        assert_eq!(
            code(&core, "alice", &stmt(kind, "n = n / 2.0", "op-1")),
            "22023",
            "{kind}: `/ 2` and `/ 2.0` must not collapse"
        );
    }
}

#[test]
fn case_coalesce_and_nullif_structure_is_part_of_the_content() {
    let (core, _g, _p) = setup("sql45-structure");
    let cases: [(&str, &[&str]); 3] = [
        (
            "n = CASE WHEN n > 5 THEN 1 WHEN n > 2 THEN 2 ELSE 0 END",
            &[
                "n = CASE WHEN n > 2 THEN 2 WHEN n > 5 THEN 1 ELSE 0 END",
                "n = CASE WHEN n > 5 THEN 1 WHEN n > 2 THEN 2 END",
                "n = CASE WHEN n > 5 THEN 3 WHEN n > 2 THEN 2 ELSE 0 END",
            ],
        ),
        ("n = COALESCE(n, m)", &["n = COALESCE(m, n)"]),
        ("n = NULLIF(n, m)", &["n = NULLIF(m, n)"]),
    ];
    for (i, (base, variants)) in cases.iter().enumerate() {
        let op = format!("struct-{i}");
        ok(&core, "alice", &stmt("pred", base, &op));
        assert_eq!(code(&core, "alice", &stmt("pred", base, &op)), "23505");
        for v in *variants {
            assert_eq!(
                code(&core, "alice", &stmt("pred", v, &op)),
                "22023",
                "`{v}` must mismatch `{base}`"
            );
        }
    }
}

#[test]
fn syntactic_normalizations_and_assignment_order_are_pinned() {
    let (core, _g, _p) = setup("sql45-pinned");
    // 単一行形はスキーマ列順へ正規化されるため、割当の宣言順が違っても同一内容。
    ok(
        &core,
        "alice",
        &stmt("single", "n = n + 1, t = 'x'", "ord-1"),
    );
    assert_eq!(
        code(
            &core,
            "alice",
            &stmt("single", "t = 'x', n = n + 1", "ord-1")
        ),
        "23505"
    );
    // 述語形は宣言順のまま hash される（現行挙動の固定。順序違いは fail-closed 側で拒否）。
    ok(&core, "alice", &stmt("pred", "n = n + 1, t = 'x'", "ord-2"));
    assert_eq!(
        code(&core, "alice", &stmt("pred", "t = 'x', n = n + 1", "ord-2")),
        "22023"
    );
    // 単項プラスは構文段で落ちるため `+1` と `1` は同一内容。
    ok(&core, "alice", &stmt("pred", "n = n + 1", "norm-1"));
    assert_eq!(
        code(&core, "alice", &stmt("pred", "n = n + +1", "norm-1")),
        "23505"
    );
    // 関数名の大文字小文字は同一内容。
    ok(&core, "alice", &stmt("pred", "n = abs(n)", "norm-2"));
    assert_eq!(
        code(&core, "alice", &stmt("pred", "n = ABS(n)", "norm-2")),
        "23505"
    );
    // 識別子は大文字小文字を区別し、`N` は未知の列として束縛段で拒否される（42703）。
    // 台帳照合に到達しないため、hash 側の大文字小文字の区別は単体テストで固定する。
    ok(&core, "alice", &stmt("pred", "n = n + 3", "norm-3"));
    assert_eq!(
        code(&core, "alice", &stmt("pred", "n = N + 3", "norm-3")),
        "42703"
    );
}

#[test]
fn atomic_and_partitioned_forms_are_separate_content_domains() {
    let (core, _g, _p) = setup("sql45-domains");
    ok(&core, "alice", &stmt("pred", "n = n + 1", "dom-1"));
    assert_eq!(
        code(&core, "alice", &stmt("part", "n = n + 1", "dom-1")),
        "22023"
    );
    ok(&core, "alice", &stmt("part", "n = n + 1", "dom-2"));
    assert_eq!(
        code(&core, "alice", &stmt("pred", "n = n + 1", "dom-2")),
        "22023"
    );
}

#[test]
fn zero_row_expression_update_is_recorded_without_revealing_row_existence() {
    let (core, _g, _p) = setup("sql45-zero");
    // 述語形の 0 行更新（該当行なし）でも台帳に記録され、再送は 23505・式違いは 22023。
    let none = "UPDATE docs SET n = n + 1 WHERE m > 1000 USING OPERATION_ID 'zero-1'";
    ok(&core, "alice", none);
    assert_eq!(code(&core, "alice", none), "23505");
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE docs SET n = n + 2 WHERE m > 1000 USING OPERATION_ID 'zero-1'"
        ),
        "22023"
    );
    // 単一行形: 存在しない id と他テナント（bob）の id は応答が区別できない。
    let missing = "UPDATE docs SET n = n + 1 WHERE id = 99 USING OPERATION_ID 'zero-2'";
    let foreign = "UPDATE docs SET n = n + 1 WHERE id = 4 USING OPERATION_ID 'zero-3'";
    let first_missing = outcome_code(&core, "alice", missing);
    let first_foreign = outcome_code(&core, "alice", foreign);
    assert_eq!(first_missing, first_foreign);
    assert_eq!(
        outcome_code(&core, "alice", missing),
        outcome_code(&core, "alice", foreign)
    );
    assert_eq!(
        outcome_code(
            &core,
            "alice",
            "UPDATE docs SET n = n + 2 WHERE id = 99 USING OPERATION_ID 'zero-2'"
        ),
        outcome_code(
            &core,
            "alice",
            "UPDATE docs SET n = n + 2 WHERE id = 4 USING OPERATION_ID 'zero-3'"
        )
    );
    // 他テナントの行は変わらない。
    assert_eq!(int_of(&core, "bob", "n", 4), 100);
}

#[test]
fn ledger_is_independent_per_tenant() {
    let (core, _g, _p) = setup("sql45-tenant");
    let a = "UPDATE docs SET n = n + 1 WHERE m > 0 USING OPERATION_ID 'shared-op'";
    ok(&core, "alice", a);
    // 同じ operation_id・同じ文でも bob は alice の台帳と干渉せず初回として適用される。
    ok(&core, "bob", a);
    assert_eq!(int_of(&core, "bob", "n", 4), 101);
    assert_eq!(int_of(&core, "alice", "n", 1), 11);
    assert_eq!(code(&core, "bob", a), "23505");
    // bob 側の台帳でも式違いは内容不一致。
    assert_eq!(
        code(
            &core,
            "bob",
            "UPDATE docs SET n = n + 2 WHERE m > 0 USING OPERATION_ID 'shared-op'"
        ),
        "22023"
    );
}

#[test]
fn resend_after_reopen_keeps_idempotency_and_mismatch_detection() {
    for kind in KINDS {
        let (core, _g, path) = setup(&format!("sql45-reopen-{kind}"));
        let sql = stmt(kind, "n = n + 1", "op-1");
        ok(&core, "alice", &sql);
        drop(core);
        let core = core_at(&path);
        assert_eq!(
            code(&core, "alice", &sql),
            "23505",
            "{kind}: resend after reopen"
        );
        assert_eq!(int_of(&core, "alice", "n", 1), 11, "{kind}: not re-applied");
        assert_eq!(
            code(&core, "alice", &stmt(kind, "n = n + 2", "op-1")),
            "22023",
            "{kind}: mismatch after reopen"
        );
    }
}
