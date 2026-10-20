//! 部分 UNIQUE 索引（`CREATE UNIQUE INDEX ... WHERE <述語>`）の結合テスト（INDEX-11 (4)、Issue #1657）。
//! ポインタ: `docs/spec/04-behavior/indexing.md` INDEX-9・INDEX-11、`docs/spec/04-behavior/error-format.md` ERR-6。
//!
//! `sql_partial_index_dml.rs`・`sql_expression_unique_index.rs` と同じ流儀（実 `Storage` ＋
//! `EngineCore::execute_sql_in_session` を production 経路として使う）。検証する契約
//! （詳細は `docs/design/partial-index.md`）:
//!
//! - 述語が TRUE の行どうしの間でだけ一意性を強制する（テナントごと・NULLS DISTINCT・`23505`）。
//!   述語外（NULL・FALSE）の行どうし、および述語外と述語内の行の間の重複は許可する
//! - UPDATE による述語への入場は新キーの衝突検査を受け、退場した行のキーは解放される
//! - 作成時の既存行検査も述語 TRUE の行だけが対象。述語・式の評価エラーは重複より先に報告される
//! - 部分 UNIQUE は UNIQUE 制約へ展開されない（`ON CONFLICT` の推論対象外）
//! - 再オープン・`DROP INDEX`・`DROP COLUMN`・`RENAME COLUMN` に追随する
//! - 他テナントの行は衝突せず、拒否文言は値・テナント・件数を含まない

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::Storage;
use engine::storage::Visibility;
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
// SQL 表層の `CREATE TABLE` は暗黙の `id` を持たない採番方式の表を作る（TABLE-23・Issue #2199）。
// このテストは疑似列 `id`・従来方式の契約を検証するため、`CREATE TABLE` を従来方式の表作成へ
// 振り向ける（`LegacyExec`）。
#[path = "../src/test_util/legacy_table.rs"]
mod legacy_table;
use legacy_table::LegacyExec as _;
use temp_db::{unique_db_path, CleanupGuard};

static OP: AtomicU64 = AtomicU64::new(0);

fn op() -> String {
    format!("pui-op-{}", OP.fetch_add(1, Ordering::Relaxed))
}

fn ctx_of(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn allowed_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn core_at(path: &std::path::Path) -> EngineCore {
    EngineCore::from_storage(
        Storage::open(path).expect("open storage"),
        Box::new(CpuScalarProvider),
    )
}

fn exec_as(
    core: &EngineCore,
    s: &mut SessionState,
    tenant: &str,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session_legacy(&ctx_of(tenant), s, sql)
}

fn run(core: &EngineCore, s: &mut SessionState, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    exec_as(core, s, "alice", sql)
}

fn code_of(core: &EngineCore, s: &mut SessionState, sql: &str) -> String {
    match run(core, s, sql) {
        Ok(outcome) => panic!("{sql} must fail, got {outcome:?}"),
        Err(e) => e.wire_code().to_string(),
    }
}

fn insert_as(
    core: &EngineCore,
    s: &mut SessionState,
    tenant: &str,
    id: u64,
    n: Option<i64>,
    code: Option<&str>,
) -> Result<(), String> {
    let n = n.map_or("NULL".to_string(), |v| v.to_string());
    let code = code.map_or("NULL".to_string(), |v| format!("'{v}'"));
    exec_as(
        core,
        s,
        tenant,
        &format!(
            "INSERT INTO xt (id, n, code) VALUES ({id}, {n}, {code}) USING OPERATION_ID '{}'",
            op()
        ),
    )
    .map(|_| ())
    .map_err(|e| e.wire_code().to_string())
}

fn insert(
    core: &EngineCore,
    s: &mut SessionState,
    id: u64,
    n: Option<i64>,
    code: Option<&str>,
) -> Result<(), String> {
    insert_as(core, s, "alice", id, n, code)
}

fn update(core: &EngineCore, s: &mut SessionState, set: &str, id: u64) -> Result<(), String> {
    run(
        core,
        s,
        &format!(
            "UPDATE xt SET {set} WHERE id = {id} USING OPERATION_ID '{}'",
            op()
        ),
    )
    .map(|_| ())
    .map_err(|e| e.wire_code().to_string())
}

fn list(path: &std::path::Path) -> Vec<engine::catalog::IndexDef> {
    Storage::open(path)
        .expect("reopen storage")
        .list_indexes()
        .expect("list indexes")
}

/// `xt (n INTEGER, code TEXT)` を作ったフィクスチャ（索引は張らない）。
fn fixture(label: &str) -> (std::path::PathBuf, CleanupGuard, EngineCore, SessionState) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let core = core_at(&path);
    let mut s = allowed_session();
    run(&core, &mut s, "CREATE TABLE xt (n INTEGER, code TEXT)").expect("create table");
    (path, guard, core, s)
}

