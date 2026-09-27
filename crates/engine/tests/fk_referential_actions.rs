//! `FOREIGN KEY` の参照アクション（`CASCADE`・`SET NULL`・`SET DEFAULT`。
//! Issue #1076・TABLE-17・TASK-205）の結合テスト。ポインタ:
//! `docs/spec/04-behavior/data-model.md` TABLE-17・`rls.md` RLS-9・RLS-10・
//! `error-format.md` ERR-4。
//!
//! 宣言・永続化（カタログ v8 `fk:` 行の 5 フィールド形）の往復は
//! `crates/engine/src/catalog.rs` の単体テストが担う。本ファイルは連鎖の適用
//! （`constraint::propagate_referential_actions`）を production 経路
//! （`EngineCore::execute_sql_in_session`）で検証する。`table17_foreign_key.rs`
//! と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`）。設計判断は
//! `docs/design/foreign-key.md` 参照（spec 本文は転記しない）。

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

fn new_core(label: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn granted_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn run(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = granted_session();
    core.execute_sql_in_session(ctx, &mut session, sql)
}

fn ok(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> SqlOutcome {
    run(core, ctx, sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
}

fn err_code(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> String {
    run(core, ctx, sql)
        .map(|o| panic!("{sql} must fail, got {o:?}"))
        .unwrap_err()
        .wire_code()
        .to_string()
}

fn row_count(core: &EngineCore, ctx: &PolicyContext, table: &str) -> usize {
    match ok(core, ctx, &format!("SELECT id FROM {table} LIMIT 10000")) {
        SqlOutcome::Query(result) => result.rows.len(),
        other => panic!("expected Query outcome, got {other:?}"),
    }
}

/// `table` の `id = id` の行の投影列 `column`（先頭 1 セル）を返す。行が無ければ
/// `None`。
fn select_cell(
    core: &EngineCore,
    ctx: &PolicyContext,
    table: &str,
    id: u64,
    column: &str,
) -> Option<Cell> {
    match ok(
        core,
        ctx,
        &format!("SELECT {column} FROM {table} WHERE id = {id} LIMIT 1"),
    ) {
        SqlOutcome::Query(result) => result.rows.first().map(|r| r.cells[0].clone()),
        other => panic!("expected Query outcome, got {other:?}"),
    }
}

// --- 宣言・永続化 -------------------------------------------------------------

#[test]
fn declared_actions_are_accepted_and_persist_across_reopen() {
    let (core, path) = new_core("fkact-declare");
    let _guard = CleanupGuard(path.clone());
    let sys = ctx("sys");
    ok(&core, &sys, "CREATE TABLE parents (name TEXT)");
    ok(
        &core,
        &sys,
        "CREATE TABLE children (\
         parent_id BIGINT REFERENCES parents ON DELETE CASCADE ON UPDATE CASCADE, \
         note TEXT)",
    );
    drop(core);
    let storage = Storage::open(&path).expect("reopen storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    // 再オープン後も宣言（アクション込み）が有効であることを、書き込みの
    // 挙動（CASCADE が実際に発火する）で確認する。
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO parents (id, name) VALUES (1, 'p1') USING OPERATION_ID 'op-p'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO children (id, parent_id) VALUES (10, 1) USING OPERATION_ID 'op-c'",
    );
    ok(
        &core,
        &alice,
        "DELETE FROM parents WHERE id = 1 USING OPERATION_ID 'op-d'",
    );
    assert_eq!(row_count(&core, &alice, "children"), 0);
}

/// 宣言時検査（Issue #1076 A8）: 常に失敗する宣言を `42830` で拒否する。
#[test]
fn declaring_always_failing_referential_actions_is_rejected_with_42830() {
    let (core, path) = new_core("fkact-a8");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    ok(&core, &sys, "CREATE TABLE parents (name TEXT)");
    for sql in [
        // (a) SET NULL だが参照元列が NOT NULL。
        "CREATE TABLE c1 (v BIGINT NOT NULL REFERENCES parents ON DELETE SET NULL)",
        // (b) SET DEFAULT だが参照元列が NOT NULL かつ DEFAULT 無し。
        "CREATE TABLE c2 (v BIGINT NOT NULL REFERENCES parents ON UPDATE SET DEFAULT)",
    ] {
        assert_eq!(err_code(&core, &sys, sql), "42830", "{sql}");
    }
}

// --- ON DELETE ----------------------------------------------------------------

fn create_id_parent_with_action(core: &EngineCore, on_delete: &str) {
    let sys = ctx("sys");
    ok(core, &sys, "CREATE TABLE parents (name TEXT)");
    ok(
        core,
        &sys,
        &format!(
            "CREATE TABLE children (parent_id BIGINT REFERENCES parents ON DELETE {on_delete}, note TEXT)"
        ),
    );
}

#[test]
fn on_delete_cascade_removes_child_rows_atomically() {
    let (core, path) = new_core("fkact-del-cascade");
    let _guard = CleanupGuard(path);
    create_id_parent_with_action(&core, "CASCADE");
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO parents (id, name) VALUES (1, 'p1') USING OPERATION_ID 'op-p'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO children (id, parent_id) VALUES (10, 1), (11, 1) USING OPERATION_ID 'op-c'",
    );
    // 参照しない子行は削除の対象外。
    ok(
        &core,
        &alice,
        "INSERT INTO children (id, note) VALUES (12, 'orphan') USING OPERATION_ID 'op-c2'",
    );
    let outcome = ok(
        &core,
        &alice,
        "DELETE FROM parents WHERE id = 1 USING OPERATION_ID 'op-d'",
    );
    match outcome {
        // 元の文の `rows_affected` は親行だけを数える（連鎖は別カウント。A12）。
        SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 1),
        other => panic!("expected Delete outcome, got {other:?}"),
    }
    assert_eq!(row_count(&core, &alice, "parents"), 0);
    assert_eq!(row_count(&core, &alice, "children"), 1);
    assert_eq!(
        select_cell(&core, &alice, "children", 12, "note"),
        Some(Cell::Text("orphan".to_string()))
    );
}

#[test]
fn on_delete_set_null_clears_child_fk_column() {
    let (core, path) = new_core("fkact-del-setnull");
    let _guard = CleanupGuard(path);
    create_id_parent_with_action(&core, "SET NULL");
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO parents (id, name) VALUES (1, 'p1') USING OPERATION_ID 'op-p'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO children (id, parent_id) VALUES (10, 1) USING OPERATION_ID 'op-c'",
    );
    ok(
        &core,
        &alice,
        "DELETE FROM parents WHERE id = 1 USING OPERATION_ID 'op-d'",
    );
    assert_eq!(row_count(&core, &alice, "children"), 1);
    assert_eq!(
        select_cell(&core, &alice, "children", 10, "parent_id"),
        Some(Cell::Null)
    );
}

#[test]
fn on_delete_set_default_missing_default_row_is_23503_with_no_side_effects() {
    let (core, path) = new_core("fkact-del-setdefault-missing");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    ok(&core, &sys, "CREATE TABLE parents (name TEXT)");
    ok(
        &core,
        &sys,
        "CREATE TABLE children (\
         parent_id BIGINT DEFAULT 999 REFERENCES parents ON DELETE SET DEFAULT, note TEXT)",
    );
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO parents (id, name) VALUES (1, 'p1') USING OPERATION_ID 'op-p'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO children (id, parent_id) VALUES (10, 1) USING OPERATION_ID 'op-c'",
    );
    // `DEFAULT` 値 999 は `parents` に存在しないため、連鎖適用後の事後検証
    // （A3 手順 2）が `23503` で拒否する。副作用ゼロ（親・子とも変化なし）。
    assert_eq!(
        err_code(
            &core,
            &alice,
            "DELETE FROM parents WHERE id = 1 USING OPERATION_ID 'op-d'"
        ),
        "23503"
    );
    assert_eq!(row_count(&core, &alice, "parents"), 1);
    assert_eq!(
        select_cell(&core, &alice, "children", 10, "parent_id"),
        Some(Cell::SignedInteger(1))
    );
}

