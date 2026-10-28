//! `engine::core::EngineCore::execute_insert_sql_batch` の結合テスト（TASK-122、
//! 対象ビヘイビア: INDEX-4。ポインタ: `docs/spec/05-tasks.md` TASK-122・
//! `docs/spec/04-behavior/indexing.md` INDEX-4）。
//!
//! `tests/incremental_index.rs` と同じ流儀（`unique_db_path` / `CleanupGuard`、実
//! `Storage` 上にテーブルを構築）で、一括投入 4 上限（①バッチあたり最大ファイル数・
//! ②1 ファイルあたり最大本文サイズ・③バッチ合計最大サイズ・④バッチあたり最大生成
//! チャンク数）それぞれの超過が `54000`（`PAYLOAD_TOO_LARGE`）で副作用ゼロに拒否
//! されること、④の判定が埋め込み呼び出しより前に完了すること、行形混在・空バッチが
//! `22000` で拒否されること、上限内のバッチが全ファイル索引化されることを検証する。

use engine::batch_limits::BatchLimits;
use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::embedding::{EmbedError, Embedder, HashingEmbedder};
use engine::incremental::IncrementalConfig;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::ledger::LedgerLookup;
use engine::recovery::required_op_id::OperationId;
use engine::storage::{Storage, Visibility};

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: u32 = 128;

/// 生成チャンク数を小さく固定するための `IncrementalConfig`（1 チャンク = 1 行）。
fn small_chunk_config() -> IncrementalConfig {
    IncrementalConfig {
        chunking: engine::chunking::ChunkingConfig {
            lines_per_chunk: 1,
            max_markdown_section_chars: None,
        },
        ..IncrementalConfig::default()
    }
}

fn new_documents_storage(path: &std::path::Path) -> Storage {
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
    storage
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

fn vector_literal(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|x| x.to_string()).collect();
    format!("'[{}]'", parts.join(","))
}

/// 埋め込み呼び出し回数を数えるフェイク埋め込み実装（④の判定タイミング検証用。
/// `HashingEmbedder` へ委譲しつつ呼び出しごとに `calls` をインクリメントする）。
struct CountingEmbedder {
    inner: HashingEmbedder,
    calls: AtomicUsize,
}

impl CountingEmbedder {
    fn new(dim: u32) -> Self {
        Self {
            inner: HashingEmbedder::new(dim).expect("valid dim"),
            calls: AtomicUsize::new(0),
        }
    }
}

impl Embedder for CountingEmbedder {
    fn dim(&self) -> u32 {
        self.inner.dim()
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.embed_batch(texts)
    }
}

/// 2 回目以降の `embed_batch` 呼び出しで失敗するフェイク埋め込み実装
/// （文単位セマンティクスの検証用: バッチ内 2 ファイル目以降の非上限系失敗が
/// 1 ファイル目の commit 済み書き込みを巻き戻さないことを確認する）。
struct FailSecondEmbedder {
    inner: HashingEmbedder,
    calls: AtomicUsize,
}

impl FailSecondEmbedder {
    fn new(dim: u32) -> Self {
        Self {
            inner: HashingEmbedder::new(dim).expect("valid dim"),
            calls: AtomicUsize::new(0),
        }
    }
}

impl Embedder for FailSecondEmbedder {
    fn dim(&self) -> u32 {
        self.inner.dim()
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.inner.embed_batch(texts)
        } else {
            Err(EmbedError::Unavailable)
        }
    }
}

fn row_count_for_path(core: &EngineCore, read_ctx: &PolicyContext, path: &str) -> usize {
    let zero_vec = vector_literal(&vec![0.0f32; DIM as usize]);
    core.execute_sql(
        read_ctx,
        &format!(
            "SELECT body FROM documents WHERE path = '{}' ORDER BY embedding <=> {zero_vec} LIMIT 100",
            sql_escape(path)
        ),
    )
    .expect("select by path should succeed")
    .rows
    .len()
}

// --- ①: バッチあたり最大ファイル数 -------------------------------------------------

