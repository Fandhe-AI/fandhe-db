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

/// 宣言時検査（Issue #1201・TABLE-20）: `ON UPDATE CASCADE` で参照元列が `NOT NULL`・
/// 参照先列が NULL 許容の組は、参照先が NULL へ更新された瞬間に必ず失敗する宣言
/// のため `42830` で拒否する。`CREATE TABLE`（列制約形・表制約形）と
/// `ALTER TABLE ... ADD FOREIGN KEY` の 2 経路（いずれも
/// `catalog::resolve_foreign_key_target` を通る）と、拒否後に副作用が残らないこと、
/// 拒否されない対照形（陽性対照）を固定する。(a)(b) は上のテストが担う。
#[test]
fn declaring_on_update_cascade_from_not_null_to_nullable_parent_is_rejected_with_42830() {
    let (core, path) = new_core("fkact-a8c");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    // `code` は NULL 許容の UNIQUE 列（`PRIMARY KEY` は NOT NULL 扱いのため使わない）。
    ok(
        &core,
        &sys,
        "CREATE TABLE parents (code TEXT UNIQUE, label TEXT)",
    );
    let column_form =
        "CREATE TABLE c1 (code TEXT NOT NULL REFERENCES parents(code) ON UPDATE CASCADE)";
    let table_form = "CREATE TABLE c2 (code TEXT NOT NULL, \
         FOREIGN KEY (code) REFERENCES parents(code) ON UPDATE CASCADE)";
    for sql in [column_form, table_form] {
        assert_eq!(err_code(&core, &sys, sql), "42830", "{sql}");
    }
    // 副作用ゼロ: カタログに何も残っていない（同名テーブルを正しい形で作れる）。
    ok(&core, &sys, "CREATE TABLE c1 (code TEXT NOT NULL)");
    ok(&core, &sys, "CREATE TABLE c2 (code TEXT NOT NULL)");

    // ALTER TABLE 経路。
    ok(&core, &sys, "CREATE TABLE c3 (code TEXT NOT NULL)");
    assert_eq!(
        err_code(
            &core,
            &sys,
            "ALTER TABLE c3 ADD FOREIGN KEY (code) REFERENCES parents(code) ON UPDATE CASCADE"
        ),
        "42830"
    );
    // 拒否後に FK は追加されていない（参照を満たさない行の INSERT が成功する）。
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO c3 (id, code) VALUES (1, 'no-such-parent') USING OPERATION_ID 'op-c3'",
    );

    // 陽性対照: 同形でも受理される宣言。
    for sql in [
        "CREATE TABLE ok1 (code TEXT NOT NULL REFERENCES parents(code) ON UPDATE NO ACTION)",
        "CREATE TABLE ok2 (code TEXT NOT NULL REFERENCES parents(code) ON DELETE CASCADE)",
        "CREATE TABLE ok3 (code TEXT REFERENCES parents(code) ON UPDATE CASCADE)",
    ] {
        ok(&core, &sys, sql);
    }
    ok(
        &core,
        &sys,
        "CREATE TABLE strict_parents (code TEXT NOT NULL UNIQUE, label TEXT)",
    );
    ok(
        &core,
        &sys,
        "CREATE TABLE ok4 (code TEXT NOT NULL REFERENCES strict_parents(code) ON UPDATE CASCADE)",
    );
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

