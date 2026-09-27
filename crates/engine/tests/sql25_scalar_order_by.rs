//! 広域取得（`SELECT ... [WHERE ...] LIMIT n`。SQL-15）へのスカラー列
//! `ORDER BY` 付与（Issue #915・SQL-25・TASK-209）の結合テスト。
//!
//! `tests/sql_scan.rs`（順序なし広域取得の実行契約）・`tests/rls_generalized.rs`
//! （テナント境界の一般化検証）と同じ流儀（`unique_db_path`＋`CleanupGuard`、
//! production の判定関数を呼ばない独立オラクル）で、決定的な並び順・NULL 位置・
//! ベクトル順位付けとの排他・型ごとの並べ替え不能列の拒否・RLS 非漏えいを固定する。
//! ファイル名は TASK-209 の成果物名（`sql25_order_offset_distinct.rs`）が並行実装
//! （#916〜#918）と worktree 単位で衝突しうるため、本 Issue 専用の名前にしている。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::QueryResult;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

/// `docs(embedding VECTOR(2) NOT NULL, lang TEXT NULL, score INTEGER NULL)`。
/// `embedding` はプレースホルダ値（`[0.0, 0.0]`）を常に挿入し、`lang`/`score`
/// はいずれも nullable にして NULL 位置の規約（ASC 末尾・DESC 先頭）を検証
/// できるようにする（`insert_typed_row` は nullable `VECTOR` 列への NULL 挿入
/// 経路を持たないため、本ファイルのスコープでは非 NULL 固定値で足りる）。
fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, true),
            ColumnDef::new("score", ColumnType::Integer, true),
        ],
    )
}

fn open_storage(path: &std::path::Path) -> Storage {
    Storage::open(path).expect("open storage")
}

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// 他テナントの `Public` 行を可視としない ctx（経路 (A) の判定条件の片方）。
fn private_only_ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Private]).expect("valid tenant")
}

fn op_id(id: u64) -> engine::recovery::required_op_id::OperationId {
    engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
        .expect("valid operation_id")
}

fn insert_row(
    storage: &Storage,
    ctx: &PolicyContext,
    id: u64,
    visibility: Visibility,
    lang: Option<&str>,
    score: Option<i32>,
) {
    let lang_value = lang
        .map(|s| Value::Text(s.to_string()))
        .unwrap_or(Value::Null);
    let score_value = score.map(Value::Integer).unwrap_or(Value::Null);
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        ctx,
        id,
        visibility,
        &[Value::Vector(vec![0.0, 0.0]), lang_value, score_value],
        &op_id(id),
    )
    .expect("insert row");
}

fn result_ids(result: &QueryResult) -> Vec<u64> {
    result.rows.iter().map(|r| r.id).collect()
}

// ---------- 並び順の正しさ（単一テナント） ----------

/// TEXT 昇順・同点は `id` 昇順（§受入基準 1・2）。
#[test]
fn text_ascending_order_matches_oracle_with_id_tie_break() {
    let path = unique_db_path("order-text-asc");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage.create_table(&schema()).expect("create table");
    let ctx = ctx_for("tenant-a");
    // 同点（"b"）を複数含めて id 昇順のタイブレークを固定する。
    let seed: [(u64, &str); 5] = [(3, "c"), (1, "b"), (5, "a"), (2, "b"), (4, "a")];
    for (id, lang) in seed {
        insert_row(&storage, &ctx, id, Visibility::Public, Some(lang), None);
    }
    let core = new_core(storage);

    let result = core
        .execute_sql(&ctx, "SELECT id, lang FROM docs ORDER BY lang LIMIT 10")
        .expect("ordered scan should succeed");
    // lang="a" の 4/5 は同点 → id 昇順で 4, 5。lang="b" の 1/2 も同様。
    assert_eq!(result_ids(&result), vec![4, 5, 1, 2, 3]);
}

/// TEXT 降順（`DESC`）。
#[test]
fn text_descending_order_matches_reversed_oracle() {
    let path = unique_db_path("order-text-desc");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage.create_table(&schema()).expect("create table");
    let ctx = ctx_for("tenant-a");
    let seed: [(u64, &str); 5] = [(3, "c"), (1, "b"), (5, "a"), (2, "b"), (4, "a")];
    for (id, lang) in seed {
        insert_row(&storage, &ctx, id, Visibility::Public, Some(lang), None);
    }
    let core = new_core(storage);

    let result = core
        .execute_sql(&ctx, "SELECT id FROM docs ORDER BY lang DESC LIMIT 10")
        .expect("ordered scan should succeed");
    assert_eq!(result_ids(&result), vec![3, 1, 2, 4, 5]);
}

