//! `CREATE INDEX ... USING hnsw (col) WITH (m = n, ef_construction = n)` の結合テスト
//! （Issue #2029。ポインタ: SQL-33 (5)・INDEX-7、`docs/design/index-ddl-declaration.md`）。
//!
//! 固定する内容:
//!
//! - 受理: 両キー・片方のみ（欠けたキーは pgvector の既定で補完）・大文字キー・境界値
//! - `22023`: 範囲外・補完後の `ef_construction < 2 * m`・未知/重複キー・小数/負値/識別子の値
//! - `0A000`: btree／gin への `WITH`、引用符付きの値
//! - `42601`: 構文の不整合（`WITH ()`・`=` 欠落・`$n`・`WHERE` の後の `WITH`）
//! - 永続化（再オープン後も保持）と `EXPLAIN` の `hnsw_params:` への反映
//! - テナント境界は宣言の有無で変わらない

use engine::catalog::{ColumnDef, ColumnType, IndexKind, TableSchema};
use engine::core::EngineCore;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::search_engine::{self, HnswScope};
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{RowInput, Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: u32 = 8;
// `sql::hnsw_cache::MIN_INDEXED_ROWS`（1,024）超。
const HNSW_ROWS: usize = 1_100;
const TENANT_B_ID_BASE: u64 = 1_000_000;

fn schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("doc", ColumnType::Jsonb, true),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn gen_vectors(seed: u64, rows: usize) -> Vec<Vec<f32>> {
    let mut state = seed;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let bits = (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as u32;
        (bits as f32) / (1u32 << 24) as f32 * 2.0 - 1.0
    };
    (0..rows)
        .map(|_| (0..DIM).map(|_| next()).collect())
        .collect()
}

fn seed(storage: &Storage, tenant: &str, id_base: u64, vectors: &[Vec<f32>]) {
    let schema = schema();
    let metadata: Vec<Vec<u8>> = (0..vectors.len())
        .map(|_| {
            engine::row_codec::encode_scalar_columns(
                &schema,
                &[Value::Null, Value::Text("ja".to_string()), Value::Null],
            )
            .expect("encode metadata")
        })
        .collect();
    let rows: Vec<(u64, RowInput<'_>)> = vectors
        .iter()
        .enumerate()
        .map(|(i, v)| {
            (
                id_base + i as u64,
                RowInput {
                    tenant_id: tenant,
                    visibility: Visibility::Private,
                    embedding: v.as_slice(),
                    metadata: metadata[i].as_slice(),
                },
            )
        })
        .collect();
    let op_id = OperationId::parse(&format!("seed-{tenant}")).expect("valid operation id");
    engine::tenant::insert_rows(storage, "docs", &ctx(tenant), &rows, &op_id).expect("seed rows");
}

fn vec_literal(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|x| x.to_string()).collect();
    format!("[{}]", parts.join(","))
}

/// 行を持たない空の `docs` を持つ HNSW opt-in（Declared）エンジン。構文・永続化の検査用。
fn empty_core(label: &str) -> (EngineCore, CleanupGuard, std::path::PathBuf) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create docs");
    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    let core =
        EngineCore::from_storage_with_engine(storage, kind).with_hnsw_scope(HnswScope::Declared);
    (core, guard, path)
}

fn run_ddl(
    core: &EngineCore,
    sql: &str,
) -> Result<SqlOutcome, engine::sql::allowlist::SqlSurfaceError> {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("tenant-a"), &mut session, sql)
}

fn create(core: &EngineCore, name: &str, with: &str) -> Result<(), String> {
    run_ddl(
        core,
        &format!("CREATE INDEX {name} ON docs USING hnsw (embedding) {with}"),
    )
    .map(|_| ())
    .map_err(|e| e.wire_code().to_string())
}

