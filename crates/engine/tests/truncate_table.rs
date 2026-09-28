//! `TRUNCATE TABLE <table> USING OPERATION_ID '<id>'`（TASK-193、対象ビヘイビア:
//! SQL-22）の結合テスト。ポインタ: `docs/spec/05-tasks.md` TASK-193・
//! `docs/spec/04-behavior/sql-surface.md` SQL-22。関連ポインタ: TABLE-4（テーブル
//! 定義は残る DDL 非該当操作）・RLS-7（暗黙のテナント境界適用）・RLS-9（他テナント
//! 存在情報の非漏えい）・RECOVER-1〜3・RECOVER-10（`operation_id` 必須化・台帳
//! 照合による再送判定）。
//!
//! `EngineCore::execute_sql_in_session`（先頭トークン `TRUNCATE` の覗き見判定 →
//! `sql::allowlist::validate_truncate_tokens` → `sql::exec::execute_truncate`）を
//! production 経路として検証する。`sql_insert_session_dispatch.rs`・
//! `sql_visible_cache.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `unique_db_path`／`CleanupGuard`）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{RowInput, Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";
const OTHER_TABLE: &str = "other_documents";

fn schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
    )
}

fn new_core_with_tables() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("truncate-table");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema(TABLE)).expect("create table");
    storage
        .create_table(&schema(OTHER_TABLE))
        .expect("create other table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx_for(tenant: &str, allow_private: bool) -> PolicyContext {
    if allow_private {
        PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
            .expect("valid tenant")
    } else {
        PolicyContext::new(tenant).expect("valid tenant")
    }
}

/// `Private` のみ許可するコンテキスト（`is_visible` は `allowed.contains` を
/// 最初に見るため、`Public` 行が同一テナントでも一切見えなくなる。他テナントの
/// `Public` 行がグローバルに可視である〔`policy.rs::is_visible` ドキュメント
/// 参照〕ため、`COUNT(*)` で「自テナントの `Private` 行数」だけを cross-tenant
/// `Public` 行の混入なしに検証したい場合に使う）。
fn ctx_private_only(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Private]).expect("valid tenant")
}

fn op_id(label: &str) -> OperationId {
    OperationId::parse(label).expect("valid operation id")
}

