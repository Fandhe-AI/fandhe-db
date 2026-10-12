//! `UPDATE ... SET col = NULL`・`ON CONFLICT ... DO UPDATE SET col = NULL` の NULL リテラル
//! 受理（SQL-41・Issue #1524）の結合テスト。
//! ポインタ: `docs/design/not-null-default.md`・`docs/design/implementation-status.md`。
//! 関連ビヘイビア: TABLE-16・TABLE-26（VECTOR の nullable 化は対象外）・RLS-9。
//!
//! 構文受理は `sql::allowlist`、束縛（nullable なら `Value::Null`・非 nullable なら `23502`）は
//! `sql::parser::bind_set_assignments`／`bind_upsert_assignments` の既存契約で、本ファイルは
//! 単一行形・述語形・分割実行・UPSERT を実 `Storage` 越しに固定する。
//! `sql_insert_null_literal.rs` と同じ流儀（`unique_db_path`／`CleanupGuard`）。

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

fn new_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        CleanupGuard(path),
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ddl(core: &EngineCore, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("sys"), &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
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

fn affected(outcome: SqlOutcome) -> u64 {
    match outcome {
        SqlOutcome::Update(o) => o.rows_affected,
        other => panic!("expected Update outcome, got {other:?}"),
    }
}

fn rows(core: &EngineCore, tenant: &str, sql: &str) -> Vec<Vec<Cell>> {
    match ok(core, tenant, sql) {
        SqlOutcome::Query(r) => r.rows.into_iter().map(|row| row.cells).collect(),
        other => panic!("expected Query outcome, got {other:?}"),
    }
}

/// 列: `lang`（nullable）・`req`（NOT NULL）・`u`（UNIQUE）。alice に id 1〜3、bob に id 4。
fn setup(label: &str) -> (EngineCore, CleanupGuard) {
    let (core, g) = new_core(label);
    ddl(
        &core,
        "CREATE TABLE docs (lang TEXT, req TEXT NOT NULL DEFAULT 'r', u TEXT UNIQUE)",
    );
    for (id, tenant) in [(1, "alice"), (2, "alice"), (3, "alice"), (4, "bob")] {
        ok(
            &core,
            tenant,
            &format!(
                "INSERT INTO docs (id, lang, req, u) VALUES ({id}, 'ja', 'r{id}', 'u{id}') \
                 USING OPERATION_ID 'seed-{id}'"
            ),
        );
    }
    (core, g)
}

fn null_ids(core: &EngineCore, tenant: &str, col: &str) -> usize {
    rows(
        core,
        tenant,
        &format!("SELECT id FROM docs WHERE {col} IS NULL LIMIT 100"),
    )
    .len()
}

fn cell_of(core: &EngineCore, tenant: &str, col: &str, id: u64) -> Cell {
    let r = rows(
        core,
        tenant,
        &format!("SELECT {col} FROM docs WHERE id = {id} LIMIT 10"),
    );
    assert_eq!(r.len(), 1);
    r[0][0].clone()
}

fn is_text(cell: &Cell, expected: &str) -> bool {
    matches!(cell, Cell::Text(s) if s == expected)
}

#[test]
fn single_row_set_null_stores_null_and_leaves_other_columns() {
    let (core, _g) = setup("upd-null-single");
    let n = affected(ok(
        &core,
        "alice",
        "UPDATE docs SET lang = NULL WHERE id = 1 USING OPERATION_ID 'u1'",
    ));
    assert_eq!(n, 1);
    assert!(matches!(cell_of(&core, "alice", "lang", 1), Cell::Null));
    assert!(is_text(&cell_of(&core, "alice", "req", 1), "r1"));
    assert!(is_text(&cell_of(&core, "alice", "lang", 2), "ja"));
    assert_eq!(null_ids(&core, "alice", "lang"), 1);
}

#[test]
fn predicate_form_set_null_updates_multiple_rows() {
    let (core, _g) = setup("upd-null-pred");
    let n = affected(ok(
        &core,
        "alice",
        "UPDATE docs SET lang = NULL WHERE lang = 'ja' USING OPERATION_ID 'u1'",
    ));
    assert_eq!(n, 3);
    assert_eq!(null_ids(&core, "alice", "lang"), 3);
    // 他テナントの行は不変（RLS）。
    assert!(is_text(&cell_of(&core, "bob", "lang", 4), "ja"));
}

#[test]
fn set_null_on_not_null_column_is_23502_and_leaves_all_rows() {
    let (core, _g) = setup("upd-null-notnull");
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE docs SET req = NULL WHERE id = 1 USING OPERATION_ID 'u1'"
        ),
        "23502"
    );
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE docs SET req = NULL WHERE lang = 'ja' USING OPERATION_ID 'u2'"
        ),
        "23502"
    );
    assert_eq!(null_ids(&core, "alice", "req"), 0);
    assert!(is_text(&cell_of(&core, "alice", "req", 1), "r1"));
}