#[test]
fn too_many_files_rejected_with_54000_and_no_side_effects() {
    let path = unique_db_path("batch-limits-too-many-files");
    let _guard = CleanupGuard(path.clone());
    let storage = new_documents_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")))
        .with_incremental_config(small_chunk_config())
        .with_batch_limits(BatchLimits {
            max_files_per_batch: 2,
            ..BatchLimits::default()
        });
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");

    let sqls: Vec<String> = (0..3)
        .map(|i| {
            insert_file_sql(
                "documents",
                &format!("docs/f{i}.txt"),
                "one line",
                &format!("op-many-{i}"),
            )
        })
        .collect();
    let sql_refs: Vec<&str> = sqls.iter().map(String::as_str).collect();

    let err = core
        .execute_insert_sql_batch(&write_ctx, &sql_refs)
        .expect_err("batch over file count limit should be rejected");
    assert_eq!(err.wire_code(), "54000");

    for i in 0..3 {
        assert_eq!(
            row_count_for_path(&core, &read_ctx, &format!("docs/f{i}.txt")),
            0
        );
    }
}

/// codex-review 指摘（PR #242）の回帰テスト: embedder 未構成の `EngineCore` に
/// ②（1 ファイルあたり最大本文サイズ）超過のバッチを渡した場合でも、embedder
/// 未構成由来の `Internal`（`XX000` 相当）ではなく上限超過の `54000` が返ることを
/// 確認する（②③ の判定は各文の束縛後に逐次行われるため、embedder 取得を
/// ①②③ 完了より前に置くと ② 違反より先に embedder 未構成が検出されてしまう。
/// 「上限超過は常に `54000`」というエラー契約を embedder 構成の有無に関わらず
/// 維持する）。
#[test]
fn single_file_body_over_limit_rejected_with_54000_even_without_embedder_configured() {
    let path = unique_db_path("batch-limits-file-body-too-large-no-embedder");
    let _guard = CleanupGuard(path.clone());
    let storage = new_documents_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_incremental_config(small_chunk_config())
        .with_batch_limits(BatchLimits {
            max_file_body_bytes: 10,
            max_batch_total_bytes: 10_000,
            ..BatchLimits::default()
        });
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let sqls = [insert_file_sql(
        "documents",
        "docs/big-no-embedder.txt",
        "this body is far longer than ten bytes",
        "op-body-too-large-no-embedder",
    )];
    let sql_refs: Vec<&str> = sqls.iter().map(String::as_str).collect();

    let err = core
        .execute_insert_sql_batch(&write_ctx, &sql_refs)
        .expect_err("file body over ② should be rejected even without an embedder");
    assert_eq!(err.wire_code(), "54000");
}

#[test]
fn file_count_exactly_at_limit_succeeds() {
    let path = unique_db_path("batch-limits-file-count-boundary");
    let _guard = CleanupGuard(path.clone());
    let storage = new_documents_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")))
        .with_incremental_config(small_chunk_config())
        .with_batch_limits(BatchLimits {
            max_files_per_batch: 2,
            ..BatchLimits::default()
        });
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");

    let sqls: Vec<String> = (0..2)
        .map(|i| {
            insert_file_sql(
                "documents",
                &format!("docs/g{i}.txt"),
                "one line",
                &format!("op-boundary-{i}"),
            )
        })
        .collect();
    let sql_refs: Vec<&str> = sqls.iter().map(String::as_str).collect();

    let outcomes = core
        .execute_insert_sql_batch(&write_ctx, &sql_refs)
        .expect("batch at file count limit should succeed");
    assert_eq!(outcomes.len(), 2);

    for i in 0..2 {
        assert_eq!(
            row_count_for_path(&core, &read_ctx, &format!("docs/g{i}.txt")),
            1
        );
    }
}

// --- ②: 1 ファイルあたり最大本文サイズ ---------------------------------------------

#[test]
fn single_file_body_over_limit_rejected_with_54000_and_no_side_effects() {
    let path = unique_db_path("batch-limits-file-body-too-large");
    let _guard = CleanupGuard(path.clone());
    let storage = new_documents_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")))
        .with_incremental_config(small_chunk_config())
        .with_batch_limits(BatchLimits {
            max_file_body_bytes: 10,
            max_batch_total_bytes: 10_000,
            ..BatchLimits::default()
        });
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");

    let sqls = [insert_file_sql(
        "documents",
        "docs/big.txt",
        "this body is far longer than ten bytes",
        "op-body-too-large",
    )];
    let sql_refs: Vec<&str> = sqls.iter().map(String::as_str).collect();

    let err = core
        .execute_insert_sql_batch(&write_ctx, &sql_refs)
        .expect_err("file body over ② should be rejected");
    assert_eq!(err.wire_code(), "54000");
    assert_eq!(row_count_for_path(&core, &read_ctx, "docs/big.txt"), 0);
}

