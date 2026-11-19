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

use crate::temp_db;
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
    // NULL は 1 行にまとめられ、`ORDER BY` を書かない既定順では末尾（既存の GROUP BY
    // 契約を継承。明示した ORDER BY の NULL 位置は Issue #1185 で PostgreSQL 既定
    // 〔ASC は末尾・DESC は先頭〕にそろえた。`sql::group_by::cmp_cells_pg_nulls` 参照）。
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

// --- SELECT DISTINCT ON（Issue #3260）: 構文解析・ORDER BY 先頭一致・fail-closed ----------

/// 全テナント（private 許可の有無を含む）で同一の SQLSTATE・文言になること（可否は文だけで決まり、
/// 行の内容・他テナントの存在に依存しない）を確認する補助。
fn distinct_on_outcome_is_tenant_independent(sql: &str) -> (String, String) {
    let path = unique_db_path("sql25-distinct-on");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let _truths = seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);
    let mut seen: Option<(String, String)> = None;
    for tenant in TENANTS {
        for allow_private in [false, true] {
            let ctx = ctx_for(tenant, allow_private);
            let err = core
                .execute_sql(&ctx, sql)
                .expect_err("this DISTINCT ON form must be rejected");
            let outcome = (err.wire_code().to_string(), err.to_string());
            match &seen {
                Some(prev) => assert_eq!(prev, &outcome, "outcome must not depend on tenant"),
                None => seen = Some(outcome),
            }
        }
    }
    seen.expect("at least one tenant")
}

#[test]
fn distinct_on_with_mismatched_order_by_is_42p10_for_every_tenant() {
    for sql in [
        "SELECT DISTINCT ON (lang) lang FROM docs ORDER BY id, lang LIMIT 10",
        "SELECT DISTINCT ON (lang) lang FROM docs ORDER BY id LIMIT 10",
        "SELECT DISTINCT ON (lang) lang FROM docs ORDER BY id, lang;",
    ] {
        let (code, msg) = distinct_on_outcome_is_tenant_independent(sql);
        assert_eq!(code, "42P10", "{sql}");
        assert_eq!(
            msg,
            "SELECT DISTINCT ON expressions must match initial ORDER BY expressions"
        );
    }
}

/// 可視行から lang ごとの代表（`ORDER BY lang, id DESC` なら id 最大、`ORDER BY lang, id` なら最小）を
/// 独立に計算するオラクル。`BTreeMap` の昇順が `ORDER BY lang`（C 照合のバイト順）の出力順になる。
fn oracle_representatives(
    visible: &[&RowTruth],
    pick_max_id: bool,
) -> std::collections::BTreeMap<&'static str, u64> {
    let mut out: std::collections::BTreeMap<&'static str, u64> = std::collections::BTreeMap::new();
    for r in visible {
        out.entry(r.lang)
            .and_modify(|cur| {
                if (pick_max_id && r.id > *cur) || (!pick_max_id && r.id < *cur) {
                    *cur = r.id;
                }
            })
            .or_insert(r.id);
    }
    out
}

fn text_and_id_rows(result: &engine::sql::exec::QueryResult) -> Vec<(String, u64)> {
    result
        .rows
        .iter()
        .map(|row| {
            (
                as_text(&row.cells[0]).expect("lang is never NULL"),
                as_integer(&row.cells[1]),
            )
        })
        .collect()
}

