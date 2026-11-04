//! crossdb ハーネスの self 側 SQL 形（`scripts/crossdb_bench/self_db.py`）が `<#>` で受理される
//! ことの回帰テスト（Issue #2952。ビヘイビア: SQL-33。ポインタ: `docs/spec/04-behavior/`）。
//!
//! crossdb の実機実行（Docker・psycopg）は CI で走らず、`scripts/tests` の parity テストは
//! 静的検査なので、実行時の拒否（`42601`／`0A000`）はここで代わりに担保する。固定する契約:
//! - self_db.py の kNN 系の形（WHERE・LIKE・LIMIT 200・`USING MODE 'recall'`・UDF 射影・EXPLAIN）が
//!   `<#>` で成功する。
//! - `<#>` + `USING MODE 'precision'` は `0A000`、`<=>` + precision は成功する（precision だけ
//!   `<=>` のまま残す crossdb 側の例外設計の根拠）。
//! - 結果順は内積の降順（`CpuScalarProvider::search` の Top-k と一致）。コサイン順とは食い違う。
//! - 他テナントの行は結果に混ざらない。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::{CpuScalarProvider, SearchInput, SearchProvider};
use engine::policy::PolicyContext;
use engine::sql::exec::QueryResult;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

/// (id, embedding, lang, body)。クエリ `[1,0]` では内積降順が 1, 4, 2, 3、コサイン昇順が 2, 4, 1, 3。
fn rows() -> Vec<(u64, [f32; 2], &'static str, &'static str)> {
    vec![
        (1, [10.0, 10.0], "ja", "xa"),
        (2, [1.0, 0.0], "en", "xb"),
        (3, [0.0, 5.0], "ja", "yc"),
        (4, [2.0, 0.0], "en", "yd"),
    ]
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn seeded(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(engine::recovery::required_op_id::LedgerMode::Ledgered);
    seed(&core, "tenant-a", &rows());
    // 他テナントの行（クエリに最も近い [100,0]）。tenant-a の結果へ混ざってはならない。
    seed(&core, "tenant-b", &[(900, [100.0, 0.0], "ja", "xz")]);
    (core, guard)
}

fn seed(core: &EngineCore, tenant: &str, rows: &[(u64, [f32; 2], &str, &str)]) {
    let ctx = ctx(tenant);
    let mut session = SessionState::default();
    for (id, v, lang, body) in rows {
        let sql = format!(
            "INSERT INTO docs (id, embedding, lang, body) \
             VALUES ({id}, '[{},{}]', '{lang}', '{body}') \
             USING OPERATION_ID 'seed-{tenant}-{id}'",
            v[0], v[1]
        );
        core.execute_sql_in_session(&ctx, &mut session, &sql)
            .unwrap_or_else(|e| panic!("seed {sql}: {e:?}"));
    }
}

fn run_in(core: &EngineCore, session: &mut SessionState, sql: &str) -> SqlOutcome {
    core.execute_sql_in_session(&ctx("tenant-a"), session, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
}

fn query(core: &EngineCore, sql: &str) -> QueryResult {
    match run_in(core, &mut SessionState::default(), sql) {
        SqlOutcome::Query(r) => r,
        other => panic!("expected Query for {sql}, got {other:?}"),
    }
}

fn ids(core: &EngineCore, sql: &str) -> Vec<u64> {
    query(core, sql).rows.iter().map(|r| r.id).collect()
}

#[test]
fn crossdb_self_knn_shapes_are_accepted_with_negative_inner_product() {
    let (core, _g) = seeded("sql33-crossdb-shapes");
    let inner_product_desc = vec![1, 4, 2, 3];
    assert_eq!(
        ids(
            &core,
            "SELECT id FROM docs ORDER BY embedding <#> '[1,0]' LIMIT 10"
        ),
        inner_product_desc
    );
    assert_eq!(
        ids(
            &core,
            "SELECT id FROM docs ORDER BY embedding <#> '[1,0]' LIMIT 10 USING MODE 'recall'"
        ),
        inner_product_desc
    );
    assert_eq!(
        ids(
            &core,
            "SELECT id FROM docs WHERE lang = 'ja' ORDER BY embedding <#> '[1,0]' LIMIT 10"
        ),
        vec![1, 3]
    );
    assert_eq!(
        ids(
            &core,
            "SELECT id FROM docs WHERE body LIKE 'x%' ORDER BY embedding <#> '[1,0]' LIMIT 10"
        ),
        vec![1, 2]
    );
    assert_eq!(
        ids(
            &core,
            "SELECT id, body FROM docs ORDER BY embedding <#> '[1,0]' LIMIT 200"
        ),
        inner_product_desc
    );
}

#[test]
fn crossdb_self_order_matches_provider_top_k_and_differs_from_cosine() {
    let (core, _g) = seeded("sql33-crossdb-provider");
    let fixture = rows();
    let row_ids: Vec<u64> = fixture.iter().map(|r| r.0).collect();
    let vectors: Vec<f32> = fixture.iter().flat_map(|r| r.1).collect();
    let expected: Vec<u64> = CpuScalarProvider
        .search(SearchInput {
            ids: &row_ids,
            vectors: &vectors,
            dim: 2,
            query: &[1.0, 0.0],
            k: 4,
        })
        .expect("provider search")
        .iter()
        .map(|h| h.id)
        .collect();
    let got = ids(
        &core,
        "SELECT id FROM docs ORDER BY embedding <#> '[1,0]' LIMIT 4",
    );
    assert_eq!(got, expected);
    let cosine = ids(
        &core,
        "SELECT id FROM docs ORDER BY embedding <=> '[1,0]' LIMIT 4",
    );
    assert_ne!(got, cosine);
}

#[test]
fn crossdb_self_udf_projection_and_explain_are_accepted() {
    let (core, _g) = seeded("sql33-crossdb-udf");
    let mut session = SessionState::default();
    session.allow_ddl();
    run_in(
        &core,
        &mut session,
        "CREATE FUNCTION norm_scale(v, s) AS s * vec_sum(vec_div(v, vec_norm(v)))",
    );
    match run_in(
        &core,
        &mut session,
        "SELECT id, norm_scale(embedding, 2.0) AS score FROM docs \
         ORDER BY embedding <#> '[1,0]' LIMIT 10",
    ) {
        SqlOutcome::Query(r) => assert_eq!(r.rows.len(), 4),
        other => panic!("expected Query, got {other:?}"),
    }
    // EXPLAIN は検索本体を実行せず成功する（42601／0A000 にならない）。
    run_in(
        &core,
        &mut SessionState::default(),
        "EXPLAIN SELECT id FROM docs ORDER BY embedding <#> '[1,0]' LIMIT 10",
    );
}

#[test]
fn crossdb_precision_keeps_cosine_because_negative_inner_product_is_rejected() {
    let (core, _g) = seeded("sql33-crossdb-precision");
    // 例外設計の根拠: `<=>` + precision は成功し、`<#>` + precision は 0A000。
    // precision は確信度しきい値で 0 件になりうるため、件数ではなく成功（Query）だけを固定する。
    query(
        &core,
        "SELECT id FROM docs ORDER BY embedding <=> '[1,0]' LIMIT 10 USING MODE 'precision'",
    );
    let err = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut SessionState::default(),
            "SELECT id FROM docs ORDER BY embedding <#> '[1,0]' LIMIT 10 USING MODE 'precision'",
        )
        .expect_err("<#> with precision must be rejected");
    assert_eq!(err.wire_code(), "0A000");
}

#[test]
fn crossdb_self_results_never_include_other_tenant_rows() {
    let (core, _g) = seeded("sql33-crossdb-tenant");
    let got = ids(
        &core,
        "SELECT id FROM docs ORDER BY embedding <#> '[1,0]' LIMIT 10",
    );
    assert!(!got.contains(&900), "other tenant row leaked: {got:?}");
}