/// codex-review P1 指摘の回帰テスト（PR #242・`batch_limits::validate_raw_sql_len`）:
/// 束縛（`validate_insert` の `lexer::tokenize`・`bind_insert_form` の文字列複製）
/// より前に、生 SQL テキスト長だけで極端に巨大な単一文を拒否できることを、存在
/// しないテーブル名を使って確認する（テーブル存在検証は `validate_insert` 内部
/// （束縛前）で行われるため、もし本ガードが束縛より前に効いていなければ
/// `UndefinedTable`（`42P01`）が先に返るはずだが、本ガードにより `54000` が
/// 先に返ることを確認する）。
#[test]
fn oversized_single_statement_rejected_before_parsing_with_54000_even_for_undefined_table() {
    let path = unique_db_path("batch-limits-raw-sql-too-large");
    let _guard = CleanupGuard(path.clone());
    let storage = new_documents_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")))
        .with_incremental_config(small_chunk_config())
        .with_batch_limits(BatchLimits {
            max_file_body_bytes: 10,
            max_batch_total_bytes: 100,
            ..BatchLimits::default()
        });
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    // 予算は path に個別上限がない後段契約（③）を基礎に算出する
    // （`batch_limits.rs` の `raw_sql_len_budget` ドキュメント参照）:
    // 2 * 100 + 4096 = 4296。これを大きく超える本文を持つ、存在しない
    // テーブルへの INSERT 文を渡す。
    let huge_body = "a".repeat(5_000);
    let sql = insert_file_sql(
        "does_not_exist",
        "docs/huge.txt",
        &huge_body,
        "op-raw-sql-too-large",
    );
    let sql_refs: Vec<&str> = vec![sql.as_str()];

    let err = core
        .execute_insert_sql_batch(&write_ctx, &sql_refs)
        .expect_err("oversized raw SQL text should be rejected before parsing");
    assert_eq!(
        err.wire_code(),
        "54000",
        "raw SQL 予算超過は UndefinedTable ではなく PAYLOAD_TOO_LARGE として返るはず"
    );
}

/// 生 SQL テキスト長ガードのバッチ累計側（codex-review P1 指摘の回帰テスト・
/// PR #242）: 個々の文は②（1 ファイルあたり最大本文サイズ）の予算内でも、複数文
/// の生テキスト長合計が③由来の予算を超えれば、全文の束縛を終える前に拒否される
/// ことを確認する（存在しないテーブル名で、束縛より前に効いていることを検証）。
#[test]
fn raw_sql_batch_total_over_budget_rejected_before_parsing_later_statements() {
    let path = unique_db_path("batch-limits-raw-sql-batch-total-too-large");
    let _guard = CleanupGuard(path.clone());
    let storage = new_documents_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")))
        .with_incremental_config(small_chunk_config())
        .with_batch_limits(BatchLimits {
            max_files_per_batch: 3,
            max_file_body_bytes: 10_000_000,
            max_batch_total_bytes: 5_000,
            ..BatchLimits::default()
        });
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    // 文単位予算（per_file_budget） = 2 * 5_000 + 4_096 = 14_096。
    // バッチ累計予算（batch_raw_sql_len_budget、codex P1 追加指摘・PR #242 対応で
    // 文数分の構文オーバーヘッドを含めるよう修正）
    // = 2 * 5_000 + max_files_per_batch(3) * 4_096 = 22_288。
    // 各文は本文 7_800 バイト（生 SQL 長は framing 込みで概ね 7_900 前後）で
    // per_file_budget を大きく下回るが、3 文合計は概ね 23_700〜23_850 前後となり
    // batch 予算 22_288 を超える。3 文目は存在しないテーブルにして、束縛
    // （テーブル存在検証を含む）まで到達していないことを示す。
    let body = "a".repeat(7_800);
    let sql0 = insert_file_sql("documents", "docs/rb0.txt", &body, "op-raw-batch-0");
    let sql1 = insert_file_sql("documents", "docs/rb1.txt", &body, "op-raw-batch-1");
    let sql2 = insert_file_sql("does_not_exist", "docs/rb2.txt", &body, "op-raw-batch-2");
    let sql_refs: Vec<&str> = vec![sql0.as_str(), sql1.as_str(), sql2.as_str()];

    let err = core
        .execute_insert_sql_batch(&write_ctx, &sql_refs)
        .expect_err("raw SQL batch total over budget should be rejected");
    assert_eq!(err.wire_code(), "54000");
}

