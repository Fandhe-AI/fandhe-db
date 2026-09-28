//! `FOREIGN KEY` 制約（TABLE-17・TASK-205、Issue #907）の結合テスト。ポインタ:
//! `docs/spec/05-tasks.md` TASK-205・`docs/spec/04-behavior/data-model.md`
//! TABLE-17（関連: TABLE-12・TABLE-15・TABLE-16）・`rls.md` RLS-9・RLS-10・
//! `error-format.md` ERR-6。
//!
//! 宣言（`CREATE TABLE` の列制約・表制約 → `sql::ddl::execute_create_table` →
//! `catalog::Storage::create_table` の write トランザクション内での参照先解決）、
//! 永続化（カタログ v8）、書き込み時検査（参照元側は
//! `constraint::enforce_row_constraints_in_txn`、参照先側は
//! `constraint::enforce_referencing_rows_in_txn`。いずれも engine 内の単一検査点）を
//! production 経路（`EngineCore::execute_sql_in_session`／`execute_sql_in_txn`）で
//! 検証する。`table16_check_constraint.rs`・`unique_constraint.rs` と同じ流儀
//! （実 `Storage` ＋ `CpuScalarProvider`、`unique_db_path`／`CleanupGuard`）。
//! 設計判断は `docs/design/foreign-key.md` 参照（spec 本文は転記しない）。

use engine::catalog::CatalogError;
use engine::core::EngineCore;
use engine::embedding::HashingEmbedder;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
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
    match ok(core, ctx, &format!("SELECT id FROM {table} LIMIT 1000")) {
        SqlOutcome::Query(result) => result.rows.len(),
        other => panic!("expected Query outcome, got {other:?}"),
    }
}

/// `id` 参照の親子（`parents`／`children.parent_id BIGINT REFERENCES parents`）。
fn create_id_parent_and_child(core: &EngineCore) {
    let sys = ctx("sys");
    ok(core, &sys, "CREATE TABLE parents (name TEXT)");
    ok(
        core,
        &sys,
        "CREATE TABLE children (parent_id BIGINT REFERENCES parents, note TEXT)",
    );
}

/// 宣言済み主キー（`TEXT`）参照の親子（`countries (code TEXT PRIMARY KEY)`・
/// `cities.country TEXT REFERENCES countries`）。
fn create_pk_parent_and_child(core: &EngineCore) {
    let sys = ctx("sys");
    ok(
        core,
        &sys,
        "CREATE TABLE countries (code TEXT PRIMARY KEY, label TEXT)",
    );
    ok(
        core,
        &sys,
        "CREATE TABLE cities (country TEXT REFERENCES countries, name TEXT)",
    );
}

// --- DDL: 受理形状・参照先の解決 -------------------------------------------

#[test]
fn create_table_resolves_omitted_referenced_columns_to_id_or_declared_primary_key() {
    let (core, path) = new_core("fk-ddl-resolve");
    let _guard = CleanupGuard(path.clone());
    create_id_parent_and_child(&core);
    create_pk_parent_and_child(&core);
    drop(core);

    let storage = Storage::open(&path).expect("reopen storage");
    let children = storage.get_table_schema("children").expect("children");
    let [fk] = children.foreign_keys() else {
        panic!("children must declare exactly one foreign key");
    };
    assert_eq!(fk.columns(), ["parent_id".to_string()]);
    assert_eq!(fk.parent_table(), "parents");
    assert_eq!(fk.parent_columns(), ["id".to_string()]);

    let cities = storage.get_table_schema("cities").expect("cities");
    let [fk] = cities.foreign_keys() else {
        panic!("cities must declare exactly one foreign key");
    };
    assert_eq!(fk.parent_columns(), ["code".to_string()]);
}

#[test]
fn create_table_accepts_explicit_id_unique_and_composite_table_constraint_forms() {
    let (core, path) = new_core("fk-ddl-forms");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    ok(
        &core,
        &sys,
        "CREATE TABLE p (a TEXT NOT NULL, b INTEGER NOT NULL, u TEXT UNIQUE, PRIMARY KEY (a, b))",
    );
    ok(
        &core,
        &sys,
        "CREATE TABLE c1 (pid INTEGER REFERENCES p (id))",
    );
    ok(&core, &sys, "CREATE TABLE c2 (pu TEXT REFERENCES p (u))");
    // 表制約・複合列。参照先列は主キーと列集合が一致すれば順序は問わない。
    ok(
        &core,
        &sys,
        "CREATE TABLE c3 (x INTEGER, y TEXT, FOREIGN KEY (x, y) REFERENCES p (b, a))",
    );
    // 表制約は列定義の前にも置ける（位置非依存）。参照動作 NO ACTION／RESTRICT は受理。
    ok(
        &core,
        &sys,
        "CREATE TABLE c4 (FOREIGN KEY (y, x) REFERENCES p ON DELETE NO ACTION ON UPDATE RESTRICT, \
         x INTEGER, y TEXT)",
    );
}

#[test]
fn create_table_rejects_referenced_columns_without_unique_constraint_with_42830() {
    let (core, path) = new_core("fk-ddl-42830");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    ok(
        &core,
        &sys,
        "CREATE TABLE p (code TEXT PRIMARY KEY, plain TEXT, n INTEGER UNIQUE)",
    );
    // 一意性を保証しない列。
    assert_eq!(
        err_code(&core, &sys, "CREATE TABLE c (v TEXT REFERENCES p (plain))"),
        "42830"
    );
    // 型不一致（TEXT → INTEGER UNIQUE、TEXT → id）。
    assert_eq!(
        err_code(&core, &sys, "CREATE TABLE c (v TEXT REFERENCES p (n))"),
        "42830"
    );
    assert_eq!(
        err_code(&core, &sys, "CREATE TABLE c (v TEXT REFERENCES p (id))"),
        "42830"
    );
    // 列数の不一致。
    assert_eq!(
        err_code(
            &core,
            &sys,
            "CREATE TABLE c (v TEXT, w TEXT, FOREIGN KEY (v, w) REFERENCES p (code))"
        ),
        "42830"
    );
    // 主キー（TEXT）への暗黙解決で型が合わない。
    assert_eq!(
        err_code(&core, &sys, "CREATE TABLE c (v INTEGER REFERENCES p)"),
        "42830"
    );
    // 拒否された文はテーブルを作らない（同名で正しい宣言が成功する）。
    ok(&core, &sys, "CREATE TABLE c (v TEXT REFERENCES p)");
}

#[test]
fn create_table_rejects_missing_view_and_index_targets() {
    let (core, path) = new_core("fk-ddl-targets");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    assert_eq!(
        err_code(&core, &sys, "CREATE TABLE c (v BIGINT REFERENCES ghost)"),
        "42P01"
    );
    ok(&core, &sys, "CREATE TABLE docs (lang TEXT)");
    ok(&core, &sys, "CREATE VIEW v_docs AS SELECT * FROM docs");
    ok(&core, &sys, "CREATE INDEX idx_docs ON docs (lang)");
    assert_eq!(
        err_code(&core, &sys, "CREATE TABLE c (v BIGINT REFERENCES v_docs)"),
        "42809"
    );
    assert_eq!(
        err_code(&core, &sys, "CREATE TABLE c (v BIGINT REFERENCES idx_docs)"),
        "42809"
    );
    // 作成対象名の重複は参照先の検査より先に判定される。
    assert_eq!(
        err_code(&core, &sys, "CREATE TABLE docs (v BIGINT REFERENCES ghost)"),
        "42P07"
    );
}

