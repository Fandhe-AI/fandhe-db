//! `ALTER TABLE <table> ADD COLUMN <column> <type>`（TASK-202、対象ビヘイビア:
//! SQL-23）の結合テスト。ポインタ: `docs/spec/05-tasks.md` TASK-202・
//! `docs/spec/04-behavior/sql-surface.md` SQL-23・TABLE-5（O(1) 列追加・
//! 既存行のバイト列不変）。関連ポインタ: RLS-7（暗黙のテナント境界適用）・
//! RLS-9（他テナント存在情報の非漏えい）・ERR-2/ERR-4/ERR-6。
//!
//! `EngineCore::execute_sql_in_session`（先頭トークン `ALTER` の覗き見判定 →
//! `sql::allowlist::validate_alter_table_tokens` → `sql::ddl::
//! require_ddl_permission` → `sql::ddl::execute_alter_table_add_column`）を
//! production 経路として検証する（`truncate_table.rs`・`sql_update_single_row.rs`
//! と同じ流儀。実 `Storage` ＋ `CpuScalarProvider`、`unique_db_path`／
//! `CleanupGuard`）。
//!
//! `EngineCore::from_storage` は `Storage` の所有権を奪うため（テスト用の
//! アクセサは存在しない）、列追加の成否は `catalog::Storage::get_table_schema`
//! による直接検査ではなく、SQL 経由の観測可能な効果（列参照の成否・値の
//! 往復・「未知の列」エラー〔`22000`〕の有無）で検証する。

use engine::catalog::{ColumnDef, ColumnType};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{RowInput, Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema(name: &str) -> engine::catalog::TableSchema {
    engine::catalog::TableSchema::new(
        name,
        vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
    )
}

fn new_core_with_table() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql-ddl-add-column");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema(TABLE)).expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

/// `create_enum_type` は `Storage::open` 直後（`EngineCore::from_storage` へ
/// 所有権を渡す前）にしか呼べない（モジュールドキュメント参照）。
fn new_core_with_table_and_enum(
    type_name: &str,
    labels: Vec<String>,
) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql-ddl-add-column-enum");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema(TABLE)).expect("create table");
    storage
        .create_enum_type(type_name, labels)
        .expect("create_enum_type");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn op_id(label: &str) -> OperationId {
    OperationId::parse(label).expect("valid operation id")
}

fn insert_row(core: &EngineCore, ctx: &PolicyContext, id: u64, seq: u64) {
    core.insert_row(
        ctx,
        TABLE,
        id,
        &RowInput {
            tenant_id: ctx.tenant_id(),
            visibility: Visibility::Public,
            embedding: &[0.1f32, 0.2f32],
            metadata: &[],
        },
        Some(&op_id(&format!("seed-{id}-{seq}"))),
    )
    .expect("insert row");
}

/// DDL 権限を持つセッション（`sql::ddl::require_ddl_permission` が `Ok` を返す
/// 唯一の作り方。`CREATE TABLE`／`DROP TABLE` と共有する wire-server の
/// `--ddl-allowed-users` 相当〔認証成功後の `SessionState::allow_ddl`〕を engine
/// 層 API から直接シミュレートする）。
fn ddl_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn alter_table(
    core: &EngineCore,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx("owner"), session, sql)
}

fn count_star(core: &EngineCore, ctx: &PolicyContext, table: &str) -> u64 {
    let result = core
        .execute_sql(ctx, &format!("SELECT COUNT(*) FROM {table}"))
        .expect("count(*) should succeed");
    assert_eq!(result.rows.len(), 1);
    match &result.rows[0].cells[0] {
        Cell::Integer(v) => *v,
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
}

/// `column` が現在のスキーマに存在するかを、SQL 経由の「未知の列」エラー
/// （`22000`）の有無で判定する（直接のスキーマ検査 API はテストから使えない
/// ため。モジュールドキュメント参照）。
fn column_exists(core: &EngineCore, ctx: &PolicyContext, table: &str, column: &str) -> bool {
    match core.execute_sql(ctx, &format!("SELECT {column} FROM {table} LIMIT 1")) {
        Ok(_) => true,
        Err(e) if e.wire_code() == "22000" => false,
        Err(e) => panic!("unexpected error while probing column {column:?}: {e}"),
    }
}

// --- 成功系: 各スカラー型 ------------------------------------------------

#[test]
fn accepts_all_scalar_types_and_columns_become_queryable() {
    for (i, (name, sql_type)) in [
        ("c_text", "TEXT"),
        ("c_int", "INTEGER"),
        ("c_bigint", "BIGINT"),
        ("c_real", "REAL"),
        ("c_double", "DOUBLE PRECISION"),
        ("c_bool", "BOOLEAN"),
        ("c_date", "DATE"),
        ("c_ts", "TIMESTAMP"),
        ("c_bytea", "BYTEA"),
        ("c_json", "JSON"),
        ("c_jsonb", "JSONB"),
        ("c_uuid", "UUID"),
        ("c_numeric", "NUMERIC(5,2)"),
    ]
    .into_iter()
    .enumerate()
    {
        let (core, path) = new_core_with_table();
        let _guard = CleanupGuard(path);
        let owner = ctx("owner");
        assert!(
            !column_exists(&core, &owner, TABLE, name),
            "case {i}: column {name} must not exist before ALTER TABLE"
        );

        let mut session = ddl_session();
        let outcome = alter_table(
            &core,
            &mut session,
            &format!("ALTER TABLE {TABLE} ADD COLUMN {name} {sql_type}"),
        )
        .unwrap_or_else(|e| panic!("case {i} ({sql_type}) rejected: {e}"));
        assert!(matches!(outcome, SqlOutcome::AlterTable(_)));

        assert!(
            column_exists(&core, &owner, TABLE, name),
            "case {i}: column {name} must exist after ALTER TABLE"
        );
    }
}

#[test]
fn accepts_enum_column_registered_before_alter() {
    let (core, path) =
        new_core_with_table_and_enum("mood", vec!["happy".to_string(), "sad".to_string()]);
    let _guard = CleanupGuard(path);

    let mut session = ddl_session();
    let outcome = alter_table(
        &core,
        &mut session,
        &format!("ALTER TABLE {TABLE} ADD COLUMN feeling mood"),
    )
    .expect("ALTER TABLE with ENUM type should succeed");
    assert!(matches!(outcome, SqlOutcome::AlterTable(_)));
    assert!(column_exists(&core, &ctx("owner"), TABLE, "feeling"));
}

// --- 既存行が NULL として読める -----------------------------------------

#[test]
fn existing_rows_read_added_column_as_null_across_read_paths() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    insert_row(&core, &owner, 1, 1);

    let mut session = ddl_session();
    alter_table(
        &core,
        &mut session,
        &format!("ALTER TABLE {TABLE} ADD COLUMN note TEXT"),
    )
    .expect("ALTER TABLE should succeed");

    // SELECT（KNN 経路）: 追加列は NULL のまま。
    let result = core
        .execute_sql(
            &owner,
            &format!("SELECT note FROM {TABLE} ORDER BY embedding <=> '[0.1,0.2]' LIMIT 10"),
        )
        .expect("select should succeed");
    assert_eq!(result.rows.len(), 1);
    assert!(matches!(result.rows[0].cells[0], Cell::Null));

    // WHERE note = '...' は 0 件。
    let result = core
        .execute_sql(
            &owner,
            &format!("SELECT id FROM {TABLE} WHERE note = 'x' LIMIT 10"),
        )
        .expect("select where should succeed");
    assert_eq!(result.rows.len(), 0);

    // COUNT(note) は NULL を数えないため 0。
    let result = core
        .execute_sql(&owner, &format!("SELECT COUNT(note) FROM {TABLE}"))
        .expect("count(note) should succeed");
    match &result.rows[0].cells[0] {
        Cell::Integer(v) => assert_eq!(*v, 0),
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
}

#[test]
fn newly_inserted_rows_can_populate_the_added_column_alongside_existing_rows() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    insert_row(&core, &owner, 1, 1);

    let mut session = ddl_session();
    alter_table(
        &core,
        &mut session,
        &format!("ALTER TABLE {TABLE} ADD COLUMN note TEXT"),
    )
    .expect("ALTER TABLE should succeed");

    core.execute_sql_in_session(
        &owner,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {TABLE} (id, embedding, note) VALUES (2, '[0.3,0.4]', 'hello') USING OPERATION_ID 'op-2'"
        ),
    )
    .expect("insert with new column should succeed");

    assert_eq!(count_star(&core, &owner, TABLE), 2);
    let result = core
        .execute_sql(
            &owner,
            &format!("SELECT id FROM {TABLE} WHERE note = 'hello' LIMIT 10"),
        )
        .expect("select where should succeed");
    assert_eq!(result.rows.len(), 1);
}

