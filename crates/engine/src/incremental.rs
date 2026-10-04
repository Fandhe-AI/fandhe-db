//! 増分インデックス反映モジュール（TASK-120、対象ビヘイビア: INDEX-1, INDEX-2。
//! ポインタ: `docs/spec/05-tasks.md` TASK-120・`docs/spec/04-behavior/indexing.md`
//! INDEX-1, INDEX-2）。
//!
//! 埋め込みを write トランザクションの外で行うのは autocommit 経路（[`index_file_batch`]
//! を含む）の話で、明示トランザクション内（`WriteTarget::InTxn`。Issue #1353）では BEGIN の
//! 時点で単一ライタを既に保持しており、`max_duration` の範囲内で埋め込みを行う。
//! 文の途中で期限を超えても、次の要求の期限解放と COMMIT 時の期限検査が abort させるため
//! 正しさは崩れない。
//!
//! 責務境界: ファイル形 `INSERT`（`sql::parser::BoundFileInsert`）1 件分を、
//! チャンク化（`chunking::chunk_file`。TASK-119）→ 埋め込み（`embedding::Embedder`。
//! write トランザクションの外で実行）→ テナント境界付き置換書き込み
//! （`tenant::replace_typed_rows_by_text_key`）の順で結線する（`sql/exec.rs::execute_file_insert`
//! から呼ばれる。同一パス再送時の置換セマンティクスは TASK-123 の決定に従う。
//! ポインタ: `docs/design/resend-semantics.md`）。
//!
//! 途中で失敗した場合は write トランザクションを一切開始しない、または開始済み
//! トランザクションを commit せず abort する（副作用ゼロ。coding-rust.md
//! 「エラー契約は fail-closed とする」）。
//!
//! [`index_file`] は内部的に副作用ゼロ区間の [`chunk_phase`]（チャンク化〜総バイト数
//! 上限判定まで）と、外部 I/O・write トランザクションを含む [`embed_and_write_phase`]
//! （埋め込み〜置換書き込み）の 2 フェーズへ分割されている（TASK-122・INDEX-4。挙動は
//! 分割前と同一）。[`index_file_batch`] はこの分割を利用し、バッチ内の全ファイルの
//! `chunk_phase` を先に完走させてから（副作用ゼロのまま①〜④の 4 上限を判定できる）、
//! 全判定を通過した場合のみファイルごとに `embed_and_write_phase` を実行する
//! （`batch_limits.rs` モジュールドキュメント参照）。

use std::time::{Duration, Instant};

use crate::embedding::{EmbedError, Embedder};
use crate::policy::PolicyContext;
use crate::row_codec::Value;
use crate::storage::{Storage, Visibility};
use crate::tenant::{ReplaceOutcome, TenantWriteError};

/// ファイル単位で生成してよいチャンク数の上限。埋め込み呼び出し・確保量を有界化する
/// （coding-rust.md「不安全な設計 / DoS」対応。一括投入 4 件上限は TASK-122 の管轄で
/// あり、本上限はそれを代替しない）。
pub const MAX_CHUNKS_PER_FILE: usize = 4_096;

/// 1 ファイル分のチャンク行として確保してよい総バイト数の上限。
///
/// 対象は「埋め込みベクトル」だけでなく、チャンク数分だけ複製される Text 値
/// （`path`・`lang` 等のテンプレート列）とチャンク本文も含めた合計。チャンク数
/// （[`MAX_CHUNKS_PER_FILE`]）・次元（`embedding::MAX_EMBEDDER_DIM`）・個々の Text 値
/// 長を個別に検証するだけでは積が非常に大きくなり得るため、合計へ独立した上限を
/// かける（codex-review P1 指摘・PR #221。coding-rust.md「不安全な設計 / DoS」・
/// 「整数演算は checked_* を使う」）。
pub const MAX_INDEX_TOTAL_BYTES: usize = 64 * 1024 * 1024;

/// [`index_file`] の挙動を調整する設定。
#[derive(Debug, Clone)]
pub struct IncrementalConfig {
    pub chunking: crate::chunking::ChunkingConfig,
    pub max_chunks_per_file: usize,
}

impl Default for IncrementalConfig {
    fn default() -> Self {
        Self {
            chunking: crate::chunking::ChunkingConfig::default(),
            max_chunks_per_file: MAX_CHUNKS_PER_FILE,
        }
    }
}

