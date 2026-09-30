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
use engine::sql::parser::DmlLimits;
use engine::sql::SqlOutcome;
use engine::storage::{RowInput, Storage, Visibility};
use std::num::NonZeroUsize;

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
// --- Issue #1200: 影響行数上限の非適用・索引失効・途中 abort の副作用ゼロ ----------
//
// 以降は SQL 表層（`CREATE TABLE` を DDL セッションで実行）で構築した
// `docs` テーブル（`kind`／`body` 列つき）を使う。SQL の INSERT は既定で常に
// `Private` 行になるため、他テナントの行はすべて Private で組み、
// `ctx_for(_, true)`（Public＋Private 許可）で読み出しても他テナントの行が
// 混入しない（`Public` のグローバル可視に依存しないオラクルにできる）。

fn sql_ok(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> SqlOutcome {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(ctx, &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
}

fn sql_err_code(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> String {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(ctx, &mut session, sql)
        .map(|o| panic!("{sql} must fail, got {o:?}"))
        .unwrap_err()
        .wire_code()
        .to_string()
}

fn sql_ids(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<u64> {
    match sql_ok(core, ctx, sql) {
        SqlOutcome::Query(result) => {
            let mut ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
            ids.sort_unstable();
            ids
        }
        other => panic!("expected Query outcome, got {other:?}"),
    }
}

fn new_docs_core(limits: Option<DmlLimits>) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("truncate-table-1200");
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let mut core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    if let Some(limits) = limits {
        core = core.with_dml_limits(limits);
    }
    sql_ok(
        &core,
        &ctx_for("sys", true),
        "CREATE TABLE docs (embedding VECTOR(2) NOT NULL, kind TEXT NOT NULL, body TEXT)",
    );
    (core, guard)
}

/// `id` の偶数を `kind = 'a'`、奇数を `kind = 'b'` として `ids` を投入する
/// （body は hybrid 用に偶数のみ検索語を含める）。
fn seed_docs(
    core: &EngineCore,
    ctx: &PolicyContext,
    ids: std::ops::RangeInclusive<u64>,
    tag: &str,
) {
    for id in ids {
        let (kind, body) = if id % 2 == 0 {
            ("a", "vector database engine")
        } else {
            ("b", "unrelated text")
        };
        sql_ok(
            core,
            ctx,
            &format!(
                "INSERT INTO docs (id, embedding, kind, body) VALUES ({id}, '[{id}.0,0.0]', '{kind}', '{body}') \
                 USING OPERATION_ID 'seed-{tag}-{id}'"
            ),
        );
    }
}

const KIND_A_SQL: &str =
    "SELECT id FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[10.0,0.0]' LIMIT 50";
const HYBRID_SQL: &str = "SELECT id FROM docs ORDER BY hybrid_rrf(embedding, '[1.0,0.0]', body, 'vector database') LIMIT 50";

/// SQL-19（影響行数上限 `54000`）は述語形 DELETE にだけ効き、TRUNCATE には
/// 適用されない。上限 1 の構成で、同じ 5 行に対し述語形 DELETE は拒否
/// （副作用ゼロ）、TRUNCATE は成功することを対で固定する。
#[test]
fn truncate_is_not_subject_to_dml_affected_rows_limit() {
    let (core, _guard) = new_docs_core(Some(DmlLimits {
        max_affected_rows: NonZeroUsize::new(1),
        max_insert_rows_per_statement: None,
    }));
    let alice = ctx_for("alice", true);
    seed_docs(&core, &alice, 1..=5, "lim");
    assert_eq!(count_star(&core, &alice, "docs"), 5);

    assert_eq!(
        sql_err_code(
            &core,
            &alice,
            "DELETE FROM docs WHERE id > 0 USING OPERATION_ID 'op-lim-del'"
        ),
        "54000"
    );
    assert_eq!(
        count_star(&core, &alice, "docs"),
        5,
        "rejected DELETE must have no effect"
    );

    let outcome = sql_ok(
        &core,
        &alice,
        "TRUNCATE TABLE docs USING OPERATION_ID 'op-lim-trunc'",
    );
    assert!(matches!(outcome, SqlOutcome::Truncate(_)));
    assert_eq!(count_star(&core, &alice, "docs"), 0);
}

/// SQL-22/SQL-18: TRUNCATE 後、温まったスカラー二次索引から古いヒットが返らず、
/// 再投入後は新しい行だけが索引経路で返る。他テナント（bob）の結果は不変。
#[test]
fn truncate_invalidates_scalar_index_and_keeps_other_tenant_intact() {
    let (core, _guard) = new_docs_core(None);
    let alice = ctx_for("alice", true);
    let bob = ctx_for("bob", true);
    seed_docs(&core, &alice, 1..=10, "sa");
    seed_docs(&core, &bob, 501..=510, "sb");

    let alice_before = sql_ids(&core, &alice, KIND_A_SQL);
    assert_eq!(alice_before, vec![2, 4, 6, 8, 10]);
    assert_eq!(sql_ids(&core, &alice, KIND_A_SQL), alice_before);
    let bob_before = sql_ids(&core, &bob, KIND_A_SQL);
    assert_eq!(bob_before, vec![502, 504, 506, 508, 510]);
    assert_eq!(sql_ids(&core, &bob, KIND_A_SQL), bob_before);
    let warm = core.scalar_index_cache_stats();
    assert!(
        warm.index_scans > 0,
        "index path must have been consumed before TRUNCATE"
    );

    sql_ok(
        &core,
        &alice,
        "TRUNCATE TABLE docs USING OPERATION_ID 'op-idx-trunc'",
    );

    assert!(sql_ids(&core, &alice, KIND_A_SQL).is_empty());
    assert!(
        core.scalar_index_cache_stats().builds > warm.builds,
        "table generation bump must force a rebuild"
    );
    assert_eq!(sql_ids(&core, &bob, KIND_A_SQL), bob_before);

    seed_docs(&core, &alice, 101..=110, "sa2");
    let scans_before = core.scalar_index_cache_stats().index_scans;
    let first = sql_ids(&core, &alice, KIND_A_SQL);
    let second = sql_ids(&core, &alice, KIND_A_SQL);
    assert_eq!(first, vec![102, 104, 106, 108, 110]);
    assert_eq!(second, first);
    assert!(
        core.scalar_index_cache_stats().index_scans > scans_before,
        "rebuilt index must be consumed again"
    );
    assert_eq!(sql_ids(&core, &bob, KIND_A_SQL), bob_before);
}

/// SQL-22/SQL-18: TRUNCATE 後の疎索引（hybrid）も世代失効し、旧 id を返さない。
#[test]
fn truncate_invalidates_sparse_index_and_keeps_other_tenant_intact() {
    let (core, _guard) = new_docs_core(None);
    let alice = ctx_for("alice", true);
    let bob = ctx_for("bob", true);
    seed_docs(&core, &alice, 1..=10, "ha");
    seed_docs(&core, &bob, 501..=510, "hb");

    let alice_before = sql_ids(&core, &alice, HYBRID_SQL);
    assert!(!alice_before.is_empty());
    assert_eq!(sql_ids(&core, &alice, HYBRID_SQL), alice_before);
    let bob_before = sql_ids(&core, &bob, HYBRID_SQL);
    assert!(!bob_before.is_empty());
    let warm = core.sparse_index_cache_stats();
    assert!(
        warm.hits > 0,
        "sparse index must have been reused before TRUNCATE"
    );

    sql_ok(
        &core,
        &alice,
        "TRUNCATE TABLE docs USING OPERATION_ID 'op-sparse-trunc'",
    );

    assert!(sql_ids(&core, &alice, HYBRID_SQL).is_empty());
    assert_eq!(sql_ids(&core, &bob, HYBRID_SQL), bob_before);

    seed_docs(&core, &alice, 101..=110, "ha2");
    let after = sql_ids(&core, &alice, HYBRID_SQL);
    assert!(!after.is_empty());
    assert!(
        after.iter().all(|id| (101..=110).contains(id)),
        "no pre-TRUNCATE id may be returned: {after:?}"
    );
    let stats = core.sparse_index_cache_stats();
    assert!(
        stats.misses > warm.misses || stats.stale_evictions > warm.stale_evictions,
        "sparse index must have been rebuilt after the generation bump"
    );
    assert_eq!(sql_ids(&core, &bob, HYBRID_SQL), bob_before);
}

fn create_fk_pair(core: &EngineCore) {
    let sys = ctx_for("sys", true);
    sql_ok(
        core,
        &sys,
        "CREATE TABLE parents (code TEXT UNIQUE, name TEXT)",
    );
    sql_ok(
        core,
        &sys,
        "CREATE TABLE children (parent_id BIGINT REFERENCES parents, note TEXT)",
    );
}

fn assert_parents_untouched(core: &EngineCore, alice: &PolicyContext, bob: &PolicyContext) {
    assert_eq!(
        sql_ids(core, alice, "SELECT id FROM parents LIMIT 100"),
        vec![1, 2, 3]
    );
    assert_eq!(
        sql_ids(core, bob, "SELECT id FROM parents LIMIT 100").len(),
        3
    );
    // 一意索引が無傷（TRUNCATE の途中でクリアされていない）なら重複は 23505。
    assert_eq!(
        sql_err_code(
            core,
            alice,
            "INSERT INTO parents (id, code) VALUES (9, 'c1') USING OPERATION_ID 'op-dup-code'"
        ),
        "23505"
    );
}

/// SQL-22（TABLE-17 との相互作用）: TRUNCATE は行削除後に FK 検査で `23503`
/// になる経路があるが、その write txn 全体が abort し副作用が一切残らない
/// （行・一意索引・台帳）。再オープン後も同じ。
#[test]
fn truncate_aborted_by_fk_violation_leaves_no_side_effects_even_after_reopen() {
    let path = unique_db_path("truncate-table-1200-abort");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx_for("alice", true);
    let bob = ctx_for("bob", true);
    {
        let storage = Storage::open(&path).expect("open storage");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        create_fk_pair(&core);
        sql_ok(
            &core,
            &alice,
            "INSERT INTO parents (id, code) VALUES (1, 'c1'), (2, 'c2'), (3, 'c3') USING OPERATION_ID 'op-pa'",
        );
        sql_ok(
            &core,
            &bob,
            "INSERT INTO parents (id, code) VALUES (1, 'c1'), (2, 'c2'), (3, 'c3') USING OPERATION_ID 'op-pb'",
        );
        sql_ok(
            &core,
            &alice,
            "INSERT INTO children (id, parent_id) VALUES (1, 1) USING OPERATION_ID 'op-ca'",
        );

        assert_eq!(
            sql_err_code(
                &core,
                &alice,
                "TRUNCATE TABLE parents USING OPERATION_ID 'op-abort'"
            ),
            "23503"
        );
        assert_parents_untouched(&core, &alice, &bob);
    }

    // 再オープン後も副作用ゼロ。
    let storage = Storage::open(&path).expect("reopen storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    assert_parents_untouched(&core, &alice, &bob);

    // 台帳が書かれていない証明: 子行を消せば同じ operation_id で成功する
    // （abort 済み txn の台帳記録が残っていれば 23505 になる）。
    sql_ok(
        &core,
        &alice,
        "DELETE FROM children WHERE id = 1 USING OPERATION_ID 'op-cd'",
    );
    let outcome = sql_ok(
        &core,
        &alice,
        "TRUNCATE TABLE parents USING OPERATION_ID 'op-abort'",
    );
    assert!(matches!(outcome, SqlOutcome::Truncate(_)));
    assert!(sql_ids(&core, &alice, "SELECT id FROM parents LIMIT 100").is_empty());
    assert_eq!(
        sql_ids(&core, &bob, "SELECT id FROM parents LIMIT 100").len(),
        3
    );
}
