//! 疎検索の英語ステミング（Issue #3050・SEARCH-16 のポインタ。`english-stemmer` feature 有効時のみ）
//! の結合テスト。`EngineCore::with_text_analysis` にステミング有効の設定を渡した core で SQL の
//! ハイブリッド検索の語形違いが当たること、設定を差し替えたときに古い疎索引を使わず再構築される
//! こと（AC2）、既定の core が変わらないことを確かめる。プロセス全体の状態は使わないので並列可。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sparse::TextAnalysis;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// 密側の順位が 2, 3, 1 になるよう並べた 3 行（文書 1 は密側で最下位）。
fn seeded_storage(name: &str) -> (Storage, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("body", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    let rows: [(u64, [f32; 2], &str); 3] = [
        (1, [0.0, 1.0], "he runs every morning"),
        (2, [1.0, 0.0], "unrelated topic"),
        (3, [0.5, 0.5], "other thing"),
    ];
    for (id, emb, body) in rows {
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx(),
            id,
            Visibility::Private,
            &[Value::Vector(emb.to_vec()), Value::Text(body.to_string())],
            &engine::recovery::required_op_id::OperationId::parse(&format!("text-analysis-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }
    (storage, guard)
}

fn core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn hybrid_sql(query: &str) -> String {
    format!(
        "SELECT * FROM docs ORDER BY hybrid_rrf(embedding, '[1.0,0.0]', body, '{query}') LIMIT 3"
    )
}

fn ids(core: &EngineCore, query: &str) -> Vec<u64> {
    let r = core
        .execute_sql(&ctx(), &hybrid_sql(query))
        .expect("hybrid");
    r.rows.iter().map(|r| r.id).collect()
}

fn stemmed() -> TextAnalysis {
    TextAnalysis::default()
        .with_english_stemmer()
        .expect("english stemmer feature is on")
}

#[test]
fn default_core_does_not_match_inflections() {
    let (storage, _g) = seeded_storage("stem-default");
    let core = core(storage);
    assert_eq!(ids(&core, "running"), vec![2, 3, 1]);
}

#[test]
fn stemming_core_matches_inflections_and_swap_rebuilds() {
    let (storage, _g) = seeded_storage("stem-on");
    let core = core(storage).with_text_analysis(stemmed());
    assert_eq!(ids(&core, "running")[0], 1);
    assert_eq!(ids(&core, "running")[0], 1);
    assert_eq!(core.sparse_index_cache_stats().hits, 1);

    // 設定を恒等へ戻すと古い索引は使われず、語形違いは当たらなくなる。
    let core = core.with_text_analysis(TextAnalysis::default());
    assert_eq!(ids(&core, "running"), vec![2, 3, 1]);
    assert_eq!(core.sparse_index_cache_stats().hits, 0);
}