#[test]
fn create_table_rejects_unsupported_foreign_key_shapes_with_42601() {
    let (core, path) = new_core("fk-ddl-42601");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    ok(&core, &sys, "CREATE TABLE p (name TEXT)");
    for sql in [
        // 参照アクション（Issue #1076）の未実装形: 列リスト形の SET NULL／
        // SET DEFAULT、`ON DELETE` の重複指定。
        "CREATE TABLE c (v BIGINT REFERENCES p ON DELETE SET NULL (v))",
        "CREATE TABLE c (v BIGINT REFERENCES p ON UPDATE SET DEFAULT (v))",
        "CREATE TABLE c (v BIGINT REFERENCES p ON DELETE RESTRICT ON DELETE RESTRICT)",
        "CREATE TABLE c (v BIGINT REFERENCES p ON DELETE CASCADE ON DELETE CASCADE)",
        // `MATCH PARTIAL` は非対応（TABLE-17・TASK-205、Issue #1077）。
        "CREATE TABLE c (v BIGINT REFERENCES p MATCH PARTIAL)",
        "CREATE TABLE c (v BIGINT REFERENCES p MATCH FULL MATCH FULL)",
        // `MATCH` は `ON` 句より前にのみ置ける（順序違反）。
        "CREATE TABLE c (v BIGINT REFERENCES p ON DELETE RESTRICT MATCH FULL)",
        // 遅延属性の矛盾・重複（TABLE-17・TASK-205、Issue #1077）。
        "CREATE TABLE c (v BIGINT REFERENCES p NOT DEFERRABLE INITIALLY DEFERRED)",
        "CREATE TABLE c (v BIGINT REFERENCES p DEFERRABLE DEFERRABLE)",
        "CREATE TABLE c (v BIGINT REFERENCES p DEFERRABLE NOT DEFERRABLE)",
        "CREATE TABLE c (v BIGINT REFERENCES p INITIALLY LATER)",
        // `RESTRICT`（即時検査が契約）と `INITIALLY DEFERRED`（COMMIT まで
        // 検査を遅延）の併用は fail-closed に拒否する（PR #1137 レビュー指摘）。
        "CREATE TABLE c (v BIGINT REFERENCES p ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED)",
        "CREATE TABLE c (v BIGINT REFERENCES p ON UPDATE RESTRICT DEFERRABLE INITIALLY DEFERRED)",
        "CREATE TABLE c (v BIGINT REFERENCES p ON DELETE NO ACTION ON UPDATE RESTRICT \
         DEFERRABLE INITIALLY DEFERRED)",
        "CREATE TABLE c (v BIGINT UNIQUE DEFERRABLE)",
        "CREATE TABLE c (v BIGINT REFERENCES p NOT NULL)",
        "CREATE TABLE c (v BIGINT, FOREIGN KEY (missing) REFERENCES p)",
        "CREATE TABLE c (v BIGINT, FOREIGN KEY (id) REFERENCES p)",
        "CREATE TABLE c (v BIGINT, CONSTRAINT fk_v FOREIGN KEY (v) REFERENCES p)",
        "CREATE TABLE c (v BIGINT REFERENCES p, FOREIGN KEY (v) REFERENCES p)",
        // 構造が同じでアクションだけが異なる重複宣言も拒否する（Issue #1076 A14）。
        "CREATE TABLE c (v BIGINT REFERENCES p, FOREIGN KEY (v) REFERENCES p ON DELETE CASCADE)",
    ] {
        assert_eq!(err_code(&core, &sys, sql), "42601", "{sql}");
    }
    // `ALTER TABLE ... ADD COLUMN ... REFERENCES` は対象外（許可リスト外）。
    assert_eq!(
        err_code(
            &core,
            &sys,
            "ALTER TABLE p ADD COLUMN v BIGINT REFERENCES p"
        ),
        "42601"
    );
    // `SET CONSTRAINTS` 自体が非対応（TABLE-17・TASK-205、Issue #1077。
    // `docs/design/foreign-key.md` D5 参照）。
    assert_eq!(
        err_code(&core, &sys, "SET CONSTRAINTS ALL DEFERRED"),
        "42601"
    );
}

// --- 参照元側（INSERT／UPDATE／UPSERT）---------------------------------------

#[test]
fn insert_requires_existing_parent_in_same_tenant_and_skips_null() {
    let (core, path) = new_core("fk-insert");
    let _guard = CleanupGuard(path);
    create_id_parent_and_child(&core);
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO parents (id, name) VALUES (1, 'p1') USING OPERATION_ID 'op-p1'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO children (id, parent_id) VALUES (10, 1) USING OPERATION_ID 'op-c10'",
    );
    // NULL は検査しない（MATCH SIMPLE）。列の省略も NULL。
    ok(
        &core,
        &alice,
        "INSERT INTO children (id, note) VALUES (11, 'orphan ok') USING OPERATION_ID 'op-c11'",
    );
    assert_eq!(
        err_code(
            &core,
            &alice,
            "INSERT INTO children (id, parent_id) VALUES (12, 999) USING OPERATION_ID 'op-c12'"
        ),
        "23503"
    );
    // 負値は物理キー `id` に一致し得ない。
    assert_eq!(
        err_code(
            &core,
            &alice,
            "INSERT INTO children (id, parent_id) VALUES (13, -1) USING OPERATION_ID 'op-c13'"
        ),
        "23503"
    );
    assert_eq!(row_count(&core, &alice, "children"), 2);
}

/// RLS-9・RLS-10 (c): 他テナントだけが保持する参照先と、どのテナントも保持しない
/// 参照先は、`wire_code`・文言の双方で区別できない。
#[test]
fn violation_response_does_not_reveal_other_tenant_parent_rows() {
    let (core, path) = new_core("fk-rls-parity");
    let _guard = CleanupGuard(path);
    create_id_parent_and_child(&core);
    create_pk_parent_and_child(&core);
    let alice = ctx("alice");
    let bob = ctx("bob");
    ok(
        &core,
        &bob,
        "INSERT INTO parents (id, name) VALUES (1, 'bob') USING OPERATION_ID 'op-bob-p'",
    );
    ok(
        &core,
        &bob,
        "INSERT INTO countries (id, code) VALUES (1, 'JP') USING OPERATION_ID 'op-bob-c'",
    );

    let pairs = [
        (
            "INSERT INTO children (id, parent_id) VALUES (1, 1) USING OPERATION_ID 'op-a1'",
            "INSERT INTO children (id, parent_id) VALUES (2, 777) USING OPERATION_ID 'op-a2'",
        ),
        (
            "INSERT INTO cities (id, country) VALUES (1, 'JP') USING OPERATION_ID 'op-a3'",
            "INSERT INTO cities (id, country) VALUES (2, 'ZZ') USING OPERATION_ID 'op-a4'",
        ),
    ];
    for (other_tenant_sql, missing_sql) in pairs {
        let e1 = run(&core, &alice, other_tenant_sql).expect_err("other tenant parent");
        let e2 = run(&core, &alice, missing_sql).expect_err("missing parent");
        assert_eq!(e1.wire_code(), "23503");
        assert_eq!(e2.wire_code(), "23503");
        assert_eq!(e1.client_message(), e2.client_message());
        assert!(!e1.client_message().contains("bob"));
        assert!(!e1.client_message().contains("JP"));
        assert!(!e1.client_message().contains("parents"));
    }
}

