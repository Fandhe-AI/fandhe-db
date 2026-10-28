//! サブクエリ深さ・実行回数・CTE 定義数・CTE 連鎖深さの起動時設定（Issue #1552。
//! ポインタ: `docs/spec/04-behavior/` SQL-46）の結合テスト。
//!
//! `EngineCore::with_nesting_limits` で設定値を注入し、`execute_sql_in_session`
//! （wire-server の simple／extended プロトコルと NoSQL 表層が共有する実行入口）を
//! production 経路として検証する。確認する契約:
//!
//! - 未注入（既定）はハード上限と同値で、上限ちょうどは成功し +1 は `54000`
//! - 下げた設定値でも、設定値ちょうどは成功し +1 は従来と同じ `54000`
//!   （エラー文には設定値が出る）
//! - 実行回数は WHERE の `IN`／`EXISTS` と投影位置のスカラーサブクエリの双方に効く
//! - `DECLARE CURSOR ... FOR WITH ...` にも CTE の設定値が効く
//! - 格納済みビュー本文の再パースはハード上限のままで、上限を下げた再起動後も
//!   既存ビューを参照できる（設計 D6）

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::nesting_limits::NestingLimits;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DOCS: &str = "docs";

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn seed(storage: &Storage) {
    storage
        .create_table(&TableSchema::new(
            DOCS,
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    for id in 1..=2u64 {
        engine::tenant::insert_typed_row(
            storage,
            DOCS,
            &ctx("alice"),
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![id as f32, 0.0]),
                Value::Text("ja".to_string()),
            ],
            &OperationId::parse(&format!("seed-{id}")).expect("op id"),
        )
        .expect("insert row");
    }
}

fn open_core(path: &std::path::Path, limits: Option<NestingLimits>) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    match limits {
        Some(l) => core.with_nesting_limits(l),
        None => core,
    }
}

/// 表を作成・投入した core（`limits` が `None` なら未注入）。
fn seeded_core(path: &std::path::Path, limits: Option<NestingLimits>) -> EngineCore {
    {
        let storage = Storage::open(path).expect("open storage");
        seed(&storage);
    }
    open_core(path, limits)
}

fn run(core: &EngineCore, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("alice"), &mut session, sql)
}

