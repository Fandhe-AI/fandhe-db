//! `CREATE INDEX ... USING hnsw (col <opclass>)` の結合テスト（Issue #2239。ポインタ:
//! INDEX-7・SQL-33 (5)、`docs/design/index-ddl-declaration.md`）。
//!
//! 固定する内容:
//!
//! - 4 種の演算子クラス（`vector_{cosine,l2,ip,l1}_ops`）を宣言でき、再オープン後も保持される
//! - 演算子クラス省略の既存構文は `vector_cosine_ops` の別名（同じ `IndexKind`・同じ EXPLAIN）
//! - `WITH (m, ef_construction)` との併用が演算子クラスと構築パラメータの両方を保つ
//! - 未知の演算子クラスは `42704`、既存の未対応の形は従来どおり拒否される
//! - `Declared` では非 cosine の宣言が厳密全走査へ縮退し、`All` の暗黙の HNSW は cosine
//! - 縮退経路でもテナント境界（他テナント混入 0 件）を保つ

use engine::catalog::{ColumnDef, ColumnType, IndexKind, TableSchema, VectorOpClass};
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

/// 行を持つ 2 テナントの `docs` を作る。返すのは tenant-b のベクトル。
fn seeded_core(label: &str, scope: HnswScope) -> (EngineCore, CleanupGuard, Vec<Vec<f32>>) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create docs");
    let vectors_a = gen_vectors(3, HNSW_ROWS);
    let vectors_b = gen_vectors(5, HNSW_ROWS);
    seed(&storage, "tenant-a", 0, &vectors_a);
    seed(&storage, "tenant-b", TENANT_B_ID_BASE, &vectors_b);
    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    let core = EngineCore::from_storage_with_engine(storage, kind).with_hnsw_scope(scope);
    (core, guard, vectors_b)
}

/// DDL を実行し、成功は空文字・失敗は SQLSTATE を返す。
fn declared(core: &EngineCore, sql: &str) -> String {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("tenant-a"), &mut session, sql)
        .map(|_| String::new())
        .unwrap_or_else(|e| e.wire_code().to_string())
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

fn line<'a>(lines: &'a [String], prefix: &str) -> &'a str {
    lines
        .iter()
        .find(|l| l.starts_with(prefix))
        .unwrap_or_else(|| panic!("{prefix} line missing: {lines:?}"))
}