// --- 世代進行・キャッシュ失効 --------------------------------------------

#[test]
fn added_column_is_visible_after_cache_was_warmed_by_prior_queries() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    insert_row(&core, &owner, 1, 1);

    // クエリを 2 回実行してアリーナ／可視ビットマップキャッシュ等を温める。
    for _ in 0..2 {
        core.execute_sql(
            &owner,
            &format!("SELECT id FROM {TABLE} ORDER BY embedding <=> '[0.1,0.2]' LIMIT 10"),
        )
        .expect("warm-up select");
        count_star(&core, &owner, TABLE);
    }

    let mut session = ddl_session();
    alter_table(
        &core,
        &mut session,
        &format!("ALTER TABLE {TABLE} ADD COLUMN note TEXT"),
    )
    .expect("ALTER TABLE should succeed");

    // 同じクエリを再実行しても新しい列が見える（古いスキーマへ固着していない）。
    let result = core
        .execute_sql(
            &owner,
            &format!("SELECT note FROM {TABLE} ORDER BY embedding <=> '[0.1,0.2]' LIMIT 10"),
        )
        .expect("select after ALTER should succeed");
    assert_eq!(result.rows.len(), 1);
    assert!(matches!(result.rows[0].cells[0], Cell::Null));
}

// --- 権限 ------------------------------------------------------------------

#[test]
fn default_session_without_ddl_privilege_is_rejected_with_42501() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let mut session = SessionState::default();
    let err = alter_table(
        &core,
        &mut session,
        &format!("ALTER TABLE {TABLE} ADD COLUMN note TEXT"),
    )
    .expect_err("must be rejected without DDL privilege");
    assert_eq!(err.wire_code(), "42501");
    assert!(matches!(err, SqlSurfaceError::InsufficientPrivilege));
}

/// 権限ゲートはカタログ照会（テーブル・列の存在確認）より必ず先に判定する
/// ——権限の無い主体へ「テーブルが存在しない」「列が重複している」等の
/// 存在情報を一切返さない（security.md P0）。
#[test]
fn permission_denial_precedes_any_catalog_lookup() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let mut session = SessionState::default();

    for sql in [
        "ALTER TABLE nonexistent_table ADD COLUMN note TEXT".to_string(),
        format!("ALTER TABLE {TABLE} ADD COLUMN embedding TEXT"),
        format!("ALTER TABLE {TABLE} ADD COLUMN v VECTOR(4)"),
    ] {
        let err = alter_table(&core, &mut session, &sql)
            .expect_err("must be rejected without DDL privilege regardless of target validity");
        assert_eq!(
            err.wire_code(),
            "42501",
            "expected permission denial to take precedence for {sql:?}, got {err:?}"
        );
    }
}

#[test]
fn permission_denial_leaves_target_column_unadded() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let mut session = SessionState::default();
    let owner = ctx("owner");

    let _ = alter_table(
        &core,
        &mut session,
        &format!("ALTER TABLE {TABLE} ADD COLUMN note TEXT"),
    );

    assert!(
        !column_exists(&core, &owner, TABLE, "note"),
        "rejected ALTER TABLE must not add the column"
    );
}

// --- エラー契約 --------------------------------------------------------

#[test]
fn undefined_table_is_rejected_with_42p01() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    let err = alter_table(
        &core,
        &mut session,
        "ALTER TABLE nonexistent_table ADD COLUMN note TEXT",
    )
    .expect_err("must reject undefined table");
    assert_eq!(err.wire_code(), "42P01");
}