#[test]
fn on_delete_set_default_with_existing_default_row_succeeds() {
    let (core, path) = new_core("fkact-del-setdefault-ok");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    ok(&core, &sys, "CREATE TABLE parents (name TEXT)");
    ok(
        &core,
        &sys,
        "CREATE TABLE children (\
         parent_id BIGINT DEFAULT 2 REFERENCES parents ON DELETE SET DEFAULT, note TEXT)",
    );
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO parents (id, name) VALUES (1, 'p1'), (2, 'fallback') USING OPERATION_ID 'op-p'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO children (id, parent_id) VALUES (10, 1) USING OPERATION_ID 'op-c'",
    );
    ok(
        &core,
        &alice,
        "DELETE FROM parents WHERE id = 1 USING OPERATION_ID 'op-d'",
    );
    assert_eq!(
        select_cell(&core, &alice, "children", 10, "parent_id"),
        Some(Cell::SignedInteger(2))
    );
}

// --- ON UPDATE ------------------------------------------------------------------

fn create_natural_key_parent_with_action(core: &EngineCore, on_update: &str) {
    let sys = ctx("sys");
    ok(
        core,
        &sys,
        "CREATE TABLE countries (code TEXT PRIMARY KEY, label TEXT)",
    );
    ok(
        core,
        &sys,
        &format!(
            "CREATE TABLE cities (country TEXT REFERENCES countries(code) ON UPDATE {on_update}, name TEXT)"
        ),
    );
}