fn knn_sql(op: &str, q: &[f32]) -> String {
    format!(
        "SELECT id FROM docs ORDER BY embedding {op} '{}' LIMIT 5",
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

/// 4 種の演算子クラスと別名を SQL で宣言でき、再オープン後も保持される（`WITH` 併用を含む）。
#[test]
fn four_opclasses_are_declared_and_persisted_across_reopen() {
    let (core, _g, path) = empty_core("sql33-opclass-ddl-persist");
    for (name, body) in [
        ("i_alias", "(embedding)"),
        ("i_cos", "(embedding vector_cosine_ops)"),
        ("i_l2", "(embedding vector_l2_ops)"),
        ("i_ip", "(embedding VECTOR_IP_OPS)"),
        ("i_l1", "(embedding vector_l1_ops)"),
        (
            "i_l2_with",
            "(embedding vector_l2_ops) WITH (m = 8, ef_construction = 40)",
        ),
    ] {
        assert_eq!(
            declared(
                &core,
                &format!("CREATE INDEX {name} ON docs USING hnsw {body}")
            ),
            "",
            "{name}"
        );
    }
    drop(core);

    let storage = Storage::open(&path).expect("reopen");
    let defs = storage.list_indexes().expect("list");
    let find = |name: &str| defs.iter().find(|d| d.name == name).expect("index present");
    for (name, expected) in [
        ("i_alias", VectorOpClass::Cosine),
        ("i_cos", VectorOpClass::Cosine),
        ("i_l2", VectorOpClass::L2),
        ("i_ip", VectorOpClass::Ip),
        ("i_l1", VectorOpClass::L1),
        ("i_l2_with", VectorOpClass::L2),
    ] {
        assert_eq!(find(name).kind, IndexKind::Hnsw(expected), "{name}");
    }
    assert_eq!(find("i_alias").hnsw_options, None);
    assert_eq!(find("i_alias").kind, find("i_cos").kind);
    assert_eq!(
        find("i_l2_with")
            .hnsw_options
            .map(|o| (o.m(), o.ef_construction())),
        Some((8, 40))
    );
}

/// 既存構文の DDL（`IF NOT EXISTS`・`WITH`・`DROP INDEX`）は従来どおり通り、未知の演算子クラスと
/// 未対応の形は拒否される。拒否された宣言は何も残さない。
#[test]
fn legacy_syntax_still_works_and_bad_forms_are_rejected() {
    let (core, _g, _path) = empty_core("sql33-opclass-ddl-legacy");
    for sql in [
        "CREATE INDEX a ON docs USING hnsw (embedding)",
        "CREATE INDEX IF NOT EXISTS a ON docs USING hnsw (embedding vector_l2_ops)",
        "CREATE INDEX b ON docs USING hnsw (embedding) WITH (m = 8, ef_construction = 40)",
        "DROP INDEX a",
        "DROP INDEX b",
    ] {
        assert_eq!(declared(&core, sql), "", "{sql}");
    }
    for (sql, code) in [
        (
            "CREATE INDEX x ON docs USING hnsw (embedding halfvec_l2_ops)",
            "42704",
        ),
        (
            "CREATE INDEX x ON docs USING hnsw (embedding jsonb_ops)",
            "42704",
        ),
        (
            "CREATE INDEX x ON docs USING hnsw (embedding vector_l2_ops, lang)",
            "0A000",
        ),
    ] {
        assert_eq!(declared(&core, sql), code, "{sql}");
    }
    // 非 VECTOR 列への宣言は演算子クラスの有無にかかわらず同じ結果（従来の列型検査）。
    let plain = declared(&core, "CREATE INDEX x ON docs USING hnsw (lang)");
    assert_ne!(plain, "");
    assert_eq!(
        declared(
            &core,
            "CREATE INDEX x ON docs USING hnsw (lang vector_l2_ops)"
        ),
        plain
    );
    assert_eq!(
        declared(&core, "CREATE INDEX x ON docs USING hnsw (embedding)"),
        ""
    );
}

/// 別名と正規構文は `EXPLAIN` の出力も同じ。`Declared` で L2 を SQL 宣言した表は `<=>` が
/// 厳密全走査へ縮退し、結果は厳密結果と一致してテナント境界を越えない。
#[test]
fn alias_and_canonical_explain_match_and_l2_declaration_degrades_safely() {
    let (alias, _g1, vb) = seeded_core("sql33-opclass-ddl-alias", HnswScope::Declared);
    let (canon, _g2, _) = seeded_core("sql33-opclass-ddl-canon", HnswScope::Declared);
    let (l2, _g3, _) = seeded_core("sql33-opclass-ddl-l2", HnswScope::Declared);
    let (exact, _g4, _) = seeded_core("sql33-opclass-ddl-exact", HnswScope::Declared);
    assert_eq!(
        declared(&alias, "CREATE INDEX idx ON docs USING hnsw (embedding)"),
        ""
    );
    assert_eq!(
        declared(
            &canon,
            "CREATE INDEX idx ON docs USING hnsw (embedding vector_cosine_ops)"
        ),
        ""
    );
    assert_eq!(
        declared(
            &l2,
            "CREATE INDEX idx ON docs USING hnsw (embedding vector_l2_ops)"
        ),
        ""
    );
    let sql = knn_sql("<=>", &vb[0]);
    let a = explain_lines(&alias, "tenant-b", &sql);
    assert_eq!(a, explain_lines(&canon, "tenant-b", &sql));
    assert!(
        line(&a, "ann_plan: ").starts_with("ann_plan: hnsw"),
        "{a:?}"
    );
    assert!(
        line(&a, "hnsw_params: ").ends_with("metric=cosine"),
        "{a:?}"
    );

    let lines = explain_lines(&l2, "tenant-b", &sql);
    assert_eq!(
        line(&lines, "ann_plan: "),
        "ann_plan: plain_scan_opclass_mismatch"
    );
    assert!(
        line(&lines, "hnsw_params: ").ends_with("metric=l2"),
        "{lines:?}"
    );
    let degraded = l2.execute_sql(&ctx("tenant-b"), &sql).expect("query");
    let baseline = exact.execute_sql(&ctx("tenant-b"), &sql).expect("query");
    assert_eq!(degraded, baseline, "fallback must equal exact scan");
    let hits = ids(&degraded);
    assert!(!hits.is_empty());
    assert!(hits.iter().all(|id| *id >= TENANT_B_ID_BASE), "{hits:?}");
}

/// `All` では宣言の無い表の暗黙の HNSW は cosine: `<=>` は HNSW 経路、`<->` は厳密全走査。
#[test]
fn scope_all_implicit_hnsw_is_cosine() {
    let (core, _g, vb) = seeded_core("sql33-opclass-ddl-all", HnswScope::All);
    let cos = explain_lines(&core, "tenant-b", &knn_sql("<=>", &vb[0]));
    assert!(
        line(&cos, "ann_plan: ").starts_with("ann_plan: hnsw"),
        "{cos:?}"
    );
    assert!(
        line(&cos, "hnsw_params: ").ends_with("metric=cosine"),
        "{cos:?}"
    );
    let l2 = explain_lines(&core, "tenant-b", &knn_sql("<->", &vb[0]));
    assert_eq!(
        line(&l2, "ann_plan: "),
        "ann_plan: plain_scan_opclass_mismatch"
    );
    let result = core
        .execute_sql(&ctx("tenant-b"), &knn_sql("<=>", &vb[0]))
        .expect("query");
    assert!(ids(&result).iter().all(|id| *id >= TENANT_B_ID_BASE));
}