/// SQL-25（Issue #3261）: 整合した DISTINCT ON は各 ON キーの先頭行（ソート順で最初）だけを返し、
/// 全テナント・private 許可の有無のいずれでも独立オラクルと一致する（不可視行は代表行・件数に現れない）。
#[test]
fn distinct_on_returns_first_row_per_group_across_tenants() {
    let path = unique_db_path("sql25-distinct-on-exec");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);
    for tenant in TENANTS {
        for allow_private in [false, true] {
            let ctx = ctx_for(tenant, allow_private);
            let visible = visible_rows(&truths, tenant, allow_private);
            for (sql, pick_max) in [
                (
                    "SELECT DISTINCT ON (lang) lang, id FROM docs ORDER BY lang, id DESC",
                    true,
                ),
                (
                    "SELECT DISTINCT ON (lang) lang, id FROM docs ORDER BY lang, id;",
                    false,
                ),
                (
                    "SELECT DISTINCT ON (lang) lang, id FROM docs ORDER BY lang DESC, id LIMIT 100",
                    false,
                ),
            ] {
                let descending_lang = sql.contains("lang DESC");
                let result = core.execute_sql(&ctx, sql).expect(sql);
                let mut expected: Vec<(String, u64)> = oracle_representatives(&visible, pick_max)
                    .into_iter()
                    .map(|(l, id)| (l.to_string(), id))
                    .collect();
                if descending_lang {
                    expected.reverse();
                }
                assert_eq!(
                    text_and_id_rows(&result),
                    expected,
                    "{tenant} {allow_private} {sql}"
                );
            }
            // ORDER BY なし: 実効キーは ON キー ASC・同点は id 昇順（契約は docs/design/select-distinct-on.md §6。
            // 専用フィクスチャでの網羅は sql25_distinct_on_determinism.rs）。代表 id まで順序込みで固定する。
            let result = core
                .execute_sql(&ctx, "SELECT DISTINCT ON (lang) lang, id FROM docs")
                .expect("distinct on without order by");
            let expected: Vec<(String, u64)> = oracle_representatives(&visible, false)
                .into_iter()
                .map(|(l, id)| (l.to_string(), id))
                .collect();
            assert_eq!(
                text_and_id_rows(&result),
                expected,
                "{tenant} {allow_private}"
            );
        }
    }
}

/// 関数の暗黙の出力名（`lower`）を ON／ORDER BY キーに書いても、物理列 `lower` ではなく
/// 投影式 `lower(lang)` へ束縛される（Issue #3261 のレビュー指摘。物理列が無くても 42703 にならない）。
#[test]
fn distinct_on_implicit_function_output_name_binds_to_expression() {
    let path = unique_db_path("sql25-distinct-on-implicit-name");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let _truths = seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a", true);
    for sql in [
        "SELECT DISTINCT ON (lower) lower(lang) FROM docs",
        "SELECT DISTINCT ON (lower) lower(lang) FROM docs ORDER BY lower",
    ] {
        let result = core.execute_sql(&ctx, sql).expect(sql);
        let got: Vec<String> = result
            .rows
            .iter()
            .map(|r| as_text(&r.cells[0]).expect("lower"))
            .collect();
        let uniq: BTreeSet<&String> = got.iter().collect();
        assert_eq!(got.len(), uniq.len(), "one row per key: {sql}");
        assert!(!got.is_empty(), "{sql}");
    }
}

/// 重複排除は OFFSET／LIMIT より前（OFFSET はグループ単位で効く）。式・別名・序数キーも受理する。
#[test]
fn distinct_on_dedups_before_offset_and_accepts_alias_ordinal_expression_keys() {
    let path = unique_db_path("sql25-distinct-on-offset");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a", true);
    let visible = visible_rows(&truths, "tenant-a", true);
    let reps = oracle_representatives(&visible, true);
    assert!(reps.len() >= 3, "corpus must have several groups");
    let langs: Vec<(&str, u64)> = reps.into_iter().collect();
    let result = core
        .execute_sql(
            &ctx,
            "SELECT DISTINCT ON (lang) lang, id FROM docs ORDER BY lang, id DESC LIMIT 1 OFFSET 1",
        )
        .expect("offset");
    assert_eq!(
        text_and_id_rows(&result),
        vec![(langs[1].0.to_string(), langs[1].1)]
    );
    for sql in [
        "SELECT DISTINCT ON (l) lang AS l, id FROM docs ORDER BY l, id DESC",
        "SELECT DISTINCT ON (1) lang, id FROM docs ORDER BY 1, id DESC",
        "SELECT DISTINCT ON (lower(lang)) lang, id FROM docs ORDER BY lower(lang), id DESC",
    ] {
        let result = core.execute_sql(&ctx, sql).expect(sql);
        let got = text_and_id_rows(&result);
        let want: Vec<(String, u64)> = langs.iter().map(|(l, id)| (l.to_string(), *id)).collect();
        assert_eq!(got, want, "{sql}");
    }
}