/// Cursor Bugbot 指摘（永続キー索引・Issue #1071 との統合バグ）: `ON DELETE
/// CASCADE`（`constraint::apply_referential_action`）が子行を物理削除する際、
/// その子テーブル自身が持つ登録済み永続キー索引（孫段の列参照 FK が使う）を
/// 同期しないと、削除済みの行の索引エントリが残留する。孫段は列参照 FK
/// （`grandchildren.child_code REFERENCES children(code)`）で `NO ACTION`
/// のため、この検査は登録済みなら索引照会（`verify_required_parent_keys` →
/// `key_index::all_keys_exist_in_txn`）に切り替わる——`children`/`["code"]`
/// 索引が未登録のまま（`grandchild_no_action_rejects_the_whole_cascade_atomically`
/// と異なり）事前の子行挿入で**先に登録済みにしてから** `CASCADE` を発火させ、
/// 残留エントリがあれば「参照先はまだ存在する」と誤判定して違反を見逃す
/// （fail-open）ことを固定する。
#[test]
fn cascade_delete_syncs_key_index_so_a_stale_entry_does_not_mask_a_no_action_violation() {
    let (core, path) = new_core("fkact-cascade-index-sync");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    ok(&core, &sys, "CREATE TABLE parents (code TEXT PRIMARY KEY)");
    ok(
        &core,
        &sys,
        "CREATE TABLE children (code TEXT PRIMARY KEY, parent_code TEXT REFERENCES parents (code) ON DELETE CASCADE)",
    );
    // 孫段は既定の `NO ACTION`（列参照。`children.code` を参照する）。
    ok(
        &core,
        &sys,
        "CREATE TABLE grandchildren (child_code TEXT REFERENCES children (code))",
    );
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO parents (id, code) VALUES (1, 'p1') USING OPERATION_ID 'op-p'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO children (id, code, parent_code) VALUES (1, 'c1', 'p1') USING OPERATION_ID 'op-c'",
    );
    // この挿入が `verify_required_parent_keys` の未登録フォールバックを経由し、
    // `children`/`["code"]` の永続キー索引を先に backfill・登録する。
    ok(
        &core,
        &alice,
        "INSERT INTO grandchildren (id, child_code) VALUES (1, 'c1') USING OPERATION_ID 'op-g'",
    );

    // 親の削除は `children` を CASCADE 削除するが、孫段は `NO ACTION` の
    // ため、削除された `children.code = 'c1'` をまだ参照する孫行がある限り
    // 文全体が `23503` で拒否されなければならない——`children`/`["code"]`
    // 索引が事前に登録済みでも同じ結果になる（索引の同期漏れがあれば、この
    // 事後検証だけが素通りして親の削除が成功してしまう）。
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

// --- 連鎖対象の限定（Issue #1076 codex-review 指摘対応） -----------------------
//
// `collect_action_targets` の `Removed` 分岐は、`tenant.rs` の各削除経路
// （`delete_row_impl`・`delete_rows_where_unchecked`・ファイル形 `INSERT` の
// 旧チャンク行置換）が `remove` の戻り値から復元して渡す `pre_images` を使い、
// 「その文で実際に削除された親キー」だけを連鎖対象にする（グローバルスキャン
// ＝「現在の親に存在しないキーを持つ子行全体」ではない）。`DELETE`／`UPDATE`
// は明示トランザクション内では未対応（`docs/design/explicit-transaction.md`）
// で、かつ自動コミットの各文は `INITIALLY DEFERRED` でも文単位で検査される
// （`autocommit_still_checks_deferred_foreign_key_per_statement`。
// `table17_foreign_key.rs`）ため、現行の SQL 表層では「無関係な既存孤立行が
// 誤って連鎖削除される」具体的な再現を単一の SQL 文だけでは組み立てられない。
// 直接検証できる範囲（同一親テーブルを参照する複数 FK の適用順序）は下記の
// テストで担保する。