// --- ③: バッチ合計最大サイズ ---------------------------------------------------------

#[test]
fn batch_total_over_limit_rejected_with_54000_even_when_each_file_is_within_its_own_limit() {
    let path = unique_db_path("batch-limits-batch-total-too-large");
    let _guard = CleanupGuard(path.clone());
    let storage = new_documents_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")))
        .with_incremental_config(small_chunk_config())
        .with_batch_limits(BatchLimits {
            max_file_body_bytes: 100,
            max_batch_total_bytes: 100,
            ..BatchLimits::default()
        });
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");

    // 各ファイルは②（100 バイト）以内だが、3 ファイル合計は③（100 バイト）を超える。
    let body = "a".repeat(60);
    let sqls: Vec<String> = (0..3)
        .map(|i| {
            insert_file_sql(
                "documents",
                &format!("docs/t{i}.txt"),
                &body,
                &format!("op-total-{i}"),
            )
        })
        .collect();
    let sql_refs: Vec<&str> = sqls.iter().map(String::as_str).collect();

    let err = core
        .execute_insert_sql_batch(&write_ctx, &sql_refs)
        .expect_err("batch total over ③ should be rejected");
    assert_eq!(err.wire_code(), "54000");

    for i in 0..3 {
        assert_eq!(
            row_count_for_path(&core, &read_ctx, &format!("docs/t{i}.txt")),
            0
        );
    }
}

// --- ④: バッチあたり最大生成チャンク数（判定タイミング含む）------------------------

#[test]
fn too_many_chunks_rejected_with_54000_and_embedder_never_called() {
    let path = unique_db_path("batch-limits-too-many-chunks");
    let _guard = CleanupGuard(path.clone());
    let storage = new_documents_storage(&path);
    let counting_embedder = std::sync::Arc::new(CountingEmbedder::new(DIM));
    // `Box<dyn Embedder>` は所有権を要求するため、呼び出し回数の検査用に生ポインタ経由の
    // カウンタではなく `Arc` を経由して観測用ハンドルを保持する（`with_embedder` は
    // `Box` を要求するため、カウンタは `Arc` 越しに共有し、`EngineCore` へは薄い
    // `Box` ラッパー経由で委譲する）。
    struct SharedEmbedder(std::sync::Arc<CountingEmbedder>);
    impl Embedder for SharedEmbedder {
        fn dim(&self) -> u32 {
            self.0.dim()
        }
        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
            self.0.embed_batch(texts)
        }
    }

    // lines_per_chunk = 1 なので、1 行の本文 = 1 チャンク。3 ファイル × 1 行 = 3 チャンク
    // が④（上限 2）を超える。
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(SharedEmbedder(counting_embedder.clone())))
        .with_incremental_config(small_chunk_config())
        .with_batch_limits(BatchLimits {
            max_batch_chunks: 2,
            ..BatchLimits::default()
        });
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");

    let sqls: Vec<String> = (0..3)
        .map(|i| {
            insert_file_sql(
                "documents",
                &format!("docs/c{i}.txt"),
                "one line",
                &format!("op-chunks-{i}"),
            )
        })
        .collect();
    let sql_refs: Vec<&str> = sqls.iter().map(String::as_str).collect();

    let err = core
        .execute_insert_sql_batch(&write_ctx, &sql_refs)
        .expect_err("batch over ④ should be rejected");
    assert_eq!(err.wire_code(), "54000");

    assert_eq!(
        counting_embedder.calls.load(Ordering::SeqCst),
        0,
        "④ の判定は埋め込み呼び出しより前に完了し、埋め込みは一度も呼ばれない"
    );

    for i in 0..3 {
        assert_eq!(
            row_count_for_path(&core, &read_ctx, &format!("docs/c{i}.txt")),
            0
        );
    }
}