/// ORDER BY に無い ON キーは末尾へ暗黙に昇順で足される（`ON (a, b) ORDER BY a` は b も昇順）。
#[test]
fn distinct_on_multi_key_appends_missing_on_key_ascending() {
    let path = unique_db_path("sql25-distinct-on-multikey");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);
    let ctx = ctx_for("tenant-b", true);
    let visible = visible_rows(&truths, "tenant-b", true);
    let result = core
        .execute_sql(
            &ctx,
            "SELECT DISTINCT ON (lang, id) lang, id FROM docs ORDER BY lang DESC",
        )
        .expect("multi key");
    // `id` は一意なので全可視行が返り、(lang DESC, id ASC) の順になる。
    let mut want: Vec<(String, u64)> = visible.iter().map(|r| (r.lang.to_string(), r.id)).collect();
    want.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    assert_eq!(text_and_id_rows(&result), want);
}

/// NULL キーは 1 グループにまとまる（PostgreSQL 準拠）。ASC は末尾・DESC／NULLS FIRST は先頭。
#[test]
fn distinct_on_groups_nulls_together() {
    let path = unique_db_path("sql25-distinct-on-null");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&TableSchema::new(
            "nl",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(DIM as u32), false),
                ColumnDef::new("tag", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    let ctx = ctx_for("tenant-a", false);
    for (id, tag) in [
        (1u64, Some("x")),
        (2, None),
        (3, None),
        (4, Some("y")),
        (5, None),
    ] {
        engine::tenant::insert_typed_row(
            &storage,
            "nl",
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![0.0f32; DIM]),
                match tag {
                    Some(t) => Value::Text(t.to_string()),
                    None => Value::Null,
                },
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!("op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }
    let core = new_core(storage);
    let run = |sql: &str| -> Vec<(Option<String>, u64)> {
        core.execute_sql(&ctx, sql)
            .expect(sql)
            .rows
            .iter()
            .map(|r| (as_text(&r.cells[0]), as_integer(&r.cells[1])))
            .collect()
    };
    let x = (Some("x".to_string()), 1);
    let y = (Some("y".to_string()), 4);
    assert_eq!(
        run("SELECT DISTINCT ON (tag) tag, id FROM nl ORDER BY tag, id"),
        vec![x.clone(), y.clone(), (None, 2)]
    );
    assert_eq!(
        run("SELECT DISTINCT ON (tag) tag, id FROM nl ORDER BY tag DESC, id"),
        vec![(None, 2), y.clone(), x.clone()]
    );
    assert_eq!(
        run("SELECT DISTINCT ON (tag) tag, id FROM nl ORDER BY tag NULLS FIRST, id DESC"),
        vec![(None, 5), x, y]
    );
}

/// 結果行数上限（`cap`）付きの core を、同一内容のコーパスで新規に作る（`with_result_limits` は
/// core を消費するため、上限を変えるたびに作り直す）。
fn corpus_core_with_cap(
    label: &str,
    cap: impl FnOnce(&[RowTruth]) -> usize,
) -> (EngineCore, Vec<RowTruth>, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_multi_tenant_corpus(&storage);
    let cap = cap(&truths);
    let core = new_core(storage).with_result_limits(engine::result_limits::ResultLimits::new(cap));
    (core, truths, guard)
}

/// LIMIT 省略時の上限（`54000`）は入力行数ではなくグループ数に掛かる。結果はテナントごとに可視行だけで決まる。
#[test]
fn distinct_on_implicit_row_cap_applies_to_groups_not_input_rows() {
    for tenant in TENANTS {
        for allow_private in [false, true] {
            let (core, truths, _guard) = corpus_core_with_cap("sql25-distinct-on-cap", |truths| {
                oracle_distinct_langs(&visible_rows(truths, tenant, allow_private)).len()
            });
            let ctx = ctx_for(tenant, allow_private);
            let visible = visible_rows(&truths, tenant, allow_private);
            let groups = oracle_distinct_langs(&visible).len();
            assert!(visible.len() > groups, "input rows exceed the group count");
            // 入力行数 > cap かつ グループ数 <= cap は成功する。
            let result = core
                .execute_sql(
                    &ctx,
                    "SELECT DISTINCT ON (lang) lang FROM docs ORDER BY lang",
                )
                .expect("groups within the cap");
            assert_eq!(result.rows.len(), groups);
        }
    }
}

#[test]
fn distinct_on_over_group_cap_is_54000_for_every_tenant() {
    for tenant in TENANTS {
        let (core, truths, _guard) = corpus_core_with_cap("sql25-distinct-on-cap-over", |truths| {
            oracle_distinct_langs(&visible_rows(truths, tenant, true)).len() - 1
        });
        let ctx = ctx_for(tenant, true);
        assert!(oracle_distinct_langs(&visible_rows(&truths, tenant, true)).len() >= 2);
        let err = core
            .execute_sql(
                &ctx,
                "SELECT DISTINCT ON (lang) lang FROM docs ORDER BY lang",
            )
            .expect_err("groups exceed the cap");
        assert_eq!(err.wire_code(), "54000");
        assert!(err.to_string().ends_with("result exceeds the row limit"));
    }
}

/// 実行段へ重複排除を運べない形は拒否（黙って重複排除を落とさない）。
// intentional: docs/design/select-distinct-on.md
#[test]
fn distinct_on_unsupported_forms_are_rejected_fail_closed() {
    for sql in [
        "SELECT DISTINCT ON (lang) lang FROM docs ORDER BY lang FETCH FIRST 2 ROWS WITH TIES",
        "SELECT DISTINCT ON (lang) lang FROM docs ORDER BY lang FOR UPDATE",
        "EXPLAIN SELECT DISTINCT ON (lang) lang FROM docs ORDER BY lang",
        "SELECT DISTINCT ON (lang) lang FROM docs WHERE id IN (SELECT DISTINCT ON (lang) id FROM docs ORDER BY lang)",
        "SELECT DISTINCT ON (lang) lang, COUNT(*) FROM docs GROUP BY lang",
        "SELECT DISTINCT ON (lang) lang, row_number() OVER () FROM docs ORDER BY lang",
        "CREATE VIEW dv AS SELECT DISTINCT ON (lang) lang FROM docs ORDER BY lang",
    ] {
        let (code, _msg) = distinct_on_outcome_is_tenant_independent(sql);
        assert!(
            matches!(code.as_str(), "0A000" | "42601"),
            "{sql} must be rejected fail-closed, got {code}"
        );
    }
}

/// 整合した文の未知列は束縛段の `42703`、不一致の文でも未知列が先に判定される（PostgreSQL と同順）。
#[test]
fn distinct_on_unknown_column_is_42703() {
    for sql in [
        "SELECT DISTINCT ON (nope) lang FROM docs ORDER BY nope",
        "SELECT DISTINCT ON (nope) lang FROM docs ORDER BY id",
        "SELECT DISTINCT ON (lang) lang FROM docs ORDER BY nope, lang",
    ] {
        let (code, _msg) = distinct_on_outcome_is_tenant_independent(sql);
        assert_eq!(code, "42703", "{sql}");
    }
}
#[test]
fn distinct_on_syntax_errors_and_limits_are_classified() {
    let many = ["lang"; 9].join(", ");
    for (sql, expected) in [
        ("SELECT DISTINCT ON () lang FROM docs", "42601"),
        ("SELECT DISTINCT ON (lang lang FROM docs", "42601"),
        ("SELECT DISTINCT ON (lang,) lang FROM docs", "42601"),
        ("SELECT DISTINCT ON (lang) FROM docs", "42601"),
        (
            &format!("SELECT DISTINCT ON ({many}) lang FROM docs ORDER BY lang"),
            "54000",
        ),
        ("SELECT DISTINCT ON (lang) lang FROM nope", "42P01"),
    ] {
        let (code, _msg) = distinct_on_outcome_is_tenant_independent(sql);
        assert_eq!(code, expected, "{sql}");
    }
}