/// 各段階の所要時間（INDEX-1 の計測対象。呼び出し元がベンチマーク・回帰テスト
/// （TASK-121）で利用できるよう公開する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexTiming {
    pub chunking: Duration,
    pub embedding: Duration,
    /// 行バッファ構築（`template_values`/`path`/`body`/ベクトルの複製・上書き）と
    /// `tenant::replace_typed_rows_by_text_key` による redb 書き込みの合計。
    /// 行バッファ構築はチャンク数・本文サイズ・ベクトル次元に比例し得るため、
    /// パイプライン全体を計測対象とする INDEX-1 の契約上ここへ含める
    /// （codex-review P1 指摘・PR #241）。
    pub write: Duration,
}

/// [`index_file`] の成功応答。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexOutcome {
    pub chunks_written: usize,
    pub rows_replaced: usize,
    pub timing: IndexTiming,
}

/// [`index_file`] の失敗理由。`wire_code()` で `sql::allowlist::SqlSurfaceError` への
/// 写像先を示す（実際の写像は `sql/exec.rs::execute_file_insert` が集約する）。
#[derive(Debug)]
pub enum IncrementalError {
    /// チャンク化入力・出力チャンク数の上限超過（`54000` 相当）。クライアントが
    /// 送った本文サイズ・チャンク数に起因するものだけをこの variant にする。
    ChunkingTooLarge(String),
    /// サーバー側の事象に起因する失敗（`XX000` 相当）。チャンク化設定の不正
    /// （`chunking::ChunkingError::InvalidConfig`。例: `lines_per_chunk == 0`）や
    /// 行バッファ確保失敗（メモリ逼迫）が該当する。これらをクライアント入力起因の
    /// `54000` として返すと、再試行・障害判定を誤らせるため分離する
    /// （Cursor Bugbot 指摘・PR #221）。`detail` は原因を展開しない固定文言のみ。
    Internal(&'static str),
    /// 埋め込みサービスの失敗・次元不一致（`XX000` 相当。応答本文・入力本文を
    /// 含めない）。
    Embed(EmbedError),
    /// テナント境界付き書き込みの失敗（[`TenantWriteError`] をそのまま保持）。
    Write(TenantWriteError),
    /// チャンク化の結果、書き込み対象のチャンクが 0 件になった（`22000` 相当）。
    /// 空・空白のみの本文を送った場合に、既存チャンクを全削除したまま
    /// 挿入 0 件で世代だけ bump するのを防ぐ（`replace_typed_rows_by_text_key`
    /// の「同一パスの既存行を全削除 → 新チャンク行を挿入」契約が、意図しない
    /// 全削除（実質的な索引破壊）に化けるのを fail-closed に拒否する）。
    EmptyChunks,
}

impl std::fmt::Display for IncrementalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IncrementalError::ChunkingTooLarge(detail) => {
                write!(f, "chunking limit exceeded: {detail}")
            }
            IncrementalError::Internal(detail) => write!(f, "{detail}"),
            IncrementalError::Embed(e) => write!(f, "embedding failed: {e}"),
            IncrementalError::Write(_) => write!(f, "incremental index write failed"),
            IncrementalError::EmptyChunks => {
                write!(f, "file body produced no chunks to index")
            }
        }
    }
}

impl std::error::Error for IncrementalError {}

impl From<EmbedError> for IncrementalError {
    fn from(e: EmbedError) -> Self {
        IncrementalError::Embed(e)
    }
}

impl From<TenantWriteError> for IncrementalError {
    fn from(e: TenantWriteError) -> Self {
        IncrementalError::Write(e)
    }
}

/// ファイル形 `INSERT` の束縛結果（`sql::parser::bind_insert_form` が構築）。
///
/// `path`/`body` 以外の Text 列（例 `lang`）は全チャンク行へ同じ値を複製するため、
/// スキーマ列順の「チャンク本文以外」のテンプレート値をあらかじめ保持する
/// （`sql/parser.rs` モジュールドキュメント参照）。
pub struct BoundFileIndexInput<'a> {
    pub table: &'a str,
    pub path: &'a str,
    pub body: &'a str,
    /// スキーマ列順のテンプレート値。`path_column_index`/`body_column_index`/
    /// VECTOR 列の位置は呼び出し時点でプレースホルダ（`Value::Null`）でよい
    /// （各チャンク行の構築時に上書きするため）。
    pub template_values: &'a [Value],
    pub path_column_index: usize,
    pub body_column_index: usize,
    pub vector_column_index: usize,
}