fn insert_row(
    core: &EngineCore,
    ctx: &PolicyContext,
    table: &str,
    id: u64,
    visibility: Visibility,
    seq: u64,
) {
    core.insert_row(
        ctx,
        table,
        id,
        &RowInput {
            tenant_id: ctx.tenant_id(),
            visibility,
            embedding: &[0.1f32, 0.2f32],
            metadata: &[],
        },
        Some(&op_id(&format!("seed-{table}-{id}-{seq}"))),
    )
    .expect("insert row");
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

fn distance_hit_count(core: &EngineCore, ctx: &PolicyContext, table: &str) -> usize {
    core.execute_sql(
        ctx,
        &format!("SELECT id FROM {table} ORDER BY embedding <=> '[0.1,0.2]' LIMIT 50"),
    )
    .expect("select should succeed")
    .rows
    .len()
}

/// RLS-7/RLS-9: TRUNCATE はセッションのテナントが所有する行（`Public`／`Private`
/// を問わない）だけを削除し、他テナントの行には一切触れない。
///
/// `Visibility::Public` は `policy.rs::is_visible` の契約上グローバルに可視
/// （許可した任意テナントから読める）であるため、`COUNT(*)`（`Public`＋
/// `Private` 許可のコンテキスト）だけでは「alice の Public 行が消えたか」と
/// 「bob の Public 行がそのまま観測可能か」を切り分けられない。そこで
/// `ctx_private_only` による各テナントの `Private` 行数の検証と、`charlie`
/// （行を一切所有しない第三者。`Public` のみ許可）による「テーブル全体で
/// 現在 `Public` 可視な行数」の検証を組み合わせ、alice の `Public` 行だけが
/// 消えたことを確認する。
#[test]
fn truncate_removes_only_own_tenant_rows_public_and_private() {
    let (core, _path) = new_core_with_tables();
    let _guard = CleanupGuard(_path);
    let alice = ctx_for("alice", true);
    let bob = ctx_for("bob", true);
    let public_observer = ctx_for("charlie", false);

    insert_row(&core, &alice, TABLE, 1, Visibility::Public, 1);
    insert_row(&core, &alice, TABLE, 2, Visibility::Private, 2);
    insert_row(&core, &bob, TABLE, 3, Visibility::Public, 3);
    insert_row(&core, &bob, TABLE, 4, Visibility::Private, 4);

    assert_eq!(count_star(&core, &ctx_private_only("alice"), TABLE), 1);
    assert_eq!(count_star(&core, &ctx_private_only("bob"), TABLE), 1);
    // グローバルに可視な Public 行は alice(1)・bob(3) の 2 件。
    assert_eq!(count_star(&core, &public_observer, TABLE), 2);

    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("TRUNCATE TABLE {TABLE} USING OPERATION_ID 'op-truncate-alice'"),
        )
        .expect("TRUNCATE should succeed");
    assert!(matches!(outcome, SqlOutcome::Truncate(_)));

    // alice の Public・Private とも消える（自テナントの Private 行数で確認）。
    assert_eq!(count_star(&core, &ctx_private_only("alice"), TABLE), 0);
    // bob の Private 行は無傷のまま。
    assert_eq!(count_star(&core, &ctx_private_only("bob"), TABLE), 1);
    // グローバルに可視な Public 行は bob(3) の 1 件のみ残る（alice(1) が消えた）。
    assert_eq!(count_star(&core, &public_observer, TABLE), 1);
    // alice 自身の視点（Public＋Private 許可）でも、bob の残存 Public 行（1 件）が
    // グローバルに見え続ける点を除けば自テナント分は 0 件（cross-tenant Public
    // 可視の契約上、alice のコンテキストでも bob の Public 行は見える）。
    assert_eq!(count_star(&core, &alice, TABLE), 1);
}

/// RLS-9 オラクル: 他テナントの行が 0 件でも複数件でも、自テナントの TRUNCATE
/// 応答（成功・`wire_code`）は区別不能でなければならない（存在情報の非漏えい）。
#[test]
fn truncate_response_is_identical_regardless_of_other_tenant_row_count() {
    // ケース 1: bob の行が 0 件。
    {
        let (core, path) = new_core_with_tables();
        let _guard = CleanupGuard(path);
        let alice = ctx_for("alice", true);
        insert_row(&core, &alice, TABLE, 1, Visibility::Public, 1);

        let mut session = SessionState::default();
        let outcome = core
            .execute_sql_in_session(
                &alice,
                &mut session,
                &format!("TRUNCATE TABLE {TABLE} USING OPERATION_ID 'op-oracle'"),
            )
            .expect("TRUNCATE should succeed with zero other-tenant rows");
        assert!(matches!(outcome, SqlOutcome::Truncate(_)));
    }

    // ケース 2: bob の行が複数件。応答の型・成否は同一（`SqlOutcome::Truncate`
    // 成功で件数を一切含まない）。
    {
        let (core, path) = new_core_with_tables();
        let _guard = CleanupGuard(path);
        let alice = ctx_for("alice", true);
        let bob = ctx_for("bob", true);
        insert_row(&core, &alice, TABLE, 1, Visibility::Public, 1);
        for id in 10..20 {
            insert_row(&core, &bob, TABLE, id, Visibility::Private, id);
        }

        let mut session = SessionState::default();
        let outcome = core
            .execute_sql_in_session(
                &alice,
                &mut session,
                &format!("TRUNCATE TABLE {TABLE} USING OPERATION_ID 'op-oracle'"),
            )
            .expect("TRUNCATE should succeed with many other-tenant rows");
        assert!(matches!(outcome, SqlOutcome::Truncate(_)));
        // bob の行数は一切変わらない（真の非漏えい確認）。
        assert_eq!(count_star(&core, &bob, TABLE), 10);
    }
}

