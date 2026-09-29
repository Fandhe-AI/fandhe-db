//! コールドスタート・ベクトルアリーナ（TASK-87、対象ビヘイビア: TABLE-8。
//! ポインタ: `docs/spec/05-tasks.md` TASK-87・`docs/spec/04-behavior/data-model.md`
//! TABLE-8）。
//!
//! 責務境界: `storage.rs` はクエリの都度 `redb` から行を読み直してデコードする経路
//! （[`crate::storage::Storage::get`]・[`crate::storage::Storage::scan`] 等）しか
//! 提供しない。本モジュールは、単一の読み取りスナップショットから一度だけ全行を
//! 連続 `Vec<f32>` バッファへデコードし、以降の参照はそのバッファ上のスライスで
//! 完結させる「コールドスタート時の一括ロード」経路を追加する。検索カーネル本体
//! （スコアリング・top-k・SIMD/GPU 経路）は後続タスクの管轄であり、本モジュールは
//! 「一度デコードした連続バッファの提供」までを責務境界とする。
//!
//! テーブル帰属: `catalog.rs` のテーブルスコープ行 API（TASK-146、対象ビヘイビア:
//! EXT-1, EXT-2）が、テーブルごとに独立した動的 redb テーブル
//! （[`crate::catalog::user_rows_table_name`] が指す `user_rows/{table_name}`）へ行を
//! 分離して永続化する。[`VectorArena::build`] はこの対象テーブル専用のテーブルだけを
//! 走査するため、他テーブルの行が次元一致だけで混入することはない
//! （かつてのスコープ外事項だった「行への永続的なテーブル識別子」の欠如は解消済み）。
//! カタログ・行テーブルの両方を単一の `redb::ReadTransaction` 上で扱うことで、
//! スキーマ取得と行走査の間に他テーブルの並行書き込みが挟まる TOCTOU も避ける。
//!
//! [`VectorArena::build`] は `pub` な公開エントリポイントであり、後続の検索カーネル
//! （TASK-133 以降）がコールドスタート時に呼び出す想定の API である。検索カーネル本体は
//! 本モジュールのスコープ外だが、`VectorArena` 自体はクレート外の呼び出し元
//! （`crates/engine/tests/arena.rs` 等の統合テストを含む）から構築・参照できる。
//!
//! RLS との関係: `tenant_id`・`visibility` はデータとして同居保持し、
//! [`VectorArena::build_filtered`] は呼び出し元が渡す述語（`predicate`）の実行結果に
//! 従って行を格納するかどうかを分岐するだけで、可視性判定ロジックそのもの
//! （テナント一致判定・許可ラベル評価）は持たない。判定ロジックは
//! `core.rs::EngineCore::search` が [`crate::policy::PolicyContext::is_visible`]
//! （単一照合パス）から構築した述語として渡す（CORE-2 の判定ロジック集約を維持する。
//! codex P2・Issue #137 対応で構築時フィルタ経路を追加。詳細は
//! [`VectorArena::build_filtered`] のドキュメント参照）。

use redb::{ReadableDatabase, ReadableTable};

use crate::catalog::{self, CatalogError};
use crate::storage::{Storage, StorageError, Visibility};

/// アリーナが保持してよい行数の上限（アロケーション前の事前検証に使う。
/// security.md「不安全な設計｜無制限リソース確保（DoS）」対応）。`pub(crate)`:
/// `rls.rs::SearchTimeFilter`（TASK-134、ストリーミング走査。アリーナを保持しないが
/// 可視行の論理データ量上限は本モジュールと揃える）が同じ数値を参照する。
/// `VectorArena::build_filtered_with_rows_in_txn_capturing` の `rls_capture` フックの
/// 型（Issue #363。RLS 段を通過した行ごとの `(id, tenant_id, visibility, embedding,
/// metadata)` 通知。`clippy::type_complexity` 回避のためのエイリアス。呼び出し文脈は
/// 同関数のドキュメント参照）。
///
/// 第 6 引数 `response_arena_bytes_in_use` は、呼び出し元（`build_filtered_with_rows_
/// and_limits_in_txn_capturing` 本体）がこの時点までにクエリ応答用アリーナ
/// （`buffers`）へ確保済みの見積もりバイト量（Issue #379 codex-review P1 対応）。
/// `SqlArenaCaptureBuilder::push` はこの値を自身の採取済みバイト量と合算し、単一の
/// ピークメモリ予算（`max_bytes`）で判定する。クエリ本体と採取データを別枠の上限で
/// 並行構築すると、選択性の高い `WHERE` で応答用アリーナが小さくても RLS 通過行
/// 全体を採取する採取用アリーナが単独で上限まで膨らみ得るため（型ドキュメント
/// 「型ドキュメント」参照）、両者を合算した単一予算に統一する。
pub(crate) type RlsCaptureFn<'a> = dyn FnMut(u64, &str, Visibility, &[f32], &[u8], usize) -> std::result::Result<(), ArenaError>
    + 'a;

pub(crate) const MAX_ARENA_ROWS: usize = 1_000_000;

/// アリーナが確保してよい総バイト量の上限。`vectors: Vec<f32>` だけでなく、
/// 同時に確保する `ids`・`tenant_ids`・`visibilities` の見積もりバイト量も合算した
/// 総量として扱う（[`check_capacity`] 参照。codex レビュー指摘対応: 一部バッファのみを
/// 上限対象にすると、他バッファの確保が上限検証をすり抜けて OOM を招き得るため）。
/// `MAX_ARENA_ROWS` だけでは次元数が大きい場合に依然として巨大確保になり得るため、
/// 行数とバイト量の両方で上限を課す（`storage.rs` の `MAX_SCAN_TOTAL_BYTES` と同方針）。
/// `pub(crate)`: [`MAX_ARENA_ROWS`] と同じ理由で `rls.rs::SearchTimeFilter` が参照する。
pub(crate) const MAX_ARENA_TOTAL_BYTES: usize = 1024 * 1024 * 1024;

/// アリーナ構築層の公開エラー型。`storage.rs` の設計メモ（`StorageError` への
/// blanket `From<E: Into<redb::Error>>` impl）が存在するため coherence 制約上
/// 同種の blanket impl はこの型へ追加できない。`redb` へは必ず `StorageError` 経由で
/// 到達し、本型は `StorageError` からの明示的な `From` のみを持つ
/// （`catalog.rs` の `CatalogError` と同方針）。
#[derive(Debug)]
pub enum ArenaError {
    /// 永続化層側で発生したエラー（`redb` バックエンドエラー・行デコード失敗等）。
    Storage(StorageError),
    /// カタログ層側で発生したエラー（対象テーブル不存在・識別子不正等）。
    Catalog(CatalogError),
    /// `expected_dim` が不正（`0` または [`crate::storage::MAX_EMBEDDING_DIM`] 超過）。
    /// 対象テーブルが `VECTOR` 列を持たない場合もこの variant を返す。
    InvalidDim,
    /// アリーナ構築対象の行数・総バイト量がアロケーション前の上限
    /// （[`MAX_ARENA_ROWS`]・[`MAX_ARENA_TOTAL_BYTES`]）を超過した。fail-closed に拒否する。
    CapacityExceeded,
    /// `expected_dim` と一致しない次元の行を検出した。黙殺スキップせず拒否する
    /// （部分的なアリーナを返さない。fail-open を避けるための判断）。
    DimMismatch { id: u64, expected: u32, found: u32 },
    /// `check_capacity` によるアロケーション前の上限検証を通過した後、実際の
    /// `Vec::try_reserve` がメモリ不足で失敗した。`Vec::with_capacity`・`Vec::push`
    /// の内部確保（失敗時に abort する）ではなく `try_reserve` を使うことで、
    /// OOM を allocator の abort ではなく `Err` として呼び出し元へ伝える
    /// （security.md「不安全な設計｜無制限リソース確保（DoS）」対応。メッセージは
    /// プログラム出力文字列のため英語）。
    AllocationFailed(String),
    /// 呼び出し元フック（`on_visible_row`）が、行データではなく**値**を理由に構築を
    /// 拒否した（TASK-79・SQL-9: `WHERE` の式述語が 0 除算・非有限値等で fail-closed に
    /// 拒否した場合）。データ破損・実装バグを表す [`ArenaError::Storage`] とは区別し、
    /// 呼び出し元（`sql::exec::map_arena_error`）が `SqlSurfaceError::InvalidInput`
    /// （`22000`）へ写像できるようにする。`arena.rs` は sql 表層の型に依存しないよう、
    /// メッセージは呼び出し元がテナント・行内容を含めずに構築した固定文言に限る。
    InvalidInput(String),
    /// フックが 0 除算を理由に拒否した（Issue #1163）。`sql::exec::map_arena_error` が
    /// `SqlSurfaceError::DivisionByZero`（`22012`）へ写像する。メッセージは固定文言。
    DivisionByZero(String),
    /// フックが算術あふれ（非有限値化）を理由に拒否した（Issue #1163）。
    /// `sql::exec::map_arena_error` が `SqlSurfaceError::NumericOutOfRange`（`22003`）へ
    /// 写像する。メッセージは固定文言。
    NumericOutOfRange(String),
}

impl std::fmt::Display for ArenaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArenaError::Storage(e) => write!(f, "arena storage error: {e}"),
            ArenaError::Catalog(e) => write!(f, "arena catalog error: {e}"),
            ArenaError::InvalidDim => write!(f, "invalid expected_dim for arena build"),
            ArenaError::CapacityExceeded => write!(f, "arena capacity exceeded"),
            ArenaError::DimMismatch {
                id,
                expected,
                found,
            } => write!(
                f,
                "embedding dim mismatch at row id={id}: expected={expected} found={found}"
            ),
            ArenaError::AllocationFailed(msg) => write!(f, "arena allocation failed: {msg}"),
            ArenaError::InvalidInput(msg)
            | ArenaError::DivisionByZero(msg)
            | ArenaError::NumericOutOfRange(msg) => {
                write!(f, "arena build rejected by hook: {msg}")
            }
        }
    }
}

impl std::error::Error for ArenaError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ArenaError::Storage(e) => Some(e),
            ArenaError::Catalog(e) => Some(e),
            ArenaError::InvalidDim
            | ArenaError::CapacityExceeded
            | ArenaError::DimMismatch { .. }
            | ArenaError::AllocationFailed(_)
            | ArenaError::InvalidInput(_)
            | ArenaError::DivisionByZero(_)
            | ArenaError::NumericOutOfRange(_) => None,
        }
    }
}

impl From<StorageError> for ArenaError {
    fn from(e: StorageError) -> Self {
        ArenaError::Storage(e)
    }
}

impl From<CatalogError> for ArenaError {
    fn from(e: CatalogError) -> Self {
        ArenaError::Catalog(e)
    }
}

pub type Result<T> = std::result::Result<T, ArenaError>;

/// [`VectorArena::build_filtered`] のアロケーション前上限検証（行数・総バイト量の両方、
/// `checked_mul`/`checked_add` によるオーバーフロー安全な演算）。成功時は、その時点まで
/// 確保すべき `vectors: Vec<f32>` の要素数（`row_count * dim`）を返す（呼び出し元は
/// 現在のところ値そのものは使わず、検証だけに使う）。
///
/// `build_filtered` は本関数を、`predicate` を通過した（＝実際にアリーナへ格納する）
/// 可視行数を `row_count` として、可視行を 1 件追加するたびに呼ぶ（codex 指摘・
/// Issue #137 対応: テーブル全行数を上限判定に使うと、対象テナントの可視行が少なくても
/// 他テナントの不可視行を含む総数次第で検索が失敗し、テナント間で可用性が干渉するため。
/// [`VectorArena::build_filtered`] のドキュメント参照）。
///
/// `max_bytes` は `vectors` だけでなく、同時に確保する `ids: Vec<u64>`・
/// `tenant_ids: Vec<String>`・`visibilities: Vec<Visibility>` の見積もりバイト量も
/// 合算した総量として扱う（codex レビュー指摘対応: `vectors` のみを上限対象にすると、
/// 最大 1M 行分の他バッファの確保が上限検証をすり抜け、`Vec::with_capacity` の
/// 確保失敗時 abort（OOM abort）を招き得るため）。`tenant_ids` の 1 要素あたりは
/// `String` 構造体自体のスタック分に加え、ヒープ上の文字列バイト列を最悪ケース
/// （[`crate::storage::MAX_TENANT_ID_LEN`]）で見積もる。
///
/// 上限値（`max_rows`・`max_bytes`）を引数として受け取る形に切り出しているのは、
/// `MAX_ARENA_ROWS`・`MAX_ARENA_TOTAL_BYTES` が private 定数であり、境界値検証
/// （ちょうど上限・上限+1 等）を本ファイル内の `#[cfg(test)]` モジュールから
/// 直接パラメータ化して再現するため。
/// `pub(crate)`: `rls.rs::SearchTimeFilter`（TASK-134、ストリーミング走査）が可視行の
/// 論理データ量上限を本モジュールと同じ判定ロジックで検査するために再利用する
/// （容量上限の重複実装を避ける）。
pub(crate) fn check_capacity(
    row_count: usize,
    dim: u32,
    max_rows: usize,
    max_bytes: usize,
) -> Result<usize> {
    if row_count > max_rows {
        return Err(ArenaError::CapacityExceeded);
    }
    let total_floats = row_count
        .checked_mul(dim as usize)
        .ok_or(ArenaError::CapacityExceeded)?;
    let total_bytes = estimate_row_bytes(row_count, dim)?;
    if total_bytes > max_bytes {
        return Err(ArenaError::CapacityExceeded);
    }
    Ok(total_floats)
}

/// `row_count` 行分の `vectors`/`ids`/`tenant_ids`/`visibilities` の見積もりバイト量
/// （[`check_capacity`] の本体から切り出した純粋計算部分。Issue #379 codex-review P1
/// 対応: `sql::exec.rs` のキャッシュ採取経路が、クエリ応答用アリーナ（本関数と同じ式で
/// 見積もる）と採取用アリーナ（[`SqlArenaCaptureBuilder`]）を合算した単一のピーク
/// メモリ予算で上限判定できるよう、両者が同じ見積もり式を共有する）。
fn estimate_row_bytes(row_count: usize, dim: u32) -> Result<usize> {
    let total_floats = row_count
        .checked_mul(dim as usize)
        .ok_or(ArenaError::CapacityExceeded)?;
    let vectors_bytes = total_floats
        .checked_mul(4)
        .ok_or(ArenaError::CapacityExceeded)?;

    let per_row_aux_bytes = per_row_aux_bytes().ok_or(ArenaError::CapacityExceeded)?;
    let aux_bytes = row_count
        .checked_mul(per_row_aux_bytes)
        .ok_or(ArenaError::CapacityExceeded)?;

    vectors_bytes
        .checked_add(aux_bytes)
        .ok_or(ArenaError::CapacityExceeded)
}

/// `ids: Vec<u64>`・`tenant_ids: Vec<String>`・`visibilities: Vec<Visibility>` の
/// 1 行あたりの見積もりバイト数（[`check_capacity`] 専用のヘルパー、境界値テストからも
/// 同じ値を参照できるよう関数として切り出す）。`tenant_ids` の 1 要素あたりは `String`
/// 構造体自体のスタック分に加え、ヒープ上の文字列バイト列を最悪ケース
/// （[`crate::storage::MAX_TENANT_ID_LEN`]）で見積もる。
fn per_row_aux_bytes() -> Option<usize> {
    std::mem::size_of::<u64>()
        .checked_add(std::mem::size_of::<String>())
        .and_then(|v| v.checked_add(crate::storage::MAX_TENANT_ID_LEN as usize))
        .and_then(|v| v.checked_add(std::mem::size_of::<Visibility>()))
}

/// `Vec::try_reserve_exact` の失敗を [`ArenaError::AllocationFailed`] へ変換する共通
/// ヘルパー（[`GrowableArenaBuffers::ensure_capacity`] 専用）。`Vec::with_capacity`・
/// `Vec::push` の内部確保（失敗時に abort する）ではなく本関数経由で予約することで、
/// `check_capacity` の上限検証を通過した後にホスト側のメモリが実際に不足した場合でも、
/// プロセスを OOM abort させず `Err` として呼び出し元へ伝える（security.md「不安全な設計｜
/// 無制限リソース確保（DoS）」対応）。`what` はエラーメッセージに含める対象バッファ名
/// （英語。プログラム出力文字列）。
///
/// `try_reserve`（amortized 成長。実装が将来の追加分を見越して多めに確保しうる）ではなく
/// `try_reserve_exact`（要求量ちょうどだけ確保する厳密版）を使う（codex P1 指摘・
/// Issue #137 対応: `try_reserve` は要求量より多く確保しうるため、`check_capacity` が
/// 検証した論理必要量を超える実確保が起こり得る。[`GrowableArenaBuffers::ensure_capacity`]
/// が amortized 成長そのものを「行数換算の capacity 目標」として自前で管理し、
/// その目標値自体を `check_capacity` で検証してから本関数でちょうど確保することで、
/// 実確保総量が上限を超えないことを保証する）。
fn try_reserve_exact<T>(buf: &mut Vec<T>, additional: usize, what: &str) -> Result<()> {
    buf.try_reserve_exact(additional)
        .map_err(|e| ArenaError::AllocationFailed(format!("failed to reserve {what}: {e}")))
}

/// [`VectorArena::build_filtered_with_limits`] が可視行を蓄積する 4 本のバッファ
/// （`vectors`/`ids`/`tenant_ids`/`visibilities`）と、それらの「行数換算の capacity」を
/// まとめて管理する内部ヘルパー（codex P1 指摘・Issue #137 対応）。
///
/// `Vec::try_reserve`（amortized 成長）は要求量より多く確保しうるため、
/// `check_capacity` が検証した論理必要量（行を追加するたびの `visible_row_count`）
/// だけを見て確保すると、実際に確保される capacity ベースの総バイト量が
/// `MAX_ARENA_TOTAL_BYTES` を超えうる。本ヘルパーは amortized 成長を自前で管理し、
/// 次に成長させる capacity（行数換算）の候補を、実際に `try_reserve_exact` で確保する
/// **前**に `check_capacity` へそのまま通す（[`Self::ensure_capacity`] 参照）ことで、
/// capacity ベースの確保量そのものが上限を超えないことを保証する。4 本のバッファは
/// 常に同じ行数分だけまとめて成長させるため、capacity は単一のスカラー
/// （[`Self::row_capacity`]）で管理できる。
struct GrowableArenaBuffers {
    vectors: Vec<f32>,
    ids: Vec<u64>,
    tenant_ids: Vec<String>,
    visibilities: Vec<Visibility>,
    /// 4 本のバッファが共通して確保済みの行数（capacity 換算。実際の `Vec::capacity()`
    /// と一致するよう、成長は必ず [`Self::ensure_capacity`] 経由でのみ行う）。
    row_capacity: usize,
}

impl GrowableArenaBuffers {
    fn new() -> Self {
        Self {
            vectors: Vec::new(),
            ids: Vec::new(),
            tenant_ids: Vec::new(),
            visibilities: Vec::new(),
            row_capacity: 0,
        }
    }

