//! `INTEGER` / `BIGINT` 列型（Issue #881、対象ビヘイビア: TABLE-13・TABLE-6・
//! TASK-196）の結合テスト。ポインタ: `docs/spec/05-tasks.md` TASK-196・
//! `docs/spec/04-behavior/data-model.md`。関連ポインタ: RECOVER-1／10（`operation_id`
//! 必須化・台帳照合による再送判定）・RLS-7／9（暗黙のテナント境界適用・他テナント
//! 存在情報の非漏えい）。
//!
//! `Storage::create_table`／`alter_table_add_column`（Rust API）とカタログの往復、
//! SQL `INSERT` の数値リテラル束縛・範囲検査（`22003`）・型不一致（`22000`）、
//! 単一行・複数行 `VALUES`・`UPDATE SET`・UPSERT の副作用ゼロ拒否、RLS 越境、
//! 台帳照合を `EngineCore::execute_sql_in_session` 経由（実 `Storage` +
//! `CpuScalarProvider`）で検証する（`sql_update_single_row.rs`・
//! `sql_upsert.rs`・`insert_multi_row.rs` と同じ流儀）。
//! 詳細な設計判断は `docs/design/column-type-integer.md` 参照。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("n", ColumnType::Integer, false),
            ColumnDef::new("b", ColumnType::BigInt, false),
            ColumnDef::new("nn", ColumnType::Integer, true),
        ],
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn count_rows(core: &EngineCore, ctx: &PolicyContext) -> u64 {
    let result = core
        .execute_sql(ctx, &format!("SELECT COUNT(*) FROM {TABLE}"))
        .expect("count select should succeed");
    match result.rows[0].cells[0] {
        Cell::Integer(n) => n,
        ref other => panic!("expected Cell::Integer for COUNT(*), got {other:?}"),
    }
}

// --- カタログ往復（Rust API）------------------------------------------------

#[test]
fn integer_and_bigint_columns_roundtrip_through_storage_and_reopen() {
    let path = unique_db_path("column-type-integer-catalog-roundtrip");
    let _cleanup = CleanupGuard(path.clone());
    {
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema()).expect("create table");
        storage
            .alter_table_add_column(TABLE, ColumnDef::new("extra_b", ColumnType::BigInt, true))
            .expect("alter table add column");
        let fetched = storage.get_table_schema(TABLE).expect("get schema");
        assert_eq!(fetched.columns[1].ty, ColumnType::Integer);
        assert_eq!(fetched.columns[2].ty, ColumnType::BigInt);
        assert_eq!(fetched.columns[4].ty, ColumnType::BigInt);
        assert!(fetched.columns[4].nullable);
    }
    // 再オープン後も往復する。
    {
        let storage = Storage::open(&path).expect("reopen storage");
        let fetched = storage.get_table_schema(TABLE).expect("get schema");
        assert_eq!(fetched.columns[1].ty, ColumnType::Integer);
        assert_eq!(fetched.columns[2].ty, ColumnType::BigInt);
    }
}

// --- SQL INSERT の境界値往復 -------------------------------------------------

#[test]
fn insert_and_select_roundtrip_boundary_values_via_scan_and_distance() {
    let path = unique_db_path("column-type-integer-boundary-roundtrip");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();

    core.execute_sql_in_session(
        &ctx,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, n, b) VALUES \
             (1, '[0.1,0.2]', {}, {}) USING OPERATION_ID 'op-1'",
            i32::MIN,
            i64::MAX
        ),
    )
    .expect("insert min/max boundary should succeed");
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, n, b) VALUES \
             (2, '[0.1,0.2]', {}, {}) USING OPERATION_ID 'op-2'",
            i32::MAX,
            i64::MIN
        ),
    )
    .expect("insert max/min boundary should succeed");

    // 広域取得（scan）経路。
    let scanned = core
        .execute_sql(
            &ctx,
            &format!("SELECT n, b FROM {TABLE} WHERE id = 1 LIMIT 10"),
        )
        .expect("scan select should succeed");
    assert_eq!(scanned.rows.len(), 1);
    assert_eq!(
        scanned.rows[0].cells,
        vec![
            Cell::SignedInteger(i64::from(i32::MIN)),
            Cell::SignedInteger(i64::MAX),
        ]
    );

    // DISTANCE 経路（VectorArena 経由）。
    let ranked = core
        .execute_sql(
            &ctx,
            &format!("SELECT n, b FROM {TABLE} ORDER BY embedding <=> '[0.1,0.2]' LIMIT 10"),
        )
        .expect("distance select should succeed");
    assert_eq!(ranked.rows.len(), 2);
    let by_n: std::collections::HashMap<i64, i64> = ranked
        .rows
        .iter()
        .map(|r| match (&r.cells[0], &r.cells[1]) {
            (Cell::SignedInteger(n), Cell::SignedInteger(b)) => (*n, *b),
            other => panic!("unexpected cells: {other:?}"),
        })
        .collect();
    assert_eq!(by_n.get(&i64::from(i32::MIN)), Some(&i64::MAX));
    assert_eq!(by_n.get(&i64::from(i32::MAX)), Some(&i64::MIN));
}