#[test]
fn chunk_total_exactly_at_limit_succeeds() {
    let path = unique_db_path("batch-limits-chunk-total-boundary");
    let _guard = CleanupGuard(path.clone());
    let storage = new_documents_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")))
        .with_incremental_config(small_chunk_config())
        .with_batch_limits(BatchLimits {
            max_batch_chunks: 2,
            ..BatchLimits::default()
        });
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");

    // 2 ファイル × 1 行 = 2 チャンク（上限ちょうど）。
    let sqls: Vec<String> = (0..2)
        .map(|i| {
            insert_file_sql(
                "documents",
                &format!("docs/d{i}.txt"),
                "one line",
                &format!("op-chunk-boundary-{i}"),
            )
        })
        .collect();
    let sql_refs: Vec<&str> = sqls.iter().map(String::as_str).collect();

    let outcomes = core
        .execute_insert_sql_batch(&write_ctx, &sql_refs)
        .expect("batch at ④ boundary should succeed");
    assert_eq!(outcomes.len(), 2);

    for i in 0..2 {
        assert_eq!(
            row_count_for_path(&core, &read_ctx, &format!("docs/d{i}.txt")),
            1
        );
    }
}

// --- 上限非起因の途中失敗（文単位セマンティクス）------------------------------------

/// 上限を超えていないバッチの 2 ファイル目で非上限系の失敗（埋め込みサービス障害）が
/// 起きた場合、1 ファイル目は個別の write トランザクションで commit 済みのまま残り、
/// 2 ファイル目だけが未索引のまま失敗する（`incremental::index_file_batch`・
/// `core::EngineCore::execute_insert_sql_batch` ドキュメントの「文単位セマンティクス」
/// 契約。本リポ独自の実装上の挙動であり、バッチ全体のロールバックはしない。上限
/// 超過時の副作用ゼロは別の契約〔TASK-122・INDEX-4〕）。台帳記録もファイル単位の
/// write トランザクションに追従することを `operation_recorded` で確認する。
#[test]
fn non_limit_failure_mid_batch_leaves_earlier_files_committed_and_is_file_scoped() {
    let path = unique_db_path("batch-limits-mid-batch-failure");
    let _guard = CleanupGuard(path.clone());
    let storage = new_documents_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(FailSecondEmbedder::new(DIM)))
        .with_incremental_config(small_chunk_config());
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let op = |id: &str| OperationId::parse(id).expect("valid operation_id");

    let sqls = [
        insert_file_sql("documents", "docs/m0.txt", "one line", "op-mid-0"),
        insert_file_sql("documents", "docs/m1.txt", "one line", "op-mid-1"),
    ];
    let sql_refs: Vec<&str> = sqls.iter().map(String::as_str).collect();

    let err = core
        .execute_insert_sql_batch(&ctx, &sql_refs)
        .expect_err("second file's embedder failure should fail the batch call");
    assert_eq!(err.wire_code(), "XX000");

    // 1 ファイル目は個別 write トランザクションで commit 済みのまま残る。
    assert_eq!(row_count_for_path(&core, &read_ctx, "docs/m0.txt"), 1);
    assert_eq!(
        core.operation_recorded(&ctx, "documents", &op("op-mid-0"))
            .expect("ledger lookup should succeed"),
        LedgerLookup::Recorded
    );

    // 2 ファイル目は埋め込み失敗のため未索引・台帳未記録のまま。
    assert_eq!(row_count_for_path(&core, &read_ctx, "docs/m1.txt"), 0);
    assert_eq!(
        core.operation_recorded(&ctx, "documents", &op("op-mid-1"))
            .expect("ledger lookup should succeed"),
        LedgerLookup::NotRecorded
    );
}

// --- 行形混在・空バッチ -------------------------------------------------------------

#[test]
fn mixed_row_and_file_form_batch_is_rejected_with_22000() {
    let path = unique_db_path("batch-limits-mixed-form");
    let _guard = CleanupGuard(path.clone());
    let storage = new_documents_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")))
        .with_incremental_config(small_chunk_config());
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");

    let file_sql = insert_file_sql(
        "documents",
        "docs/mixed-file.txt",
        "one line",
        "op-mixed-file",
    );
    let row_vec = vector_literal(&vec![0.1f32; DIM as usize]);
    let row_sql = format!(
        "INSERT INTO documents (id, embedding, path, body) VALUES (1, {row_vec}, 'docs/mixed-row.txt', 'row body') USING OPERATION_ID 'op-mixed-row'"
    );
    let sql_refs: Vec<&str> = vec![file_sql.as_str(), row_sql.as_str()];

    let err = core
        .execute_insert_sql_batch(&write_ctx, &sql_refs)
        .expect_err("mixed row/file batch should be rejected");
    assert_eq!(err.wire_code(), "22000");

    assert_eq!(
        row_count_for_path(&core, &read_ctx, "docs/mixed-file.txt"),
        0
    );
    assert_eq!(
        row_count_for_path(&core, &read_ctx, "docs/mixed-row.txt"),
        0
    );
}

