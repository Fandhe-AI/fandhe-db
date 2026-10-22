//! `UPDATE ... SET <col> = DEFAULT`（SQL-50、Issue #1746）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/sql.md` SQL-50、`data-model.md` TABLE-16・TABLE-39、
//! `recovery.md` RECOVER-10・RECOVER-11、`rls.md` RLS-9・RLS-10。設計記録は
//! `docs/design/not-null-default.md`「Issue #1746 追記」。INSERT 側は
//! `sql50_insert_default_values.rs`（Issue #1730）。
//!
//! 実 `Storage` ＋ `CpuScalarProvider` の production 経路（`EngineCore::execute_sql_in_session`）で、
//! (1) 列 DEFAULT あり／なし（NULL・`23502`）の代入結果、(2) id 指定形・述語形・分割実行・ビュー、
//! (3) RETURNING が等価リテラルの SET と同じ列・行を返すこと、(4) 同一 `OPERATION_ID` の再送が
//! 列 DEFAULT の変更を挟んでも `23505`（内容照合は送られた構文形）、(5) fail-closed な拒否
//! （時刻・SEQUENCE・IDENTITY の DEFAULT、引用付き識別子、UPSERT）、(6) テナント境界を固定する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/legacy_table.rs"]
mod legacy_table;
#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use legacy_table::LegacyExec as _;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session_legacy(&ctx(tenant), &mut session, sql)
}