#[test]
fn duplicate_column_is_rejected_with_42701() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    let err = alter_table(
        &core,
        &mut session,
        &format!("ALTER TABLE {TABLE} ADD COLUMN embedding TEXT"),
    )
    .expect_err("must reject duplicate column name");
    assert_eq!(err.wire_code(), "42701");
}

#[test]
fn column_count_limit_is_rejected_with_54000_and_has_no_side_effect() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    let owner = ctx("owner");

    // 既存 1 列（embedding）に 255 列を足して上限（256）ちょうどにする。
    for i in 0..255 {
        alter_table(
            &core,
            &mut session,
            &format!("ALTER TABLE {TABLE} ADD COLUMN c{i} TEXT"),
        )
        .unwrap_or_else(|e| panic!("column {i} unexpectedly rejected: {e}"));
    }

    let err = alter_table(
        &core,
        &mut session,
        &format!("ALTER TABLE {TABLE} ADD COLUMN one_too_many TEXT"),
    )
    .expect_err("257th column must be rejected");
    assert_eq!(err.wire_code(), "54000");

    assert!(
        !column_exists(&core, &owner, TABLE, "one_too_many"),
        "rejected ALTER TABLE must not add the column"
    );
}

#[test]
fn vector_column_is_rejected_with_0a000() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    let err = alter_table(
        &core,
        &mut session,
        &format!("ALTER TABLE {TABLE} ADD COLUMN v VECTOR(4)"),
    )
    .expect_err("VECTOR column must be rejected");
    assert_eq!(err.wire_code(), "0A000");
}

#[test]
fn malformed_syntax_variants_are_rejected_with_42601() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();

    for sql in [
        format!("ALTER TABLE {TABLE} ADD note TEXT"),
        format!("ALTER TABLE {TABLE} ADD COLUMN IF NOT EXISTS note TEXT"),
        format!("ALTER TABLE {TABLE} ADD COLUMN note TEXT, ADD COLUMN note2 TEXT"),
        format!("ALTER TABLE {TABLE} ADD COLUMN note TEXT UNIQUE"),
        format!("ALTER TABLE {TABLE} ADD COLUMN note TEXT PRIMARY KEY"),
        format!("ALTER TABLE {TABLE} ADD COLUMN note TEXT DEFAULT NULL"),
        format!("ALTER TABLE {TABLE} ADD COLUMN note TEXT NOT NULL NOT NULL"),
        format!("ALTER TABLE {TABLE} ADD COLUMN note TEXT DEFAULT 'a' DEFAULT 'b'"),
        format!("ALTER TABLE {TABLE} ADD COLUMN note TEXT DEFAULT 1"),
        format!("ALTER TABLE {TABLE} ADD COLUMN note INTEGER DEFAULT 'x'"),
        format!("ALTER TABLE {TABLE} ADD COLUMN note BOOLEAN DEFAULT 1"),
        // `DROP COLUMN embedding` は VECTOR 列保護（Issue #1167 以降）で 42601。
        // `ALTER COLUMN ... TYPE` の契約は `sql_ddl_drop_alter_column.rs` が担う。
        format!("ALTER TABLE {TABLE} DROP COLUMN embedding"),
        format!("ALTER TABLE {TABLE} ADD COLUMN note TEXT USING OPERATION_ID 'op-1'"),
        format!("ALTER TABLE {TABLE} ADD COLUMN note TEXT RETURNING id"),
        format!("ALTER TABLE {TABLE} ADD COLUMN note NUMERIC(0,0)"),
        format!("ALTER TABLE {TABLE} ADD COLUMN note NUMERIC(39,0)"),
        format!("ALTER TABLE {TABLE} ADD COLUMN note NUMERIC(5,6)"),
        format!("ALTER TABLE {TABLE} ADD COLUMN note DOUBLE"),
        format!("ALTER TABLE {TABLE} ADD COLUMN note VECTOR()"),
        format!("ALTER TABLE {TABLE} ADD COLUMN note TEXT; SELECT 1"),
    ] {
        let err = alter_table(&core, &mut session, &sql)
            .expect_err(&format!("expected {sql:?} to be rejected"));
        assert_eq!(
            err.wire_code(),
            "42601",
            "unexpected wire_code for {sql:?}: {err:?}"
        );
    }
}

/// 予約列名（`id`／`tenant_id`／`visibility`。大文字小文字を問わない）は
/// `CREATE TABLE` と同じく構造検証段階で `42601` 拒否する（疑似列・RLS 内部列を
/// 隠蔽する列を DDL で作らせない。fail-closed）。
#[test]
fn reserved_column_names_are_rejected_with_42601() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();

    for name in [
        "id",
        "tenant_id",
        "visibility",
        "ID",
        "Tenant_Id",
        "VISIBILITY",
        // `CREATE TABLE` と揃えた予約列名（Issue #906。`CHECK` 制約構文との
        // 曖昧さ排除のため）。
        "check",
        "Constraint",
    ] {
        let sql = format!("ALTER TABLE {TABLE} ADD COLUMN {name} TEXT");
        let err = alter_table(&core, &mut session, &sql)
            .expect_err(&format!("expected {sql:?} to be rejected"));
        assert_eq!(
            err.wire_code(),
            "42601",
            "unexpected wire_code for {sql:?}: {err:?}"
        );
    }
    // 予約列名の拒否は後続の正当な列追加を妨げない（セッション状態を汚さない）。
    assert!(matches!(
        alter_table(
            &core,
            &mut session,
            &format!("ALTER TABLE {TABLE} ADD COLUMN note TEXT")
        ),
        Ok(SqlOutcome::AlterTable(_))
    ));
}