/// ファイル 1 件分（パス＋本文）をチャンク化・埋め込み・置換書き込みする
/// （TASK-120・INDEX-1, INDEX-2）。`sql/exec.rs::execute_file_insert` から呼ばれる
/// 唯一の入口。
///
/// 手順（コメント順序が実装順序と一致する。§2.3 の設計順序）:
/// 1. チャンク化（`chunking::chunk_file`）— 上限超過は副作用ゼロで `Err`
/// 2. ファイル単位のチャンク数上限ガード（`config.max_chunks_per_file`）
/// 3. 埋め込み（`embedder.embed_batch`）— **write トランザクションの外**で実行
/// 4. `tenant::replace_typed_rows_by_text_key` で単一 write トランザクション内の
///    「同一パスの既存行を全削除 → 新チャンク行を挿入 → 世代 bump」
///
/// `Embedder::dim()` とスキーマの `VECTOR(N)` の不一致はチャンク化・埋め込み呼び出し
/// より前（手順の最初）に検出する。埋め込み実装が自己申告と異なる次元のベクトルを
/// 返した場合の防御的検証は埋め込み呼び出し直後で別途行う。次元検証自体は
/// `tenant.rs` 側の `validate_embedding_dim` が書き込みトランザクション内でも
/// 行うため、いずれも二重防御になる。
/// チャンク行を実際に構築した場合に確保される総バイト数の見積り（[`index_file`] が
/// [`MAX_INDEX_TOTAL_BYTES`] と突き合わせる。オーバーフローは `None` を返して
/// 呼び出し元が拒否側へ倒す）。
///
/// 内訳はチャンク数 ×（ベクトル本体 + `path` + テンプレートの Text 値合計）に
/// 全チャンク本文の合計を加えたもの。テンプレートの `path`/`body`/VECTOR 位置は
/// `sql::parser::bind_file_insert` が `Value::Null` に戻しているため二重計上しない。
fn estimate_total_row_bytes(
    chunks: &[crate::chunking::Chunk],
    input: &BoundFileIndexInput<'_>,
    table_dim: u32,
) -> Option<usize> {
    let mut template_text_bytes: usize = 0;
    for v in input.template_values {
        if let Value::Text(s) = v {
            template_text_bytes = template_text_bytes.checked_add(s.len())?;
        }
    }
    let vector_bytes = (table_dim as usize).checked_mul(std::mem::size_of::<f32>())?;
    let per_row = vector_bytes
        .checked_add(input.path.len())?
        .checked_add(template_text_bytes)?;
    let mut total = chunks.len().checked_mul(per_row)?;
    for c in chunks {
        total = total.checked_add(c.text.len())?;
    }
    Some(total)
}

/// [`chunk_phase`] の成功応答。バッチ経路（[`index_file_batch`]）が全ファイル分を
/// 一時保持してから ④（総チャンク数）判定・[`embed_and_write_phase`] へ渡すための
/// 中間データ。
struct ChunkedFile {
    chunks: Vec<crate::chunking::Chunk>,
    chunking_elapsed: Duration,
}