fn ok(core: &EngineCore, sql: &str) {
    run(core, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {e:?}"));
}

/// `54000` で拒否されることを確認し、エラー文を返す。
fn too_large(core: &EngineCore, sql: &str) -> String {
    match run(core, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => {
            assert_eq!(e.wire_code(), "54000", "{sql}: {e}");
            assert!(matches!(e, SqlSurfaceError::PayloadTooLarge { .. }));
            e.to_string()
        }
    }
}

/// サブクエリの入れ子が `depth` 段の `IN` 文。
fn nested_in(depth: usize) -> String {
    let mut inner = format!("SELECT lang FROM {DOCS} LIMIT 100");
    for _ in 1..depth {
        inner = format!("SELECT lang FROM {DOCS} WHERE lang IN ({inner}) LIMIT 100");
    }
    format!("SELECT id FROM {DOCS} WHERE lang IN ({inner}) LIMIT 100")
}

/// 内側クエリを `n` 回実行する文（深さ 1 の `IN` を AND 連結）。
fn many_in(n: usize) -> String {
    let preds: Vec<String> = (0..n)
        .map(|_| format!("lang IN (SELECT lang FROM {DOCS} LIMIT 100)"))
        .collect();
    format!(
        "SELECT id FROM {DOCS} WHERE {} LIMIT 100",
        preds.join(" AND ")
    )
}

/// 内側クエリを `n` 回実行する文（投影位置のスカラーサブクエリ）。
fn many_scalar(n: usize) -> String {
    let items: Vec<String> = (0..n)
        .map(|i| format!("(SELECT lang FROM {DOCS} LIMIT 1) AS s{i}"))
        .collect();
    format!("SELECT id, {} FROM {DOCS} LIMIT 100", items.join(", "))
}

/// CTE 定義が `n` 件（どれも基底表を直接読む。連鎖なし）の文。
fn cte_defs(n: usize) -> String {
    let defs: Vec<String> = (0..n)
        .map(|i| format!("c{i} AS (SELECT id FROM {DOCS})"))
        .collect();
    format!("WITH {} SELECT id FROM c0 LIMIT 10", defs.join(", "))
}

/// `c0 -> c1 -> ... ` と `len` 件連鎖する文（`len` 件目を主クエリが読む）。
fn cte_chain(len: usize) -> String {
    let mut defs = vec![format!("c0 AS (SELECT id FROM {DOCS})")];
    for i in 1..len {
        defs.push(format!("c{i} AS (SELECT id FROM c{})", i - 1));
    }
    format!(
        "WITH {} SELECT id FROM c{} LIMIT 10",
        defs.join(", "),
        len - 1
    )
}

#[test]
fn default_limits_equal_hard_limits_and_boundaries_hold() {
    let d = NestingLimits::default();
    assert_eq!(
        (
            d.max_subquery_depth(),
            d.max_subquery_executions(),
            d.max_cte_definitions(),
            d.max_cte_nesting_depth()
        ),
        (4, 16, 16, 4)
    );

    let path = unique_db_path("nesting-limits-default");
    let _g = CleanupGuard(path.clone());
    let core = seeded_core(&path, None);
    assert_eq!(core.nesting_limits(), NestingLimits::default());

    ok(&core, &nested_in(4));
    too_large(&core, &nested_in(5));
    ok(&core, &many_in(16));
    too_large(&core, &many_in(17));
    ok(&core, &cte_defs(16));
    too_large(&core, &cte_defs(17));
    ok(&core, &cte_chain(4));
    too_large(&core, &cte_chain(5));
}

#[test]
fn lowered_limits_apply_exactly_and_report_the_configured_value() {
    let path = unique_db_path("nesting-limits-lowered");
    let _g = CleanupGuard(path.clone());
    let core = seeded_core(&path, Some(NestingLimits::new(2, 3, 2, 1)));

    ok(&core, &nested_in(2));
    assert!(too_large(&core, &nested_in(3)).contains("limit 2"));

    ok(&core, &many_in(3));
    assert!(too_large(&core, &many_in(4)).contains("limit 3"));

    ok(&core, &cte_defs(2));
    too_large(&core, &cte_defs(3));

    ok(&core, &cte_chain(1));
    too_large(&core, &cte_chain(2));
}

#[test]
fn execution_budget_applies_to_exists_and_projection_subqueries() {
    let path = unique_db_path("nesting-limits-exec-paths");
    let _g = CleanupGuard(path.clone());
    let core = seeded_core(&path, Some(NestingLimits::new(4, 2, 16, 4)));

    let exists = |n: usize| {
        let preds: Vec<String> = (0..n)
            .map(|_| format!("EXISTS (SELECT lang FROM {DOCS} LIMIT 1)"))
            .collect();
        format!(
            "SELECT id FROM {DOCS} WHERE {} LIMIT 100",
            preds.join(" AND ")
        )
    };
    ok(&core, &exists(2));
    too_large(&core, &exists(3));

    ok(&core, &many_scalar(2));
    too_large(&core, &many_scalar(3));
}

#[test]
fn declare_cursor_for_with_honors_cte_limits() {
    let path = unique_db_path("nesting-limits-cursor");
    let _g = CleanupGuard(path.clone());
    let core = seeded_core(&path, Some(NestingLimits::new(4, 16, 2, 1)));

    let declare = |inner: String| format!("DECLARE cur CURSOR FOR {inner}");
    too_large(&core, &declare(cte_defs(3)));
    too_large(&core, &declare(cte_chain(2)));
}

#[test]
fn stored_view_bodies_are_revalidated_with_hard_limits() {
    let path = unique_db_path("nesting-limits-view");
    let _g = CleanupGuard(path.clone());
    {
        // 既定（ハード上限）の core でビューを作る。本文は 3 定義・2 段連鎖。
        let core = seeded_core(&path, None);
        ok(
            &core,
            "CREATE VIEW v AS WITH c0 AS (SELECT id FROM docs), c1 AS (SELECT id FROM c0), \
             c2 AS (SELECT id FROM c1) SELECT id FROM c2 LIMIT 5",
        );
    }
    // 上限を下げて再起動しても、既存ビューは参照できる（セッション SQL の CTE には効く）。
    let core = open_core(&path, Some(NestingLimits::new(1, 1, 1, 1)));
    ok(&core, "SELECT id FROM v LIMIT 10");
    too_large(&core, &cte_defs(2));
}
// ---- 相関サブクエリ・派生テーブル経路（Issue #1850。ポインタ: SQL-46・SQL-61・SQL-62）----

const TAGS: &str = "tags";

/// `docs` に加えて相関の内側表 `tags(doc_id)` を作成・投入した core。
///
/// `tags` は doc 1 に 2 行、doc 2 に 1 行を持つ（相関 EXISTS／IN／スカラーの内側表）。
fn seeded_core_with_tags(path: &std::path::Path, limits: Option<NestingLimits>) -> EngineCore {
    {
        let storage = Storage::open(path).expect("open storage");
        seed(&storage);
        storage
            .create_table(&TableSchema::new(
                TAGS,
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("doc_id", ColumnType::BigInt, true),
                ],
            ))
            .expect("create tags");
        for (id, doc) in [(1u64, 1i64), (2, 1), (3, 2)] {
            engine::tenant::insert_typed_row(
                &storage,
                TAGS,
                &ctx("alice"),
                id,
                Visibility::Public,
                &[Value::Vector(vec![0.0, 0.0]), Value::BigInt(doc)],
                &OperationId::parse(&format!("seed-tag-{id}")).expect("op id"),
            )
            .expect("insert tag");
        }
    }
    open_core(path, limits)
}