/// 対象テーブルの存在確認は型名解決より先に行う（PR #1052 codex P1 指摘の
/// 回帰テスト）。存在しないテーブルへの要求は、型名が未登録 ENUM・`VECTOR`・
/// 構文上は妥当なスカラー型のいずれであっても一律 `42P01` になる。
#[test]
fn undefined_table_takes_precedence_over_type_name_resolution() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();

    for sql in [
        "ALTER TABLE nonexistent_table ADD COLUMN feeling unregistered_mood",
        "ALTER TABLE nonexistent_table ADD COLUMN v VECTOR(4)",
        "ALTER TABLE nonexistent_table ADD COLUMN note NUMERIC(39,0)",
        "ALTER TABLE nonexistent_table ADD COLUMN note TEXT",
    ] {
        let err = alter_table(&core, &mut session, sql)
            .expect_err(&format!("expected {sql:?} to be rejected"));
        assert_eq!(
            err.wire_code(),
            "42P01",
            "unexpected wire_code for {sql:?}: {err:?}"
        );
    }
}

/// 対象がビュー（Issue #909）の場合は `CREATE VIEW` の書き込み系方針と同じく
/// `42809`（型名の正誤・`VECTOR` 指定に関わらず）。ビューもベーステーブルも
/// 変更されない。
#[test]
fn alter_table_on_a_view_is_rejected_with_42809() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    core.execute_sql_in_session(
        &ctx("owner"),
        &mut session,
        &format!("CREATE VIEW v_docs AS SELECT * FROM {TABLE}"),
    )
    .expect("create view");

    for sql in [
        "ALTER TABLE v_docs ADD COLUMN note TEXT",
        "ALTER TABLE v_docs ADD COLUMN feeling unregistered_mood",
        "ALTER TABLE v_docs ADD COLUMN v VECTOR(4)",
    ] {
        let err = alter_table(&core, &mut session, sql)
            .expect_err(&format!("expected {sql:?} to be rejected"));
        assert_eq!(
            err.wire_code(),
            "42809",
            "unexpected wire_code for {sql:?}: {err:?}"
        );
    }
    assert!(
        !column_exists(&core, &ctx("owner"), TABLE, "note"),
        "ALTER TABLE on a view must not alter its base table"
    );
}

/// DDL 権限の無いセッションでは、ビュー名を指定しても `42501` のみ（ビューの
/// 存在も権限ゲートより前には観測できない）。
#[test]
fn permission_denial_precedes_view_detection() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let mut admin = ddl_session();
    core.execute_sql_in_session(
        &ctx("owner"),
        &mut admin,
        &format!("CREATE VIEW v_docs AS SELECT * FROM {TABLE}"),
    )
    .expect("create view");

    let mut session = SessionState::default();
    let err = alter_table(
        &core,
        &mut session,
        "ALTER TABLE v_docs ADD COLUMN note TEXT",
    )
    .expect_err("unprivileged ALTER TABLE must be rejected");
    assert_eq!(err.wire_code(), "42501");
}

#[test]
fn unregistered_enum_type_name_is_rejected_with_42601() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    let err = alter_table(
        &core,
        &mut session,
        &format!("ALTER TABLE {TABLE} ADD COLUMN feeling unregistered_mood"),
    )
    .expect_err("unregistered ENUM type name must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

// --- EXPLAIN との相互排他 -------------------------------------------------

#[test]
fn explain_prefix_before_alter_table_is_rejected_with_42601() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    let err = alter_table(
        &core,
        &mut session,
        &format!("EXPLAIN ALTER TABLE {TABLE} ADD COLUMN note TEXT"),
    )
    .expect_err("EXPLAIN + ALTER TABLE must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

// --- extended query: $n パラメータは構造上どこにも許可されない -------------

#[test]
fn dollar_parameter_anywhere_in_alter_table_is_rejected_at_parse() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let err = core
        .parse_sql_prepared(&format!("ALTER TABLE {TABLE} ADD COLUMN note $1"))
        .expect_err("$n in ALTER TABLE must be rejected at Parse");
    assert_eq!(err.wire_code(), "42601");
}

// --- 明示トランザクション内の DDL（SQL-31・TASK-221 の既存方針を継承） -----

/// 明示トランザクション内の `ALTER TABLE ADD COLUMN` は、`CREATE TABLE`／
/// `DROP TABLE` と同じく DDL 権限の有無に関わらず `0A000` で拒否され、
/// トランザクションは `Failed` へ遷移し、列は追加されない。
#[test]
fn alter_table_inside_explicit_transaction_is_rejected_with_0a000() {
    use engine::sql::transaction::TransactionStatus;

    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let caller = ctx("owner");
    let mut session = ddl_session();
    let mut txn = core.new_session_transaction();

    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            &format!("ALTER TABLE {TABLE} ADD COLUMN note TEXT"),
        )
        .expect_err("ALTER TABLE inside an explicit transaction must be rejected");
    assert_eq!(err.wire_code(), "0A000");
    assert_eq!(txn.status(), TransactionStatus::Failed);

    assert_eq!(
        core.execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
            .expect("rollback"),
        SqlOutcome::Rollback
    );
    assert_eq!(txn.status(), TransactionStatus::Idle);
    assert!(
        !column_exists(&core, &caller, TABLE, "note"),
        "a rejected in-transaction ALTER TABLE must not add the column"
    );
}

// --- RLS: DDL はテナント境界・行可視性に影響しない -------------------------

#[test]
fn alter_table_by_one_tenant_does_not_change_other_tenants_row_visibility() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let bob = ctx("bob");
    insert_row(&core, &alice, 1, 1);
    insert_row(&core, &bob, 2, 2);

    let mut session = ddl_session();
    alter_table(
        &core,
        &mut session,
        &format!("ALTER TABLE {TABLE} ADD COLUMN note TEXT"),
    )
    .expect("ALTER TABLE should succeed");

    // bob からは自分の 1 行 + alice の Public 行が見える（クロステナント Public
    // 可視の既存契約）。DDL 自体が可視性を変えていないことのみを確認する。
    assert_eq!(count_star(&core, &bob, TABLE), 2);
    assert_eq!(count_star(&core, &alice, TABLE), 2);

    let result = core
        .execute_sql(
            &bob,
            &format!("SELECT note FROM {TABLE} WHERE id = 2 LIMIT 10"),
        )
        .expect("select should succeed");
    assert_eq!(result.rows.len(), 1);
    assert!(matches!(result.rows[0].cells[0], Cell::Null));
}

// --- Describe（拡張クエリプロトコル） ---------------------------------------