const CREATE_PARTIAL: &str = "CREATE UNIQUE INDEX pu ON xt (code) WHERE n > 0";

/// 述語内の重複は `23505`、述語外どうし・述語外と述語内の重複は受理される。
#[test]
fn duplicates_are_rejected_only_inside_the_predicate() {
    let (_path, _guard, core, mut s) = fixture("pui-basic");
    run(&core, &mut s, CREATE_PARTIAL).expect("create partial unique index");
    insert(&core, &mut s, 1, Some(5), Some("a")).expect("first in-predicate row");
    assert_eq!(
        insert(&core, &mut s, 2, Some(7), Some("a")),
        Err("23505".to_string()),
        "duplicate inside the predicate"
    );
    // 述語外どうし（FALSE・NULL）の重複。
    insert(&core, &mut s, 3, Some(0), Some("a")).expect("outside (FALSE) row");
    insert(&core, &mut s, 4, Some(-1), Some("a")).expect("outside (FALSE) duplicate");
    insert(&core, &mut s, 5, None, Some("a")).expect("outside (NULL) duplicate");
    // 述語外の行と同じ値を持つ別値は述語内でも使える。
    insert(&core, &mut s, 6, Some(1), Some("b")).expect("distinct key inside");
    // 失敗した文は副作用を残さない。
    assert_eq!(
        insert(&core, &mut s, 2, Some(7), Some("a")),
        Err("23505".to_string())
    );
}

/// 要素が NULL の行は述語内でも重複しない（NULLS DISTINCT）。
#[test]
fn null_elements_do_not_conflict() {
    let (_path, _guard, core, mut s) = fixture("pui-null");
    run(&core, &mut s, CREATE_PARTIAL).expect("create");
    insert(&core, &mut s, 1, Some(5), None).expect("null key");
    insert(&core, &mut s, 2, Some(6), None).expect("null key again");
}

/// UPDATE による述語への入場は衝突検査を受け、退場した行のキーは解放される。
#[test]
fn update_entering_and_leaving_the_predicate() {
    let (_path, _guard, core, mut s) = fixture("pui-update");
    run(&core, &mut s, CREATE_PARTIAL).expect("create");
    insert(&core, &mut s, 1, Some(5), Some("a")).expect("inside");
    insert(&core, &mut s, 2, Some(0), Some("a")).expect("outside");
    // 述語外の行が述語内へ入ると既存キーと衝突する。
    assert_eq!(update(&core, &mut s, "n = 3", 2), Err("23505".to_string()));
    // 述語内の行が退場するとキーが解放され、同じ値を別の述語内の行で使える。
    update(&core, &mut s, "n = 0", 1).expect("leave predicate");
    update(&core, &mut s, "n = 3", 2).expect("enter after the key was released");
    assert_eq!(
        insert(&core, &mut s, 3, Some(9), Some("a")),
        Err("23505".to_string())
    );
    // キー列の変更も検査される。
    insert(&core, &mut s, 4, Some(1), Some("z")).expect("inside");
    assert_eq!(
        update(&core, &mut s, "code = 'a'", 4),
        Err("23505".to_string())
    );
    // DELETE でキーが解放される。
    run(
        &core,
        &mut s,
        &format!("DELETE FROM xt WHERE id = 2 USING OPERATION_ID '{}'", op()),
    )
    .expect("delete");
    insert(&core, &mut s, 5, Some(1), Some("a")).expect("key released by delete");
}

/// 作成時の既存行検査は述語 TRUE の行だけが対象。述語内に重複があれば拒否し、宣言を残さない。
#[test]
fn creation_checks_only_rows_inside_the_predicate() {
    let (path, _guard, core, mut s) = fixture("pui-create");
    insert(&core, &mut s, 1, Some(0), Some("a")).expect("outside");
    insert(&core, &mut s, 2, Some(0), Some("a")).expect("outside duplicate");
    insert(&core, &mut s, 3, Some(4), Some("a")).expect("inside");
    run(&core, &mut s, CREATE_PARTIAL).expect("outside-only duplicates do not block creation");
    run(&core, &mut s, "DROP INDEX pu").expect("drop");
    insert(&core, &mut s, 4, Some(5), Some("a")).expect("inside duplicate without the index");
    assert_eq!(code_of(&core, &mut s, CREATE_PARTIAL), "23505");
    assert_eq!(code_of(&core, &mut s, "DROP INDEX pu"), "42704");
    drop(core);
    assert!(
        list(&path).is_empty(),
        "rejected index leaves no declaration"
    );
}