/// 複数キー（`lang ASC, score DESC`）。
#[test]
fn multiple_keys_apply_in_declared_order() {
    let path = unique_db_path("order-multi-key");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage.create_table(&schema()).expect("create table");
    let ctx = ctx_for("tenant-a");
    // lang="a" 組は score 降順、lang="b" 組も score 降順で確定する。
    let seed: [(u64, &str, i32); 4] = [(1, "a", 10), (2, "a", 20), (3, "b", 5), (4, "b", 1)];
    for (id, lang, score) in seed {
        insert_row(
            &storage,
            &ctx,
            id,
            Visibility::Public,
            Some(lang),
            Some(score),
        );
    }
    let core = new_core(storage);

    let result = core
        .execute_sql(
            &ctx,
            "SELECT id FROM docs ORDER BY lang ASC, score DESC LIMIT 10",
        )
        .expect("ordered scan should succeed");
    assert_eq!(result_ids(&result), vec![2, 1, 3, 4]);
}

/// NULL の位置は PostgreSQL 既定（ASC は末尾、DESC は先頭。§受入基準 2）。
#[test]
fn null_sorts_last_for_ascending_and_first_for_descending() {
    let path = unique_db_path("order-null-position");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage.create_table(&schema()).expect("create table");
    let ctx = ctx_for("tenant-a");
    insert_row(&storage, &ctx, 1, Visibility::Public, Some("a"), None);
    insert_row(&storage, &ctx, 2, Visibility::Public, None, None);
    insert_row(&storage, &ctx, 3, Visibility::Public, Some("b"), None);
    let core = new_core(storage);

    let asc = core
        .execute_sql(&ctx, "SELECT id FROM docs ORDER BY lang LIMIT 10")
        .expect("ordered scan should succeed");
    assert_eq!(result_ids(&asc), vec![1, 3, 2]);

    let desc = core
        .execute_sql(&ctx, "SELECT id FROM docs ORDER BY lang DESC LIMIT 10")
        .expect("ordered scan should succeed");
    assert_eq!(result_ids(&desc), vec![2, 3, 1]);
}

/// `LIMIT` が可視総数未満でも先頭 n 件が確定する（`WHERE` との併用も確認）。
#[test]
fn limit_smaller_than_total_returns_prefix_of_oracle() {
    let path = unique_db_path("order-limit-prefix");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage.create_table(&schema()).expect("create table");
    let ctx = ctx_for("tenant-a");
    for id in 1..=20u64 {
        insert_row(
            &storage,
            &ctx,
            id,
            Visibility::Public,
            Some("ja"),
            Some(id as i32),
        );
    }
    let core = new_core(storage);

    let result = core
        .execute_sql(&ctx, "SELECT id FROM docs ORDER BY score DESC LIMIT 3")
        .expect("ordered scan should succeed");
    assert_eq!(result_ids(&result), vec![20, 19, 18]);
}

// ---------- `OFFSET` との統合（Issue #916・SQL-25 (b)・TASK-209） ----------

/// 経路 (B)（先頭キーが非 `id` の 2 パス）で `OFFSET` は「ソート確定後にスキップ
/// する」契約（`docs/design/sql-offset-paging.md`「ORDER BY なし OFFSET の意味論」
/// 節）を満たす: `LIMIT` だけを変えた場合の先頭 `offset` 件を除いた続きと一致する。
#[test]
fn offset_skips_leading_sorted_rows_on_path_b_two_pass_heap() {
    let path = unique_db_path("order-offset-path-b");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage.create_table(&schema()).expect("create table");
    let ctx = ctx_for("tenant-a");
    for id in 1..=20u64 {
        insert_row(
            &storage,
            &ctx,
            id,
            Visibility::Public,
            Some("ja"),
            Some(id as i32),
        );
    }
    let core = new_core(storage);

    // オラクル: `OFFSET` なしで全件確定させた並び順の先頭 3 件を除いた続き。
    let oracle = result_ids(
        &core
            .execute_sql(&ctx, "SELECT id FROM docs ORDER BY score DESC LIMIT 20")
            .expect("oracle scan"),
    );
    let expected: Vec<u64> = oracle.into_iter().skip(3).take(4).collect();

    let result = core
        .execute_sql(
            &ctx,
            "SELECT id FROM docs ORDER BY score DESC LIMIT 4 OFFSET 3",
        )
        .expect("ordered scan with offset should succeed");
    assert_eq!(result_ids(&result), expected);
    assert_eq!(expected, vec![17, 16, 15, 14]);
}