// --- 範囲外リテラル（22003）と副作用ゼロ -----------------------------------

#[test]
fn out_of_range_integer_literal_is_rejected_with_22003_and_has_no_side_effects() {
    let path = unique_db_path("column-type-integer-out-of-range");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();

    let over_i32 = i64::from(i32::MAX) + 1;
    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, n, b) VALUES (1, '[0.1,0.2]', {over_i32}, 0) \
                 USING OPERATION_ID 'op-1'"
            ),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22003");
    assert_eq!(count_rows(&core, &ctx), 0, "row must not be inserted");

    let under_i32 = i64::from(i32::MIN) - 1;
    let err2 = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, n, b) VALUES (1, '[0.1,0.2]', {under_i32}, 0) \
                 USING OPERATION_ID 'op-2'"
            ),
        )
        .unwrap_err();
    assert_eq!(err2.wire_code(), "22003");
    assert_eq!(count_rows(&core, &ctx), 0);

    // BIGINT の範囲外は i128 が必要な桁数（i64::MAX + 1 / i64::MIN - 1）で表現する。
    let err3 = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, n, b) VALUES (1, '[0.1,0.2]', 0, {}) \
                 USING OPERATION_ID 'op-3'",
                i128::from(i64::MAX) + 1
            ),
        )
        .unwrap_err();
    assert_eq!(err3.wire_code(), "22003");
    assert_eq!(count_rows(&core, &ctx), 0);

    let err4 = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, n, b) VALUES (1, '[0.1,0.2]', 0, {}) \
                 USING OPERATION_ID 'op-4'",
                i128::from(i64::MIN) - 1
            ),
        )
        .unwrap_err();
    assert_eq!(err4.wire_code(), "22003");
    assert_eq!(count_rows(&core, &ctx), 0);
}

/// 複数行 `VALUES` の一部が範囲外の場合、バッチ全体が原子的に拒否される
/// （SQL-16 の原子性契約。1 行目は正当だが 2 行目が範囲外）。
#[test]
fn out_of_range_literal_in_multi_row_insert_rejects_the_whole_batch() {
    let path = unique_db_path("column-type-integer-multi-row-out-of-range");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();

    let over_i32 = i64::from(i32::MAX) + 1;
    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, n, b) VALUES \
                 (1, '[0.1,0.2]', 1, 1), (2, '[0.3,0.4]', {over_i32}, 1) \
                 USING OPERATION_ID 'op-multi'"
            ),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22003");
    assert_eq!(
        count_rows(&core, &ctx),
        0,
        "no row from the batch must persist"
    );
}

/// UPDATE SET の範囲外リテラルは対象行の探索より前に拒否され、既存行は不変。
#[test]
fn out_of_range_literal_in_update_set_is_rejected_before_touching_the_row() {
    let path = unique_db_path("column-type-integer-update-out-of-range");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();

    core.execute_sql_in_session(
        &ctx,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, n, b) VALUES (1, '[0.1,0.2]', 1, 1) \
             USING OPERATION_ID 'op-seed'"
        ),
    )
    .expect("seed insert should succeed");

    let over_i32 = i64::from(i32::MAX) + 1;
    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            &format!(
                "UPDATE {TABLE} SET n = {over_i32} WHERE id = 1 USING OPERATION_ID 'op-update'"
            ),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22003");

    let after = core
        .execute_sql(
            &ctx,
            &format!("SELECT n FROM {TABLE} WHERE id = 1 LIMIT 10"),
        )
        .expect("select after failed update");
    assert_eq!(after.rows[0].cells, vec![Cell::SignedInteger(1)]);
}

