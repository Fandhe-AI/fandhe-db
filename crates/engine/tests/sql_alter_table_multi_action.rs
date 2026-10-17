//! `ALTER TABLE ... ADD COLUMN IF NOT EXISTS`（TABLE-36）と `ALTER TABLE` の複数アクション
//! （TABLE-39 (5)、Issue #1633）の結合テスト。ポインタ: `docs/spec/04-behavior/data-model.md`
//! TABLE-36・TABLE-38 (6)・TABLE-39、`docs/spec/04-behavior/error-format.md` ERR-6。
//!
//! 他の `sql_alter_table_*.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `EngineCore::execute_sql_in_session` を production 経路として検証）。ここでは構文
//! （`,` 区切り・混在規則）、原子性（途中失敗で何も適用されない）、IF NOT EXISTS の no-op、
//! 権限ゲート、他テナント情報の非漏えいを固定する。世代（`USING PLAN` 失効契約）の不変・
//! 1 回だけの bump は crate 内部 API を要するため `sql::ddl` のユニットテストが担う。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ddl_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn exec(
    core: &EngineCore,
    session: &mut SessionState,
    tenant: &str,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(tenant), session, sql)
}

fn ok(core: &EngineCore, session: &mut SessionState, sql: &str) {
    exec(core, session, "owner", sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn code(core: &EngineCore, session: &mut SessionState, sql: &str) -> String {
    match exec(core, session, "owner", sql) {
        Ok(_) => panic!("statement must fail: {sql}"),
        Err(e) => e,
    }
    .wire_code()
    .to_string()
}

fn setup(label: &str) -> (EngineCore, std::path::PathBuf, SessionState) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = ddl_session();
    ok(&core, &mut session, "CREATE TABLE docs (a TEXT, n INTEGER)");
    (core, path, session)
}

/// 候補列のうち現在のスキーマに存在するものを、SQL 経由の「未知の列」エラー（`42703`）の
/// 有無で判定して返す（`EngineCore` から `Storage` を取り出す手段がないため。
/// `sql_ddl_add_column.rs` と同じ流儀）。
fn column_names(core: &EngineCore) -> Vec<String> {
    ["a", "b", "c", "n", "note", "x", "y"]
        .iter()
        .filter(|name| {
            match core.execute_sql(&ctx("owner"), &format!("SELECT {name} FROM docs LIMIT 1")) {
                Ok(_) => true,
                Err(e) if e.wire_code() == "42703" => false,
                Err(e) => panic!("unexpected error probing {name}: {e}"),
            }
        })
        .map(|name| name.to_string())
        .collect()
}

fn insert_null_a(core: &EngineCore, session: &mut SessionState, tenant: &str, id: u32) {
    let sql =
        format!("INSERT INTO docs (id, n) VALUES ({id}, 7) USING OPERATION_ID 'op-{tenant}-{id}'");
    exec(core, session, tenant, &sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// TABLE-36: 既存列への `IF NOT EXISTS` は成功し、スキーマを変えない。無ければ追加する。
/// 句なしの重複は `42701` のまま。
#[test]
fn add_column_if_not_exists_is_noop_on_existing_and_adds_missing() {
    let (core, path, mut session) = setup("multi-ine-single");
    let _guard = CleanupGuard(path);
    let before = column_names(&core);

    ok(
        &core,
        &mut session,
        "ALTER TABLE docs ADD COLUMN IF NOT EXISTS a TEXT",
    );
    // 型が異なっても既存列があれば no-op（PostgreSQL と同じく型は比較しない）。
    ok(
        &core,
        &mut session,
        "ALTER TABLE docs ADD COLUMN IF NOT EXISTS a INTEGER NOT NULL DEFAULT 1",
    );
    assert_eq!(column_names(&core), before);

    ok(
        &core,
        &mut session,
        "ALTER TABLE docs ADD COLUMN IF NOT EXISTS note TEXT",
    );
    assert_eq!(column_names(&core), ["a", "n", "note"]);
    assert_eq!(
        code(&core, &mut session, "ALTER TABLE docs ADD COLUMN note TEXT"),
        "42701"
    );
}

/// TABLE-36: 墓標（DROP 済み列）と同名なら名前空間に無いので追加される。
#[test]
fn add_column_if_not_exists_adds_over_dropped_tombstone() {
    let (core, path, mut session) = setup("multi-ine-tombstone");
    let _guard = CleanupGuard(path);
    ok(&core, &mut session, "ALTER TABLE docs DROP COLUMN n");
    ok(
        &core,
        &mut session,
        "ALTER TABLE docs ADD COLUMN IF NOT EXISTS n INTEGER",
    );
    assert_eq!(column_names(&core), ["a", "n"]);
}

/// 表の不在・予約列名・権限は `IF NOT EXISTS` でも緩めない（fail-closed）。
#[test]
fn if_not_exists_does_not_relax_other_checks() {
    let (core, path, mut session) = setup("multi-ine-failclosed");
    let _guard = CleanupGuard(path);
    assert_eq!(
        code(
            &core,
            &mut session,
            "ALTER TABLE nope ADD COLUMN IF NOT EXISTS x TEXT"
        ),
        "42P01"
    );
    assert_eq!(
        code(
            &core,
            &mut session,
            "ALTER TABLE docs ADD COLUMN IF NOT EXISTS id TEXT"
        ),
        "42601"
    );
    let mut denied = SessionState::default();
    assert_eq!(
        code(
            &core,
            &mut denied,
            "ALTER TABLE docs ADD COLUMN IF NOT EXISTS a TEXT"
        ),
        "42501"
    );
    assert_eq!(
        code(
            &core,
            &mut denied,
            "ALTER TABLE docs ADD COLUMN x TEXT, ADD COLUMN y TEXT"
        ),
        "42501"
    );
}

/// TABLE-39 (5): 複数アクションが 1 文で適用される（適用順は DROP → ADD → NOT NULL → DEFAULT）。
#[test]
fn multi_action_applies_all_actions() {
    let (core, path, mut session) = setup("multi-ok");
    let _guard = CleanupGuard(path);
    ok(
        &core,
        &mut session,
        "ALTER TABLE docs ADD COLUMN x INTEGER DEFAULT 1, \
         ALTER COLUMN x SET NOT NULL, ADD COLUMN y TEXT, ALTER COLUMN y SET DEFAULT 'v', \
         DROP COLUMN n",
    );
    assert_eq!(column_names(&core), ["a", "x", "y"]);
    // SET DEFAULT が効く: 省略した y には 'v' が入り、x の既定値 1 が入る。
    ok(
        &core,
        &mut session,
        "INSERT INTO docs (id, a) VALUES (1, 'p') USING OPERATION_ID 'op-multi-1'",
    );
    let rows = core
        .execute_sql(&ctx("owner"), "SELECT y, x FROM docs WHERE id = 1 LIMIT 1")
        .expect("select");
    let dump = format!("{:?}", rows.rows[0].cells);
    assert!(dump.contains("\"v\"") && dump.contains('1'), "{dump}");
    // SET NOT NULL が効く: x へ明示 NULL は 23502。
    assert_eq!(
        code(
            &core,
            &mut session,
            "INSERT INTO docs (id, a, x) VALUES (2, 'p', NULL) USING OPERATION_ID 'op-multi-2'"
        ),
        "23502"
    );
}

/// TABLE-39 (5): 最後のアクションが失敗したら先行アクションも適用されない。
#[test]
fn multi_action_failure_applies_nothing() {
    let (core, path, mut session) = setup("multi-atomic");
    let _guard = CleanupGuard(path);
    let before = column_names(&core);
    assert_eq!(
        code(
            &core,
            &mut session,
            "ALTER TABLE docs ADD COLUMN b TEXT, ADD COLUMN c INTEGER, DROP COLUMN zz"
        ),
        "42703"
    );
    assert_eq!(column_names(&core), before);
    // 追加が巻き戻っているので、同名列を改めて追加できる。
    ok(
        &core,
        &mut session,
        "ALTER TABLE docs ADD COLUMN b TEXT, ADD COLUMN c INTEGER",
    );
}

/// 途中の `SET NOT NULL` 違反（`23502`）でも何も適用されない。違反行が他テナントのものでも
/// 文言はテナント・id・値を含まない。
#[test]
fn multi_action_not_null_violation_rolls_back_without_leaking() {
    let (core, path, mut session) = setup("multi-nn-violation");
    let _guard = CleanupGuard(path);
    insert_null_a(&core, &mut session, "tenant-zzz", 4242);
    let before = column_names(&core);
    let err = exec(
        &core,
        &mut session,
        "owner",
        "ALTER TABLE docs ADD COLUMN x TEXT, ALTER COLUMN a SET NOT NULL",
    )
    .expect_err("must fail");
    assert_eq!(err.wire_code(), "23502");
    let msg = err.to_string();
    for leak in ["tenant-zzz", "4242"] {
        assert!(!msg.contains(leak), "leaked {leak}: {msg}");
    }
    assert_eq!(column_names(&core), before);
}

/// 同じ列への複数アクションの扱い（PostgreSQL の pass 順に合わせる）。
#[test]
fn multi_action_same_column_semantics() {
    let (core, path, mut session) = setup("multi-same-col");
    let _guard = CleanupGuard(path);
    // 同名 ADD の重複は 2 個目で 42701（何も適用されない）。
    assert_eq!(
        code(
            &core,
            &mut session,
            "ALTER TABLE docs ADD COLUMN x TEXT, ADD COLUMN x TEXT"
        ),
        "42701"
    );
    assert_eq!(column_names(&core), ["a", "n"]);
    // IF NOT EXISTS の 2 重指定は 1 個目だけ適用される。
    ok(
        &core,
        &mut session,
        "ALTER TABLE docs ADD COLUMN IF NOT EXISTS x TEXT, ADD COLUMN IF NOT EXISTS x TEXT",
    );
    assert_eq!(column_names(&core), ["a", "n", "x"]);
    // DROP は ADD より先に適用されるため、DROP x, ADD x は新しい列で置き換わる。
    ok(
        &core,
        &mut session,
        "ALTER TABLE docs ADD COLUMN x INTEGER, DROP COLUMN x",
    );
    assert_eq!(column_names(&core), ["a", "n", "x"]);
    // 同じ列の DROP の重複は 42703。
    assert_eq!(
        code(
            &core,
            &mut session,
            "ALTER TABLE docs DROP COLUMN n, DROP COLUMN n"
        ),
        "42703"
    );
    assert_eq!(column_names(&core), ["a", "n", "x"]);
}

/// 全アクションが no-op（skip・冪等）でも成功し、スキーマは変わらない。
#[test]
fn multi_action_all_noop_succeeds_unchanged() {
    let (core, path, mut session) = setup("multi-noop");
    let _guard = CleanupGuard(path);
    let before = column_names(&core);
    ok(
        &core,
        &mut session,
        "ALTER TABLE docs ADD COLUMN IF NOT EXISTS a TEXT, ALTER COLUMN a DROP DEFAULT, \
         ALTER COLUMN a DROP NOT NULL",
    );
    assert_eq!(column_names(&core), before);
}

/// RENAME との混在は `42601`（TABLE-38 (6)）、未対応の種別との混在は `0A000`（fail-closed）。
#[test]
fn multi_action_unsupported_mixes_are_rejected() {
    let (core, path, mut session) = setup("multi-mix");
    let _guard = CleanupGuard(path);
    for sql in [
        "ALTER TABLE docs ADD COLUMN x TEXT, RENAME COLUMN a TO a2",
        "ALTER TABLE docs RENAME COLUMN a TO a2, ADD COLUMN x TEXT",
        "ALTER TABLE docs ADD COLUMN x TEXT,",
        "ALTER TABLE docs ADD COLUMN x TEXT, ,ADD COLUMN y TEXT",
    ] {
        assert_eq!(code(&core, &mut session, sql), "42601", "{sql}");
    }
    for sql in [
        "ALTER TABLE docs ADD COLUMN x TEXT, ALTER COLUMN a TYPE VARCHAR(10)",
        "ALTER TABLE docs ADD COLUMN x TEXT, ADD UNIQUE (a)",
        "ALTER TABLE docs ADD COLUMN x TEXT, ADD CONSTRAINT c CHECK (n > 0)",
        "ALTER TABLE docs ADD COLUMN x TEXT, DROP CONSTRAINT c",
        "ALTER TABLE docs ADD COLUMN x TEXT, ADD PRIMARY KEY (a)",
    ] {
        assert_eq!(code(&core, &mut session, sql), "0A000", "{sql}");
    }
    assert_eq!(column_names(&core), ["a", "n"]);
}

/// アクション数の上限超過は確保前に `54000`。
#[test]
fn multi_action_count_limit_is_enforced() {
    let (core, path, mut session) = setup("multi-limit");
    let _guard = CleanupGuard(path);
    let sql = format!(
        "ALTER TABLE docs {}",
        (0..300)
            .map(|i| format!("ADD COLUMN IF NOT EXISTS c{i} TEXT"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    assert_eq!(code(&core, &mut session, &sql), "54000");
    assert_eq!(column_names(&core), ["a", "n"]);
}
