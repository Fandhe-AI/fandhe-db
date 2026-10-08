//! `engine::rls::ImplicitRlsHook` の結合テスト（TASK-137・対象ビヘイビア: RLS-6, RLS-7。
//! ポインタ: `docs/spec/05-tasks.md` TASK-137・`docs/spec/04-behavior/rls.md` RLS-6, RLS-7）。
//!
//! `tests/rls_security.rs`・`tests/sql_surface.rs` と同じ流儀（`unique_db_path` +
//! `CleanupGuard`、決定的擬似乱数 xorshift64*、production の判定関数
//! （[`engine::policy::PolicyContext::is_visible`]）を一切呼ばない独立オラクル、
//! `Storage::open` → `EngineCore::from_storage`）で実 `Storage` 上に複数テナントの
//! コーパスを構築し、`EngineCore::execute_sql`（SQL 経由）と `VectorCore::search`
//! （trait 経由）の両経路で RLS-6・RLS-7 を機械検証する。

use std::collections::HashSet;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{EngineCore, VectorCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::QueryResult;
use engine::storage::{Storage, Visibility};

// ---------- 決定的擬似乱数（xorshift64*。`tests/rls_security.rs` と同一実装） ----------

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

    fn next_f32_signed(&mut self) -> f32 {
        let unit = (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
        (unit * 2.0 - 1.0) as f32
    }
}

// ---------- テスト共通のセットアップ ----------
// 一時 DB パス払い出し（`unique_db_path` / `CleanupGuard`）は Issue #173 で
// `crates/engine/src/test_util/temp_db.rs` へ一本化した。

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: usize = 4;
const TABLE: &str = "docs";
const TENANTS: [&str; 3] = ["tenant-a", "tenant-b", "tenant-c"];
const LANGS: [&str; 2] = ["ja", "en"];
const ROWS_PER_TENANT: u64 = 12;

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM as u32), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

/// シード時の行の真実（オラクル用。production の可視性判定は一切通さない）。
#[derive(Clone, Copy)]
struct RowTruth {
    id: u64,
    tenant: &'static str,
    visibility: Visibility,
    lang: &'static str,
}

/// 独立オラクル: `PolicyContext::is_visible` を呼ばずに、テスト側で記録した
/// `RowTruth` から許可可否を判定する（production の判定関数自体のバグを
/// 見逃さないため。`tests/rls_security.rs` と同じ方針）。
fn is_allowed(row: &RowTruth, viewer_tenant: &str, allow_private: bool) -> bool {
    match row.visibility {
        Visibility::Public => true,
        Visibility::Private => row.tenant == viewer_tenant && allow_private,
    }
}

/// `body` に「その行だけが持つ一意なキーワード」を埋め込む（他行への混入検出に使う。
/// 行 id を含むトークンなので他の行の body とは一致しない）。
fn unique_keyword(id: u64) -> String {
    format!("uniquetoken{id}")
}