/// RECOVER-1: `operation_id` 句の省略（明示 `NULL` を含む）は `Ledgered`（既定）
/// 構成では `23502` で書き込みトランザクション開始前に拒否される。
#[test]
fn truncate_missing_operation_id_is_rejected_with_23502_before_any_write() {
    let (core, path) = new_core_with_tables();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    insert_row(&core, &alice, TABLE, 1, Visibility::Public, 1);

    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(&alice, &mut session, &format!("TRUNCATE TABLE {TABLE}"))
        .expect_err("missing operation_id must be rejected");
    assert_eq!(err.wire_code(), "23502");
    // 拒否されたので行は残ったまま。
    assert_eq!(count_star(&core, &alice, TABLE), 1);

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("TRUNCATE TABLE {TABLE} USING OPERATION_ID NULL"),
        )
        .expect_err("explicit NULL operation_id must be rejected");
    assert_eq!(err.wire_code(), "23502");
    assert_eq!(count_star(&core, &alice, TABLE), 1);
}

/// RECOVER-10: 同一 `operation_id` への TRUNCATE 再送は内容一致（`for_truncate`
/// はテーブル名以外の入力を持たないため常に一致する）として `23505` で拒否される。
#[test]
fn truncate_resending_same_operation_id_is_rejected_with_23505() {
    let (core, path) = new_core_with_tables();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    insert_row(&core, &alice, TABLE, 1, Visibility::Public, 1);

    let mut session = SessionState::default();
    let sql = format!("TRUNCATE TABLE {TABLE} USING OPERATION_ID 'op-resend'");
    core.execute_sql_in_session(&alice, &mut session, &sql)
        .expect("first TRUNCATE should succeed");
    assert_eq!(count_star(&core, &alice, TABLE), 0);

    let err = core
        .execute_sql_in_session(&alice, &mut session, &sql)
        .expect_err("resend with the same operation_id must be rejected as duplicate commit");
    assert_eq!(err.wire_code(), "23505");
}

/// 0 行 TRUNCATE の冪等性: 対象テナントの行が 0 件でも台帳記録・世代進行は
/// 確実に発生する（再送すると `23505` になることで検証する。この検証が
/// なければ「0 件時に台帳へ書かず commit もしない」実装でも見かけ上パスして
/// しまう vacuous pass になる）。
#[test]
fn truncate_on_empty_table_still_records_the_ledger_entry() {
    let (core, path) = new_core_with_tables();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    assert_eq!(count_star(&core, &alice, TABLE), 0);

    let mut session = SessionState::default();
    let sql = format!("TRUNCATE TABLE {TABLE} USING OPERATION_ID 'op-empty'");
    let outcome = core
        .execute_sql_in_session(&alice, &mut session, &sql)
        .expect("TRUNCATE of an already-empty table must succeed");
    assert!(matches!(outcome, SqlOutcome::Truncate(_)));

    let err = core
        .execute_sql_in_session(&alice, &mut session, &sql)
        .expect_err("resend against the same operation_id must still be a duplicate");
    assert_eq!(
        err.wire_code(),
        "23505",
        "0-row TRUNCATE must have recorded the ledger entry on first commit"
    );
}

/// TABLE-4: TRUNCATE 後もテーブル定義（カタログ）は残る（`Storage::drop_table`
/// との対比。同一テーブルへの後続 INSERT が成功することで確認する）。他テーブルの
/// 行・カタログも無変更のまま。
#[test]
fn truncate_keeps_table_definition_and_does_not_affect_other_tables() {
    let (core, path) = new_core_with_tables();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    insert_row(&core, &alice, TABLE, 1, Visibility::Public, 1);
    insert_row(&core, &alice, OTHER_TABLE, 100, Visibility::Public, 100);

    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!("TRUNCATE TABLE {TABLE} USING OPERATION_ID 'op-ddl-contrast'"),
    )
    .expect("TRUNCATE should succeed");

    // 他テーブルの行は無変更。
    assert_eq!(count_star(&core, &alice, OTHER_TABLE), 1);

    // テーブル定義は残っているため、TRUNCATE 後も同一テーブルへ INSERT できる
    // （`drop_table` ならテーブル不存在で `42P01` になる）。
    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding) VALUES (5, '[0.3,0.4]') \
             USING OPERATION_ID 'op-post-truncate-insert'"
        ),
    )
    .expect("INSERT after TRUNCATE must succeed because the table definition still exists");
    assert_eq!(count_star(&core, &alice, TABLE), 1);
}