/// 述語 `pred` を `n` 個 AND 連結した文。
fn and_preds(n: usize, pred: &str) -> String {
    let preds: Vec<&str> = (0..n).map(|_| pred).collect();
    format!(
        "SELECT id FROM {DOCS} WHERE {} LIMIT 100",
        preds.join(" AND ")
    )
}

/// 実行回数 `limit` の core で、`pred` が「`limit` 個は成功・+1 個は 54000」になることを確かめる。
fn assert_exec_budget(name: &str, pred: &str, limit: usize) {
    let path = unique_db_path(name);
    let _g = CleanupGuard(path.clone());
    let core = seeded_core_with_tags(&path, Some(NestingLimits::new(4, limit, 16, 4)));
    let n = limit;
    ok(&core, &and_preds(n, pred));
    let msg = too_large(&core, &and_preds(n + 1, pred));
    assert!(msg.contains(&format!("limit {limit}")), "{msg}");
}

#[test]
fn execution_budget_applies_to_correlated_exists_and_in() {
    let t = format!("FROM {TAGS} WHERE {TAGS}.doc_id = {DOCS}.id");
    assert_exec_budget("nl-corr-exists", &format!("EXISTS (SELECT 1 {t})"), 1);
    assert_exec_budget(
        "nl-corr-not-exists",
        &format!("NOT EXISTS (SELECT 1 {t})"),
        2,
    );
    assert_exec_budget(
        "nl-corr-in",
        &format!("id IN (SELECT {TAGS}.doc_id {t})"),
        1,
    );
    assert_exec_budget(
        "nl-corr-not-in",
        &format!("id NOT IN (SELECT {TAGS}.doc_id {t})"),
        2,
    );
}