#[test]
fn empty_batch_is_rejected_with_22000() {
    let path = unique_db_path("batch-limits-empty-batch");
    let _guard = CleanupGuard(path.clone());
    let storage = new_documents_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")))
        .with_incremental_config(small_chunk_config());
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let err = core
        .execute_insert_sql_batch(&write_ctx, &[])
        .expect_err("empty batch should be rejected");
    assert_eq!(err.wire_code(), "22000");
}

// --- 副作用ゼロ検証（台帳未記録） ---------------------------------------------------

#[test]
fn rejected_batch_leaves_no_ledger_entry() {
    let path = unique_db_path("batch-limits-ledger-untouched");
    let _guard = CleanupGuard(path.clone());
    let storage = new_documents_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")))
        .with_incremental_config(small_chunk_config())
        .with_batch_limits(BatchLimits {
            max_files_per_batch: 1,
            ..BatchLimits::default()
        });
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let op = |id: &str| OperationId::parse(id).expect("valid operation_id");

    let sqls: Vec<String> = (0..2)
        .map(|i| {
            insert_file_sql(
                "documents",
                &format!("docs/l{i}.txt"),
                "one line",
                &format!("op-ledger-{i}"),
            )
        })
        .collect();
    let sql_refs: Vec<&str> = sqls.iter().map(String::as_str).collect();

    core.execute_insert_sql_batch(&ctx, &sql_refs)
        .expect_err("batch over ① should be rejected");

    for i in 0..2 {
        assert_eq!(
            core.operation_recorded(&ctx, "documents", &op(&format!("op-ledger-{i}")))
                .expect("ledger lookup should succeed"),
            LedgerLookup::NotRecorded
        );
    }
}

// --- 正常系: 上限内の複数ファイルバッチが全ファイル索引化される --------------------

#[test]
fn batch_within_all_limits_indexes_every_file_and_is_searchable() {
    let path = unique_db_path("batch-limits-happy-path");
    let _guard = CleanupGuard(path.clone());
    let storage = new_documents_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")))
        .with_incremental_config(small_chunk_config());
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");

    let bodies = [
        "alpha alpha alpha token one",
        "bravo bravo bravo token two",
        "charlie charlie charlie unique marker zzzqq",
    ];
    let sqls: Vec<String> = bodies
        .iter()
        .enumerate()
        .map(|(i, body)| {
            insert_file_sql(
                "documents",
                &format!("docs/h{i}.txt"),
                body,
                &format!("op-happy-{i}"),
            )
        })
        .collect();
    let sql_refs: Vec<&str> = sqls.iter().map(String::as_str).collect();

    let outcomes = core
        .execute_insert_sql_batch(&write_ctx, &sql_refs)
        .expect("batch within all limits should succeed");
    assert_eq!(outcomes.len(), 3);
    for outcome in &outcomes {
        assert_eq!(outcome.rows_affected, 1);
        assert!(outcome.incremental.is_some());
    }

    for i in 0..3 {
        assert_eq!(
            row_count_for_path(&core, &read_ctx, &format!("docs/h{i}.txt")),
            1
        );
    }

    // 固有語を含むチャンクが密検索で Top-1 に来ることを INDEX-2 と同じ流儀で確認する。
    let embedder = HashingEmbedder::new(DIM).expect("valid dim");
    let query_vec = embedder
        .embed_batch(&["unique marker zzzqq"])
        .expect("query embedding should succeed")
        .remove(0);
    let query_literal = vector_literal(&query_vec);
    let distance_sql =
        format!("SELECT body FROM documents ORDER BY embedding <=> {query_literal} LIMIT 1");
    let distance_result = core
        .execute_sql(&read_ctx, &distance_sql)
        .expect("distance search should succeed");
    let top_body = match distance_result.rows.first().and_then(|r| r.cells.first()) {
        Some(engine::sql::exec::Cell::Text(s)) => s.clone(),
        other => panic!("expected Cell::Text, got {other:?}"),
    };
    assert!(top_body.contains("zzzqq"));
}
