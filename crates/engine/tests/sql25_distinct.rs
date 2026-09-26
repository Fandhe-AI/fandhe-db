//! `SELECT DISTINCT`・`COUNT(DISTINCT <expr>)`（SQL-25 (c)・TASK-209）の
//! 結合テスト。ポインタ: `docs/spec/05-tasks.md` TASK-209・
//! `docs/spec/04-behavior/sql-surface.md` SQL-25 (c)（関連: SQL-8・SQL-13・
//! SQL-14）・`docs/spec/04-behavior/rls.md` RLS-7, RLS-8。
//!
//! `tests/sql_group_by.rs`（TASK-167・SQL-14）と同じ流儀（`unique_db_path`＋
//! `CleanupGuard`、決定的擬似乱数 xorshift64*、`PolicyContext::is_visible` を
//! 呼ばない独立オラクルで可視集合・異なり値を手計算し、`EngineCore::execute_sql`
//! （SQL 経由）の結果と突き合わせる）で検証する。`SELECT DISTINCT` は
//! `GROUP BY` 実行器への脱糖なので、対象外の入力（複数列・`*`・非 TEXT 列）の
//! 拒否確認は `sql::allowlist` の単体テストに委ね、ここでは実データを用いた
//! 受理経路・RLS 境界・NULL 契約・上限を確認する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::Cell;
use engine::storage::{Storage, Visibility};
use std::collections::BTreeSet;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

struct Xorshift64 {
    state: u64,
}

impl Xorshift64 {
    fn new(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

const DIM: usize = 4;
const TABLE: &str = "docs";
const TENANTS: [&str; 3] = ["tenant-a", "tenant-b", "tenant-c"];
const LANGS: [&str; 5] = ["ja", "en", "fr", "de", "es"];
const ROWS_PER_TENANT: u64 = 12;

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM as u32), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

#[derive(Clone)]
struct RowTruth {
    // `id` は本ファイルのオラクル関数群では直接参照しないが、コーパス生成の
    // 一意性検証・将来の COUNT(DISTINCT id) 系オラクル拡張のために保持する
    // （`tests/sql_group_by.rs::RowTruth` と同じ構造を踏襲）。
    #[allow(dead_code)]
    id: u64,
    tenant: &'static str,
    visibility: Visibility,
    lang: &'static str,
}

fn is_allowed(row: &RowTruth, viewer_tenant: &str, allow_private: bool) -> bool {
    match row.visibility {
        Visibility::Public => true,
        Visibility::Private => row.tenant == viewer_tenant && allow_private,
    }
}

fn seed_multi_tenant_corpus(storage: &Storage) -> Vec<RowTruth> {
    storage.create_table(&schema()).expect("create table");
    let mut rng = Xorshift64::new(0x1357_9BDF_2468_ACE0);
    let mut truths = Vec::new();
    let mut id = 1u64;
    for &tenant in TENANTS.iter() {
        for i in 0..ROWS_PER_TENANT {
            let visibility = if i % 3 == 0 {
                Visibility::Private
            } else {
                Visibility::Public
            };
            let lang = LANGS[(rng.next_u64() as usize) % LANGS.len()];
            let ctx =
                PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                    .expect("valid tenant");
            engine::tenant::insert_typed_row(
                storage,
                TABLE,
                &ctx,
                id,
                visibility,
                &[
                    Value::Vector(vec![0.0f32; DIM]),
                    Value::Text(lang.to_string()),
                ],
                &engine::recovery::required_op_id::OperationId::parse(&format!("op-{id}"))
                    .expect("valid operation_id"),
            )
            .expect("insert row");
            truths.push(RowTruth {
                id,
                tenant,
                visibility,
                lang,
            });
            id += 1;
        }
    }
    truths
}

fn open_storage(path: &std::path::Path) -> Storage {
    Storage::open(path).expect("open storage")
}

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn visible_rows<'a>(
    truths: &'a [RowTruth],
    viewer_tenant: &str,
    allow_private: bool,
) -> Vec<&'a RowTruth> {
    truths
        .iter()
        .filter(|t| is_allowed(t, viewer_tenant, allow_private))
        .collect()
}

fn ctx_for(tenant: &str, allow_private: bool) -> PolicyContext {
    if allow_private {
        PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
            .expect("valid tenant")
    } else {
        PolicyContext::new(tenant).expect("valid tenant")
    }
}