/// 同一子テーブルが同一親テーブルを複数の `FOREIGN KEY`（別列）で参照する
/// 場合、親行の削除で発火する各 `ON DELETE SET NULL` はすべて適用されてから
/// 検証される（Issue #1076 codex-review 指摘対応）。最初の 1 件を適用した
/// 直後に子行の**全** FK を検証すると、まだ書き換えていない残りの FK が旧値
/// （削除済みの親キー）のまま検査され `23503` に誤って失敗する。
#[test]
fn multiple_foreign_keys_to_the_same_parent_all_apply_before_validation() {
    let (core, path) = new_core("fkact-multi-fk-same-parent");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    ok(&core, &sys, "CREATE TABLE parents (name TEXT)");
    ok(
        &core,
        &sys,
        "CREATE TABLE children (\
         parent_a BIGINT REFERENCES parents ON DELETE SET NULL, \
         parent_b BIGINT REFERENCES parents ON DELETE SET NULL, \
         note TEXT)",
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
        "INSERT INTO children (id, parent_a, parent_b, note) VALUES (10, 1, 1, 'n') \
         USING OPERATION_ID 'op-c'",
    );
    ok(
        &core,
        &alice,
        "DELETE FROM parents WHERE id = 1 USING OPERATION_ID 'op-d'",
    );
    assert_eq!(row_count(&core, &alice, "children"), 1);
    assert_eq!(
        select_cell(&core, &alice, "children", 10, "parent_a"),
        Some(Cell::Null)
    );
    assert_eq!(
        select_cell(&core, &alice, "children", 10, "parent_b"),
        Some(Cell::Null)
    );
}

// --- `id` 参照 FK と親側の PRIMARY KEY／UNIQUE 併存（codex-review・Cursor
// Bugbot 指摘対応） -----------------------------------------------------------
//
// 参照元列を省略した `REFERENCES parents`（`PRIMARY KEY` 未宣言）は疑似列 `id`
// へ解決される（D2）。親テーブルが `id` 以外の列に `UNIQUE`／`PRIMARY KEY` を
// 別途宣言していると、`tenant.rs` の削除経路は「参照先になり得る」と判定して
// 削除前の全列値（`removed_pre_images`）を積む。`id` 参照の `ON DELETE`
// アクションはこの pre-image 付き経路（`collect_action_targets` の限定版）を
// 通るため、`fk.parent_columns()`（疑似列 `id`）をライブスキーマの実列として
// 検索すると内部エラーになっていた（修正前は `CASCADE`／`SET NULL` のいずれも
// 適用前に失敗していた）。

#[test]
fn on_delete_cascade_for_id_reference_with_unrelated_parent_unique_column_succeeds() {
    let (core, path) = new_core("fkact-id-ref-parent-unique-cascade");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    ok(
        &core,
        &sys,
        "CREATE TABLE parents (code TEXT UNIQUE, name TEXT)",
    );
    ok(
        &core,
        &sys,
        "CREATE TABLE children (parent_id BIGINT REFERENCES parents ON DELETE CASCADE, note TEXT)",
    );
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO parents (id, code, name) VALUES (1, 'p-1', 'p1') USING OPERATION_ID 'op-p'",
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
    assert_eq!(row_count(&core, &alice, "parents"), 0);
    assert_eq!(row_count(&core, &alice, "children"), 0);
}