    /// 呼び出し元が次に追加する行を含めて `target_row_count` 行を保持できるよう、
    /// 必要であれば 4 本のバッファの capacity を成長させる。呼び出し元は、行を
    /// バッファへ追加する前に必ず本メソッドを呼ぶこと（`target_row_count` は
    /// 呼び出し元側で `check_capacity` により論理必要量として検証済みの値を渡す。
    /// `build_filtered_with_limits` 参照）。
    fn ensure_capacity(
        &mut self,
        target_row_count: usize,
        dim_usize: usize,
        dim_u32: u32,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<()> {
        if target_row_count <= self.row_capacity {
            // 既存の capacity で足りる。amortized 成長で確保済みの分を再利用するだけで、
            // 新規確保は発生しない（＝実確保総量は増えない）。
            return Ok(());
        }

        // amortized 成長の候補（capacity を倍々に増やす。`checked_mul` がオーバーフロー
        // する巨大な capacity では上限 `max_rows` へ丸める）。
        let doubled = self.row_capacity.checked_mul(2).unwrap_or(max_rows);
        let candidate = doubled.max(target_row_count).min(max_rows);

        // 成長候補（capacity 換算の行数）に基づく総確保バイト量を、実際に
        // `try_reserve_exact` で確保する前に検証する。`check_capacity` は
        // vectors/ids/tenant_ids/visibilities 全体の見積もりバイト量を checked 演算で
        // 計算する既存ヘルパーで、ここでは `candidate`（capacity 換算の行数）を渡す
        // ことで「これから確保しようとしている capacity」自体を検証対象にする
        // （`target_row_count`＝論理必要量ではない点が [`check_capacity`] の他の
        // 呼び出しとの違い）。
        let new_row_capacity = match check_capacity(candidate, dim_u32, max_rows, max_bytes) {
            Ok(_) => candidate,
            // amortized な先読み確保が上限を超えるなら、先読みを諦めて「今必要な分
            // だけちょうど」へ縮退する（`try_reserve_exact` へ切り替える。codex P1
            // 対応）。`target_row_count` は呼び出し元が既に `check_capacity` で
            // 検証済みの値なので、この再検証は必ず成功するはずだが、呼び出し元との
            // 不整合（バグ）があった場合に備えて `?` で fail-closed に伝播する。
            Err(ArenaError::CapacityExceeded) => {
                check_capacity(target_row_count, dim_u32, max_rows, max_bytes)?;
                target_row_count
            }
            Err(e) => return Err(e),
        };

        let additional_rows = new_row_capacity - self.row_capacity;
        let additional_floats = additional_rows
            .checked_mul(dim_usize)
            .ok_or(ArenaError::CapacityExceeded)?;
        try_reserve_exact(&mut self.vectors, additional_floats, "vectors")?;
        try_reserve_exact(&mut self.ids, additional_rows, "ids")?;
        try_reserve_exact(&mut self.tenant_ids, additional_rows, "tenant_ids")?;
        try_reserve_exact(&mut self.visibilities, additional_rows, "visibilities")?;
        self.row_capacity = new_row_capacity;
        Ok(())
    }

    /// 1 行分を 4 本のバッファへ追加する。呼び出し元が直前に
    /// [`Self::ensure_capacity`] を呼び、十分な capacity を確保済みであること
    /// （＝ここでの追加が再確保を発生させないこと）が前提。
    fn push_row(&mut self, id: u64, embedding: &[f32], tenant_id: String, visibility: Visibility) {
        self.vectors.extend_from_slice(embedding);
        self.ids.push(id);
        self.tenant_ids.push(tenant_id);
        self.visibilities.push(visibility);
    }
}

/// RLS 段（`predicate`）を通過済みの 1 行に対し、SCALAR 段フック（`on_visible_row`）を
/// 呼び、通過した行だけをアリーナへ追加する共通処理（Issue #363。
/// `VectorArena::build_filtered_with_rows_and_limits_in_txn_capturing`（redb 走査・
/// コールドパス）と [`VectorArena::build_from_cached_rls_rows`]（`SqlArenaCache`
/// ヒット時・キャッシュ済み行からの再構築）の両方が本関数を経由することで、
/// スロット番号契約（`on_visible_row` の第 1 引数＝`candidate_columns`/疎 DocId/
/// provider id が 1 対 1 に対応する契約。TABLE-12 対応）・容量検証・push の実装を
/// 1 箇所に集約する（2 経路が個別に実装すると、どちらか一方だけを直してもう一方を
/// 直し忘れる形でスロット番号契約が壊れうるため）。
///
/// `visible_row_count` は呼び出し元が管理する可変カウンタで、次に push される行の
/// スロット番号（＝呼び出しごとに 1 ずつ進む）の源泉。`on_visible_row` が `Ok(false)`
/// を返した行は容量検証・push のいずれの対象にもしない（元実装と同じ契約）。
#[allow(clippy::too_many_arguments)]
fn push_visible_row<G>(
    buffers: &mut GrowableArenaBuffers,
    visible_row_count: &mut usize,
    dim: usize,
    expected_dim: u32,
    max_rows: usize,
    max_bytes: usize,
    id: u64,
    tenant_id: &str,
    visibility: Visibility,
    embedding: &[f32],
    metadata: &[u8],
    on_visible_row: &mut G,
) -> Result<()>
where
    G: FnMut(usize, u64, &[f32], &[u8]) -> std::result::Result<bool, ArenaError>,
{
    if !on_visible_row(*visible_row_count, id, embedding, metadata)? {
        return Ok(());
    }
    // アロケーション前の上限検証（.claude/rules/security.md「不安全な設計｜無制限
    // リソース確保（DoS）」対応）を、行を追加する直前に可視行数基準で行う。
    *visible_row_count = visible_row_count
        .checked_add(1)
        .ok_or(ArenaError::CapacityExceeded)?;
    check_capacity(*visible_row_count, expected_dim, max_rows, max_bytes)?;
    buffers.ensure_capacity(*visible_row_count, dim, expected_dim, max_rows, max_bytes)?;
    buffers.push_row(id, embedding, tenant_id.to_string(), visibility);
    Ok(())
}

/// `sql::exec::execute_statement_with_cache`（Issue #363・`SqlArenaCache` ミス時）が、
/// redb 走査中に採取した RLS 通過行から、キャッシュ用スナップショット材料
/// （RLS-only の [`VectorArena`]＋行ごとの metadata 複製）を組み立てる蓄積器。
///
/// クエリ本体の候補構築（`on_visible_row`・SCALAR 段）とは**別に**、
/// `VectorArena::build_filtered_with_rows_in_txn_capturing` の `rls_capture` から
/// 呼ばれる想定（RLS 段を通過した行を無条件・全件受け取る）。
///
/// 行 metadata の複製バイト量が `metadata_byte_cap` を超えた場合、または
/// [`check_capacity`]・確保自体が失敗した場合は、以降の [`Self::push`] を静かに
/// 無視して [`Self::failed`] を立てるだけで `Err` を伝播しない（キャッシュ採取の
/// 断念であり、クエリ本体の失敗ではないため。呼び出し元がこの状態を見て
/// `SqlArenaCache::insert` 自体を取りやめる）。
pub(crate) struct SqlArenaCaptureBuilder {
    buffers: GrowableArenaBuffers,
    metadata: Vec<Vec<u8>>,
    row_count: usize,
    metadata_bytes: usize,
    metadata_byte_cap: usize,
    max_rows: usize,
    max_bytes: usize,
    dim: usize,
    expected_dim: u32,
    failed: bool,
}

impl SqlArenaCaptureBuilder {
    pub(crate) fn new(
        expected_dim: u32,
        max_rows: usize,
        max_bytes: usize,
        metadata_byte_cap: usize,
    ) -> Self {
        Self {
            buffers: GrowableArenaBuffers::new(),
            metadata: Vec::new(),
            row_count: 0,
            metadata_bytes: 0,
            metadata_byte_cap,
            max_rows,
            max_bytes,
            dim: expected_dim as usize,
            expected_dim,
            failed: false,
        }
    }

    /// 容量超過・確保失敗により、これ以上行を蓄積できなくなったか
    /// （[`Self::finish`] が `None` を返すことが確定した状態か）。`failed` に
    /// 一度でも遷移すると [`Self::push`] は即座に no-op へ縮退するため、以降の
    /// 呼び出しは安価だが、呼び出し元が本メソッドで早期に検知して残り行の
    /// デコード自体を打ち切ることで、無駄なデコード・確保コストを避けられる
    /// （`sql::aggregate::capture_scalar_index_snapshot` が使用。Issue #475
    /// codex-review P2 対応）。
    pub(crate) fn failed(&self) -> bool {
        self.failed
    }

    /// RLS 通過行を 1 件追加する。型ドキュメント参照。
    /// `external_bytes_in_use`（Issue #379 codex-review P1 対応）: 同一クエリで並行
    /// 構築中のクエリ応答用アリーナが既に確保済みの見積もりバイト量（[`RlsCaptureFn`]
    /// ドキュメント参照）。本メソッドは自身の採取済みバイト量（embedding 相当＋
    /// metadata）とこの値を合算した単一のピーク予算で `max_bytes` を判定する。
    /// 採取単体では上限未達でも、応答用アリーナと合算した総量が `max_bytes` を
    /// 超える場合は、このクエリの応答自体は妨げずに採取だけを断念する
    /// （型ドキュメント「行 metadata の複製バイト量が…」参照）。
    pub(crate) fn push(
        &mut self,
        id: u64,
        tenant_id: &str,
        visibility: Visibility,
        embedding: &[f32],
        metadata: &[u8],
        external_bytes_in_use: usize,
    ) {
        if self.failed {
            return;
        }
        let Some(next_count) = self.row_count.checked_add(1) else {
            self.failed = true;
            return;
        };
        if check_capacity(next_count, self.expected_dim, self.max_rows, self.max_bytes).is_err() {
            self.failed = true;
            return;
        }
        let next_metadata_bytes = self.metadata_bytes.saturating_add(metadata.len());
        if next_metadata_bytes > self.metadata_byte_cap {
            self.failed = true;
            return;
        }
        // 単一のピークメモリ予算判定（Issue #379 codex-review P1 対応。上記
        // ドキュメント参照）。自身（採取用アリーナの embedding 相当＋metadata）と
        // `external_bytes_in_use`（応答用アリーナ）を合算した総量で `max_bytes` を
        // 判定することで、選択性の高い `WHERE` で応答用アリーナが小さくても採取用
        // アリーナが単独で `max_bytes` まで膨らむ「実質 3 倍のピークメモリ」を防ぐ。
        let Ok(self_bytes) = estimate_row_bytes(next_count, self.expected_dim) else {
            self.failed = true;
            return;
        };
        let combined_bytes = self_bytes
            .saturating_add(next_metadata_bytes)
            .saturating_add(external_bytes_in_use);
        if combined_bytes > self.max_bytes {
            self.failed = true;
            return;
        }
        if self
            .buffers
            .ensure_capacity(
                next_count,
                self.dim,
                self.expected_dim,
                self.max_rows,
                self.max_bytes,
            )
            .is_err()
        {
            self.failed = true;
            return;
        }
        let mut owned_metadata: Vec<u8> = Vec::new();
        if owned_metadata.try_reserve_exact(metadata.len()).is_err() {
            self.failed = true;
            return;
        }
        owned_metadata.extend_from_slice(metadata);
        // `tenant_id.to_string()`（infallible。確保失敗時 OOM abort）の代わりに
        // `try_reserve_exact` で事前確保してから複製する（Issue #379 codex-review P1
        // 対応: 本型は soft-fail 契約〔型ドキュメント参照〕であり、確保失敗は
        // `failed` へ遷移させるだけでプロセスを止めてはならない）。
        let mut owned_tenant_id = String::new();
        if owned_tenant_id.try_reserve_exact(tenant_id.len()).is_err() {
            self.failed = true;
            return;
        }
        owned_tenant_id.push_str(tenant_id);
        // `self.metadata: Vec<Vec<u8>>`（行ごとの outer コンテナ）自体の成長も
        // `Vec::push` の内部確保（infallible）に任せず、`try_reserve` で事前に
        // 予約してから追加する（同上。`owned_metadata` 自体の確保は既に
        // `try_reserve_exact` 済みだが、それを保持する外側 `Vec` の成長は別途
        // fallible 化が必要）。
        if self.metadata.try_reserve(1).is_err() {
            self.failed = true;
            return;
        }
        self.buffers
            .push_row(id, embedding, owned_tenant_id, visibility);
        self.metadata.push(owned_metadata);
        self.row_count = next_count;
        self.metadata_bytes = next_metadata_bytes;
    }

    /// 蓄積結果を確定する。[`Self::push`] が一度でも断念（[`Self::failed`]）した
    /// 場合は `None`（キャッシュへ登録しない。型ドキュメント参照）。
    pub(crate) fn finish(self, table_name: &str) -> Option<(VectorArena, Vec<Vec<u8>>)> {
        if self.failed {
            return None;
        }
        Some((
            VectorArena {
                table_name: table_name.to_string(),
                dim: self.expected_dim,
                vectors: self.buffers.vectors,
                ids: self.buffers.ids,
                tenant_ids: self.buffers.tenant_ids,
                visibilities: self.buffers.visibilities,
            },
            self.metadata,
        ))
    }
}

/// テーブルスキーマから埋め込み次元を取得し検証する（`0 < dim <= MAX_EMBEDDING_DIM`）。
/// `pub(crate)`: [`VectorArena::build_filtered_with_limits`]・
/// `rls.rs::SearchTimeFilter::build`（TASK-134）が共有する（同じ検証ロジックの重複実装を
/// 避けるため）。呼び出し元が単一の `read_txn` 上でスキーマ取得・行走査を行う契約は
/// 呼び出し元側の責務（本関数はスキーマ取得のみ行う）。
pub(crate) fn validated_vector_dim_in_txn(
    read_txn: &impl crate::storage::read_source::ReadSource,
    table_name: &str,
) -> Result<u32> {
    let schema = catalog::get_table_schema_in_txn(read_txn, table_name)?;
    let dim = schema.vector_dim().ok_or(ArenaError::InvalidDim)?;
    if dim == 0 || dim > crate::storage::MAX_EMBEDDING_DIM {
        return Err(ArenaError::InvalidDim);
    }
    Ok(dim)
}

/// コールドスタート時に一括デコードした連続ベクトルバッファ（対象ビヘイビア: TABLE-8）。
///
/// [`VectorArena::build`] が単一の読み取りスナップショットから構築する。構築後に
/// `Storage` へ加わった変更（他ライタによる書き込みを含む）は反映されない
/// （`redb::ReadTransaction` のスナップショット契約をそのまま引き継ぐ）。
#[derive(Debug)]
pub struct VectorArena {
    /// 構築時に `build` へ渡されたテーブル名。`vectors`/`ids` の全行はこのテーブル専用の
    /// 行テーブル（[`crate::catalog::user_rows_table_name`] が指す
    /// `user_rows/{table_name}`）のみを走査した結果であり、他テーブルの行は保存先が
    /// 分離されているため混入しない（モジュールドキュメントの「テーブル帰属」参照）。
    table_name: String,
    dim: u32,
    /// 行 ID 昇順・row-major の連続バッファ。長さは常に `ids.len() * dim` と一致する。
    vectors: Vec<f32>,
    /// `vectors` の第 i 行（`vectors[i*dim..(i+1)*dim]`）に対応する行 ID。
    ids: Vec<u64>,
    /// 後続の RLS 事前フィルタ（TASK-89/133 系）が redb 再読なしでテナント判定できる
    /// よう同居保持する（ポリシー評価自体は行わない。モジュールドキュメント参照）。
    tenant_ids: Vec<String>,
    visibilities: Vec<Visibility>,
}

impl VectorArena {
    /// `storage` の現時点のスナップショットから、カタログ上のテーブル `table_name`
    /// を対象としたアリーナを構築する公開エントリポイント（対象ビヘイビア: TABLE-8）。
    ///
    /// 呼び出し文脈: 後続の検索カーネル（TASK-133 以降）が、コールドスタート時
    /// （プロセス起動直後・対象テーブルの初回クエリ時等）に本関数を呼び、以降のクエリは
    /// 返された [`VectorArena`] 上のスライスを参照する想定の公開 API である。検索カーネル
    /// 本体（スコアリング・top-k 選定）は本モジュールのスコープ外で、本関数は
    /// 「一度デコードした連続バッファを返す」までを責務とする。
    ///
    /// `expected_dim` はカタログ（[`Storage::get_table_schema`] →
    /// [`crate::catalog::TableSchema::vector_dim`]）から取得し、呼び出し元からは受け取らない
    /// （呼び出し元が渡す次元値をテーブル識別の代用にすると、同一次元の別テーブルが
    /// 混入しても検出できないため）。
    ///
    /// 走査対象は `table_name` に対応する行テーブル
    /// （[`crate::catalog::user_rows_table_name`] が指す `user_rows/{table_name}`。
    /// TASK-146・EXT-1/EXT-2）のみで、他テーブルの行は保存先そのものが分離されている
    /// ため次元一致だけで混入することはない（対象ビヘイビア: TABLE-8）。
    ///
    /// 単一の `storage.db().begin_read()` 上でカタログのスキーマ取得・対象テーブルの行走査を
    /// 行う（[`crate::storage::Storage::scan_page`]・[`crate::storage::Storage::scan_table_page`]
    /// は呼び出しごとに別トランザクションを開くためページ間のスナップショット一貫性がなく、
    /// アリーナ構築には使えない）。アロケーション前に行数・総バイト量の上限を検証してから
    /// 確保する（無制限 `Vec::with_capacity` 禁止。.claude/rules/coding-rust.md）。
    ///
    /// 対象テーブルがカタログに存在しない場合・`VECTOR` 列を持たない場合は
    /// `Err`（テーブル未作成の空アリーナという特別扱いはしない。カタログに登録されて
    /// いて 1 行も書き込まれていない場合のみ空アリーナとして成功する）。次元不一致の
    /// 行を検出した場合はスキップせず `Err(ArenaError::DimMismatch)` を返す
    /// （部分的なアリーナを返さない fail-closed な判断。通常この分岐へは到達しない。
    /// `insert_row_into_table`/`insert_rows_into_table` が挿入時に次元検証済みのため、
    /// 事後にスキーマ・行データが手で書き換えられた場合の防御として残す）。
    pub fn build(storage: &Storage, table_name: &str) -> Result<Self> {
        Self::build_filtered(storage, table_name, |_, _| true)
    }