fn as_integer(cell: &Cell) -> u64 {
    match cell {
        Cell::Integer(v) => *v,
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
}

fn as_text(cell: &Cell) -> Option<String> {
    match cell {
        Cell::Text(v) => Some(v.clone()),
        Cell::Null => None,
        other => panic!("expected Cell::Text or Cell::Null, got {other:?}"),
    }
}

/// オラクル: 可視行に現れる `lang` の異なり値集合（NULL 無し。本コーパスは
/// `lang` を `nullable: false` で宣言しているため NULL 行は生じない）。
fn oracle_distinct_langs(visible: &[&RowTruth]) -> BTreeSet<&'static str> {
    visible.iter().map(|r| r.lang).collect()
}

// --- SELECT DISTINCT: 可視集合の異なり値がオラクルと一致 ------------------------

#[test]
fn select_distinct_matches_independent_oracle_across_tenants() {
    let path = unique_db_path("sql25-distinct-select");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);

    for &tenant in TENANTS.iter() {
        for allow_private in [false, true] {
            let ctx = ctx_for(tenant, allow_private);
            let visible = visible_rows(&truths, tenant, allow_private);
            let expected = oracle_distinct_langs(&visible);

            let result = core
                .execute_sql(&ctx, "SELECT DISTINCT lang FROM docs")
                .expect("SELECT DISTINCT should succeed");

            let got: BTreeSet<String> = result
                .rows
                .iter()
                .map(|row| as_text(&row.cells[0]).expect("lang is not nullable in this corpus"))
                .collect();
            let expected_owned: BTreeSet<String> = expected.iter().map(|s| s.to_string()).collect();
            assert_eq!(
                got, expected_owned,
                "distinct set mismatch tenant={tenant} allow_private={allow_private}"
            );
        }
    }
}

#[test]
fn select_distinct_with_where_and_order_by_and_limit() {
    let path = unique_db_path("sql25-distinct-where-order-limit");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let _truths = seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a", true);

    let result = core
        .execute_sql(
            &ctx,
            "SELECT DISTINCT lang FROM docs WHERE lang = 'ja' ORDER BY lang DESC LIMIT 10",
        )
        .expect("SELECT DISTINCT with WHERE/ORDER BY/LIMIT should succeed");
    for row in &result.rows {
        assert_eq!(as_text(&row.cells[0]).as_deref(), Some("ja"));
    }
}

// --- COUNT(DISTINCT col): NULL 除外・空集合で 0・可視集合に対する異なり数 --------

#[test]
fn count_distinct_matches_independent_oracle_and_excludes_invisible_tenants() {
    let path = unique_db_path("sql25-count-distinct");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);

    for &tenant in TENANTS.iter() {
        for allow_private in [false, true] {
            let ctx = ctx_for(tenant, allow_private);
            let visible = visible_rows(&truths, tenant, allow_private);
            let expected = oracle_distinct_langs(&visible).len() as u64;

            let result = core
                .execute_sql(&ctx, "SELECT COUNT(DISTINCT lang) AS n FROM docs")
                .expect("COUNT(DISTINCT lang) should succeed");
            assert_eq!(result.rows.len(), 1);
            assert_eq!(
                as_integer(&result.rows[0].cells[0]),
                expected,
                "COUNT(DISTINCT lang) mismatch tenant={tenant} allow_private={allow_private}"
            );
        }
    }
}