#[test]
fn execution_budget_applies_to_correlated_scalars() {
    let path = unique_db_path("nl-corr-scalar");
    let _g = CleanupGuard(path.clone());
    let core = seeded_core_with_tags(&path, Some(NestingLimits::new(4, 1, 16, 4)));
    let agg = format!("(SELECT count(*) FROM {TAGS} WHERE {TAGS}.doc_id = {DOCS}.id)");
    let plain =
        format!("(SELECT {TAGS}.doc_id FROM {TAGS} WHERE {TAGS}.doc_id = {DOCS}.id LIMIT 1)");

    // 投影位置: 集計形・非集計形・式の中。1 件は成功し、2 件目で 54000。
    ok(&core, &format!("SELECT id, {agg} FROM {DOCS} LIMIT 100"));
    ok(&core, &format!("SELECT id, {plain} FROM {DOCS} LIMIT 100"));
    ok(
        &core,
        &format!("SELECT id, 1 + {agg} FROM {DOCS} LIMIT 100"),
    );
    let msg = too_large(
        &core,
        &format!("SELECT id, 1 + {agg}, 2 + {agg} FROM {DOCS} LIMIT 100"),
    );
    assert!(msg.contains("limit 1"), "{msg}");
    too_large(
        &core,
        &format!("SELECT id, {plain}, {plain} FROM {DOCS} LIMIT 100"),
    );

    // WHERE 位置。
    ok(
        &core,
        &format!("SELECT id FROM {DOCS} WHERE id <= {agg} LIMIT 100"),
    );
    too_large(
        &core,
        &format!("SELECT id FROM {DOCS} WHERE id <= {agg} AND id >= {agg} LIMIT 100"),
    );
}

#[test]
fn subquery_depth_applies_to_correlated_exists() {
    let path = unique_db_path("nl-corr-depth");
    let _g = CleanupGuard(path.clone());
    let core = seeded_core_with_tags(&path, Some(NestingLimits::new(1, 16, 16, 4)));
    let one = format!(
        "SELECT id FROM {DOCS} WHERE EXISTS (SELECT 1 FROM {TAGS} WHERE {TAGS}.doc_id = {DOCS}.id) \
         LIMIT 100"
    );
    ok(&core, &one);
    let two = format!(
        "SELECT id FROM {DOCS} WHERE EXISTS (SELECT 1 FROM {TAGS} WHERE {TAGS}.doc_id = {DOCS}.id \
         AND {TAGS}.id IN (SELECT id FROM {TAGS} LIMIT 100)) LIMIT 100"
    );
    let msg = too_large(&core, &two);
    assert!(msg.contains("limit 1"), "{msg}");
}

/// 深さ `n` の派生テーブル入れ子（最内側が実表。SQL-62）。
fn nested_derived(n: usize) -> String {
    let mut inner = DOCS.to_string();
    for k in (0..n).rev() {
        inner = format!("(SELECT lang FROM {inner}) a{k}");
    }
    format!("SELECT * FROM {inner}")
}

#[test]
fn derived_table_nesting_honors_lowered_depth_and_bodies_reject_subqueries() {
    let path = unique_db_path("nl-derived");
    let _g = CleanupGuard(path.clone());
    let core = seeded_core_with_tags(&path, Some(NestingLimits::new(2, 2, 16, 4)));
    // 深さ: 設定値ちょうどは成功し、+1 は 54000（メッセージに設定値）。
    ok(&core, &nested_derived(2));
    assert!(too_large(&core, &nested_derived(3)).contains("limit 2"));
    // 派生テーブル本文内のサブクエリは現状受理されず、設定値を迂回する経路にもならない
    // （fail-closed。受理されるようになった場合は設定値の適用を本テストへ追加すること）。
    let sql = format!(
        "SELECT t.id FROM (SELECT id, lang FROM {DOCS} WHERE lang IN \
         (SELECT lang FROM {DOCS} LIMIT 100)) t LIMIT 100"
    );
    let e = run(&core, &sql).expect_err("subquery in derived body must be rejected");
    assert!(matches!(e.wire_code(), "42601" | "0A000"), "{e}");
}

#[test]
fn default_limits_leave_correlated_and_derived_paths_unchanged() {
    let path = unique_db_path("nl-default-paths");
    let _g = CleanupGuard(path.clone());
    let core = seeded_core_with_tags(&path, None);
    ok(
        &core,
        &and_preds(
            16,
            &format!("EXISTS (SELECT 1 FROM {TAGS} WHERE {TAGS}.doc_id = {DOCS}.id)"),
        ),
    );
    ok(&core, &nested_derived(4));
}
