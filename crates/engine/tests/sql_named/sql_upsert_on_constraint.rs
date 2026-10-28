//! `INSERT ... ON CONFLICT ON CONSTRAINT <名前>`（SQL-49、Issue #1737）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/` SQL-49・INDEX-9 (3)・TABLE-16・RLS-9・ERR-6。
//!
//! `sql_unique_index.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `EngineCore::execute_sql_in_session_legacy` を production 経路として検証）。検証する契約
//! （設計は `docs/design/sql-upsert.md` の Issue #1737 追記）:
//!
//! - UNIQUE 制約名・PRIMARY KEY（明示名・導出名）を `DO NOTHING`／`DO UPDATE` で受理する
//! - 不在の名前は `42704`、CHECK・FOREIGN KEY の名前は `42809`、
//!   `CREATE UNIQUE INDEX` 由来の名前は `42704`。いずれも副作用を残さない
//! - 同じ制約の列リスト形と `RETURNING` の列・行が一致し、再送は台帳重複として扱われる

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
// 疑似列 `id` を持つ従来方式の表で検証する（TABLE-23・Issue #2199）。
use crate::legacy_table;
use legacy_table::LegacyExec as _;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn exec(
    core: &EngineCore,
    s: &mut SessionState,
    tenant: &str,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session_legacy(&ctx(tenant), s, sql)
}

fn code(core: &EngineCore, s: &mut SessionState, tenant: &str, sql: &str) -> &'static str {
    exec(core, s, tenant, sql).expect_err(sql).wire_code()
}

/// DDL を流した core を返す。
fn setup(label: &str, ddl: &[&str]) -> (EngineCore, std::path::PathBuf, SessionState) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut s = SessionState::default();
    s.allow_ddl();
    for sql in ddl {
        exec(&core, &mut s, "owner", sql).expect(sql);
    }
    (core, path, s)
}

fn seed(core: &EngineCore, s: &mut SessionState) {
    exec(
        core,
        s,
        "owner",
        "INSERT INTO docs (id, a, b) VALUES (1, 'x', 'b0') USING OPERATION_ID 'seed-1'",
    )
    .expect("seed");
}

fn rows(core: &EngineCore, s: &mut SessionState, table: &str) -> String {
    format!(
        "{:?}",
        exec(core, s, "owner", &format!("SELECT * FROM {table} LIMIT 50")).expect("select")
    )
}

const UNIQ: &[&str] = &["CREATE TABLE docs (a TEXT, b TEXT, CONSTRAINT uq_a UNIQUE (a))"];

#[test]
fn unique_constraint_name_do_nothing_and_do_update() {
    let (core, path, mut s) = setup("onc-unique", UNIQ);
    let _g = CleanupGuard(path);
    seed(&core, &mut s);
    // 衝突なし: 挿入される。
    exec(
        &core,
        &mut s,
        "owner",
        "INSERT INTO docs (id, a, b) VALUES (2, 'y', 'b1') ON CONFLICT ON CONSTRAINT uq_a \
         DO NOTHING USING OPERATION_ID 'op-1'",
    )
    .expect("no conflict");
    assert!(rows(&core, &mut s, "docs").contains("b1"));
    // 衝突あり DO NOTHING: 変更なし。
    let before = rows(&core, &mut s, "docs");
    exec(
        &core,
        &mut s,
        "owner",
        "INSERT INTO docs (id, a, b) VALUES (3, 'x', 'zz') ON CONFLICT ON CONSTRAINT uq_a \
         DO NOTHING USING OPERATION_ID 'op-2'",
    )
    .expect("conflict do nothing");
    assert_eq!(before, rows(&core, &mut s, "docs"));
    // 衝突あり DO UPDATE: 既存行が更新される。
    exec(
        &core,
        &mut s,
        "owner",
        "INSERT INTO docs (id, a, b) VALUES (4, 'x', 'b9') ON CONFLICT ON CONSTRAINT uq_a \
         DO UPDATE SET b = EXCLUDED.b USING OPERATION_ID 'op-3'",
    )
    .expect("conflict do update");
    let after = rows(&core, &mut s, "docs");
    assert!(after.contains("b9"), "{after}");
    assert!(!after.contains("b0"), "{after}");
}