/// 参照先の母集合はテナント所有の全行（可視性を問わない）。Private 行も参照を満たす。
#[test]
fn parent_rows_count_regardless_of_session_visibility() {
    let (core, path) = new_core("fk-private-parent");
    let _guard = CleanupGuard(path);
    create_id_parent_and_child(&core);
    let alice_private =
        PolicyContext::with_visibilities("alice", [Visibility::Private]).expect("valid tenant");
    let alice_public =
        PolicyContext::with_visibilities("alice", [Visibility::Public]).expect("valid tenant");
    ok(
        &core,
        &alice_private,
        "INSERT INTO parents (id, name) VALUES (1, 'hidden') USING OPERATION_ID 'op-p'",
    );
    ok(
        &core,
        &alice_public,
        "INSERT INTO children (id, parent_id) VALUES (1, 1) USING OPERATION_ID 'op-c'",
    );
    // 逆に、参照元が Private でも参照先の削除は阻止される。
    assert_eq!(
        err_code(
            &core,
            &alice_public,
            "DELETE FROM parents WHERE id = 1 USING OPERATION_ID 'op-d'"
        ),
        "23503"
    );
}

#[test]
fn multi_row_insert_is_atomic_and_same_statement_parents_satisfy_references() {
    let (core, path) = new_core("fk-insert-batch");
    let _guard = CleanupGuard(path);
    create_id_parent_and_child(&core);
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO parents (id, name) VALUES (1, 'p1') USING OPERATION_ID 'op-p1'",
    );
    assert_eq!(
        err_code(
            &core,
            &alice,
            "INSERT INTO children (id, parent_id) VALUES (1, 1), (2, 999) USING OPERATION_ID 'op-b'"
        ),
        "23503"
    );
    assert_eq!(row_count(&core, &alice, "children"), 0);
    // 失敗した文は台帳にも残らない（同じ operation_id で訂正内容を送れる）。
    ok(
        &core,
        &alice,
        "INSERT INTO children (id, parent_id) VALUES (1, 1), (2, 1) USING OPERATION_ID 'op-b'",
    );
    assert_eq!(row_count(&core, &alice, "children"), 2);
}

#[test]
fn composite_foreign_key_with_null_component_is_not_checked() {
    let (core, path) = new_core("fk-composite");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    ok(
        &core,
        &sys,
        "CREATE TABLE p (a TEXT NOT NULL, b INTEGER NOT NULL, PRIMARY KEY (a, b))",
    );
    ok(
        &core,
        &sys,
        "CREATE TABLE c (x TEXT, y INTEGER, FOREIGN KEY (x, y) REFERENCES p)",
    );
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO p (id, a, b) VALUES (1, 'k', 7) USING OPERATION_ID 'op-p'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO c (id, x, y) VALUES (1, 'k', 7) USING OPERATION_ID 'op-c1'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO c (id, x) VALUES (2, 'no-such') USING OPERATION_ID 'op-c2'",
    );
    assert_eq!(
        err_code(
            &core,
            &alice,
            "INSERT INTO c (id, x, y) VALUES (3, 'k', 8) USING OPERATION_ID 'op-c3'"
        ),
        "23503"
    );
}

#[test]
fn update_and_upsert_of_referencing_columns_are_checked_without_side_effects() {
    let (core, path) = new_core("fk-update-child");
    let _guard = CleanupGuard(path);
    create_pk_parent_and_child(&core);
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
    assert_eq!(
        err_code(
            &core,
            &alice,
            "UPDATE cities SET country = 'ZZ' WHERE id = 1 USING OPERATION_ID 'op-u1'"
        ),
        "23503"
    );
    assert_eq!(
        err_code(
            &core,
            &alice,
            "UPDATE cities SET country = 'ZZ' WHERE name = 'Tokyo' USING OPERATION_ID 'op-u2'"
        ),
        "23503"
    );
    assert_eq!(
        err_code(
            &core,
            &alice,
            "INSERT INTO cities (id, country) VALUES (1, 'ZZ') \
             ON CONFLICT (id) DO UPDATE SET country = EXCLUDED.country USING OPERATION_ID 'op-u3'"
        ),
        "23503"
    );
    // 参照元列に触れない更新は通る（既存値は参照を満たしたまま）。
    ok(
        &core,
        &alice,
        "UPDATE cities SET name = 'Edo' WHERE id = 1 USING OPERATION_ID 'op-u4'",
    );
    // 副作用ゼロ: 参照先の削除は依然として阻止される（'JP' を参照したまま）。
    assert_eq!(
        err_code(
            &core,
            &alice,
            "DELETE FROM countries WHERE id = 1 USING OPERATION_ID 'op-d'"
        ),
        "23503"
    );
}

// --- 参照先側（DELETE／UPDATE／TRUNCATE）--------------------------------------

#[test]
fn deleting_referenced_rows_is_rejected_until_referencing_rows_are_gone() {
    let (core, path) = new_core("fk-delete-parent");
    let _guard = CleanupGuard(path);
    create_id_parent_and_child(&core);
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO parents (id, name) VALUES (1, 'p1'), (2, 'p2') USING OPERATION_ID 'op-p'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO children (id, parent_id) VALUES (1, 1) USING OPERATION_ID 'op-c'",
    );
    assert_eq!(
        err_code(
            &core,
            &alice,
            "DELETE FROM parents WHERE id = 1 USING OPERATION_ID 'op-d1'"
        ),
        "23503"
    );
    assert_eq!(
        err_code(
            &core,
            &alice,
            "DELETE FROM parents WHERE name = 'p1' USING OPERATION_ID 'op-d2'"
        ),
        "23503"
    );
    assert_eq!(
        err_code(
            &core,
            &alice,
            "TRUNCATE TABLE parents USING OPERATION_ID 'op-t1'"
        ),
        "23503"
    );
    assert_eq!(row_count(&core, &alice, "parents"), 2);
    // 参照されていない行は削除できる。
    ok(
        &core,
        &alice,
        "DELETE FROM parents WHERE id = 2 USING OPERATION_ID 'op-d3'",
    );
    ok(
        &core,
        &alice,
        "DELETE FROM children WHERE id = 1 USING OPERATION_ID 'op-d4'",
    );
    ok(
        &core,
        &alice,
        "TRUNCATE TABLE parents USING OPERATION_ID 'op-t2'",
    );
    assert_eq!(row_count(&core, &alice, "parents"), 0);
}