/// [`index_file`]・[`index_file_batch`] 共通の副作用ゼロ区間（TASK-122 分割前の
/// [`index_file`] 手順 1〜2 相当）。次元検証 → `chunk_file` → 空チャンク拒否 →
/// ファイル単位チャンク数上限 → 総バイト数上限までを行う。`storage.get_table_schema`
/// によるスキーマ読み取りは発生するが、書き込み・埋め込みサービス呼び出しは一切
/// 行わない（呼び出し元がバッチの①〜③・④判定を埋め込み・write トランザクション
/// より前に完了できるようにするための分割。`incremental.rs` モジュールドキュメント
/// 参照）。
fn chunk_phase(
    storage: &Storage,
    embedder_dim: u32,
    config: &IncrementalConfig,
    input: &BoundFileIndexInput<'_>,
) -> Result<ChunkedFile, IncrementalError> {
    // `embedder.dim()` を対象テーブルの `VECTOR(N)` と突き合わせる（サーバー側の
    // Embedder 設定とスキーマの不整合。チャンク化・埋め込み呼び出しの前に検出し、
    // 誤設定時の無駄な計算・外部 I/O を避ける。`tenant.rs` 側の
    // `validate_embedding_dim` が書き込みトランザクション内でも同じ検証を行うため
    // 二重防御になる）。
    let schema = storage
        .get_table_schema(input.table)
        .map_err(TenantWriteError::Catalog)?;
    let table_dim = schema.vector_dim().ok_or_else(|| {
        TenantWriteError::Catalog(crate::catalog::CatalogError::Invalid(
            "table has no VECTOR column".to_string(),
        ))
    })?;
    if embedder_dim != table_dim {
        return Err(IncrementalError::Embed(EmbedError::DimMismatch {
            expected: table_dim,
            got: embedder_dim as usize,
        }));
    }

    let chunking_start = Instant::now();
    // `chunk_file` の失敗を原因別に写像する。入力本文サイズ超過のみクライアント起因
    // （`54000`）とし、チャンク化設定の不正はサーバー構成の誤りとして `XX000` へ倒す
    // （Cursor Bugbot 指摘・PR #221。`detail` に設定値・本文を載せない）。
    let chunks = crate::chunking::chunk_file(input.path, input.body, &config.chunking).map_err(
        |e| match e {
            crate::chunking::ChunkingError::InputTooLarge { .. }
            | crate::chunking::ChunkingError::TooManyLines { .. } => {
                IncrementalError::ChunkingTooLarge(e.to_string())
            }
            crate::chunking::ChunkingError::InvalidConfig { .. } => {
                IncrementalError::Internal("invalid chunking configuration")
            }
        },
    )?;
    let chunking_elapsed = chunking_start.elapsed();

    // 空・空白のみの本文（`chunk_file` が 0 チャンクを返す入力）は、ここで
    // fail-closed に拒否する。ガードなしで `replace_typed_rows_by_text_key`
    // まで進むと、同一パスの既存チャンクを全削除したうえで挿入 0 件のまま
    // 世代だけ bump してコミットし、実質的な索引破壊が「成功」応答になる
    // （Issue #68 レビュー指摘）。
    if chunks.is_empty() {
        return Err(IncrementalError::EmptyChunks);
    }

    if chunks.len() > config.max_chunks_per_file {
        return Err(IncrementalError::ChunkingTooLarge(format!(
            "chunk count {} exceeds limit {}",
            chunks.len(),
            config.max_chunks_per_file
        )));
    }

    // 生成予定のチャンク行の総バイト数へ上限をかける。ベクトル（次元 × `f32`）に
    // 加え、チャンク数分だけ複製される Text 値（`path` とテンプレート列。例 `lang`）・
    // チャンク本文も数える（個別上限だけでは積・総和が有界にならない）。埋め込み
    // 呼び出しの前に checked 演算で判定し、確保も外部呼び出しも行わずに拒否する。
    let total_bytes = estimate_total_row_bytes(&chunks, input, table_dim);
    match total_bytes {
        Some(bytes) if bytes <= MAX_INDEX_TOTAL_BYTES => {}
        _ => {
            return Err(IncrementalError::ChunkingTooLarge(format!(
                "index payload too large: {} chunks x dim {} exceeds {} bytes",
                chunks.len(),
                table_dim,
                MAX_INDEX_TOTAL_BYTES
            )))
        }
    }

    Ok(ChunkedFile {
        chunks,
        chunking_elapsed,
    })
}

