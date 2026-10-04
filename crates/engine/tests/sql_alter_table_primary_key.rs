//! `ALTER TABLE ... ADD PRIMARY KEY (...)` ／ 主キーを対象にした
//! `ALTER TABLE ... DROP CONSTRAINT <table>_pkey`（TABLE-22 (a)(d)・TASK-233、
//! Issue #1196）の結合テスト。ポインタ: `docs/spec/04-behavior/data-model.md`
//! TABLE-12・TABLE-16・TABLE-17・TABLE-22・`docs/spec/04-behavior/rls.md`
//! RLS-9・RLS-10 (c)・`docs/spec/04-behavior/error-format.md` ERR-6。
//!
//! `sql_alter_table_unique_constraint.rs` と同じ流儀（実 `Storage` ＋
//! `CpuScalarProvider`、`EngineCore::execute_sql_in_session` を production 経路として
//! 検証する）。既存行の一意性判定は単一検査点
//! `constraint::table_has_duplicate_unique_key` を再利用するため、ここでは SQL 表層
//! （構文・権限ゲート・名前解決・エラー分類）と「全テナント走査・副作用ゼロ・
//! 他テナント情報の非漏えい」の観点を検証する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::ddl::AlterTableAction;
use engine::sql::mode::SessionState;
use engine::sql::transaction::TransactionStatus;
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

fn ddl_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn exec(
    core: &EngineCore,
    session: &mut SessionState,
    ctx: &PolicyContext,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(ctx, session, sql)
}

fn code(r: Result<SqlOutcome, SqlSurfaceError>) -> String {
    r.expect_err("statement must fail").wire_code().to_string()
}

fn setup(label: &str) -> (EngineCore, std::path::PathBuf, SessionState) {
    let (core, path) = new_core(label);
    let mut session = ddl_session();
    exec(
        &core,
        &mut session,
        &ctx("owner"),
        "CREATE TABLE docs (a TEXT, b TEXT)",
    )
    .expect("create table");
    (core, path, session)
}

fn insert(
    core: &EngineCore,
    session: &mut SessionState,
    tenant: &str,
    id: u32,
    a: Option<&str>,
    b: &str,
    op: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    let sql = match a {
        Some(a) => format!(
            "INSERT INTO docs (id, a, b) VALUES ({id}, '{a}', '{b}') USING OPERATION_ID '{op}'"
        ),
        None => {
            format!("INSERT INTO docs (id, b) VALUES ({id}, '{b}') USING OPERATION_ID '{op}'")
        }
    };
    exec(core, session, &ctx(tenant), &sql)
}

// --- ADD: 成功系 ----------------------------------------------------------

/// 単一列の ADD が成功し導出擬似名を返す。同一テナント内の重複は `23505`、
/// 別テナントの同値は成功する（テナントごとの一意性。TABLE-12）。
#[test]
fn add_primary_key_returns_pseudo_name_and_enforces_per_tenant() {
    let (core, path, mut session) = setup("alter-pk-add-single");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");

    let outcome = exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD PRIMARY KEY (a)",
    )
    .expect("add primary key");
    match outcome {
        SqlOutcome::AlterTable(o) => assert_eq!(
            o.action,
            AlterTableAction::AddConstraint {
                constraint_name: "docs_pkey".to_string()
            }
        ),
        other => panic!("unexpected outcome: {other:?}"),
    }

    insert(&core, &mut session, "owner", 1, Some("x"), "y", "op-1").expect("first");
    assert_eq!(
        code(insert(
            &core,
            &mut session,
            "owner",
            2,
            Some("x"),
            "z",
            "op-2"
        )),
        "23505"
    );
    insert(&core, &mut session, "other", 1, Some("x"), "y", "op-3")
        .expect("same value in another tenant is allowed");
    // PK 列は NOT NULL になる。
    assert_eq!(
        code(insert(&core, &mut session, "owner", 3, None, "z", "op-4")),
        "23502"
    );
}