/// 3 テナント × Public/Private × lang{ja,en} の決定的コーパスを構築する。
fn seed_multi_tenant_corpus(storage: &Storage) -> Vec<RowTruth> {
    storage.create_table(&schema()).expect("create table");
    let mut rng = Xorshift64::new(0x0C0F_FEE0_1137);
    let mut truths = Vec::new();
    let mut id = 1u64;
    for &tenant in TENANTS.iter() {
        for i in 0..ROWS_PER_TENANT {
            let visibility = if i % 2 == 0 {
                Visibility::Public
            } else {
                Visibility::Private
            };
            let lang = LANGS[(i as usize) % LANGS.len()];
            let mut emb = vec![0.0f32; DIM];
            for slot in emb.iter_mut() {
                *slot = rng.next_f32_signed();
            }
            let body = format!("{lang} document about vectors {}", unique_keyword(id));
            // テナント境界付き API 経由（`tenant_id` は `ctx` から導出される）。
            let ctx =
                PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                    .expect("valid tenant");
            // TASK-101（RECOVER-10）: 台帳は (tenant, table, operation_id) 単位で内容
            // ハッシュを持つため、同一テナント内で内容の異なる複数行へ同一
            // operation_id を使い回すと 2 件目以降が OperationIdContentMismatch で
            // 拒否される。行ごとに一意の operation_id を使う。
            let op_id =
                engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
                    .expect("valid operation_id");
            engine::tenant::insert_typed_row(
                storage,
                TABLE,
                &ctx,
                id,
                visibility,
                &[
                    Value::Vector(emb),
                    Value::Text(lang.to_string()),
                    Value::Text(body),
                ],
                &op_id,
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

fn result_ids(result: &QueryResult) -> Vec<u64> {
    result.rows.iter().map(|r| r.id).collect()
}

fn allowed_set(truths: &[RowTruth], viewer_tenant: &str, allow_private: bool) -> HashSet<u64> {
    truths
        .iter()
        .filter(|t| is_allowed(t, viewer_tenant, allow_private))
        .map(|t| t.id)
        .collect()
}

fn query_vec() -> String {
    let v = [1.0f32; DIM];
    format!(
        "[{}]",
        v.iter()
            .map(|x| x.to_string())
            .collect::<Vec<_>>()
            .join(",")
    )
}

// --- RLS-7（TASK-137） -----------------------------------------------------------

#[test]
fn rls7_topk_query_visibility_regression() {
    let path = unique_db_path("rls7-c1");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);
    let q = query_vec();

    for &tenant in TENANTS.iter() {
        for allow_private in [false, true] {
            let ctx = if allow_private {
                PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                    .expect("valid tenant")
            } else {
                PolicyContext::new(tenant).expect("valid tenant")
            };
            let allowed = allowed_set(&truths, tenant, allow_private);

            for k in [1usize, 5, 20] {
                let sql_no_pred =
                    format!("SELECT * FROM docs ORDER BY embedding <=> '{q}' LIMIT {k}");
                let sql_pred = format!(
                    "SELECT * FROM docs WHERE visible() ORDER BY embedding <=> '{q}' LIMIT {k}"
                );
                let r1 = core
                    .execute_sql(&ctx, &sql_no_pred)
                    .expect("query should succeed");
                let r2 = core
                    .execute_sql(&ctx, &sql_pred)
                    .expect("query should succeed");
                assert_eq!(
                    result_ids(&r1),
                    result_ids(&r2),
                    "tenant={tenant} allow_private={allow_private} k={k}"
                );
                for id in result_ids(&r1) {
                    assert!(
                        allowed.contains(&id),
                        "disallowed row leaked: tenant={tenant} allow_private={allow_private} k={k} id={id}"
                    );
                }
            }
        }
    }
}

#[test]
fn rls7_scalar_filtered_query_visibility_regression() {
    let path = unique_db_path("rls7-c2");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);
    let q = query_vec();

    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let allowed = allowed_set(&truths, "tenant-a", true);
    let allowed_ja: HashSet<u64> = truths
        .iter()
        .filter(|t| t.lang == "ja" && is_allowed(t, "tenant-a", true))
        .map(|t| t.id)
        .collect();

    let sql_no_pred =
        format!("SELECT * FROM docs WHERE lang = 'ja' ORDER BY embedding <=> '{q}' LIMIT 20");
    let sql_pred = format!(
        "SELECT * FROM docs WHERE lang = 'ja' AND visible() ORDER BY embedding <=> '{q}' LIMIT 20"
    );

    let r1 = core
        .execute_sql(&ctx, &sql_no_pred)
        .expect("query should succeed");
    let r2 = core
        .execute_sql(&ctx, &sql_pred)
        .expect("query should succeed");
    assert_eq!(result_ids(&r1), result_ids(&r2));

    let got: HashSet<u64> = result_ids(&r1).into_iter().collect();
    assert_eq!(got.len(), allowed_ja.len().min(20));
    for id in &got {
        assert!(allowed.contains(id), "disallowed row leaked: id={id}");
        assert!(allowed_ja.contains(id), "lang filter bypassed: id={id}");
    }
}

#[test]
fn rls7_hybrid_query_visibility_regression() {
    let path = unique_db_path("rls7-c4");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);
    let q = query_vec();

    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let allowed = allowed_set(&truths, "tenant-a", false);

    let invisible_row = truths
        .iter()
        .find(|t| t.tenant == "tenant-b" && matches!(t.visibility, Visibility::Private))
        .expect("fixture has a tenant-b private row");
    let invisible_keyword = unique_keyword(invisible_row.id);

    let sql_invisible = format!(
        "SELECT * FROM docs ORDER BY hybrid_rrf(embedding, '{q}', body, '{invisible_keyword}') LIMIT 20"
    );
    let result = core
        .execute_sql(&ctx, &sql_invisible)
        .expect("query should succeed");
    assert!(
        !result_ids(&result).contains(&invisible_row.id),
        "disallowed row leaked: id={}",
        invisible_row.id
    );
    for id in result_ids(&result) {
        assert!(allowed.contains(&id), "disallowed row leaked: id={id}");
    }

    let visible_row = truths
        .iter()
        .find(|t| is_allowed(t, "tenant-a", false))
        .expect("fixture has an allowed row");
    let visible_keyword = unique_keyword(visible_row.id);
    let sql_visible = format!(
        "SELECT * FROM docs ORDER BY hybrid_rrf(embedding, '{q}', body, '{visible_keyword}') LIMIT 20"
    );
    let result_visible = core
        .execute_sql(&ctx, &sql_visible)
        .expect("query should succeed");
    assert!(
        result_ids(&result_visible).contains(&visible_row.id),
        "visible row with its own unique keyword should be retrievable"
    );
}

#[test]
fn rls7_predicate_alteration_rejected() {
    let path = unique_db_path("rls7-alteration");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);
    let q = query_vec();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let altered_forms = [
        format!("SELECT * FROM docs WHERE visible('tenant-b') ORDER BY embedding <=> '{q}' LIMIT 10"),
        format!("SELECT * FROM docs WHERE NOT visible() ORDER BY embedding <=> '{q}' LIMIT 10"),
        format!(
            "SELECT * FROM docs WHERE visible() OR lang = 'ja' ORDER BY embedding <=> '{q}' LIMIT 10"
        ),
    ];
    for sql in altered_forms {
        let err = core
            .execute_sql(&ctx, &sql)
            .expect_err("should be rejected");
        assert!(!err.wire_code().is_empty(), "sql={sql:?}");
    }
}

