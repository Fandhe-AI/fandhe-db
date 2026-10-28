//! `engine::core::EngineCore::dictionary_snapshot` の結合テスト（TASK-109、対象
//! ビヘイビア: PLAN-5。ポインタ: `docs/spec/05-tasks.md` TASK-109・
//! `docs/spec/04-behavior/query-planning.md` PLAN-5）。
//!
//! `tests/incremental_index.rs`・`tests/batch_limits.rs` と同じ流儀（`unique_db_path` /
//! `CleanupGuard`、実 `Storage` 上にテーブルを構築、`HashingEmbedder` による決定的
//! だが意味的ではない埋め込み）で、ファイル形 `INSERT`（単発・バッチ）が辞書的情報源
//! （シンボル辞書・ファイルツリー・用語索引）へ反映されること、同一パス再送
//! （増分インデックス連動＝世代失効）・テナント境界・再起動時の再構築・キャッシュヒット
//! を検証する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::dictionary::DictionaryConfig;
use engine::embedding::HashingEmbedder;
use engine::incremental::IncrementalConfig;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::storage::Storage;

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: u32 = 32;

fn new_core_with_documents_table(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")))
        .with_incremental_config(small_chunk_config())
}

/// テーブルを新規作成せず既存の `Storage` 上で `EngineCore` を再構築する
/// （再起動シナリオの検証用。埋め込み・チャンク設定は新規テーブル作成時と同一）。
fn open_core_on_existing_table(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("reopen storage");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")))
        .with_incremental_config(small_chunk_config())
}

fn small_chunk_config() -> IncrementalConfig {
    IncrementalConfig {
        chunking: engine::chunking::ChunkingConfig {
            lines_per_chunk: 4,
            max_markdown_section_chars: None,
        },
        ..IncrementalConfig::default()
    }
}

fn sql_escape(s: &str) -> String {
    s.replace('\'', "''")
}

fn insert_file_sql(table: &str, path: &str, body: &str, op_id: &str) -> String {
    format!(
        "INSERT INTO {table} (path, body) VALUES ('{}', '{}') USING OPERATION_ID '{op_id}'",
        sql_escape(path),
        sql_escape(body)
    )
}

/// ファイル形 `INSERT` はチャンク行を `Visibility::Private` で書き込む
/// （`incremental.rs::index_file` 参照）。読み取り側の `PolicyContext` は
/// `Private` を明示許可しないと自テナントの行すら見えないため、`tests/
/// incremental_index.rs` の `read_ctx` と同じく `Public`/`Private` 両方を
/// 許可したコンテキストを使う。
fn tenant_ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(
        tenant,
        [
            engine::storage::Visibility::Public,
            engine::storage::Visibility::Private,
        ],
    )
    .expect("valid tenant")
}

// --- 基本反映: ファイル形 INSERT 後にシンボル・ツリー・用語が辞書へ現れること -------

#[test]
fn dictionary_snapshot_reflects_file_insert() {
    let path = unique_db_path("dict-basic");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = tenant_ctx("tenant-a");

    let body = "//! module doc about caching\npub fn run_batch() {}\nstruct Wrapper {}\n";
    core.execute_insert_sql(
        &ctx,
        &insert_file_sql("documents", "src/x.rs", body, "op-1"),
    )
    .expect("file insert should succeed");

    let dict = core
        .dictionary_snapshot(&ctx, "documents")
        .expect("dictionary snapshot should succeed");

    assert!(
        dict.symbols.iter().any(|s| s.name == "run_batch"),
        "expected run_batch symbol, got {:?}",
        dict.symbols
    );
    assert!(
        dict.symbols.iter().any(|s| s.name == "Wrapper"),
        "expected Wrapper symbol, got {:?}",
        dict.symbols
    );
    assert!(dict.file_tree.paths.contains("src/x.rs"));
    assert!(dict.term_index.contains_key("caching"));
}

// --- 増分インデックス連動: 同一パス再送（置換）で世代が bump し辞書が再構築される ----

#[test]
fn dictionary_snapshot_invalidates_on_same_path_replace() {
    let path = unique_db_path("dict-replace");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = tenant_ctx("tenant-a");

    core.execute_insert_sql(
        &ctx,
        &insert_file_sql("documents", "src/x.rs", "fn old_symbol() {}\n", "op-1"),
    )
    .expect("first insert should succeed");
    let first = core
        .dictionary_snapshot(&ctx, "documents")
        .expect("first snapshot should succeed");
    assert!(first.symbols.iter().any(|s| s.name == "old_symbol"));

    core.execute_insert_sql(
        &ctx,
        &insert_file_sql("documents", "src/x.rs", "fn new_symbol() {}\n", "op-2"),
    )
    .expect("replace insert should succeed");
    let second = core
        .dictionary_snapshot(&ctx, "documents")
        .expect("second snapshot should succeed");

    assert!(
        !second.symbols.iter().any(|s| s.name == "old_symbol"),
        "stale symbol from replaced body must not remain: {:?}",
        second.symbols
    );
    assert!(second.symbols.iter().any(|s| s.name == "new_symbol"));
}