/// [`index_file`]・[`index_file_batch`] 共通の埋め込み〜置換書き込み区間
/// （TASK-122 分割前の [`index_file`] 手順 3〜4 相当）。[`chunk_phase`] の成功応答を
/// 受け取り、埋め込み（write トランザクションの外）→ 単一 write トランザクション内の
/// 置換書き込みの順に実行する。バッチ経路では①〜④のすべての限度判定を通過した
/// ファイルに対してのみ呼ばれる契約（`index_file_batch` ドキュメント参照）。
fn embed_and_write_phase(
    target: crate::tenant::WriteTarget<'_>,
    ctx: &PolicyContext,
    embedder: &dyn Embedder,
    input: &BoundFileIndexInput<'_>,
    chunked: ChunkedFile,
    ledger_write: crate::recovery::ledger::LedgerWrite<'_>,
) -> Result<IndexOutcome, IncrementalError> {
    let ChunkedFile {
        chunks,
        chunking_elapsed,
    } = chunked;

    let embedding_start = Instant::now();
    let texts: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();
    let vectors = embedder.embed_batch(&texts)?;
    if vectors.len() != chunks.len() {
        return Err(IncrementalError::Embed(EmbedError::InvalidResponse));
    }
    let expected_dim = embedder.dim();
    for v in &vectors {
        if v.len() != expected_dim as usize {
            return Err(IncrementalError::Embed(EmbedError::DimMismatch {
                expected: expected_dim,
                got: v.len(),
            }));
        }
        // 非有限値（`NaN`/`±Inf`）を永続化前に拒否する。SQL のベクトルリテラルは
        // `sql::parser::parse_vector_literal` が同じ検証を行っており、外部実装も
        // 許容する `Embedder` の応答経路がこの防御を迂回すると、距離計算・順位付けが
        // 不定になった行が索引に残り検索が継続的に壊れる（codex-review P1 指摘・
        // PR #221。coding-rust.md「エラー契約は fail-closed とする」）。
        if v.iter().any(|x| !x.is_finite()) {
            return Err(IncrementalError::Embed(EmbedError::InvalidResponse));
        }
    }
    let embedding_elapsed = embedding_start.elapsed();

    // `write_start` は行バッファ構築（テンプレート値の複製・`path`/`body`/ベクトルの
    // 上書き）の直前に起点を置く。この構築処理はチャンク数・本文サイズ・ベクトル
    // 次元に比例し得るため、`IndexTiming.write` を「redb 書き込みのみ」に狭めると
    // ファイル形 `INSERT` パイプライン全体（本モジュールドキュメントの手順 4 相当）の
    // うちここだけが計測から漏れ、区間の性能退行を検出できなくなる
    // （codex-review P1 指摘・PR #241）。
    let write_start = Instant::now();
    let mut rows: Vec<Vec<Value>> = Vec::new();
    rows.try_reserve_exact(chunks.len())
        .map_err(|_| IncrementalError::Internal("failed to reserve chunk rows"))?;
    for (chunk, vector) in chunks.iter().zip(vectors) {
        let mut values = input.template_values.to_vec();
        if let Some(slot) = values.get_mut(input.path_column_index) {
            *slot = Value::Text(input.path.to_string());
        }
        if let Some(slot) = values.get_mut(input.body_column_index) {
            *slot = Value::Text(chunk.text.clone());
        }
        if let Some(slot) = values.get_mut(input.vector_column_index) {
            *slot = Value::Vector(vector);
        }
        rows.push(values);
    }

    let ReplaceOutcome {
        removed: _,
        inserted,
        first_id: _,
    } = crate::tenant::replace_typed_rows_by_text_key(
        target,
        ctx,
        crate::tenant::ReplaceByTextKey {
            table: input.table,
            // ファイル形 INSERT のチャンク置換キーは常に `path` 列（`sql/parser.rs`
            // モジュールドキュメントの判別規則 §2.1 で固定されている）。
            key_column: "path",
            key_value: input.path,
            visibility: Visibility::Private,
            rows: &rows,
            // 内容照合ハッシュ（TASK-101・RECOVER-10）はチャンク化・埋め込み前の
            // raw クライアント要求から構成する（codex-review P1 指摘・PR #248。
            // `tenant::ReplaceByTextKey` ドキュメント参照）。
            content_hash_path: input.path,
            content_hash_body: input.body,
            content_hash_template_values: input.template_values,
            ledger_write,
        },
    )?;
    let write_elapsed = write_start.elapsed();

    Ok(IndexOutcome {
        chunks_written: chunks.len(),
        rows_replaced: inserted,
        timing: IndexTiming {
            chunking: chunking_elapsed,
            embedding: embedding_elapsed,
            write: write_elapsed,
        },
    })
}

