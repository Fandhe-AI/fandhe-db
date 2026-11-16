//! 別名つき単一表 `UPDATE`／`DELETE`（FROM／USING なし）の SQL 経由の受け入れテスト
//! （SQL-53・RLS-10・RECOVER-10/11・Issue #3291。前提 #3202）。
//!
//! 契約: 入口のトークン正規化（`sql::dml_target_alias`）により、別名つき文は「別名なしの同じ文」と
//! 同じタグ・同じ行・同じ RETURNING 結果になる。別名を付けた表を本名で修飾すると `42P01`。
//! 未対応の形（SET 左辺の修飾・予約語の別名・余剰トークン・`EXPLAIN`・データ変更 CTE 本文・PL/pgSQL 本体）は
//! 副作用ゼロ（行・台帳が不変）で `42601` に拒否する（fail-closed）。RLS 相当のテナント境界は別名なし文と同一の経路。
//! ポインタ: `docs/design/update-from-delete-using.md`（別名つき単一表 DML 節）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core(label: &str, require_where: bool) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("k", ColumnType::Text, false),
                ColumnDef::new("x", ColumnType::Integer, true),
            ],
        ))
        .expect("create t");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_require_where_for_dml(require_where);
    (core, CleanupGuard(path))
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(core: &EngineCore, c: &PolicyContext, sql: &str) -> Result<String, SqlSurfaceError> {
    core.execute_sql_in_session(c, &mut SessionState::default(), sql)
        .map(|o| format!("{o:?}"))
}

fn code(r: Result<String, SqlSurfaceError>) -> String {
    r.expect_err("must be rejected").wire_code().to_string()
}

fn rows(core: &EngineCore, c: &PolicyContext) -> String {
    run(core, c, "SELECT id, k, x FROM t ORDER BY id LIMIT 100").expect("scan t")
}

/// alice: (1,a,10) (2,b,20) (3,c,30)。
fn seed(core: &EngineCore, c: &PolicyContext) {
    for (id, k, x) in [(1, "a", 10), (2, "b", 20), (3, "c", 30)] {
        run(
            core,
            c,
            &format!("INSERT INTO t (id, embedding, k, x) VALUES ({id}, '[0.1,0.2]', '{k}', {x})"),
        )
        .expect("seed");
    }
}

fn tag(kind: &str, n: u64) -> String {
    match kind {
        "UPD" => format!("Update(UpdateOutcome {{ rows_affected: {n} }})"),
        _ => format!("Delete(DeleteOutcome {{ rows_affected: {n} }})"),
    }
}

/// 別名つき文と別名なし文を、同じ初期状態の別 DB で実行し、結果と最終行が一致することを確かめる。
fn same_as_unaliased(label: &str, aliased: &str, plain: &str) -> String {
    let a = ctx("alice");
    let (c1, _g1) = new_core(&format!("{label}-a"), false);
    seed(&c1, &a);
    let (c2, _g2) = new_core(&format!("{label}-p"), false);
    seed(&c2, &a);
    let r1 = run(&c1, &a, aliased).unwrap_or_else(|e| panic!("{aliased}: {}", e.wire_code()));
    let r2 = run(&c2, &a, plain).unwrap_or_else(|e| panic!("{plain}: {}", e.wire_code()));
    assert_eq!(r1, r2, "{aliased}");
    assert_eq!(rows(&c1, &a), rows(&c2, &a), "{aliased}");
    r1
}

#[test]
fn update_with_alias_matches_unaliased() {
    let out = same_as_unaliased(
        "sql53-alias-upd",
        "UPDATE t AS tt SET x = tt.x + 1 WHERE tt.k = 'a'",
        "UPDATE t SET x = x + 1 WHERE k = 'a'",
    );
    assert_eq!(out, tag("UPD", 1));
    same_as_unaliased(
        "sql53-alias-upd-implicit",
        "UPDATE t tt SET x = 7 WHERE tt.k = 'b';",
        "UPDATE t SET x = 7 WHERE k = 'b';",
    );
    // `WHERE tt.id = <n>` の単一行形。
    same_as_unaliased(
        "sql53-alias-upd-id",
        "UPDATE t tt SET x = 99 WHERE tt.id = 2",
        "UPDATE t SET x = 99 WHERE id = 2",
    );
}

#[test]
fn update_returning_with_alias_matches_unaliased() {
    same_as_unaliased(
        "sql53-alias-upd-ret",
        "UPDATE t tt SET x = tt.x + 1 WHERE tt.k = 'a' RETURNING tt.k, tt.x",
        "UPDATE t SET x = x + 1 WHERE k = 'a' RETURNING k, x",
    );
    same_as_unaliased(
        "sql53-alias-upd-ret-star",
        "UPDATE t AS tt SET x = 5 WHERE tt.k = 'c' RETURNING tt.*",
        "UPDATE t SET x = 5 WHERE k = 'c' RETURNING *",
    );
}