/// キャッシュ失効: `SqlArenaCache`（DISTANCE クエリ）・`VisibleBitmapCache`
/// （`COUNT(*)`）をそれぞれ TRUNCATE 前に 1 度実行して温めてから TRUNCATE し、
/// 同じクエリを再実行して削除済み行が一切ヒットしないことを確認する（世代整合の
/// 非自明な検証。cold のみのテストでは世代失効を検証したことにならない）。
#[test]
fn truncate_invalidates_arena_and_visible_bitmap_caches() {
    let (core, path) = new_core_with_tables();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    insert_row(&core, &alice, TABLE, 1, Visibility::Public, 1);
    insert_row(&core, &alice, TABLE, 2, Visibility::Public, 2);

    // DISTANCE クエリ（SqlArenaCache）・COUNT(*)（VisibleBitmapCache）をそれぞれ
    // 1 度実行してキャッシュを温める。
    assert_eq!(distance_hit_count(&core, &alice, TABLE), 2);
    assert_eq!(count_star(&core, &alice, TABLE), 2);

    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!("TRUNCATE TABLE {TABLE} USING OPERATION_ID 'op-cache-invalidate'"),
    )
    .expect("TRUNCATE should succeed");

    // テーブル世代が進んでいるため、両キャッシュとも失効し削除済み行は
    // 一切ヒットしない。
    assert_eq!(distance_hit_count(&core, &alice, TABLE), 0);
    assert_eq!(count_star(&core, &alice, TABLE), 0);
}

/// `USING PLAN` と同じく `EXPLAIN TRUNCATE ...` は許可形状に存在しないため
/// `42601` で拒否される（`EXPLAIN` は検索 SELECT の前置専用。TASK-78・SQL-6。
/// `sql_insert_session_dispatch.rs::session_explain_insert_is_rejected_as_unsupported_syntax`
/// と同型の確認）。
#[test]
fn explain_truncate_is_rejected_as_unsupported_syntax() {
    let (core, path) = new_core_with_tables();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("EXPLAIN TRUNCATE TABLE {TABLE} USING OPERATION_ID 'op-explain-truncate'"),
        )
        .expect_err("EXPLAIN TRUNCATE must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

/// `TRUNCATE TABLE` 複数指定（PostgreSQL 拡張句）は許可リスト外として `42601`
/// で拒否される。
#[test]
fn truncate_with_multiple_tables_is_rejected_as_unsupported_syntax() {
    let (core, path) = new_core_with_tables();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("TRUNCATE TABLE {TABLE}, {OTHER_TABLE} USING OPERATION_ID 'op-multi-table'"),
        )
        .expect_err("multiple tables must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

/// 存在しないテーブルへの TRUNCATE は `42P01`（`UndefinedTable`）。
#[test]
fn truncate_undefined_table_is_rejected_with_42p01() {
    let (core, path) = new_core_with_tables();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "TRUNCATE TABLE ghost USING OPERATION_ID 'op-undefined-table'",
        )
        .expect_err("undefined table must be rejected");
    assert_eq!(err.wire_code(), "42P01");
}

/// `EngineCore::execute_truncate_sql`（セッション非経由の直接エントリポイント）
/// が `execute_sql_in_session` の TRUNCATE 分岐と同じ契約であることを確認する。
#[test]
fn execute_truncate_sql_direct_entry_point_matches_session_dispatch_contract() {
    let (core, path) = new_core_with_tables();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    insert_row(&core, &alice, TABLE, 1, Visibility::Public, 1);

    let outcome = core
        .execute_truncate_sql(
            &alice,
            &format!("TRUNCATE TABLE {TABLE} USING OPERATION_ID 'op-direct-entry'"),
        )
        .expect("direct entry point TRUNCATE should succeed");
    let _: engine::sql::exec::TruncateOutcome = outcome;
    assert_eq!(count_star(&core, &alice, TABLE), 0);
}