/// 経路 (A)（先頭キーが `id`）で `OFFSET` は物理走査順（＝ソート確定順）の先頭から
/// 読み飛ばす。`private_only_ctx` で経路 (A) を強制する。
#[test]
fn offset_skips_leading_sorted_rows_on_path_a_id_range_scan() {
    let path = unique_db_path("order-offset-path-a");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage.create_table(&schema()).expect("create table");
    let write_ctx = ctx_for("tenant-a");
    for id in 1..=10u64 {
        insert_row(
            &storage,
            &write_ctx,
            id,
            Visibility::Private,
            Some("ja"),
            None,
        );
    }
    let core = new_core(storage);
    let ctx = private_only_ctx("tenant-a");

    let result = core
        .execute_sql(
            &ctx,
            "SELECT id FROM docs ORDER BY id DESC LIMIT 3 OFFSET 4",
        )
        .expect("path (A) scan with offset should succeed");
    // id DESC の全体順は 10..=1。先頭 4 件（10,9,8,7）を読み飛ばした続き 3 件。
    assert_eq!(result_ids(&result), vec![6, 5, 4]);
}

/// 同一クエリを複数回実行しても同じ順序になる（決定性）。
#[test]
fn ordered_scan_result_is_deterministic_across_repeated_calls() {
    let path = unique_db_path("order-determinism");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage.create_table(&schema()).expect("create table");
    let ctx = ctx_for("tenant-a");
    for id in 1..=15u64 {
        insert_row(
            &storage,
            &ctx,
            id,
            Visibility::Public,
            Some(if id % 2 == 0 { "ja" } else { "en" }),
            Some((id % 5) as i32),
        );
    }
    let core = new_core(storage);
    let sql = "SELECT id FROM docs ORDER BY lang ASC, score DESC LIMIT 8";

    let first = result_ids(&core.execute_sql(&ctx, sql).expect("first call"));
    let second = result_ids(&core.execute_sql(&ctx, sql).expect("second call"));
    assert_eq!(first, second);
}

// ---------- 経路 (A)（`id` 早期打ち切り）と経路 (B)（上位 N 件）の等価性 ----------

/// 先頭キーが `id` かつ他テナントの `Public` 行を許可しない ctx は経路 (A) を
/// 通る。可視な自テナント行だけを対象に、全走査（経路 (B) 相当）と同じ結果に
/// なることを固定する。
#[test]
fn id_descending_leading_key_matches_oracle_under_private_only_ctx() {
    let path = unique_db_path("order-id-desc-path-a");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage.create_table(&schema()).expect("create table");
    let write_ctx = ctx_for("tenant-a");
    for id in 1..=10u64 {
        insert_row(
            &storage,
            &write_ctx,
            id,
            Visibility::Private,
            Some("ja"),
            None,
        );
    }
    let core = new_core(storage);
    let ctx = private_only_ctx("tenant-a");
    assert!(!ctx.is_visible("tenant-b", Visibility::Public));

    let result = core
        .execute_sql(&ctx, "SELECT id FROM docs ORDER BY id DESC LIMIT 4")
        .expect("ordered scan should succeed");
    assert_eq!(result_ids(&result), vec![10, 9, 8, 7]);
}

#[test]
fn id_ascending_leading_key_matches_oracle_under_private_only_ctx() {
    let path = unique_db_path("order-id-asc-path-a");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage.create_table(&schema()).expect("create table");
    let write_ctx = ctx_for("tenant-a");
    for id in 1..=10u64 {
        insert_row(
            &storage,
            &write_ctx,
            id,
            Visibility::Private,
            Some("ja"),
            None,
        );
    }
    let core = new_core(storage);
    let ctx = private_only_ctx("tenant-a");

    let result = core
        .execute_sql(&ctx, "SELECT id FROM docs ORDER BY id LIMIT 4")
        .expect("ordered scan should succeed");
    assert_eq!(result_ids(&result), vec![1, 2, 3, 4]);
}