#[test]
fn describe_alter_table_returns_no_result_columns() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let session = SessionState::default();
    let parsed = core
        .parse_sql(&format!("ALTER TABLE {TABLE} ADD COLUMN note TEXT"))
        .expect("parse should succeed regardless of DDL privilege");
    let described = core
        .describe_parsed_in_session(&session, &parsed)
        .expect("describe should succeed");
    assert!(described.is_none());
}
// --- NOT NULL／DEFAULT（Issue #1169。ポインタ: TABLE-5・TABLE-16・SQL-23） --------

fn one_cell(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Cell {
    let result = core.execute_sql(ctx, sql).expect("select should succeed");
    assert_eq!(result.rows.len(), 1, "{sql}");
    result.rows[0].cells[0].clone()
}

fn add_column(core: &EngineCore, decl: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = ddl_session();
    alter_table(
        core,
        &mut session,
        &format!("ALTER TABLE {TABLE} ADD COLUMN {decl}"),
    )
}

#[test]
fn default_fills_existing_rows_across_read_paths() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    insert_row(&core, &owner, 1, 1);

    add_column(&core, "note TEXT DEFAULT 'hi'").expect("ADD COLUMN DEFAULT");

    let knn = format!("SELECT note FROM {TABLE} ORDER BY embedding <=> '[0.1,0.2]' LIMIT 10");
    assert!(matches!(one_cell(&core, &owner, &knn), Cell::Text(ref t) if t == "hi"));
    let hit = core
        .execute_sql(
            &owner,
            &format!("SELECT id FROM {TABLE} WHERE note = 'hi' LIMIT 10"),
        )
        .expect("where");
    assert_eq!(hit.rows.len(), 1);
    assert!(matches!(
        one_cell(&core, &owner, &format!("SELECT COUNT(note) FROM {TABLE}")),
        Cell::Integer(1)
    ));
}

#[test]
fn not_null_default_fills_existing_rows_and_new_rows_use_default_on_omission() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    insert_row(&core, &owner, 1, 1);

    add_column(&core, "n INTEGER NOT NULL DEFAULT 7").expect("ADD COLUMN NOT NULL DEFAULT");
    assert!(matches!(
        one_cell(
            &core,
            &owner,
            &format!("SELECT n FROM {TABLE} WHERE id = 1 LIMIT 1")
        ),
        Cell::SignedInteger(7)
    ));

    let mut s = SessionState::default();
    core.execute_sql_in_session(
        &owner,
        &mut s,
        &format!(
            "INSERT INTO {TABLE} (id, embedding) VALUES (2, '[0.3,0.4]') USING OPERATION_ID 'op-2'"
        ),
    )
    .expect("insert omitting n");
    core.execute_sql_in_session(
        &owner,
        &mut s,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, n) VALUES (3, '[0.3,0.4]', 9) USING OPERATION_ID 'op-3'"
        ),
    )
    .expect("insert explicit n");
    assert!(matches!(
        one_cell(
            &core,
            &owner,
            &format!("SELECT n FROM {TABLE} WHERE id = 2 LIMIT 1")
        ),
        Cell::SignedInteger(7)
    ));
    assert!(matches!(
        one_cell(
            &core,
            &owner,
            &format!("SELECT n FROM {TABLE} WHERE id = 3 LIMIT 1")
        ),
        Cell::SignedInteger(9)
    ));
}

#[test]
fn default_values_are_correct_for_each_supported_type() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    insert_row(&core, &owner, 1, 1);

    for decl in [
        "b BOOLEAN NOT NULL DEFAULT true",
        "big BIGINT DEFAULT -5",
        "r REAL DEFAULT 1.5",
        "d DOUBLE PRECISION DEFAULT 2.5",
        "num NUMERIC(5,2) NOT NULL DEFAULT 3.14",
    ] {
        add_column(&core, decl).unwrap_or_else(|e| panic!("{decl}: {e}"));
    }
    let q = |col: &str| {
        one_cell(
            &core,
            &owner,
            &format!("SELECT {col} FROM {TABLE} WHERE id = 1 LIMIT 1"),
        )
    };
    assert!(matches!(q("b"), Cell::Bool(true)));
    assert!(matches!(q("big"), Cell::SignedInteger(-5)));
    assert!(matches!(q("r"), Cell::Float(v) if (v - 1.5).abs() < 1e-9));
    assert!(matches!(q("d"), Cell::Float(v) if (v - 2.5).abs() < 1e-9));
    let num = format!("{:?}", q("num"));
    assert!(
        num.contains("3.14") || num.contains("314"),
        "unexpected NUMERIC cell: {num}"
    );
}

/// `DEFAULT` を欠く `NOT NULL` の追加は、行の有無（自テナント・他テナント・空）に
/// かかわらず同一の `42601` で拒否される（TABLE-16）。他テナントの行の存在が DDL の
/// 成否から判別できないこと（テナント境界 P0）と、副作用ゼロを固定する。
#[test]
fn not_null_without_default_is_rejected_uniformly_regardless_of_rows() {
    // 空テーブル
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let empty_err = add_column(&core, "note TEXT NOT NULL").expect_err("empty: rejected");
    assert_eq!(empty_err.wire_code(), "42601");
    assert!(!column_exists(&core, &ctx("bob"), TABLE, "note"));

    // 他テナントの行だけがある場合
    insert_row(&core, &ctx("bob"), 1, 1);
    let other_err = add_column(&core, "note TEXT NOT NULL").expect_err("other tenant: rejected");
    assert_eq!(other_err.wire_code(), "42601");
    assert!(!column_exists(&core, &ctx("bob"), TABLE, "note"));

    // 自テナントの行もある場合
    insert_row(&core, &ctx("owner"), 2, 2);
    let own_err = add_column(&core, "note TEXT NOT NULL").expect_err("own rows: rejected");
    assert_eq!(own_err.wire_code(), "42601");
    assert!(!column_exists(&core, &ctx("owner"), TABLE, "note"));

    // 応答は行の有無で区別できない（同一の文言）。
    assert_eq!(empty_err.to_string(), other_err.to_string());
    assert_eq!(empty_err.to_string(), own_err.to_string());
}