#[test]
fn delete_with_alias_matches_unaliased() {
    let out = same_as_unaliased(
        "sql53-alias-del",
        "DELETE FROM t tt WHERE tt.k = 'a'",
        "DELETE FROM t WHERE k = 'a'",
    );
    assert_eq!(out, tag("DEL", 1));
    same_as_unaliased(
        "sql53-alias-del-as",
        "DELETE FROM t AS tt WHERE tt.k = 'b' RETURNING tt.k",
        "DELETE FROM t WHERE k = 'b' RETURNING k",
    );
    same_as_unaliased(
        "sql53-alias-del-id",
        "DELETE FROM t tt WHERE tt.id = 3;",
        "DELETE FROM t WHERE id = 3;",
    );
    same_as_unaliased(
        "sql53-alias-del-schema",
        "DELETE FROM public.t tt WHERE tt.k = 'a'",
        "DELETE FROM public.t WHERE k = 'a'",
    );
}

#[test]
fn partitioned_with_alias_matches_unaliased() {
    same_as_unaliased(
        "sql53-alias-part",
        "DELETE FROM t tt WHERE tt.k = 'a' USING OPERATION_ID 'p1' PARTITIONED CHUNK 1",
        "DELETE FROM t WHERE k = 'a' USING OPERATION_ID 'p1' PARTITIONED CHUNK 1",
    );
}

/// 別名を付けた表の本名・他表名による修飾は `42P01`。行は不変。存在しない表は別名なし文と同じコード。
#[test]
fn original_name_and_unknown_qualifiers_are_42p01_without_side_effects() {
    let a = ctx("alice");
    let (core, _g) = new_core("sql53-alias-42p01", false);
    seed(&core, &a);
    let before = rows(&core, &a);
    for sql in [
        "UPDATE t tt SET x = 1 WHERE t.k = 'a'",
        "UPDATE t tt SET x = t.x WHERE tt.k = 'a'",
        "UPDATE t tt SET x = 1 WHERE other.k = 'a'",
        "DELETE FROM t tt WHERE t.k = 'a'",
        "DELETE FROM t tt WHERE tt.k = 'a' RETURNING t.k",
        "DELETE FROM t tt WHERE tt.k = 'a' RETURNING t.*",
    ] {
        assert_eq!(code(run(&core, &a, sql)), "42P01", "{sql}");
    }
    assert_eq!(before, rows(&core, &a));
    // 存在しない表: 別名の有無でコードが変わらない（存在のオラクルにならない）。
    assert_eq!(
        code(run(&core, &a, "DELETE FROM nosuch tt WHERE tt.k = 'a'")),
        code(run(&core, &a, "DELETE FROM nosuch WHERE k = 'a'"))
    );
}

#[test]
fn unsupported_shapes_fail_closed_without_side_effects() {
    let a = ctx("alice");
    let (core, _g) = new_core("sql53-alias-reject", false);
    seed(&core, &a);
    let before = rows(&core, &a);
    for sql in [
        // SET 左辺の修飾・行値左辺の修飾
        "UPDATE t tt SET tt.x = 1 WHERE tt.k = 'a'",
        "UPDATE t tt SET (tt.x, tt.k) = (1, 'z') WHERE tt.k = 'a'",
        // 予約語・余剰トークン
        "UPDATE t AS set SET x = 1 WHERE k = 'a'",
        "DELETE FROM t tt y WHERE k = 'a'",
        "DELETE FROM t AS where",
        // 修飾の誤用
        "UPDATE t tt SET x = tt.null WHERE tt.k = 'a'",
        "UPDATE t tt SET x = tt.default WHERE tt.k = 'a'",
        "UPDATE t tt SET x = 1 WHERE tt.k = 'a' RETURNING tt.* + 1",
        "DELETE FROM t tt WHERE tt.*",
        // 対象外の経路（従来どおり）
        "EXPLAIN DELETE FROM t tt WHERE tt.k = 'a' USING OPERATION_ID 'p' PARTITIONED CHUNK 1",
        "WITH d AS (DELETE FROM t tt WHERE tt.k = 'a' RETURNING tt.k) SELECT k FROM d",
    ] {
        assert_eq!(code(run(&core, &a, sql)), "42601", "{sql}");
    }
    assert_eq!(before, rows(&core, &a));
}

/// 別名なしの本名修飾は従来どおり（Issue #3291 は別名なしの経路を変えない）。
#[test]
fn unaliased_qualified_form_is_unchanged() {
    let a = ctx("alice");
    let (core, _g) = new_core("sql53-alias-nonreg", false);
    seed(&core, &a);
    assert_eq!(
        code(run(&core, &a, "UPDATE t SET x = 1 WHERE t.k = 'a'")),
        "42601"
    );
}