// --- バッチ投入経路でも反映されること ------------------------------------------------

#[test]
fn dictionary_snapshot_reflects_batch_insert() {
    let path = unique_db_path("dict-batch");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = tenant_ctx("tenant-a");

    let sql_a = insert_file_sql("documents", "src/a.rs", "fn from_a() {}\n", "op-batch-a");
    let sql_b = insert_file_sql("documents", "src/b.rs", "fn from_b() {}\n", "op-batch-b");
    core.execute_insert_sql_batch(&ctx, &[sql_a.as_str(), sql_b.as_str()])
        .expect("batch insert should succeed");

    let dict = core
        .dictionary_snapshot(&ctx, "documents")
        .expect("dictionary snapshot should succeed");
    assert!(dict.symbols.iter().any(|s| s.name == "from_a"));
    assert!(dict.symbols.iter().any(|s| s.name == "from_b"));
    assert!(dict.file_tree.paths.contains("src/a.rs"));
    assert!(dict.file_tree.paths.contains("src/b.rs"));
}

// --- テナント境界: tenant A の索引内容は tenant B の辞書に一切含まれないこと --------

#[test]
fn dictionary_snapshot_does_not_leak_across_tenants() {
    let path = unique_db_path("dict-tenant-isolation");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx_a = tenant_ctx("tenant-a");
    let ctx_b = tenant_ctx("tenant-b");

    core.execute_insert_sql(
        &ctx_a,
        &insert_file_sql(
            "documents",
            "src/secret.rs",
            "fn tenant_a_only_symbol() {}\n",
            "op-a",
        ),
    )
    .expect("tenant a insert should succeed");
    core.execute_insert_sql(
        &ctx_b,
        &insert_file_sql(
            "documents",
            "src/public.rs",
            "fn tenant_b_symbol() {}\n",
            "op-b",
        ),
    )
    .expect("tenant b insert should succeed");

    let dict_b = core
        .dictionary_snapshot(&ctx_b, "documents")
        .expect("tenant b snapshot should succeed");

    assert!(
        !dict_b
            .symbols
            .iter()
            .any(|s| s.name == "tenant_a_only_symbol"),
        "tenant b dictionary must not contain tenant a's private symbols: {:?}",
        dict_b.symbols
    );
    assert!(!dict_b.file_tree.paths.contains("src/secret.rs"));
    assert!(dict_b.symbols.iter().any(|s| s.name == "tenant_b_symbol"));
}

// --- 再起動: EngineCore 再 open 後も redb から再構築され同一内容になること ----------

#[test]
fn dictionary_snapshot_rebuilds_after_reopen() {
    let path = unique_db_path("dict-reopen");
    let _guard = CleanupGuard(path.clone());
    let ctx = tenant_ctx("tenant-a");

    {
        let core = new_core_with_documents_table(&path);
        core.execute_insert_sql(
            &ctx,
            &insert_file_sql(
                "documents",
                "src/persisted.rs",
                "fn persisted_symbol() {}\n",
                "op-1",
            ),
        )
        .expect("insert should succeed");
    }

    let core = open_core_on_existing_table(&path);
    let dict = core
        .dictionary_snapshot(&ctx, "documents")
        .expect("snapshot after reopen should succeed");
    assert!(dict.symbols.iter().any(|s| s.name == "persisted_symbol"));
}

// --- キャッシュヒット: 世代不変なら再走査せず同一 Arc を返すこと --------------------

#[test]
fn dictionary_snapshot_cache_hit_returns_same_arc_without_rescanning() {
    let path = unique_db_path("dict-cache-hit");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = tenant_ctx("tenant-a");

    core.execute_insert_sql(
        &ctx,
        &insert_file_sql("documents", "src/x.rs", "fn cached_symbol() {}\n", "op-1"),
    )
    .expect("insert should succeed");

    let first = core
        .dictionary_snapshot(&ctx, "documents")
        .expect("first snapshot should succeed");
    let second = core
        .dictionary_snapshot(&ctx, "documents")
        .expect("second snapshot should succeed");

    assert!(
        std::sync::Arc::ptr_eq(&first, &second),
        "world generation unchanged between calls: cache hit must return the same Arc"
    );
}

// --- config フラグによる情報源の無効化（シンボル辞書には無効化スイッチが無い） ----