#[test]
fn default_type_check_errors() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    for (decl, code) in [
        ("a INTEGER DEFAULT 99999999999", "22003"),
        ("b NUMERIC(3,1) DEFAULT 123.45", "22003"),
        ("c DATE DEFAULT 1", "42601"),
        ("c2 DATE DEFAULT 'abc'", "22007"),
        ("c3 DATE DEFAULT '2020-02-30'", "22008"),
        ("c4 DATE DEFAULT '2020-13-01'", "22008"),
        ("c5 TIMESTAMP DEFAULT 1", "42601"),
        ("c6 TIMESTAMP DEFAULT 'abc'", "22007"),
        ("c7 TIMESTAMP DEFAULT '2020-01-01'", "22007"),
        ("c8 TIMESTAMP DEFAULT '2020-02-30 00:00:00'", "22008"),
        ("c9 TIMESTAMP DEFAULT '2020-01-01 24:00:00'", "22008"),
        ("d UUID DEFAULT 1", "42601"),
        ("d2 UUID DEFAULT 'abc'", "22P02"),
        (
            "d3 UUID DEFAULT '00000000000000000000000000000001'",
            "22P02",
        ),
        (
            "d4 UUID DEFAULT '{00000000-0000-0000-0000-000000000001}'",
            "22P02",
        ),
        (
            "d5 UUID DEFAULT 'zzzzzzzz-0000-0000-0000-000000000001'",
            "22P02",
        ),
        ("e TEXT DEFAULT 1", "42601"),
        ("f BOOLEAN DEFAULT 'x'", "42601"),
    ] {
        let err = add_column(&core, decl).expect_err(decl);
        assert_eq!(err.wire_code(), code, "{decl}: {err:?}");
        let name = decl.split(' ').next().unwrap_or("");
        assert!(!column_exists(&core, &ctx("owner"), TABLE, name), "{decl}");
    }
}

#[test]
fn default_persists_across_reopen() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path.clone());
    let owner = ctx("owner");
    insert_row(&core, &owner, 1, 1);
    add_column(&core, "note TEXT NOT NULL DEFAULT 'kept'").expect("ADD COLUMN");
    drop(core);

    let storage = Storage::open(&path).expect("reopen");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    assert!(matches!(
        one_cell(
            &core,
            &owner,
            &format!("SELECT note FROM {TABLE} WHERE id = 1 LIMIT 1")
        ),
        Cell::Text(ref t) if t == "kept"
    ));
}

#[test]
fn update_of_old_row_keeps_default() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    insert_row(&core, &owner, 1, 1);
    add_column(&core, "tag TEXT DEFAULT 'd'").expect("ADD COLUMN tag");
    add_column(&core, "extra TEXT").expect("ADD COLUMN extra");

    let mut s = SessionState::default();
    core.execute_sql_in_session(
        &owner,
        &mut s,
        &format!("UPDATE {TABLE} SET extra = 'e' WHERE id = 1 USING OPERATION_ID 'op-u1'"),
    )
    .expect("update old row");
    assert!(matches!(
        one_cell(
            &core,
            &owner,
            &format!("SELECT tag FROM {TABLE} WHERE id = 1 LIMIT 1")
        ),
        Cell::Text(ref t) if t == "d"
    ));
}

#[test]
fn default_add_column_requires_ddl_permission_before_anything_else() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let err = core
        .execute_sql_in_session(
            &ctx("owner"),
            &mut SessionState::default(),
            &format!("ALTER TABLE {TABLE} ADD COLUMN note TEXT NOT NULL DEFAULT 'x'"),
        )
        .expect_err("no DDL privilege");
    assert_eq!(err.wire_code(), "42501");
}

/// `DATE` 列の `DEFAULT`（Issue #1279）。既存行は読み出し時に既定値で補われ、
/// 各読み出し経路（投影・WHERE・集計）と新規 INSERT の列省略で同じ値になる。
#[test]
fn date_default_fills_existing_rows_across_read_paths() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    insert_row(&core, &owner, 1, 1);
    add_column(&core, "dt DATE NOT NULL DEFAULT '2020-01-02'").expect("ADD COLUMN DATE");
    add_column(&core, "dn DATE DEFAULT '1999-12-31'").expect("ADD COLUMN nullable DATE");

    let days = engine::datetime::parse_date("2020-01-02").expect("date");
    let days_n = engine::datetime::parse_date("1999-12-31").expect("date");
    let q = |col: &str| {
        one_cell(
            &core,
            &owner,
            &format!("SELECT {col} FROM {TABLE} WHERE id = 1 LIMIT 1"),
        )
    };
    assert!(matches!(q("dt"), Cell::Date(d) if d == days));
    assert!(matches!(q("dn"), Cell::Date(d) if d == days_n));
    assert_eq!(
        count_star(&core, &owner, TABLE),
        1,
        "row count must be unchanged"
    );
    let filtered = core
        .execute_sql(
            &owner,
            &format!("SELECT id FROM {TABLE} WHERE dt = '2020-01-02' LIMIT 10"),
        )
        .expect("WHERE on DATE default");
    assert_eq!(filtered.rows.len(), 1);

    // 新規 INSERT で列省略 → 既定値、明示値 → その値。
    let mut s = SessionState::default();
    core.execute_sql_in_session(
        &owner,
        &mut s,
        &format!(
            "INSERT INTO {TABLE} (id, embedding) VALUES (2, '[0.3,0.4]') USING OPERATION_ID 'op-2'"
        ),
    )
    .expect("insert omitting dt");
    core.execute_sql_in_session(
        &owner,
        &mut s,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, dt) VALUES (3, '[0.3,0.4]', '2021-03-04') USING OPERATION_ID 'op-3'"
        ),
    )
    .expect("insert explicit dt");
    let explicit = engine::datetime::parse_date("2021-03-04").expect("date");
    assert!(matches!(
        one_cell(&core, &owner, &format!("SELECT dt FROM {TABLE} WHERE id = 2 LIMIT 1")),
        Cell::Date(d) if d == days
    ));
    assert!(matches!(
        one_cell(&core, &owner, &format!("SELECT dt FROM {TABLE} WHERE id = 3 LIMIT 1")),
        Cell::Date(d) if d == explicit
    ));
}