#[test]
fn added_unique_constraint_and_primary_key_names_resolve() {
    let (core, path, mut s) = setup(
        "onc-pk",
        &[
            "CREATE TABLE docs (a TEXT, b TEXT, CONSTRAINT pk_ab PRIMARY KEY (a, b))",
            "CREATE TABLE plain (a TEXT PRIMARY KEY, b TEXT)",
            "ALTER TABLE plain ADD CONSTRAINT uq_b UNIQUE (b)",
        ],
    );
    let _g = CleanupGuard(path);
    for (i, sql) in [
        "INSERT INTO docs (id, a, b) VALUES (1, 'x', 'y') ON CONFLICT ON CONSTRAINT pk_ab \
         DO NOTHING USING OPERATION_ID 'pk-1'",
        "INSERT INTO plain (id, a, b) VALUES (1, 'x', 'y') ON CONFLICT ON CONSTRAINT plain_pkey \
         DO NOTHING USING OPERATION_ID 'pk-2'",
        "INSERT INTO plain (id, a, b) VALUES (2, 'q', 'y') ON CONFLICT ON CONSTRAINT uq_b \
         DO UPDATE SET a = EXCLUDED.a USING OPERATION_ID 'pk-3'",
    ]
    .iter()
    .enumerate()
    {
        exec(&core, &mut s, "owner", sql).unwrap_or_else(|e| panic!("{i}: {e:?}"));
    }
    let plain = rows(&core, &mut s, "plain");
    assert!(plain.contains('q'), "{plain}");
}

#[test]
fn row_key_has_no_constraint_name_on_legacy_tables() {
    // 疑似列 `id` だけが行キーの従来方式の表は PRIMARY KEY 制約を持たないため、導出名
    // `<table>_pkey` も解決しない（fail-closed の `42704`）。行キー衝突は `(id)` 形で書く。
    let (core, path, mut s) = setup("onc-rowid", &["CREATE TABLE docs (a TEXT, b TEXT)"]);
    let _g = CleanupGuard(path);
    seed(&core, &mut s);
    let sql = "INSERT INTO docs (id, a, b) VALUES (1, 'n', 'n') ON CONFLICT ON CONSTRAINT \
               docs_pkey DO NOTHING USING OPERATION_ID 'rid-1'";
    assert_eq!(code(&core, &mut s, "owner", sql), "42704");
}

#[test]
fn undefined_and_wrong_kind_names_are_rejected_without_side_effects() {
    let (core, path, mut s) = setup(
        "onc-errors",
        &[
            "CREATE TABLE parent (a TEXT UNIQUE)",
            "CREATE TABLE docs (a TEXT, b TEXT, n INT, CONSTRAINT uq_a UNIQUE (a), \
             CONSTRAINT ck_n CHECK (n > 0), CONSTRAINT fk_b FOREIGN KEY (b) REFERENCES parent (a))",
            "CREATE TABLE idx (a TEXT)",
            "CREATE UNIQUE INDEX idx_a ON idx (a)",
        ],
    );
    let _g = CleanupGuard(path);
    let ins = |table: &str, name: &str, op: &str| {
        format!(
            "INSERT INTO {table} (id, a) VALUES (9, 'z') ON CONFLICT ON CONSTRAINT {name} \
             DO NOTHING USING OPERATION_ID '{op}'"
        )
    };
    assert_eq!(
        code(&core, &mut s, "owner", &ins("docs", "nope", "e-1")),
        "42704"
    );
    // 大文字小文字は区別する。
    assert_eq!(
        code(&core, &mut s, "owner", &ins("docs", "UQ_A", "e-2")),
        "42704"
    );
    assert_eq!(
        code(&core, &mut s, "owner", &ins("docs", "ck_n", "e-3")),
        "42809"
    );
    assert_eq!(
        code(&core, &mut s, "owner", &ins("docs", "fk_b", "e-4")),
        "42809"
    );
    // `CREATE UNIQUE INDEX` 由来の名前は制約扱いしない（INDEX-9 (3)）。
    assert_eq!(
        code(&core, &mut s, "owner", &ins("idx", "idx_a", "e-5")),
        "42704"
    );
    // 副作用なし（行も台帳も残らない）。拒否された operation_id で正しい文が通る。
    assert!(rows(&core, &mut s, "idx").contains("rows: []"));
    exec(&core, &mut s, "owner", &ins("docs", "uq_a", "e-3"))
        .expect("operation id was not consumed by the rejected statement");
}