/// UPSERT の `DO UPDATE SET` 側の範囲外リテラルも同様に拒否される。
#[test]
fn out_of_range_literal_in_upsert_do_update_is_rejected() {
    let path = unique_db_path("column-type-integer-upsert-out-of-range");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();

    core.execute_sql_in_session(
        &ctx,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, n, b) VALUES (1, '[0.1,0.2]', 1, 1) \
             USING OPERATION_ID 'op-seed'"
        ),
    )
    .expect("seed insert should succeed");

    let over_i32 = i64::from(i32::MAX) + 1;
    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, n, b) VALUES (1, '[0.1,0.2]', 2, 2) \
                 ON CONFLICT (id) DO UPDATE SET n = {over_i32} \
                 USING OPERATION_ID 'op-upsert'"
            ),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22003");

    let after = core
        .execute_sql(
            &ctx,
            &format!("SELECT n FROM {TABLE} WHERE id = 1 LIMIT 10"),
        )
        .expect("select after failed upsert");
    assert_eq!(after.rows[0].cells, vec![Cell::SignedInteger(1)]);
}

// --- 型不一致（22000） -------------------------------------------------------

#[test]
fn non_integer_literal_forms_are_rejected_with_22000() {
    let path = unique_db_path("column-type-integer-type-mismatch");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();

    // 小数リテラル。
    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, n, b) VALUES (1, '[0.1,0.2]', 1.5, 1) \
                 USING OPERATION_ID 'op-1'"
            ),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22P02");
    assert_eq!(count_rows(&core, &ctx), 0);

    // 文字列リテラル。
    let err2 = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, n, b) VALUES (1, '[0.1,0.2]', 'x', 1) \
                 USING OPERATION_ID 'op-2'"
            ),
        )
        .unwrap_err();
    assert_eq!(err2.wire_code(), "22P02");
    assert_eq!(count_rows(&core, &ctx), 0);
}

/// 整数列を `WHERE`（数値リテラルとの比較）・`GROUP BY`・式で参照する形は
/// Issue #1183（SQL-24・SQL-26・TABLE-13 ポインタ）で受理される（旧契約は
/// レーン A 未実装のため `22000` で拒否していた）。空テーブルに対する束縛・実行が
/// 成功することを固定する（値の意味論は `scalar_types_predicates` 等で検証）。
#[test]
fn integer_column_reference_in_where_group_by_and_expr_is_accepted() {
    let path = unique_db_path("column-type-integer-accepted-refs");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("tenant-a");

    core.execute_sql(
        &ctx,
        &format!("SELECT id FROM {TABLE} WHERE n = 1 LIMIT 10"),
    )
    .expect("WHERE on INTEGER column must be accepted");

    core.execute_sql(&ctx, &format!("SELECT n, COUNT(*) FROM {TABLE} GROUP BY n"))
        .expect("GROUP BY on INTEGER column must be accepted");

    core.execute_sql(
        &ctx,
        &format!(
            "SELECT vec_div(embedding, n) FROM {TABLE} \
             ORDER BY embedding <=> '[0.1,0.2]' LIMIT 10"
        ),
    )
    .expect("INTEGER column in an expression must be accepted");
}