/// 受理される形はすべて成功し、拒否される形は期待どおりの SQLSTATE になる。
#[test]
fn with_clause_acceptance_and_sqlstate_truth_table() {
    let accepted = [
        "WITH (m = 8, ef_construction = 40)",
        "WITH (ef_construction = 40, m = 8)",
        "WITH (M = 8, EF_CONSTRUCTION = 40)",
        "WITH (m = 16)",
        "WITH (ef_construction = 100)",
        "WITH (m = 2, ef_construction = 4)",
        "WITH (m = 100, ef_construction = 1000)",
        "WITH (m = 100, ef_construction = 200)",
        "WITH (m = 8, ef_construction = 16)",
    ];
    for (i, with) in accepted.iter().enumerate() {
        let (core, _g, _p) = empty_core(&format!("sql33-with-accept-{i}"));
        assert_eq!(create(&core, "i1", with), Ok(()), "{with}");
    }
    let rejected: &[(&str, &str)] = &[
        // 範囲外・補完後の 2m 違反。
        ("WITH (m = 1)", "22023"),
        ("WITH (m = 101, ef_construction = 1000)", "22023"),
        ("WITH (ef_construction = 3)", "22023"),
        ("WITH (ef_construction = 1001)", "22023"),
        ("WITH (m = 8, ef_construction = 15)", "22023"),
        ("WITH (m = 40)", "22023"),
        ("WITH (m = 99999999999999999999999)", "22023"),
        // 未知・重複・値の形。
        ("WITH (fillfactor = 70)", "22023"),
        ("WITH (m = 8, m = 16)", "22023"),
        ("WITH (m = 1.5)", "22023"),
        ("WITH (m = -8)", "22023"),
        ("WITH (m = abc)", "22023"),
        // 未対応の形。
        ("WITH (m = '16')", "0A000"),
        // 構文の不整合。
        ("WITH ()", "42601"),
        ("WITH (m 16)", "42601"),
        ("WITH (m = )", "42601"),
        ("WITH (m = 16", "42601"),
        ("WITH m = 16", "42601"),
        ("WITH (m = 16,)", "42601"),
        ("WITH (m = $1)", "42601"),
        ("WHERE lang = 'ja' WITH (m = 16)", "42601"),
    ];
    for (with, code) in rejected {
        let (core, _g, _p) = empty_core("sql33-with-reject");
        assert_eq!(create(&core, "i1", with), Err(code.to_string()), "{with}");
        // 拒否された宣言は何も残さない（同名で素の宣言が通る）。
        assert_eq!(create(&core, "i1", ""), Ok(()), "{with}");
    }
}

/// btree／gin への `WITH` は `0A000`（storage parameter は実装しない）。
#[test]
fn with_clause_on_non_hnsw_methods_is_not_supported() {
    let (core, _g, _p) = empty_core("sql33-with-non-hnsw");
    for sql in [
        "CREATE INDEX i1 ON docs (lang) WITH (fillfactor = 70)",
        "CREATE INDEX i2 ON docs USING btree (lang) WITH (m = 8)",
        "CREATE INDEX i3 ON docs USING gin (doc) WITH (fastupdate = off)",
    ] {
        let err = run_ddl(&core, sql).expect_err(sql);
        assert_eq!(err.wire_code(), "0A000", "{sql}");
    }
    // 拒否された宣言は何も残さない（同名で素の宣言が通る）。
    assert_eq!(create(&core, "i1", ""), Ok(()));
}

/// `WITH` 付き HNSW の部分索引・式索引・UNIQUE は従来どおり拒否される。
#[test]
fn with_clause_does_not_widen_unsupported_hnsw_shapes() {
    let (core, _g, _p) = empty_core("sql33-with-shapes");
    for sql in [
        "CREATE INDEX i1 ON docs USING hnsw (embedding) WITH (m = 8, ef_construction = 40) WHERE lang = 'ja'",
        "CREATE UNIQUE INDEX i2 ON docs USING hnsw (embedding) WITH (m = 8, ef_construction = 40)",
    ] {
        let err = run_ddl(&core, sql).expect_err(sql);
        assert_eq!(err.wire_code(), "0A000", "{sql}");
    }
    // 拒否された宣言は何も残さない（同名で素の宣言が通る）。
    assert_eq!(create(&core, "i1", ""), Ok(()));
}

/// 宣言した値（補完後）が永続化され、再オープン後も保持される。`WITH` 省略は `None` のまま。
#[test]
fn with_options_are_persisted_across_reopen() {
    let (core, _g, path) = empty_core("sql33-with-persist");
    create(&core, "i_full", "WITH (m = 8, ef_construction = 40)").expect("full");
    create(&core, "i_m_only", "WITH (m = 6)").expect("m only");
    create(&core, "i_plain", "").expect("plain");
    // `IF NOT EXISTS` は同名があれば副作用なしで成功する。
    run_ddl(
        &core,
        "CREATE INDEX IF NOT EXISTS i_full ON docs USING hnsw (embedding) WITH (m = 12)",
    )
    .expect("if not exists");
    drop(core);

    let storage = Storage::open(&path).expect("reopen");
    let defs = storage.list_indexes().expect("list");
    let options = |name: &str| {
        defs.iter()
            .find(|d| d.name == name)
            .unwrap_or_else(|| panic!("{name} missing"))
            .hnsw_options
            .map(|o| (o.m(), o.ef_construction()))
    };
    assert_eq!(options("i_full"), Some((8, 40)));
    // 欠けた `ef_construction` は pgvector の既定 64 で補う。
    assert_eq!(options("i_m_only"), Some((6, 64)));
    assert_eq!(options("i_plain"), None);
    assert!(defs.iter().all(|d| matches!(d.kind, IndexKind::Hnsw(_))));
}