#[test]
fn view_target_is_rejected_and_insert_select_is_accepted_with_id() {
    let (core, path, mut s) = setup(
        "onc-view",
        &[
            "CREATE TABLE docs (a TEXT, b TEXT, CONSTRAINT uq_a UNIQUE (a))",
            "CREATE VIEW v AS SELECT id, a, b FROM docs",
        ],
    );
    let _g = CleanupGuard(path);
    let view = "INSERT INTO v (id, a, b) VALUES (1, 'x', 'y') ON CONFLICT ON CONSTRAINT uq_a \
                DO NOTHING USING OPERATION_ID 'v-1'";
    assert_eq!(code(&core, &mut s, "owner", view), "42704");
    // INSERT ... SELECT との併用は受理される（SQL-51・Issue #2614）。疑似列 `id` を省略した
    // 列リストは従来方式の表では `id` 必須のため fail-closed のまま。
    let sel = "INSERT INTO docs (id, a, b) SELECT id, a, b FROM docs ORDER BY id LIMIT 10 \
               ON CONFLICT ON CONSTRAINT uq_a DO NOTHING USING OPERATION_ID 'v-2'";
    exec(&core, &mut s, "owner", sel).expect("insert select with on conflict");
    let sel_no_id = "INSERT INTO docs (a, b) SELECT a, b FROM docs ORDER BY id LIMIT 10 \
                     ON CONFLICT ON CONSTRAINT uq_a DO NOTHING USING OPERATION_ID 'v-3'";
    assert_eq!(code(&core, &mut s, "owner", sel_no_id), "22000");
}

#[test]
fn returning_matches_the_column_list_form() {
    for action in ["DO NOTHING", "DO UPDATE SET b = EXCLUDED.b"] {
        for (id, a) in [(2, "x"), (2, "fresh")] {
            let run = |target: &str, label: &str| {
                let (core, path, mut s) = setup(label, UNIQ);
                let _g = CleanupGuard(path);
                seed(&core, &mut s);
                let sql = format!(
                    "INSERT INTO docs (id, a, b) VALUES ({id}, '{a}', 'new') ON CONFLICT {target} \
                     {action} RETURNING * USING OPERATION_ID 'ret-1'"
                );
                let out = format!("{:?}", exec(&core, &mut s, "owner", &sql).expect(&sql));
                (out, rows(&core, &mut s, "docs"))
            };
            let by_name = run("ON CONSTRAINT uq_a", "onc-ret-name");
            let by_cols = run("(a)", "onc-ret-cols");
            assert_eq!(by_name, by_cols, "{action} id={id} a={a}");
        }
    }
}

#[test]
fn retry_across_forms_is_a_ledger_duplicate_not_a_mismatch() {
    let (core, path, mut s) = setup("onc-retry", UNIQ);
    let _g = CleanupGuard(path);
    seed(&core, &mut s);
    let by_cols = "INSERT INTO docs (id, a, b) VALUES (2, 'y', 'v') ON CONFLICT (a) \
                   DO UPDATE SET b = EXCLUDED.b USING OPERATION_ID 'retry-1'";
    let by_name = "INSERT INTO docs (id, a, b) VALUES (2, 'y', 'v') ON CONFLICT ON CONSTRAINT \
                   uq_a DO UPDATE SET b = EXCLUDED.b USING OPERATION_ID 'retry-1'";
    exec(&core, &mut s, "owner", by_cols).expect("first");
    // 解決後の衝突対象が同じなので内容照合は一致し、台帳重複（`22023` ではない）になる。
    assert!(matches!(
        exec(&core, &mut s, "owner", by_name),
        Err(SqlSurfaceError::DuplicateOperationId)
    ));
    let changed = "INSERT INTO docs (id, a, b) VALUES (2, 'y', 'v') ON CONFLICT ON CONSTRAINT \
                   uq_a DO UPDATE SET b = 'other' USING OPERATION_ID 'retry-1'";
    assert_eq!(code(&core, &mut s, "owner", changed), "22023");
}

#[test]
fn tenants_are_independent() {
    let (core, path, mut s) = setup("onc-tenant", UNIQ);
    let _g = CleanupGuard(path);
    seed(&core, &mut s);
    // 別テナントの同じキーとは衝突しない（自テナントの行として挿入される）。
    exec(
        &core,
        &mut s,
        "other",
        "INSERT INTO docs (id, a, b) VALUES (1, 'x', 'o') ON CONFLICT ON CONSTRAINT uq_a \
         DO UPDATE SET b = EXCLUDED.b USING OPERATION_ID 'ten-1'",
    )
    .expect("other tenant inserts its own row");
    assert!(rows(&core, &mut s, "docs").contains("b0"));
}