/// 作成時の拒否文言は固定で、値・テナント・行 id・件数を含まない。他テナントの不可視行の重複も
/// 同じテナント内の重複としては数えない。
#[test]
fn creation_rejection_is_fixed_and_tenant_scoped() {
    let (_path, _guard, core, mut s) = fixture("pui-create-tenant");
    insert_as(&core, &mut s, "alice", 1, Some(5), Some("secret-key")).expect("alice");
    insert_as(&core, &mut s, "bob", 1, Some(5), Some("secret-key")).expect("bob same key");
    run(&core, &mut s, CREATE_PARTIAL).expect("different tenants are not duplicates");
    run(&core, &mut s, "DROP INDEX pu").expect("drop");
    insert_as(&core, &mut s, "bob", 2, Some(6), Some("secret-key")).expect("bob duplicate");
    let err = run(&core, &mut s, CREATE_PARTIAL).expect_err("bob duplicate blocks creation");
    assert_eq!(err.wire_code(), "23505");
    let message = err.to_string();
    for needle in ["secret-key", "bob", "alice", "xt", "2"] {
        assert!(
            !message.contains(needle),
            "message must not leak {needle}: {message}"
        );
    }
}

/// 述語の評価エラーは重複より先に報告される。述語外の行では評価されない式要素は拒否理由にならない。
#[test]
fn evaluation_errors_precede_duplicates() {
    let (_path, _guard, core, mut s) = fixture("pui-eval");
    insert(&core, &mut s, 1, Some(0), Some("a")).expect("row");
    insert(&core, &mut s, 2, Some(0), Some("a")).expect("duplicate row");
    // n = 0 で 0 除算になる述語は 22012（重複の 23505 ではない）。
    assert_eq!(
        code_of(
            &core,
            &mut s,
            "CREATE UNIQUE INDEX pe ON xt (code) WHERE 10 / n > 1"
        ),
        "22012"
    );
    // 式要素 `100 / n` は述語 `n <> 0` の外（n = 0）では評価されないため作成できる。
    run(
        &core,
        &mut s,
        "CREATE UNIQUE INDEX pe2 ON xt ((100 / n)) WHERE n <> 0",
    )
    .expect("element is not evaluated outside the predicate");
}

/// 式要素付きの部分 UNIQUE も同じ規則（述語内だけ・NULLS DISTINCT）。
#[test]
fn expression_elements_follow_the_same_rule() {
    let (_path, _guard, core, mut s) = fixture("pui-expr");
    run(
        &core,
        &mut s,
        "CREATE UNIQUE INDEX pe ON xt ((lower(code))) WHERE n > 0",
    )
    .expect("create");
    insert(&core, &mut s, 1, Some(1), Some("Ja")).expect("first");
    assert_eq!(
        insert(&core, &mut s, 2, Some(2), Some("jA")),
        Err("23505".to_string())
    );
    insert(&core, &mut s, 3, Some(0), Some("ja")).expect("outside the predicate");
    insert(&core, &mut s, 4, Some(1), None).expect("NULL element");
    insert(&core, &mut s, 5, Some(1), None).expect("NULL element again");
}

/// 他テナントの同じキーは衝突せず、衝突時のエラーは固定文言で他テナントの存在を示さない。
#[test]
fn tenant_boundary_is_preserved() {
    let (_path, _guard, core, mut s) = fixture("pui-tenant");
    run(&core, &mut s, CREATE_PARTIAL).expect("create");
    insert_as(&core, &mut s, "alice", 1, Some(5), Some("k")).expect("alice");
    insert_as(&core, &mut s, "bob", 1, Some(5), Some("k")).expect("bob same key");
    let err = exec_as(
        &core,
        &mut s,
        "carol",
        &format!(
            "INSERT INTO xt (id, n, code) VALUES (1, 5, 'k') USING OPERATION_ID '{}'",
            op()
        ),
    );
    assert!(err.is_ok(), "carol has no row with that key");
    let dup = insert_as(&core, &mut s, "alice", 2, Some(5), Some("k"));
    assert_eq!(dup, Err("23505".to_string()));
}