/// `DATE DEFAULT` は再オープン後も既定値として読める。
#[test]
fn date_default_persists_across_reopen() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path.clone());
    let owner = ctx("owner");
    insert_row(&core, &owner, 1, 1);
    add_column(&core, "dt DATE NOT NULL DEFAULT '2020-01-02'").expect("ADD COLUMN");
    drop(core);

    let storage = Storage::open(&path).expect("reopen");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let days = engine::datetime::parse_date("2020-01-02").expect("date");
    assert!(matches!(
        one_cell(
            &core,
            &owner,
            &format!("SELECT dt FROM {TABLE} WHERE id = 1 LIMIT 1")
        ),
        Cell::Date(d) if d == days
    ));
}

/// `DATE NOT NULL`（DEFAULT なし）は行の有無にかかわらず同一の `42601`、
/// `DATE NOT NULL DEFAULT` の成功も行の有無に依存しない（テナント境界 P0）。
#[test]
fn date_not_null_outcome_is_independent_of_rows_and_tenants() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let empty_err = add_column(&core, "d DATE NOT NULL").expect_err("empty");
    assert_eq!(empty_err.wire_code(), "42601");
    insert_row(&core, &ctx("bob"), 1, 1);
    let other_err = add_column(&core, "d DATE NOT NULL").expect_err("other tenant");
    assert_eq!(other_err.wire_code(), "42601");
    assert_eq!(empty_err.to_string(), other_err.to_string());
    assert!(!column_exists(&core, &ctx("bob"), TABLE, "d"));

    add_column(&core, "d DATE NOT NULL DEFAULT '2020-01-01'").expect("other tenant rows: ok");
    let days = engine::datetime::parse_date("2020-01-01").expect("date");
    assert!(matches!(
        one_cell(
            &core,
            &ctx("bob"),
            &format!("SELECT d FROM {TABLE} WHERE id = 1 LIMIT 1")
        ),
        Cell::Date(v) if v == days
    ));
}

/// `UUID` 列の `DEFAULT`（Issue #1281）。既存行は読み出し時に既定値で補われ、
/// 読み出しでは正規小文字表記になる。新規 INSERT の列省略でも同じ値になる。
#[test]
fn uuid_default_fills_existing_rows_across_read_paths() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    insert_row(&core, &owner, 1, 1);
    let upper = "0A0B0C0D-0000-0000-0000-00000000000F";
    let lower = "0a0b0c0d-0000-0000-0000-00000000000f";
    let other = "00000000-0000-0000-0000-000000000002";
    add_column(&core, &format!("u UUID NOT NULL DEFAULT '{upper}'")).expect("ADD COLUMN UUID");
    add_column(&core, &format!("un UUID DEFAULT '{other}'")).expect("ADD COLUMN nullable UUID");

    let q = |col: &str| {
        one_cell(
            &core,
            &owner,
            &format!("SELECT {col} FROM {TABLE} WHERE id = 1 LIMIT 1"),
        )
    };
    assert!(matches!(q("u"), Cell::Uuid(v) if v.to_string() == lower));
    assert!(matches!(q("un"), Cell::Uuid(v) if v.to_string() == other));
    assert_eq!(
        count_star(&core, &owner, TABLE),
        1,
        "row count must be unchanged"
    );
    let filtered = core
        .execute_sql(
            &owner,
            &format!("SELECT id FROM {TABLE} WHERE u = '{lower}' LIMIT 10"),
        )
        .expect("WHERE on UUID default");
    assert_eq!(filtered.rows.len(), 1);

    let mut s = SessionState::default();
    core.execute_sql_in_session(
        &owner,
        &mut s,
        &format!(
            "INSERT INTO {TABLE} (id, embedding) VALUES (2, '[0.3,0.4]') USING OPERATION_ID 'op-2'"
        ),
    )
    .expect("insert omitting u");
    core.execute_sql_in_session(
        &owner,
        &mut s,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, u) VALUES (3, '[0.3,0.4]', '{other}') USING OPERATION_ID 'op-3'"
        ),
    )
    .expect("insert explicit u");
    assert!(matches!(
        one_cell(&core, &owner, &format!("SELECT u FROM {TABLE} WHERE id = 2 LIMIT 1")),
        Cell::Uuid(v) if v.to_string() == lower
    ));
    assert!(matches!(
        one_cell(&core, &owner, &format!("SELECT u FROM {TABLE} WHERE id = 3 LIMIT 1")),
        Cell::Uuid(v) if v.to_string() == other
    ));
}

/// `UUID DEFAULT` は再オープン後も既定値として読める。
#[test]
fn uuid_default_persists_across_reopen() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path.clone());
    let owner = ctx("owner");
    insert_row(&core, &owner, 1, 1);
    add_column(
        &core,
        "u UUID NOT NULL DEFAULT '00000000-0000-0000-0000-000000000001'",
    )
    .expect("ADD COLUMN");
    drop(core);

    let storage = Storage::open(&path).expect("reopen");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    assert!(matches!(
        one_cell(
            &core,
            &owner,
            &format!("SELECT u FROM {TABLE} WHERE id = 1 LIMIT 1")
        ),
        Cell::Uuid(v) if v.to_string() == "00000000-0000-0000-0000-000000000001"
    ));
}

/// `UUID NOT NULL`（DEFAULT なし）は行の有無にかかわらず同一の `42601`、
/// `UUID NOT NULL DEFAULT` の成功も行の有無に依存しない（テナント境界 P0）。
#[test]
fn uuid_not_null_outcome_is_independent_of_rows_and_tenants() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let empty_err = add_column(&core, "u UUID NOT NULL").expect_err("empty");
    assert_eq!(empty_err.wire_code(), "42601");
    insert_row(&core, &ctx("bob"), 1, 1);
    let other_err = add_column(&core, "u UUID NOT NULL").expect_err("other tenant");
    assert_eq!(other_err.wire_code(), "42601");
    assert_eq!(empty_err.to_string(), other_err.to_string());
    assert!(!column_exists(&core, &ctx("bob"), TABLE, "u"));

    add_column(
        &core,
        "u UUID NOT NULL DEFAULT '00000000-0000-0000-0000-000000000001'",
    )
    .expect("other tenant rows: ok");
    assert!(matches!(
        one_cell(
            &core,
            &ctx("bob"),
            &format!("SELECT u FROM {TABLE} WHERE id = 1 LIMIT 1")
        ),
        Cell::Uuid(v) if v.to_string() == "00000000-0000-0000-0000-000000000001"
    ));
}