#[test]
fn on_delete_set_null_for_id_reference_with_unrelated_parent_unique_column_succeeds() {
    let (core, path) = new_core("fkact-id-ref-parent-unique-setnull");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    ok(
        &core,
        &sys,
        "CREATE TABLE parents (code TEXT UNIQUE, name TEXT)",
    );
    ok(
        &core,
        &sys,
        "CREATE TABLE children (parent_id BIGINT REFERENCES parents ON DELETE SET NULL, note TEXT)",
    );
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO parents (id, code, name) VALUES (1, 'p-1', 'p1') USING OPERATION_ID 'op-p'",
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

// --- `id` 参照 FK・親テーブルが主キー・UNIQUE を一切宣言しない場合の
// 述語 DELETE（codex-review 指摘・PR #1138）------------------------------------
//
// `id` 参照 FK（`REFERENCES parent`。D2）は親テーブルの物理キー `id` を参照先に
// でき、これは親テーブルが主キー・UNIQUE を一切宣言していなくても成立する。
// `tenant::delete_rows_where_unchecked`（述語 DELETE・SQL-19）の削除前
// pre-image 記録要否をこの事実を含めずに（親テーブル自身の主キー・UNIQUE 宣言
// の有無だけで）判定すると、`needs_fk_removed_pre_image` が偽になり
// `collect_action_targets` の限定版（`Removed` 分岐）を使えなくなる（`tenant.rs`
// の `needs_fk_removed_pre_image` 代入箇所のドキュメント参照）。本テストは複数
// 親行を 1 文でまとめて削除する経路（`delete_rows_where_unchecked`）で、この
// 判定が新しいカタログ走査を含めても壊れておらず、主キー・UNIQUE 未宣言の親
// テーブルでも各親行の CASCADE が正しく自分の子行にだけ適用されることを固定
// する。
//
// 注意（708〜721 行目の既存コメントと同じ限界）: 限定版とフォールバック
// （全走査）が実際に異なる結果を返すのは、削除対象と無関係な孤立行が
// トランザクション内に既に存在する場合のみ（`INITIALLY DEFERRED` の一時的な
// 合法孤立行 等）。`DELETE`／`UPDATE` は明示トランザクション内では未対応のため
// 単一の SQL 文（本テストを含む）だけではその分岐を判別できない——修正前の
// コードでも本テストは成功する。本テストは「新しい判定条件（カタログ走査）が
// 既存の正しい CASCADE 適用を壊していない」ことの回帰であり、フォールバックへの
// 意図しない分岐そのものを再現するものではない。
#[test]
fn predicate_delete_cascades_correctly_for_id_reference_when_parent_has_no_key() {
    let (core, path) = new_core("fkact-id-ref-no-key-predicate-delete");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    // `parents` は主キー・UNIQUE を一切宣言しない（`id` 疑似列のみが参照先）。
    ok(&core, &sys, "CREATE TABLE parents (group_name TEXT)");
    ok(
        &core,
        &sys,
        "CREATE TABLE children (parent_id BIGINT REFERENCES parents ON DELETE CASCADE, note TEXT)",
    );
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO parents (id, group_name) VALUES (1, 'target'), (2, 'target'), (3, 'keep') \
         USING OPERATION_ID 'op-p'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO children (id, parent_id, note) VALUES (10, 1, 'c1'), (11, 2, 'c2'), \
         (12, 3, 'c3') USING OPERATION_ID 'op-c'",
    );
    // 述語一致（`group_name = 'target'`）で親行 1・2 をまとめて削除する
    // （`delete_rows_where_unchecked`。単一 `id = N` の最適化経路とは別）。
    ok(
        &core,
        &alice,
        "DELETE FROM parents WHERE group_name = 'target' USING OPERATION_ID 'op-d'",
    );
    assert_eq!(row_count(&core, &alice, "parents"), 1);
    // 削除された親（1・2）の子だけが連鎖削除され、無関係な親（3）の子は残る。
    assert_eq!(row_count(&core, &alice, "children"), 1);
    assert_eq!(
        select_cell(&core, &alice, "children", 12, "note"),
        Some(Cell::Text("c3".to_string()))
    );
}