fn ok_as(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn ok(core: &EngineCore, sql: &str) -> SqlOutcome {
    ok_as(core, "owner", sql)
}

fn code_as(core: &EngineCore, tenant: &str, sql: &str) -> String {
    match run(core, tenant, sql) {
        Ok(o) => panic!("statement must fail: {sql} -> {o:?}"),
        Err(e) => e.wire_code().to_string(),
    }
}

fn code(core: &EngineCore, sql: &str) -> String {
    code_as(core, "owner", sql)
}

fn affected(outcome: SqlOutcome) -> u64 {
    match outcome {
        SqlOutcome::Update(o) => o.rows_affected,
        other => panic!("expected Update outcome, got {other:?}"),
    }
}

fn rows_as(core: &EngineCore, tenant: &str, sql: &str) -> Vec<Vec<Cell>> {
    match ok_as(core, tenant, sql) {
        SqlOutcome::Query(r) => r.rows.into_iter().map(|row| row.cells).collect(),
        SqlOutcome::Returning(r) => r.result.rows.into_iter().map(|row| row.cells).collect(),
        other => panic!("expected Query or Returning outcome, got {other:?}"),
    }
}

fn rows(core: &EngineCore, sql: &str) -> Vec<Vec<Cell>> {
    rows_as(core, "owner", sql)
}

/// RETURNING の列名と行（等価リテラル SET との比較用）。
fn returning_of(out: &SqlOutcome) -> (Vec<String>, Vec<Vec<Cell>>) {
    match out {
        SqlOutcome::Returning(r) => (
            r.result.columns.iter().map(|c| format!("{c:?}")).collect(),
            r.result.rows.iter().map(|row| row.cells.clone()).collect(),
        ),
        other => panic!("expected RETURNING, got {other:?}"),
    }
}

fn text(s: &str) -> Cell {
    Cell::Text(s.to_string())
}

fn int(v: i64) -> Cell {
    Cell::SignedInteger(v)
}

/// 列 DEFAULT ありの `c`・`n`、DEFAULT なしの `a`・`o` を持つ表 `t`（id 1〜3 を owner が所有）。
fn setup(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    ok(
        &core,
        "CREATE TABLE t (a TEXT, c TEXT DEFAULT 'dflt', n INTEGER DEFAULT 7, o TEXT)",
    );
    for id in 1..=3 {
        ok(
            &core,
            &format!(
                "INSERT INTO t (id, a, c, n, o) VALUES ({id}, 'a{id}', 'c{id}', {id}, 'o{id}') \
                 USING OPERATION_ID 'seed-{id}'"
            ),
        );
    }
    (core, CleanupGuard(path))
}

fn t_rows(core: &EngineCore) -> Vec<Vec<Cell>> {
    rows(core, "SELECT a, c, n, o FROM t ORDER BY id LIMIT 100")
}

/// DEFAULT あり: id 指定形・述語形（WHERE あり／TRUE）で列 DEFAULT が入り、他の列は不変。
#[test]
fn default_present_sets_column_default_in_id_and_predicate_forms() {
    let (core, _g) = setup("sql50-upd-present");
    assert_eq!(
        affected(ok(
            &core,
            "UPDATE t SET c = DEFAULT WHERE id = 1 USING OPERATION_ID 'u1'"
        )),
        1
    );
    assert_eq!(
        t_rows(&core)[0],
        vec![text("a1"), text("dflt"), int(1), text("o1")]
    );
    // 述語形・大小無視・複数列混在（リテラル／式との混在）。
    assert_eq!(
        affected(ok(
            &core,
            "UPDATE t SET a = 'x', c = default, n = DEFAULT WHERE id > 1 USING OPERATION_ID 'u2'"
        )),
        2
    );
    assert_eq!(
        t_rows(&core)[1],
        vec![text("x"), text("dflt"), int(7), text("o2")]
    );
    ok(
        &core,
        "UPDATE t SET n = n + 1, c = DEFAULT WHERE id = 3 USING OPERATION_ID 'u3'",
    );
    assert_eq!(
        t_rows(&core)[2],
        vec![text("x"), text("dflt"), int(8), text("o3")]
    );
    // WHERE TRUE / WHERE 省略。
    ok(
        &core,
        "UPDATE t SET c = 'k' WHERE id > 0 USING OPERATION_ID 'u4'",
    );
    ok(
        &core,
        "UPDATE t SET c = DEFAULT WHERE TRUE USING OPERATION_ID 'u5'",
    );
    assert!(t_rows(&core).iter().all(|r| r[1] == text("dflt")));
}

/// DEFAULT なし: nullable 列は NULL、NOT NULL 列は `23502`（副作用なし）、`SET c = NULL` と同じ規則。
#[test]
fn default_absent_sets_null_or_violates_not_null() {
    let (core, _g) = setup("sql50-upd-absent");
    ok(
        &core,
        "UPDATE t SET o = DEFAULT, a = DEFAULT WHERE id = 2 USING OPERATION_ID 'n1'",
    );
    assert_eq!(
        t_rows(&core)[1],
        vec![Cell::Null, text("c2"), int(2), Cell::Null]
    );
    ok(
        &core,
        "CREATE TABLE nn (k TEXT NOT NULL, v TEXT NOT NULL DEFAULT 'd')",
    );
    ok(
        &core,
        "INSERT INTO nn (id, k, v) VALUES (1, 'k1', 'v1') USING OPERATION_ID 'nn-seed'",
    );
    // id 指定形・述語形とも `23502`（行は不変）。
    assert_eq!(
        code(
            &core,
            "UPDATE nn SET k = DEFAULT WHERE id = 1 USING OPERATION_ID 'nn1'"
        ),
        "23502"
    );
    assert_eq!(
        code(
            &core,
            "UPDATE nn SET k = DEFAULT WHERE id > 0 USING OPERATION_ID 'nn2'"
        ),
        "23502"
    );
    // 対象行が存在しない id でも同じ拒否（行の有無で応答が分かれない。RLS-9・RLS-10）。
    assert_eq!(
        code(
            &core,
            "UPDATE nn SET k = DEFAULT WHERE id = 999 USING OPERATION_ID 'nn3'"
        ),
        "23502"
    );
    ok(
        &core,
        "UPDATE nn SET v = DEFAULT WHERE id = 1 USING OPERATION_ID 'nn4'",
    );
    assert_eq!(
        rows(&core, "SELECT k, v FROM nn ORDER BY id LIMIT 10"),
        vec![vec![text("k1"), text("d")]]
    );
}

/// RETURNING は等価なリテラルの SET と同じ列メタ・行を返す（id 指定形・述語形）。
#[test]
fn returning_matches_equivalent_literal_update() {
    let (core, _g) = setup("sql50-upd-returning");
    let lit = ok(
        &core,
        "UPDATE t SET c = 'dflt', o = NULL WHERE id = 1 RETURNING * USING OPERATION_ID 'r1'",
    );
    let def = ok(
        &core,
        "UPDATE t SET c = DEFAULT, o = DEFAULT WHERE id = 2 RETURNING * USING OPERATION_ID 'r2'",
    );
    let (lit_cols, lit_rows) = returning_of(&lit);
    let (def_cols, def_rows) = returning_of(&def);
    assert_eq!(lit_cols, def_cols);
    // `a`・`n` は行ごとに異なるため、更新対象の `c`・`o` を比べる。
    assert_eq!(lit_rows[0][2], def_rows[0][2]);
    assert_eq!(lit_rows[0][4], def_rows[0][4]);
    let lit = ok(
        &core,
        "UPDATE t SET c = 'dflt', n = 7 WHERE id > 2 RETURNING c, n USING OPERATION_ID 'r3'",
    );
    let def = ok(
        &core,
        "UPDATE t SET c = DEFAULT, n = DEFAULT WHERE id = 1 RETURNING c, n USING OPERATION_ID 'r4'",
    );
    let (lit_cols, lit_rows) = returning_of(&lit);
    let (def_cols, def_rows) = returning_of(&def);
    assert_eq!(lit_cols, def_cols);
    assert_eq!(lit_rows.len(), 1);
    assert_eq!(lit_rows, def_rows);
}

/// `ALTER COLUMN SET DEFAULT` 後は新しい既定、`DROP DEFAULT` 後は NULL（TABLE-39 (1)(2)）。
#[test]
fn follows_alter_column_default() {
    let (core, _g) = setup("sql50-upd-alter");
    ok(&core, "ALTER TABLE t ALTER COLUMN c SET DEFAULT 'changed'");
    ok(
        &core,
        "UPDATE t SET c = DEFAULT WHERE id = 1 USING OPERATION_ID 'a1'",
    );
    assert_eq!(t_rows(&core)[0][1], text("changed"));
    ok(&core, "ALTER TABLE t ALTER COLUMN c DROP DEFAULT");
    ok(
        &core,
        "UPDATE t SET c = DEFAULT WHERE id = 2 USING OPERATION_ID 'a2'",
    );
    assert_eq!(t_rows(&core)[1][1], Cell::Null);
}

/// 再送（RECOVER-10・RECOVER-11）: 同一 OPERATION_ID・同一文は列 DEFAULT の変更を挟んでも `23505`、
/// `DEFAULT` を等価リテラルへ替えた同一 OPERATION_ID の文は内容違い（`22023`）。
#[test]
fn resend_is_duplicate_after_default_change_and_literal_swap_is_mismatch() {
    let (core, _g) = setup("sql50-upd-resend");
    let id_sql = "UPDATE t SET c = DEFAULT WHERE id = 1 USING OPERATION_ID 'op-id'";
    let pred_sql = "UPDATE t SET c = DEFAULT WHERE id > 1 USING OPERATION_ID 'op-pred'";
    ok(&core, id_sql);
    ok(&core, pred_sql);
    assert_eq!(code(&core, id_sql), "23505");
    assert_eq!(code(&core, pred_sql), "23505");
    ok(&core, "ALTER TABLE t ALTER COLUMN c SET DEFAULT 'changed'");
    assert_eq!(code(&core, id_sql), "23505");
    assert_eq!(code(&core, pred_sql), "23505");
    // 二重適用されていない。
    assert!(t_rows(&core).iter().all(|r| r[1] == text("dflt")));
    assert_eq!(
        code(
            &core,
            "UPDATE t SET c = 'dflt' WHERE id = 1 USING OPERATION_ID 'op-id'"
        ),
        "22023"
    );
    assert_eq!(
        code(
            &core,
            "UPDATE t SET c = 'dflt' WHERE id > 1 USING OPERATION_ID 'op-pred'"
        ),
        "22023"
    );
}

/// CHECK 制約に違反する既定値は通常の SET と同じく `23514`。
#[test]
fn check_constraint_applies_to_resolved_default() {
    let (core, _g) = setup("sql50-upd-check");
    ok(
        &core,
        "CREATE TABLE ck (n INTEGER DEFAULT 5, CHECK (n < 3))",
    );
    ok(
        &core,
        "INSERT INTO ck (id, n) VALUES (1, 1) USING OPERATION_ID 'ck-seed'",
    );
    assert_eq!(
        code(
            &core,
            "UPDATE ck SET n = DEFAULT WHERE id = 1 USING OPERATION_ID 'ck1'"
        ),
        "23514"
    );
    assert_eq!(
        rows(&core, "SELECT n FROM ck ORDER BY id LIMIT 10"),
        vec![vec![int(1)]]
    );
}

/// 評価文脈の無い既定（時刻・SEQUENCE・IDENTITY）は `0A000` で拒否し、行は変えない（fail-closed）。
#[test]
fn context_dependent_defaults_are_rejected() {
    let (core, _g) = setup("sql50-upd-ctx");
    ok(&core, "CREATE SEQUENCE sq");
    ok(
        &core,
        "CREATE TABLE s (g BIGINT GENERATED ALWAYS AS IDENTITY, q BIGINT DEFAULT nextval('sq'), \
         ts TIMESTAMP DEFAULT now(), v TEXT)",
    );
    ok(
        &core,
        "INSERT INTO s (id, v) VALUES (1, 'x') USING OPERATION_ID 's-seed'",
    );
    let before = rows(&core, "SELECT g, q, v FROM s ORDER BY id LIMIT 10");
    for (col, op) in [("g", "c1"), ("q", "c2"), ("ts", "c3")] {
        assert_eq!(
            code(
                &core,
                &format!("UPDATE s SET {col} = DEFAULT WHERE id = 1 USING OPERATION_ID '{op}'")
            ),
            "0A000",
            "{col}"
        );
    }
    assert_eq!(
        rows(&core, "SELECT g, q, v FROM s ORDER BY id LIMIT 10"),
        before
    );
}

/// 構文の fail-closed: 引用付き `"DEFAULT"` は列参照（`42703`）、`DEFAULT + 1` は `42601`、
/// UPSERT の `DO UPDATE SET c = DEFAULT` は従来どおり拒否する。
#[test]
fn syntax_edges_stay_fail_closed() {
    let (core, _g) = setup("sql50-upd-syntax");
    assert_eq!(
        code(
            &core,
            "UPDATE t SET c = \"DEFAULT\" WHERE id = 1 USING OPERATION_ID 'x1'"
        ),
        "42703"
    );
    assert_eq!(
        code(
            &core,
            "UPDATE t SET n = DEFAULT + 1 WHERE id = 1 USING OPERATION_ID 'x2'"
        ),
        "42601"
    );
    assert_eq!(
        code(
            &core,
            "INSERT INTO t (id, a) VALUES (1, 'x') ON CONFLICT (id) DO UPDATE SET c = DEFAULT"
        ),
        "42601"
    );
    // 擬似列は DEFAULT の解決より先に拒否。
    assert_eq!(
        code(
            &core,
            "UPDATE t SET id = DEFAULT WHERE id = 1 USING OPERATION_ID 'x3'"
        ),
        "42601"
    );
    assert_eq!(t_rows(&core)[0][1], text("c1"));
}

/// ビュー経由の UPDATE は基表の列 DEFAULT が効く。
#[test]
fn view_update_uses_base_default() {
    let (core, _g) = setup("sql50-upd-view");
    ok(&core, "CREATE VIEW v AS SELECT id, a, c, n FROM t");
    ok(
        &core,
        "UPDATE v SET c = DEFAULT WHERE id = 1 USING OPERATION_ID 'v1'",
    );
    assert_eq!(t_rows(&core)[0][1], text("dflt"));
}

/// 分割実行（PARTITIONED）でも全行に既定値が入る。
#[test]
fn partitioned_update_applies_default_to_all_rows() {
    let (core, _g) = setup("sql50-upd-part");
    assert_eq!(
        affected(ok(
            &core,
            "UPDATE t SET c = DEFAULT WHERE id > 0 USING OPERATION_ID 'p1' PARTITIONED CHUNK 1"
        )),
        3
    );
    assert!(t_rows(&core).iter().all(|r| r[1] == text("dflt")));
}

/// テナント境界: 他テナントの行 id への `SET c = DEFAULT` は不存在 id と同じ `UPDATE 0` で、
/// 他テナントの行は変わらない。
#[test]
fn tenant_boundary_is_preserved() {
    let (core, _g) = setup("sql50-upd-tenant");
    ok_as(
        &core,
        "alice",
        "INSERT INTO t (id, a, c) VALUES (10, 'al', 'cal') USING OPERATION_ID 'al-1'",
    );
    assert_eq!(
        affected(ok_as(
            &core,
            "bob",
            "UPDATE t SET c = DEFAULT WHERE id = 10 USING OPERATION_ID 'bob-1'"
        )),
        0
    );
    assert_eq!(
        affected(ok_as(
            &core,
            "bob",
            "UPDATE t SET c = DEFAULT WHERE id = 999 USING OPERATION_ID 'bob-2'"
        )),
        0
    );
    assert_eq!(
        rows_as(&core, "alice", "SELECT c FROM t WHERE id = 10 LIMIT 10"),
        vec![vec![text("cal")]]
    );
}

/// 拡張クエリ: `$n` を含む文でも `DEFAULT` を受理し、束縛・実行できる。
#[test]
fn extended_query_with_params() {
    let (core, _g) = setup("sql50-upd-ext");
    let prepared = core
        .parse_sql_prepared(
            "UPDATE t SET c = DEFAULT, a = $1 WHERE id = $2 USING OPERATION_ID 'e1'",
        )
        .expect("prepare");
    assert_eq!(prepared.param_types().len(), 2);
}