    /// [`Self::build`] の構築時フィルタ付き版（codex P2・Issue #137 対応）。
    /// `predicate(tenant_id, visibility)` が `false` を返す行は、decode 直後に
    /// 破棄してアリーナへ格納しない（`vectors`/`ids`/`tenant_ids`/`visibilities` の
    /// いずれにも追加しない）。
    ///
    /// 呼び出し文脈: `core.rs::EngineCore::search` が
    /// `|tenant, visibility| ctx.is_visible(tenant, visibility)` をそのまま渡すことで、
    /// `PolicyContext` の下で不可視な行（他テナント行を含む）をそもそもアリーナへ確保
    /// しない「構築時フィルタ」を実現する。以前は [`Self::build`] が対象テーブル全行の
    /// アリーナを構築してから、`core.rs` 側が可視行だけの別バッファへ改めて確保・
    /// 全コピーしていたため、1 検索あたりのピークメモリが最大で 2 倍（全行アリーナ ＋
    /// 可視行コピー）になっていた。`predicate` を構築ループへ渡す設計にすることで、
    /// 可視縮約ビューを単一確保・コピーなしで得られるようにする。
    ///
    /// アロケーション前の上限検証（[`check_capacity`]）は「`predicate` を通過した行
    /// （＝実際にアリーナへ格納する行）」に対して、行を追加するたびに逐次行う
    /// （codex 指摘・Issue #137 対応: 以前はテーブル全行数（`table.len()`）を上限判定に
    /// 使っていたため、対象テナントの可視行が少なくても、他テナントの不可視行を含む
    /// テーブル全体の行数・バイト量が [`MAX_ARENA_ROWS`]・[`MAX_ARENA_TOTAL_BYTES`] を
    /// 超えると検索そのものが `CapacityExceeded` で失敗し、他テナントのデータ量が
    /// 対象テナントの検索可用性へ干渉していた。可視行基準に変えることで、この干渉を
    /// なくす）。事前の全行分一括予約もしない。可視行 1 件を追加するたびに
    /// `Vec::try_reserve`（amortized 成長。`Vec::with_capacity`/`push` の内部確保のように
    /// 失敗時 abort しない）で少しずつ確保する。
    ///
    /// 可視性判定（`predicate` 適用）は、embedding のデコード・次元検証より **前** に行う
    /// （codex P0 指摘・Issue #137 対応）。行ごとにまず
    /// [`crate::storage::decode_row_tenant_and_visibility`] で `tenant_id`・`visibility`
    /// だけを安全に取得し（embedding・metadata へは触れない）、`predicate` が `false`
    /// （＝呼び出し元から不可視）を返した行は、embedding のデコード・次元検証を一切
    /// 行わずスキップする。以前は次元不一致検証を `predicate` 適用より前に行っていたため、
    /// 他テナントの不可視行が破損しているだけで対象テナントの検索全体が失敗していた
    /// （他テナントの状態による可用性干渉であり、`ArenaError::DimMismatch` のエラー経由で
    /// 「他テナントの行が破損している」という存在情報を漏らすことにもなっていた。
    /// security.md「エラー・ログ経由で他テナントのデータ・存在情報を漏らさない」）。
    ///
    /// 可視行（`predicate` が `true` を返した行）は、従来どおり embedding を含む完全
    /// デコード・次元検証を行い、破損・次元不一致は fail-closed に `Err` を返す
    /// （部分的なアリーナを返さない。`Self::build`・従来の全件検証と同じ挙動）。
    ///
    /// 不可視行の破損検出自体は、本関数の責務外（管理・整合性検査など別経路の責務。
    /// spec 側のタスクに対応する場合は当該 TASK-nn を参照。本関数のスコープではない）。
    /// 一方、行バイト列のヘッダ部（バージョン・`tenant_len`・`tenant_id`・`visibility`）
    /// 自体が破損していて可視性を判定できない場合は、その行を「不可視だからスキップ」と
    /// 判断できないため、[`Self::build`] と同じ fail-closed な方針で `Err` を返す
    /// （`decode_row_tenant_and_visibility` のドキュメント参照）。
    pub fn build_filtered<F>(storage: &Storage, table_name: &str, predicate: F) -> Result<Self>
    where
        F: FnMut(&str, Visibility) -> bool,
    {
        Self::build_filtered_with_limits(
            storage,
            table_name,
            predicate,
            MAX_ARENA_ROWS,
            MAX_ARENA_TOTAL_BYTES,
        )
    }

    /// [`Self::build_filtered`] へ、可視行（RLS 述語 `predicate` を通過した行）ごとに
    /// 呼ばれる第 2 段のフック `on_visible_row(id, metadata)` を追加した版（TASK-75、
    /// 対象ビヘイビア: SQL-2）。
    ///
    /// 呼び出し文脈: `sql::exec`（TASK-75）が `WHERE <列> = '<literal>'`
    /// （スカラー等価条件）の事前フィルタをここで適用する。`on_visible_row` が
    /// `Ok(false)` を返した行は、`predicate`（RLS 段）を通過した行と同様に
    /// embedding をデコード済みだが、アリーナへは格納しない（`vectors`/`ids`/
    /// `tenant_ids`/`visibilities` のいずれにも追加せず、アリーナ容量の上限検証
    /// （[`check_capacity`]）の対象からも除外する）。`predicate`（RLS 段）→
    /// `on_visible_row`（SCALAR 段）の評価順序は固定であり、[`Self::build_filtered`]
    /// と同じく可視行だけが `on_visible_row` に到達する（不可視行の metadata は一切
    /// デコードしない。呼び出し元がこの順序を入れ替えることはできない）。
    ///
    /// `on_visible_row` が `Err` を返した場合（例: `row_codec::decode_scalar_columns`
    /// のデコード失敗）は、当該行を黙ってスキップせず fail-closed にアリーナ構築全体を
    /// 拒否する（部分的なアリーナを返さない。[`Self::build_filtered`] の既存契約と同方針）。
    pub fn build_filtered_with_rows<F, G>(
        storage: &Storage,
        table_name: &str,
        predicate: F,
        on_visible_row: G,
    ) -> Result<Self>
    where
        F: FnMut(&str, Visibility) -> bool,
        G: FnMut(usize, u64, &[f32], &[u8]) -> std::result::Result<bool, ArenaError>,
    {
        Self::build_filtered_with_rows_and_limits(
            storage,
            table_name,
            predicate,
            on_visible_row,
            MAX_ARENA_ROWS,
            MAX_ARENA_TOTAL_BYTES,
        )
    }