// --- 同一子列に競合する複数の参照アクションが作用する場合の宣言順非依存性
// （codex-review 指摘・PR #1138）--------------------------------------------------
//
// 子列 `x` が親の別々の `UNIQUE` 列（`code`・`alt_code`）をそれぞれ参照する
// 2 つの `FOREIGN KEY`（一方 `ON DELETE SET NULL`・他方 `ON DELETE CASCADE`）を
// 持つ場合、親行の `code` と `alt_code` が同じ値であれば、その親行の削除は
// 両方の FK にとって「参照先が失われた」削除になる。修正前は各 FK の対象を
// 現在の子行から順に収集・即適用していたため、宣言順で先に処理された
// `SET NULL` が子列 `x` を `NULL` に書き換えると、後から処理される `CASCADE`
// の走査ではその子行が対象から消え、削除されずに残っていた（宣言順で結果が
// 変わる不具合）。本テストは `SET NULL`／`CASCADE` の宣言順を入れ替えた 2 つの
// テーブル組で同じ最終状態（子行は削除される）になることを固定する。
fn create_competing_action_tables(core: &EngineCore, set_null_declared_first: bool) {
    let sys = ctx("sys");
    ok(
        core,
        &sys,
        "CREATE TABLE parents (code TEXT UNIQUE, alt_code TEXT UNIQUE)",
    );
    let children_ddl = if set_null_declared_first {
        "CREATE TABLE children (\
         x TEXT, note TEXT, \
         FOREIGN KEY (x) REFERENCES parents (code) ON DELETE SET NULL, \
         FOREIGN KEY (x) REFERENCES parents (alt_code) ON DELETE CASCADE)"
    } else {
        "CREATE TABLE children (\
         x TEXT, note TEXT, \
         FOREIGN KEY (x) REFERENCES parents (alt_code) ON DELETE CASCADE, \
         FOREIGN KEY (x) REFERENCES parents (code) ON DELETE SET NULL)"
    };
    ok(core, &sys, children_ddl);
}

#[test]
fn same_child_column_competing_actions_are_order_independent_of_declaration() {
    for set_null_declared_first in [true, false] {
        let label = if set_null_declared_first {
            "fkact-competing-set-null-first"
        } else {
            "fkact-competing-cascade-first"
        };
        let (core, path) = new_core(label);
        let _guard = CleanupGuard(path);
        create_competing_action_tables(&core, set_null_declared_first);
        let alice = ctx("alice");
        // `code`・`alt_code` を同じ値にし、この 1 行の削除が両方の FK にとって
        // 「参照先が失われた」削除になるようにする。
        ok(
            &core,
            &alice,
            "INSERT INTO parents (id, code, alt_code) VALUES (1, 'shared', 'shared') \
             USING OPERATION_ID 'op-p'",
        );
        ok(
            &core,
            &alice,
            "INSERT INTO children (id, x, note) VALUES (10, 'shared', 'c1') \
             USING OPERATION_ID 'op-c'",
        );
        ok(
            &core,
            &alice,
            "DELETE FROM parents WHERE id = 1 USING OPERATION_ID 'op-d'",
        );
        // 宣言順に関わらず `CASCADE` が最終的に勝ち、子行は削除される
        // （修正前は `set_null_declared_first == true` のケースで子行が
        // `x = NULL` のまま残っていた）。
        assert_eq!(
            row_count(&core, &alice, "children"),
            0,
            "set_null_declared_first={set_null_declared_first}: 宣言順に関わらず CASCADE が勝ち子行は削除されるべき"
        );
    }
}