/// RLS-7・RLS-8: 他テナントだけが多数の異なり値を持っていても、自テナントの
/// `COUNT(DISTINCT)` はその基数の影響を受けない（漏えいしない）。
#[test]
fn count_distinct_does_not_leak_other_tenant_cardinality() {
    let path = unique_db_path("sql25-count-distinct-rls");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage.create_table(&schema()).expect("create table");

    // tenant-a: 3 種類だけの lang を Public で 30 行。
    let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut id = 1u64;
    for i in 0..30u64 {
        let lang = LANGS[(i % 3) as usize];
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx_a,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![0.0f32; DIM]),
                Value::Text(lang.to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!("op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
        id += 1;
    }

    // tenant-b: 全 5 種類の lang を Private で多数行（tenant-a より基数が大きい）。
    // `Visibility::Private` はテナント自身にしか見えないため（`Visibility::Public`
    // はテナント間で共有される既存契約。`is_allowed` 参照）、tenant-a から
    // 見えないことを構造的に保証する。
    let ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    for i in 0..50u64 {
        let lang = LANGS[(i % LANGS.len() as u64) as usize];
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx_b,
            id,
            Visibility::Private,
            &[
                Value::Vector(vec![0.0f32; DIM]),
                Value::Text(lang.to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!("op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
        id += 1;
    }

    let core = new_core(storage);
    let result = core
        .execute_sql(&ctx_a, "SELECT COUNT(DISTINCT lang) AS n FROM docs")
        .expect("COUNT(DISTINCT lang) should succeed for tenant-a");
    assert_eq!(
        as_integer(&result.rows[0].cells[0]),
        3,
        "tenant-a must see only its own 3 distinct values, not tenant-b's 5"
    );
}

#[test]
fn count_distinct_on_id_counts_visible_rows_since_id_has_no_duplicates() {
    let path = unique_db_path("sql25-count-distinct-id");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a", true);
    let visible = visible_rows(&truths, "tenant-a", true);

    let result = core
        .execute_sql(&ctx, "SELECT COUNT(DISTINCT id) AS n FROM docs")
        .expect("COUNT(DISTINCT id) should succeed");
    assert_eq!(as_integer(&result.rows[0].cells[0]), visible.len() as u64);
}

// --- 型別の拒否（22000）: VECTOR・ARRAY・JSON はいずれも束縛段で拒否 -----------

#[test]
fn count_distinct_on_vector_column_is_rejected_as_invalid_input() {
    let path = unique_db_path("sql25-count-distinct-vector-reject");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let _truths = seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a", true);

    let err = core
        .execute_sql(&ctx, "SELECT COUNT(DISTINCT embedding) FROM docs")
        .expect_err("COUNT(DISTINCT <VECTOR column>) must be rejected");
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn select_distinct_on_vector_column_is_rejected_as_invalid_input() {
    let path = unique_db_path("sql25-select-distinct-vector-reject");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let _truths = seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a", true);

    let err = core
        .execute_sql(&ctx, "SELECT DISTINCT embedding FROM docs")
        .expect_err("SELECT DISTINCT <VECTOR column> must be rejected");
    assert_eq!(err.wire_code(), "22000");
}

// --- COUNT(DISTINCT) と GROUP BY の組み合わせ ---------------------------------

#[test]
fn count_distinct_combined_with_group_by_matches_oracle_per_group() {
    let path = unique_db_path("sql25-count-distinct-group-by");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage.create_table(&schema()).expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    // 2 グループ相当を作るため、`lang` に加えて `id` の異なり値を数える列は
    // 無いので、代わりに同一 lang グループ内の `id` の COUNT(DISTINCT) が
    // グループの行数と一致すること（id は重複しないため）を確認する。
    for (id, lang) in (1u64..).zip(["ja", "ja", "ja", "en", "en"]) {
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![0.0f32; DIM]),
                Value::Text(lang.to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!("op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }

    let core = new_core(storage);
    let result = core
        .execute_sql(
            &ctx,
            "SELECT lang, COUNT(DISTINCT id) AS n FROM docs GROUP BY lang ORDER BY lang ASC",
        )
        .expect("COUNT(DISTINCT id) GROUP BY lang should succeed");
    assert_eq!(result.rows.len(), 2);
    let ja = &result.rows[0];
    let en = &result.rows[1];
    assert_eq!(as_text(&ja.cells[0]).as_deref(), Some("en"));
    assert_eq!(as_integer(&ja.cells[1]), 2);
    assert_eq!(as_text(&en.cells[0]).as_deref(), Some("ja"));
    assert_eq!(as_integer(&en.cells[1]), 3);
}

// --- 後方互換: 列名 `distinct` を持つ表への既存の解釈が壊れない ----------------

#[test]
fn column_named_distinct_keeps_existing_projection_semantics() {
    let path = unique_db_path("sql25-distinct-column-name-compat");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let schema = TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM as u32), false),
            ColumnDef::new("distinct", ColumnType::Text, false),
        ],
    );
    storage.create_table(&schema).expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &ctx,
        1,
        Visibility::Public,
        &[
            Value::Vector(vec![0.0f32; DIM]),
            Value::Text("x".to_string()),
        ],
        &engine::recovery::required_op_id::OperationId::parse("op-1").expect("valid operation_id"),
    )
    .expect("insert row");

    let core = new_core(storage);

    // `SELECT distinct FROM t LIMIT n`: 列名としての解釈を維持する。
    let result = core
        .execute_sql(&ctx, "SELECT distinct FROM docs LIMIT 5")
        .expect("bare column named 'distinct' must remain a projection");
    assert_eq!(as_text(&result.rows[0].cells[0]).as_deref(), Some("x"));

    // `COUNT(distinct)`: 集計対象の列名としての解釈を維持する（DISTINCT 修飾では
    // ない）。
    let result = core
        .execute_sql(&ctx, "SELECT COUNT(distinct) AS n FROM docs")
        .expect("COUNT(distinct) (column named 'distinct') must remain accepted");
    assert_eq!(as_integer(&result.rows[0].cells[0]), 1);
}

// --- 上限: SELECT DISTINCT は既存の MAX_GROUPS（10,000）を継承する -------------

#[test]
fn select_distinct_rejects_when_distinct_value_count_exceeds_max_groups() {
    let path = unique_db_path("sql25-distinct-max-groups");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage.create_table(&schema()).expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    // MAX_GROUPS(10,000) を超える異なり値（各行が一意な lang 値を持つ）を挿入する。
    for i in 0..10_001u64 {
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx,
            i + 1,
            Visibility::Public,
            &[
                Value::Vector(vec![0.0f32; DIM]),
                Value::Text(format!("lang-{i}")),
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!("op-{}", i + 1))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }

    let core = new_core(storage);
    let err = core
        .execute_sql(&ctx, "SELECT DISTINCT lang FROM docs")
        .expect_err("SELECT DISTINCT must reject when distinct values exceed MAX_GROUPS");
    assert_eq!(err.wire_code(), "54000");
}

// --- NULL 契約: COUNT(DISTINCT) は NULL を除外し、SELECT DISTINCT は NULL を
//     1 行にまとめて末尾に置く（受け入れ条件 4「NULL の扱いを明示する」） -----

fn schema_nullable_text() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM as u32), false),
            ColumnDef::new("lang", ColumnType::Text, true),
        ],
    )
}