/// `TIMESTAMP` 列の `DEFAULT`（Issue #1280）。既存行は読み出し時に既定値で補われ、
/// 各読み出し経路（投影・WHERE・集計）と新規 INSERT の列省略で同じ値になる。
#[test]
fn timestamp_default_fills_existing_rows_across_read_paths() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    insert_row(&core, &owner, 1, 1);
    add_column(
        &core,
        "ts TIMESTAMP NOT NULL DEFAULT '2020-01-02 03:04:05.5'",
    )
    .expect("ADD COLUMN TIMESTAMP");
    add_column(&core, "tn TIMESTAMP DEFAULT '1999-12-31T23:59:59'")
        .expect("ADD COLUMN nullable TIMESTAMP");

    let micros = engine::datetime::parse_timestamp("2020-01-02 03:04:05.5").expect("ts");
    let micros_n = engine::datetime::parse_timestamp("1999-12-31T23:59:59").expect("ts");
    let q = |col: &str| {
        one_cell(
            &core,
            &owner,
            &format!("SELECT {col} FROM {TABLE} WHERE id = 1 LIMIT 1"),
        )
    };
    assert!(matches!(q("ts"), Cell::Timestamp(v) if v == micros));
    assert!(matches!(q("tn"), Cell::Timestamp(v) if v == micros_n));
    assert_eq!(
        count_star(&core, &owner, TABLE),
        1,
        "row count must be unchanged"
    );
    let filtered = core
        .execute_sql(
            &owner,
            &format!("SELECT id FROM {TABLE} WHERE ts = '2020-01-02 03:04:05.5' LIMIT 10"),
        )
        .expect("WHERE on TIMESTAMP default");
    assert_eq!(filtered.rows.len(), 1);

    let mut s = SessionState::default();
    core.execute_sql_in_session(
        &owner,
        &mut s,
        &format!(
            "INSERT INTO {TABLE} (id, embedding) VALUES (2, '[0.3,0.4]') USING OPERATION_ID 'op-2'"
        ),
    )
    .expect("insert omitting ts");
    core.execute_sql_in_session(
        &owner,
        &mut s,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, ts) VALUES (3, '[0.3,0.4]', '2021-03-04 05:06:07') USING OPERATION_ID 'op-3'"
        ),
    )
    .expect("insert explicit ts");
    let explicit = engine::datetime::parse_timestamp("2021-03-04 05:06:07").expect("ts");
    assert!(matches!(
        one_cell(&core, &owner, &format!("SELECT ts FROM {TABLE} WHERE id = 2 LIMIT 1")),
        Cell::Timestamp(v) if v == micros
    ));
    assert!(matches!(
        one_cell(&core, &owner, &format!("SELECT ts FROM {TABLE} WHERE id = 3 LIMIT 1")),
        Cell::Timestamp(v) if v == explicit
    ));
}

/// `TIMESTAMP DEFAULT` は再オープン後も既定値として読める。
#[test]
fn timestamp_default_persists_across_reopen() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path.clone());
    let owner = ctx("owner");
    insert_row(&core, &owner, 1, 1);
    add_column(&core, "ts TIMESTAMP NOT NULL DEFAULT '2020-01-02 03:04:05'").expect("ADD COLUMN");
    drop(core);

    let storage = Storage::open(&path).expect("reopen");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let micros = engine::datetime::parse_timestamp("2020-01-02 03:04:05").expect("ts");
    assert!(matches!(
        one_cell(
            &core,
            &owner,
            &format!("SELECT ts FROM {TABLE} WHERE id = 1 LIMIT 1")
        ),
        Cell::Timestamp(v) if v == micros
    ));
}

/// `TIMESTAMP NOT NULL`（DEFAULT なし）は行の有無にかかわらず同一の `42601`、
/// `TIMESTAMP NOT NULL DEFAULT` の成功も行の有無に依存しない（テナント境界 P0）。
#[test]
fn timestamp_not_null_outcome_is_independent_of_rows_and_tenants() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let empty_err = add_column(&core, "t TIMESTAMP NOT NULL").expect_err("empty");
    assert_eq!(empty_err.wire_code(), "42601");
    insert_row(&core, &ctx("bob"), 1, 1);
    let other_err = add_column(&core, "t TIMESTAMP NOT NULL").expect_err("other tenant");
    assert_eq!(other_err.wire_code(), "42601");
    assert_eq!(empty_err.to_string(), other_err.to_string());
    assert!(!column_exists(&core, &ctx("bob"), TABLE, "t"));

    add_column(&core, "t TIMESTAMP NOT NULL DEFAULT '2020-01-01 00:00:00'")
        .expect("other tenant rows: ok");
    let micros = engine::datetime::parse_timestamp("2020-01-01 00:00:00").expect("ts");
    assert!(matches!(
        one_cell(
            &core,
            &ctx("bob"),
            &format!("SELECT t FROM {TABLE} WHERE id = 1 LIMIT 1")
        ),
        Cell::Timestamp(v) if v == micros
    ));
}

/// 揮発性の既定値（`CURRENT_TIMESTAMP`・`now()`）は読み出し時補完と両立しないため拒否する。
#[test]
fn timestamp_volatile_default_is_rejected() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    for decl in [
        "ts TIMESTAMP DEFAULT CURRENT_TIMESTAMP",
        "ts TIMESTAMP DEFAULT now()",
    ] {
        let err = add_column(&core, decl).expect_err(decl);
        assert_eq!(err.wire_code(), "42601", "{decl}: {err:?}");
        assert!(!column_exists(&core, &ctx("owner"), TABLE, "ts"), "{decl}");
    }
}