/// 複合キーの ADD が成功し、複合値の重複は `23505`。
#[test]
fn add_composite_primary_key_enforces_composite_uniqueness() {
    let (core, path, mut session) = setup("alter-pk-add-composite");
    let _guard = CleanupGuard(path);
    exec(
        &core,
        &mut session,
        &ctx("owner"),
        "ALTER TABLE docs ADD PRIMARY KEY (a, b)",
    )
    .expect("add composite primary key");
    insert(&core, &mut session, "owner", 1, Some("x"), "y", "op-1").expect("first");
    insert(&core, &mut session, "owner", 2, Some("x"), "z", "op-2").expect("differs in b");
    assert_eq!(
        code(insert(
            &core,
            &mut session,
            "owner",
            3,
            Some("x"),
            "y",
            "op-3"
        )),
        "23505"
    );
}

// --- ADD: 既存行検証（副作用ゼロ）------------------------------------------

/// 既存行に NULL があれば `23502`。拒否後もカタログ・行は変わらない。
#[test]
fn add_primary_key_with_existing_null_is_23502_without_side_effects() {
    let (core, path, mut session) = setup("alter-pk-existing-null");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    insert(&core, &mut session, "owner", 1, None, "y", "op-1").expect("row with NULL a");

    assert_eq!(
        code(exec(
            &core,
            &mut session,
            &owner,
            "ALTER TABLE docs ADD PRIMARY KEY (a)"
        )),
        "23502"
    );
    // 副作用ゼロ: 制約は追加されておらず NULL の INSERT も通る。
    insert(&core, &mut session, "owner", 2, None, "z", "op-2").expect("no NOT NULL was added");
}

/// `ADD COLUMN`（DEFAULT なし）より前に書かれた行（列のバイトが欠落）も NULL として
/// 検出され `23502`（`XX000` にならない）。
#[test]
fn add_primary_key_on_column_added_after_rows_exist_is_23502() {
    let (core, path, mut session) = setup("alter-pk-legacy-rows");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    insert(&core, &mut session, "owner", 1, Some("x"), "y", "op-1").expect("legacy row");
    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD COLUMN c TEXT",
    )
    .expect("add column");

    assert_eq!(
        code(exec(
            &core,
            &mut session,
            &owner,
            "ALTER TABLE docs ADD PRIMARY KEY (c)"
        )),
        "23502"
    );
}

/// DEFAULT 付きで追加した列は既存行で既定値として読まれ NULL ではない。
/// 2 行以上あれば全行が同値になり `23505`。
#[test]
fn add_primary_key_on_defaulted_column_with_rows_is_23505() {
    let (core, path, mut session) = setup("alter-pk-defaulted-column");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    insert(&core, &mut session, "owner", 1, Some("x"), "y", "op-1").expect("row 1");
    insert(&core, &mut session, "owner", 2, Some("x2"), "y", "op-2").expect("row 2");
    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD COLUMN c TEXT NOT NULL DEFAULT 'k'",
    )
    .expect("add defaulted column");

    assert_eq!(
        code(exec(
            &core,
            &mut session,
            &owner,
            "ALTER TABLE docs ADD PRIMARY KEY (c)"
        )),
        "23505"
    );
}

/// 既存行の重複は `23505`。拒否後も重複 INSERT は成功する（副作用ゼロ）。
#[test]
fn add_primary_key_with_existing_duplicates_is_23505_without_side_effects() {
    let (core, path, mut session) = setup("alter-pk-existing-dup");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    insert(&core, &mut session, "owner", 1, Some("x"), "y", "op-1").expect("row 1");
    insert(&core, &mut session, "owner", 2, Some("x"), "z", "op-2").expect("row 2");

    assert_eq!(
        code(exec(
            &core,
            &mut session,
            &owner,
            "ALTER TABLE docs ADD PRIMARY KEY (a)"
        )),
        "23505"
    );
    insert(&core, &mut session, "owner", 3, Some("x"), "w", "op-3")
        .expect("no primary key was added");
}