#[test]
fn on_update_cascade_propagates_new_key_value() {
    let (core, path) = new_core("fkact-upd-cascade");
    let _guard = CleanupGuard(path);
    create_natural_key_parent_with_action(&core, "CASCADE");
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO countries (id, code, label) VALUES (1, 'JP', 'Japan') USING OPERATION_ID 'op-p'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO cities (id, country, name) VALUES (1, 'JP', 'Tokyo') USING OPERATION_ID 'op-c'",
    );
    ok(
        &core,
        &alice,
        "UPDATE countries SET code = 'JPN' WHERE id = 1 USING OPERATION_ID 'op-u'",
    );
    assert_eq!(
        select_cell(&core, &alice, "cities", 1, "country"),
        Some(Cell::Text("JPN".to_string()))
    );
    // 非キー列（`label`）の更新は連鎖を発火させない（早期 return。既存動作維持）。
    ok(
        &core,
        &alice,
        "UPDATE countries SET label = 'Nippon' WHERE id = 1 USING OPERATION_ID 'op-u2'",
    );
    assert_eq!(
        select_cell(&core, &alice, "cities", 1, "country"),
        Some(Cell::Text("JPN".to_string()))
    );
}

#[test]
fn on_update_set_null_via_predicate_and_upsert() {
    let (core, path) = new_core("fkact-upd-setnull");
    let _guard = CleanupGuard(path);
    create_natural_key_parent_with_action(&core, "SET NULL");
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO countries (id, code) VALUES (1, 'JP') USING OPERATION_ID 'op-p'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO cities (id, country, name) VALUES (1, 'JP', 'Tokyo') USING OPERATION_ID 'op-c'",
    );
    ok(
        &core,
        &alice,
        "UPDATE countries SET code = 'JPN' WHERE code = 'JP' USING OPERATION_ID 'op-u'",
    );
    assert_eq!(
        select_cell(&core, &alice, "cities", 1, "country"),
        Some(Cell::Null)
    );

    // UPSERT の `DO UPDATE` 経由でも同じ連鎖が起きる（`upsert_typed_rows_unchecked`
    // の pre-image 捕捉。predicate UPDATE とは別の書き込み経路）。
    ok(
        &core,
        &alice,
        "INSERT INTO cities (id, country, name) VALUES (2, 'JPN', 'Osaka') USING OPERATION_ID 'op-c2'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO countries (id, code) VALUES (1, 'NEW') \
         ON CONFLICT (id) DO UPDATE SET code = EXCLUDED.code USING OPERATION_ID 'op-u2'",
    );
    assert_eq!(
        select_cell(&core, &alice, "cities", 2, "country"),
        Some(Cell::Null)
    );
}

// UNIQUE 対象（`ON CONFLICT (code)`）の UPSERT で、衝突する既存行の `id` が
// VALUES 自身の `id` と異なる場合の回帰（PR #1138 codex/review・cursor bugbot
// 指摘）。ON UPDATE pre-image は「実際に書き換わる既存行」の id で記録しなければ
// ならず、VALUES の id で記録すると連鎖側が誤った（存在しない）親行を読み直し
// CASCADE が発火しない。
#[test]
fn on_update_cascade_via_unique_target_upsert_uses_existing_row_id() {
    let (core, path) = new_core("fkact-upd-cascade-unique-upsert");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    // `code` は（`PRIMARY KEY` ではなく）`UNIQUE` 制約として宣言する。
    // `ON CONFLICT (code)` が `UpsertTarget::Unique` として解決されるには
    // 宣言済み UNIQUE 制約との一致が必要（`PRIMARY KEY` 単独では一致しない）。
    ok(
        &core,
        &sys,
        "CREATE TABLE countries (code TEXT UNIQUE, label TEXT)",
    );
    ok(
        &core,
        &sys,
        "CREATE TABLE cities (country TEXT REFERENCES countries(code) ON UPDATE CASCADE, name TEXT)",
    );
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO countries (id, code) VALUES (1, 'JP') USING OPERATION_ID 'op-p'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO cities (id, country, name) VALUES (1, 'JP', 'Tokyo') USING OPERATION_ID 'op-c'",
    );
    // VALUES の `id`（99）は既存の衝突行の `id`（1）と異なる。`ON CONFLICT (code)`
    // は UNIQUE 対象（natural key）なので、実際に書き換わるのは id=1 の既存行。
    ok(
        &core,
        &alice,
        "INSERT INTO countries (id, code) VALUES (99, 'JP') \
         ON CONFLICT (code) DO UPDATE SET code = 'JPN' USING OPERATION_ID 'op-u'",
    );
    assert_eq!(
        select_cell(&core, &alice, "cities", 1, "country"),
        Some(Cell::Text("JPN".to_string()))
    );
    // 新規行（id=99）は挿入されていない（衝突により UPDATE のみが適用された）。
    assert_eq!(select_cell(&core, &alice, "countries", 99, "code"), None);
}