/// 参照整合性の判定はテナントごとに独立: alice の参照元行は bob の同じ `id` の
/// 参照先行の削除・TRUNCATE を阻止しない（他テナントの行は走査対象外）。
#[test]
fn other_tenant_referencing_rows_do_not_block_parent_changes() {
    let (core, path) = new_core("fk-tenant-isolation");
    let _guard = CleanupGuard(path);
    create_id_parent_and_child(&core);
    let alice = ctx("alice");
    let bob = ctx("bob");
    for (who, op) in [(&alice, "a"), (&bob, "b")] {
        ok(
            &core,
            who,
            &format!(
                "INSERT INTO parents (id, name) VALUES (1, 'p'), (2, 'q') USING OPERATION_ID 'op-p-{op}'"
            ),
        );
    }
    ok(
        &core,
        &alice,
        "INSERT INTO children (id, parent_id) VALUES (1, 1) USING OPERATION_ID 'op-c-a'",
    );
    ok(
        &core,
        &bob,
        "DELETE FROM parents WHERE id = 1 USING OPERATION_ID 'op-d-b'",
    );
    ok(
        &core,
        &bob,
        "TRUNCATE TABLE parents USING OPERATION_ID 'op-t-b'",
    );
    // alice 側は依然として阻止され、alice の参照先行は残っている。
    assert_eq!(
        err_code(
            &core,
            &alice,
            "DELETE FROM parents WHERE id = 1 USING OPERATION_ID 'op-d-a'"
        ),
        "23503"
    );
    assert_eq!(row_count(&core, &alice, "parents"), 2);
    // 他テナント所有の行を指定した DELETE は 0 行（FK 走査もしない）で成功する。
    ok(
        &core,
        &bob,
        "DELETE FROM parents WHERE id = 1 USING OPERATION_ID 'op-d-b2'",
    );
}

#[test]
fn updating_referenced_unique_key_is_rejected_but_non_key_columns_are_free() {
    let (core, path) = new_core("fk-update-parent");
    let _guard = CleanupGuard(path);
    create_pk_parent_and_child(&core);
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO countries (id, code, label) VALUES (1, 'JP', 'Japan'), (2, 'FR', 'France') \
         USING OPERATION_ID 'op-p'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO cities (id, country) VALUES (1, 'JP') USING OPERATION_ID 'op-c'",
    );
    assert_eq!(
        err_code(
            &core,
            &alice,
            "UPDATE countries SET code = 'XX' WHERE id = 1 USING OPERATION_ID 'op-u1'"
        ),
        "23503"
    );
    assert_eq!(
        err_code(
            &core,
            &alice,
            "UPDATE countries SET code = 'XX' WHERE label = 'Japan' USING OPERATION_ID 'op-u2'"
        ),
        "23503"
    );
    assert_eq!(
        err_code(
            &core,
            &alice,
            "INSERT INTO countries (id, code) VALUES (1, 'YY') \
             ON CONFLICT (id) DO UPDATE SET code = EXCLUDED.code USING OPERATION_ID 'op-u3'"
        ),
        "23503"
    );
    // 参照されていないキーの更新・非キー列の更新は通る。
    ok(
        &core,
        &alice,
        "UPDATE countries SET code = 'DE' WHERE id = 2 USING OPERATION_ID 'op-u4'",
    );
    ok(
        &core,
        &alice,
        "UPDATE countries SET label = 'Nippon' WHERE id = 1 USING OPERATION_ID 'op-u5'",
    );
}

// --- 自己参照 -----------------------------------------------------------

#[test]
fn self_referencing_foreign_key_is_checked_on_both_sides() {
    let (core, path) = new_core("fk-self");
    let _guard = CleanupGuard(path.clone());
    let sys = ctx("sys");
    ok(
        &core,
        &sys,
        "CREATE TABLE nodes (parent_id BIGINT REFERENCES nodes, label TEXT)",
    );
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO nodes (id, label) VALUES (1, 'root') USING OPERATION_ID 'op-1'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO nodes (id, parent_id) VALUES (2, 1) USING OPERATION_ID 'op-2'",
    );
    // 同一文内で書く行同士・自分自身への参照も満たされる（文単位の検査）。
    ok(
        &core,
        &alice,
        "INSERT INTO nodes (id, parent_id) VALUES (3, 4), (4, 4) USING OPERATION_ID 'op-3'",
    );
    assert_eq!(
        err_code(
            &core,
            &alice,
            "INSERT INTO nodes (id, parent_id) VALUES (5, 99) USING OPERATION_ID 'op-5'"
        ),
        "23503"
    );
    assert_eq!(
        err_code(
            &core,
            &alice,
            "DELETE FROM nodes WHERE id = 1 USING OPERATION_ID 'op-d1'"
        ),
        "23503"
    );
    // 自分自身だけを参照する行は削除できる。葉も削除できる。
    ok(
        &core,
        &alice,
        "DELETE FROM nodes WHERE id = 2 USING OPERATION_ID 'op-d2'",
    );
    ok(
        &core,
        &alice,
        "DELETE FROM nodes WHERE id = 1 USING OPERATION_ID 'op-d3'",
    );
    assert_eq!(
        err_code(
            &core,
            &alice,
            "DELETE FROM nodes WHERE id = 4 USING OPERATION_ID 'op-d4'"
        ),
        "23503"
    );
    ok(
        &core,
        &alice,
        "TRUNCATE TABLE nodes USING OPERATION_ID 'op-t'",
    );
    // 自己参照は DROP TABLE の依存に数えない。
    ok(&core, &sys, "DROP TABLE nodes");
}

// --- DDL 側の依存（DROP TABLE／DROP COLUMN）・永続化 --------------------------

#[test]
fn drop_table_of_referenced_table_is_rejected_with_2bp01() {
    let (core, path) = new_core("fk-drop-table");
    let _guard = CleanupGuard(path);
    create_id_parent_and_child(&core);
    let sys = ctx("sys");
    // データの有無を問わずカタログのみで判定する。
    assert_eq!(err_code(&core, &sys, "DROP TABLE parents"), "2BP01");
    ok(&core, &sys, "DROP TABLE children");
    ok(&core, &sys, "DROP TABLE parents");
    // 同名の再作成で旧 FK が引き継がれない。
    ok(
        &core,
        &sys,
        "CREATE TABLE children (parent_id BIGINT, note TEXT)",
    );
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO children (id, parent_id) VALUES (1, 999) USING OPERATION_ID 'op-c'",
    );
}