/// 2 テナントが同じ値を持っていても ADD は成功する（テナントを跨いだ同値は違反ではない）。
#[test]
fn add_primary_key_allows_same_value_across_tenants() {
    let (core, path, mut session) = setup("alter-pk-cross-tenant");
    let _guard = CleanupGuard(path);
    insert(&core, &mut session, "t1", 1, Some("x"), "y", "op-1").expect("t1");
    insert(&core, &mut session, "t2", 1, Some("x"), "y", "op-2").expect("t2");
    exec(
        &core,
        &mut session,
        &ctx("owner"),
        "ALTER TABLE docs ADD PRIMARY KEY (a)",
    )
    .expect("cross-tenant equal values are not a violation");
}

/// 可視性（Public／Private）で母集合を縮めない: 同一テナント内で片方が Private の
/// 重複でも `23505`。
#[test]
fn add_primary_key_scans_private_rows_too() {
    let (core, path, mut session) = setup("alter-pk-private-rows");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    insert(&core, &mut session, "owner", 1, Some("x"), "y", "op-1").expect("row 1");
    // Private 行を書ける主体で同値の 2 行目を書き込む。
    let private_writer =
        PolicyContext::with_visibilities("owner", [Visibility::Private]).expect("valid tenant");
    let _ = exec(
        &core,
        &mut session,
        &private_writer,
        "INSERT INTO docs (id, a, b) VALUES (2, 'x', 'z') USING OPERATION_ID 'op-2'",
    );
    assert_eq!(
        code(exec(
            &core,
            &mut session,
            &owner,
            "ALTER TABLE docs ADD PRIMARY KEY (a)"
        )),
        "23505"
    );
}

/// NULL と重複が併存しても、行の順序に関係なく常に `23502` が優先される。
#[test]
fn null_takes_precedence_over_duplicate_regardless_of_row_order() {
    // true = NULL 行、false = 重複値 'dup' の行。行 ID 順に挿入する。
    for (label, order) in [
        ("alter-pk-prec-1", [false, false, true]),
        ("alter-pk-prec-2", [true, false, false]),
    ] {
        let (core, path, mut session) = setup(label);
        let _guard = CleanupGuard(path);
        for (n, is_null) in order.iter().enumerate() {
            let id = u32::try_from(n).expect("small") + 1;
            let op = format!("op-{id}");
            let a = if *is_null { None } else { Some("dup") };
            insert(&core, &mut session, "owner", id, a, "y", &op).expect("seed row");
        }
        assert_eq!(
            code(exec(
                &core,
                &mut session,
                &ctx("owner"),
                "ALTER TABLE docs ADD PRIMARY KEY (a)"
            )),
            "23502"
        );
    }
}

// --- ADD: 拒否系 ----------------------------------------------------------

/// 未知の列・VECTOR 列は `42601`。
#[test]
fn add_primary_key_invalid_columns_are_42601() {
    let (core, path, mut session) = setup("alter-pk-invalid");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    assert_eq!(
        code(exec(
            &core,
            &mut session,
            &owner,
            "ALTER TABLE docs ADD PRIMARY KEY (missing)"
        )),
        "42601"
    );
    exec(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE vecs (embedding VECTOR(3))",
    )
    .expect("create vector table");
    assert_eq!(
        code(exec(
            &core,
            &mut session,
            &owner,
            "ALTER TABLE vecs ADD PRIMARY KEY (embedding)"
        )),
        "42601"
    );
}