#[test]
fn dictionary_config_disables_only_auxiliary_sources() {
    let path = unique_db_path("dict-config-flags");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")))
        .with_incremental_config(small_chunk_config())
        .with_dictionary_config(DictionaryConfig {
            enable_file_tree: false,
            enable_term_index: false,
            top_terms: engine::dictionary::DEFAULT_TOP_TERMS,
        });
    let ctx = tenant_ctx("tenant-a");

    core.execute_insert_sql(
        &ctx,
        &insert_file_sql(
            "documents",
            "src/x.rs",
            "//! doc term coverage\nfn only_symbol_matters() {}\n",
            "op-1",
        ),
    )
    .expect("insert should succeed");

    let dict = core
        .dictionary_snapshot(&ctx, "documents")
        .expect("snapshot should succeed");
    // PLAN-5: シンボル辞書は無効化スイッチを持たず常に構築される。
    assert!(dict.symbols.iter().any(|s| s.name == "only_symbol_matters"));
    assert!(dict.file_tree.paths.is_empty());
    assert!(dict.term_index.is_empty());
}

// --- path/body 列を持たないテーブルは拒否されること --------------------------------

#[test]
fn dictionary_snapshot_rejects_table_without_path_or_body_columns() {
    let path = unique_db_path("dict-missing-columns");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "vectors_only",
            vec![ColumnDef::new("embedding", ColumnType::Vector(DIM), false)],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = tenant_ctx("tenant-a");

    let err = core
        .dictionary_snapshot(&ctx, "vectors_only")
        .expect_err("table without path/body columns must be rejected");
    // path 列の欠落が先に判定されるため、固定の英語メッセージは path 側のみを指す
    // （body 側の分岐は `dictionary_snapshot_rejects_table_without_body_column` で検証）。
    assert!(format!("{err}").contains("path column"));
}

#[test]
fn dictionary_snapshot_rejects_table_without_body_column() {
    let path = unique_db_path("dict-missing-body-column");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "path_only",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
                ColumnDef::new("path", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = tenant_ctx("tenant-a");

    let err = core
        .dictionary_snapshot(&ctx, "path_only")
        .expect_err("table without body column must be rejected");
    // body 列欠落側の分岐（`core.rs` の 2 つ目の `ok_or_else`）を単独で検証する。
    assert!(format!("{err}").contains("body column"));
}

/// `path`/`body` という列名が存在しても `ColumnType::Text` でなければ拒否する
/// （PR #249 codex-review P1 指摘の回帰テスト）。列名一致だけで受理すると、
/// 後段の `Value::Text` match が全行を黙ってスキップし、成功応答の空/不完全な
/// 辞書を返してしまう。ここでは `path` 列を `Vector` 型にして型不一致を再現する。
#[test]
fn dictionary_snapshot_rejects_non_text_path_column() {
    let path = unique_db_path("dict-non-text-path-column");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "wrong_path_type",
            vec![
                ColumnDef::new("path", ColumnType::Vector(DIM), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = tenant_ctx("tenant-a");

    let err = core
        .dictionary_snapshot(&ctx, "wrong_path_type")
        .expect_err("table with non-text path column must be rejected");
    assert!(format!("{err}").contains("path column"));
}

/// `body` 列が非 Text 型の場合も同様に拒否する（上記テストの body 側）。
#[test]
fn dictionary_snapshot_rejects_non_text_body_column() {
    let path = unique_db_path("dict-non-text-body-column");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "wrong_body_type",
            vec![
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Vector(DIM), false),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = tenant_ctx("tenant-a");

    let err = core
        .dictionary_snapshot(&ctx, "wrong_body_type")
        .expect_err("table with non-text body column must be rejected");
    assert!(format!("{err}").contains("body column"));
}

/// `path` 列が nullable な Text 型の場合も拒否する（PR #249 codex-review P1
/// 指摘の回帰テスト）。nullable を許すと、NULL の `path` を持つ行が
/// `Value::Null` に一致して黙ってスキップされ、成功応答の空/不完全な辞書を
/// 返してしまう。スキーマ検証の時点で non-nullable を必須化する。
#[test]
fn dictionary_snapshot_rejects_nullable_path_column() {
    let path = unique_db_path("dict-nullable-path-column");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "nullable_path",
            vec![
                ColumnDef::new("path", ColumnType::Text, true),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = tenant_ctx("tenant-a");

    let err = core
        .dictionary_snapshot(&ctx, "nullable_path")
        .expect_err("table with nullable path column must be rejected");
    assert!(format!("{err}").contains("path column"));
}

/// `body` 列が nullable な Text 型の場合も同様に拒否する（上記テストの body 側）。
#[test]
fn dictionary_snapshot_rejects_nullable_body_column() {
    let path = unique_db_path("dict-nullable-body-column");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "nullable_body",
            vec![
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = tenant_ctx("tenant-a");

    let err = core
        .dictionary_snapshot(&ctx, "nullable_body")
        .expect_err("table with nullable body column must be rejected");
    assert!(format!("{err}").contains("body column"));
}