// --- 多段連鎖・自己参照 ----------------------------------------------------------

#[test]
fn cascade_propagates_through_multiple_levels() {
    let (core, path) = new_core("fkact-multi-level");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    ok(
        &core,
        &sys,
        "CREATE TABLE countries (code TEXT PRIMARY KEY)",
    );
    ok(
        &core,
        &sys,
        "CREATE TABLE cities (country TEXT REFERENCES countries(code) ON DELETE CASCADE, name TEXT)",
    );
    ok(
        &core,
        &sys,
        "CREATE TABLE districts (city_id BIGINT REFERENCES cities ON DELETE CASCADE, name TEXT)",
    );
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO countries (id, code) VALUES (1, 'JP') USING OPERATION_ID 'op-p'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO cities (id, country, name) VALUES (1, 'JP', 'Tokyo') USING OPERATION_ID 'op-c'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO districts (id, city_id, name) VALUES (1, 1, 'Shibuya') USING OPERATION_ID 'op-d'",
    );
    ok(
        &core,
        &alice,
        "DELETE FROM countries WHERE id = 1 USING OPERATION_ID 'op-del'",
    );
    assert_eq!(row_count(&core, &alice, "countries"), 0);
    assert_eq!(row_count(&core, &alice, "cities"), 0);
    assert_eq!(row_count(&core, &alice, "districts"), 0);
}

#[test]
fn self_referencing_cascade_delete_removes_descendant_tree() {
    let (core, path) = new_core("fkact-self-ref");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    ok(
        &core,
        &sys,
        "CREATE TABLE nodes (parent_id BIGINT REFERENCES nodes ON DELETE CASCADE, name TEXT)",
    );
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO nodes (id, name) VALUES (1, 'root') USING OPERATION_ID 'op-root'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO nodes (id, parent_id, name) VALUES (2, 1, 'child') USING OPERATION_ID 'op-c1'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO nodes (id, parent_id, name) VALUES (3, 2, 'grandchild') USING OPERATION_ID 'op-c2'",
    );
    ok(
        &core,
        &alice,
        "DELETE FROM nodes WHERE id = 1 USING OPERATION_ID 'op-del'",
    );
    assert_eq!(row_count(&core, &alice, "nodes"), 0);
}

/// 連鎖の深さ上限（Issue #1076 A10。実装既定値 `MAX_REFERENTIAL_ACTION_DEPTH`）を
/// 超える自己参照の木を根から `CASCADE` 削除すると `54000` で拒否され、副作用が
/// 一切残らない（A11。行・台帳とも痕跡ゼロ）。
#[test]
fn cascade_delete_beyond_depth_limit_is_54000_with_zero_side_effects() {
    let (core, path) = new_core("fkact-depth-limit");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    ok(
        &core,
        &sys,
        "CREATE TABLE nodes (parent_id BIGINT REFERENCES nodes ON DELETE CASCADE, name TEXT)",
    );
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO nodes (id, name) VALUES (1, 'root') USING OPERATION_ID 'op-root'",
    );
    // 深さ 16 段（`MAX_REFERENTIAL_ACTION_DEPTH`）を超える線形の親子チェーンを
    // 作る（root=1 の子が 2、2 の子が 3、... と続く一本鎖）。
    for id in 2..=20u64 {
        ok(
            &core,
            &alice,
            &format!(
                "INSERT INTO nodes (id, parent_id, name) VALUES ({id}, {prev}, 'n{id}') USING OPERATION_ID 'op-n{id}'",
                prev = id - 1
            ),
        );
    }
    assert_eq!(
        err_code(
            &core,
            &alice,
            "DELETE FROM nodes WHERE id = 1 USING OPERATION_ID 'op-del'"
        ),
        "54000"
    );
    // 副作用ゼロ: 全 20 行がそのまま残る。
    assert_eq!(row_count(&core, &alice, "nodes"), 20);
    // 台帳にも記録されていないため、同じ `operation_id` は依然として未使用
    // （別の内容で再送でき、拒否原因は残らない）。
    assert_eq!(
        err_code(
            &core,
            &alice,
            "DELETE FROM nodes WHERE id = 1 USING OPERATION_ID 'op-del'"
        ),
        "54000"
    );
}