// --- 参照アクション後の CHECK 再検証は通常の書き込み経路と同じ SQLSTATE を
// 返す（PR #1145・オーナー判断 2026-09-28・Issue #1075 との整合）------------------
//
// `propagate_referential_actions` は書き換えた子行を
// `constraint::enforce_row_constraints_in_txn`（`CHECK` → UNIQUE → FK 参照元側の
// 順に検査する唯一の入口。TABLE-16・TASK-204）で再検証する。これは通常の
// INSERT／UPDATE／UPSERT が使うのと**同一の**関数であり、`CHECK` 式評価中の
// エラー（0 除算等）を `TenantWriteError::CheckEvaluationFailed`（`SqlSurfaceError`
// を保持。PR #1145）として返す経路も共有する。本テストは `ON DELETE SET DEFAULT`
// が書き換えた列に依存する `CHECK` が評価エラー（0 除算）になる場合、参照
// アクション経由でも `23514`（制約違反）や `XX000` ではなく、通常の式評価と
// 同じ `22000`（`SqlSurfaceError::InvalidInput`）が返ることを固定する。
#[test]
fn referential_action_check_evaluation_error_uses_same_sqlstate_as_normal_write_path() {
    let (core, path) = new_core("fkact-check-eval-error-via-cascade");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    ok(&core, &sys, "CREATE TABLE parents (code TEXT UNIQUE)");
    ok(
        &core,
        &sys,
        "CREATE TABLE children (\
         divisor INTEGER DEFAULT 0 REFERENCES parents ON DELETE SET DEFAULT, \
         CHECK (100 / divisor > 1))",
    );
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO parents (id, code) VALUES (1, 'p1') USING OPERATION_ID 'op-p'",
    );
    // `divisor = 1` は現時点の CHECK を満たす（100 / 1 > 1）。
    ok(
        &core,
        &alice,
        "INSERT INTO children (id, divisor) VALUES (10, 1) USING OPERATION_ID 'op-c'",
    );
    // 親の削除で `ON DELETE SET DEFAULT` が `divisor` を `DEFAULT`（0）へ
    // 書き換え、再検証する `CHECK (100 / divisor > 1)` が 0 除算で評価エラーに
    // なる。通常の式評価（`compiled_checks_enforce_division_by_zero_fails_closed`）
    // と同じ `22000` になるべきで、`23514`（制約違反）や `XX000` にはならない。
    assert_eq!(
        err_code(
            &core,
            &alice,
            "DELETE FROM parents WHERE id = 1 USING OPERATION_ID 'op-d'"
        ),
        "22012"
    );
    // fail-closed: 副作用ゼロ（親行も子行も変化しない）。
    assert_eq!(row_count(&core, &alice, "parents"), 1);
    assert_eq!(
        select_cell(&core, &alice, "children", 10, "divisor"),
        Some(Cell::SignedInteger(1))
    );
}

// --- `SET NULL`／`SET DEFAULT` が `ON DELETE CASCADE` より先に適用される場合の
// 孫段連鎖（Cursor Bugbot 指摘・PR #1138）----------------------------------------
//
// 子列 `x` が親の別々の `UNIQUE` 列（`alt_code`・`code`）をそれぞれ参照する
// 2 つの `FOREIGN KEY`（一方 `ON DELETE SET NULL`・他方 `ON DELETE CASCADE`）を
// 持ち、`x` 自身が孫テーブルから参照される `UNIQUE` キーでもある場合。正準順
// （子テーブル名・参照元列が同じため参照先列の辞書順でタイブレーク。
// `"alt_code" < "code"`）で `SET NULL` が先に適用されると、修正前は後続の
// `CASCADE` が「現在の行」（`x` が既に `NULL` になった後）を再読取りして
// pre-image を作っていた。`NULL` は正しい参照先キーではないため、孫段の連鎖
// 対象特定（`collect_action_targets` の `wanted_keys`）が本来の旧キー（`x` の
// 削除前の値）を見失い、連鎖されず残った孫行が事後検証の `NO ACTION`
// バックストップで `23503` になっていた。
#[test]
fn set_null_applied_before_cascade_still_cascades_to_grandchildren() {
    let (core, path) = new_core("fkact-set-then-cascade-grandchild");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    ok(
        &core,
        &sys,
        "CREATE TABLE parents (code TEXT UNIQUE, alt_code TEXT UNIQUE)",
    );
    ok(
        &core,
        &sys,
        "CREATE TABLE children (\
         x TEXT UNIQUE, note TEXT, \
         FOREIGN KEY (x) REFERENCES parents (alt_code) ON DELETE SET NULL, \
         FOREIGN KEY (x) REFERENCES parents (code) ON DELETE CASCADE)",
    );
    ok(
        &core,
        &sys,
        "CREATE TABLE grandchildren (\
         child_x TEXT REFERENCES children (x) ON DELETE CASCADE, note TEXT)",
    );
    let alice = ctx("alice");
    // `code`・`alt_code` を同じ値にし、この親行の削除が両方の FK にとって
    // 「参照先が失われた」削除になるようにする。
    ok(
        &core,
        &alice,
        "INSERT INTO parents (id, code, alt_code) VALUES (1, 'shared', 'shared') \
         USING OPERATION_ID 'op-p'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO children (id, x, note) VALUES (10, 'shared', 'c1') \
         USING OPERATION_ID 'op-c'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO grandchildren (id, child_x, note) VALUES (100, 'shared', 'g1') \
         USING OPERATION_ID 'op-g'",
    );
    ok(
        &core,
        &alice,
        "DELETE FROM parents WHERE id = 1 USING OPERATION_ID 'op-d'",
    );
    // `SET NULL`（`alt_code` 経由）が正準順で先に適用されても、`CASCADE`
    // （`code` 経由）が最終的に子行を削除し、孫行も連鎖削除される。
    assert_eq!(row_count(&core, &alice, "children"), 0);
    assert_eq!(row_count(&core, &alice, "grandchildren"), 0);
}