    /// [`Self::build_filtered`] の上限値パラメータ化版。実装は本関数に集約し、
    /// [`Self::build_filtered`] は本番用の定数（[`MAX_ARENA_ROWS`]・
    /// [`MAX_ARENA_TOTAL_BYTES`]）で呼び出すだけの薄いラッパーにする。
    ///
    /// `max_rows`・`max_bytes` を引数化しているのは、[`check_capacity`] と同じ理由
    /// （上記 [`check_capacity`] のドキュメント参照）で、境界値検証を
    /// 本ファイル内の `#[cfg(test)]` モジュールから小さい上限値で再現するため
    /// （本番の 1,000,000 行・1 GiB 相当のデータセットをテストごとに用意するのは
    /// 非現実的）。「他テナントの不可視行が大量にあっても対象テナントの可視行が
    /// 上限内なら検索が失敗しないこと」を検証するテスト
    /// （`build_filtered_capacity_check_is_based_on_visible_rows_not_total_table_rows`）
    /// が、小さい `max_rows` を指定してこの構造を直接検証する。
    fn build_filtered_with_limits<F>(
        storage: &Storage,
        table_name: &str,
        predicate: F,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<Self>
    where
        F: FnMut(&str, Visibility) -> bool,
    {
        Self::build_filtered_with_rows_and_limits(
            storage,
            table_name,
            predicate,
            |_slot, _id, _embedding, _metadata| Ok(true),
            max_rows,
            max_bytes,
        )
    }

    /// [`Self::build_filtered_with_rows_and_limits_in_txn`] を `storage` から新規に
    /// 開いた `read_txn` 上で呼ぶ薄いラッパー（[`Self::build_filtered_with_rows`]・
    /// [`Self::build_filtered_with_limits`] が共有する）。スキーマ取得・行走査を
    /// 単一の `read_txn`（同一スナップショット）上で行う（別トランザクションに
    /// 分かれていると、スキーマ取得後・走査前に対象テーブルへの並行書き込みが
    /// 挟まってもスナップショットの一貫性が保証できない）。
    fn build_filtered_with_rows_and_limits<F, G>(
        storage: &Storage,
        table_name: &str,
        predicate: F,
        on_visible_row: G,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<Self>
    where
        F: FnMut(&str, Visibility) -> bool,
        G: FnMut(usize, u64, &[f32], &[u8]) -> std::result::Result<bool, ArenaError>,
    {
        let read_txn = storage.db().begin_read().map_err(StorageError::from)?;
        Self::build_filtered_with_rows_and_limits_in_txn(
            &read_txn,
            table_name,
            predicate,
            on_visible_row,
            max_rows,
            max_bytes,
        )
    }

    /// [`Self::build_filtered_with_rows`] の、呼び出し元が管理する既存 `read_txn` を
    /// 受け取る版（TASK-75、対象ビヘイビア: SQL-2。Issue #56 レビュー指摘対応・codex P1）。
    ///
    /// 呼び出し文脈: `sql::exec::execute_statement_in_txn` が、`core.rs::EngineCore::
    /// execute_sql` で `catalog::get_table_schema_in_txn` によりスキーマを取得したのと
    /// **同一の** `read_txn` をここへ渡す。これにより「`sql::parser::bind` が束縛した
    /// スキーマ」と「本関数が走査する行データのスキーマ世代」が常に同一スナップショット
    /// 由来になることを型で強制する（呼び出し元が別トランザクションを渡す・スキーマだけ
    /// 別経路で取得するミスを、シグネチャ上で構造的に防ぐ）。[`Self::build_filtered_with_rows`]
    /// （`Storage` から新規に `read_txn` を開く版）と異なり、本関数はトランザクションを
    /// 開始しない。
    pub(crate) fn build_filtered_with_rows_in_txn<F, G>(
        read_txn: &impl crate::storage::read_source::ReadSource,
        table_name: &str,
        predicate: F,
        on_visible_row: G,
    ) -> Result<Self>
    where
        F: FnMut(&str, Visibility) -> bool,
        G: FnMut(usize, u64, &[f32], &[u8]) -> std::result::Result<bool, ArenaError>,
    {
        Self::build_filtered_with_rows_and_limits_in_txn(
            read_txn,
            table_name,
            predicate,
            on_visible_row,
            MAX_ARENA_ROWS,
            MAX_ARENA_TOTAL_BYTES,
        )
    }

    /// [`Self::build_filtered_with_rows_in_txn`] の、RLS 通過行の
    /// `(id, tenant_id, visibility, embedding, metadata)` を追加採取する版（Issue #363・
    /// `sql::exec::execute_statement_with_cache` の `SqlArenaCache` ミス時専用）。
    /// `rls_capture` は `on_visible_row`（SCALAR 段）の判定結果に関係なく、RLS 段を
    /// 通過した行ごとに必ず 1 回呼ばれる（[`Self::build_filtered_with_rows_and_limits_in_txn_capturing`]
    /// のドキュメント参照）。
    pub(crate) fn build_filtered_with_rows_in_txn_capturing<F, G>(
        read_txn: &impl crate::storage::read_source::ReadSource,
        table_name: &str,
        predicate: F,
        on_visible_row: G,
        rls_capture: &mut RlsCaptureFn<'_>,
    ) -> Result<Self>
    where
        F: FnMut(&str, Visibility) -> bool,
        G: FnMut(usize, u64, &[f32], &[u8]) -> std::result::Result<bool, ArenaError>,
    {
        Self::build_filtered_with_rows_and_limits_in_txn_capturing(
            read_txn,
            table_name,
            predicate,
            on_visible_row,
            MAX_ARENA_ROWS,
            MAX_ARENA_TOTAL_BYTES,
            Some(rls_capture),
        )
    }

    /// `SqlArenaCache` ヒット時、redb を再走査せずキャッシュ済みの RLS 通過行から
    /// アリーナを構築する（Issue #363）。`rows` は
    /// [`Self::build_filtered_with_rows_in_txn_capturing`] の `rls_capture` で採取した
    /// 行のみを渡すこと（本関数は RLS 述語を一切評価しない。呼び出し元
    /// `sql::exec::execute_statement_with_cache` が、キャッシュ構築時に使った
    /// `PolicyContext` と本クエリの `PolicyContext` の完全一致をキャッシュ照合
    /// （`SqlArenaCache::lookup`）で保証している契約に依存する。security.md P0
    /// 「テナント分離の検査を外す/緩める/バイパス経路を作らない」: 本関数自体は
    /// 検査を行わないため、呼び出し元のキー完全一致照合が唯一の防御線になる）。
    /// 行単位の処理（SCALAR 段フック呼び出し・容量検証・push）は
    /// [`push_visible_row`] を共有し、redb コールドパスと同一の挙動を保証する。
    ///
    /// `source_arena`（RLS 段のみを適用して構築済みの、キャッシュ由来のアリーナ）と
    /// `metadata`（`source_arena` とスロット添字が 1 対 1 に対応する行 metadata の
    /// 複製。[`crate::sql::arena_cache::SqlArenaSnapshot`] が保持する）の行数が一致しない場合は
    /// 呼び出し元の不変条件違反として fail-closed に `Err` を返す（`[]` での無検査
    /// 添字アクセスを避け、`get`/インデックスアクセサ経由で取得する。
    /// .claude/rules/coding-rust.md 方針）。
    pub(crate) fn build_from_cached_rls_rows<G>(
        table_name: &str,
        expected_dim: u32,
        source_arena: &VectorArena,
        metadata: &[Vec<u8>],
        mut on_visible_row: G,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<Self>
    where
        G: FnMut(usize, u64, &[f32], &[u8]) -> std::result::Result<bool, ArenaError>,
    {
        if source_arena.len() != metadata.len() {
            return Err(ArenaError::InvalidInput(
                "cached RLS snapshot row/metadata count mismatch".to_string(),
            ));
        }
        let dim = expected_dim as usize;
        let mut buffers = GrowableArenaBuffers::new();
        let mut visible_row_count: usize = 0;
        let out_of_bounds =
            || ArenaError::InvalidInput("cached RLS snapshot row index out of bounds".to_string());
        for (i, meta) in metadata.iter().enumerate() {
            let id = *source_arena.ids().get(i).ok_or_else(out_of_bounds)?;
            let tenant_id = source_arena.tenant_id(i).ok_or_else(out_of_bounds)?;
            let visibility = source_arena.visibility(i).ok_or_else(out_of_bounds)?;
            let embedding = source_arena.vector(i).ok_or_else(out_of_bounds)?;
            push_visible_row(
                &mut buffers,
                &mut visible_row_count,
                dim,
                expected_dim,
                max_rows,
                max_bytes,
                id,
                tenant_id,
                visibility,
                embedding,
                meta,
                &mut on_visible_row,
            )?;
        }
        Ok(VectorArena {
            table_name: table_name.to_string(),
            dim: expected_dim,
            vectors: buffers.vectors,
            ids: buffers.ids,
            tenant_ids: buffers.tenant_ids,
            visibilities: buffers.visibilities,
        })
    }

    /// [`Self::build_from_cached_rls_rows`] の部分集合版（Issue #474）。
    /// `sql::scalar_index::ScalarIndex::resolve_candidates` が絞った候補スロット
    /// （`source_slots`。`source_arena`／`metadata` に対する添字）だけを昇順に
    /// 辿り、`push_visible_row` で `on_visible_row`（`matches_all`＋式述語）を
    /// 適用する。索引は候補行を「絞る」ことしかできず「通す」ことはできない
    /// ため、`on_visible_row` を省略せず候補ごとに引き続き呼ぶ——出力
    /// アリーナ（行・順序・スロット番号）は `source_slots` が全スロット
    /// （`0..metadata.len()`）の場合、[`Self::build_from_cached_rls_rows`] と
    /// ビット同一になる（[`crate::sql::exec`] の `on_visible_row` に渡る
    /// `slot` 引数は入力スロットではなく出力側の連番であり、両関数とも
    /// [`push_visible_row`] が管理する同じカウンタから払い出すため）。
    ///
    /// `source_slots` は**重複のない狭義昇順**でなければならない
    /// （`ScalarIndex::candidates_for`／`candidates_id_range`／
    /// `resolve_candidates` の交差はいずれも昇順・重複なしの `Vec<u32>` を
    /// 返す契約。この前提が崩れた場合、行の重複計上・黙った脱落につながる
    /// ため `InvalidInput` で fail-closed に拒否する。呼び出し規約違反への
    /// 多層防御であり、正常系のオーバーヘッドにはならない）。範囲外スロット・
    /// `metadata.len()` との不一致も同様に拒否する。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn build_from_cached_rls_rows_subset<G>(
        table_name: &str,
        expected_dim: u32,
        source_arena: &VectorArena,
        metadata: &[Vec<u8>],
        source_slots: &[u32],
        mut on_visible_row: G,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<Self>
    where
        G: FnMut(usize, u64, &[f32], &[u8]) -> std::result::Result<bool, ArenaError>,
    {
        if source_arena.len() != metadata.len() {
            return Err(ArenaError::InvalidInput(
                "cached RLS snapshot row/metadata count mismatch".to_string(),
            ));
        }
        let dim = expected_dim as usize;
        let mut buffers = GrowableArenaBuffers::new();
        let mut visible_row_count: usize = 0;
        let out_of_bounds =
            || ArenaError::InvalidInput("cached RLS snapshot row index out of bounds".to_string());
        let mut prev_slot: Option<u32> = None;
        for &slot in source_slots {
            if let Some(prev) = prev_slot {
                if slot <= prev {
                    return Err(ArenaError::InvalidInput(
                        "scalar index candidate slots must be strictly ascending and unique"
                            .to_string(),
                    ));
                }
            }
            prev_slot = Some(slot);
            let i = usize::try_from(slot).map_err(|_| out_of_bounds())?;
            let meta = metadata.get(i).ok_or_else(out_of_bounds)?;
            let id = *source_arena.ids().get(i).ok_or_else(out_of_bounds)?;
            let tenant_id = source_arena.tenant_id(i).ok_or_else(out_of_bounds)?;
            let visibility = source_arena.visibility(i).ok_or_else(out_of_bounds)?;
            let embedding = source_arena.vector(i).ok_or_else(out_of_bounds)?;
            push_visible_row(
                &mut buffers,
                &mut visible_row_count,
                dim,
                expected_dim,
                max_rows,
                max_bytes,
                id,
                tenant_id,
                visibility,
                embedding,
                meta,
                &mut on_visible_row,
            )?;
        }
        Ok(VectorArena {
            table_name: table_name.to_string(),
            dim: expected_dim,
            vectors: buffers.vectors,
            ids: buffers.ids,
            tenant_ids: buffers.tenant_ids,
            visibilities: buffers.visibilities,
        })
    }

    /// [`Self::build_from_cached_rls_rows_subset`] の**複製しない**版（Issue #654）。
    ///
    /// 新規 `VectorArena` を構築せず、`source_arena`（キャッシュ済みスナップショットの
    /// `VectorArena`。呼び出し元が借用したまま保持し続ける）に対して
    /// [`Self::build_from_cached_rls_rows_subset`] と全く同じ検証・`on_visible_row`
    /// 適用を行ったうえで、通過した行の**元スロット番号**（`source_arena`／
    /// `metadata` に対する添字）だけを狭義昇順の `Vec<u32>` として返す。
    ///
    /// `on_visible_row` の第 1 引数（出力側の連番）は
    /// [`Self::build_from_cached_rls_rows_subset`] と同一の意味・同一の値になる
    /// （両関数とも [`push_visible_row`] が管理する同じカウンタから払い出すため。
    /// `sql::exec` の `candidate_columns`（出力側連番で添字づけられる）契約は
    /// そのまま満たされる）。索引は候補行を「絞る」ことしかできず「通す」ことは
    /// できないため、`on_visible_row` を省略せず候補ごとに引き続き呼ぶ。
    ///
    /// 返す `Vec<u32>` は「`source_arena`／`metadata` の添字のうち、`on_visible_row`
    /// を通過したものだけを昇順に残した部分列」であり、`source_slots` が全スロット
    /// （`0..metadata.len()`）の場合は「可視かつ `on_visible_row` を通過した行の
    /// スロット番号を昇順に列挙したもの」になる。呼び出し元（`sql::exec`）はこの
    /// 戻り値を [`crate::kernel::SubsetSearchInput::slots`] へそのまま渡し、
    /// `source_arena.vectors()`／`source_arena.ids()`／`source_arena.tenant_id()` を
    /// 直接借用したまま探索する（新規アリーナへの embedding・id・tenant_id
    /// （`String` 複製）が発生しない）。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn filter_cached_rls_rows_subset<G>(
        expected_dim: u32,
        source_arena: &VectorArena,
        metadata: &[Vec<u8>],
        source_slots: &[u32],
        mut on_visible_row: G,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<Vec<u32>>
    where
        G: FnMut(usize, u64, &[f32], &[u8]) -> std::result::Result<bool, ArenaError>,
    {
        if source_arena.len() != metadata.len() {
            return Err(ArenaError::InvalidInput(
                "cached RLS snapshot row/metadata count mismatch".to_string(),
            ));
        }
        let out_of_bounds =
            || ArenaError::InvalidInput("cached RLS snapshot row index out of bounds".to_string());
        let mut kept: Vec<u32> = Vec::new();
        let mut visible_row_count: usize = 0;
        let mut prev_slot: Option<u32> = None;
        for &slot in source_slots {
            if let Some(prev) = prev_slot {
                if slot <= prev {
                    return Err(ArenaError::InvalidInput(
                        "scalar index candidate slots must be strictly ascending and unique"
                            .to_string(),
                    ));
                }
            }
            prev_slot = Some(slot);
            let i = usize::try_from(slot).map_err(|_| out_of_bounds())?;
            let meta = metadata.get(i).ok_or_else(out_of_bounds)?;
            let id = *source_arena.ids().get(i).ok_or_else(out_of_bounds)?;
            let embedding = source_arena.vector(i).ok_or_else(out_of_bounds)?;
            if !on_visible_row(visible_row_count, id, embedding, meta)? {
                continue;
            }
            // `push_visible_row` が行う容量検証（アロケーション前の上限チェック）を、
            // 実際のバッファ確保は伴わない本関数でも同じ契約で踏襲する
            // （新規アリーナを作らないため `GrowableArenaBuffers::ensure_capacity` は
            // 呼ばないが、「可視行数が `max_rows`／`max_bytes` を超えたら拒否する」
            // という security.md「不安全な設計｜無制限リソース確保（DoS）」対応の
            // 契約自体は維持する。`kept: Vec<u32>` 自体の確保量も間接的にこれで
            // 有界になる）。
            visible_row_count = visible_row_count
                .checked_add(1)
                .ok_or(ArenaError::CapacityExceeded)?;
            check_capacity(visible_row_count, expected_dim, max_rows, max_bytes)?;
            if kept.len() == kept.capacity() {
                kept.try_reserve(1).map_err(|e| {
                    ArenaError::AllocationFailed(format!(
                        "failed to reserve mask-filtered candidate slots: {e}"
                    ))
                })?;
            }
            kept.push(slot);
        }
        Ok(kept)
    }

    /// [`Self::build_filtered_with_limits`] の行フック付き版。呼び出し元が管理する
    /// `read_txn` 上で実行する実装本体（[`Self::build_filtered_with_rows_and_limits`]・
    /// `sql::exec::execute_statement_in_txn`（TASK-75・Issue #56 レビュー指摘対応）が
    /// 共有する）。
    ///
    /// スキーマ取得（[`validated_vector_dim_in_txn`]）・対象テーブルの行走査を、
    /// 呼び出し元から渡された**同一** `read_txn`（同一スナップショット）上で行う。
    /// 呼び出し元がスキーマを別トランザクションで取得してから本関数へ渡した場合、
    /// スキーマ取得後・走査前に対象テーブルへの並行 DDL（`alter_table_add_column` 等）が
    /// 挟まるとスナップショットの一貫性が保証できない（codex P1 指摘・Issue #56:
    /// `sql::parser::bind` が束縛したスキーマ世代と、本関数が走査する行データの
    /// スキーマ世代が食い違い、新設列の欠落や `row_codec` デコード失敗を招き得た）。
    /// 本関数自身はどのトランザクションでスキーマを取得したかを検証しない
    /// （契約は「呼び出し元が bind に使ったスキーマと同一の `read_txn` を渡すこと」。
    /// `core.rs::EngineCore::execute_sql` のドキュメント参照）。
    fn build_filtered_with_rows_and_limits_in_txn<F, G>(
        read_txn: &impl crate::storage::read_source::ReadSource,
        table_name: &str,
        predicate: F,
        on_visible_row: G,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<Self>
    where
        F: FnMut(&str, Visibility) -> bool,
        G: FnMut(usize, u64, &[f32], &[u8]) -> std::result::Result<bool, ArenaError>,
    {
        Self::build_filtered_with_rows_and_limits_in_txn_capturing(
            read_txn,
            table_name,
            predicate,
            on_visible_row,
            max_rows,
            max_bytes,
            None,
        )
    }

    /// [`Self::build_filtered_with_rows_and_limits_in_txn`] の本体。加えて
    /// `rls_capture` が `Some` の場合、RLS 段（`predicate`）を通過した行ごとに
    /// `(id, tenant_id, visibility, embedding, metadata)` を、`on_visible_row`
    /// （SCALAR 段）の判定結果に関係なく必ず 1 回渡す。
    ///
    /// 呼び出し文脈: `sql::exec::execute_statement_with_cache`（Issue #363・
    /// `SqlArenaCache` ミス時）が、クエリ実行そのものの候補構築（`on_visible_row`
    /// による SCALAR 事前フィルタ・hybrid 疎コーパス蓄積等）と**同一の redb 走査**
    /// の中で、次回以降のクエリが再利用できる「RLS 通過行全体」のスナップショット
    /// （`(id, embedding, metadata)` の複製）を同時に採取するために追加した。
    /// RLS 判定・redb 走査はそれぞれ 1 回で済み、キャッシュ採取のための追加走査は
    /// 発生しない。`rls_capture` が `Err` を返した場合はアリーナ構築全体を
    /// fail-closed に拒否する（`on_visible_row` と同じ契約）。
    fn build_filtered_with_rows_and_limits_in_txn_capturing<F, G>(
        read_txn: &impl crate::storage::read_source::ReadSource,
        table_name: &str,
        mut predicate: F,
        mut on_visible_row: G,
        max_rows: usize,
        max_bytes: usize,
        mut rls_capture: Option<&mut RlsCaptureFn<'_>>,
    ) -> Result<Self>
    where
        F: FnMut(&str, Visibility) -> bool,
        G: FnMut(usize, u64, &[f32], &[u8]) -> std::result::Result<bool, ArenaError>,
    {
        let expected_dim = validated_vector_dim_in_txn(read_txn, table_name)?;

        // 対象テーブル専用の行テーブル（`user_rows/{table_name}`）だけを開く。
        // `catalog::user_rows_table_name` は識別子検証を行わないが、直前の
        // `get_table_schema_in_txn` がカタログ照会の前段で `validate_identifier` を
        // 通しているため、ここで改めて検証する必要はない。
        let row_table_name = catalog::user_rows_table_name(table_name);
        // 物理キーは `(tenant_id, id)`（対象ビヘイビア: TABLE-12。定義は
        // `catalog::user_rows_table_def`）。全件走査は従来どおりテーブル全行を列挙し
        // （テナントで絞らない）、可視性判定は `predicate`（呼び出し元の
        // `PolicyContext::is_visible`）へ委譲する契約を変えない。
        let table = match read_txn.open_table(catalog::user_rows_table_def(&row_table_name)) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Ok(VectorArena {
                    table_name: table_name.to_string(),
                    dim: expected_dim,
                    vectors: Vec::new(),
                    ids: Vec::new(),
                    tenant_ids: Vec::new(),
                    visibilities: Vec::new(),
                });
            }
            // 旧フォーマット（キーが `id` のみ）の DB は
            // `catalog::map_row_table_error` 経由で fail-closed に拒否する（TABLE-12）。
            Err(e) => return Err(ArenaError::Catalog(catalog::map_row_table_error(e))),
        };

        // `table.len()` は redb 側の集計値（テーブル全行数。不可視行を含む）で、
        // アロケーション上限検証には使わない（上記ドキュメント参照）。走査自体には
        // 使わないため取得しない。
        let dim = expected_dim as usize;
        let mut buffers = GrowableArenaBuffers::new();
        // `predicate` を通過した（＝実際に格納する）行数。上限検証はこの値に対して行う。
        let mut visible_row_count: usize = 0;
        // 可視行ごとの embedding デコード先スクラッチバッファ（Issue #314・SQL-1・
        // TASK-83 条件7）。`decode_row`（`Vec::with_capacity` を行ごとに新規確保）の
        // 代わりに `storage::decode_row_embedding_and_metadata_into` で本バッファへ
        // 書き込み、`buffers.push_row` が `extend_from_slice` で平坦バッファへコピー
        // した直後に次の行へ再利用する。同一テーブルは全行同一次元
        // （`validated_vector_dim_in_txn` で確認済み）のため、初回のみ確保が発生し
        // 2 行目以降は再確保なしで使い回せる。
        let mut embedding_scratch: Vec<f32> = Vec::new();

        for entry in table.iter().map_err(StorageError::from)? {
            let (k, v) = entry.map_err(StorageError::from)?;
            // 複合キーの第 2 要素が行 `id`（第 1 要素 `key_tenant` は物理キー側の
            // テナント。可視性判定自体は従来どおり行データ側のヘッダ由来の値で
            // 行うが、可視と判定した行は下で `key_tenant` とヘッダ側 `tenant_id` の
            // 整合を検査する）。
            let (key_tenant, id) = k.value();
            let buf = v.value();

            // まず tenant_id・visibility だけを安全に取得する（embedding・metadata の
            // デコードは行わない）。ヘッダ部自体が壊れていて可視性を判定できない場合は
            // 「不可視だからスキップ」と判断できないため fail-closed に伝播する
            // （`decode_row_tenant_and_visibility` のドキュメント参照。codex P0 対応・
            // Issue #137）。
            // `tenant_id` は `buf` を借用した `&str`（ヒープアロケーションなし。Issue #174）。
            let (tenant_id, visibility) =
                crate::storage::decode_row_tenant_and_visibility(buf).map_err(ArenaError::from)?;

            // 構築時フィルタ（codex P0/P2 対応）: `predicate` が `false` を返す行（呼び出し元
            // から不可視）は、embedding のデコード・次元検証を一切行わずここで打ち切る。
            // 不可視行の embedding が破損していても、対象テナントの検索を失敗させない
            // （他テナントの状態による可用性干渉・エラー経由の存在情報漏えいを避ける）。
            // 不可視行の破損検出自体は本関数の責務外（別経路の責務。上記ドキュメント参照）。
            if !predicate(tenant_id, visibility) {
                continue;
            }

            // 可視行・常に: 複合キー側の `key_tenant` とヘッダ側 `tenant_id` の
            // 不一致（内部バグ・raw redb 書き込みによる異常）を fail-closed に拒否する
            // （対象ビヘイビア: TABLE-12。`sql/aggregate.rs` の走査ループと同一パターン。
            // PR #563 codex-review P0 対応: この検査を欠いたまま
            // `sql::exec::decode_deferred_scalars_from_redb` の遅延投影再取得が
            // `arena.tenant_id(slot)`（ヘッダ側）をキーに `(tenant, id)` で
            // 再取得すると、物理キーとヘッダの tenant が不一致に壊れた行では
            // 再取得後の `verify_row_key_tenant` がヘッダ側 tenant 同士の一致
            // としてすり抜け、本来無関係な別テナントの物理行を re-fetch して
            // しまう恐れがあった。ここで不一致行をアリーナへ一切格納しないことで、
            // アリーナに載る全スロットは物理キー・ヘッダ双方の tenant が一致
            // することを不変条件として保証し、後続の再取得キーが指す物理行は
            // 常に走査時と同一であることを保証する）。
            crate::storage::verify_row_key_tenant(key_tenant, tenant_id)
                .map_err(ArenaError::from)?;

            // ここに到達するのは可視行だけ。以降は embedding を含む完全デコードを行い、
            // 次元不一致・デコードエラーは従来どおり fail-closed に伝播する
            // （部分的なアリーナを返さない）。行ごとに新しい `Vec<f32>` を確保する
            // `decode_row` の代わりに、ループ外で確保した `embedding_scratch` へ
            // 書き込む借用デコード API を使う（上記コメント参照。Issue #314）。
            let (found_dim, metadata) =
                crate::storage::decode_row_embedding_and_metadata_into(buf, &mut embedding_scratch)
                    .map_err(ArenaError::from)?;
            if found_dim != expected_dim {
                return Err(ArenaError::DimMismatch {
                    id,
                    expected: expected_dim,
                    found: found_dim,
                });
            }

            // `rls_capture`（Issue #363・`SqlArenaCache` ミス時のみ `Some`）: RLS 段を
            // 通過した行を、直後の SCALAR 段（`on_visible_row`）の判定結果に関係なく
            // 無条件に 1 回だけ通知する。同一 redb 走査の中で「RLS 通過行全体」の
            // スナップショットを採取するための唯一のフック地点（上記関数ドキュメント
            // 参照）。
            if let Some(capture) = rls_capture.as_deref_mut() {
                // クエリ応答用アリーナ（`buffers`）がこの時点までに確保済みの
                // 見積もりバイト量。`visible_row_count`（SCALAR 段を通過し既に
                // `buffers` へ push 済みの行数）を [`estimate_row_bytes`] と同じ式に
                // 通す（Issue #379 codex-review P1 対応。[`RlsCaptureFn`] ドキュメント
                // 参照）。見積もり自体が失敗する（オーバーフロー等）場合は
                // fail-closed に `max_bytes` そのものを渡し、採取側の予算を実質 0 へ
                // 倒す（採取を諦めるだけでクエリ応答自体は継続する）。
                let response_arena_bytes_in_use =
                    estimate_row_bytes(visible_row_count, expected_dim).unwrap_or(max_bytes);
                capture(
                    id,
                    tenant_id,
                    visibility,
                    &embedding_scratch,
                    metadata,
                    response_arena_bytes_in_use,
                )?;
            }

            // SCALAR 段（TASK-75）: RLS 段（`predicate`）を通過した行の metadata に対し
            // 呼び出し元のスカラー等価条件を適用する。`false` はアリーナへ格納せず
            // スキップし（容量検証の対象にも含めない）、`Err` は fail-closed に
            // 全体を拒否する（[`Self::build_filtered_with_rows`] のドキュメント参照）。
            // 第 1 引数はこの行がアリーナへ格納される場合のスロット番号
            // （`VectorArena::ids()`/`vectors()`/`tenant_ids()` の添字）。呼び出し元
            // （`sql/exec.rs`）が候補行の付随データ（スカラー列・疎コーパス）を
            // 行 `id` ではなくスロット番号で対応づけられるようにするための契約
            // （対象ビヘイビア: TABLE-12。行 `id` の一意性スコープがテナント内に
            // 閉じたため、`id` は 1 つの可視集合内で行を一意に指せない。スロット番号は
            // `(tenant_id, id)` の行と 1 対 1 に対応する）。呼び出し元側で
            // 別カウンタを持たせない（本ループの push 条件とドリフトさせない）。
            // TASK-79（SQL-9）: `embedding_scratch` はこの時点で既にデコード済み
            // （直前の `decode_row_embedding_and_metadata_into` 呼び出し）のため、
            // 追加コピーなしでフックへ渡せる。呼び出し元（`sql::exec.rs` の
            // `on_visible_row`）は `WHERE` の式述語（宣言的 UDF・組み込み関数呼び出し）
            // を SCALAR 段の一部として評価するために使う。
            push_visible_row(
                &mut buffers,
                &mut visible_row_count,
                dim,
                expected_dim,
                max_rows,
                max_bytes,
                id,
                tenant_id,
                visibility,
                &embedding_scratch,
                metadata,
                &mut on_visible_row,
            )?;
        }

        Ok(VectorArena {
            table_name: table_name.to_string(),
            dim: expected_dim,
            vectors: buffers.vectors,
            ids: buffers.ids,
            tenant_ids: buffers.tenant_ids,
            visibilities: buffers.visibilities,
        })
    }

    /// 構築時に `build` へ渡されたテーブル名（全行がこのテーブルに帰属することを
    /// 保証する。上記フィールドのドキュメント参照）。
    pub fn table_name(&self) -> &str {
        &self.table_name
    }

    /// 埋め込みの次元数。
    pub fn dim(&self) -> u32 {
        self.dim
    }

    /// 保持している行数。
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// 行を 1 件も保持していないか。
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// 行 ID の一覧（構築時のスキャン順＝行 ID 昇順）。
    pub fn ids(&self) -> &[u64] {
        &self.ids
    }

    /// row-major の連続ベクトルバッファ全体（長さ = `len() * dim()`）。
    pub fn vectors(&self) -> &[f32] {
        &self.vectors
    }

    /// `index` 番目の行の埋め込みスライスを返す。範囲外は `None`
    /// （`.claude/rules/coding-rust.md`: 添字アクセス `[]` を production コードで使わない）。
    pub fn vector(&self, index: usize) -> Option<&[f32]> {
        let dim = self.dim as usize;
        let start = index.checked_mul(dim)?;
        let end = start.checked_add(dim)?;
        self.vectors.get(start..end)
    }

    /// `index` 番目の行のテナント識別子。範囲外は `None`。
    pub fn tenant_id(&self, index: usize) -> Option<&str> {
        self.tenant_ids.get(index).map(String::as_str)
    }

    /// `index` 番目の行の可視性ラベル。範囲外は `None`。
    pub fn visibility(&self, index: usize) -> Option<Visibility> {
        self.visibilities.get(index).copied()
    }

    /// このアリーナが保持するバッファの実確保量ベースの概算ヒープ使用量バイト数
    /// （`vectors`・`ids`・`visibilities`・`tenant_ids` は要素数ではなく
    /// `Vec::capacity()` を使う。`len()` ベースだと amortized 成長で確保済みの
    /// 未使用 capacity 分を見逃し、総量上限の判定が実際の確保量を過小評価する
    /// ため（codex-review P1 対応・PR #191）。`tenant_ids: Vec<String>` は
    /// 各要素 `String` の内部確保分（`capacity()`）に加え、`Vec<String>` 自体の
    /// backing allocation（`String` 構造体 1 個分のサイズ × capacity）も計上する。
    /// 構造体自体のスタックサイズ・アロケータのオーバーヘッド（malloc ヘッダ等）は
    /// 含まない大まかな目安（=保守的な上振れ推定であり、下振れはしない）。
    ///
    /// 呼び出し文脈: `core.rs::PrefilterCache`（TASK-133 後続。RLS-1〜4）がキャッシュに
    /// 常駐させる `PrefilterSnapshot`（内部に本アリーナを保持）の総量上限判定に使う。
    /// オーバーフローで panic しないよう `checked_mul`/`saturating_add` のみで計算する
    /// （.claude/rules/coding-rust.md: 整数演算は checked/saturating を使う）。
    pub(crate) fn approx_heap_bytes(&self) -> usize {
        let vectors_bytes = self
            .vectors
            .capacity()
            .saturating_mul(std::mem::size_of::<f32>());
        let ids_bytes = self
            .ids
            .capacity()
            .saturating_mul(std::mem::size_of::<u64>());
        let visibilities_bytes = self
            .visibilities
            .capacity()
            .saturating_mul(std::mem::size_of::<Visibility>());
        // 各 String の内部確保分（未使用 capacity 込み）。
        let tenant_strings_bytes = self
            .tenant_ids
            .iter()
            .map(|s| s.capacity())
            .fold(0usize, |acc, n| acc.saturating_add(n));
        // `Vec<String>` 自体の backing allocation（`String` の構造体サイズ ×
        // capacity）。未使用 capacity 分も含めて計上する。
        let tenant_vec_bytes = self
            .tenant_ids
            .capacity()
            .saturating_mul(std::mem::size_of::<String>());
        let tenant_ids_bytes = tenant_strings_bytes.saturating_add(tenant_vec_bytes);
        vectors_bytes
            .saturating_add(ids_bytes)
            .saturating_add(visibilities_bytes)
            .saturating_add(tenant_ids_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::RowInput;

    // `build_from_cached_rls_rows_subset`（Issue #474）: 候補が全スロット
    // （`0..metadata.len()`）のとき、`build_from_cached_rls_rows`（全走査）と
    // ビット同一の出力アリーナを構築することを固定する（モジュール
    // ドキュメント「出力アリーナは…ビット同一になる」の直接検証）。
    #[test]
    fn build_from_cached_rls_rows_subset_with_all_slots_matches_full_scan() {
        let mut capture = SqlArenaCaptureBuilder::new(2, 100, usize::MAX, usize::MAX);
        capture.push(1, "tenant-a", Visibility::Public, &[1.0, 0.0], b"a", 0);
        capture.push(2, "tenant-a", Visibility::Public, &[0.0, 1.0], b"b", 0);
        capture.push(3, "tenant-a", Visibility::Public, &[1.0, 1.0], b"c", 0);
        let (source_arena, metadata) = capture.finish("docs").expect("within budget");

        let full = VectorArena::build_from_cached_rls_rows(
            "docs",
            2,
            &source_arena,
            &metadata,
            |_, _, _, _| Ok(true),
            100,
            usize::MAX,
        )
        .expect("full scan build");
        let all_slots: Vec<u32> = (0..metadata.len() as u32).collect();
        let subset = VectorArena::build_from_cached_rls_rows_subset(
            "docs",
            2,
            &source_arena,
            &metadata,
            &all_slots,
            |_, _, _, _| Ok(true),
            100,
            usize::MAX,
        )
        .expect("subset build with all slots");
        assert_eq!(full.ids(), subset.ids());
        assert_eq!(full.len(), subset.len());
        for i in 0..full.len() {
            assert_eq!(full.vector(i), subset.vector(i));
            assert_eq!(full.tenant_id(i), subset.tenant_id(i));
        }
    }

    // 部分集合（候補スロットが一部のみ）の場合、`build_from_cached_rls_rows_subset`
    // の結果が「全走査してから候補以外の行を on_visible_row で false にした
    // 場合」と一致することを固定する（索引が行を「絞る」ことしかできないという
    // fail-closed 契約の直接検証）。
    #[test]
    fn build_from_cached_rls_rows_subset_matches_full_scan_filtered_to_candidates() {
        let mut capture = SqlArenaCaptureBuilder::new(2, 100, usize::MAX, usize::MAX);
        capture.push(1, "tenant-a", Visibility::Public, &[1.0, 0.0], b"a", 0);
        capture.push(2, "tenant-a", Visibility::Public, &[0.0, 1.0], b"b", 0);
        capture.push(3, "tenant-a", Visibility::Public, &[1.0, 1.0], b"c", 0);
        let (source_arena, metadata) = capture.finish("docs").expect("within budget");

        // 候補はスロット 0・2（id=1, id=3）のみ。
        let candidate_slots: Vec<u32> = vec![0, 2];
        let subset = VectorArena::build_from_cached_rls_rows_subset(
            "docs",
            2,
            &source_arena,
            &metadata,
            &candidate_slots,
            |_, _, _, _| Ok(true),
            100,
            usize::MAX,
        )
        .expect("subset build");
        let candidates: std::collections::HashSet<u32> = candidate_slots.iter().copied().collect();
        let mut seen = 0usize;
        let full_filtered = VectorArena::build_from_cached_rls_rows(
            "docs",
            2,
            &source_arena,
            &metadata,
            |_slot, _id, _emb, _meta| {
                let ok = candidates.contains(&(seen as u32));
                seen += 1;
                Ok(ok)
            },
            100,
            usize::MAX,
        )
        .expect("full scan filtered build");
        assert_eq!(subset.ids(), full_filtered.ids());
        assert_eq!(subset.ids(), &[1, 3]);
    }

    // 非昇順・重複スロットは `InvalidInput` で拒否する（呼び出し規約違反への
    // 多層防御。モジュールドキュメント参照）。
    #[test]
    fn build_from_cached_rls_rows_subset_rejects_non_ascending_slots() {
        let mut capture = SqlArenaCaptureBuilder::new(2, 100, usize::MAX, usize::MAX);
        capture.push(1, "tenant-a", Visibility::Public, &[1.0, 0.0], b"a", 0);
        capture.push(2, "tenant-a", Visibility::Public, &[0.0, 1.0], b"b", 0);
        let (source_arena, metadata) = capture.finish("docs").expect("within budget");

        let bad_slots: Vec<u32> = vec![1, 0];
        let err = VectorArena::build_from_cached_rls_rows_subset(
            "docs",
            2,
            &source_arena,
            &metadata,
            &bad_slots,
            |_, _, _, _| Ok(true),
            100,
            usize::MAX,
        )
        .expect_err("non-ascending slots must be rejected");
        assert!(matches!(err, ArenaError::InvalidInput(_)));
    }

    // Issue #654: `filter_cached_rls_rows_subset` が返す「保持スロット」を
    // `source_arena` から手で拾い直した embedding/id/tenant_id が、複製版
    // `build_from_cached_rls_rows_subset` の出力アリーナとビット同一になること
    // （「行を複製しても複製しなくても結果は同じ」という要件 2 の直接検証）。
    #[test]
    fn filter_cached_rls_rows_subset_kept_slots_match_build_from_cached_rls_rows_subset() {
        let mut capture = SqlArenaCaptureBuilder::new(2, 100, usize::MAX, usize::MAX);
        capture.push(1, "tenant-a", Visibility::Public, &[1.0, 0.0], b"a", 0);
        capture.push(2, "tenant-a", Visibility::Public, &[0.0, 1.0], b"b", 0);
        capture.push(3, "tenant-a", Visibility::Public, &[1.0, 1.0], b"c", 0);
        capture.push(4, "tenant-a", Visibility::Public, &[2.0, 2.0], b"d", 0);
        let (source_arena, metadata) = capture.finish("docs").expect("within budget");

        // 候補はスロット 0・1・3（id=1, id=2, id=4）。
        let candidate_slots: Vec<u32> = vec![0, 1, 3];
        let built = VectorArena::build_from_cached_rls_rows_subset(
            "docs",
            2,
            &source_arena,
            &metadata,
            &candidate_slots,
            |_, _, _, _| Ok(true),
            100,
            usize::MAX,
        )
        .expect("subset build");
        let kept = VectorArena::filter_cached_rls_rows_subset(
            2,
            &source_arena,
            &metadata,
            &candidate_slots,
            |_, _, _, _| Ok(true),
            100,
            usize::MAX,
        )
        .expect("filter subset");

        assert_eq!(kept, candidate_slots);
        assert_eq!(built.len(), kept.len());
        for (i, &slot) in kept.iter().enumerate() {
            assert_eq!(
                built.vector(i),
                source_arena.vector(slot as usize),
                "vector at output slot {i} (source slot {slot})"
            );
            assert_eq!(built.ids().get(i), source_arena.ids().get(slot as usize));
            assert_eq!(built.tenant_id(i), source_arena.tenant_id(slot as usize));
        }
    }

    // `on_visible_row` が一部の候補を `false` で落とす場合、`filter_cached_rls_rows_subset`
    // が返す `Vec<u32>` は「通過した候補だけの部分列」になり、`on_visible_row` へ渡る
    // 第 1 引数（出力側連番）は複製版と同一の値になること（`push_visible_row` と
    // 同じカウンタ規約を独立実装せず踏襲していることの検証）。
    #[test]
    fn filter_cached_rls_rows_subset_second_arg_is_output_side_ordinal_and_skips_rejected_rows() {
        let mut capture = SqlArenaCaptureBuilder::new(2, 100, usize::MAX, usize::MAX);
        capture.push(10, "tenant-a", Visibility::Public, &[1.0, 0.0], b"a", 0);
        capture.push(20, "tenant-a", Visibility::Public, &[0.0, 1.0], b"b", 0);
        capture.push(30, "tenant-a", Visibility::Public, &[1.0, 1.0], b"c", 0);
        let (source_arena, metadata) = capture.finish("docs").expect("within budget");

        let candidate_slots: Vec<u32> = vec![0, 1, 2];
        let mut observed_ordinals = Vec::new();
        let kept = VectorArena::filter_cached_rls_rows_subset(
            2,
            &source_arena,
            &metadata,
            &candidate_slots,
            |ordinal, id, _emb, _meta| {
                observed_ordinals.push(ordinal);
                // id=20（スロット 1）だけを落とす。
                Ok(id != 20)
            },
            100,
            usize::MAX,
        )
        .expect("filter subset");

        // `on_visible_row` は各候補につき必ず 1 回呼ばれる（拒否された行も含む）。
        // 引数の値は「呼び出し時点での可視カウンタ」であり、通過した行（id=10,
        // id=30）だけがカウンタを進める: id=10 → 0（→1 へ進む）、id=20（拒否）→
        // 1（進まない）、id=30 → 1（→2 へ進む）。
        assert_eq!(observed_ordinals, vec![0, 1, 1]);
        assert_eq!(kept, vec![0, 2]);
    }

    // 非昇順・重複スロットは `filter_cached_rls_rows_subset` でも `InvalidInput` で
    // 拒否する（複製版と同じ呼び出し規約）。
    #[test]
    fn filter_cached_rls_rows_subset_rejects_non_ascending_slots() {
        let mut capture = SqlArenaCaptureBuilder::new(2, 100, usize::MAX, usize::MAX);
        capture.push(1, "tenant-a", Visibility::Public, &[1.0, 0.0], b"a", 0);
        capture.push(2, "tenant-a", Visibility::Public, &[0.0, 1.0], b"b", 0);
        let (source_arena, metadata) = capture.finish("docs").expect("within budget");

        let bad_slots: Vec<u32> = vec![0, 0];
        let err = VectorArena::filter_cached_rls_rows_subset(
            2,
            &source_arena,
            &metadata,
            &bad_slots,
            |_, _, _, _| Ok(true),
            100,
            usize::MAX,
        )
        .expect_err("duplicate slots must be rejected");
        assert!(matches!(err, ArenaError::InvalidInput(_)));
    }

    // `SqlArenaCaptureBuilder`（Issue #363）: metadata バイト上限超過時は `push` を
    // 静かに無視して `failed()` を立てるだけで `finish()` が `None` を返すことを
    // 確認する（型ドキュメント「容量超過・確保失敗により…」参照）。この経路は
    // `sql::exec::execute_statement_with_cache` にとって「キャッシュ登録を諦めるが
    // クエリ応答は妨げない」唯一の分岐であり、ここが誤って `panic`／`Err` へ
    // 変わるとキャッシュ容量超過だけでクエリ全体が失敗する fail-open ならぬ
    // 過剰拒否のリグレッションになるため固定する。
    #[test]
    fn sql_arena_capture_builder_soft_fails_when_metadata_byte_cap_is_exceeded() {
        let mut capture =
            SqlArenaCaptureBuilder::new(2, 100, usize::MAX, /* metadata_byte_cap */ 4);
        capture.push(1, "tenant-a", Visibility::Public, &[1.0, 0.0], b"ab", 0);
        assert!(!capture.failed(), "first push stays within the 4-byte cap");
        capture.push(2, "tenant-a", Visibility::Public, &[0.0, 1.0], b"cdef", 0);
        assert!(
            capture.failed(),
            "second push exceeds the 4-byte cap (2 + 4 > 4) and must soft-fail"
        );
        // 断念後の追加 `push` も静かに無視され続ける（`failed` のまま）。
        capture.push(3, "tenant-a", Visibility::Public, &[1.0, 1.0], b"x", 0);
        assert!(capture.failed());
        assert!(
            capture.finish("docs").is_none(),
            "a failed capture must never be registered into the cache"
        );
    }

    #[test]
    fn sql_arena_capture_builder_finishes_within_budget() {
        let mut capture = SqlArenaCaptureBuilder::new(2, 100, usize::MAX, 1024);
        capture.push(1, "tenant-a", Visibility::Public, &[1.0, 0.0], b"a", 0);
        capture.push(2, "tenant-a", Visibility::Public, &[0.0, 1.0], b"b", 0);
        assert!(!capture.failed());
        let (arena, metadata) = capture.finish("docs").expect("within budget must finish");
        assert_eq!(arena.ids(), &[1, 2]);
        assert_eq!(metadata, vec![b"a".to_vec(), b"b".to_vec()]);
    }

    // `SqlArenaCaptureBuilder::push` の `external_bytes_in_use`（Issue #379
    // codex-review P1 対応）: 採取用アリーナ自身は `max_bytes` 未満でも、呼び出し元
    // （クエリ応答用アリーナ）が既に確保済みの見積もりバイト量と合算した総量が
    // `max_bytes` を超えれば採取を断念する（単一のピークメモリ予算。型・`push` の
    // ドキュメント参照）。応答用アリーナと採取用アリーナがそれぞれ独立に `max_bytes`
    // まで確保できてしまうリグレッション（1 クエリのピークメモリが実質数倍化する
    // DoS 増幅）を固定する。
    #[test]
    fn sql_arena_capture_builder_soft_fails_when_combined_with_response_arena_exceeds_budget() {
        // 1 行あたりの見積もりバイト量（`estimate_row_bytes`）が `max_bytes` の
        // ちょうど半分になるよう `dim` を選ぶ。採取用アリーナ単体では 2 行目まで
        // 収まるが、`external_bytes_in_use`（応答用アリーナが既に使用中の量）を
        // 合算すると 1 行目の時点で超過する設定にする。
        let dim: u32 = 4;
        let one_row_bytes =
            estimate_row_bytes(1, dim).expect("single row byte estimate must not overflow");
        let max_bytes = one_row_bytes * 4;
        let mut capture = SqlArenaCaptureBuilder::new(dim, 100, max_bytes, usize::MAX);
        // 応答用アリーナが既に `max_bytes` の半分を超えて使用中と仮定する。
        let external_bytes_in_use = max_bytes / 2 + 1;
        capture.push(
            1,
            "tenant-a",
            Visibility::Public,
            &[1.0, 2.0, 3.0, 4.0],
            b"m",
            external_bytes_in_use,
        );
        assert!(
            !capture.failed(),
            "single row combined with external usage must still fit within max_bytes"
        );
        capture.push(
            2,
            "tenant-a",
            Visibility::Public,
            &[5.0, 6.0, 7.0, 8.0],
            b"m",
            external_bytes_in_use,
        );
        assert!(
            capture.failed(),
            "second row pushes the combined (self + external) total past max_bytes even \
             though the capture builder alone would still be within its own row/byte limits"
        );
        assert!(
            capture.finish("docs").is_none(),
            "a failed capture must never be registered into the cache"
        );
    }

    #[test]
    fn capacity_check_accepts_at_row_limit() {
        assert_eq!(
            check_capacity(100, 4, 100, usize::MAX).expect("within row limit"),
            400
        );
    }

    #[test]
    fn capacity_check_rejects_over_row_limit() {
        assert!(matches!(
            check_capacity(101, 4, 100, usize::MAX),
            Err(ArenaError::CapacityExceeded)
        ));
    }

    #[test]
    fn capacity_check_accepts_at_byte_limit() {
        // row_count * dim * 4（vectors）+ row_count * per_row_aux_bytes（ids・tenant_ids・
        // visibilities の見積もり）== max_bytes ちょうど。
        let row_count = 10usize;
        let dim = 4u32;
        let aux = per_row_aux_bytes().expect("aux bytes computation must not overflow in test");
        let max_bytes = row_count * (dim as usize) * 4 + row_count * aux;
        assert_eq!(
            check_capacity(row_count, dim, usize::MAX, max_bytes).expect("within byte limit"),
            40
        );
    }

    #[test]
    fn capacity_check_rejects_over_byte_limit() {
        let row_count = 10usize;
        let dim = 4u32;
        let aux = per_row_aux_bytes().expect("aux bytes computation must not overflow in test");
        let max_bytes = row_count * (dim as usize) * 4 + row_count * aux;
        assert!(matches!(
            check_capacity(row_count, dim, usize::MAX, max_bytes - 1),
            Err(ArenaError::CapacityExceeded)
        ));
    }

    // 対象ビヘイビア: TABLE-8（codex P1 対応）。`vectors`（f32 埋め込みバッファ）の
    // バイト量だけでは上限に収まっていても、`ids`・`tenant_ids`・`visibilities` の
    // 見積もりバイト量を合算すると上限を超える場合に拒否すること
    // （codex 指摘: 一部バッファのみを上限検証の対象にすると、他バッファの確保が
    // 検証をすり抜けて OOM abort を招き得るため）。
    #[test]
    fn capacity_check_rejects_when_aux_buffers_push_total_over_limit() {
        let row_count = 10usize;
        let dim = 4u32;
        let vectors_bytes = row_count * (dim as usize) * 4;
        // vectors だけなら上限内だが、aux バッファ込みでは超過する上限値。
        let max_bytes = vectors_bytes;
        assert!(matches!(
            check_capacity(row_count, dim, usize::MAX, max_bytes),
            Err(ArenaError::CapacityExceeded)
        ));
    }

    #[test]
    fn capacity_check_does_not_overflow_on_huge_dim() {
        // usize::MAX 近傍の dim を渡しても checked_mul が Err に落ちるだけで panic しない。
        let result = check_capacity(usize::MAX / 2, u32::MAX, usize::MAX, usize::MAX);
        assert!(matches!(result, Err(ArenaError::CapacityExceeded)));
    }

    // 対象ビヘイビア: security.md「不安全な設計｜無制限リソース確保（DoS）」
    // （codex-review P1 対応・PR #191）。`approx_heap_bytes` は `Vec::len()` ではなく
    // `Vec::capacity()` ベースで課金しなければならない。amortized 成長で確保済みの
    // 未使用 capacity（`len() < capacity()`）を意図的に作り、len ベースの旧計算より
    // 大きい値が返ることを確認する（旧実装は未使用 capacity を無視し、実確保量を
    // 過小評価していた＝本テストは退行防止）。
    #[test]
    fn approx_heap_bytes_charges_unused_vec_capacity_not_just_len() {
        let mut vectors = Vec::with_capacity(64);
        vectors.extend(std::iter::repeat_n(1.0f32, 4));
        let mut ids = Vec::with_capacity(64);
        ids.push(1u64);
        let mut visibilities = Vec::with_capacity(64);
        visibilities.push(Visibility::Public);
        let mut tenant_ids: Vec<String> = Vec::with_capacity(64);
        let mut s = String::with_capacity(256);
        s.push_str("tenant-a");
        tenant_ids.push(s);

        let arena = VectorArena {
            table_name: "t".to_string(),
            dim: 4,
            vectors,
            ids,
            tenant_ids,
            visibilities,
        };

        // len ベースだった旧実装が返していたはずの値（要素数のみ）。
        let len_based = arena.vectors.len() * std::mem::size_of::<f32>()
            + arena.ids.len() * std::mem::size_of::<u64>()
            + arena.visibilities.len() * std::mem::size_of::<Visibility>()
            + arena.tenant_ids.iter().map(String::capacity).sum::<usize>();

        let actual = arena.approx_heap_bytes();
        assert!(
            actual > len_based,
            "capacity-based estimate must charge unused Vec/String capacity, \
             actual={actual} len_based={len_based}"
        );
        // Vec<String> 自体の backing allocation（String 構造体サイズ × capacity）も
        // 計上されていること。
        assert!(actual >= arena.tenant_ids.capacity() * std::mem::size_of::<String>());
    }

    // 対象ビヘイビア: TABLE-8（codex P1 対応）。`check_capacity` の上限検証を素通りする
    // ほど巨大な予約要求（`isize::MAX` バイト超）に対して、`try_reserve_exact` が
    // `Vec::with_capacity`/`Vec::push` のように abort せず `Err(ArenaError::AllocationFailed)`
    // を返すことを検証する。`isize::MAX` 超のレイアウトは Rust のアロケーション API 契約上
    // 実メモリを確保しようとする前に即座に拒否されるため、CI 環境で実際に大量のメモリを
    // 消費せず決定的に再現できる。
    #[test]
    fn try_reserve_exact_converts_oversized_request_to_allocation_failed_without_aborting() {
        let mut buf: Vec<u8> = Vec::new();
        let oversized = (isize::MAX as usize).saturating_add(1);
        let result = try_reserve_exact(&mut buf, oversized, "test buffer");
        assert!(matches!(result, Err(ArenaError::AllocationFailed(_))));
    }

    // 一時 DB パス払い出し（`unique_db_path` / `CleanupGuard`）は Issue #173 で
    // `crate::test_util::temp_db` へ一本化した（旧: このモジュール内の複製）。
    // `VectorArena::build`・`catalog::get_table_schema_in_txn` が `pub(crate)` のため、
    // 統合テストからは呼べずこのモジュール内 unit test でのみ検証できる点は変わらない。
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

    // スキーマ取得と行走査の TOCTOU への回帰テスト（対象ビヘイビア: TABLE-8）。
    // 対象テーブル `a` についてスキーマを取得した read_txn を保持したまま、*その後*に
    // 別テーブル `b` を作成・行を書き込んでも、同一 read_txn 上で見える対象テーブル `a`
    // の行テーブル内容は書き込み前のスナップショットのまま（`b` への並行書き込みが
    // `a` の走査結果へ影響しないこと）を検証する。
    #[test]
    fn get_table_schema_in_txn_observes_a_single_snapshot_across_concurrent_writes() {
        use crate::catalog::{self, ColumnDef, ColumnType, TableSchema};
        use crate::storage::{RowInput, Visibility};
        // `table.len()`（本テスト専用の検証用途）にのみ必要なトレイト。`build_filtered`
        // はもうテーブル全行数を使わないため、モジュール先頭の `use` からは外した
        // （codex 指摘・Issue #137 対応。上記 `check_capacity` のドキュメント参照）。
        use redb::ReadableTableMetadata;

        let path = unique_db_path("toctou");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        let schema_a = TableSchema::new(
            "a",
            vec![ColumnDef::new("embedding", ColumnType::Vector(4), false)],
        );
        storage.create_table(&schema_a).expect("create table a");
        storage
            .insert_row_into_table(
                "a",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Private,
                    embedding: &[0.0, 1.0, 2.0, 3.0],
                    metadata: &[],
                },
            )
            .expect("insert row into a");

        // スキーマ取得・行テーブルオープン用のスナップショットを先に確立する。
        let read_txn = storage.db().begin_read().expect("begin_read");
        let schema = catalog::get_table_schema_in_txn(&read_txn, "a").expect("get schema for a");
        assert_eq!(schema.name, "a");

        // read_txn 確立後に別テーブル・同次元の行を並行挿入する（TOCTOU 再現）。
        let schema_b = TableSchema::new(
            "b",
            vec![ColumnDef::new("embedding", ColumnType::Vector(4), false)],
        );
        storage.create_table(&schema_b).expect("create table b");
        storage
            .insert_row_into_table(
                "b",
                2,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Private,
                    embedding: &[9.0, 9.0, 9.0, 9.0],
                    metadata: &[],
                },
            )
            .expect("insert row into b");

        // 同一 read_txn 上では、後発の書き込みは一切見えない。対象テーブル `a` の
        // 行テーブルは read_txn 確立時点のスナップショットのまま 1 行だけを保持する。
        let row_table_name = catalog::user_rows_table_name("a");
        let table = read_txn
            .open_table(catalog::user_rows_table_def(&row_table_name))
            .expect("open row table for a");
        assert_eq!(table.len().expect("table len"), 1);

        drop(read_txn);
    }

    // 対象ビヘイビア: SQL-2（Issue #56 レビュー指摘対応・codex P1）。
    // `core.rs::EngineCore::execute_sql` は `catalog::get_table_schema_in_txn` で
    // スキーマを取得した `read_txn` を、そのまま `VectorArena::build_filtered_with_rows_in_txn`
    // へ渡す契約になった。本テストは、その契約どおり同一 `read_txn` を渡す限り、
    // スキーマ取得後に並行 `alter_table_add_column` がコミットされても、行走査が
    // 取得済みスキーマと矛盾する新スナップショット（新設列を含む行）を観測しないこと
    // （＝新設列の欠落や `row_codec` デコード失敗を招かないこと）を検証する。
    #[test]
    fn build_filtered_with_rows_in_txn_stays_consistent_with_schema_across_concurrent_alter_table()
    {
        use crate::catalog::{self, ColumnDef, ColumnType, TableSchema};
        use crate::row_codec;
        use crate::storage::{RowInput, Visibility};

        let path = unique_db_path("alter-table-toctou");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(4), false),
                ColumnDef::new("body", ColumnType::Text, true),
            ],
        );
        storage.create_table(&schema).expect("create table");
        storage
            .insert_row_into_table(
                "docs",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Private,
                    embedding: &[0.0, 1.0, 2.0, 3.0],
                    metadata: &row_codec::encode_scalar_columns(
                        &schema,
                        &[
                            row_codec::Value::Null,
                            row_codec::Value::Text("seed".to_string()),
                        ],
                    )
                    .expect("encode metadata for seed row"),
                },
            )
            .expect("insert seed row");