/// 連鎖で変更した各テーブルも全 FK について事後検証する（Issue #1076 A3 手順 2）
/// ことを固定する: 孫段が `NO ACTION` の場合、`ON DELETE CASCADE` で子行を削除
/// できても孫行が子を参照したままだと文全体が `23503` で拒否され、親・子・孫の
/// いずれも副作用が残らない。
#[test]
fn grandchild_no_action_rejects_the_whole_cascade_atomically() {
    let (core, path) = new_core("fkact-grandchild-no-action");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    ok(&core, &sys, "CREATE TABLE parents (name TEXT)");
    ok(
        &core,
        &sys,
        "CREATE TABLE children (parent_id BIGINT REFERENCES parents ON DELETE CASCADE, name TEXT)",
    );
    // 孫段は既定の `NO ACTION`（宣言しない）。
    ok(
        &core,
        &sys,
        "CREATE TABLE grandchildren (child_id BIGINT REFERENCES children, name TEXT)",
    );
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO parents (id, name) VALUES (1, 'p1') USING OPERATION_ID 'op-p'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO children (id, parent_id, name) VALUES (1, 1, 'c1') USING OPERATION_ID 'op-c'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO grandchildren (id, child_id, name) VALUES (1, 1, 'g1') USING OPERATION_ID 'op-g'",
    );
    assert_eq!(
        err_code(
            &core,
            &alice,
            "DELETE FROM parents WHERE id = 1 USING OPERATION_ID 'op-del'"
        ),
        "23503"
    );
    // 副作用ゼロ: 連鎖で削除されるはずだった children も含め、全テーブルが不変。
    assert_eq!(row_count(&core, &alice, "parents"), 1);
    assert_eq!(row_count(&core, &alice, "children"), 1);
    assert_eq!(row_count(&core, &alice, "grandchildren"), 1);
}

// --- テナント境界・TRUNCATE ------------------------------------------------------

#[test]
fn other_tenant_child_rows_are_never_touched_by_cascade() {
    let (core, path) = new_core("fkact-tenant-boundary");
    let _guard = CleanupGuard(path);
    create_id_parent_with_action(&core, "CASCADE");
    let alice = ctx("alice");
    let bob = ctx("bob");
    ok(
        &core,
        &alice,
        "INSERT INTO parents (id, name) VALUES (1, 'p1') USING OPERATION_ID 'op-p-a'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO children (id, parent_id) VALUES (10, 1) USING OPERATION_ID 'op-c-a'",
    );
    // bob は独立したテナント名前空間に同じ id 1 の親行を持つ（RLS-9）。
    ok(
        &core,
        &bob,
        "INSERT INTO parents (id, name) VALUES (1, 'p1-bob') USING OPERATION_ID 'op-p-b'",
    );
    ok(
        &core,
        &bob,
        "INSERT INTO children (id, parent_id) VALUES (10, 1) USING OPERATION_ID 'op-c-b'",
    );
    ok(
        &core,
        &alice,
        "DELETE FROM parents WHERE id = 1 USING OPERATION_ID 'op-d'",
    );
    // alice の子行は連鎖で削除される一方、bob の行は一切変化しない。
    assert_eq!(row_count(&core, &alice, "children"), 0);
    assert_eq!(row_count(&core, &bob, "parents"), 1);
    assert_eq!(row_count(&core, &bob, "children"), 1);
}

#[test]
fn truncate_does_not_fire_referential_actions() {
    let (core, path) = new_core("fkact-truncate");
    let _guard = CleanupGuard(path);
    create_id_parent_with_action(&core, "CASCADE");
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO parents (id, name) VALUES (1, 'p1') USING OPERATION_ID 'op-p'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO children (id, parent_id) VALUES (10, 1) USING OPERATION_ID 'op-c'",
    );
    // `TRUNCATE` は `CASCADE` を発火させない（A4）。自テナントの参照元行が
    // 残っているため `23503` で拒否され、`parents` の内容も変化しない。
    assert_eq!(
        err_code(
            &core,
            &alice,
            "TRUNCATE TABLE parents USING OPERATION_ID 'op-t'"
        ),
        "23503"
    );
    assert_eq!(row_count(&core, &alice, "parents"), 1);
    assert_eq!(row_count(&core, &alice, "children"), 1);
}