// `SET NULL`／`SET DEFAULT`／`ON UPDATE CASCADE`（read-merge-write で子行を
// 書き換える経路。`constraint::apply_referential_action`）が、書き換えた
// 子行の永続キー索引（Issue #1071）を同期しないまま孫段の `NO ACTION`
// 事後検証（`enforce_referencing_rows_in_txn`）へ進むと、索引に旧キーが
// 残留し「参照先はまだ存在する」と誤判定して本来 `23503` になるべき更新が
// 素通りしてしまう（fail-open）懸念（PR #1146 codex-review 指摘）。
// `propagate_referential_actions` は `ColumnsUpdated` な子行 id を
// `pending_validation` へ蓄積し、全 FK 適用後に
// `enforce_row_constraints_in_txn`（内部で `sync_rows_in_txn` を先頭に呼ぶ）
// でまとめて検証する契約になっており、本テストはその契約が実際に索引の
// 同期漏れを防いでいることを固定する。
#[test]
fn on_update_cascade_syncs_key_index_for_grandchild_no_action_check() {
    let (core, path) = new_core("fkact-upd-cascade-index-sync");
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
        "CREATE TABLE cities (id_col INTEGER, country TEXT UNIQUE REFERENCES countries(code) ON UPDATE CASCADE)",
    );
    // 孫段は cities.country を参照する（既定の NO ACTION）。
    ok(
        &core,
        &sys,
        "CREATE TABLE districts (city_country TEXT REFERENCES cities(country))",
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
        "INSERT INTO cities (id, id_col, country) VALUES (1, 1, 'JP') USING OPERATION_ID 'op-c'",
    );
    // この挿入が cities/["country"] の永続キー索引を backfill・登録する。
    ok(
        &core,
        &alice,
        "INSERT INTO districts (id, city_country) VALUES (1, 'JP') USING OPERATION_ID 'op-g'",
    );

    // countries.code の更新は cities.country へ CASCADE するが、districts が
    // まだ旧値 'JP' を参照しているため、孫段 NO ACTION により全体が 23503 で
    // 拒否されなければならない。索引の同期漏れがあれば素通りしてしまう。
    let result = err_code(
        &core,
        &alice,
        "UPDATE countries SET code = 'JPN' WHERE id = 1 USING OPERATION_ID 'op-u'",
    );
    assert_eq!(result, "23503");
    assert_eq!(
        select_cell(&core, &alice, "cities", 1, "country"),
        Some(Cell::Text("JP".to_string()))
    );
}

// --- 連鎖行数の上限（Issue #1201・TABLE-20） ---------------------------------