/// `SUM`/`AVG`/`MIN`/`MAX(<INTEGER>/<BIGINT>)` は Issue #892 で受理された
/// （TABLE-13・SQL-13）。`SUM` は `BIGINT` 相当（`Cell::SignedInteger`）・
/// `AVG` は `DOUBLE PRECISION` 相当（`Cell::Float`）で返る。
#[test]
fn sum_of_integer_column_succeeds_after_issue_892() {
    let path = unique_db_path("column-type-integer-sum-after-892");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();

    core.execute_sql_in_session(
        &ctx,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, n, b) VALUES \
             (1, '[0.1,0.2]', 3, 30) USING OPERATION_ID 'op-1'"
        ),
    )
    .expect("insert row 1 should succeed");
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, n, b) VALUES \
             (2, '[0.1,0.2]', 5, 50) USING OPERATION_ID 'op-2'"
        ),
    )
    .expect("insert row 2 should succeed");

    let sum = core
        .execute_sql(&ctx, &format!("SELECT SUM(n) FROM {TABLE}"))
        .expect("SUM(INTEGER) must succeed after Issue #892");
    assert_eq!(sum.rows[0].cells, vec![Cell::SignedInteger(8)]);

    let avg = core
        .execute_sql(&ctx, &format!("SELECT AVG(b) FROM {TABLE}"))
        .expect("AVG(BIGINT) must succeed after Issue #892");
    assert_eq!(avg.rows[0].cells, vec![Cell::Float(40.0)]);

    let min_max = core
        .execute_sql(&ctx, &format!("SELECT MIN(n), MAX(n) FROM {TABLE}"))
        .expect("MIN/MAX(INTEGER) must succeed after Issue #892");
    assert_eq!(
        min_max.rows[0].cells,
        vec![Cell::SignedInteger(3), Cell::SignedInteger(5)]
    );

    // 空集合（`WHERE` で全行除外）は NULL。
    let empty = core
        .execute_sql(&ctx, &format!("SELECT SUM(n) FROM {TABLE} WHERE id = 999"))
        .expect("SUM over an empty result set must still succeed");
    assert_eq!(empty.rows[0].cells, vec![Cell::Null]);
}

// --- 負数リテラルの既存挙動からの変化（42601 → 22000） ----------------------

/// TEXT 列・疑似列 `id` への負数リテラルは、本 Issue 前は構造的に受理しない
/// （`42601`）挙動だったが、単項マイナスを許可リストへ受理するようにした結果、
/// 型不一致（`22000`）へ変わる（PR 本文に明記する既知の挙動変化）。
#[test]
fn negative_literal_into_text_column_or_id_is_now_22000_instead_of_42601() {
    let path = unique_db_path("column-type-integer-negative-into-text");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();

    // `n`／`b` は必須なので有効な値を渡しつつ、`id` 疑似列へ負数を渡すケースを
    // 検証する。
    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, n, b) VALUES (-1, '[0.1,0.2]', 1, 1) \
                 USING OPERATION_ID 'op-1'"
            ),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");
}

// --- RLS 越境 ----------------------------------------------------------------

#[test]
fn integer_and_bigint_values_do_not_leak_across_tenants() {
    let path = unique_db_path("column-type-integer-rls");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let alice = ctx_for("alice");
    let bob = ctx_for("bob");
    let mut session = SessionState::default();

    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, n, b) VALUES (1, '[0.1,0.2]', 42, -42) \
             USING OPERATION_ID 'op-alice'"
        ),
    )
    .expect("alice insert should succeed");

    // bob からは alice の Private 行は不可視（COUNT(*) が 0 のまま）。
    assert_eq!(count_rows(&core, &bob), 0);

    // 同一 id を bob が挿入しても応答は成功し、bob 視点では自分の値のみ見える
    // （TABLE-12: 物理キーはテナントで分離されている）。
    core.execute_sql_in_session(
        &bob,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, n, b) VALUES (1, '[0.5,0.6]', 7, -7) \
             USING OPERATION_ID 'op-bob'"
        ),
    )
    .expect("bob insert with same id should succeed");

    let bob_view = core
        .execute_sql(
            &bob,
            &format!("SELECT n, b FROM {TABLE} WHERE id = 1 LIMIT 10"),
        )
        .expect("bob select should succeed");
    assert_eq!(
        bob_view.rows[0].cells,
        vec![Cell::SignedInteger(7), Cell::SignedInteger(-7)]
    );

    let alice_view = core
        .execute_sql(
            &alice,
            &format!("SELECT n, b FROM {TABLE} WHERE id = 1 LIMIT 10"),
        )
        .expect("alice select should succeed");
    assert_eq!(
        alice_view.rows[0].cells,
        vec![Cell::SignedInteger(42), Cell::SignedInteger(-42)]
    );
}

// --- 台帳照合（RECOVER-10） --------------------------------------------------

