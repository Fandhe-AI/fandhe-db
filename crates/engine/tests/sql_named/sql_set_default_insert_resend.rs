//! `SET DEFAULT` の INSERT 系への反映と、`operation_id` 再送時の内容照合（content_hash）・
//! 台帳整合（TABLE-39 (1)(2)(6)、RECOVER-10、RECOVER-11、TABLE-5、TABLE-16、Issue #1629）の
//! 結合テスト。ポインタ: `docs/spec/04-behavior/data-model.md`・`docs/spec/04-behavior/recovery.md`。
//!
//! 実 `Storage` ＋ `CpuScalarProvider` の production 経路（`EngineCore::execute_sql_in_session`）で、
//! (1) 省略列の補完が現在の INSERT 既定値（`default_expr`）を使うこと、(2) 省略列に適用した
//! DEFAULT は内容照合ハッシュの材料に含まれず、DEFAULT の変更・削除・追加（ADD COLUMN）を挟んだ
//! 同一文の再送が `23505`（再実行しない）になることを固定する。
//! 対象外: ファイル形 INSERT・COPY・NoSQL の再送系は同じ `fill_omitted_columns` 経由で追随する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("owner", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open_core(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn exec(core: &EngineCore, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx(), &mut session, sql)
}

fn ok(core: &EngineCore, sql: &str) {
    exec(core, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn code(core: &EngineCore, sql: &str) -> String {
    match exec(core, sql) {
        Ok(_) => panic!("statement must fail: {sql}"),
        Err(e) => e.wire_code().to_string(),
    }
}

fn cells(core: &EngineCore, sql: &str) -> Vec<Cell> {
    let r = core
        .execute_sql(&ctx(), &format!("{sql} LIMIT 20"))
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    r.rows
        .into_iter()
        .map(|row| row.cells.into_iter().next().expect("cell"))
        .collect()
}

fn text(s: &str) -> Cell {
    Cell::Text(s.to_string())
}

fn setup(label: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(label);
    let core = open_core(&path);
    ok(
        &core,
        "CREATE TABLE docs (id BIGINT PRIMARY KEY, a TEXT, c TEXT DEFAULT 'old', t TIMESTAMP DEFAULT now())",
    );
    (core, path)
}

const SINGLE: &str = "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-s'";
const MULTI: &str =
    "INSERT INTO docs (id, a) VALUES (11, 'x'), (12, 'y') USING OPERATION_ID 'op-m'";
const UPSERT: &str = "INSERT INTO docs (id, a) VALUES (21, 'x') \
                      ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a USING OPERATION_ID 'op-u'";

/// 反映: SET DEFAULT の後、単一行・複数行・UPSERT の省略列は新しい既定値で保存される。
/// DROP DEFAULT 後の nullable 列は NULL。既存行は変わらない。
#[test]
fn insert_paths_use_current_default() {
    let (core, path) = setup("1629-reflect");
    let _g = CleanupGuard(path);
    ok(&core, "ALTER TABLE docs ALTER COLUMN c SET DEFAULT 'new'");
    ok(&core, SINGLE);
    ok(&core, MULTI);
    ok(&core, UPSERT);
    for id in [1, 11, 12, 21] {
        assert_eq!(
            cells(&core, &format!("SELECT c FROM docs WHERE id = {id}")),
            vec![text("new")],
            "id {id}"
        );
    }
    ok(&core, "ALTER TABLE docs ALTER COLUMN c DROP DEFAULT");
    ok(
        &core,
        "INSERT INTO docs (id, a) VALUES (2, 'z') USING OPERATION_ID 'op-2'",
    );
    assert_eq!(
        cells(&core, "SELECT c FROM docs WHERE id = 2"),
        vec![Cell::Null]
    );
    assert_eq!(
        cells(&core, "SELECT c FROM docs WHERE id = 1"),
        vec![text("new")]
    );
}

/// 再送 1: SET DEFAULT を挟んだ同一文の再送は `23505`（再実行しない）。行は記録時の既定値のまま。
#[test]
fn resend_after_set_default_is_duplicate() {
    let (core, path) = setup("1629-resend-set");
    let _g = CleanupGuard(path);
    for sql in [SINGLE, MULTI, UPSERT] {
        ok(&core, sql);
    }
    ok(&core, "ALTER TABLE docs ALTER COLUMN c SET DEFAULT 'new'");
    for sql in [SINGLE, MULTI, UPSERT] {
        assert_eq!(code(&core, sql), "23505", "{sql}");
    }
    assert_eq!(
        cells(&core, "SELECT c FROM docs WHERE id = 1"),
        vec![text("old")]
    );
}

/// 再送 2: DROP DEFAULT（定数・時刻とも）や時刻 DEFAULT の構文形変更を挟んでも `23505`。
#[test]
fn resend_after_drop_default_is_duplicate() {
    let (core, path) = setup("1629-resend-drop");
    let _g = CleanupGuard(path);
    for sql in [SINGLE, MULTI, UPSERT] {
        ok(&core, sql);
    }
    ok(
        &core,
        "ALTER TABLE docs ALTER COLUMN t SET DEFAULT CURRENT_TIMESTAMP",
    );
    for sql in [SINGLE, MULTI, UPSERT] {
        assert_eq!(code(&core, sql), "23505", "{sql}");
    }
    ok(&core, "ALTER TABLE docs ALTER COLUMN c DROP DEFAULT");
    ok(&core, "ALTER TABLE docs ALTER COLUMN t DROP DEFAULT");
    for sql in [SINGLE, MULTI, UPSERT] {
        assert_eq!(code(&core, sql), "23505", "{sql}");
    }
}

/// 再送 3: 記録後に DEFAULT 付き ADD COLUMN を挟んでも `23505`（ADD COLUMN 不変条件の回復）。
#[test]
fn resend_after_add_column_with_default_is_duplicate() {
    let (core, path) = setup("1629-resend-add");
    let _g = CleanupGuard(path);
    for sql in [SINGLE, MULTI, UPSERT] {
        ok(&core, sql);
    }
    ok(&core, "ALTER TABLE docs ADD COLUMN e TEXT DEFAULT 'x'");
    for sql in [SINGLE, MULTI, UPSERT] {
        assert_eq!(code(&core, sql), "23505", "{sql}");
    }
}

/// 対照: 強さの維持。明示した値が異なる同 op_id の文は `22023`。
#[test]
fn different_explicit_content_is_still_rejected() {
    let (core, path) = setup("1629-contrast");
    let _g = CleanupGuard(path);
    ok(&core, SINGLE);
    ok(&core, "ALTER TABLE docs ALTER COLUMN c SET DEFAULT 'new'");
    assert_eq!(
        code(
            &core,
            "INSERT INTO docs (id, a) VALUES (1, 'other') USING OPERATION_ID 'op-s'"
        ),
        "22023"
    );
    // 省略していた列を明示すると別内容。
    assert_eq!(
        code(
            &core,
            "INSERT INTO docs (id, a, c) VALUES (1, 'x', 'old') USING OPERATION_ID 'op-s'"
        ),
        "22023"
    );
}

/// 既知の衝突（D3）: 明示 NULL と、省略して DEFAULT が適用される文は同一ハッシュになる。
/// 明示 NULL を種別付きでハッシュする拡張は #1525 と同種の互換性判断として別 Issue で扱う。
#[test]
fn known_collision_explicit_null_vs_omitted_with_default() {
    let (core, path) = setup("1629-known-collision");
    let _g = CleanupGuard(path);
    ok(
        &core,
        "INSERT INTO docs (id, a, c) VALUES (1, 'x', NULL) USING OPERATION_ID 'op-k'",
    );
    assert_eq!(
        code(
            &core,
            "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-k'"
        ),
        "23505"
    );
}

/// 復旧: 記録 → SET DEFAULT → 再 open → 同じ文の再送は `23505`（台帳は永続）。
#[test]
fn resend_after_reopen_is_duplicate() {
    let (core, path) = setup("1629-reopen");
    let _g = CleanupGuard(path.clone());
    ok(&core, SINGLE);
    ok(&core, "ALTER TABLE docs ALTER COLUMN c SET DEFAULT 'new'");
    drop(core);
    let core = open_core(&path);
    assert_eq!(code(&core, SINGLE), "23505");
    assert_eq!(
        cells(&core, "SELECT c FROM docs WHERE id = 1"),
        vec![text("old")]
    );
}

/// 再送 4（NOT NULL）: NOT NULL DEFAULT 列の DROP DEFAULT／SET DEFAULT を挟んでも、
/// 同一文の再送は束縛で `23502` にならず `23505`（台帳照合が NOT NULL 検査より先）。
/// 台帳に無い新規の文は DROP DEFAULT 後に `23502` で拒否され、台帳追記も残らない。
#[test]
fn resend_after_default_change_on_not_null_column_is_duplicate() {
    let path = unique_db_path("1629-resend-not-null");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    ok(
        &core,
        "CREATE TABLE nn (id BIGINT PRIMARY KEY, a TEXT, c TEXT NOT NULL DEFAULT 'old')",
    );
    let single = "INSERT INTO nn (id, a) VALUES (1, 'x') USING OPERATION_ID 'nn-s'";
    let multi = "INSERT INTO nn (id, a) VALUES (11, 'x'), (12, 'y') USING OPERATION_ID 'nn-m'";
    ok(&core, single);
    ok(&core, multi);

    ok(&core, "ALTER TABLE nn ALTER COLUMN c SET DEFAULT 'new'");
    assert_eq!(code(&core, single), "23505");
    assert_eq!(code(&core, multi), "23505");

    ok(&core, "ALTER TABLE nn ALTER COLUMN c DROP DEFAULT");
    assert_eq!(code(&core, single), "23505");
    assert_eq!(code(&core, multi), "23505");
    assert_eq!(
        cells(&core, "SELECT c FROM nn WHERE id = 1"),
        vec![text("old")]
    );

    // 新規の文は NOT NULL 違反（fail-closed）。行も台帳も残らない。
    let fresh = "INSERT INTO nn (id, a) VALUES (2, 'z') USING OPERATION_ID 'nn-new'";
    let fresh_multi =
        "INSERT INTO nn (id, a) VALUES (3, 'z'), (4, 'w') USING OPERATION_ID 'nn-new-m'";
    assert_eq!(code(&core, fresh), "23502");
    assert_eq!(code(&core, fresh_multi), "23502");
    assert!(cells(&core, "SELECT c FROM nn WHERE id = 2").is_empty());
    ok(&core, "ALTER TABLE nn ALTER COLUMN c SET DEFAULT 'again'");
    ok(&core, fresh);
    assert_eq!(
        cells(&core, "SELECT c FROM nn WHERE id = 2"),
        vec![text("again")]
    );
}