/// 1 文あたりの連鎖対象行数の上限（`constraint::MAX_REFERENTIAL_ACTION_ROWS` =
/// 10,000）を超える `ON DELETE CASCADE` は `54000` で拒否され、副作用ゼロ
/// （親・子とも変更なし・台帳未記録）であること、予算がテナント内に閉じて
/// 他テナントの行に触れないこと、上限ちょうど（10,000 行）は成功することを固定する。
#[test]
fn cascade_delete_beyond_row_limit_is_54000_with_zero_side_effects() {
    use engine::batch_limits::BatchLimits;

    let path = unique_db_path("fkact-row-limit");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    // 1 万行超の複数行 INSERT を少ない文数で投入するため、バッチ行数上限を広げる。
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_batch_limits(
        BatchLimits {
            max_files_per_batch: 20_000,
            ..BatchLimits::default()
        },
    );
    let sys = ctx("sys");
    ok(&core, &sys, "CREATE TABLE parents (name TEXT)");
    ok(
        &core,
        &sys,
        "CREATE TABLE children (parent_id BIGINT REFERENCES parents ON DELETE CASCADE, note TEXT)",
    );
    let alice = ctx("alice");
    let bob = ctx("bob");
    ok(
        &core,
        &alice,
        "INSERT INTO parents (id, name) VALUES (1, 'p') USING OPERATION_ID 'op-a-p'",
    );
    ok(
        &core,
        &bob,
        "INSERT INTO parents (id, name) VALUES (1, 'p') USING OPERATION_ID 'op-b-p'",
    );
    let insert_children = |who: &PolicyContext, tag: &str, first: u64, count: u64| {
        let mut done = 0;
        let mut chunk = 0;
        while done < count {
            let n = (count - done).min(1_000);
            let values: Vec<String> = (0..n)
                .map(|i| format!("({}, 1, 'c')", first + done + i))
                .collect();
            ok(
                &core,
                who,
                &format!(
                    "INSERT INTO children (id, parent_id, note) VALUES {} USING OPERATION_ID 'op-{tag}-{chunk}'",
                    values.join(", ")
                ),
            );
            done += n;
            chunk += 1;
        }
    };
    insert_children(&alice, "a", 1, 10_001);
    insert_children(&bob, "b", 1, 50);

    let count = |who: &PolicyContext, table: &str| -> usize {
        // `LIMIT` 上限（10,000）を超える件数を観測するため `COUNT(*)` を使う。
        match ok(&core, who, &format!("SELECT COUNT(*) FROM {table}")) {
            SqlOutcome::Query(result) => match result.rows.first().map(|r| r.cells[0].clone()) {
                Some(Cell::Integer(n)) => n as usize,
                other => panic!("expected Integer count, got {other:?}"),
            },
            other => panic!("expected Query outcome, got {other:?}"),
        }
    };
    assert_eq!(count(&alice, "children"), 10_001);

    // 10,001 行 > 上限 10,000 行 → 54000。
    for _ in 0..2 {
        // 2 回目は同じ operation_id の再送: 台帳に残っていないため同じ結果になる。
        assert_eq!(
            err_code(
                &core,
                &alice,
                "DELETE FROM parents WHERE id = 1 USING OPERATION_ID 'op-del'"
            ),
            "54000"
        );
    }
    // 副作用ゼロ。他テナントも不変。
    assert_eq!(count(&alice, "parents"), 1);
    assert_eq!(count(&alice, "children"), 10_001);
    assert_eq!(count(&bob, "parents"), 1);
    assert_eq!(count(&bob, "children"), 50);

    // 境界: 子を 1 行減らして 10,000 行にすると連鎖は成功する。
    ok(
        &core,
        &alice,
        "DELETE FROM children WHERE id = 1 USING OPERATION_ID 'op-del-one'",
    );
    assert_eq!(count(&alice, "children"), 10_000);
    ok(
        &core,
        &alice,
        "DELETE FROM parents WHERE id = 1 USING OPERATION_ID 'op-del-ok'",
    );
    assert_eq!(count(&alice, "parents"), 0);
    assert_eq!(count(&alice, "children"), 0);
    // 予算・適用ともテナント内に閉じ、bob の行は変更されない。
    assert_eq!(count(&bob, "parents"), 1);
    assert_eq!(count(&bob, "children"), 50);
}