#[test]
fn drop_column_of_foreign_key_column_is_rejected_and_definitions_survive_reopen() {
    let (core, path) = new_core("fk-drop-column");
    let _guard = CleanupGuard(path.clone());
    create_id_parent_and_child(&core);
    drop(core);

    let storage = Storage::open(&path).expect("reopen storage");
    let err = storage
        .alter_table_drop_column("children", "parent_id")
        .expect_err("dropping a foreign key column must be rejected");
    assert!(matches!(err, CatalogError::DependentObjectsStillExist(_)));
    storage
        .alter_table_drop_column("children", "note")
        .expect("dropping an unrelated column must succeed");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let alice = ctx("alice");
    assert_eq!(
        err_code(
            &core,
            &alice,
            "INSERT INTO children (id, parent_id) VALUES (1, 5) USING OPERATION_ID 'op-c'"
        ),
        "23503"
    );
}

// --- 台帳照合の優先順位・ファイル形 INSERT・明示トランザクション -------------

/// 同一 `operation_id` の再送は、現在の内容が参照整合性に違反する状態でも台帳照合
/// （`23505`）が先に判定される（検査は台帳記録の後・commit の前）。
#[test]
fn resend_of_committed_operation_id_is_23505_not_23503() {
    let (core, path) = new_core("fk-ledger-order");
    let _guard = CleanupGuard(path);
    create_id_parent_and_child(&core);
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO parents (id, name) VALUES (1, 'p') USING OPERATION_ID 'op-p'",
    );
    let insert_child =
        "INSERT INTO children (id, parent_id) VALUES (1, 1) USING OPERATION_ID 'op-c'";
    ok(&core, &alice, insert_child);
    ok(
        &core,
        &alice,
        "DELETE FROM children WHERE id = 1 USING OPERATION_ID 'op-dc'",
    );
    ok(
        &core,
        &alice,
        "DELETE FROM parents WHERE id = 1 USING OPERATION_ID 'op-dp'",
    );
    assert_eq!(err_code(&core, &alice, insert_child), "23505");
}

#[test]
fn file_form_insert_replacing_referenced_chunk_rows_is_rejected_atomically() {
    let path = unique_db_path("fk-file-replace");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(2).expect("valid dim")));
    let sys = ctx("sys");
    ok(
        &core,
        &sys,
        "CREATE TABLE files (embedding VECTOR(2), path TEXT, body TEXT)",
    );
    ok(
        &core,
        &sys,
        "CREATE TABLE refs (chunk_id BIGINT REFERENCES files)",
    );
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO files (path, body) VALUES ('a.txt', 'first') USING OPERATION_ID 'op-f1'",
    );
    // 空のテーブルへの最初のファイル形 INSERT は id 0 から採番される。
    ok(
        &core,
        &alice,
        "INSERT INTO refs (id, chunk_id) VALUES (1, 0) USING OPERATION_ID 'op-r'",
    );
    assert_eq!(
        err_code(
            &core,
            &alice,
            "INSERT INTO files (path, body) VALUES ('a.txt', 'second') USING OPERATION_ID 'op-f2'"
        ),
        "23503"
    );
    ok(
        &core,
        &alice,
        "DELETE FROM refs WHERE id = 1 USING OPERATION_ID 'op-dr'",
    );
    // 拒否された置換は台帳に残らないため、同じ operation_id で再実行できる。
    ok(
        &core,
        &alice,
        "INSERT INTO files (path, body) VALUES ('a.txt', 'second') USING OPERATION_ID 'op-f2'",
    );
}