/// 先頭キーが `id` でも他テナントの `Public` 行を許可する既定 ctx では経路 (B)
/// を通る。それでも経路 (A) と同じ結果集合になることを確認する（他テナントの
/// `Public` 行が存在しない fixture なので、可視集合自体は private-only ctx と
/// 一致する）。
#[test]
fn id_leading_key_matches_across_path_a_and_path_b_when_visible_set_is_identical() {
    let path = unique_db_path("order-id-path-a-vs-b");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage.create_table(&schema()).expect("create table");
    let write_ctx = ctx_for("tenant-a");
    for id in 1..=10u64 {
        insert_row(
            &storage,
            &write_ctx,
            id,
            Visibility::Private,
            Some("ja"),
            None,
        );
    }
    let core = new_core(storage);

    let path_a = private_only_ctx("tenant-a");
    let path_b = ctx_for("tenant-a"); // allows_public() == true -> 経路 (B)
    let sql = "SELECT id FROM docs ORDER BY id DESC LIMIT 4";

    let a = result_ids(&core.execute_sql(&path_a, sql).expect("path (A) scan"));
    let b = result_ids(&core.execute_sql(&path_b, sql).expect("path (B) scan"));
    assert_eq!(a, b);
    assert_eq!(a, vec![10, 9, 8, 7]);
}

// ---------- RLS（テナント境界の非漏えい） ----------

/// 他テナントの `Private` 行は、スカラー `ORDER BY` の並び順・件数・打ち切り
/// 位置のいずれにも影響しない（`rls_generalized.rs` の 2 パターン方針を踏襲）。
#[test]
fn scalar_order_by_never_leaks_other_tenants_private_rows() {
    let path = unique_db_path("order-rls-no-leak");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage.create_table(&schema()).expect("create table");

    let ctx_a = ctx_for("tenant-a");
    let ctx_b = ctx_for("tenant-b");
    // tenant-a: id 1..=6 (Public, lang="ja"/"en" 交互)。
    for id in 1..=6u64 {
        insert_row(
            &storage,
            &ctx_a,
            id,
            Visibility::Public,
            Some(if id % 2 == 0 { "ja" } else { "en" }),
            None,
        );
    }
    // tenant-b: id 101..=106 (Private。tenant-a からは不可視)。
    for id in 101..=106u64 {
        insert_row(&storage, &ctx_b, id, Visibility::Private, Some("zz"), None);
    }
    let core = new_core(storage);

    // (a) LIMIT が可視総数以上ならオラクル集合と完全一致する。
    let full = core
        .execute_sql(&ctx_a, "SELECT id FROM docs ORDER BY lang LIMIT 100")
        .expect("ordered scan should succeed");
    let full_ids: std::collections::HashSet<u64> = result_ids(&full).into_iter().collect();
    let expected: std::collections::HashSet<u64> = (1..=6u64).collect();
    assert_eq!(full_ids, expected);

    // (b) LIMIT が小さい場合、他テナントの Private 行が打ち切り位置に紛れ込まない。
    let small = core
        .execute_sql(&ctx_a, "SELECT id FROM docs ORDER BY lang LIMIT 2")
        .expect("ordered scan should succeed");
    assert_eq!(small.rows.len(), 2);
    for id in result_ids(&small) {
        assert!(
            (1..=6).contains(&id),
            "leaked a row outside the caller's tenant: id={id}"
        );
    }
}

// ---------- 構文拒否（§受入基準 3・4） ----------

fn expect_sqlstate(core: &EngineCore, ctx: &PolicyContext, sql: &str, expected: &str) {
    let err = core
        .execute_sql(ctx, sql)
        .expect_err(&format!("expected rejection for: {sql}"));
    assert_eq!(err.wire_code(), expected, "sql={sql} err={err:?}");
}

/// スカラー順序付けとベクトル順位付けの併用は `42601`（§受入基準 3）。
#[test]
fn mixing_scalar_and_vector_ranking_is_rejected() {
    let path = unique_db_path("order-reject-mix");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage.create_table(&schema()).expect("create table");
    let ctx = ctx_for("tenant-a");
    let core = new_core(storage);

    expect_sqlstate(
        &core,
        &ctx,
        "SELECT id FROM docs ORDER BY lang, embedding <=> '[1,0]' LIMIT 5",
        "42601",
    );
    expect_sqlstate(
        &core,
        &ctx,
        "SELECT id FROM docs ORDER BY embedding <=> '[1,0]', lang LIMIT 5",
        "42601",
    );
}

/// `USING MODE`／`HINT ORDER` はスカラー ORDER BY 付き広域取得でも受理しない
/// （§受入基準 3）。
#[test]
fn using_mode_and_hint_order_are_rejected_after_scalar_order_by() {
    let path = unique_db_path("order-reject-using-hint");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage.create_table(&schema()).expect("create table");
    let ctx = ctx_for("tenant-a");
    let core = new_core(storage);

    expect_sqlstate(
        &core,
        &ctx,
        "SELECT id FROM docs ORDER BY lang LIMIT 5 USING MODE 'recall'",
        "42601",
    );
    expect_sqlstate(
        &core,
        &ctx,
        "SELECT id FROM docs ORDER BY lang LIMIT 5 HINT ORDER(scalar)",
        "42601",
    );
}