/// 可視性: `operation_id` 必須化ガード（TASK-92・RECOVER-1）を自身では適用しない
/// 内部結線用 API のため `pub(crate)` に閉じる（`sql/exec.rs::execute_file_insert`
/// が唯一の呼び出し元。codex-review P1 指摘・PR #221）。
///
/// `ledger_write` は行形 `INSERT` 経路（`tenant::insert_typed_row_unchecked`）と同じ
/// 契約で、置換書き込みと同一の write トランザクション内で台帳へ記録される
/// （TASK-93・RECOVER-2。`LedgerWrite::Disabled` なら台帳へ一切触れない）。
///
/// 実体は [`chunk_phase`] → [`embed_and_write_phase`] を直列に呼ぶだけ（TASK-122 で
/// バッチ経路 [`index_file_batch`] と共通化するために分割。挙動は分割前と同一）。
pub(crate) fn index_file(
    target: crate::tenant::WriteTarget<'_>,
    storage: &Storage,
    ctx: &PolicyContext,
    embedder: &dyn Embedder,
    config: &IncrementalConfig,
    input: &BoundFileIndexInput<'_>,
    ledger_write: crate::recovery::ledger::LedgerWrite<'_>,
) -> Result<IndexOutcome, IncrementalError> {
    let chunked = chunk_phase(storage, embedder.dim(), config, input)?;
    embed_and_write_phase(target, ctx, embedder, input, chunked, ledger_write)
}

/// [`index_file_batch`] の失敗理由。バッチ全体に対する上限超過（[`batch_limits`]・
/// TASK-122・INDEX-4）と、個々のファイルに起因する非上限系の失敗（[`IncrementalError`]。
/// 単一ファイル経路 [`index_file`] と同じ写像先）を型で区別する（`sql/exec.rs` が
/// `wire_code` の写像先を変えるための区別。上限超過は常に `54000`、`Item` は
/// `IncrementalError` 自身の分類にそのまま従う）。
#[derive(Debug)]
pub(crate) enum BatchIncrementalError {
    /// ①〜④いずれかの上限超過（副作用ゼロ。`crate::batch_limits::BatchLimitsError`
    /// を参照）。
    Limits(crate::batch_limits::BatchLimitsError),
    /// バッチ内 `index` 番目のファイルに起因する非上限系の失敗。全上限判定を通過した
    /// 後の埋め込み・書き込み段階でのみ発生し得る（すでに処理済みの先行ファイルは
    /// 個別の write トランザクションで commit 済みのまま残る。文単位セマンティクス。
    /// `index_file_batch` ドキュメント参照）。
    Item {
        index: usize,
        source: IncrementalError,
    },
    /// 特定ファイルに起因しない、バッチ全体に対する内部エラー（一括バッファの
    /// `try_reserve_exact` 失敗＝ DoS 対策の確保拒否等）。`Item` と型で分離し、
    /// 「バッチ内のどのファイルが原因か」という誤解を招く表現を避ける。
    Internal(&'static str),
}

impl std::fmt::Display for BatchIncrementalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BatchIncrementalError::Limits(e) => write!(f, "{e}"),
            BatchIncrementalError::Item { index, source } => {
                write!(f, "file at batch index {index}: {source}")
            }
            BatchIncrementalError::Internal(detail) => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for BatchIncrementalError {}

/// バッチ 1 件分の入力（束縛済みファイル形 `INSERT` の入力と、その `operation_id` から
/// 解決済みの台帳書き込み指示の組）。`core::EngineCore::execute_insert_sql_batch` が
/// バッチ内の各文を `sql::allowlist::validate_insert` → `sql::parser::bind_insert_form`
/// で束縛したうえで構築する。
pub(crate) struct BatchFileIndexItem<'a> {
    pub input: BoundFileIndexInput<'a>,
    pub ledger_write: crate::recovery::ledger::LedgerWrite<'a>,
}