#[test]
fn count_distinct_excludes_null_and_is_zero_when_all_null() {
    let path = unique_db_path("sql25-count-distinct-null");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&schema_nullable_text())
        .expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    // ja, ja, NULL, NULL, en の 5 行 → 異なり値は {ja, en} の 2 件（NULL 除外）。
    for (id, lang) in (1u64..).zip([Some("ja"), Some("ja"), None, None, Some("en")]) {
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![0.0f32; DIM]),
                lang.map(|s| Value::Text(s.to_string()))
                    .unwrap_or(Value::Null),
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!("op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }

    let core = new_core(storage);
    let result = core
        .execute_sql(&ctx, "SELECT COUNT(DISTINCT lang) AS n FROM docs")
        .expect("COUNT(DISTINCT lang) should succeed");
    assert_eq!(as_integer(&result.rows[0].cells[0]), 2);

    // 全行 NULL → 空集合契約で 0（PostgreSQL 互換。既存の COUNT と同じ）。
    let path2 = unique_db_path("sql25-count-distinct-all-null");
    let _guard2 = CleanupGuard(path2.clone());
    let storage2 = open_storage(&path2);
    storage2
        .create_table(&schema_nullable_text())
        .expect("create table");
    for id in 1u64..=3 {
        engine::tenant::insert_typed_row(
            &storage2,
            TABLE,
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(vec![0.0f32; DIM]), Value::Null],
            &engine::recovery::required_op_id::OperationId::parse(&format!("op2-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }
    let core2 = new_core(storage2);
    let result2 = core2
        .execute_sql(&ctx, "SELECT COUNT(DISTINCT lang) AS n FROM docs")
        .expect("COUNT(DISTINCT lang) over all-NULL column should succeed");
    assert_eq!(as_integer(&result2.rows[0].cells[0]), 0);
}

#[test]
fn select_distinct_merges_null_rows_into_a_single_trailing_row() {
    let path = unique_db_path("sql25-select-distinct-null");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&schema_nullable_text())
        .expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    for (id, lang) in (1u64..).zip([Some("ja"), None, Some("en"), None, Some("ja")]) {
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![0.0f32; DIM]),
                lang.map(|s| Value::Text(s.to_string()))
                    .unwrap_or(Value::Null),
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!("op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }

    let core = new_core(storage);
    let result = core
        .execute_sql(&ctx, "SELECT DISTINCT lang FROM docs")
        .expect("SELECT DISTINCT should succeed");
    // NULL は 1 行にまとめられ常に末尾（既存の GROUP BY 契約を継承。ASC/DESC を
    // 問わず末尾に置かれる契約は `sql::group_by::order_with_nulls_last` 参照）。
    assert_eq!(result.rows.len(), 3);
    assert_eq!(as_text(&result.rows[0].cells[0]).as_deref(), Some("en"));
    assert_eq!(as_text(&result.rows[1].cells[0]).as_deref(), Some("ja"));
    assert_eq!(as_text(&result.rows[2].cells[0]), None);
}

// --- 数値正準化: -0.0 と 0.0 は同一視される（`canon_f64`） -------------------

#[test]
fn count_distinct_on_double_column_treats_negative_zero_as_equal_to_zero() {
    let path = unique_db_path("sql25-count-distinct-double-neg-zero");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let schema = TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM as u32), false),
            ColumnDef::new("score", ColumnType::Double, false),
        ],
    );
    storage.create_table(&schema).expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    for (id, score) in (1u64..).zip([0.0f64, -0.0f64, 1.5f64]) {
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(vec![0.0f32; DIM]), Value::Double(score)],
            &engine::recovery::required_op_id::OperationId::parse(&format!("op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }

    let core = new_core(storage);
    let result = core
        .execute_sql(&ctx, "SELECT COUNT(DISTINCT score) AS n FROM docs")
        .expect("COUNT(DISTINCT score) should succeed");
    // 0.0 と -0.0 は同一視されるため異なり値は {0.0, 1.5} の 2 件。
    assert_eq!(as_integer(&result.rows[0].cells[0]), 2);
}

// --- 上限: COUNT(DISTINCT) の累計バイト数上限（16 MiB） ------------------------

#[test]
fn count_distinct_rejects_when_accumulated_key_bytes_exceed_budget() {
    let path = unique_db_path("sql25-count-distinct-byte-budget");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&schema_nullable_text())
        .expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    // 一意な TEXT 値（各 200,000 バイト。`row_codec::MAX_TEXT_FIELD_LEN`〔4 MiB〕
    // を大きく下回るため列単体の上限には抵触しない）を 90 行分挿入する。
    // `DistinctBudget` の累計上限は 16 MiB（エントリごとの固定オーバーヘッド
    // 込み）で、90 件 ×（200,000 + 32）バイト ≈ 18.0 MiB は上限を超えるため、
    // 途中の挿入で `54000` になる（`16,777,216 / 200,032 ≈ 83.9` 件目付近）。
    const VALUE_LEN: usize = 200_000;
    for id in 1u64..=90 {
        // 値ごとに先頭バイトを変えて一意にする。
        let mut value = vec![b'a'; VALUE_LEN];
        value[0..8].copy_from_slice(format!("{id:08}").as_bytes());
        let text = String::from_utf8(value).expect("ascii bytes are valid utf-8");
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(vec![0.0f32; DIM]), Value::Text(text)],
            &engine::recovery::required_op_id::OperationId::parse(&format!("op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }

    let core = new_core(storage);
    let err = core
        .execute_sql(&ctx, "SELECT COUNT(DISTINCT lang) AS n FROM docs")
        .expect_err("COUNT(DISTINCT) must reject when accumulated key bytes exceed the budget");
    assert_eq!(err.wire_code(), "54000");
}

// --- HAVING が COUNT(DISTINCT) の別名を参照できる -----------------------------

#[test]
fn having_can_reference_count_distinct_alias() {
    let path = unique_db_path("sql25-count-distinct-having");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage.create_table(&schema()).expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    // lang="ja": id 1..=3 (3 件), lang="en": id 4..=4 (1 件)。
    for (id, lang) in (1u64..).zip(["ja", "ja", "ja", "en"]) {
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![0.0f32; DIM]),
                Value::Text(lang.to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!("op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }

    let core = new_core(storage);
    let result = core
        .execute_sql(
            &ctx,
            "SELECT lang, COUNT(DISTINCT id) AS n FROM docs GROUP BY lang HAVING n >= 2",
        )
        .expect("HAVING referencing COUNT(DISTINCT) alias should succeed");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(as_text(&result.rows[0].cells[0]).as_deref(), Some("ja"));
    assert_eq!(as_integer(&result.rows[0].cells[1]), 3);
}