/// 擬似名と同名の CHECK がある表への ADD、PK 宣言済み表への同名 UNIQUE の ADD は `42P07`。
#[test]
fn pseudo_name_collisions_are_42p07() {
    let (core, path, mut session) = setup("alter-pk-name-collision");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    exec(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE t1 (a TEXT, b TEXT, CONSTRAINT t1_pkey CHECK (a = 'z'))",
    )
    .expect("create with check named like the pseudo name");
    assert_eq!(
        code(exec(
            &core,
            &mut session,
            &owner,
            "ALTER TABLE t1 ADD PRIMARY KEY (a)"
        )),
        "42P07"
    );

    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD PRIMARY KEY (a)",
    )
    .expect("add primary key");
    assert_eq!(
        code(exec(
            &core,
            &mut session,
            &owner,
            "ALTER TABLE docs ADD CONSTRAINT docs_pkey UNIQUE (b)"
        )),
        "42P07"
    );
}

/// 構文の拒否: `id` を含む・空リスト・重複列は構造段で拒否される。
#[test]
fn structural_rejections() {
    let (core, path, mut session) = setup("alter-pk-syntax");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    for sql in [
        "ALTER TABLE docs ADD CONSTRAINT x PRIMARY KEY (id)",
        "ALTER TABLE docs ADD PRIMARY KEY (id)",
        "ALTER TABLE docs ADD PRIMARY KEY (ID)",
        "ALTER TABLE docs ADD PRIMARY KEY (id, a)",
        "ALTER TABLE docs ADD PRIMARY KEY ()",
        "ALTER TABLE docs ADD PRIMARY KEY a",
    ] {
        assert_eq!(
            code(exec(&core, &mut session, &owner, sql)),
            "42601",
            "{sql}"
        );
    }
    assert_eq!(
        code(exec(
            &core,
            &mut session,
            &owner,
            "ALTER TABLE docs ADD PRIMARY KEY (a, a)"
        )),
        "42701"
    );
    let cols: Vec<String> = (0..33).map(|i| format!("c{i}")).collect();
    assert_eq!(
        code(exec(
            &core,
            &mut session,
            &owner,
            &format!("ALTER TABLE docs ADD PRIMARY KEY ({})", cols.join(", "))
        )),
        "54000"
    );
}

/// DDL 権限が無ければ、存在するテーブル・しないテーブルのどちらでも `42501`。
/// 明示トランザクション内は `0A000` で `Failed` へ遷移し PK は追加されない。
#[test]
fn permission_and_explicit_transaction_gates() {
    let (core, path, mut session) = setup("alter-pk-gates");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");

    let mut plain = SessionState::default();
    assert_eq!(
        code(exec(
            &core,
            &mut plain,
            &owner,
            "ALTER TABLE docs ADD PRIMARY KEY (a)"
        )),
        "42501"
    );
    assert_eq!(
        code(exec(
            &core,
            &mut plain,
            &owner,
            "ALTER TABLE missing ADD PRIMARY KEY (a)"
        )),
        "42501"
    );
    assert_eq!(
        code(exec(
            &core,
            &mut ddl_session(),
            &owner,
            "ALTER TABLE missing ADD PRIMARY KEY (a)"
        )),
        "42P01"
    );

    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&owner, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(
            &owner,
            &mut session,
            &mut txn,
            "ALTER TABLE docs ADD PRIMARY KEY (a)",
        )
        .expect_err("explicit transaction must reject DDL");
    assert_eq!(err.wire_code(), "0A000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
    core.execute_sql_in_txn(&owner, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    insert(&core, &mut session, "owner", 1, None, "y", "op-1").expect("no NOT NULL was added");
}

// --- DROP -----------------------------------------------------------------

/// DROP は擬似名で成功し、以後は重複 INSERT が成功する（世代 bump で即時反映）。
/// NOT NULL は残る。再 DROP と PK 未宣言表での DROP は `42704`。
#[test]
fn drop_primary_key_by_pseudo_name() {
    let (core, path, mut session) = setup("alter-pk-drop");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD PRIMARY KEY (a)",
    )
    .expect("add");
    insert(&core, &mut session, "owner", 1, Some("x"), "y", "op-1").expect("row");
    assert_eq!(
        code(insert(
            &core,
            &mut session,
            "owner",
            2,
            Some("x"),
            "z",
            "op-2"
        )),
        "23505"
    );

    let outcome = exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs DROP CONSTRAINT docs_pkey",
    )
    .expect("drop primary key");
    match outcome {
        SqlOutcome::AlterTable(o) => assert_eq!(
            o.action,
            AlterTableAction::DropConstraint {
                constraint_name: "docs_pkey".to_string()
            }
        ),
        other => panic!("unexpected outcome: {other:?}"),
    }
    insert(&core, &mut session, "owner", 2, Some("x"), "z", "op-2b")
        .expect("duplicate is allowed after drop");
    assert_eq!(
        code(insert(&core, &mut session, "owner", 3, None, "z", "op-3")),
        "23502",
        "NOT NULL remains after dropping the primary key"
    );

    assert_eq!(
        code(exec(
            &core,
            &mut session,
            &owner,
            "ALTER TABLE docs DROP CONSTRAINT docs_pkey"
        )),
        "42704"
    );
    exec(&core, &mut session, &owner, "CREATE TABLE plain (a TEXT)").expect("create");
    assert_eq!(
        code(exec(
            &core,
            &mut session,
            &owner,
            "ALTER TABLE plain DROP CONSTRAINT plain_pkey"
        )),
        "42704"
    );
}