        // `core.rs::EngineCore::execute_sql` の契約どおり、スキーマ取得と行走査に使う
        // `read_txn` をここで一本化する（`bind` 相当のスキーマ取得）。
        let read_txn = storage.db().begin_read().expect("begin_read");
        let bound_schema = catalog::get_table_schema_in_txn(&read_txn, "docs").expect("get schema");
        assert_eq!(bound_schema.columns.len(), 2);

        // `read_txn` 確立後に列追加 DDL がコミットされ、さらに新スキーマ前提の行が
        // 挿入される（レビュー指摘が懸念した並行 DDL の再現）。
        storage
            .alter_table_add_column("docs", ColumnDef::new("tag", ColumnType::Text, true))
            .expect("alter table add column");
        let new_schema = storage.get_table_schema("docs").expect("get new schema");
        assert_eq!(new_schema.columns.len(), 3);
        storage
            .insert_row_into_table(
                "docs",
                2,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Private,
                    embedding: &[4.0, 5.0, 6.0, 7.0],
                    metadata: &row_codec::encode_scalar_columns(
                        &new_schema,
                        &[
                            row_codec::Value::Null,
                            row_codec::Value::Text("post-alter".to_string()),
                            row_codec::Value::Text("new-tag".to_string()),
                        ],
                    )
                    .expect("encode metadata for post-alter row"),
                },
            )
            .expect("insert post-alter row");

        // 取得済み `bound_schema`（2 列）で `read_txn` 上の行走査・デコードを行う。
        // `read_txn` 確立前の 1 行のみが観測され、そのデコードは `bound_schema` と
        // 矛盾なく成功すること（新設列を含む行が混入したり、デコード失敗
        // （XX000 相当）が起きたりしないこと）を検証する。
        let mut decoded_bodies: Vec<Option<String>> = Vec::new();
        let arena = VectorArena::build_filtered_with_rows_in_txn(
            &read_txn,
            "docs",
            |_tenant, _visibility| true,
            |_slot, _id, _embedding, metadata| {
                let decoded = row_codec::decode_scalar_columns(&bound_schema, metadata)
                    .map_err(|e| ArenaError::Storage(StorageError::Codec(e.to_string())))?;
                decoded_bodies.push(match decoded.get(1) {
                    Some(row_codec::Value::Text(t)) => Some(t.clone()),
                    _ => None,
                });
                Ok(true)
            },
        )
        .expect("build_filtered_with_rows_in_txn must observe a schema-consistent snapshot");

        assert_eq!(arena.ids(), &[1]);
        assert_eq!(decoded_bodies, vec![Some("seed".to_string())]);

        drop(read_txn);
    }

    // 以下は旧 `tests/arena.rs`・`tests/arena_perf.rs`（統合テスト）からの移設分。
    // `VectorArena::build` が `pub(crate)`（テーブルスコープの内部 API に依存する）
    // ため、統合テストからは呼べず、クレート内の `#[cfg(test)]` モジュールへ
    // 移設したまま保持している。

    const TENANT_ID: &str = "tenant-a";

    /// 外部クレート非依存の決定的擬似乱数生成器（xorshift32）。テストデータ生成にのみ使う。
    struct Xorshift32(u32);

    impl Xorshift32 {
        fn next_u32(&mut self) -> u32 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            self.0 = x;
            x
        }

        fn next_f32(&mut self) -> f32 {
            (self.next_u32() as f64 / u32::MAX as f64) as f32
        }
    }

    fn make_embedding(rng: &mut Xorshift32, dim: usize) -> Vec<f32> {
        (0..dim).map(|_| rng.next_f32()).collect()
    }

    /// `table_name` の schema を組み立てる（`embedding` 列 1 本のみを持つ最小構成。
    /// `multi_dim_tables.rs` と同方針）。
    fn schema_for(table_name: &str, dim: u32) -> crate::catalog::TableSchema {
        use crate::catalog::{ColumnDef, ColumnType};
        crate::catalog::TableSchema::new(
            table_name,
            vec![ColumnDef::new("embedding", ColumnType::Vector(dim), false)],
        )
    }

    // 対象ビヘイビア: TABLE-8。複数行を投入して build した結果が、行数・次元・各行の
    // 内容とも Storage::get の読み直し結果と一致し、連続バッファの長さが len * dim と
    // 一致すること（コールドスタート・アリーナの基本契約）を検証する。
    #[test]
    fn build_produces_contiguous_arena_matching_storage_rows() {
        let path = unique_db_path("basic");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        let dim: usize = 8;
        storage
            .create_table(&schema_for("docs", dim as u32))
            .expect("create_table");

        let mut rng = Xorshift32(0x1234_5678);
        let embeddings: Vec<Vec<f32>> = (0..10).map(|_| make_embedding(&mut rng, dim)).collect();
        let rows: Vec<(u64, RowInput<'_>)> = embeddings
            .iter()
            .enumerate()
            .map(|(i, emb)| {
                (
                    i as u64,
                    RowInput {
                        tenant_id: TENANT_ID,
                        visibility: if i % 2 == 0 {
                            Visibility::Public
                        } else {
                            Visibility::Private
                        },
                        embedding: emb,
                        metadata: b"m",
                    },
                )
            })
            .collect();
        storage
            .insert_rows_into_table("docs", &rows)
            .expect("seed rows");

        let arena = VectorArena::build(&storage, "docs").expect("build arena");
        assert_eq!(arena.table_name(), "docs");
        assert_eq!(arena.dim(), dim as u32);
        assert_eq!(arena.len(), 10);
        assert!(!arena.is_empty());
        assert_eq!(arena.vectors().len(), 10 * dim);
        assert_eq!(arena.ids(), &(0u64..10).collect::<Vec<_>>()[..]);

        for i in 0..10usize {
            let expected_row = storage
                .get_row_from_table("docs", TENANT_ID, i as u64)
                .expect("read row back via storage");
            assert_eq!(arena.vector(i), Some(expected_row.embedding.as_slice()));
            assert_eq!(arena.tenant_id(i), Some(expected_row.tenant_id.as_str()));
            assert_eq!(arena.visibility(i), Some(expected_row.visibility));
        }

        // 範囲外は panic せず None を返す。
        assert_eq!(arena.vector(10), None);
        assert_eq!(arena.tenant_id(10), None);
        assert_eq!(arena.visibility(10), None);
    }

    // 対象ビヘイビア: TABLE-8。カタログに登録済みだが 1 行も書き込んでいないテーブル
    // （`user_rows/{table_name}` 未作成）は空アリーナとして成功すること。
    #[test]
    fn build_on_empty_table_returns_empty_arena() {
        let path = unique_db_path("empty");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&schema_for("docs", 16))
            .expect("create_table");

        let arena = VectorArena::build(&storage, "docs").expect("build arena on empty table");
        assert_eq!(arena.dim(), 16);
        assert_eq!(arena.len(), 0);
        assert!(arena.is_empty());
        assert!(arena.vectors().is_empty());
        assert!(arena.ids().is_empty());
    }

    // 対象ビヘイビア: TABLE-8。次元不一致の行が 1 行でも混在していれば、部分的な
    // アリーナを返さず Err(DimMismatch) で fail-closed に拒否すること
    // （黙殺スキップは検索結果の欠落＝fail-open に相当するため禁止）。
    #[test]
    fn build_rejects_dimension_mismatch_without_partial_result() {
        let path = unique_db_path("dim-mismatch");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&schema_for("docs", 4))
            .expect("create_table");

        storage
            .insert_row_into_table(
                "docs",
                0,
                &RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding: &[1.0, 2.0, 3.0, 4.0],
                    metadata: b"m",
                },
            )
            .expect("seed matching-dim row");
        // `insert_row_into_table` は挿入時点で次元検証するため、次元不一致行を通常経路で
        // 作れない。`build` 側の防御（事後にスキーマ・行データが書き換えられた場合）を
        // 検証するため、ここでは検証を経由しない生の write トランザクションで直接
        // 対象テーブルの行テーブルへ次元不一致の行を書き込む。
        {
            let write_txn = storage.db().begin_write().expect("begin_write");
            {
                let row_table_name = catalog::user_rows_table_name("docs");
                let mut row_table = write_txn
                    .open_table(catalog::user_rows_table_def(&row_table_name))
                    .expect("open row table");
                let encoded = crate::storage::encode_row(&RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding: &[1.0, 2.0],
                    metadata: b"m",
                })
                .expect("encode mismatched-dim row");
                // 物理キーは `(tenant_id, id)`（TABLE-12）。
                row_table
                    .insert((TENANT_ID, 1u64), encoded.as_slice())
                    .expect("insert mismatched-dim row bypassing dim validation");
            }
            write_txn.commit().expect("commit mismatched-dim row");
        }

        let err = VectorArena::build(&storage, "docs").expect_err("dim mismatch must be rejected");
        match err {
            ArenaError::DimMismatch {
                id,
                expected,
                found,
            } => {
                assert_eq!(id, 1);
                assert_eq!(expected, 4);
                assert_eq!(found, 2);
            }
            other => panic!("expected DimMismatch, got {other:?}"),
        }
    }

    // 対象ビヘイビア: TABLE-8。対象テーブルがカタログに存在しない場合、および
    // `VECTOR` 列を持たない場合は `Err(InvalidDim)` で拒否すること。
    #[test]
    fn build_rejects_missing_table_and_table_without_vector_column() {
        use crate::catalog::{ColumnDef, ColumnType, TableSchema};

        let path = unique_db_path("invalid-dim");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        // カタログに未登録のテーブル名。
        assert!(VectorArena::build(&storage, "not_registered").is_err());

        // VECTOR 列を持たないテーブル。
        let text_only = TableSchema::new(
            "notes",
            vec![ColumnDef::new("body", ColumnType::Text, false)],
        );
        storage.create_table(&text_only).expect("create_table");
        assert!(matches!(
            VectorArena::build(&storage, "notes"),
            Err(ArenaError::InvalidDim)
        ));
    }

    // 対象ビヘイビア: TABLE-8（codex P1 対応）。複数テーブルが共存する状態で、
    // 対象テーブル以外に書き込まれた行（次元が一致する行を含む）が混入しないこと、
    // かつ対象テーブルの行はすべて取得できることを検証する。行はテーブルごとに
    // 分離された動的 redb テーブル（`user_rows/{table_name}`）へ永続化されるため、
    // 次元一致のみを根拠とした混入は起こり得ない。
    #[test]
    fn build_scopes_arena_to_the_requested_table_only() {
        let path = unique_db_path("multi-table");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        storage
            .create_table(&schema_for("docs_a", 4))
            .expect("create_table docs_a");
        storage
            .create_table(&schema_for("docs_b", 4))
            .expect("create_table docs_b");

        storage
            .insert_row_into_table(
                "docs_a",
                0,
                &RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding: &[1.0, 2.0, 3.0, 4.0],
                    metadata: b"table=docs_a",
                },
            )
            .expect("seed docs_a row");
        storage
            .insert_row_into_table(
                "docs_b",
                0,
                &RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding: &[9.0, 9.0, 9.0, 9.0],
                    metadata: b"table=docs_b",
                },
            )
            .expect("seed docs_b row");
        // docs_b にはさらに、docs_a に存在しない ID の行も追加しておく（行数だけで
        // 混入の有無を誤判定しないようにするため）。
        storage
            .insert_row_into_table(
                "docs_b",
                1,
                &RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding: &[8.0, 8.0, 8.0, 8.0],
                    metadata: b"table=docs_b",
                },
            )
            .expect("seed second docs_b row");

        let arena_a = VectorArena::build(&storage, "docs_a").expect("build arena for docs_a");
        assert_eq!(arena_a.len(), 1);
        assert_eq!(arena_a.ids(), &[0u64]);
        assert_eq!(arena_a.vector(0), Some([1.0, 2.0, 3.0, 4.0].as_slice()));

        let arena_b = VectorArena::build(&storage, "docs_b").expect("build arena for docs_b");
        assert_eq!(arena_b.len(), 2);
        assert_eq!(arena_b.ids(), &[0u64, 1u64]);
        assert_eq!(arena_b.vector(0), Some([9.0, 9.0, 9.0, 9.0].as_slice()));
        assert_eq!(arena_b.vector(1), Some([8.0, 8.0, 8.0, 8.0].as_slice()));
    }

    // 対象ビヘイビア: TABLE-8（codex P2 対応・Issue #137）。`build_filtered` の
    // `predicate` が `false` を返す行は、そもそもアリーナへ格納されない（`ids`・
    // `vectors`・`tenant_ids`・`visibilities` のいずれにも現れない）ことを検証する。
    // `build`（`predicate` 常に `true`）と同じデータセットに対して行うことで、
    // 構築時フィルタが「後から除外する」のではなく「最初から格納しない」ことを
    // 行数・内容の両面で確認する。
    #[test]
    fn build_filtered_excludes_rows_failing_the_predicate_at_construction_time() {
        let path = unique_db_path("filtered-excludes");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create_table");

        storage
            .insert_row_into_table(
                "docs",
                0,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: b"m",
                },
            )
            .expect("seed tenant-a row");
        storage
            .insert_row_into_table(
                "docs",
                1,
                &RowInput {
                    tenant_id: "tenant-b",
                    visibility: Visibility::Public,
                    embedding: &[2.0, 0.0],
                    metadata: b"m",
                },
            )
            .expect("seed tenant-b row");
        storage
            .insert_row_into_table(
                "docs",
                2,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Private,
                    embedding: &[3.0, 0.0],
                    metadata: b"m",
                },
            )
            .expect("seed tenant-a private row");

        // tenant-a・Public の行だけを可視とする述語（`PolicyContext::is_visible` の
        // 既定挙動と同じ形の判定を模す）。
        let arena = VectorArena::build_filtered(&storage, "docs", |tenant, visibility| {
            tenant == "tenant-a" && visibility == Visibility::Public
        })
        .expect("build_filtered ok");

        assert_eq!(
            arena.len(),
            1,
            "only the tenant-a/Public row must be stored"
        );
        assert_eq!(arena.ids(), &[0u64]);
        assert_eq!(arena.vector(0), Some([1.0, 0.0].as_slice()));
        assert_eq!(
            arena.vectors().len(),
            2,
            "no capacity for excluded rows' vectors is retained in len()"
        );
        assert_eq!(arena.tenant_id(0), Some("tenant-a"));

        // 除外された行（id=1, id=2）はどのインデックスにも現れない。
        for idx in 0..arena.len() {
            assert_ne!(arena.tenant_id(idx), Some("tenant-b"));
        }
    }

    /// `docs` テーブル（dim=4）の行テーブルへ、次元不一致（dim=2）で破損した行を
    /// 検証を経由しない生の write トランザクションで直接書き込む
    /// （`insert_row_into_table` は挿入時点で次元検証するため、通常経路では作れない。
    /// `build_rejects_dimension_mismatch_without_partial_result` と同手法）。
    fn seed_dimension_mismatched_row(storage: &Storage, id: u64, tenant_id: &str) {
        let write_txn = storage.db().begin_write().expect("begin_write");
        {
            let row_table_name = catalog::user_rows_table_name("docs");
            let mut row_table = write_txn
                .open_table(catalog::user_rows_table_def(&row_table_name))
                .expect("open row table");
            let encoded = crate::storage::encode_row(&RowInput {
                tenant_id,
                visibility: Visibility::Public,
                embedding: &[1.0, 2.0],
                metadata: b"m",
            })
            .expect("encode mismatched-dim row");
            row_table
                .insert((tenant_id, id), encoded.as_slice())
                .expect("insert mismatched-dim row bypassing dim validation");
        }
        write_txn.commit().expect("commit mismatched-dim row");
    }

    // 対象ビヘイビア: TABLE-8（codex P0 対応・Issue #137）。`predicate` が `false` を
    // 返す（＝不可視な）行は、embedding の次元不一致検証を一切行わずスキップされる
    // こと（＝データ破損があっても `Err` にならず、そもそも走査結果から除外される
    // だけであること）を検証する。以前は不可視行でも次元不一致検出が先に走り、
    // 他テナントの破損状態だけで対象テナントの検索全体が失敗していた（他テナントの
    // 状態による可用性干渉。`build_filtered_still_detects_dimension_mismatch_in_rows_excluded_by_the_predicate`
    // という逆方向の契約を検証していた旧テストを、この新契約へ反転更新したもの）。
    #[test]
    fn build_filtered_skips_dimension_mismatched_rows_excluded_by_the_predicate_instead_of_failing()
    {
        let path = unique_db_path("filtered-dim-mismatch-skip");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&schema_for("docs", 4))
            .expect("create_table");

        seed_dimension_mismatched_row(&storage, 0, "tenant-b");

        // 述語は tenant-b を常に不可視として除外する。以前の契約では次元不一致検出が
        // 述語適用より前に走り Err(DimMismatch) になっていたが、新契約では embedding
        // のデコードにすら到達せず、単に空のアリーナとして成功する。
        let arena = VectorArena::build_filtered(&storage, "docs", |tenant, _| tenant != "tenant-b")
            .expect("invisible row's dimension mismatch must not fail the search");
        assert!(arena.is_empty());
    }

    // 対象ビヘイビア: TABLE-8（codex P0 対応・Issue #137）。他テナント（不可視）の破損行
    // （次元不一致）が存在していても、対象テナントの可視行が正常であれば検索
    // （`build_filtered`）が成功し、可視行だけを返すことを検証する。他テナントの
    // データ破損状態が対象テナントの検索可用性へ干渉しないことの直接的な確認。
    #[test]
    fn build_filtered_succeeds_for_visible_tenant_despite_corrupted_invisible_rows_of_another_tenant(
    ) {
        let path = unique_db_path("filtered-dim-mismatch-other-tenant");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&schema_for("docs", 4))
            .expect("create_table");

        // tenant-b（不可視想定）の破損行。
        seed_dimension_mismatched_row(&storage, 0, "tenant-b");
        // tenant-a（可視想定）の正常な行。
        storage
            .insert_row_into_table(
                "docs",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 2.0, 3.0, 4.0],
                    metadata: b"m",
                },
            )
            .expect("seed tenant-a row");

        let arena = VectorArena::build_filtered(&storage, "docs", |tenant, _| tenant == "tenant-a")
            .expect("search for tenant-a must succeed even though tenant-b's row is corrupted");
        assert_eq!(arena.len(), 1);
        assert_eq!(arena.ids(), &[1u64]);
        assert_eq!(arena.tenant_id(0), Some("tenant-a"));
    }

    // 対象ビヘイビア: TABLE-8（codex P0 対応・Issue #137）。行バイト列のヘッダ部
    // （バージョン・`tenant_len`・`tenant_id`・`visibility`）自体が破損していて
    // 可視性を判定できない場合は、`predicate` が全行を不可視と判定するとしても
    // `Err` を維持すること（fail-closed）。ヘッダが読めない時点では「不可視だから
    // スキップしてよい」と判断する根拠がないため、`decode_row_tenant_and_visibility`
    // の失敗はそのまま伝播しなければならない
    // （`crate::storage::decode_row_tenant_and_visibility` のドキュメント参照）。
    #[test]
    fn build_filtered_still_fails_when_row_header_itself_is_corrupted_regardless_of_predicate() {
        let path = unique_db_path("filtered-header-corrupt");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&schema_for("docs", 4))
            .expect("create_table");

        {
            let write_txn = storage.db().begin_write().expect("begin_write");
            {
                let row_table_name = catalog::user_rows_table_name("docs");
                let mut row_table = write_txn
                    .open_table(catalog::user_rows_table_def(&row_table_name))
                    .expect("open row table");
                // version バイトのみで tenant_len 以降が一切ない、意図的な破損バイト列
                // （`crates/engine/tests/vector_core.rs` の破損行テストと同手法）。
                row_table
                    .insert(("tenant-a", 0u64), &[1u8][..])
                    .expect("insert header-corrupted row");
            }
            write_txn.commit().expect("commit header-corrupted row");
        }

        // 全行を不可視とする述語を渡しても、ヘッダ自体が読めないので `Err` のまま。
        let err = VectorArena::build_filtered(&storage, "docs", |_, _| false)
            .expect_err("header corruption must fail closed regardless of predicate");
        assert!(
            matches!(err, ArenaError::Storage(_)),
            "expected ArenaError::Storage for header decode failure, got: {err:?}"
        );
    }

    // 対象ビヘイビア: TABLE-12（PR #563 codex-review P0 対応）。物理キー側
    // `tenant_id` とヘッダ側 `tenant_id` が意図的にずれた行（内部バグ・raw redb
    // 書き込みによる異常を想定）は、ヘッダ側が `Public` で可視と判定されても
    // アリーナへ格納せず fail-closed に拒否すること。この検査を欠いたまま
    // アリーナへ格納すると、`arena.tenant_id(slot)`（ヘッダ側の値）をキーに行を
    // 再取得する `sql::exec::decode_deferred_scalars_from_redb` の遅延投影経路が、
    // 元の走査対象とは異なる物理行（ここでは本物の `(tenant-b, 1)`）を re-fetch
    // してしまい、無関係な別テナントの `Private` 本文が漏えいする恐れがあった
    // （`sql/aggregate.rs::key_tenant_header_tenant_mismatch_is_rejected_fail_closed`
    // と同じ観点の検査を `arena.rs` の走査ループへも追加する回帰）。
    #[test]
    fn build_filtered_rejects_row_whose_key_tenant_and_header_tenant_mismatch() {
        let path = unique_db_path("key-header-tenant-mismatch");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&schema_for("docs", 4))
            .expect("create_table");

        {
            let write_txn = storage.db().begin_write().expect("begin_write");
            {
                let row_table_name = catalog::user_rows_table_name("docs");
                let mut row_table = write_txn
                    .open_table(catalog::user_rows_table_def(&row_table_name))
                    .expect("open row table");
                // 物理キーは tenant-a、ヘッダ内容は tenant-b・Public という
                // TABLE-12 違反の異常行（raw redb 書き込みで意図的に構成）。
                let buf = crate::storage::encode_row(&RowInput {
                    tenant_id: "tenant-b",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 2.0, 3.0, 4.0],
                    metadata: b"m",
                })
                .expect("encode row");
                row_table
                    .insert(("tenant-a", 1u64), buf.as_slice())
                    .expect("insert key/header tenant mismatched row");
            }
            write_txn.commit().expect("commit mismatched row");
        }

        // ヘッダ側 tenant（tenant-b）・`Public` により可視と判定される述語を
        // 渡しても、key/header 不一致は fail-closed に拒否される。
        let err = VectorArena::build_filtered(&storage, "docs", |tenant, visibility| {
            tenant == "tenant-b" && visibility == Visibility::Public
        })
        .expect_err("key/header tenant mismatch must be rejected fail-closed");
        assert!(
            matches!(err, ArenaError::Storage(_)),
            "expected ArenaError::Storage for key/header tenant mismatch, got: {err:?}"
        );
    }

    // 対象ビヘイビア: TABLE-8（codex 指摘対応・Issue #137）。他テナントの不可視行が
    // 大量にあっても、対象テナントの可視行数が上限内であれば検索
    // （`VectorArena::build_filtered`）が `CapacityExceeded` にならないことを検証する。
    // 本番の上限定数（`MAX_ARENA_ROWS` = 1,000,000・`MAX_ARENA_TOTAL_BYTES` = 1 GiB）で
    // 同じ状況を再現するのは非現実的（テストごとに 100 万行超を用意する必要がある）
    // ため、`check_capacity` と同じ理由でパラメータ化された
    // `build_filtered_with_limits` へ小さい `max_rows` を渡して同じ構造を検証する。
    //
    // `max_rows = 3` に対し、テーブル全体は 10 行（tenant-b の不可視行 8 件 + tenant-a の
    // 可視行 2 件）で、テーブル全行数（10）は上限（3）を超えている。以前の実装
    // （テーブル全行数を上限判定に使う）であればこの時点で `CapacityExceeded` になるが、
    // 可視行数（2）は上限（3）以下のため、修正後は成功し可視行だけが返る。
    #[test]
    fn build_filtered_capacity_check_is_based_on_visible_rows_not_total_table_rows() {
        let path = unique_db_path("capacity-visible-rows-only");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create_table");

        // tenant-b（不可視想定）の行を、可視行数の上限（3）を上回る本数だけ挿入する。
        for i in 0..8u64 {
            storage
                .insert_row_into_table(
                    "docs",
                    i,
                    &RowInput {
                        tenant_id: "tenant-b",
                        visibility: Visibility::Public,
                        embedding: &[9.0, 9.0],
                        metadata: b"m",
                    },
                )
                .expect("seed tenant-b row");
        }
        // tenant-a（可視想定）の行は上限（3）以下の 2 件のみ。
        for i in 8..10u64 {
            storage
                .insert_row_into_table(
                    "docs",
                    i,
                    &RowInput {
                        tenant_id: "tenant-a",
                        visibility: Visibility::Public,
                        embedding: &[1.0, 1.0],
                        metadata: b"m",
                    },
                )
                .expect("seed tenant-a row");
        }

        let max_rows = 3usize;
        let max_bytes = usize::MAX; // 本テストでは行数上限だけを検証対象にする。
        let arena = VectorArena::build_filtered_with_limits(
            &storage,
            "docs",
            |tenant, _| tenant == "tenant-a",
            max_rows,
            max_bytes,
        )
        .expect(
            "capacity check must be based on the 2 visible rows (<= max_rows), \
             not the 10 total table rows (> max_rows)",
        );

        assert_eq!(arena.len(), 2);
        assert_eq!(arena.ids(), &[8u64, 9u64]);
        for idx in 0..arena.len() {
            assert_eq!(arena.tenant_id(idx), Some("tenant-a"));
        }
    }

    // 対象ビヘイビア: TASK-100・RECOVER-9。`crates/engine/tests/index_failure_injection.rs`
    // の (2-b) は `SearchProvider::search`（アリーナ構築が完了した後の検索段）だけを
    // 失敗させており、`VectorArena::build_filtered_with_rows_in_txn` 自体（索引の
    // 再構築処理そのもの）が途中失敗するケースは公開 API 経由では注入できない
    // （codex-review P1 指摘。同ファイル冒頭コメント参照）。本テストはその欠落を、
    // 既に境界値検証用に確立済みの `build_filtered_with_limits`（本 `#[cfg(test)]`
    // モジュール内から小さい `max_rows` を渡す手法。上のテスト参照）を使って埋める:
    // 「可視行数が上限を超える」という、実データのみで到達可能な条件で再構築処理を
    // 実際に途中失敗させ、(a) 失敗が commit 済みの永続データを一切変更しないこと、
    // (b) 上限内の可視性へ絞った再試行（同じ commit 済みデータからの再構築）が
    // 欠落・重複なく完全一致で成功することを確認する。テスト専用 feature ゲート API は
    // 新設しない（本ファイル冒頭・`tests/index_failure_injection.rs` 冒頭コメントの
    // 方針を維持）。
    #[test]
    fn build_filtered_with_limits_failure_mid_rebuild_leaves_committed_rows_intact_and_recovers() {
        let path = unique_db_path("capacity-mid-rebuild-recovery");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create_table");

        // tenant-a の可視行を 5 件 commit する（この行データそのものが検証対象。
        // 再構築の成否にかかわらず一切変更されないことを後段で確認する）。
        for i in 0..5u64 {
            storage
                .insert_row_into_table(
                    "docs",
                    i,
                    &RowInput {
                        tenant_id: "tenant-a",
                        visibility: Visibility::Public,
                        embedding: &[1.0, 1.0],
                        metadata: b"m",
                    },
                )
                .expect("seed tenant-a row");
        }

        // 索引反映（再構築）1 回目: 可視行 5 件に対し上限を 3 件に絞り、再構築処理
        // そのものを `CapacityExceeded` で途中失敗させる（`build_filtered_with_rows_and_limits`
        // が行走査ループの途中で上限超過を検出して打ち切る経路。この時点で redb 側の
        // commit 済み行データには一切触れていない — 読み取り専用トランザクションの
        // ため書き込みが起きようがない）。
        let narrow_max_rows = 3usize;
        let err = VectorArena::build_filtered_with_limits(
            &storage,
            "docs",
            |tenant, _| tenant == "tenant-a",
            narrow_max_rows,
            usize::MAX,
        )
        .expect_err("rebuild itself must fail while the visible row count exceeds max_rows");
        assert!(
            matches!(err, ArenaError::CapacityExceeded),
            "expected ArenaError::CapacityExceeded from the rebuild path, got: {err:?}"
        );

        // 索引反映（再構築）2 回目: 同じ commit 済みデータに対し、上限を可視行数以上に
        // 戻して再試行する。1 回目の途中失敗が永続データを破損・欠落させていなければ、
        // 5 件全件が欠落・重複なく完全一致で返るはずである。
        let recovered = VectorArena::build_filtered_with_limits(
            &storage,
            "docs",
            |tenant, _| tenant == "tenant-a",
            10,
            usize::MAX,
        )
        .expect("retry with a sufficient max_rows must succeed after the mid-rebuild failure");
        assert_eq!(
            recovered.len(),
            5,
            "no row must be lost or duplicated by the earlier mid-rebuild failure"
        );
        let mut recovered_ids: Vec<u64> = recovered.ids().to_vec();
        recovered_ids.sort_unstable();
        assert_eq!(recovered_ids, vec![0u64, 1, 2, 3, 4]);
        for idx in 0..recovered.len() {
            assert_eq!(recovered.tenant_id(idx), Some("tenant-a"));
        }
    }

    // 対象ビヘイビア: TABLE-8（codex P1 対応・Issue #137）。`GrowableArenaBuffers` の
    // amortized 成長（capacity を倍々に増やす）が、実際に確保する capacity ベースの
    // 総バイト量を `max_bytes` の範囲内に収め続けることを検証する
    // （`Vec::try_reserve` を素朴に使うと要求量より多く確保しうるため、この検証なしでは
    // 実確保量が上限を超えうる）。
    //
    // `max_bytes` を「ちょうど 6 行分」に設定し、行数を 1 → 6 まで 1 件ずつ増やす。
    // 素朴な倍々成長だと、5 行目を追加しようとした時点で capacity を 4 → 8 へ
    // 倍加させたくなるが、8 行分は `max_bytes`（6 行分）を超える。本テストは、
    // その時点で `ensure_capacity` が先読み成長を諦め「今必要な 5 行分だけちょうど」へ
    // 縮退すること（＝実際の capacity が row_capacity として記録した値と一致し、
    // どの時点でも `check_capacity(row_capacity, ...)` が成功する＝max_bytes を
    // 超えないこと）を、内部状態（`row_capacity`・各 `Vec::capacity()`）を直接検査して
    // 確認する。
    #[test]
    fn growable_arena_buffers_capacity_growth_never_exceeds_max_bytes() {
        let dim_u32: u32 = 1;
        let dim_usize: usize = 1;
        let max_rows = 1_000usize; // 本テストでは行数上限ではなくバイト量上限を検証する。
        let per_row_bytes = (dim_usize * 4)
            + per_row_aux_bytes().expect("aux bytes computation must not overflow in test");
        let max_bytes = per_row_bytes * 6; // ちょうど 6 行分。

        let mut buffers = GrowableArenaBuffers::new();
        let mut previous_row_capacity = 0usize;
        for target_row_count in 1..=6usize {
            buffers
                .ensure_capacity(target_row_count, dim_usize, dim_u32, max_rows, max_bytes)
                .expect("growth within max_bytes budget must succeed");
            // `ensure_capacity` の内部計算（`additional_rows = new_row_capacity -
            // self.row_capacity` を `try_reserve_exact` へ渡す）は「呼び出し元が毎回
            // 直前に 1 行分を実際に push している」という不変条件（本番コードの
            // `build_filtered_with_limits` のループと同じ形）に依存するため、テストでも
            // 同じ順序（ensure_capacity → push_row）を守る。
            buffers.push_row(
                target_row_count as u64,
                &vec![0.0f32; dim_usize],
                "tenant-a".to_string(),
                Visibility::Public,
            );

            // 実際に記録された capacity（＝実際に `try_reserve_exact` で確保した量）が、
            // その時点でなお `max_bytes` の範囲内であることを、行った成長のたびに
            // 直接検証する（素朴な倍々成長のままなら 8 行分を確保しようとして
            // 上限超過になっていたはずの境界を含む）。
            assert!(
                check_capacity(buffers.row_capacity, dim_u32, max_rows, max_bytes).is_ok(),
                "row_capacity={} at target={target_row_count} must stay within max_bytes",
                buffers.row_capacity
            );
            assert!(
                buffers.row_capacity >= target_row_count,
                "row_capacity must always cover the requested target_row_count"
            );
            assert!(
                buffers.row_capacity >= previous_row_capacity,
                "row_capacity must never shrink across calls"
            );
            assert!(
                buffers.vectors.capacity() >= buffers.row_capacity * dim_usize,
                "vectors.capacity() must cover row_capacity * dim"
            );
            assert!(buffers.ids.capacity() >= buffers.row_capacity);
            assert!(buffers.tenant_ids.capacity() >= buffers.row_capacity);
            assert!(buffers.visibilities.capacity() >= buffers.row_capacity);

            previous_row_capacity = buffers.row_capacity;
        }

        // 7 行目は max_bytes（6 行分）を超えるため、成長候補・縮退後のちょうど確保
        // いずれの経路でも拒否される（`ensure_capacity` 呼び出し元は、この前段で
        // `check_capacity(visible_row_count, ...)` により先に拒否される想定だが、
        // `ensure_capacity` 単体でも fail-closed であることを確認する）。
        let err = buffers
            .ensure_capacity(7, dim_usize, dim_u32, max_rows, max_bytes)
            .expect_err("growth beyond max_bytes budget must fail");
        assert!(matches!(err, ArenaError::CapacityExceeded));
    }

    // 対象ビヘイビア: TABLE-8。アリーナは構築時点のスナップショットであり、build 後に
    // 追加された行は反映されない（単一スナップショットで構築する契約）。再 build すれば
    // 反映される。
    #[test]
    fn build_captures_a_snapshot_not_reflecting_later_writes() {
        let path = unique_db_path("snapshot");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&schema_for("docs", 2))
            .expect("create_table");

        storage
            .insert_row_into_table(
                "docs",
                0,
                &RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding: &[1.0, 2.0],
                    metadata: b"m",
                },
            )
            .expect("seed initial row");

        let arena_before = VectorArena::build(&storage, "docs").expect("build before extra write");
        assert_eq!(arena_before.len(), 1);

        storage
            .insert_row_into_table(
                "docs",
                1,
                &RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding: &[3.0, 4.0],
                    metadata: b"m",
                },
            )
            .expect("seed additional row after snapshot");

        // build 前に取得した arena_before はそのまま（後続の put の影響を受けない）。
        assert_eq!(arena_before.len(), 1);

        let arena_after = VectorArena::build(&storage, "docs").expect("rebuild after extra write");
        assert_eq!(arena_after.len(), 2);
    }

    // 対象ビヘイビア: TABLE-8（codex P1 対応）。旧 `Storage::put`/`Storage::scan` 系の
    // 平坦な行ストア（`storage.rs::ROWS_TABLE`）へ書き込まれた行は、テーブルスコープ行
    // API（`insert_row_into_table` 等）が使う `user_rows/{table_name}` とは別の redb
    // テーブルであるため、対象テーブルのアリーナには一切混入しないことを検証する
    // （以前の実装は「行への永続的なテーブル識別子」が無く、この分離を保証できなかった）。
    #[test]
    fn build_does_not_mix_in_rows_from_the_flat_legacy_row_store() {
        use crate::storage::RowInput as FlatRowInput;

        let path = unique_db_path("legacy-flat-store");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        // 旧経路（テーブル帰属を持たない平坦な行ストア）へ、同次元の行を書き込む。
        storage
            .put(
                999,
                &FlatRowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding: &[1.0, 2.0, 3.0, 4.0],
                    metadata: b"legacy-flat-store",
                },
            )
            .expect("seed row into legacy flat row store");

        storage
            .create_table(&schema_for("docs", 4))
            .expect("create_table");
        storage
            .insert_row_into_table(
                "docs",
                0,
                &RowInput {
                    tenant_id: TENANT_ID,
                    visibility: Visibility::Public,
                    embedding: &[5.0, 6.0, 7.0, 8.0],
                    metadata: b"table=docs",
                },
            )
            .expect("seed row into table-scoped store");

        let arena = VectorArena::build(&storage, "docs").expect("build arena");
        // 旧経路の行（id=999）は混入せず、テーブルスコープ経路の行（id=0）のみを保持する。
        assert_eq!(arena.len(), 1);
        assert_eq!(arena.ids(), &[0u64]);
        assert_eq!(arena.vector(0), Some([5.0, 6.0, 7.0, 8.0].as_slice()));
    }

    // 以下、旧 `tests/arena_perf.rs`（統合テスト）からの移設分。「コールドスタート時に
    // 一度だけアリーナを構築し、以降の検索はアリーナ上のスライスを参照する経路」が
    // 「クエリの都度 `Storage::scan_table_page` でページングしながら読み直してデコードする
    // 経路」より十分速いことを CI で検出可能にする（`tests/incremental_write_perf.rs`
    // （TASK-143）と同一の計測方針: ウォームアップ 1 回を除外・複数ラウンドの中央値比較・
    // 判定閾値は本テスト固有の計測パラメータで spec の実測比そのものは転記しない）。
    //
    // 規模の選定: `Storage::scan_table_page` は 1 ページあたり総バイト量
    // `MAX_SCAN_PAGE_BYTES`（16MiB）超で次ページへ打ち切る（`storage.rs` 参照）。
    // 本テストの行数・次元は、その上限に対して十分な余裕を残し、かつ debug ビルドでも
    // CI 実行時間が長くなりすぎないよう小さく抑えている
    // （ROWS × DIM × 4 バイト ≈ 2.6 MiB で 1 ページに収まる）。
    //
    // 計測対象の設計（Issue #171・フレーキー解消）: 両経路の消費者を「全行のベクトル
    // スライスへアクセスし軽量なチェックサムを積算するだけ」に絞り、内積等の
    // スコアリングは計測から外す。スコアリングコストは両経路に等しく乗る共通項であり、
    // それが全体時間に占める割合（=比率を 1 に近づける希釈度合い）は CPU 特性・
    // ビルド最適化・I/O 特性によって環境ごとに変わるため、ラウンド数や中央値では
    // 解消できない環境依存の下限を生む。TABLE-8 が検証したい性質は「一度デコードした
    // 連続バッファの提供によりクエリごとの再読込・再デコードを不要にする」ことに
    // 限られ、スコアリングは検索カーネル（`kernel.rs` / `parallel_search.rs`）の管轄で
    // アリーナの性質ではないため、計測対象から除外して問題ない。
    //
    // あわせて、両経路が「同一量のデータを処理している」ことを処理量カウンタ
    // （走査行数・チェックサム一致）で決定的に検証する。実時間比較だけでは
    // コンパイラ最適化による消去や比較条件のずれを見逃しうるため、これらは
    // 負荷に依存しない不変条件として毎ラウンド確認する。
    mod perf {
        use super::*;
        use std::time::Instant;

        /// 計測対象テーブル名。カタログにこのテーブルのみを登録し、`VectorArena::build`
        /// のテーブルスコープゲートを満たす。
        const TABLE_NAME: &str = "docs";

        /// 行数・次元（モジュールドキュメントの規模選定を参照）。
        const ROWS: u64 = 5_000;
        const DIM: usize = 128;

        /// 1 ラウンドで実行するクエリ反復回数（各回、全行のベクトルスライスへ
        /// アクセスしチェックサムを積算する。`RATIO_THRESHOLD_DENOM` の理論下限
        /// `≈ 1 / QUERY_COUNT` を決めるパラメータでもある）。
        const QUERY_COUNT: usize = 40;

        /// ノイズ対策として、複数ラウンド計測しラウンドごとの比率（`t_arena / t_rescan`）の
        /// 中央値を判定に使う回数（codex-review P2 指摘対応・PR #158: 旧版は両経路
        /// それぞれ独立に最小値（best-of-N）を取ってから比率を計算していたが、これは
        /// 「アリーナ経路がたまたま速かったラウンド」と「都度読み直し経路がたまたま
        /// 遅かったラウンド」が別ラウンドでも組み合わさってしまい、実際には一度も
        /// 両立していない好条件同士を比較することで、持続的な性能回帰を過小評価しうる。
        /// 同一ラウンド内でペアの比率を取ってから中央値を選ぶことで、ランナーノイズへの
        /// 頑健性（少数ラウンドの外れ値には引きずられない）を保ちつつ、大半のラウンドで
        /// 一貫して悪化する回帰は検出できるようにする）。
        const MEASUREMENT_ROUNDS: usize = 7;

        /// 判定閾値の分母（アリーナ経路は都度読み直し経路の `1 / RATIO_THRESHOLD_DENOM`
        /// 以下の時間で完了すること）。本テストの計測パラメータであり、アサーション
        /// 弱体化は行わない（`.claude/rules/coding-rust.md` 参照）。
        ///
        /// 両経路の消費者をチェックサム積算のみに絞ったことで、比率の理論下限は
        /// `≈ 1 / QUERY_COUNT`（本テストの設定では 0.025 程度）まで下がり、
        /// 閾値 1/2（0.5）に対して環境非依存の十分なマージンを持つ計測条件になった。
        /// これは検証の弱体化ではなく計測条件の是正であり、閾値の数値自体は
        /// 変更していない（Issue #171）。
        const RATIO_THRESHOLD_DENOM: u32 = 2;

        fn make_vector(rng: &mut Xorshift32, dim: usize) -> Vec<f32> {
            (0..dim).map(|_| rng.next_f32()).collect()
        }

        /// 複数ラウンドの比率（`t_arena / t_rescan`。同一ラウンド内のペアで計算済み）の
        /// 中央値を取る（`MEASUREMENT_ROUNDS` のドキュメントコメント参照）。
        fn median_ratio(mut ratios: Vec<f64>) -> f64 {
            assert!(!ratios.is_empty(), "at least one measurement round");
            ratios.sort_by(|a, b| a.partial_cmp(b).expect("ratio is finite"));
            let mid = ratios.len() / 2;
            if ratios.len() % 2 == 1 {
                ratios[mid]
            } else {
                (ratios[mid - 1] + ratios[mid]) / 2.0
            }
        }

        /// アリーナ経路の消費者。全行のベクトルスライスへアクセスし、先頭・末尾要素を
        /// `f64` へ積算した軽量チェックサムを返す（モジュールドキュメント参照:
        /// スコアリングを含めないことで比率希釈の環境依存性を避ける）。戻り値の
        /// `usize` は走査した行数で、`access_checksum_over_rescan` との比較条件
        /// （同一データ量を処理していること）を検証するために使う。
        /// `std::hint::black_box` を通し、コンパイラによる消去を防ぐ。
        fn access_checksum_over_arena(arena: &VectorArena) -> (f64, usize) {
            let mut checksum = 0.0f64;
            let mut visited = 0usize;
            for i in 0..arena.len() {
                let v = arena.vector(i).expect("index within arena bounds");
                let first = v.first().copied().unwrap_or(0.0) as f64;
                let last = v.last().copied().unwrap_or(0.0) as f64;
                checksum = std::hint::black_box(checksum + first + last);
                visited += 1;
            }
            (checksum, visited)
        }

        /// 都度読み直し経路の消費者。`access_checksum_over_arena` と同じ積算方式
        /// （行 ID 昇順・同一の先頭・末尾要素加算）を用い、チェックサムが決定的に
        /// 一致するようにする。
        fn access_checksum_over_rescan(storage: &Storage) -> (f64, usize) {
            // テーブルスコープ経路（`scan_table_page`）でページングしながら都度読み直す。
            // `Storage::scan`（`storage.rs` の平坦な `ROWS_TABLE` 走査）は本テストが使う
            // テーブルスコープ行 API（`insert_rows_into_table`）とは別の redb テーブルを
            // 参照するため、ここでは使えない。
            let mut checksum = 0.0f64;
            let mut visited = 0usize;
            // カーソルは物理キーと同形の `(tenant_id, id)`（TABLE-12）。
            let mut cursor: Option<(String, u64)> = None;
            loop {
                let (rows, next_cursor) = storage
                    .scan_table_page(
                        TABLE_NAME,
                        cursor.as_ref().map(|(t, id)| (t.as_str(), *id)),
                        crate::storage::MAX_SCAN_PAGE_LIMIT,
                    )
                    .expect("scan_table_page within configured limits");
                for row in &rows {
                    let first = row.embedding.first().copied().unwrap_or(0.0) as f64;
                    let last = row.embedding.last().copied().unwrap_or(0.0) as f64;
                    checksum = std::hint::black_box(checksum + first + last);
                    visited += 1;
                }
                match next_cursor {
                    Some(c) => cursor = Some(c),
                    None => break,
                }
            }
            (checksum, visited)
        }

        fn seed_storage(path: &std::path::Path) -> Storage {
            let storage = Storage::open(path).expect("open storage");
            storage
                .create_table(&schema_for(TABLE_NAME, DIM as u32))
                .expect("create_table");
            let mut rng = Xorshift32(0x2545_f491);
            let rows: Vec<(u64, Vec<f32>)> = (0..ROWS)
                .map(|id| (id, make_vector(&mut rng, DIM)))
                .collect();
            let batch: Vec<(u64, RowInput<'_>)> = rows
                .iter()
                .map(|(id, emb)| {
                    (
                        *id,
                        RowInput {
                            tenant_id: TENANT_ID,
                            visibility: Visibility::Public,
                            embedding: emb,
                            metadata: b"m",
                        },
                    )
                })
                .collect();
            storage
                .insert_rows_into_table(TABLE_NAME, &batch)
                .expect("seed dataset");
            storage
        }

        // 対象ビヘイビア: TABLE-8。「コールドスタート時に一度だけアリーナを構築し、以降の
        // クエリはアリーナ走査で完結する経路」が「クエリの都度 Storage::scan で全行を
        // 読み直しデコードする経路」より十分速いことを、判定閾値（RATIO_THRESHOLD_DENOM）で
        // 検証する。消費者はスコアリングを含まないチェックサム積算に限定し（モジュール
        // ドキュメント参照）、あわせて処理量（走査行数・チェックサム一致）を毎ラウンド
        // 検証することで、実時間比較だけでは見逃しうる比較条件のずれを防ぐ。
        #[test]
        fn table8_arena_query_path_completes_within_ratio_threshold_of_rescan_path() {
            let path = unique_db_path("perf-dataset");
            let _cleanup = CleanupGuard(path.clone());
            let storage = seed_storage(&path);

            // ウォームアップ 1 回（ファイルシステムキャッシュ等の初回コストを計測から
            // 除外する。既存 perf テスト tests/incremental_write_perf.rs と同方針）。
            {
                let arena = VectorArena::build(&storage, TABLE_NAME).expect("warmup build arena");
                for _ in 0..QUERY_COUNT {
                    std::hint::black_box(access_checksum_over_arena(&arena));
                }
                for _ in 0..QUERY_COUNT {
                    std::hint::black_box(access_checksum_over_rescan(&storage));
                }
            }

            let mut round_ratios = Vec::with_capacity(MEASUREMENT_ROUNDS);
            let mut round_durations = Vec::with_capacity(MEASUREMENT_ROUNDS);

            for round in 0..MEASUREMENT_ROUNDS as u32 {
                // ラウンドごとに計測順を交互にし、ランナー負荷の系統的なドリフト
                // （例: 前半ラウンドほど空いている／混んでいる）が一方の経路にだけ
                // 有利・不利に働くのを相殺する。
                let arena_first = round % 2 == 0;

                let (t_arena_round, t_rescan_round, arena_checksum, rescan_checksum) =
                    if arena_first {
                        let (t_arena, arena_checksum) = measure_arena_round(&storage);
                        let (t_rescan, rescan_checksum) = measure_rescan_round(&storage);
                        (t_arena, t_rescan, arena_checksum, rescan_checksum)
                    } else {
                        let (t_rescan, rescan_checksum) = measure_rescan_round(&storage);
                        let (t_arena, arena_checksum) = measure_arena_round(&storage);
                        (t_arena, t_rescan, arena_checksum, rescan_checksum)
                    };

                // 処理量検証: 両経路が同一データ量（全 ROWS 行 × QUERY_COUNT 反復分）を
                // 処理し、かつ同一の値を積算したことを確認する。コンパイラ最適化に
                // よる消去や、比較条件のずれ（例: 片方だけ行数が違う）を負荷に依存せず
                // 検出する。
                assert_eq!(
                    arena_checksum, rescan_checksum,
                    "arena and rescan paths must process identical data (round={round})"
                );

                // 比率は同一ラウンド内のペアで計算する（`MEASUREMENT_ROUNDS`
                // ドキュメンテーションコメント参照。異なるラウンドの最小値同士を
                // 比較しない）。
                round_ratios.push(
                    t_arena_round.as_secs_f64() / t_rescan_round.as_secs_f64().max(f64::EPSILON),
                );
                round_durations.push((t_arena_round, t_rescan_round));
            }

            let ratio = median_ratio(round_ratios);

            // プログラム出力文字列は英語規約（CI ログから経年変化を追跡できるようにする）。
            println!(
                "table8 arena vs rescan perf: median_ratio={ratio:.4} rounds={round_durations:?}"
            );

            assert!(
                ratio * f64::from(RATIO_THRESHOLD_DENOM) <= 1.0,
                "arena query path must complete within 1/{RATIO_THRESHOLD_DENOM} of the \
                 rescan path across a majority of rounds, median_ratio={ratio:.4} \
                 rounds={round_durations:?}"
            );
        }

        /// 1 ラウンド分のアリーナ経路計測。build 自体もこの経路のコストとして計測に
        /// 含める（都度読み直し経路の各反復が redb からの読み直しコストを含むのと
        /// 対称にするため）。`arena.len()` が全行数と一致することも検証する。
        fn measure_arena_round(storage: &Storage) -> (std::time::Duration, f64) {
            let started = Instant::now();
            let arena = VectorArena::build(storage, TABLE_NAME).expect("build arena (measured)");
            assert_eq!(
                arena.len(),
                ROWS as usize,
                "arena must hold all seeded rows"
            );
            let mut checksum = 0.0f64;
            for _ in 0..QUERY_COUNT {
                let (c, visited) = access_checksum_over_arena(&arena);
                assert_eq!(visited, ROWS as usize, "arena path must visit all rows");
                checksum += c;
            }
            (started.elapsed(), checksum)
        }

        /// 1 ラウンド分の都度読み直し経路計測。反復ごとに `scan_table_page` で
        /// 全行を読み直し、走査行数が全行数と一致することを検証する。
        fn measure_rescan_round(storage: &Storage) -> (std::time::Duration, f64) {
            let started = Instant::now();
            let mut checksum = 0.0f64;
            for _ in 0..QUERY_COUNT {
                let (c, visited) = access_checksum_over_rescan(storage);
                assert_eq!(visited, ROWS as usize, "rescan path must visit all rows");
                checksum += c;
            }
            (started.elapsed(), checksum)
        }
    }
}