// --- RLS-6（TASK-137） -----------------------------------------------------------

#[test]
fn rls6_tenant_scoping_regression() {
    let path = unique_db_path("rls6-derivation");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);
    let q = query_vec();

    let sql =
        format!("SELECT * FROM docs WHERE lang = 'tenant-b' ORDER BY embedding <=> '{q}' LIMIT 10");

    for &tenant in TENANTS.iter() {
        let ctx = PolicyContext::new(tenant).expect("valid tenant");
        let allowed = allowed_set(&truths, tenant, false);
        let result = core.execute_sql(&ctx, &sql).expect("query should succeed");
        assert!(result_ids(&result).is_empty());
        for id in result_ids(&result) {
            assert!(allowed.contains(&id));
        }
    }

    let sql_topk = format!("SELECT * FROM docs ORDER BY embedding <=> '{q}' LIMIT 10");
    for &tenant in TENANTS.iter() {
        let ctx = PolicyContext::new(tenant).expect("valid tenant");
        let allowed = allowed_set(&truths, tenant, false);
        let result = core
            .execute_sql(&ctx, &sql_topk)
            .expect("query should succeed");
        for id in result_ids(&result) {
            assert!(
                allowed.contains(&id),
                "tenant={tenant} id={id} not in oracle allowed set"
            );
        }
    }
}

#[test]
fn rls6_disallowed_sql_forms_rejected() {
    let path = unique_db_path("rls6-no-tenant-column");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);
    let q = query_vec();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let sql_unknown_column = format!(
        "SELECT * FROM docs WHERE tenant_id = 'tenant-b' ORDER BY embedding <=> '{q}' LIMIT 10"
    );
    let err = core
        .execute_sql(&ctx, &sql_unknown_column)
        .expect_err("should be rejected");
    assert_eq!(err.wire_code(), "22000");

    for sql in [
        "SET search_path = tenant_b",
        "SELECT set_config('fandhe_db.tenant', 'tenant-b', false)",
        format!("SELECT * FROM docs ORDER BY embedding <=> '{q}' LIMIT 10; SELECT 1").as_str(),
    ] {
        let err = core.execute_sql(&ctx, sql).expect_err("should be rejected");
        assert_eq!(err.wire_code(), "42601", "sql={sql:?}");
    }
}

// --- RLS-6/7（TASK-137）: `VectorCore::search`（trait 経由）でも同じ契約 ----------

#[test]
fn rls_trait_search_path_visibility_regression() {
    let path = unique_db_path("rls-hook-trait-path");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_multi_tenant_corpus(&storage);
    let core: Box<dyn VectorCore> = Box::new(new_core(storage));
    let query = vec![1.0f32; DIM];

    for &tenant in TENANTS.iter() {
        for allow_private in [false, true] {
            let ctx = if allow_private {
                PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                    .expect("valid tenant")
            } else {
                PolicyContext::new(tenant).expect("valid tenant")
            };
            let allowed = allowed_set(&truths, tenant, allow_private);
            let hits = core
                .search(&ctx, TABLE, &query, 20)
                .expect("search should succeed");
            for hit in hits {
                assert!(
                    allowed.contains(&hit.id),
                    "disallowed row leaked via VectorCore::search: tenant={tenant} allow_private={allow_private} id={}",
                    hit.id
                );
            }
        }
    }
}