fn explain_lines(core: &EngineCore, tenant: &str, sql: &str) -> Vec<String> {
    let mut session = SessionState::default();
    match core
        .execute_sql_in_session(&ctx(tenant), &mut session, &format!("EXPLAIN {sql}"))
        .expect("explain")
    {
        SqlOutcome::Explain(result) => result
            .rows
            .iter()
            .filter_map(|row| match row.cells.first() {
                Some(Cell::Text(s)) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        other => panic!("expected SqlOutcome::Explain, got {other:?}"),
    }
}

fn hnsw_params_line(lines: &[String]) -> &str {
    lines
        .iter()
        .find(|l| l.starts_with("hnsw_params: "))
        .unwrap_or_else(|| panic!("hnsw_params line missing: {lines:?}"))
}

fn knn_sql(q: &[f32]) -> String {
    format!(
        "SELECT id FROM docs ORDER BY embedding <=> '{}' LIMIT 5",
        vec_literal(q)
    )
}

fn ids(result: &QueryResult) -> Vec<u64> {
    result
        .rows
        .iter()
        .filter_map(|r| match r.cells.first() {
            Some(Cell::Integer(v)) => Some(*v),
            _ => None,
        })
        .collect()
}

/// 宣言すると `EXPLAIN` の `hnsw_params:` が宣言の値になり、DROP すると内部既定へ戻る。
/// 検索結果はテナント境界を越えない（宣言の有無で変わらない）。
#[test]
fn explain_reflects_declared_options_and_tenant_boundary_holds() {
    let path = unique_db_path("sql33-with-explain");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create docs");
    let vectors_a = gen_vectors(3, HNSW_ROWS);
    let vectors_b = gen_vectors(5, HNSW_ROWS);
    seed(&storage, "tenant-a", 0, &vectors_a);
    seed(&storage, "tenant-b", TENANT_B_ID_BASE, &vectors_b);
    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    let core =
        EngineCore::from_storage_with_engine(storage, kind).with_hnsw_scope(HnswScope::Declared);
    let sql = knn_sql(&vectors_b[0]);

    run_ddl(
        &core,
        "CREATE INDEX idx ON docs USING hnsw (embedding) WITH (m = 8, ef_construction = 40)",
    )
    .expect("declare");
    let lines = explain_lines(&core, "tenant-b", &sql);
    assert!(
        hnsw_params_line(&lines).starts_with("hnsw_params: m=8,ef_construction=40,ef_search=64,"),
        "{lines:?}"
    );
    let result = core.execute_sql(&ctx("tenant-b"), &sql).expect("query");
    let hits = ids(&result);
    assert!(!hits.is_empty());
    assert!(
        hits.iter().all(|id| *id >= TENANT_B_ID_BASE),
        "tenant-b must only see its own rows: {hits:?}"
    );

    // 宣言を差し替えると EXPLAIN も追随し、再構築後も結果はテナント境界内に留まる。
    run_ddl(&core, "DROP INDEX idx").expect("drop");
    run_ddl(
        &core,
        "CREATE INDEX idx ON docs USING hnsw (embedding) WITH (m = 12, ef_construction = 48)",
    )
    .expect("redeclare");
    let lines = explain_lines(&core, "tenant-b", &sql);
    assert!(
        hnsw_params_line(&lines).starts_with("hnsw_params: m=12,ef_construction=48,"),
        "{lines:?}"
    );
    let hits = ids(&core.execute_sql(&ctx("tenant-b"), &sql).expect("query"));
    assert!(!hits.is_empty() && hits.iter().all(|id| *id >= TENANT_B_ID_BASE));

    // `WITH` 省略の宣言は内部既定のまま。
    run_ddl(&core, "DROP INDEX idx").expect("drop");
    run_ddl(&core, "CREATE INDEX idx ON docs USING hnsw (embedding)").expect("plain");
    let lines = explain_lines(&core, "tenant-b", &sql);
    assert!(
        hnsw_params_line(&lines).starts_with("hnsw_params: m=16,ef_construction=100,"),
        "{lines:?}"
    );
}