/// FK が参照する PK の DROP は `2BP01`（列リスト省略・明示列・自己参照・UNIQUE 併存）。
/// FK を落とした後は PK を DROP できる。
#[test]
fn drop_primary_key_referenced_by_foreign_key_is_2bp01() {
    let (core, path, mut session) = setup("alter-pk-drop-fk");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    exec(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE parents (code TEXT, other TEXT)",
    )
    .expect("create parents");
    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE parents ADD PRIMARY KEY (code)",
    )
    .expect("add pk");
    exec(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE kids (p TEXT, CONSTRAINT kid_fk FOREIGN KEY (p) REFERENCES parents)",
    )
    .expect("create kids referencing pk implicitly");
    assert_eq!(
        code(exec(
            &core,
            &mut session,
            &owner,
            "ALTER TABLE parents DROP CONSTRAINT parents_pkey"
        )),
        "2BP01"
    );

    // 同じ集合を UNIQUE が覆っていても救済しない。
    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE parents ADD CONSTRAINT uq_code UNIQUE (code)",
    )
    .expect("add covering unique");
    assert_eq!(
        code(exec(
            &core,
            &mut session,
            &owner,
            "ALTER TABLE parents DROP CONSTRAINT parents_pkey"
        )),
        "2BP01"
    );

    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE kids DROP CONSTRAINT kid_fk",
    )
    .expect("drop fk");
    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE parents DROP CONSTRAINT parents_pkey",
    )
    .expect("drop pk after fk is gone");

    // 明示列リストの FK。
    exec(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE p2 (code TEXT, other TEXT, PRIMARY KEY (code))",
    )
    .expect("create p2");
    exec(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE k2 (p TEXT REFERENCES p2 (code))",
    )
    .expect("create k2");
    assert_eq!(
        code(exec(
            &core,
            &mut session,
            &owner,
            "ALTER TABLE p2 DROP CONSTRAINT p2_pkey"
        )),
        "2BP01"
    );
}

/// 自己参照 FK が PK を参照していれば DROP は `2BP01`。
#[test]
fn drop_primary_key_referenced_by_self_reference_is_2bp01() {
    let (core, path, mut session) = setup("alter-pk-drop-self-fk");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    exec(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE tree (code TEXT, parent TEXT, PRIMARY KEY (code), \
         FOREIGN KEY (parent) REFERENCES tree (code))",
    )
    .expect("create self-referencing table");
    assert_eq!(
        code(exec(
            &core,
            &mut session,
            &owner,
            "ALTER TABLE tree DROP CONSTRAINT tree_pkey"
        )),
        "2BP01"
    );
}