#[test]
fn explicit_transaction_sees_its_own_parent_rows_and_rejects_violations() {
    let (core, path) = new_core("fk-explicit-txn");
    let _guard = CleanupGuard(path);
    create_id_parent_and_child(&core);
    let alice = ctx("alice");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    for sql in [
        "BEGIN",
        "INSERT INTO parents (id, name) VALUES (1, 'p') USING OPERATION_ID 'op-p'",
        "INSERT INTO children (id, parent_id) VALUES (1, 1) USING OPERATION_ID 'op-c'",
        "COMMIT",
    ] {
        core.execute_sql_in_txn(&alice, &mut session, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
    }
    assert_eq!(row_count(&core, &alice, "children"), 1);

    core.execute_sql_in_txn(&alice, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(
            &alice,
            &mut session,
            &mut txn,
            "TRUNCATE TABLE parents USING OPERATION_ID 'op-t'",
        )
        .expect_err("truncating a referenced table inside a transaction must fail");
    assert_eq!(err.wire_code(), "23503");
    core.execute_sql_in_txn(&alice, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");

    core.execute_sql_in_txn(&alice, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(
            &alice,
            &mut session,
            &mut txn,
            "INSERT INTO children (id, parent_id) VALUES (2, 42) USING OPERATION_ID 'op-c2'",
        )
        .expect_err("a dangling reference inside a transaction must fail");
    assert_eq!(err.wire_code(), "23503");
    core.execute_sql_in_txn(&alice, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(row_count(&core, &alice, "parents"), 1);
    assert_eq!(row_count(&core, &alice, "children"), 1);
}

// --- MATCH FULL・DEFERRABLE（TABLE-17・TASK-205、Issue #1077）---------------

use engine::catalog::{ForeignKeyDeferrability, ForeignKeyMatch};
use engine::sql::transaction::TransactionStatus;

/// `MATCH FULL` の複合 FK 宣言（R2）。NULL 混在の組は違反、全 NULL・全非 NULL は
/// `MATCH SIMPLE` と同じ扱い。単一列は挙動が変わらない。
#[test]
fn match_full_rejects_mixed_null_and_accepts_all_null_or_all_present() {
    let (core, path) = new_core("fk-match-full");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    ok(
        &core,
        &sys,
        "CREATE TABLE p (a TEXT NOT NULL, b INTEGER NOT NULL, PRIMARY KEY (a, b))",
    );
    ok(
        &core,
        &sys,
        "CREATE TABLE c (x TEXT, y INTEGER, FOREIGN KEY (x, y) REFERENCES p MATCH FULL)",
    );
    ok(
        &core,
        &sys,
        "CREATE TABLE c_simple (x TEXT, y INTEGER, FOREIGN KEY (x, y) REFERENCES p)",
    );
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO p (id, a, b) VALUES (1, 'k', 7) USING OPERATION_ID 'op-p'",
    );
    // 全 NULL は検査対象外。
    ok(
        &core,
        &alice,
        "INSERT INTO c (id) VALUES (1) USING OPERATION_ID 'op-c1'",
    );
    // 全て非 NULL かつ参照先が存在すれば受理。
    ok(
        &core,
        &alice,
        "INSERT INTO c (id, x, y) VALUES (2, 'k', 7) USING OPERATION_ID 'op-c2'",
    );
    // 全て非 NULL だが参照先が無ければ違反（MATCH SIMPLE と同じ）。
    assert_eq!(
        err_code(
            &core,
            &alice,
            "INSERT INTO c (id, x, y) VALUES (3, 'k', 8) USING OPERATION_ID 'op-c3'"
        ),
        "23503"
    );
    // NULL 混在は MATCH FULL では違反（`y` を省略＝NULL、`x` は非 NULL）。
    assert_eq!(
        err_code(
            &core,
            &alice,
            "INSERT INTO c (id, x) VALUES (4, 'no-such') USING OPERATION_ID 'op-c4'"
        ),
        "23503"
    );
    assert_eq!(
        err_code(
            &core,
            &alice,
            "INSERT INTO c (id, y) VALUES (5, 7) USING OPERATION_ID 'op-c5'"
        ),
        "23503"
    );
    // 対照: 同じ NULL 混在の組は MATCH SIMPLE（既定）なら受理される。
    ok(
        &core,
        &alice,
        "INSERT INTO c_simple (id, x) VALUES (1, 'no-such') USING OPERATION_ID 'op-cs1'",
    );
    assert_eq!(row_count(&core, &alice, "c"), 2);
}

/// 単一列の `MATCH FULL` は `MATCH SIMPLE` と同じ挙動になる（NULL は 0 個か
/// 全部＝1 個のいずれかしかあり得ないため）。
#[test]
fn match_full_on_single_column_behaves_like_match_simple() {
    let (core, path) = new_core("fk-match-full-single");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    ok(&core, &sys, "CREATE TABLE p (name TEXT)");
    ok(
        &core,
        &sys,
        "CREATE TABLE c (parent_id BIGINT REFERENCES p MATCH FULL)",
    );
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO p (id, name) VALUES (1, 'p') USING OPERATION_ID 'op-p'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO c (id) VALUES (1) USING OPERATION_ID 'op-c1'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO c (id, parent_id) VALUES (2, 1) USING OPERATION_ID 'op-c2'",
    );
}

/// 宣言した `MATCH`・遅延属性がカタログへ永続化され、再オープン後も残る
/// （TABLE-17・TASK-205、Issue #1077）。
#[test]
fn foreign_key_options_persist_across_reopen() {
    let (core, path) = new_core("fk-options-persist");
    let _guard = CleanupGuard(path.clone());
    let sys = ctx("sys");
    ok(&core, &sys, "CREATE TABLE p (name TEXT)");
    ok(
        &core,
        &sys,
        "CREATE TABLE c1 (parent_id BIGINT REFERENCES p MATCH FULL DEFERRABLE INITIALLY DEFERRED)",
    );
    ok(
        &core,
        &sys,
        "CREATE TABLE c2 (parent_id BIGINT REFERENCES p DEFERRABLE)",
    );
    ok(
        &core,
        &sys,
        "CREATE TABLE c3 (parent_id BIGINT REFERENCES p INITIALLY DEFERRED)",
    );
    ok(
        &core,
        &sys,
        "CREATE TABLE c4 (parent_id BIGINT REFERENCES p INITIALLY IMMEDIATE DEFERRABLE)",
    );
    ok(
        &core,
        &sys,
        "CREATE TABLE c5 (parent_id BIGINT REFERENCES p NOT DEFERRABLE)",
    );
    drop(core);

    let storage = Storage::open(&path).expect("reopen storage");
    let assert_options =
        |table: &str, match_type: ForeignKeyMatch, deferrability: ForeignKeyDeferrability| {
            let schema = storage.get_table_schema(table).expect(table);
            let [fk] = schema.foreign_keys() else {
                panic!("{table} must declare exactly one foreign key");
            };
            assert_eq!(fk.match_type(), match_type, "{table}");
            assert_eq!(fk.deferrability(), deferrability, "{table}");
        };
    assert_options(
        "c1",
        ForeignKeyMatch::Full,
        ForeignKeyDeferrability::DeferrableInitiallyDeferred,
    );
    assert_options(
        "c2",
        ForeignKeyMatch::Simple,
        ForeignKeyDeferrability::DeferrableInitiallyImmediate,
    );
    // `INITIALLY DEFERRED` 単独は `DEFERRABLE` を含意する。
    assert_options(
        "c3",
        ForeignKeyMatch::Simple,
        ForeignKeyDeferrability::DeferrableInitiallyDeferred,
    );
    assert_options(
        "c4",
        ForeignKeyMatch::Simple,
        ForeignKeyDeferrability::DeferrableInitiallyImmediate,
    );
    assert_options(
        "c5",
        ForeignKeyMatch::Simple,
        ForeignKeyDeferrability::NotDeferrable,
    );
}

/// `INITIALLY DEFERRED` の FK は autocommit では文単位で検査される（R1 の
/// 「遅延は検査しないことを意味しない」不変条件。TABLE-17・TASK-205、
/// Issue #1077）。
#[test]
fn autocommit_still_checks_deferred_foreign_key_per_statement() {
    let (core, path) = new_core("fk-deferred-autocommit");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    ok(&core, &sys, "CREATE TABLE p (name TEXT)");
    ok(
        &core,
        &sys,
        "CREATE TABLE c (parent_id BIGINT REFERENCES p DEFERRABLE INITIALLY DEFERRED)",
    );
    let alice = ctx("alice");
    assert_eq!(
        err_code(
            &core,
            &alice,
            "INSERT INTO c (id, parent_id) VALUES (1, 999) USING OPERATION_ID 'op-c1'"
        ),
        "23503"
    );
    ok(
        &core,
        &alice,
        "INSERT INTO p (id, name) VALUES (1, 'p') USING OPERATION_ID 'op-p'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO c (id, parent_id) VALUES (2, 1) USING OPERATION_ID 'op-c2'",
    );
    assert_eq!(
        err_code(
            &core,
            &alice,
            "DELETE FROM p WHERE id = 1 USING OPERATION_ID 'op-d'"
        ),
        "23503"
    );
}

/// `DEFERRABLE INITIALLY IMMEDIATE`（`DEFERRABLE` 単独）は文単位で検査される
/// （`SET CONSTRAINTS` 非対応のため実行時に遅延へ切り替える経路がない。
/// TABLE-17・TASK-205、Issue #1077）。
#[test]
fn deferrable_initially_immediate_is_checked_per_statement_inside_explicit_transaction() {
    let (core, path) = new_core("fk-deferrable-immediate");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    ok(&core, &sys, "CREATE TABLE p (name TEXT)");
    ok(
        &core,
        &sys,
        "CREATE TABLE c (parent_id BIGINT REFERENCES p DEFERRABLE)",
    );
    let alice = ctx("alice");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&alice, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(
            &alice,
            &mut session,
            &mut txn,
            "INSERT INTO c (id, parent_id) VALUES (1, 999) USING OPERATION_ID 'op-c1'",
        )
        .expect_err("DEFERRABLE INITIALLY IMMEDIATE must still check per statement");
    assert_eq!(err.wire_code(), "23503");
}

/// R1 の中核シナリオ: `INITIALLY DEFERRED` の FK は明示トランザクション中の
/// 文単位検査を省き、COMMIT 時にまとめて検査する。子→親の順で書いても
/// COMMIT が成功する（自トランザクション内の未 commit 書き込みが母集合に
/// 含まれる。TABLE-17・TASK-205、Issue #1077）。
#[test]
fn deferred_foreign_key_check_is_postponed_to_commit_and_sees_same_transaction_writes() {
    let (core, path) = new_core("fk-deferred-commit-ok");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    ok(&core, &sys, "CREATE TABLE p (name TEXT)");
    ok(
        &core,
        &sys,
        "CREATE TABLE c (parent_id BIGINT REFERENCES p DEFERRABLE INITIALLY DEFERRED)",
    );
    let alice = ctx("alice");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    for sql in [
        "BEGIN",
        // 子を先に書いても、文単位検査が遅延されているため成功する。
        "INSERT INTO c (id, parent_id) VALUES (1, 1) USING OPERATION_ID 'op-c'",
        "INSERT INTO p (id, name) VALUES (1, 'p') USING OPERATION_ID 'op-p'",
        "COMMIT",
    ] {
        core.execute_sql_in_txn(&alice, &mut session, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
    }
    assert_eq!(txn.status(), TransactionStatus::Idle);
    assert_eq!(row_count(&core, &alice, "p"), 1);
    assert_eq!(row_count(&core, &alice, "c"), 1);
}

/// R1: COMMIT 時に遅延 FK が違反すれば、同一トランザクションで書いた無関係の
/// 行（親テーブル自身の行を含む）ごとトランザクション全体が破棄される
/// （TABLE-17・TASK-205、Issue #1077）。COMMIT 後の状態は `Idle` になり
/// （PostgreSQL と同じくロールバック扱い）、続く `ROLLBACK` は `25P01`。
/// セッション状態も `BEGIN` 時点へ復元される。
#[test]
fn deferred_foreign_key_violation_at_commit_discards_entire_transaction() {
    let (core, path) = new_core("fk-deferred-commit-violation");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    ok(&core, &sys, "CREATE TABLE p (name TEXT)");
    ok(&core, &sys, "CREATE TABLE unrelated (label TEXT)");
    ok(
        &core,
        &sys,
        "CREATE TABLE c (parent_id BIGINT REFERENCES p DEFERRABLE INITIALLY DEFERRED)",
    );
    let alice = ctx("alice");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&alice, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(
        &alice,
        &mut session,
        &mut txn,
        "INSERT INTO unrelated (id, label) VALUES (1, 'x') USING OPERATION_ID 'op-u'",
    )
    .expect("insert unrelated must succeed inside the transaction");
    core.execute_sql_in_txn(
        &alice,
        &mut session,
        &mut txn,
        // 親が存在しない（文単位検査は遅延されているため、この時点では成功する）。
        "INSERT INTO c (id, parent_id) VALUES (1, 999) USING OPERATION_ID 'op-c'",
    )
    .expect("insert with a dangling deferred reference must succeed at statement time");
    let err = core
        .execute_sql_in_txn(&alice, &mut session, &mut txn, "COMMIT")
        .expect_err("COMMIT must fail when a deferred foreign key is still violated");
    assert_eq!(err.wire_code(), "23503");
    assert_eq!(txn.status(), TransactionStatus::Idle);
    let err = core
        .execute_sql_in_txn(&alice, &mut session, &mut txn, "ROLLBACK")
        .expect_err("ROLLBACK after an already-resolved COMMIT must be 25P01");
    assert_eq!(err.wire_code(), "25P01");
    assert_eq!(row_count(&core, &alice, "c"), 0);
    assert_eq!(row_count(&core, &alice, "unrelated"), 0);
}

/// R1: 親を `TRUNCATE` してから同じキーで再挿入した場合、COMMIT 時点の事後
/// 状態だけを見るため成功する。`TRUNCATE` だけで終えた場合は COMMIT が違反に
/// なり、親の行は残る（TABLE-17・TASK-205、Issue #1077）。
#[test]
fn deferred_foreign_key_checks_post_commit_state_around_truncate() {
    let (core, path) = new_core("fk-deferred-truncate");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    ok(&core, &sys, "CREATE TABLE p (name TEXT)");
    ok(
        &core,
        &sys,
        "CREATE TABLE c (parent_id BIGINT REFERENCES p DEFERRABLE INITIALLY DEFERRED)",
    );
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO p (id, name) VALUES (1, 'p') USING OPERATION_ID 'op-p0'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO c (id, parent_id) VALUES (1, 1) USING OPERATION_ID 'op-c0'",
    );

    // シナリオ 1: TRUNCATE してから同じキーで再挿入 → COMMIT 成功。
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    for sql in [
        "BEGIN",
        "TRUNCATE TABLE p USING OPERATION_ID 'op-t1'",
        "INSERT INTO p (id, name) VALUES (1, 'p2') USING OPERATION_ID 'op-p1'",
        "COMMIT",
    ] {
        core.execute_sql_in_txn(&alice, &mut session, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
    }
    assert_eq!(row_count(&core, &alice, "c"), 1);

    // シナリオ 2: TRUNCATE のみ → COMMIT が違反、親行は残る。
    core.execute_sql_in_txn(&alice, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(
        &alice,
        &mut session,
        &mut txn,
        "TRUNCATE TABLE p USING OPERATION_ID 'op-t2'",
    )
    .expect("truncate must succeed at statement time (check is deferred)");
    let err = core
        .execute_sql_in_txn(&alice, &mut session, &mut txn, "COMMIT")
        .expect_err("COMMIT must fail: the deferred child row would dangle");
    assert_eq!(err.wire_code(), "23503");
    assert_eq!(row_count(&core, &alice, "p"), 1);
}

/// 自己参照の遅延 FK は、後から挿入される行を先に参照しても COMMIT 時点の
/// 事後状態で成立していれば成功する（TABLE-17・TASK-205、Issue #1077）。
#[test]
fn self_referencing_deferred_foreign_key_is_checked_at_commit() {
    let (core, path) = new_core("fk-deferred-self");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    ok(
        &core,
        &sys,
        "CREATE TABLE nodes (parent_id BIGINT REFERENCES nodes DEFERRABLE INITIALLY DEFERRED)",
    );
    let alice = ctx("alice");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    for sql in [
        "BEGIN",
        // 2 が 3 を先に参照するが、3 は後の文で挿入される。
        "INSERT INTO nodes (id, parent_id) VALUES (2, 3) USING OPERATION_ID 'op-n2'",
        "INSERT INTO nodes (id, parent_id) VALUES (3, 2) USING OPERATION_ID 'op-n3'",
        "COMMIT",
    ] {
        core.execute_sql_in_txn(&alice, &mut session, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
    }
    assert_eq!(row_count(&core, &alice, "nodes"), 2);

    // 参照先が最後まで現れない場合は COMMIT が違反になる。
    core.execute_sql_in_txn(&alice, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(
        &alice,
        &mut session,
        &mut txn,
        "INSERT INTO nodes (id, parent_id) VALUES (10, 999) USING OPERATION_ID 'op-n10'",
    )
    .expect("insert with a dangling deferred self-reference must succeed at statement time");
    let err = core
        .execute_sql_in_txn(&alice, &mut session, &mut txn, "COMMIT")
        .expect_err("COMMIT must fail: the self-reference never resolves");
    assert_eq!(err.wire_code(), "23503");
    assert_eq!(row_count(&core, &alice, "nodes"), 2);
}

/// `MATCH FULL` と `DEFERRABLE INITIALLY DEFERRED` の併用: COMMIT 時点の事後
/// 状態で NULL 混在が残っていれば違反になる（TABLE-17・TASK-205、Issue #1077）。
#[test]
fn deferred_foreign_key_with_match_full_checks_null_mixing_at_commit() {
    let (core, path) = new_core("fk-deferred-match-full");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    ok(
        &core,
        &sys,
        "CREATE TABLE p (a TEXT NOT NULL, b INTEGER NOT NULL, PRIMARY KEY (a, b))",
    );
    ok(
        &core,
        &sys,
        "CREATE TABLE c (x TEXT, y INTEGER, \
         FOREIGN KEY (x, y) REFERENCES p MATCH FULL DEFERRABLE INITIALLY DEFERRED)",
    );
    let alice = ctx("alice");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&alice, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(
        &alice,
        &mut session,
        &mut txn,
        "INSERT INTO c (id, x) VALUES (1, 'k') USING OPERATION_ID 'op-c'",
    )
    .expect("NULL-mixed deferred insert must succeed at statement time");
    let err = core
        .execute_sql_in_txn(&alice, &mut session, &mut txn, "COMMIT")
        .expect_err("COMMIT must fail: MATCH FULL rejects the NULL-mixed tuple");
    assert_eq!(err.wire_code(), "23503");
}

/// RLS: 他テナントにだけ存在する親を参照した遅延 FK の COMMIT 違反は、親が
/// 存在しない場合と `wire_code` が同一になる（RLS-9・RLS-10 (c)）。他テナントの
/// 子行は自テナントの `TRUNCATE`・COMMIT を妨げない。
#[test]
fn deferred_foreign_key_commit_violation_does_not_reveal_other_tenant_rows() {
    let (core, path) = new_core("fk-deferred-rls");
    let _guard = CleanupGuard(path);
    let sys = ctx("sys");
    ok(&core, &sys, "CREATE TABLE p (name TEXT)");
    ok(
        &core,
        &sys,
        "CREATE TABLE c (parent_id BIGINT REFERENCES p DEFERRABLE INITIALLY DEFERRED)",
    );
    let bob = ctx("bob");
    ok(
        &core,
        &bob,
        "INSERT INTO p (id, name) VALUES (1, 'bob-p') USING OPERATION_ID 'op-bob-p'",
    );

    let alice = ctx("alice");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&alice, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(
        &alice,
        &mut session,
        &mut txn,
        "INSERT INTO c (id, parent_id) VALUES (1, 1) USING OPERATION_ID 'op-c'",
    )
    .expect("insert referencing only another tenant's row must succeed at statement time");
    let err = core
        .execute_sql_in_txn(&alice, &mut session, &mut txn, "COMMIT")
        .expect_err("COMMIT must fail: id 1 does not exist in alice's own rows");
    assert_eq!(err.wire_code(), "23503");

    // alice が自テナント内で有効な親子を確定させる（同じ id 1 を alice の
    // テナント名前空間で使う。TABLE-12: 物理キーはテナントで名前空間化される
    // ため bob の id 1 とは衝突しない）。
    core.execute_sql_in_txn(&alice, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    for sql in [
        "INSERT INTO p (id, name) VALUES (1, 'alice-p') USING OPERATION_ID 'op-alice-p'",
        "INSERT INTO c (id, parent_id) VALUES (2, 1) USING OPERATION_ID 'op-alice-c'",
        "COMMIT",
    ] {
        core.execute_sql_in_txn(&alice, &mut session, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
    }

    // bob の `TRUNCATE TABLE p`・COMMIT は、alice が今テナント境界を越えて
    // bob の id 1 を（もう存在しないダングリング参照として）指す子行
    // （`c.id=1`）を残していても成功する——COMMIT 時の逆引き走査が bob
    // 自身のテナント範囲に閉じており、alice の行を「参照元」として拾わない
    // ことを固定する（RLS-9・RLS-10 (c)）。bob には子行が無いため、この
    // 検査を素通りしても成功して当然にならないよう、bob 自身の子テーブルへの
    // 書き込みは一切行わない。
    let mut bob_session = SessionState::default();
    let mut bob_txn = core.new_session_transaction();
    for sql in [
        "BEGIN",
        "TRUNCATE TABLE p USING OPERATION_ID 'op-bob-t'",
        "COMMIT",
    ] {
        core.execute_sql_in_txn(&bob, &mut bob_session, &mut bob_txn, sql)
            .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
    }
    // alice 自身の（有効な）参照は残ったまま（bob の TRUNCATE の影響を受けない）。
    assert_eq!(row_count(&core, &alice, "c"), 1);
}

/// P0 回帰: `MATCH FULL`／`DEFERRABLE` 宣言を持つ FK（カタログ v9）は、
/// 通常の（オプション既定の）FK と同じく `DELETE`／`TRUNCATE`（`23503`）・
/// `DROP TABLE`（`2BP01`）・子の `DROP COLUMN`（依存エラー）の逆引き検査が効く
/// （`catalog::referencing_foreign_keys_in_txn` の v9 対応。TABLE-17・TASK-205、
/// Issue #1077）。v9 をこの逆引きが見落とすと、これらすべてが fail-open になる。
#[test]
fn v9_catalog_children_are_covered_by_reverse_lookup_checks() {
    let (core, path) = new_core("fk-v9-reverse-lookup");
    let _guard = CleanupGuard(path.clone());
    let sys = ctx("sys");
    ok(&core, &sys, "CREATE TABLE p (name TEXT)");
    ok(
        &core,
        &sys,
        "CREATE TABLE c (parent_id BIGINT REFERENCES p MATCH FULL DEFERRABLE)",
    );
    let alice = ctx("alice");
    ok(
        &core,
        &alice,
        "INSERT INTO p (id, name) VALUES (1, 'p') USING OPERATION_ID 'op-p'",
    );
    ok(
        &core,
        &alice,
        "INSERT INTO c (id, parent_id) VALUES (1, 1) USING OPERATION_ID 'op-c'",
    );
    assert_eq!(
        err_code(
            &core,
            &alice,
            "DELETE FROM p WHERE id = 1 USING OPERATION_ID 'op-d'"
        ),
        "23503"
    );
    assert_eq!(
        err_code(&core, &alice, "TRUNCATE TABLE p USING OPERATION_ID 'op-t'"),
        "23503"
    );
    assert_eq!(err_code(&core, &sys, "DROP TABLE p"), "2BP01");
    drop(core);

    // `DROP COLUMN` は SQL 表層に無く `Storage` API 専用
    // （`drop_column_of_foreign_key_column_is_rejected_and_definitions_survive_reopen`
    // と同じ流儀）。
    let storage = Storage::open(&path).expect("reopen storage");
    let err = storage
        .alter_table_drop_column("c", "parent_id")
        .expect_err("dropping a v9 foreign key column must be rejected");
    assert!(matches!(err, CatalogError::DependentObjectsStillExist(_)));
}