/// RLS（RLS-10）: 別名つき文は別名なし文と同じテナント境界で、他テナントの行を変えず件数にも漏らさない。
#[test]
fn tenant_boundary_is_unchanged_by_alias() {
    let alice = ctx("alice");
    let bob = ctx("bob");
    let (core, _g) = new_core("sql53-alias-rls", false);
    seed(&core, &alice);
    for (id, k, x) in [(11, "a", 110), (12, "b", 120)] {
        run(
            &core,
            &bob,
            &format!("INSERT INTO t (id, embedding, k, x) VALUES ({id}, '[0.1,0.2]', '{k}', {x})"),
        )
        .expect("seed bob");
    }
    let bob_before = rows(&core, &bob);
    assert_eq!(
        run(&core, &alice, "UPDATE t tt SET x = 0 WHERE tt.k = 'a'").unwrap(),
        tag("UPD", 1)
    );
    assert_eq!(
        run(&core, &alice, "DELETE FROM t tt WHERE tt.k = 'b'").unwrap(),
        tag("DEL", 1)
    );
    assert_eq!(bob_before, rows(&core, &bob));
}

#[test]
fn explicit_transaction_rollback_and_commit() {
    let a = ctx("alice");
    let (core, _g) = new_core("sql53-alias-tx", false);
    seed(&core, &a);
    let before = rows(&core, &a);
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    let mut tx = |sql: &str| {
        core.execute_sql_in_txn(&a, &mut session, &mut txn, sql)
            .map(|o| format!("{o:?}"))
    };
    tx("BEGIN").expect("begin");
    assert_eq!(
        tx("UPDATE t tt SET x = 1 WHERE tt.k = 'a'").expect("update"),
        tag("UPD", 1)
    );
    // 失敗した文のあとの Tx は別名なし文と同じ 25P02。
    assert_eq!(
        tx("UPDATE t tt SET x = 1 WHERE t.k = 'a'")
            .unwrap_err()
            .wire_code(),
        "42P01"
    );
    assert_eq!(
        tx("UPDATE t tt SET x = 2 WHERE tt.k = 'a'")
            .unwrap_err()
            .wire_code(),
        "25P02"
    );
    tx("ROLLBACK").expect("rollback");
    assert_eq!(before, rows(&core, &a));

    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    let mut tx = |sql: &str| {
        core.execute_sql_in_txn(&a, &mut session, &mut txn, sql)
            .map(|o| format!("{o:?}"))
    };
    tx("BEGIN").expect("begin");
    tx("DELETE FROM t AS tt WHERE tt.k = 'c'").expect("delete");
    tx("COMMIT").expect("commit");
    assert!(!rows(&core, &a).contains("30"));
}

/// 台帳（RECOVER-10/11）: 別名つき文は 1 回だけ適用され、再送は `23505`。別名の有無は同じ操作として扱い、
/// 別の文への再利用は `22023`。
#[test]
fn operation_id_ledger_treats_alias_and_unaliased_as_same_operation() {
    let a = ctx("alice");
    let (core, _g) = new_core("sql53-alias-ledger", false);
    seed(&core, &a);
    assert_eq!(
        run(
            &core,
            &a,
            "UPDATE t tt SET x = 1 WHERE tt.k = 'a' USING OPERATION_ID 'op-1'"
        )
        .unwrap(),
        tag("UPD", 1)
    );
    let after = rows(&core, &a);
    assert_eq!(
        code(run(
            &core,
            &a,
            "UPDATE t tt SET x = 1 WHERE tt.k = 'a' USING OPERATION_ID 'op-1'"
        )),
        "23505"
    );
    assert_eq!(
        code(run(
            &core,
            &a,
            "UPDATE t SET x = 1 WHERE k = 'a' USING OPERATION_ID 'op-1'"
        )),
        "23505"
    );
    assert_eq!(
        code(run(
            &core,
            &a,
            "UPDATE t tt SET x = 2 WHERE tt.k = 'a' USING OPERATION_ID 'op-1'"
        )),
        "22023"
    );
    assert_eq!(after, rows(&core, &a));
    assert_eq!(
        run(
            &core,
            &a,
            "DELETE FROM t tt WHERE tt.k = 'b' USING OPERATION_ID 'op-2'"
        )
        .unwrap(),
        tag("DEL", 1)
    );
}

/// `--require-where-for-dml` 有効時: WHERE の無い別名つき文は拒否、WHERE があれば受理。
#[test]
fn require_where_applies_to_aliased_statements() {
    let a = ctx("alice");
    let (core, _g) = new_core("sql53-alias-reqwhere", true);
    seed(&core, &a);
    let before = rows(&core, &a);
    assert_eq!(code(run(&core, &a, "DELETE FROM t tt;")), "42601");
    assert_eq!(code(run(&core, &a, "UPDATE t tt SET x = 1")), "42601");
    assert_eq!(before, rows(&core, &a));
    assert_eq!(
        run(&core, &a, "DELETE FROM t tt WHERE tt.k = 'a'").unwrap(),
        tag("DEL", 1)
    );
}