#[test]
fn resending_the_same_operation_id_with_identical_integer_values_is_23505() {
    let path = unique_db_path("column-type-integer-ledger-duplicate");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();

    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, n, b) VALUES (1, '[0.1,0.2]', 1, 1) \
         USING OPERATION_ID 'op-resend'"
    );
    core.execute_sql_in_session(&ctx, &mut session, &sql)
        .expect("first insert should succeed");
    let err = core
        .execute_sql_in_session(&ctx, &mut session, &sql)
        .unwrap_err();
    assert_eq!(err.wire_code(), "23505");
    assert_eq!(count_rows(&core, &ctx), 1);
}

#[test]
fn resending_the_same_operation_id_with_different_integer_value_is_22023() {
    let path = unique_db_path("column-type-integer-ledger-mismatch");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();

    core.execute_sql_in_session(
        &ctx,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, n, b) VALUES (1, '[0.1,0.2]', 1, 1) \
             USING OPERATION_ID 'op-mismatch'"
        ),
    )
    .expect("first insert should succeed");
    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, n, b) VALUES (1, '[0.1,0.2]', 2, 1) \
                 USING OPERATION_ID 'op-mismatch'"
            ),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22023");
    assert_eq!(count_rows(&core, &ctx), 1);
}

// --- 成功経路（UPDATE SET・UPSERT DO UPDATE） -------------------------------

/// `UPDATE ... SET n = ..., b = ...`（境界値・負数を含む）の成功経路。
/// `validate_set_assignments`（`tenant.rs`）の `Integer`／`BigInt` 分岐が
/// 正しく機能していること（既定の型不一致腕に落ちて `22000`／`XX000` に
/// ならないこと）を read-back で確認する。
#[test]
fn update_set_integer_and_bigint_succeeds_and_reads_back() {
    let path = unique_db_path("column-type-integer-update-success");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();

    core.execute_sql_in_session(
        &ctx,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, n, b) VALUES (1, '[0.1,0.2]', 1, 1) \
             USING OPERATION_ID 'op-seed'"
        ),
    )
    .expect("seed insert should succeed");

    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            &format!(
                "UPDATE {TABLE} SET n = -5, b = {} WHERE id = 1 USING OPERATION_ID 'op-update'",
                i64::MAX
            ),
        )
        .expect("update should succeed");
    match outcome {
        SqlOutcome::Update(o) => assert_eq!(o.rows_affected, 1),
        other => panic!("expected SqlOutcome::Update, got {other:?}"),
    }

    let after = core
        .execute_sql(
            &ctx,
            &format!("SELECT n, b FROM {TABLE} WHERE id = 1 LIMIT 10"),
        )
        .expect("select after update should succeed");
    assert_eq!(
        after.rows[0].cells,
        vec![Cell::SignedInteger(-5), Cell::SignedInteger(i64::MAX)]
    );
}

/// `UPSERT ... ON CONFLICT (id) DO UPDATE SET n = ...` の成功経路。
#[test]
fn upsert_do_update_set_integer_succeeds_and_reads_back() {
    let path = unique_db_path("column-type-integer-upsert-success");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();

    core.execute_sql_in_session(
        &ctx,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, n, b) VALUES (1, '[0.1,0.2]', 1, 1) \
             USING OPERATION_ID 'op-seed'"
        ),
    )
    .expect("seed insert should succeed");

    core.execute_sql_in_session(
        &ctx,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, n, b) VALUES (1, '[0.1,0.2]', 2, 2) \
             ON CONFLICT (id) DO UPDATE SET n = 42 \
             USING OPERATION_ID 'op-upsert'"
        ),
    )
    .expect("upsert do update should succeed");

    let after = core
        .execute_sql(
            &ctx,
            &format!("SELECT n, b FROM {TABLE} WHERE id = 1 LIMIT 10"),
        )
        .expect("select after upsert should succeed");
    // n は DO UPDATE SET で 42 に上書きされ、SET 対象でない b（既存値 1）は
    // read-merge-write の `merge_encode_scalar_columns` 経路（`ScalarRef`
    // 既存値の書き戻し）でそのまま保持される。
    assert_eq!(
        after.rows[0].cells,
        vec![Cell::SignedInteger(42), Cell::SignedInteger(1)]
    );
}
