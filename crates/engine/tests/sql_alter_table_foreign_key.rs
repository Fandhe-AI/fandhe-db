//! `ALTER TABLE ... ADD [CONSTRAINT <name>] FOREIGN KEY (...) REFERENCES ...` ／
//! `ALTER TABLE ... DROP CONSTRAINT <name>`（FK 対応。TABLE-22・TASK-233、
//! Issue #1069）の結合テスト。ポインタ: `docs/spec/05-tasks.md` TASK-233・
//! `docs/spec/04-behavior/data-model.md` TABLE-22（関連: TABLE-16・TABLE-17）・
//! `docs/spec/04-behavior/sql-surface.md` SQL-23・SQL-31・
//! `docs/spec/04-behavior/rls.md` RLS-9・RLS-10 (c)・
//! `docs/spec/04-behavior/error-format.md` ERR-1・ERR-2・ERR-4・ERR-6。
//!
//! `sql_alter_table_unique_constraint.rs`・`table17_foreign_key.rs` と同じ流儀
//! （実 `Storage` ＋ `CpuScalarProvider`、`unique_db_path`／`CleanupGuard`、
//! `EngineCore::execute_sql_in_session` を production 経路として検証する）。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
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

fn ok(core: &EngineCore, session: &mut SessionState, ctx: &PolicyContext, sql: &str) -> SqlOutcome {
    exec(core, session, ctx, sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
}

fn err_code(
    core: &EngineCore,
    session: &mut SessionState,
    ctx: &PolicyContext,
    sql: &str,
) -> String {
    exec(core, session, ctx, sql)
        .expect_err(&format!("{sql} must fail"))
        .wire_code()
        .to_string()
}

// --- ADD: 成功系・既定名 -----------------------------------------------------

/// 名前省略の `ADD FOREIGN KEY` は既定名（`<table>_<col>_fkey`）で確定する
/// （設計 F2）。追加後は参照整合性検査が有効になる。
#[test]
fn add_foreign_key_without_name_uses_default_name_and_enforces_referential_integrity() {
    let (core, path) = new_core("alter-fk-add-default-name");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    ok(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE parents (name TEXT)",
    );
    ok(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE children (parent_id BIGINT)",
    );
    ok(
        &core,
        &mut session,
        &owner,
        "INSERT INTO parents (id, name) VALUES (1, 'p1') USING OPERATION_ID 'op-p1'",
    );
    // FK 追加前は無関係の値でも挿入できる。
    ok(
        &core,
        &mut session,
        &owner,
        "INSERT INTO children (id) VALUES (100) USING OPERATION_ID 'op-c-pre'",
    );

    let outcome = ok(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE children ADD FOREIGN KEY (parent_id) REFERENCES parents",
    );
    match outcome {
        SqlOutcome::AlterTable(o) => match o.action {
            engine::sql::ddl::AlterTableAction::AddConstraint { constraint_name } => {
                assert_eq!(constraint_name, "children_parent_id_fkey");
            }
            other => panic!("unexpected AlterTableAction: {other:?}"),
        },
        other => panic!("unexpected SqlOutcome: {other:?}"),
    }

    // 追加後は参照先 `id` が存在する行のみ受理される。
    ok(
        &core,
        &mut session,
        &owner,
        "INSERT INTO children (id, parent_id) VALUES (1, 1) USING OPERATION_ID 'op-c1'",
    );
    assert_eq!(
        err_code(
            &core,
            &mut session,
            &owner,
            "INSERT INTO children (id, parent_id) VALUES (2, 999) USING OPERATION_ID 'op-c2'"
        ),
        "23503"
    );
}

/// 明示 `CONSTRAINT <name> FOREIGN KEY` は指定した名前で確定し、
/// `DROP CONSTRAINT` でその名前を指定して削除できる。削除後は既存行を
/// 変更せず、以後の孤児行 INSERT を許すようになる。
#[test]
fn add_named_foreign_key_then_drop_by_name_round_trips() {
    let (core, path) = new_core("alter-fk-add-named");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    ok(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE parents (name TEXT)",
    );
    ok(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE children (parent_id BIGINT)",
    );
    ok(
        &core,
        &mut session,
        &owner,
        "INSERT INTO parents (id, name) VALUES (1, 'p1') USING OPERATION_ID 'op-p1'",
    );

    ok(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE children ADD CONSTRAINT fk_children_parent FOREIGN KEY (parent_id) \
         REFERENCES parents",
    );

    ok(
        &core,
        &mut session,
        &owner,
        "INSERT INTO children (id, parent_id) VALUES (1, 1) USING OPERATION_ID 'op-c1'",
    );
    assert_eq!(
        err_code(
            &core,
            &mut session,
            &owner,
            "INSERT INTO children (id, parent_id) VALUES (2, 999) USING OPERATION_ID 'op-c2'"
        ),
        "23503"
    );

    ok(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE children DROP CONSTRAINT fk_children_parent",
    );

    // 既存行は変更されない。
    ok(
        &core,
        &mut session,
        &owner,
        "INSERT INTO children (id, parent_id) VALUES (2, 999) USING OPERATION_ID 'op-c2b'",
    );
}

/// `ADD FOREIGN KEY` は対象テーブルの既存行を検証し、違反があれば
/// 副作用ゼロで `23503` を返す（テーブル・制約とも変更されない）。
#[test]
fn add_foreign_key_rejects_when_existing_rows_already_violate() {
    let (core, path) = new_core("alter-fk-add-existing-violation");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    ok(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE parents (name TEXT)",
    );
    ok(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE children (parent_id BIGINT)",
    );
    ok(
        &core,
        &mut session,
        &owner,
        "INSERT INTO parents (id, name) VALUES (1, 'p1') USING OPERATION_ID 'op-p1'",
    );
    ok(
        &core,
        &mut session,
        &owner,
        "INSERT INTO children (id, parent_id) VALUES (1, 999) USING OPERATION_ID 'op-c1'",
    );

    assert_eq!(
        err_code(
            &core,
            &mut session,
            &owner,
            "ALTER TABLE children ADD FOREIGN KEY (parent_id) REFERENCES parents"
        ),
        "23503"
    );

    // 拒否後もカタログは変更されておらず、無関係な値の挿入は自由に行える。
    ok(
        &core,
        &mut session,
        &owner,
        "INSERT INTO children (id, parent_id) VALUES (2, 12345) USING OPERATION_ID 'op-c2'",
    );
}

/// テナント境界（RLS-9・RLS-10 (c)）: あるテナントの子行が別テナントの親行
/// でしか満たせない場合、`ADD FOREIGN KEY` は `23503` で拒否する（全テナント
/// 検証であり、他テナントの存在では救済されない）。エラー応答はテナント名・
/// 値・表名を含まない固定文言。
#[test]
fn add_foreign_key_cross_tenant_orphan_rows_are_not_satisfied() {
    let (core, path) = new_core("alter-fk-add-cross-tenant");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    ok(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE parents (name TEXT)",
    );
    ok(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE children (parent_id BIGINT)",
    );

    let tenant_a = ctx("tenant-a");
    let tenant_b = ctx("tenant-b");
    // テナント B にのみ親行 id=1 が存在する。
    ok(
        &core,
        &mut session,
        &tenant_b,
        "INSERT INTO parents (id, name) VALUES (1, 'b-parent') USING OPERATION_ID 'op-b-p1'",
    );
    // テナント A の子行は id=1 を参照するが、テナント A 自身には親行が無い。
    ok(
        &core,
        &mut session,
        &tenant_a,
        "INSERT INTO children (id, parent_id) VALUES (1, 1) USING OPERATION_ID 'op-a-c1'",
    );

    let err = exec(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE children ADD FOREIGN KEY (parent_id) REFERENCES parents",
    )
    .expect_err("cross-tenant match must not satisfy the new foreign key");
    assert_eq!(err.wire_code(), "23503");
    let message = format!("{err}");
    assert!(
        !message.contains("tenant-a") && !message.contains("tenant-b"),
        "error message must not leak tenant identifiers: {message}"
    );

    // テナント B 自身は自テナント内で整合しているため、後から同じ制約を
    // テナント B だけの状況で追加できることを別テーブルで確認する
    // （全テナント検証であることの対照: A を除けば成功するはず）。
    ok(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE parents2 (name TEXT)",
    );
    ok(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE children2 (parent_id BIGINT)",
    );
    ok(
        &core,
        &mut session,
        &tenant_b,
        "INSERT INTO parents2 (id, name) VALUES (1, 'b-parent') USING OPERATION_ID 'op-b-p2'",
    );
    ok(
        &core,
        &mut session,
        &tenant_b,
        "INSERT INTO children2 (id, parent_id) VALUES (1, 1) USING OPERATION_ID 'op-b-c2'",
    );
    ok(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE children2 ADD FOREIGN KEY (parent_id) REFERENCES parents2",
    );
}

/// 自己参照 FK を `ALTER TABLE ADD` で追加できる。
#[test]
fn add_self_referencing_foreign_key() {
    let (core, path) = new_core("alter-fk-add-self-ref");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    ok(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE nodes (parent_id BIGINT)",
    );
    ok(
        &core,
        &mut session,
        &owner,
        "INSERT INTO nodes (id) VALUES (1) USING OPERATION_ID 'op-n1'",
    );
    ok(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE nodes ADD FOREIGN KEY (parent_id) REFERENCES nodes",
    );
    ok(
        &core,
        &mut session,
        &owner,
        "INSERT INTO nodes (id, parent_id) VALUES (2, 1) USING OPERATION_ID 'op-n2'",
    );
    assert_eq!(
        err_code(
            &core,
            &mut session,
            &owner,
            "INSERT INTO nodes (id, parent_id) VALUES (3, 999) USING OPERATION_ID 'op-n3'"
        ),
        "23503"
    );
}

// --- ADD: エラー分類 ---------------------------------------------------------

/// 参照先テーブルが存在しない場合は `42P01`（親テーブル名で報告する）。
#[test]
fn add_foreign_key_rejects_missing_parent_table_with_42p01() {
    let (core, path) = new_core("alter-fk-add-missing-parent");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    ok(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE children (parent_id BIGINT)",
    );
    assert_eq!(
        err_code(
            &core,
            &mut session,
            &owner,
            "ALTER TABLE children ADD FOREIGN KEY (parent_id) REFERENCES missing_parent"
        ),
        "42P01"
    );
}

/// 明示制約名が既存の UNIQUE・CHECK 制約名と衝突する場合は `42P07`
/// （設計 F1。UNIQUE・CHECK・FOREIGN KEY はテーブル単位の名前空間を共有する）。
#[test]
fn add_foreign_key_rejects_name_collision_with_existing_unique() {
    let (core, path) = new_core("alter-fk-add-name-collision");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    ok(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE parents (name TEXT)",
    );
    ok(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE children (parent_id BIGINT)",
    );
    ok(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE children ADD CONSTRAINT dup_name UNIQUE (parent_id)",
    );
    assert_eq!(
        err_code(
            &core,
            &mut session,
            &owner,
            "ALTER TABLE children ADD CONSTRAINT dup_name FOREIGN KEY (parent_id) \
             REFERENCES parents"
        ),
        "42P07"
    );
}

/// 参照先が一意性を持たない列集合の場合は `42830`。
#[test]
fn add_foreign_key_rejects_non_unique_target_with_42830() {
    let (core, path) = new_core("alter-fk-add-non-unique-target");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    ok(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE parents (code TEXT)",
    );
    ok(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE children (code TEXT)",
    );
    assert_eq!(
        err_code(
            &core,
            &mut session,
            &owner,
            "ALTER TABLE children ADD FOREIGN KEY (code) REFERENCES parents (code)"
        ),
        "42830"
    );
}

/// DDL 権限を持たないセッションは対象テーブルの存否によらず `42501`
/// （存在オラクルにしない）。
#[test]
fn add_foreign_key_without_ddl_permission_is_42501_for_existing_and_missing_table() {
    let (core, path) = new_core("alter-fk-add-no-permission");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut ddl = ddl_session();
    ok(&core, &mut ddl, &owner, "CREATE TABLE parents (name TEXT)");
    ok(
        &core,
        &mut ddl,
        &owner,
        "CREATE TABLE children (parent_id BIGINT)",
    );

    let mut no_ddl = SessionState::default();
    assert_eq!(
        err_code(
            &core,
            &mut no_ddl,
            &owner,
            "ALTER TABLE children ADD FOREIGN KEY (parent_id) REFERENCES parents"
        ),
        "42501"
    );
    assert_eq!(
        err_code(
            &core,
            &mut no_ddl,
            &owner,
            "ALTER TABLE missing ADD FOREIGN KEY (parent_id) REFERENCES parents"
        ),
        "42501"
    );
}

// --- 索引衛生（P0。設計 §3.4） ----------------------------------------------

/// `ADD FOREIGN KEY` の既存行検証は、DROP されていた間に追加された行も含め
/// 常に**現在**の子テーブルの状態を見る（`verify_new_foreign_key_all_tenants_in_txn`
/// は子テーブルを毎回フルスキャンするため、`id` 参照は `key_index.rs` を一切
/// 経由しない。この経路自体は永続キー索引を持たないため、本テストは索引の
/// stale 化そのものを再現するものではない。索引の stale 化 P0 回帰は
/// 下記 `readding_foreign_key_after_parent_unique_gap_detects_current_parent_rows`
/// が固定する）。
#[test]
fn readding_foreign_key_after_drop_detects_rows_added_during_the_gap() {
    let (core, path) = new_core("alter-fk-index-hygiene-child");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    ok(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE parents (name TEXT)",
    );
    ok(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE children (parent_id BIGINT)",
    );
    ok(
        &core,
        &mut session,
        &owner,
        "INSERT INTO parents (id, name) VALUES (1, 'p1') USING OPERATION_ID 'op-p1'",
    );
    ok(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE children ADD CONSTRAINT fk_c FOREIGN KEY (parent_id) REFERENCES parents",
    );
    ok(
        &core,
        &mut session,
        &owner,
        "INSERT INTO children (id, parent_id) VALUES (1, 1) USING OPERATION_ID 'op-c1'",
    );
    ok(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE children DROP CONSTRAINT fk_c",
    );
    // FK が無い間に違反行を追加する（空白期間）。
    ok(
        &core,
        &mut session,
        &owner,
        "INSERT INTO children (id, parent_id) VALUES (2, 999) USING OPERATION_ID 'op-c2'",
    );
    // 再 ADD 時に新しい違反行が検出されなければならない。
    assert_eq!(
        err_code(
            &core,
            &mut session,
            &owner,
            "ALTER TABLE children ADD CONSTRAINT fk_c FOREIGN KEY (parent_id) \
             REFERENCES parents"
        ),
        "23503"
    );
}

/// 親側の stale 索引回帰（P0）: 親の UNIQUE 列を DROP・再 ADD する空白期間に
/// 親行を削除・再作成しても、以後の FK 追加時の検証が古い索引を信用せず
/// 現在の親行だけを見る。
#[test]
fn readding_foreign_key_after_parent_unique_gap_detects_current_parent_rows() {
    let (core, path) = new_core("alter-fk-index-hygiene-parent");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    ok(&core, &mut session, &owner, "CREATE TABLE parents (u TEXT)");
    ok(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE children (u TEXT)",
    );
    ok(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE parents ADD CONSTRAINT uq_u UNIQUE (u)",
    );
    ok(
        &core,
        &mut session,
        &owner,
        "INSERT INTO parents (id, u) VALUES (1, 'x') USING OPERATION_ID 'op-p1'",
    );
    ok(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE children ADD CONSTRAINT fk_u FOREIGN KEY (u) REFERENCES parents (u)",
    );
    // 子行を 1 件挿入し、FK 検査経路で親（`parents`, `u`）の永続キー索引を
    // 実際に backfill・登録させる（索引は初回の未登録照会でのみ構築される。
    // ここで登録しないと、以後 DROP しても「刈り込む索引が無い」だけの
    // 弱いテストになってしまう）。
    ok(
        &core,
        &mut session,
        &owner,
        "INSERT INTO children (id, u) VALUES (1, 'x') USING OPERATION_ID 'op-c1'",
    );
    ok(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE children DROP CONSTRAINT fk_u",
    );
    ok(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE parents DROP CONSTRAINT uq_u",
    );
    // 親行 x を削除する（索引が同期されない空白期間の変更。`parents` は
    // この時点で FK・PK・UNIQUE のいずれも持たないため
    // `table_may_need_index` が偽になり、登録済み索引は同期されない）。
    ok(
        &core,
        &mut session,
        &owner,
        "DELETE FROM parents WHERE id = 1 USING OPERATION_ID 'op-p-del'",
    );
    ok(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE parents ADD CONSTRAINT uq_u UNIQUE (u)",
    );
    // 子には x を参照する行（id=1）が残っている。索引衛生
    // （`key_index::prune_unneeded_indexes_in_txn`）が無ければ、DROP FK／
    // DROP UNIQUE の間に登録簿へ残ったままの stale な索引（x を含む）を
    // `all_keys_exist_in_txn` が信用してしまい、現在は存在しない x への
    // 参照を誤って通してしまう（fail-open）。
    assert_eq!(
        err_code(
            &core,
            &mut session,
            &owner,
            "ALTER TABLE children ADD CONSTRAINT fk_u FOREIGN KEY (u) REFERENCES parents (u)"
        ),
        "23503"
    );
}

// --- CREATE TABLE との相互作用 ----------------------------------------------

/// `CREATE TABLE` の明示 FK 名と CHECK 名の重複は `42601`（設計 F6）。
#[test]
fn create_table_rejects_duplicate_name_between_foreign_key_and_check() {
    let (core, path) = new_core("create-table-fk-check-name-collision");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    ok(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE parents (name TEXT)",
    );
    assert_eq!(
        err_code(
            &core,
            &mut session,
            &owner,
            "CREATE TABLE children (v BIGINT, \
             CONSTRAINT dup CHECK (v > 0), \
             CONSTRAINT dup FOREIGN KEY (v) REFERENCES parents)"
        ),
        "42601"
    );
}

/// `ALTER TABLE ... ADD` の対象外形状は `42601` のまま
/// （`NOT VALID`・列制約 `CONSTRAINT n REFERENCES` は設計スコープ外）。
#[test]
fn out_of_scope_alter_table_foreign_key_forms_are_rejected_with_42601() {
    let (core, path) = new_core("alter-fk-out-of-scope");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    ok(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE parents (name TEXT)",
    );
    ok(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE children (parent_id BIGINT)",
    );
    for sql in [
        "ALTER TABLE children ADD FOREIGN KEY (parent_id) REFERENCES parents NOT VALID",
        "ALTER TABLE children ADD COLUMN v BIGINT CONSTRAINT fk_v REFERENCES parents",
    ] {
        assert_eq!(err_code(&core, &mut session, &owner, sql), "42601", "{sql}");
    }
}

/// 明示トランザクション内の `ALTER TABLE ... ADD FOREIGN KEY` は DDL を
/// 拒否する既存の catch-all により `0A000` になり、トランザクションを失敗
/// させる（SQL-31・TASK-221。`sql_alter_table_unique_constraint.rs` の同種
/// テストと同じ判定）。
#[test]
fn add_foreign_key_inside_explicit_transaction_is_rejected_with_0a000() {
    let (core, path) = new_core("alter-fk-explicit-txn");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    ok(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE parents (name TEXT)",
    );
    ok(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE children (parent_id BIGINT)",
    );

    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&owner, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(
            &owner,
            &mut session,
            &mut txn,
            "ALTER TABLE children ADD FOREIGN KEY (parent_id) REFERENCES parents",
        )
        .expect_err("ALTER TABLE ADD FOREIGN KEY inside an explicit transaction must be rejected");
    assert_eq!(err.wire_code(), "0A000");
    assert_eq!(txn.status(), TransactionStatus::Failed);

    core.execute_sql_in_txn(&owner, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(txn.status(), TransactionStatus::Idle);

    // 制約が追加されていないことを、無関係な値の INSERT が成功することで確認する。
    ok(
        &core,
        &mut session,
        &owner,
        "INSERT INTO children (id, parent_id) VALUES (1, 999) USING OPERATION_ID 'op-c1'",
    );
}

/// 設計 F3: v8〜v11 の無名 FK（`CREATE TABLE` 由来）は decode 時に既定名
/// （`<table>_<col>_fkey`）を導出し、その名前で `DROP CONSTRAINT` できる
/// （新規 `ALTER TABLE ADD` だけでなく既存 DB の FK も対象になることの確認）。
#[test]
fn drop_constraint_removes_legacy_unnamed_foreign_key_by_derived_name() {
    let (core, path) = new_core("alter-fk-drop-legacy-derived-name");
    let _guard = CleanupGuard(path);
    let owner = ctx("owner");
    let mut session = ddl_session();
    ok(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE parents (name TEXT)",
    );
    ok(
        &core,
        &mut session,
        &owner,
        "CREATE TABLE children (v BIGINT REFERENCES parents)",
    );
    ok(
        &core,
        &mut session,
        &owner,
        "INSERT INTO parents (id, name) VALUES (1, 'p1') USING OPERATION_ID 'op-p1'",
    );
    assert_eq!(
        err_code(
            &core,
            &mut session,
            &owner,
            "INSERT INTO children (id, v) VALUES (1, 999) USING OPERATION_ID 'op-c1'"
        ),
        "23503"
    );

    ok(
        &core,
        &mut session,
        &owner,
        "ALTER TABLE children DROP CONSTRAINT children_v_fkey",
    );

    // 削除後は孤児行が受理される。
    ok(
        &core,
        &mut session,
        &owner,
        "INSERT INTO children (id, v) VALUES (1, 999) USING OPERATION_ID 'op-c1b'",
    );
}