/// ADD → DROP → ADD（別の列）で、永続一意索引の古いエントリが誤って衝突させない。
#[test]
fn add_drop_add_with_different_columns_has_no_stale_index() {
    let (core, path, mut session) = setup("alter-pk-add-drop-add");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD PRIMARY KEY (a)",
    )
    .expect("add on a");
    insert(&core, &mut session, "owner", 1, Some("x"), "p", "op-1").expect("row 1");
    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs DROP CONSTRAINT docs_pkey",
    )
    .expect("drop");
    insert(&core, &mut session, "owner", 2, Some("x"), "q", "op-2").expect("row 2 shares a");
    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD PRIMARY KEY (b)",
    )
    .expect("add on b");
    assert_eq!(
        code(insert(
            &core,
            &mut session,
            "owner",
            3,
            Some("y"),
            "p",
            "op-3"
        )),
        "23505"
    );
    insert(&core, &mut session, "owner", 3, Some("x"), "r", "op-4")
        .expect("a is no longer unique, b is distinct");
}

/// ADD 後に `Storage` を開き直しても PK が維持される。名前付き UNIQUE と併存しても往復できる。
#[test]
fn primary_key_survives_reopen() {
    let (core, path, mut session) = setup("alter-pk-reopen");
    let _guard = CleanupGuard(path.clone());
    let owner = ctx("owner");
    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD CONSTRAINT uq_b UNIQUE (b)",
    )
    .expect("named unique");
    exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD PRIMARY KEY (a)",
    )
    .expect("add pk");
    drop(session);
    drop(core);

    let storage = Storage::open(&path).expect("reopen");
    let schema = storage.get_table_schema("docs").expect("schema");
    assert_eq!(schema.primary_key(), Some(&["a".to_string()][..]));
    assert_eq!(schema.unique_constraints().len(), 1);
}

// --- 名前付き主キー・重複宣言（Issue #1364）---------------------------------