/// `DROP INDEX` で強制が外れ、再オープンしても強制は続く。列だけの部分 UNIQUE でも `XX000` にならない。
#[test]
fn drop_and_reopen() {
    let (path, _guard, core, mut s) = fixture("pui-reopen");
    run(&core, &mut s, CREATE_PARTIAL).expect("create");
    insert(&core, &mut s, 1, Some(5), Some("a")).expect("first");
    drop(core);
    let defs = list(&path);
    assert_eq!(defs.len(), 1);
    assert!(defs[0].unique && defs[0].predicate.is_some());
    let core = core_at(&path);
    let mut s = allowed_session();
    assert_eq!(
        insert(&core, &mut s, 2, Some(6), Some("a")),
        Err("23505".to_string()),
        "enforcement survives reopen"
    );
    run(&core, &mut s, "DROP INDEX pu").expect("drop must not be XX000");
    insert(&core, &mut s, 2, Some(6), Some("a")).expect("no longer enforced");
    drop(core);
    assert!(list(&path).is_empty());
}

/// `IF NOT EXISTS` の再実行は副作用なしで成功する。
#[test]
fn if_not_exists_is_idempotent() {
    let (path, _guard, core, mut s) = fixture("pui-ine");
    run(&core, &mut s, CREATE_PARTIAL).expect("create");
    run(
        &core,
        &mut s,
        "CREATE UNIQUE INDEX IF NOT EXISTS pu ON xt (code) WHERE n > 0",
    )
    .expect("rerun");
    assert_eq!(code_of(&core, &mut s, CREATE_PARTIAL), "42P07");
    drop(core);
    assert_eq!(list(&path).len(), 1);
}

/// 列 DDL: 述語・要素の列の `DROP COLUMN` は索引を連鎖削除して強制が外れ、`RENAME COLUMN` 後も強制が続く。
#[test]
fn column_ddl_follows_the_index() {
    let (_path, _guard, core, mut s) = fixture("pui-columns");
    run(&core, &mut s, CREATE_PARTIAL).expect("create");
    insert(&core, &mut s, 1, Some(5), Some("a")).expect("first");
    run(&core, &mut s, "ALTER TABLE xt RENAME COLUMN n TO m").expect("rename predicate column");
    run(&core, &mut s, "ALTER TABLE xt RENAME COLUMN code TO label").expect("rename key column");
    assert_eq!(
        run(
            &core,
            &mut s,
            &format!(
                "INSERT INTO xt (id, m, label) VALUES (2, 6, 'a') USING OPERATION_ID '{}'",
                op()
            )
        )
        .map(|_| ())
        .map_err(|e| e.wire_code().to_string()),
        Err("23505".to_string()),
        "enforcement follows renamed columns"
    );
    run(&core, &mut s, "ALTER TABLE xt DROP COLUMN m").expect("drop predicate column");
    run(
        &core,
        &mut s,
        &format!(
            "INSERT INTO xt (id, label) VALUES (2, 'a') USING OPERATION_ID '{}'",
            op()
        ),
    )
    .expect("index was dropped with its column, nothing is enforced");
}

/// 部分 UNIQUE は UNIQUE 制約へ展開されず、`ON CONFLICT` の推論対象にならない。
#[test]
fn partial_unique_index_is_not_an_on_conflict_arbiter() {
    let (_path, _guard, core, mut s) = fixture("pui-conflict");
    run(&core, &mut s, CREATE_PARTIAL).expect("create");
    let sql = |target: &str| {
        format!(
            "INSERT INTO xt (id, n, code) VALUES (1, 5, 'a') ON CONFLICT {target} DO NOTHING \
             USING OPERATION_ID '{}'",
            op()
        )
    };
    assert_eq!(code_of(&core, &mut s, &sql("(code)")), "42P10");
    assert_eq!(code_of(&core, &mut s, &sql("(code) WHERE n > 0")), "42601");
}

/// 未対応の組み合わせは従来どおり `0A000`。一意キー許可型外の素の列は式一意索引と同じ拒否。
#[test]
fn unsupported_combinations_stay_rejected() {
    let (_path, _guard, core, mut s) = fixture("pui-unsupported");
    assert_eq!(
        code_of(
            &core,
            &mut s,
            "CREATE UNIQUE INDEX u1 ON xt USING hnsw (embedding) WHERE n > 0"
        ),
        "0A000"
    );
    assert_eq!(
        code_of(
            &core,
            &mut s,
            "CREATE UNIQUE INDEX u2 ON xt (code COLLATE \"C\") WHERE n > 0"
        ),
        "0A000"
    );
    // 暗黙の `id` は一意索引の素の列要素にできない。
    assert!(run(
        &core,
        &mut s,
        "CREATE UNIQUE INDEX u3 ON xt (id) WHERE n > 0"
    )
    .is_err());
    // 述語の参照列の型変更は 0A000 のまま。
    run(&core, &mut s, CREATE_PARTIAL).expect("create");
    assert_eq!(
        code_of(&core, &mut s, "ALTER TABLE xt ALTER COLUMN n TYPE BIGINT"),
        "0A000"
    );
}