#[test]
fn set_null_on_primary_key_column_is_23502() {
    let (core, _g) = new_core("upd-null-pk");
    ddl(&core, "CREATE TABLE pk (k TEXT PRIMARY KEY, v TEXT)");
    ok(
        &core,
        "alice",
        "INSERT INTO pk (id, k, v) VALUES (1, 'a', 'x') USING OPERATION_ID 's1'",
    );
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE pk SET k = NULL WHERE id = 1 USING OPERATION_ID 'u1'"
        ),
        "23502"
    );
}

#[test]
fn partitioned_set_null_rejects_without_partial_commit_or_job() {
    let (core, _g) = setup("upd-null-partitioned");
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE docs SET req = NULL WHERE lang = 'ja' USING OPERATION_ID 'p1' \
             PARTITIONED CHUNK 1"
        ),
        "23502"
    );
    assert_eq!(null_ids(&core, "alice", "req"), 0);
    // ジョブは登録されていない（束縛エラーは分割実行器より前）。
    let shown = rows(&core, "alice", "SHOW PARTITIONED DML 'p1' ON docs");
    assert!(shown.is_empty(), "{shown:?}");

    // nullable 列なら分割実行でも全行に NULL が入る。
    let n = affected(ok(
        &core,
        "alice",
        "UPDATE docs SET lang = NULL WHERE lang = 'ja' USING OPERATION_ID 'p2' \
         PARTITIONED CHUNK 1",
    ));
    assert_eq!(n, 3);
    assert_eq!(null_ids(&core, "alice", "lang"), 3);
}

#[test]
fn upsert_do_update_set_null() {
    let (core, _g) = setup("upd-null-upsert");
    ok(
        &core,
        "alice",
        "INSERT INTO docs (id, lang, req) VALUES (1, 'en', 'x') ON CONFLICT (id) \
         DO UPDATE SET lang = NULL USING OPERATION_ID 'up1'",
    );
    assert!(matches!(cell_of(&core, "alice", "lang", 1), Cell::Null));
    assert_eq!(
        code(
            &core,
            "alice",
            "INSERT INTO docs (id, lang, req) VALUES (2, 'en', 'x') ON CONFLICT (id) \
             DO UPDATE SET req = NULL USING OPERATION_ID 'up2'"
        ),
        "23502"
    );
    assert!(is_text(&cell_of(&core, "alice", "req", 2), "r2"));
    assert!(is_text(&cell_of(&core, "alice", "lang", 2), "ja"));
}

#[test]
fn unique_key_is_freed_by_set_null_and_nulls_are_distinct() {
    let (core, _g) = setup("upd-null-unique");
    ok(
        &core,
        "alice",
        "UPDATE docs SET u = NULL WHERE id = 1 USING OPERATION_ID 'u1'",
    );
    ok(
        &core,
        "alice",
        "UPDATE docs SET u = NULL WHERE id = 2 USING OPERATION_ID 'u2'",
    );
    ok(
        &core,
        "alice",
        "INSERT INTO docs (id, u) VALUES (10, 'u1') USING OPERATION_ID 'i1'",
    );
    assert_eq!(null_ids(&core, "alice", "u"), 2);
}

#[test]
fn scalar_index_hit_drops_row_updated_to_null() {
    let (core, _g) = setup("upd-null-index");
    ddl(&core, "CREATE INDEX idx_lang ON docs (lang)");
    let ja = |core: &EngineCore| {
        rows(
            core,
            "alice",
            "SELECT id FROM docs WHERE lang = 'ja' LIMIT 100",
        )
        .len()
    };
    assert_eq!(ja(&core), 3);
    ok(
        &core,
        "alice",
        "UPDATE docs SET lang = NULL WHERE id = 1 USING OPERATION_ID 'u1'",
    );
    assert_eq!(ja(&core), 2);
    assert_eq!(null_ids(&core, "alice", "lang"), 1);
}

