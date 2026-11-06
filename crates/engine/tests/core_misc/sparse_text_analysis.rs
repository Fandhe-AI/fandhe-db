//! 疎検索のテキスト処理設定（ストップワード・同義語。Issue #2130・SEARCH-16 のポインタ）の
//! 結合テスト。`EngineCore::with_text_analysis` を渡した core で SQL のハイブリッド検索に
//! 設定が効くこと、既定の core が変わらないこと、設定を差し替えたときに古い疎索引を
//! 使わないことを確かめる。プロセス全体の状態・環境変数は使わないので並列実行してよい。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sparse::TextAnalysis;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
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
        (1, [0.0, 1.0], "an automobile the the"),
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

fn analysis() -> TextAnalysis {
    TextAnalysis::from_lists(Some("the"), Some("car automobile")).expect("analysis")
}

#[test]
fn default_core_ignores_synonyms_and_stopwords() {
    let (storage, _g) = seeded_storage("text-analysis-default");
    let core = core(storage);
    assert!(core.text_analysis().is_empty());
    // 同義語なし: "car" は疎側で何にも当たらず、密側の順位（2, 3, 1）のまま。
    assert_eq!(ids(&core, "car"), vec![2, 3, 1]);
    // ストップワードなし: "the" は文書 1 に当たり、文書 1 が先頭に出る。
    assert_eq!(ids(&core, "the")[0], 1);
}

#[test]
fn configured_core_applies_synonyms_and_stopwords() {
    let (storage, _g) = seeded_storage("text-analysis-on");
    let core = core(storage).with_text_analysis(analysis());
    // car -> automobile が索引とクエリの両方で効き、文書 1 が先頭になる。
    assert_eq!(ids(&core, "car")[0], 1);
    // "the" だけのクエリはストップワードで空になり、密側の順位に戻る。
    assert_eq!(ids(&core, "the"), vec![2, 3, 1]);
}

#[test]
fn configuration_applies_inside_write_transaction_where_cache_is_bypassed() {
    let (storage, _g) = seeded_storage("text-analysis-txn");
    let core = core(storage).with_text_analysis(analysis());
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    let c = ctx();
    core.execute_sql_in_txn(&c, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(
        &c,
        &mut session,
        &mut txn,
        "INSERT INTO docs (id, embedding, body) VALUES (9, '[0.2,0.8]', 'filler text') \
         USING OPERATION_ID 'text-analysis-txn-ins'",
    )
    .expect("insert");
    let out = core
        .execute_sql_in_txn(&c, &mut session, &mut txn, &hybrid_sql("car"))
        .expect("select in txn");
    let SqlOutcome::Query(result) = out else {
        panic!("expected query outcome");
    };
    assert_eq!(result.rows.first().map(|r| r.id), Some(1));
    core.execute_sql_in_txn(&c, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(core.sparse_index_cache_stats().hits, 0);
}

#[test]
fn replacing_configuration_never_reuses_a_stale_index() {
    let (storage, _g) = seeded_storage("text-analysis-swap");
    let core = core(storage).with_text_analysis(analysis());
    assert_eq!(ids(&core, "car")[0], 1);
    assert_eq!(ids(&core, "car")[0], 1);
    assert_eq!(core.sparse_index_cache_stats().hits, 1);

    // 設定を恒等に差し替えると、キャッシュ済みの索引は使われず新しい設定の結果になる。
    let core = core.with_text_analysis(TextAnalysis::default());
    assert_eq!(ids(&core, "car"), vec![2, 3, 1]);
    assert_eq!(core.sparse_index_cache_stats().hits, 0);
}