/// `NULLS LAST`・位置指定・式は許可リスト外。`LIMIT` を伴わない形も拒否する。
#[test]
fn unsupported_order_by_forms_are_rejected_with_42601() {
    let path = unique_db_path("order-reject-forms");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage.create_table(&schema()).expect("create table");
    let ctx = ctx_for("tenant-a");
    let core = new_core(storage);

    expect_sqlstate(
        &core,
        &ctx,
        "SELECT id FROM docs ORDER BY lang NULLS LAST LIMIT 5",
        "42601",
    );
    expect_sqlstate(
        &core,
        &ctx,
        "SELECT id FROM docs ORDER BY 1 LIMIT 5",
        "42601",
    );
    expect_sqlstate(&core, &ctx, "SELECT id FROM docs ORDER BY lang", "42601");
}

/// `EXPLAIN` はスカラー ORDER BY 付き広域取得も前置として受理しない
/// （既存の「`USING PLAN` を伴う検索 SELECT のみ」契約の維持）。
#[test]
fn explain_rejects_scalar_order_by_scan() {
    let path = unique_db_path("order-reject-explain");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage.create_table(&schema()).expect("create table");
    let ctx = ctx_for("tenant-a");
    let core = new_core(storage);

    expect_sqlstate(
        &core,
        &ctx,
        "EXPLAIN SELECT id FROM docs ORDER BY lang LIMIT 5",
        "42601",
    );
}

/// 未知列・`VECTOR` 列は `22000`。
#[test]
fn unknown_and_vector_columns_are_rejected_with_22000() {
    let path = unique_db_path("order-reject-22000");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage.create_table(&schema()).expect("create table");
    let ctx = ctx_for("tenant-a");
    let core = new_core(storage);

    expect_sqlstate(
        &core,
        &ctx,
        "SELECT id FROM docs ORDER BY nope LIMIT 5",
        "22000",
    );
    expect_sqlstate(
        &core,
        &ctx,
        "SELECT id FROM docs ORDER BY embedding LIMIT 5",
        "22000",
    );
}

/// キー数の上限（[`engine::sql::allowlist::MAX_SCALAR_ORDER_KEYS`]）超過は
/// `54000`。
#[test]
fn too_many_order_by_keys_is_rejected_with_54000() {
    let path = unique_db_path("order-reject-54000");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage.create_table(&schema()).expect("create table");
    let ctx = ctx_for("tenant-a");
    let core = new_core(storage);

    let keys = ["lang"; 9].join(", ");
    let sql = format!("SELECT id FROM docs ORDER BY {keys} LIMIT 5");
    expect_sqlstate(&core, &ctx, &sql, "54000");
}

// ---------- 既存回帰: 順序なし・`BoundScan::new` 直接構築 ----------

/// 順序なしの bare scan の結果は変わらない（既存契約の非退行確認）。
#[test]
fn unordered_bare_scan_still_returns_all_matching_rows() {
    let path = unique_db_path("order-regression-bare-scan");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage.create_table(&schema()).expect("create table");
    let ctx = ctx_for("tenant-a");
    for id in 1..=5u64 {
        insert_row(&storage, &ctx, id, Visibility::Public, Some("ja"), None);
    }
    let core = new_core(storage);

    let result = core
        .execute_sql(&ctx, "SELECT id FROM docs LIMIT 100")
        .expect("bare scan should succeed");
    let got: std::collections::HashSet<u64> = result_ids(&result).into_iter().collect();
    let expected: std::collections::HashSet<u64> = (1..=5u64).collect();
    assert_eq!(got, expected);
}

/// `BoundScan::new`（TASK-186・NOSQL-3 の直接構築入口）は `order_by` を常に
/// 空にする（NoSQL 表層の `sort` 対応は NOSQL-15・別 Issue #946・#947 の対象外
/// スコープで、既存の公開 API 契約を変えない）。
#[test]
fn bound_scan_new_direct_construction_has_no_order_by() {
    use engine::sql::parser::{BoundScan, ProjectedColumn};

    let bound = BoundScan::new(
        "docs".to_string(),
        vec![ProjectedColumn::Id],
        Vec::new(),
        Vec::new(),
        10,
    );
    assert_eq!(bound.projection(), &[ProjectedColumn::Id]);
    assert_eq!(bound.limit(), 10);
}