/// 複数ファイル（`items`）を 1 バッチとして索引化する（ポインタ: TASK-122・
/// `docs/spec/04-behavior/indexing.md` INDEX-4。上限の分類・判定順序は本リポ独自の
/// 実装上の設計であり、spec 側契約の構造をそのまま転記したものではない
/// （spec-confidentiality.md 準拠））。`core::EngineCore::execute_insert_sql_batch`
/// からのみ呼ばれる想定で、[`index_file`] と同じく `operation_id` 必須化ガード
/// （TASK-92・RECOVER-1）を自身では適用しないため `pub(crate)` に閉じる。
///
/// 本実装での手順:
/// 1. `items` の `(path.len(), body.len())` から `batch_limits::validate_batch_shape`
///    を呼ぶ（チャンク化・埋め込み・write トランザクションのいずれよりも前）。
/// 2. ファイルを 1 件ずつ [`chunk_phase`] にかけ（副作用ゼロ）、生成チャンク数を
///    `checked_add` で累算するたびに `batch_limits::validate_chunk_total` を都度
///    呼ぶ（オーバーフローは `54000` へ倒す）。超過を検出した時点で
///    残りファイルの `chunk_phase` を実行せず即座に拒否する（早期リターン）。これに
///    より上限超過時に保持する `ChunkedFile` を上限判定通過分だけに抑え、超過検出
///    前の無制限なチャンク生成・保持を防ぐ。
/// 3. 2 をすべて通過した場合のみ、ファイルごとに [`embed_and_write_phase`] を実行
///    する（write トランザクションはファイル単位。TASK-120 の既存契約を維持）。
///
/// 1〜2 のいずれかで拒否された場合、`chunk_phase` は redb への読み取り（スキーマ
/// 参照）のみで書き込み・埋め込みサービス呼び出しを行わないため副作用はゼロ
/// （redb・インメモリ索引・`operation_id` 台帳とも変更なし。3 の write トランザク
/// ション自体が開始されていないため台帳記録も発生しない）。
///
/// 3 の途中（例: 2 ファイル目の埋め込み失敗）で非上限起因の失敗が起きた場合は
/// 文単位セマンティクスとする（すでに `embed_and_write_phase` を完走したファイルは
/// 個別の write トランザクションで commit 済みのまま残り、ロールバックしない）。
pub(crate) fn index_file_batch(
    storage: &Storage,
    ctx: &PolicyContext,
    embedder: &dyn Embedder,
    config: &IncrementalConfig,
    limits: &crate::batch_limits::BatchLimits,
    items: Vec<BatchFileIndexItem<'_>>,
) -> Result<Vec<IndexOutcome>, BatchIncrementalError> {
    // ①②③: バッチの解析段階（チャンク化・埋め込み・write トランザクションより前）。
    let shapes: Vec<(usize, usize)> = items
        .iter()
        .map(|item| (item.input.path.len(), item.input.body.len()))
        .collect();
    crate::batch_limits::validate_batch_shape(&shapes, limits)
        .map_err(BatchIncrementalError::Limits)?;

    // 全ファイルの chunk_phase を先に完走させる（副作用ゼロ区間のみ）。④の上限判定は
    // checked_add の直後・毎ファイルで都度行い、超過を検出した時点で残りファイルの
    // chunk_phase を実行せず早期に拒否する（大量チャンクの生成・保持による DoS を防ぐ）。
    let mut chunked_files: Vec<ChunkedFile> = Vec::new();
    chunked_files
        .try_reserve_exact(items.len())
        .map_err(|_| BatchIncrementalError::Internal("failed to reserve batch chunk buffer"))?;
    let mut total_chunks: usize = 0;
    for (index, item) in items.iter().enumerate() {
        let chunked = chunk_phase(storage, embedder.dim(), config, &item.input)
            .map_err(|e| BatchIncrementalError::Item { index, source: e })?;
        total_chunks =
            total_chunks
                .checked_add(chunked.chunks.len())
                .ok_or(BatchIncrementalError::Limits(
                    crate::batch_limits::BatchLimitsError::TooManyChunks {
                        total: usize::MAX,
                        max: limits.max_batch_chunks,
                    },
                ))?;
        // ④: チャンク分割後・埋め込み処理の開始前。checked_add 直後に都度判定し、
        // 超過時は chunked_files への保持・残りファイルの chunk_phase を行わない。
        crate::batch_limits::validate_chunk_total(total_chunks, limits)
            .map_err(BatchIncrementalError::Limits)?;
        chunked_files.push(chunked);
    }

    // 1〜3 をすべて通過した場合のみ、ファイルごとに埋め込み・write トランザクション
    // を実行する（文単位セマンティクス。上記ドキュメント参照）。
    let mut outcomes: Vec<IndexOutcome> = Vec::new();
    outcomes
        .try_reserve_exact(items.len())
        .map_err(|_| BatchIncrementalError::Internal("failed to reserve batch outcome buffer"))?;
    for (index, (item, chunked)) in items.into_iter().zip(chunked_files).enumerate() {
        let outcome = embed_and_write_phase(
            crate::tenant::WriteTarget::Autocommit(storage),
            ctx,
            embedder,
            &item.input,
            chunked,
            item.ledger_write,
        )
        .map_err(|e| BatchIncrementalError::Item { index, source: e })?;
        outcomes.push(outcome);
    }

    Ok(outcomes)
}