/// VECTOR 列の NULL は現行コードを固定するだけで、揃える作業は TABLE-26 の範囲。
#[test]
fn vector_column_null_keeps_current_codes() {
    let (core, _g) = new_core("upd-null-vector");
    ddl(&core, "CREATE TABLE v (embedding VECTOR(2), note TEXT)");
    ok(
        &core,
        "alice",
        "INSERT INTO v (id, embedding, note) VALUES (1, '[0.1,0.2]', 'n') \
         USING OPERATION_ID 's1'",
    );
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE v SET embedding = NULL WHERE id = 1 USING OPERATION_ID 'u1'"
        ),
        "22000"
    );
    assert_eq!(
        code(
            &core,
            "alice",
            "INSERT INTO v (id, embedding) VALUES (1, '[0.1,0.2]') ON CONFLICT (id) \
             DO UPDATE SET embedding = NULL USING OPERATION_ID 'u2'"
        ),
        "23502"
    );
}

#[test]
fn rls_other_tenant_row_is_untouched_and_error_is_row_independent() {
    let (core, _g) = setup("upd-null-rls");
    // bob から alice の行 id=1 を狙っても UPDATE 0 で、行は不変。
    let n = affected(ok(
        &core,
        "bob",
        "UPDATE docs SET lang = NULL WHERE id = 1 USING OPERATION_ID 'b1'",
    ));
    assert_eq!(n, 0);
    assert!(is_text(&cell_of(&core, "alice", "lang", 1), "ja"));
    // NOT NULL 列の 23502 は対象行の有無（自テナント行・他テナント行・不在 id）で同一。
    let codes: Vec<String> = [1, 4, 999]
        .iter()
        .map(|id| {
            code(
                &core,
                "alice",
                &format!("UPDATE docs SET req = NULL WHERE id = {id} USING OPERATION_ID 'c{id}'"),
            )
        })
        .collect();
    assert_eq!(codes, vec!["23502", "23502", "23502"]);
}

/// 二重引用符識別子 `"NULL"` は NULL 定数として受理しない（Issue #1524 レビュー指摘）。
/// SET 右辺が式を受理する（Issue #1544・SQL-45）ようになったため、`"NULL"` は列参照として
/// 解析され、そのような列は無いので `22000`（未知の列）で拒否される。対象行を含むどの行も
/// 変化しない。
#[test]
fn quoted_null_identifier_is_rejected_and_row_unchanged() {
    let (core, _g) = setup("upd-null-quoted");
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE docs SET lang = \"NULL\" WHERE id = 1 USING OPERATION_ID 'q1'",
        ),
        "22000"
    );
    assert!(is_text(&cell_of(&core, "alice", "lang", 1), "ja"));
    assert!(is_text(&cell_of(&core, "alice", "lang", 2), "ja"));
}

/// 同一 `operation_id` で NULL 割当だけが異なる単一行 UPDATE は内容不一致（`22023`）になる
/// （RECOVER-10。Issue #1524 レビュー指摘: NULL 列が内容照合ハッシュから脱落しない）。
#[test]
fn same_operation_id_with_different_null_assignment_is_content_mismatch() {
    let (core, _g) = setup("upd-null-hash");
    ok(
        &core,
        "alice",
        "UPDATE docs SET lang = 'x' WHERE id = 1 USING OPERATION_ID 'h1'",
    );
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE docs SET lang = 'x', u = NULL WHERE id = 1 USING OPERATION_ID 'h1'",
        ),
        "22023"
    );
    assert!(is_text(&cell_of(&core, "alice", "u", 1), "u1"));
}

/// `EXPLAIN UPDATE ... PARTITIONED` でも引用付き `"NULL"` は NULL 定数として受理せず
/// 列参照として束縛を試みて `22000`（実行時の UPDATE と同じ契約。Issue #1524 レビュー指摘、
/// Issue #1544 で `42601` から変更）。引用なしの `NULL` は従来どおり EXPLAIN を受理する。
#[test]
fn explain_partitioned_update_distinguishes_quoted_null() {
    let (core, _g) = setup("upd-null-explain");
    assert_eq!(
        code(
            &core,
            "alice",
            "EXPLAIN UPDATE docs SET lang = \"NULL\" WHERE id > 0 \
             USING OPERATION_ID 'ex-q' PARTITIONED",
        ),
        "22000"
    );
    assert!(matches!(
        ok(
            &core,
            "alice",
            "EXPLAIN UPDATE docs SET lang = NULL WHERE id > 0 \
             USING OPERATION_ID 'ex-n' PARTITIONED",
        ),
        SqlOutcome::Explain(_)
    ));
}