fn run(core: &EngineCore, session: &mut SessionState, sql: &str) {
    exec(core, session, &ctx("owner"), sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn fail(core: &EngineCore, session: &mut SessionState, sql: &str) -> String {
    code(exec(core, session, &ctx("owner"), sql))
}

/// 名前付き PK が受理され応答に明示名が載り、テナント内の一意性が強制される。
/// 導出名での DROP は `42704`、明示名での DROP は成功し以後は重複を許す。
#[test]
fn named_primary_key_is_accepted_and_dropped_by_explicit_name() {
    let (core, path, mut session) = setup("alter-pk-named");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let outcome = exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE docs ADD CONSTRAINT pk_docs PRIMARY KEY (a)",
    )
    .expect("named primary key");
    match outcome {
        SqlOutcome::AlterTable(o) => assert_eq!(
            o.action,
            AlterTableAction::AddConstraint {
                constraint_name: "pk_docs".to_string()
            }
        ),
        other => panic!("unexpected outcome: {other:?}"),
    }
    insert(&core, &mut session, "owner", 1, Some("x"), "y", "op-1").expect("first");
    assert_eq!(
        code(insert(
            &core,
            &mut session,
            "owner",
            2,
            Some("x"),
            "z",
            "op-2"
        )),
        "23505"
    );
    assert_eq!(
        fail(
            &core,
            &mut session,
            "ALTER TABLE docs DROP CONSTRAINT docs_pkey"
        ),
        "42704"
    );
    run(
        &core,
        &mut session,
        "ALTER TABLE docs DROP CONSTRAINT pk_docs",
    );
    insert(&core, &mut session, "owner", 3, Some("x"), "z", "op-3").expect("pk dropped");
    // 削除後は同じ名前を再利用できる（名前だけが残らない）。
    run(
        &core,
        &mut session,
        "ALTER TABLE docs ADD CONSTRAINT pk_docs UNIQUE (b)",
    );
}

/// 名前空間は UNIQUE・CHECK・FK と共有される。導出名は PK の明示名がある間は解放される。
#[test]
fn named_primary_key_shares_constraint_namespace() {
    let (core, path, mut session) = setup("alter-pk-named-ns");
    let _guard = CleanupGuard(path);
    run(
        &core,
        &mut session,
        "ALTER TABLE docs ADD CONSTRAINT pk_docs PRIMARY KEY (a)",
    );
    assert_eq!(
        fail(
            &core,
            &mut session,
            "ALTER TABLE docs ADD CONSTRAINT pk_docs UNIQUE (b)"
        ),
        "42P07"
    );
    assert_eq!(
        fail(
            &core,
            &mut session,
            "ALTER TABLE docs ADD CONSTRAINT pk_docs CHECK (b = 'z')"
        ),
        "42710"
    );
    run(
        &core,
        &mut session,
        "ALTER TABLE docs ADD CONSTRAINT docs_pkey UNIQUE (b)",
    );
}

/// 明示名が既存の UNIQUE 名と衝突する PK は `42P07` で、スキーマは変わらない。
#[test]
fn named_primary_key_colliding_with_existing_constraint_is_42p07() {
    let (core, path, mut session) = setup("alter-pk-named-collide");
    let _guard = CleanupGuard(path);
    run(
        &core,
        &mut session,
        "ALTER TABLE docs ADD CONSTRAINT uq_b UNIQUE (b)",
    );
    assert_eq!(
        fail(
            &core,
            &mut session,
            "ALTER TABLE docs ADD CONSTRAINT uq_b PRIMARY KEY (a)"
        ),
        "42P07"
    );
    // PK は付いていないので、無名の PK はまだ追加できる。
    run(&core, &mut session, "ALTER TABLE docs ADD PRIMARY KEY (a)");
}

/// 明示名が UNIQUE の既定名と同じでも、後続の ADD UNIQUE は別の導出名へ回避し、
/// DROP は明示名の PK だけを消す。
#[test]
fn named_primary_key_makes_default_unique_name_avoid_it() {
    let (core, path, mut session) = setup("alter-pk-named-avoid");
    let _guard = CleanupGuard(path.clone());
    run(
        &core,
        &mut session,
        "ALTER TABLE docs ADD CONSTRAINT docs_b_key PRIMARY KEY (a)",
    );
    run(&core, &mut session, "ALTER TABLE docs ADD UNIQUE (b)");
    run(
        &core,
        &mut session,
        "ALTER TABLE docs DROP CONSTRAINT docs_b_key",
    );
    drop(session);
    drop(core);
    let storage = Storage::open(&path).expect("reopen");
    let schema = storage.get_table_schema("docs").expect("schema");
    assert!(schema.primary_key().is_none());
    assert_eq!(schema.unique_constraints().len(), 1);
    assert_ne!(schema.unique_constraints()[0].name(), "docs_b_key");
}

/// 主キーの重複宣言（無名・名前付きとも）は `42P16`。名前衝突より優先され、
/// スキーマは変わらない（副作用ゼロ）。
#[test]
fn duplicate_primary_key_declaration_is_42p16_without_side_effects() {
    let (core, path, mut session) = setup("alter-pk-dup");
    let _guard = CleanupGuard(path.clone());
    run(
        &core,
        &mut session,
        "ALTER TABLE docs ADD CONSTRAINT pk_docs PRIMARY KEY (a)",
    );
    let snapshot = |p: &std::path::Path| {
        let storage = Storage::open(p);
        storage
            .ok()
            .map(|s| s.get_table_schema("docs").expect("schema"))
    };
    for sql in [
        "ALTER TABLE docs ADD PRIMARY KEY (b)",
        "ALTER TABLE docs ADD CONSTRAINT other PRIMARY KEY (b)",
        "ALTER TABLE docs ADD CONSTRAINT pk_docs PRIMARY KEY (b)",
    ] {
        assert_eq!(fail(&core, &mut session, sql), "42P16", "{sql}");
    }
    // 開き直し前後の一致で副作用ゼロを確認する。
    drop(session);
    drop(core);
    let first = snapshot(&path).expect("reopen");
    assert_eq!(first.primary_key(), Some(&["a".to_string()][..]));
    assert_eq!(
        first.primary_key_constraint_name_effective().as_deref(),
        Some("pk_docs")
    );
}

/// 導出名と同じ明示名の PK は無名と同じ扱い（導出名で DROP できる）。
#[test]
fn named_primary_key_equal_to_derived_name_behaves_as_unnamed() {
    let (core, path, mut session) = setup("alter-pk-named-derived");
    let _guard = CleanupGuard(path);
    run(
        &core,
        &mut session,
        "ALTER TABLE docs ADD CONSTRAINT docs_pkey PRIMARY KEY (a)",
    );
    run(
        &core,
        &mut session,
        "ALTER TABLE docs DROP CONSTRAINT docs_pkey",
    );
}

/// FK が参照する名前付き PK の DROP は `2BP01`。
#[test]
fn drop_named_primary_key_referenced_by_foreign_key_is_2bp01() {
    let (core, path, mut session) = setup("alter-pk-named-fk");
    let _guard = CleanupGuard(path);
    run(
        &core,
        &mut session,
        "CREATE TABLE parents (code TEXT, other TEXT)",
    );
    run(
        &core,
        &mut session,
        "ALTER TABLE parents ADD CONSTRAINT pk_parents PRIMARY KEY (code)",
    );
    run(
        &core,
        &mut session,
        "CREATE TABLE kids (p TEXT, CONSTRAINT kid_fk FOREIGN KEY (p) REFERENCES parents)",
    );
    assert_eq!(
        fail(
            &core,
            &mut session,
            "ALTER TABLE parents DROP CONSTRAINT pk_parents"
        ),
        "2BP01"
    );
    // 親表の DROP TABLE も 2BP01（v13 が FK 保持版として登録されている）。
    assert_eq!(fail(&core, &mut session, "DROP TABLE parents"), "2BP01");
}

/// 名前付き PK は名前付き UNIQUE と併存しても再オープンで往復する。
#[test]
fn named_primary_key_survives_reopen() {
    let (core, path, mut session) = setup("alter-pk-named-reopen");
    let _guard = CleanupGuard(path.clone());
    run(
        &core,
        &mut session,
        "ALTER TABLE docs ADD CONSTRAINT uq_b UNIQUE (b)",
    );
    run(
        &core,
        &mut session,
        "ALTER TABLE docs ADD CONSTRAINT pk_docs PRIMARY KEY (a)",
    );
    drop(session);
    drop(core);
    let storage = Storage::open(&path).expect("reopen");
    let schema = storage.get_table_schema("docs").expect("schema");
    assert_eq!(schema.primary_key(), Some(&["a".to_string()][..]));
    assert_eq!(
        schema.primary_key_constraint_name_effective().as_deref(),
        Some("pk_docs")
    );
    assert_eq!(schema.unique_constraints().len(), 1);
    assert_eq!(schema.unique_constraints()[0].name(), "uq_b");
}

/// 名前付きの形でも DDL 権限ゲート（`42501`）が先に判定される。
#[test]
fn named_primary_key_respects_permission_gate() {
    let (core, path, _session) = setup("alter-pk-named-gate");
    let _guard = CleanupGuard(path);
    let mut plain = SessionState::default();
    assert_eq!(
        code(exec(
            &core,
            &mut plain,
            &ctx("owner"),
            "ALTER TABLE docs ADD CONSTRAINT pk_docs PRIMARY KEY (a)"
        )),
        "42501"
    );
}
