//! SQL 表層の実行計画層（TASK-75・TASK-76、対象ビヘイビア: SQL-1, SQL-2, SQL-3, SQL-4,
//! SQL-7。ポインタ: `docs/spec/05-tasks.md` TASK-75・TASK-76・
//! `docs/spec/04-behavior/sql-surface.md`・`docs/spec/04-behavior/rls.md` RLS-5）。
//!
//! 責務境界: [`parser::bind`](crate::sql::parser::bind) が返す [`BoundStatement`] を、
//! 既定順序 **RLS → SCALAR → DISTANCE**、または `HINT ORDER(...)` が指定した順序
//! （[`crate::sql::plan::EvaluationOrder`]）で実行する。順序が実効を持つのは
//! SCALAR と DISTANCE の相対位置のみ（[`crate::sql::plan::ExecutionPlan`] 参照）:
//! SCALAR が先なら候補構築時に等価条件を事前適用し（従来どおり正確に `limit` 件）、
//! DISTANCE が先なら DISTANCE 段の後で等価条件を事後適用する（`limit` 未満になり
//! 得る。under-fetch の救済（オーバーサンプル等）は本モジュールの管轄外）。
//!
//! RLS は **`HINT ORDER` の内容に関係なく**、唯一の実効的な防御である候補構築時の
//! 暗黙事前フィルタ（`VectorArena::build_filtered_with_rows_in_txn` の `predicate`。
//! `WHERE` 句の `visible()` 呼び出し（[`BoundStatement::rls_predicate_present`]）の
//! 有無に**関係なく**無条件に適用する。SQL-3・RLS-7・RLS-8）を必ず経由する。この
//! 事前フィルタの述語は RLS の暗黙適用フック（[`crate::rls::ImplicitRlsHook`]）
//! 経由で取得する（TASK-137・RLS-6, RLS-7）。加えて
//! [`crate::rls::RlsSafetyNet`]（TASK-136・RLS-5）を最終結果へ無条件に適用するが、
//! この安全網は事前フィルタと同じ `arena`（既に `ctx.is_visible` を通過済みの候補
//! 集合）由来のラベルで再判定するため、現状の実行経路では不可視行を追加で落とす
//! ことはない。無効化できない構造にしてあるのは、候補集合の構築元が将来広がった
//! 場合の defense-in-depth としてで、「独立した 2 つの検査が効いている」という
//! 意味ではない（詳細は `plan.rs` のモジュールドキュメント参照）。`HINT ORDER` で
//! RLS 段を後段に置いても、事前フィルタの適用そのものは外れない（security.md P0
//! 「テナント分離の検査を外す/緩める/バイパス経路を作らない」）。安全網通過済みの
//! `hits` は [`crate::rls::RlsVerifiedHits`]（witness 型）としてのみ投影段へ渡り、
//! 生の `Vec<(u64, f64)>` から投影へ到達する経路は型として存在しない。
//!
//! `core.rs::EngineCore::execute_sql`（TASK-75 で追加する固有メソッド。`VectorCore`
//! trait は不変）からのみ呼ばれる想定で、`Storage`・`SearchProvider`・`PolicyContext`
//! を束ねる。

use std::collections::HashSet;
use std::sync::Arc;

use crate::arena::{ArenaError, VectorArena};
use crate::catalog::{CatalogError, ColumnType, TableSchema};
use crate::core;
use crate::declarative_filter;
use crate::hybrid::{self, HybridError, HybridHit, RrfConfig};
use crate::kernel::{KernelError, SearchInput, SearchProvider};
use crate::policy::PolicyContext;
use crate::rls::{ImplicitRlsHook, RlsSafetyNet, RlsVerifiedHits};
use crate::row_codec::{self, RowCodecError, Value};
use crate::sparse::{DocId, SparseError, SparseIndex};
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::parser::{
    BoundConflictAction, BoundPredicateDelete, BoundPredicateUpdate, BoundStatement, BoundUpsert,
    BoundUpsertValue, ProjectedColumn, Ranking,
};
use crate::sql::plan::ExecutionPlan;
use crate::sql::udf_call;
use crate::storage::StorageError;

/// 疎コーパス側へ集約する候補プールの既定深さ。`hybrid::hybrid_search` の
/// `cfg.pool_depth()` に渡す（[`hybrid::RrfConfig::new`] の検証を通過する範囲で、
/// `bound.limit`（`1..=core::MAX_SEARCH_K`）を必ず満たせるよう `bound.limit` との
/// 最大値を取る）。
const DEFAULT_HYBRID_POOL_DEPTH: usize = 200;

/// `candidate_columns`（RLS/SCALAR 段を通過した可視行のうち、投影で実際に使う
/// スカラー列だけを保持するキャッシュ）が累計で保持してよいバイト数の上限
/// （Issue #56 レビュー指摘対応・codex P1: 1 フィールド最大 4 MiB（`row_codec::MAX_TEXT_FIELD_LEN`）
/// × 候補最大 `arena::MAX_ARENA_ROWS`（100 万行）の組み合わせでは、投影に不要な列まで
/// 複製・保持すると巨大確保に至り得る。`on_visible_row` は投影で使う列のみを保持し、
/// かつ保持前にこの累計上限をアロケーション前に検証する。上限判定の主体は
/// テキスト実体（`Value::Text` の複製バイト数）であり、これが本対応の中心。
/// 加えて、行ごとに必ず確保される `Vec<Value>` 自体の構造体サイズ
/// （`schema.columns.len() * size_of::<Value>()`）も加算する。列数が少ない
/// スキーマでは `MAX_ARENA_ROWS`（100 万行）分でも本上限には遠く及ばないため、
/// この構造体分の加算は「投影列を一切持たない `SELECT id ...` かつ列数が多い
/// スキーマ」の残余リスクを塞ぐ副次的な安全網であり、主たる歯止めではない。
/// `arena::MAX_ARENA_TOTAL_BYTES`
/// と同一の 1 GiB を採用する（256 MiB 等より小さい値にすると、候補行数が多いが
/// 1 行あたりは小さいテーブルへの `LIMIT` 付き正当なクエリを不必要に拒否しうるため、
/// 密ベクタ側の総量上限と同じ桁数に揃えて過度に厳しくしない）。
const MAX_CANDIDATE_SCALAR_BYTES: usize = crate::arena::MAX_ARENA_TOTAL_BYTES;

/// `current` に `add` を加えた累計が `cap` を超えないことを検証してから返す
/// （超過時は [`ArenaError::CapacityExceeded`]。アロケーション前の累計バイト量
/// 検証を 1 箇所に集約するための共通ヘルパー。`on_visible_row` の構造体バイト・
/// テキストバイトの両方の検証で使う）。
fn try_accumulate_budget(current: usize, add: usize, cap: usize) -> Result<usize, ArenaError> {
    let next = current.saturating_add(add);
    if next > cap {
        return Err(ArenaError::CapacityExceeded);
    }
    Ok(next)
}

/// `!scalar_prefilter`（DISTANCE 先行・SCALAR 事後フィルタ）の precision 経路で、
/// `visible_len`（`arena.ids().len()`。`ImplicitRlsHook` で RLS 済みの可視集合件数）
/// が `core::MAX_SEARCH_K` を超えるかどうかを純粋関数として切り出したもの
/// （`sql::exec::execute_statement` の DISTANCE 段呼び出し直前で使う）。超える場合、
/// DISTANCE 段の取得件数 `k_eff` は `MAX_SEARCH_K` へクランプされ、可視集合全体を
/// 対象にした「WHERE を満たす候補の完全な順位列」を構築できなくなる（`MAX_SEARCH_K`
/// 件目より後ろに僅差の Top-2 相当が存在しても取得できず、`precision::apply_gate`
/// が「Top-2 不在＝マージン成立」と誤判定する fail-open 経路になる。codex-review
/// 指摘・PRRT_kwDOUAKASM6cPLHE）。呼び出し元は `true` の場合、DISTANCE 検索自体を
/// 実行せず空集合（fail-closed の通常応答）へ倒す。
fn precision_completeness_unbounded(
    is_precision: bool,
    scalar_prefilter: bool,
    visible_len: usize,
) -> bool {
    is_precision && !scalar_prefilter && visible_len > core::MAX_SEARCH_K
}

/// `on_visible_row`（RLS/SCALAR 段の行フック）専用の、借用 `&str` から `String` への
/// 選択的複製ヘルパー（Issue #56 レビュー指摘対応・codex P1: `decode_scalar_columns`
/// の全列無条件確保を廃し、`row_codec::scan_scalar_columns` の borrow 結果から実際に
/// 必要な列だけを複製する設計へ切り替えた本対応の中心）。累計バイト量
/// （`budget`）を `cap` 超過なら**確保前**に `ArenaError::CapacityExceeded` として拒否し
/// （行単位の上限は `on_visible_row` 側で行構造体分を別途計上する）、確保自体も
/// `String::try_reserve_exact` を使うことで、論理サイズが上限内でもホスト側メモリが
/// 実際に不足した場合に abort ではなく `Err(ArenaError::AllocationFailed)` を返す
/// （`try_clone_text`・`arena.rs::try_reserve_exact` と同方針）。
fn try_alloc_text_for_budget(
    text: &str,
    budget: &mut usize,
    cap: usize,
) -> Result<String, ArenaError> {
    *budget = try_accumulate_budget(*budget, text.len(), cap)?;
    let mut owned = String::new();
    owned.try_reserve_exact(text.len()).map_err(|e| {
        ArenaError::AllocationFailed(format!("failed to reserve scalar text field: {e}"))
    })?;
    owned.push_str(text);
    Ok(owned)
}

/// [`try_alloc_text_for_budget`] の配列列版（Issue #888）。要素本文の合計バイト数
/// （TEXT 要素は文字列長・BOOLEAN 要素は 1 バイト／個）を確保前に予算計上してから
/// [`row_codec::ArrayRef::to_value`] で複製する。`ArrayRef` はすでに走査時
/// （[`row_codec::scan_scalar_columns`]）で構造・UTF-8 を検証済みのため、ここでの
/// 複製自体が失敗するのはメモリ不足時のみ。
fn try_alloc_array_for_budget(
    array_ref: row_codec::ArrayRef<'_>,
    budget: &mut usize,
    cap: usize,
) -> Result<Value, ArenaError> {
    // 要素本文（`payload_bytes`。TEXT 要素の実体を含む）に加え、`Vec<String>`
    // の構造体分（`String` 1 個あたり）も計上する（Issue #888 レビュー指摘:
    // 要素数のみでは可変長 TEXT 要素の実バイト量を捉えられない）。
    let approx_bytes = array_ref
        .payload_bytes()
        .saturating_add((array_ref.count() as usize).saturating_mul(std::mem::size_of::<String>()));
    *budget = try_accumulate_budget(*budget, approx_bytes, cap)?;
    let array_value = array_ref
        .to_value()
        .map_err(|e| ArenaError::AllocationFailed(format!("failed to decode array field: {e}")))?;
    Ok(Value::Array(array_value))
}

/// [`try_alloc_text_for_budget`] の `BYTEA` 版（Issue #886）。UTF-8 検証が
/// 無い点のみ異なり、累計バイト量の予算管理・確保失敗時の扱いは共有する。
fn try_alloc_bytes_for_budget(
    bytes: &[u8],
    budget: &mut usize,
    cap: usize,
) -> Result<Vec<u8>, ArenaError> {
    *budget = try_accumulate_budget(*budget, bytes.len(), cap)?;
    let mut owned: Vec<u8> = Vec::new();
    owned.try_reserve_exact(bytes.len()).map_err(|e| {
        ArenaError::AllocationFailed(format!("failed to reserve scalar bytea field: {e}"))
    })?;
    owned.extend_from_slice(bytes);
    Ok(owned)
}

// `pool_depth = bound.limit.max(DEFAULT_HYBRID_POOL_DEPTH)` が常に
// `hybrid::RrfConfig::new` の検証（`1..=hybrid::MAX_POOL_DEPTH`）を通過するのは、
// `bound.limit` の上限（`core::MAX_SEARCH_K`。`sql::parser::bind` が検証済み）が
// `hybrid::MAX_POOL_DEPTH` を超えない場合に限る。両定数は独立に管理されているため、
// 片方だけの変更でこの前提が崩れないようコンパイル時に固定する
// （`row_codec.rs`・`catalog.rs` の既存 `const _: () = assert!(...)` と同方針）。
const _: () = assert!(
    core::MAX_SEARCH_K <= hybrid::MAX_POOL_DEPTH,
    "sql::exec's hybrid pool_depth derivation assumes core::MAX_SEARCH_K <= hybrid::MAX_POOL_DEPTH"
);

// `precision` 時の `k_eff`（SCALAR 事前フィルタ経路: `bound.limit.max(2)`。
// SCALAR 事後フィルタ経路: `arena.ids().len().clamp(2, core::MAX_SEARCH_K)`。
// いずれも「`LIMIT 1` でも Top-2 を取得する」ための下限が `2`）が常に
// `core::MAX_SEARCH_K` の範囲内に収まることをコンパイル時に固定する
// （`bound.limit` は `sql::parser::bind` が `1..=core::MAX_SEARCH_K` を検証済み、
// 事後フィルタ経路は `.min(core::MAX_SEARCH_K)` で明示的にクランプ済みのため、
// `k_eff` が `MAX_SEARCH_K` を超えるのは `MAX_SEARCH_K < 2` の場合のみ）。
const _: () = assert!(
    2 <= core::MAX_SEARCH_K,
    "sql::exec's precision k_eff derivation assumes core::MAX_SEARCH_K >= 2"
);

/// 投影結果 1 セル。`row_codec::Value` の公開 enum は変更せず、`id` 疑似列
/// （`u64`）を表現するため独自の enum を持つ。
/// **TASK-79（SQL-9）で追加した破壊的変更（BREAKING CHANGE）**: `Float`・`Bool`
/// variant を追加した（宣言的 UDF・組み込み関数呼び出しの結果列。`row_codec::Value`
/// は変更しない方針を踏襲する）。
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Null,
    Integer(u64),
    Text(String),
    Vector(Vec<f32>),
    /// 式項目（TASK-79・SQL-9）の `Scalar` 型評価結果。
    Float(f64),
    /// 式項目（TASK-79・SQL-9）の `Bool` 型評価結果。BOOLEAN 列（TABLE-13・
    /// TASK-196、Issue #883）の投影結果もこの variant を共有する。
    Bool(bool),
    /// `INTEGER`／`BIGINT` 列の投影結果（Issue #881・TABLE-13・TASK-196）。
    /// 既存の `Integer(u64)`（疑似列 `id`・`COUNT` 用）とは意味が異なるため
    /// 別 variant とする。`INTEGER` 列は `i64::from` で無損失に格上げして格納する。
    SignedInteger(i64),
    /// `DATE` 列の投影結果（TABLE-13・TASK-197、Issue #884）。1970-01-01 起点の
    /// 日数。テキスト整形は [`crate::datetime::format_date`] に委譲する。
    Date(i32),
    /// `TIMESTAMP` 列の投影結果（TABLE-13・TASK-197、Issue #884）。1970-01-01
    /// 00:00:00 起点のマイクロ秒（タイムゾーンなし）。テキスト整形は
    /// [`crate::datetime::format_timestamp`] に委譲する。
    Timestamp(i64),
    /// 配列列（TABLE-14・TASK-198、Issue #888）の投影結果。
    Array(row_codec::ArrayValue),
    /// `BYTEA` 列の投影結果（Issue #886）。
    Bytes(Vec<u8>),
    /// `JSON`／`JSONB` 列の投影結果（TABLE-14・TASK-198、Issue #889）。格納テキスト
    /// をそのまま保持する（`JSON` 列は入力テキスト・`JSONB` 列は正規化済みテキスト。
    /// wire のテキスト表現はこれをそのまま出力する）。
    Json(String),
    /// `NUMERIC` 列の投影結果（TABLE-13〔検討中〕・TASK-197、Issue #885）。
    /// 正規テキスト表現は [`crate::numeric::Decimal`] の `Display` を参照。
    Numeric(crate::numeric::Decimal),
    /// `UUID` 列の投影結果（TABLE-13〔検討中〕・TASK-197、Issue #887）。
    /// 正規テキスト表現は [`crate::uuid::Uuid`] の `Display` を参照。
    Uuid(crate::uuid::Uuid),
}

/// 投影結果の列メタデータ。`Id` は疑似列（`ColumnType` を持たない）。
///
/// **TASK-79（SQL-9）で追加した破壊的変更（BREAKING CHANGE）**: `Computed` variant を
/// 追加した。
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnMeta {
    Id,
    Scalar {
        name: String,
        ty: ColumnType,
    },
    /// 式項目（TASK-79・SQL-9）。`ColumnType` を持たない（`Cell::Float`/`Cell::Bool`/
    /// `Cell::Vector` のいずれになるかは実行時の評価結果の型による）。
    Computed {
        name: String,
    },
}

/// 投影結果 1 行。
#[derive(Debug, Clone, PartialEq)]
pub struct ResultRow {
    pub id: u64,
    pub score: f64,
    pub cells: Vec<Cell>,
}

/// `EngineCore::execute_sql` の成功応答。
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<ResultRow>,
}

/// `EngineCore::execute_insert_sql` の成功応答（SQL-10、TASK-80）。行形 `INSERT` は
/// 単一行 `VALUES (...)` では `rows_affected` が常に `1` になるが、複数行
/// `VALUES (...), (...)`（SQL-16・TASK-190）では束縛した行数がそのまま
/// `rows_affected` になる（`INSERT 0 N` 相当の wire 応答〔TASK-73〕へ写像しやすい
/// よう件数フィールドとして保持する設計は不変）。ファイル形 `INSERT`
/// （TASK-120・INDEX-1, INDEX-2）は複数チャンク行を書き込むため `incremental` に
/// 計測・件数を保持し、行形では常に `None`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InsertOutcome {
    pub rows_affected: u64,
    pub incremental: Option<crate::incremental::IndexOutcome>,
}

/// `EngineCore::execute_truncate_sql` の成功応答（SQL-22、TASK-195）。削除件数を
/// 一切返さない契約（SQL-22。自テナント件数であっても再送側が件数推定に使えない
/// ようにする設計）を型で表現する固定タグのみの応答。フィールドを持たないが、
/// 将来の拡張余地を残すため `#[non_exhaustive]` にする。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct TruncateOutcome {}

/// `EngineCore::execute_delete_sql` の成功応答（SQL-18、TASK-191、#867）。
/// 単一行・`id` 等価指定形（[`execute_delete`]）では `rows_affected` は `0`
/// または `1` のいずれかになる——他テナント保持 id・未存在 id への削除要求は
/// [`execute_delete`] のドキュメント参照のとおり `0` へ写像し、対象行の
/// 有無（他テナント所有か未存在か）を応答から一切区別しない（RLS-9・RLS-10。
/// security.md「エラー・ログ経由で他テナントのデータ・存在情報を漏らさない」）。
///
/// 述語形（SQL-19・TASK-192、Issue #871。[`execute_predicate_delete`]）では
/// `rows_affected` は `0..=MAX_DML_AFFECTED_ROWS`（`sql::parser::
/// DEFAULT_MAX_DML_AFFECTED_ROWS`）の範囲を取り、一致した自テナント所有行の
/// 件数をそのまま表す（同じく他テナント行・不可視行は候補にすら含まれない）。
///
/// `rows_affected` は自テナントの結果を表すのみのため、`TruncateOutcome` と
/// 異なり露出しても再送側の件数推定材料にはならない（`INSERT 0 <rows>` と
/// 同じ pg 互換タグ `DELETE <rows>` へ写像する。`wire-server::simple_query`
/// 参照）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeleteOutcome {
    pub rows_affected: u64,
}

/// `RETURNING` 句（Issue #873・SQL-21）付き `INSERT`／`DELETE` の成功応答。
/// `rows_affected` は既存の `InsertOutcome`／`DeleteOutcome` と同じ意味（実際に
/// 変更した行数）で、`result.rows.len()` とは**独立**の値である——RLS 再判定
/// （[`crate::policy::PolicyContext::is_visible`]。多層防御）で書き込み本人にも
/// 不可視な行は `result.rows` から除外されるため、`rows.len() < rows_affected`
/// になりうる（`PolicyContext::new`〔`Public` のみ可視〕で `Private` 行を
/// `RETURNING` した場合など）。`CommandComplete` のタグ（`INSERT 0 <n>` 等）は
/// 必ず `rows_affected` を使い、`result.rows.len()` を使わない
/// （`wire-server::simple_query` 参照）。
#[derive(Debug, Clone, PartialEq)]
pub struct ReturningOutcome {
    pub command: crate::sql::returning::DmlCommand,
    pub rows_affected: u64,
    pub result: QueryResult,
}

/// `EngineCore::execute_sql_in_session` の `UPDATE` が成功したことを示す応答。
/// 単一行・`id` 完全一致形（SQL-17・TASK-191、Issue #865。
/// [`execute_update`]・`EngineCore::execute_update_sql` 経由）・述語形
/// （SQL-19・TASK-192、Issue #871。[`execute_predicate_update`]・
/// `EngineCore::execute_predicate_update_form` 経由）の両方がこの型を共有する。
/// `TruncateOutcome` とは異なり `UPDATE` は pg 互換の `CommandComplete` タグ
/// （`UPDATE <n>`）が更新行数を要求するため、`rows_affected` を保持する。
///
/// 単一行形: 対象行が他テナントの行・未存在 id・RLS 可視集合外のいずれであっても
/// `Ok(UpdateOutcome { rows_affected: 0 })` を返す（区別しない。
/// `tenant::update_row_columns_unchecked` ドキュメント参照）。
///
/// 述語形: `rows_affected` は `0..=MAX_DML_AFFECTED_ROWS`（`sql::parser::
/// DEFAULT_MAX_DML_AFFECTED_ROWS`）の範囲を取り、一致した自テナント所有行の
/// 件数をそのまま表す（他テナント行・不可視行は候補にすら含まれない。
/// [`DeleteOutcome`] の述語形と同じ意味論）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateOutcome {
    pub rows_affected: u64,
}

impl From<RowCodecError> for SqlSurfaceError {
    /// スカラーペイロードのデコード失敗は、格納済みデータの破損・実装バグの
    /// いずれかであり、SQL 入力自体の不正ではないため fail-closed に `XX000` へ
    /// 丸め込む（他テナントの行内容をエラー経由で漏らさない。security.md 準拠）。
    fn from(_e: RowCodecError) -> Self {
        SqlSurfaceError::Internal {
            detail: "scalar payload decode failed".to_string(),
        }
    }
}

/// `arena::build_filtered_with_rows` の RLS/SCALAR 段が返す `ArenaError` を SQL 表層の
/// エラー契約へ写像する。テーブル不在は `42P01`、容量上限超過は `54000`、それ以外
/// （データ破損・実装バグ）は `XX000` に丸め込む（他テナントの存在情報を漏らさない）。
fn map_arena_error(table: &str, e: ArenaError) -> SqlSurfaceError {
    match e {
        ArenaError::Catalog(CatalogError::TableNotFound(_)) => SqlSurfaceError::UndefinedTable {
            name: table.to_string(),
        },
        ArenaError::CapacityExceeded => {
            SqlSurfaceError::payload_too_large("candidate set exceeds arena capacity")
        }
        // TASK-79（SQL-9）: `WHERE` 式述語の評価（`sql::udf_call::eval`）が 0 除算・
        // 非有限値等で fail-closed に拒否した場合（`expr_eval_error_to_arena` 参照）。
        // データ破損・実装バグ（`Storage`/`Catalog`/`InvalidDim`/`DimMismatch`/
        // `AllocationFailed`）とは異なり、入力値に起因する「受理構文だが値が不正」
        // として `22000` へ写像する（`sql::parser::bind_in_session` の他の
        // `InvalidInput` と同じ分類）。
        ArenaError::InvalidInput(detail) => SqlSurfaceError::invalid_input(detail),
        ArenaError::Storage(_)
        | ArenaError::Catalog(_)
        | ArenaError::InvalidDim
        | ArenaError::DimMismatch { .. }
        | ArenaError::AllocationFailed(_) => SqlSurfaceError::Internal {
            detail: "arena build failed".to_string(),
        },
    }
}

/// `sql::udf_call::eval` が返す [`SqlSurfaceError`] を、`on_visible_row`（行フック。
/// 戻り値が [`ArenaError`] に固定されている）から返せる形へ写像する（TASK-79・
/// SQL-9）。`map_arena_error` がこの逆写像（`ArenaError` → `SqlSurfaceError`）を
/// 呼び出し元で行い、`22000`／`54000` の wire_code を保つ（`arena.rs` は sql 表層の
/// 型に依存しないため、両関数の対で往復させる）。
fn expr_eval_error_to_arena(e: SqlSurfaceError) -> ArenaError {
    match e {
        SqlSurfaceError::PayloadTooLarge { detail } => {
            let _ = detail;
            ArenaError::CapacityExceeded
        }
        SqlSurfaceError::InvalidInput { detail } => ArenaError::InvalidInput(detail),
        other => ArenaError::Storage(StorageError::Codec(other.to_string())),
    }
}

fn map_kernel_error(_e: KernelError) -> SqlSurfaceError {
    // クエリの次元・有限性は `sql::parser::bind`（ベクトルリテラル解析）が既に
    // 検証済みのため、ここへ到達するのは provider 側の契約違反・実装バグのみ
    // （fail-closed に内部エラーへ丸め込む）。
    SqlSurfaceError::Internal {
        detail: "search provider error".to_string(),
    }
}

/// 投影段（`VECTOR` 列を返す `SELECT *`・明示投影）で、候補選択と同一スナップショット
/// の `embedding`（`arena` が保持するバッファのスライス）を応答行用に複製する
/// （Issue #56 レビュー指摘対応・codex P1: `catalog::validate_schema` は 1 テーブルに
/// つき `VECTOR` 列を高々 1 本しか許さず、`hits.len()` は候補集合（`arena`。既に
/// `arena::MAX_ARENA_TOTAL_BYTES` で総量検査済み）の部分集合であるため、この複製の
/// 論理上限は arena 自体の容量検査で既に閉じている。残る懸念は `Vec::to_vec`
/// （内部で `Vec::with_capacity` を使い、確保失敗時に abort する）が、arena の
/// 検査を通過した論理サイズであってもホスト側のメモリが実際に不足した場合に
/// プロセスを OOM abort させてしまう点。`try_reserve_exact` を使うことで、その場合を
/// abort ではなく `Err`（fail-closed に `PayloadTooLarge`）として呼び出し元へ伝える
/// （`arena.rs::try_reserve_exact`・security.md「不安全な設計｜無制限リソース確保
/// （DoS）」と同方針）。
fn try_clone_embedding(embedding: &[f32]) -> Result<Vec<f32>, SqlSurfaceError> {
    let mut out: Vec<f32> = Vec::new();
    out.try_reserve_exact(embedding.len()).map_err(|_| {
        SqlSurfaceError::payload_too_large("projected vector payload exceeds available memory")
    })?;
    out.extend_from_slice(embedding);
    Ok(out)
}

/// 投影段で `Text` 列を応答行用に複製する。`candidate_columns`（`on_visible_row`）
/// 側は [`MAX_CANDIDATE_SCALAR_BYTES`] で累計バイト量を検証済みだが、`String::clone`
/// （内部で `String::with_capacity` 相当を使い、確保失敗時に abort する）を素朴に
/// 使うと、その論理上限内であってもホスト側のメモリが実際に不足した場合に
/// プロセスを OOM abort させ得る（Issue #56 レビュー指摘対応・codex P1:
/// `decode_scalar_columns`／`candidate_columns` の複製に関する指摘と同根の問題が
/// 応答構築側にも残っていたための追加対応。[`try_clone_embedding`] と同方針で
/// `try_reserve_exact` を使い、失敗を abort ではなく `Err`（fail-closed に
/// `PayloadTooLarge`）へ変換する）。
fn try_clone_text(text: &str) -> Result<String, SqlSurfaceError> {
    let mut out = String::new();
    out.try_reserve_exact(text.len()).map_err(|_| {
        SqlSurfaceError::payload_too_large("projected text payload exceeds available memory")
    })?;
    out.push_str(text);
    Ok(out)
}

fn map_hybrid_error(e: HybridError) -> SqlSurfaceError {
    match e {
        HybridError::Sparse(SparseError::QueryTooLong { .. })
        | HybridError::Sparse(SparseError::TooManyQueryTerms { .. }) => {
            SqlSurfaceError::invalid_input("hybrid query text exceeds allowed length")
        }
        HybridError::Sparse(
            SparseError::DocTooLong { .. }
            | SparseError::TooManyDocs { .. }
            | SparseError::CorpusTooLarge { .. }
            | SparseError::TooManyTokens { .. },
        ) => SqlSurfaceError::payload_too_large("hybrid sparse corpus exceeds capacity"),
        _ => SqlSurfaceError::Internal {
            detail: "hybrid search error".to_string(),
        },
    }
}

/// [`BoundStatement`] を実行する（TASK-75 の公開 API）。`read_txn`・`schema` は
/// 呼び出し元（`core.rs::EngineCore::execute_sql`）が単一の read トランザクション上で
/// `catalog::get_table_schema_in_txn` により取得し、`sql::parser::bind` へ渡したのと
/// 同一のものを渡す契約とする（Issue #56 レビュー指摘対応・codex P1: スキーマ取得
/// （`bind` 用）と候補走査（[`VectorArena::build_filtered_with_rows_in_txn`]）が別
/// read トランザクションに分かれていると、その間に並行 DDL（`alter_table_add_column`
/// 等）がコミットされた場合、`bind` が束縛した旧スキーマで新スナップショットの行を
/// 走査することになり、新設列の欠落や `row_codec` デコード失敗（スキーマ世代不一致）
/// が生じ得た。同一 `read_txn` を型で強制することで、スキーマ取得・bind・候補走査を
/// 単一スナップショットへ閉じ込める）。
///
/// `sparse_cache`（Issue #357・[`crate::sql::sparse_cache::SparseIndexCache`]）は
/// hybrid ランキング（[`Ranking::Hybrid`]）かつ `bound.metadata_filters`・
/// `bound.expr_filters` がともに空のクエリに限り経由する（詳細は
/// `sql/sparse_cache.rs` モジュールドキュメントの「適用条件」参照）。それ以外の
/// クエリ（DISTANCE 専用・フィルタ付き hybrid）は本関数内で一切参照しない。
/// `None` を渡した場合はキャッシュを一切経由せず、常に新規構築する（下記
/// [`execute_statement`] 参照）。
///
/// `arena_cache`（Issue #363・[`crate::sql::arena_cache::SqlArenaCache`]）が
/// `Some` の場合、候補構築（`VectorArena` の RLS 段アリーナ・行 metadata）を同一
/// テーブル世代内で再利用する。`None`（[`execute_statement`] からの呼び出し・
/// テスト等）の場合は従来どおり毎回 redb を走査する。`arena_cache` 内の `storage`
/// 参照は「クエリ実行に使う `read_txn` と同一テーブルを対象とする」契約を呼び出し
/// 元が保証する（`SqlArenaCache::lookup` は `read_txn` 自身からテーブル世代を読む
/// ため、呼び出し元が異なるテーブルの `read_txn` を渡す誤りを構造的に防げない点は
/// `bound.table` との一致を前提にする既存の契約と同じ）。ヒット時・ミス時いずれも、
/// SCALAR 段（`on_visible_row`。`WHERE`・hybrid 疎コーパス蓄積・投影用スカラー列
/// 保持）はクエリごとに毎回このまま実行する（キャッシュするのは RLS 段まで通過した
/// 行の埋め込み・metadata のみ。モジュールドキュメント「RLS → SCALAR →
/// DISTANCE」の責務境界は変えない）。
///
/// crate 内部専用（`pub(crate)`）。[`crate::sql::sparse_cache::SparseCacheAccess`]・
/// [`crate::sql::arena_cache::ArenaCacheAccess`] が属するモジュールはいずれも
/// `pub(crate)` であり、両型は crate 外から構築不能（AGENTS.md「公開 API・エラー
/// 契約の互換性（P1）」）。公開 API としては下記の [`execute_statement`]（従来
/// シグネチャを維持し両方に `None` を渡す薄いラッパー）のみを経由させ、この拡張版は
/// `core.rs::EngineCore::execute_validated_in_session` の `Statement::Select`
/// アーム（`USING PLAN` 展開経由を含む）からの crate 内呼び出し専用とする。
///
/// `hnsw_cache`（Issue #408・[`crate::sql::hnsw_cache::HnswIndexCache`]）は
/// `Ranking::Distance` かつ `bound.metadata_filters`・`bound.expr_filters` がともに
/// 空のクエリに限り経由する（詳細は `sql/hnsw_cache.rs` モジュールドキュメントの
/// 「適用条件」参照）。それ以外のクエリ（hybrid・フィルタ付き DISTANCE）は本関数内で
/// 一切参照しない。`None` を渡した場合はキャッシュを一切経由せず、常に全件
/// brute-force で探索する（下記 [`execute_statement`] 参照）。
///
/// `sparse_cache`・`arena_cache`・`hnsw_cache` の 3 引数を追加した結果
/// `clippy::too_many_arguments` の閾値（7）を超える。3 キャッシュアクセス束は意味の
/// 異なる独立したオプションパラメータであり、1 つの構造体へまとめると「呼ばれない
/// 方にも常に `None` を明示する」契約が読み取りにくくなるため、`pub(crate)` 専用の
/// 内部関数としてここで許容する（公開 API である [`execute_statement`] は従来どおり
/// 6 引数のまま）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_statement_with_cache(
    read_txn: &redb::ReadTransaction,
    provider: &dyn SearchProvider,
    ctx: &PolicyContext,
    schema: &TableSchema,
    bound: &BoundStatement,
    precision_policy: &crate::precision::PrecisionPolicy,
    sparse_cache: Option<crate::sql::sparse_cache::SparseCacheAccess<'_>>,
    arena_cache: Option<crate::sql::arena_cache::ArenaCacheAccess<'_>>,
    hnsw_cache: Option<crate::sql::hnsw_cache::HnswCacheAccess<'_>>,
    scalar_cache: Option<crate::sql::scalar_index::ScalarCacheAccess<'_>>,
) -> Result<QueryResult, SqlSurfaceError> {
    // TASK-162（対象ビヘイビア SEARCH-9）: `precision` の実行契約本体は
    // `crate::precision`（確信度判定・空集合 fail-closed 応答の純粋関数群）に
    // 切り出してあり、本関数は適用位置（DISTANCE 段＋事後 SCALAR フィルタの後・
    // `RlsSafetyNet::apply` の前）を決めるだけの薄い配線を担う（詳細は
    // `crate::precision` モジュールドキュメント「PRECISION ゲート段」参照）。
    // `recall`（既定）の実行経路はこの分岐に触れないため一切変わらない。
    let is_precision = bound.mode.mode == crate::sql::mode::SearchMode::Precision;

    // HINT ORDER（未指定なら既定の RLS→SCALAR→DISTANCE）から導出する実行方針。
    // `scalar_prefilter` のみが分岐点（`ExecutionPlan::from_evaluation_order` の
    // ドキュメント参照。TASK-76・SQL-7）。RLS 安全網はこの構造体に分岐用の
    // フィールドを持たせず、下記で無条件に呼び出す。
    let plan = ExecutionPlan::from_evaluation_order(bound.evaluation_order);

    // RLS 段（無条件）+ SCALAR 段（同一走査の行フック。`plan.scalar_prefilter` が
    // `false` の場合はここでは等価条件を判定せず、DISTANCE 段の後で事後適用する）。
    // 可視行だけがフックへ到達する（`arena::VectorArena::build_filtered_with_rows`
    // のドキュメント参照）。
    let mut sparse_docs: Vec<(u64, String)> = Vec::new();
    let is_hybrid = matches!(bound.ranking, Ranking::Hybrid { .. });
    let text_column_index = match &bound.ranking {
        Ranking::Hybrid {
            text_column_index, ..
        } => Some(*text_column_index),
        Ranking::Distance { .. } => None,
    };

    // Issue #357: `SparseIndex` のテーブル世代整合キャッシュ。フィルタなし hybrid
    // クエリに限り経由する（`sql/sparse_cache.rs` モジュールドキュメント「適用条件」
    // 参照。疎コーパスは SCALAR 事前フィルタ通過行にのみ割り当てられるため、
    // フィルタが無い場合のみ「RLS 可視行のうち本文非 NULL の全行」というクエリ
    // 非依存のコーパスが成立し、同一世代内でのスロット番号割当を含む再現性
    // 不変条件を満たす）。ヒットした場合は下の `on_visible_row` で疎コーパスへの
    // 複製蓄積自体を省略し（`skip_sparse_accumulation`）、フュージョン段で
    // キャッシュ済み索引をそのまま使う。
    // `sparse_cache` が `None`（公開ラッパー `execute_statement` 経由。モジュール
    // 冒頭ドキュメント参照）の場合はキャッシュを一切経由しない。
    // TASK-208・Issue #912: `or_filters` を含めないと `WHERE a OR b` だけの
    // クエリが「フィルタなし」と誤判定され、疎索引・HNSW フィルタなし専用
    // キャッシュ経路（下記 `sparse_cache_eligible`／`hnsw_full_visible_eligible`）
    // が OR 条件を一切適用せず全可視行を対象にしてしまう fail-open のバグに
    // なる（security.md「不安全な設計」対応）。
    let filters_empty = !bound.has_where_filters();
    let sparse_cache_eligible = sparse_cache.is_some() && is_hybrid && filters_empty;
    // Issue #408: `HnswIndexCache` の適用条件は `Ranking::Distance`（hybrid でない）
    // かつフィルタなし。`sparse_cache_eligible` の hybrid 版と対称の条件（`sql/
    // hnsw_cache.rs` モジュールドキュメント「適用条件」参照）。`hnsw_cache` が
    // `None`（`execute_statement` 経由・フィルタ付き DISTANCE）の場合は経由しない。
    // Issue #408 追記（Bugbot High 指摘）: `precision` モードは対象外とする。
    // `precision::apply_gate`（本ファイル下部）は DISTANCE 段が返す近似 Top-2 の
    // マージンで確信度ゲートを評価するため、ANN（HNSW）の近似近傍を渡すと真の
    // 最近傍を取りこぼしてマージンを過大評価し、SEARCH-9 が要求する「確信度不足
    // 時は空集合の fail-closed 応答」を満たせない可能性がある
    // （TASK-162・SEARCH-9）。`precision` では従来どおり厳密 brute-force 経路
    // （`hnsw_full_visible_eligible`／`hnsw_subset_eligible` いずれも `false`
    // の分岐）でゲートを評価する。
    //
    // Issue #409: 上記フィルタなし DISTANCE（`FullVisible` 形状）に加え、
    // SCALAR 事前フィルタ付き DISTANCE（`Subset` 形状。`!filters_empty &&
    // plan.scalar_prefilter`）も候補マスク付き ANN 経路へ載せる
    // （`sql::hnsw_cache::search_subset_or_fallback`）。DISTANCE 先行・SCALAR
    // 事後フィルタ（`!plan.scalar_prefilter`）の場合はフィルタの有無に関わらず
    // アリーナが可視全集合のままであり（`on_visible_row` が等価条件を判定せず
    // 素通しする。上のコメント「DISTANCE 先行時」参照）、`FullVisible` の不変
    // 条件（索引済みベースの `IndexedBase::build` 入力と一致するアリーナ）を
    // そのまま満たすため `filters_empty` を要求しない。逆に `!filters_empty &&
    // plan.scalar_prefilter` はアリーナが WHERE 適用後の**真部分集合**になり、
    // `FullVisible` の索引済みベースをそのまま探索対象にはできないため
    // `Subset` 形状（per-query 写像・キャッシュ非登録）へ回す
    // （`sql::hnsw_cache` モジュールドキュメント「適用条件」・
    // `docs/design/hnsw-rls-cardinality-switch.md` 参照）。
    // Issue #411: 判定本体は `sql::hnsw_cache::classify_ann_plan` に集約した
    // （`EXPLAIN` の `ann_plan:` 行が同じ関数を呼ぶ単一情報源。上記コメント
    // 「Issue #408」「Issue #409」の適用条件そのものは不変）。ここでは
    // `AnnShapeInput` を組み立てて分類し、`is_hybrid` で 4 boolean へ振り分ける
    // だけに簡素化する。
    let ann_plan =
        crate::sql::hnsw_cache::classify_ann_plan(crate::sql::hnsw_cache::AnnShapeInput {
            hnsw_enabled: hnsw_cache.is_some(),
            // Issue #411 追記（codex-review P1 指摘・PR #437）:
            // `sql::exec` の 4 boolean は `AnnPlan::UnknownCustomProvider` と
            // `AnnPlan::PlainScanEngine` のどちらであっても等しく `false`
            // （いずれも `HnswFullVisible`／`HnswSubset` ではない）ため、この
            // 判別は `EXPLAIN`（`core.rs`）専用の表示情報。ここでは常に
            // `false`（「エンジン種別は既知」）を渡す。
            engine_kind_unknown: false,
            is_hybrid,
            is_precision,
            filters_empty,
            scalar_prefilter: plan.scalar_prefilter,
        });
    let hnsw_full_visible_eligible =
        !is_hybrid && ann_plan == crate::sql::hnsw_cache::AnnPlan::HnswFullVisible;
    let hnsw_subset_eligible =
        !is_hybrid && ann_plan == crate::sql::hnsw_cache::AnnPlan::HnswSubset;
    // Issue #410: hybrid 密側の再取得ループ（`hybrid.rs::hybrid_search_boosted`
    // の `dense_fetch_k` 倍増ループ）向けの適用条件。`hnsw_full_visible_eligible`／
    // `hnsw_subset_eligible` と対称だが `is_hybrid` を要求する点のみが異なる
    // （`FullVisible`／`Subset` の形状判定自体は DISTANCE と同じロジックを流用
    // できる——両方とも「アリーナが可視全集合か WHERE 適用後の部分集合か」の
    // 判定であり `Ranking` の種別に依存しない）。`precision` を除外する理由は
    // DISTANCE と同じ（`precision_policy.hybrid()` の確信度ゲートは厳密順位を
    // 前提とするため。TASK-162・SEARCH-9）。実際の結線は `sql::hnsw_hybrid::
    // HnswDenseProvider` を介して行う（`hybrid.rs`・`SearchProvider` trait は
    // 無変更）。詳細は `docs/design/hnsw-hybrid-iterative-scan.md` 参照。
    let hnsw_hybrid_full_visible_eligible =
        is_hybrid && ann_plan == crate::sql::hnsw_cache::AnnPlan::HnswFullVisible;
    let hnsw_hybrid_subset_eligible =
        is_hybrid && ann_plan == crate::sql::hnsw_cache::AnnPlan::HnswSubset;
    // `sparse_cache_eligible` を満たす場合、`is_hybrid` の定義から
    // `text_column_index` は必ず `Some`（`Ranking::Hybrid` 分岐由来）。キャッシュ
    // キーに `text_column_index` を含める理由は `sql/sparse_cache.rs` モジュール
    // ドキュメント参照（同一テーブルで異なる TEXT 列を hybrid 本文に指定する 2
    // クエリを取り違えないため）。`sparse_cache.as_ref()` と `is_hybrid`・
    // `filters_empty` を独立に再チェックすることで、`expect`/`unwrap` を使わず
    // `sparse_cache_eligible` と実際の分岐を一致させる。
    let cached_sparse_index: Option<Arc<SparseIndex>> = if is_hybrid && filters_empty {
        match (sparse_cache.as_ref(), text_column_index) {
            (Some(access), Some(idx)) => {
                access
                    .cache
                    .lookup(access.storage, read_txn, &bound.table, ctx, idx)
            }
            _ => None,
        }
    } else {
        None
    };
    let skip_sparse_accumulation = cached_sparse_index.is_some();

    // 疎コーパスへ蓄積する累計文書数・バイト量。`sparse::SparseIndex::build` の上限
    // （`MAX_CORPUS_DOCS`・`MAX_CORPUS_BYTES`）は `try_alloc_text_for_budget` が
    // `String` 確保の**前**に検証する（.claude/rules/coding-rust.md「長さフィールドは
    // 上限検証してからアロケーションに使う」・security.md「不安全な設計｜無制限
    // リソース確保（DoS）」対応。可視行数の上限は `arena::MAX_ARENA_ROWS`（最大 100 万件）
    // だが、疎コーパス側の上限はそれよりずっと小さいため、蓄積前の検証がないと候補集合
    // 構築の途中で無制限に近いメモリを確保してから拒否することになる）。
    let mut sparse_bytes: usize = 0;

    // RLS → SCALAR 段（`on_visible_row`）で候補選択に使ったのと同一の read
    // トランザクション由来のデコード済みスカラー列を、投影段でそのまま再利用する
    // ために保持する（Issue #56 レビュー指摘対応・codex P1 / Bugbot Medium:
    // 以前は投影段で `storage.get_row_from_table` により id ごとに**別スナップショット**
    // を再取得しており、可視性のみ再検証してスカラー WHERE 条件は再検証していなかった
    // ため、候補選択後に行が更新されるとスカラー条件不一致の値や候補選択時と異なる
    // embedding を旧 score と組み合わせて返し得た。ここで候補構築時の行データを
    // 保持して投影に流用することで、SQL 実行全体を単一スナップショット
    // （`VectorArena::build_filtered_with_rows` が開く read トランザクション）に
    // 閉じ込め、再取得によるスナップショット不一致の窓をなくす）。
    //
    // 対応づけのキーは行 `id` ではなく**アリーナのスロット番号**
    // （`VectorArena::build_filtered_with_rows_in_txn` が `on_visible_row` の第 1 引数で
    // 渡す値。`arena.ids()` 等の添字と一致する）。行 `id` の一意性スコープはテナント内に
    // 閉じている（対象ビヘイビア: TABLE-12）ため、1 つの可視集合に同じ `id` の行が
    // 複数含まれうる（自テナント行と他テナントの `Public` 行）。`id` をキーにすると
    // embedding（アリーナ由来）とスカラー列（本表由来）が別テナントの行から混ざる
    // （Bugbot High 指摘）。スロット番号は `(tenant_id, id)` の行と 1 対 1 に対応するため、
    // 混線が構造的に起こらない。`Vec` の添字がスロット番号そのものであることは
    // `on_visible_row` が `true` を返した行だけが順番に push される契約で担保する。
    let mut candidate_columns: Vec<Vec<Value>> = Vec::new();

    // 投影段（下記ループ）が実際に参照する Text 列インデックスの集合。`VECTOR` 列は
    // `scan_scalar_columns` が常に `None` を返すだけ（実体は `arena` から引く）ため
    // 対象外。この集合に含まれない列は `on_visible_row` で借用のまま素通りし、
    // `String` として複製・保持しない（[`MAX_CANDIDATE_SCALAR_BYTES`] のドキュメント
    // 参照。投影に不要な列まで全候補分複製・保持する P1 指摘への対応）。
    // DISTANCE 先行時（`!plan.scalar_prefilter`）は SCALAR 条件を DISTANCE 段の後で
    // 事後適用するため、`metadata_filters`（TASK-147・EXT-3。等価・前方一致）が
    // 参照する列も候補構築時に保持しておく必要がある（TASK-76・SQL-7）。SCALAR
    // 先行時（既定）はここで条件を判定し終えるため、値の保持自体は不要（従来どおり
    // 投影列のみ保持する）。
    let mut needed_column_indices: HashSet<usize> = bound
        .projection
        .iter()
        .filter_map(|col| match col {
            ProjectedColumn::Column { index, .. } => Some(*index),
            // `Computed`（TASK-79・SQL-9）は候補構築時に保持した `candidate_columns`
            // （`Text` 列のデコード結果）を参照しない。評価に使うのは `arena` 由来の
            // `id`／embedding のみ（`sql::udf_call::eval` 参照。`TEXT` 列参照は
            // 束縛段で拒否済み）。
            ProjectedColumn::Id | ProjectedColumn::Computed { .. } => None,
        })
        .collect();
    if !plan.scalar_prefilter {
        needed_column_indices.extend(bound.metadata_filters.iter().map(|f| f.column_index()));
        // TASK-208・Issue #912: OR 群が参照する列も同様に保持する（DISTANCE
        // 先行時は SCALAR 条件を DISTANCE 段の後で事後適用するため、候補構築時に
        // 保持しておかないと事後適用の時点で値が無く、OR 条件が判定不能になる）。
        for group in &bound.or_filters {
            group.visit_column_indices(&mut |idx| {
                needed_column_indices.insert(idx);
            });
        }
    }

    // Issue #453（SQL-1・SQL-3。ポインタ: `docs/spec/04-behavior/sql-surface.md`
    // SQL-1・SQL-3、`docs/design/crossdb-bench.md`「self の投影コスト切り分け」節）:
    // `WHERE`（メタデータフィルタ・式述語）も hybrid（疎コーパス蓄積に本文列を
    // 要する）も絡まないクエリでスカラー列を投影する場合に限り、`on_visible_row`
    // での毎行 `scan_scalar_columns`（構造検証＋`Text` 列の `String` 複製）を
    // 省略し、`RlsSafetyNet::apply` 通過後の Top-k（またはそれ以下）の行だけへ
    // 遅延させる（下記 `project_rows` の `ScalarSource::Deferred` 参照）。この
    // 3 条件が成り立つ場合、SCALAR 段は `HINT ORDER` の内容に関係なく事前・事後
    // いずれの等価条件判定も持たない恒等写像になるため、遅延しても既存の実行順序
    // 契約（モジュールドキュメント「RLS → SCALAR → DISTANCE」）を変えない。
    //
    // Issue #660 の改善候補 P2: 「hybrid でない」条件を
    // 「hybrid なら疎索引キャッシュヒット時のみ」へ緩和する。hybrid が
    // 遅延対象外だったのは `on_visible_row` が疎コーパス（本文 `TEXT` 列）の
    // 蓄積のために必ず行を走査する必要があったためで、`SparseIndexCache`
    // （Issue #357）がヒットしていれば（`skip_sparse_accumulation`）その蓄積は
    // 行われず SCALAR 段は恒等写像になる。この場合の投影列デコードは
    // Top-k 確定後（`ScalarSource::Deferred`）へ安全に遅らせられる
    // （hybrid の Top-k スロット番号はアリーナ＝スナップショットのスロット
    // 番号そのものであり、`DeferredScalars::Snapshot` の添字と一致する）。
    let defer_projection = !bound.has_where_filters()
        && (!is_hybrid || skip_sparse_accumulation)
        && !needed_column_indices.is_empty();

    // `candidate_columns` に保持する Text 値の実バイト数に加え、行ごとに必ず確保する
    // `Vec<Value>` 自体の構造体サイズも累計する（[`MAX_CANDIDATE_SCALAR_BYTES`] の
    // ドキュメント参照。投影列を持たない `SELECT id ...` でも候補行数に比例して
    // 積み上がる構造体アロケーションを見逃さないため）。
    let mut candidate_scalar_bytes: usize = 0;
    let row_struct_bytes = schema
        .columns
        .len()
        .saturating_mul(std::mem::size_of::<Value>());

    // Issue #353・PR #373 codex-review 指摘対応: `ExprProgram::eval` の明示
    // スタック。`on_visible_row`（下記クロージャ）は行ごとに異なる late-bound
    // ライフタイムで `embedding` を受け取るが、[`crate::sql::expr_program::StackValue`]
    // は借用を保持しないため `embedding` のライフタイムに紐付かない。そのため
    // このクロージャの外（行ループの外）で 1 回だけ確保し、`&mut` 捕獲で
    // 行ごとに使い回せる（以前は `ExprValue<'a>` を直接積んでおり、クロージャ
    // 境界を越える借用の衝突（E0521）を避けるためクロージャ本体のローカルとして
    // 毎呼び出し新規確保していた）。
    let mut expr_scratch: Vec<crate::sql::expr_program::StackValue> = Vec::new();
    // TASK-208・Issue #912: `WHERE` の OR 群評価（`BoundOrGroup::matches`）用の
    // スクラッチバッファ。`expr_scratch` と同じ理由で行ループの外に 1 回だけ
    // 確保し、行ごとに使い回す。
    let mut or_scratch: Vec<crate::sql::expr_program::StackValue> = Vec::new();

    let on_visible_row = |slot: usize,
                          id: u64,
                          embedding: &[f32],
                          metadata: &[u8]|
     -> std::result::Result<bool, ArenaError> {
        // Issue #453: `defer_projection` が真の場合（`WHERE`・式述語・hybrid の
        // いずれも絡まずスカラー列を投影するクエリ）、SCALAR 段は恒等写像であり
        // ここで判定すべきフィルタが存在しない。`scan_scalar_columns`（構造検証＋
        // `Text` 列の `String` 複製）を全可視行に適用するコストを避けるため、
        // ここでは一切走査せず可視行をそのまま通過させる。投影に必要なスカラー
        // 列の取得・デコードは `RlsSafetyNet::apply` 通過後の Top-k 行に限って
        // `project_rows` の `ScalarSource::Deferred` が行う（下記参照）。
        if defer_projection {
            debug_assert_eq!(candidate_columns.len(), slot);
            candidate_columns.push(Vec::new());
            return Ok(true);
        }
        // Issue #56 レビュー指摘対応・codex P1: 旧実装は `decode_scalar_columns` で
        // 全 `Text` 列を無条件に `to_string()` 確保してから、投影に不要な列を
        // 捨てていた。最大長 `Text` 列を多数持つスキーマでは `SELECT id` のような
        // 値を保持しないクエリでも 1 行分の巨大な一時確保が先に発生し得た
        // （security.md「不安全な設計｜無制限リソース確保（DoS）」）。
        // `scan_scalar_columns` は構造検証のみを行い `Text` 値を `metadata` 借用の
        // `&str` として返すため、ここではヒープ確保が一切発生しない。フィルタ条件
        // の突合も借用のまま行い、投影・hybrid 本文として実際に必要な列だけを
        // 下のループで選択的に複製する。
        let scanned = row_codec::scan_scalar_columns(schema, metadata)
            .map_err(|e| ArenaError::Storage(StorageError::Codec(e.to_string())))?;
        // SCALAR 先行（既定）の場合のみここでメタデータフィルタ（TASK-147・EXT-3。
        // 等価・前方一致）を事前適用する。DISTANCE 先行（`HINT ORDER`）の場合は
        // 可視行を無条件に通過させ、DISTANCE 段の後で `apply_scalar_postfilter` が
        // 事後適用する（§モジュールドキュメント参照）。
        if plan.scalar_prefilter {
            if !declarative_filter::matches_all(&bound.metadata_filters, &scanned) {
                return Ok(false);
            }
            // TASK-79（SQL-9）: `WHERE` の式述語（宣言的 UDF・組み込み関数呼び出し）を
            // 既存のメタデータフィルタと同じ SCALAR 段の一部として事前適用する。可視行
            // （RLS-8 の暗黙適用を通過した行）にのみ到達するため、不可視行では
            // 式が一切評価されない。評価エラー（0 除算・非有限値等）は当該行を
            // 黙ってスキップせず、クエリ全体の失敗として fail-closed に伝播する
            // （`expr_eval_error_to_arena` 経由で `22000`／`54000` へ写像）。
            for program in &bound.expr_filter_programs {
                match program
                    .eval(id, embedding, &mut expr_scratch)
                    .map_err(expr_eval_error_to_arena)?
                {
                    udf_call::ExprValue::Bool(true) => {}
                    // NULL（UNKNOWN）は非該当として扱う（対象ビヘイビア: SQL-26。
                    // Issue #921）。
                    udf_call::ExprValue::Bool(false) | udf_call::ExprValue::Null => {
                        return Ok(false)
                    }
                    // 束縛段（`sql::parser::bind_in_session`）が `WHERE` 式述語の型を
                    // `Bool` に限定済みのため到達しない（`ExprType::Bool` 検査参照）。
                    _ => {
                        return Err(ArenaError::InvalidInput(
                            "WHERE expression did not evaluate to a boolean".to_string(),
                        ))
                    }
                }
            }
            // TASK-208・SQL-24（Issue #912）: `WHERE` の OR 群を、既存の
            // メタデータフィルタ・式述語と同じ SCALAR 事前フィルタ段の一部として
            // 適用する（宣言順で AND 結合される最後の条件群という位置づけ）。
            for group in &bound.or_filters {
                if !group
                    .matches(&scanned, id, embedding, embedding.len(), &mut or_scratch)
                    .map_err(expr_eval_error_to_arena)?
                {
                    return Ok(false);
                }
            }
        }
        // Issue #357: キャッシュヒット時（`skip_sparse_accumulation`）は疎コーパスの
        // 複製蓄積自体を省略する（フュージョン段でキャッシュ済み索引をそのまま使う
        // ため、ここで文書を複製・保持する必要がない。上のキャッシュ lookup のドキュ
        // メント参照）。
        if is_hybrid && !skip_sparse_accumulation {
            if let Some(idx) = text_column_index {
                if let Some(t) = scanned
                    .get(idx)
                    .copied()
                    .flatten()
                    .and_then(|v| v.as_text())
                {
                    if sparse_docs.len() >= crate::sparse::MAX_CORPUS_DOCS {
                        return Err(ArenaError::CapacityExceeded);
                    }
                    let owned = try_alloc_text_for_budget(
                        t,
                        &mut sparse_bytes,
                        crate::sparse::MAX_CORPUS_BYTES,
                    )?;
                    // 疎コーパスの文書 id もスロット番号（`DocId = u64`）にする。
                    // 行 `id` を使うと、同一 `id` の可視行が 2 テナント分あるときに
                    // `SparseIndex::build` が `DuplicateDocId` で失敗し、ハイブリッド
                    // SQL 全体が落ちる（Bugbot Medium 指摘。対象ビヘイビア: TABLE-12）。
                    let doc_id = u64::try_from(slot).map_err(|_| ArenaError::CapacityExceeded)?;
                    sparse_docs.push((doc_id, owned));
                }
                // NULL 本文は疎側に含めない（密のみへ寄与する）。
            }
        }
        // スカラー条件（あれば）を通過した行のみを、投影段で再利用するために保持する。
        // ただし投影で実際に参照される列（`needed_column_indices`）だけを複製し、
        // それ以外は `Value::Null` として保持する（複製しない）。行を保持することで
        // 増える累計バイト量（行構造体分 `row_struct_bytes` を先に、Text 実体分は
        // 複製する列ごとに）を、それぞれの確保の**前**に検証し、上限超過は
        // fail-closed に `ArenaError::CapacityExceeded`（`map_arena_error` 経由で
        // `PayloadTooLarge`）として拒否する（投影列を持たない `SELECT id ...` でも、
        // 候補行数に比例する `Vec<Value>` 構造体分だけで無制限に積み上がらない
        // ようにする）。行単位の上限は「投影された列と hybrid 本文 1 本のみが複製
        // 対象になる」という構造そのものが担保する（必要な列だけを、必要な分だけ
        // 確保前に検証してから複製するため、行あたりの複製量は投影列の実バイト数
        // を超えない）。
        candidate_scalar_bytes = try_accumulate_budget(
            candidate_scalar_bytes,
            row_struct_bytes,
            MAX_CANDIDATE_SCALAR_BYTES,
        )?;
        // 投影で参照される列が 1 つも無い（`SELECT id ...`。TASK-83 条件7 の C1
        // 規範形）場合、下のループは全件 `Value::Null` を積むだけになる。その値は
        // `project_rows` で一切読まれない（`candidate_columns.get(slot)` は存在
        // 確認のみに使われ、`ProjectedColumn::Id` のみの投影では中身を見ない。
        // `project_rows` のドキュメント参照）ため、行数分のヒープ確保・push を
        // 省略できる（Issue #314・SQL-1・TASK-83 条件7: SQL 表層 C1 経路の固定
        // コスト削減）。`Vec::new()` は容量 0 でヒープ確保しない。スロット添字
        // 契約（`candidate_columns[slot]` が可視行と 1 対 1）は空 `Vec` でも
        // `candidate_columns.push` によって維持される。
        let kept: Vec<Value> = if needed_column_indices.is_empty() {
            Vec::new()
        } else {
            let mut kept: Vec<Value> = Vec::new();
            kept.try_reserve_exact(scanned.len()).map_err(|e| {
                ArenaError::AllocationFailed(format!("failed to reserve scalar column slots: {e}"))
            })?;
            for (idx, slot) in scanned.into_iter().enumerate() {
                if !needed_column_indices.contains(&idx) {
                    kept.push(Value::Null);
                    continue;
                }
                match slot {
                    None => kept.push(Value::Null),
                    Some(row_codec::ScalarRef::Text(t)) => {
                        let owned = try_alloc_text_for_budget(
                            t,
                            &mut candidate_scalar_bytes,
                            MAX_CANDIDATE_SCALAR_BYTES,
                        )?;
                        kept.push(Value::Text(owned));
                    }
                    Some(row_codec::ScalarRef::Integer(v)) => kept.push(Value::Integer(v)),
                    Some(row_codec::ScalarRef::BigInt(v)) => kept.push(Value::BigInt(v)),
                    // REAL/DOUBLE は固定長のためヒープ確保・バイト予算計上を
                    // 要しない（`Value::Real`/`Value::Double` はスタック上の値）。
                    Some(row_codec::ScalarRef::Real(v)) => kept.push(Value::Real(v)),
                    Some(row_codec::ScalarRef::Double(v)) => kept.push(Value::Double(v)),
                    Some(row_codec::ScalarRef::Enum(label)) => {
                        let owned = try_alloc_text_for_budget(
                            label,
                            &mut candidate_scalar_bytes,
                            MAX_CANDIDATE_SCALAR_BYTES,
                        )?;
                        kept.push(Value::Enum(owned));
                    }
                    Some(row_codec::ScalarRef::Bool(b)) => {
                        kept.push(Value::Bool(b));
                    }
                    Some(row_codec::ScalarRef::Date(d)) => {
                        kept.push(Value::Date(d));
                    }
                    Some(row_codec::ScalarRef::Timestamp(t)) => {
                        kept.push(Value::Timestamp(t));
                    }
                    Some(row_codec::ScalarRef::Array(array_ref)) => {
                        let value = try_alloc_array_for_budget(
                            array_ref,
                            &mut candidate_scalar_bytes,
                            MAX_CANDIDATE_SCALAR_BYTES,
                        )?;
                        kept.push(value);
                    }
                    Some(row_codec::ScalarRef::Bytes(bytes)) => {
                        let owned = try_alloc_bytes_for_budget(
                            bytes,
                            &mut candidate_scalar_bytes,
                            MAX_CANDIDATE_SCALAR_BYTES,
                        )?;
                        kept.push(Value::Bytes(owned));
                    }
                    Some(row_codec::ScalarRef::Json(t)) => {
                        let owned = try_alloc_text_for_budget(
                            t,
                            &mut candidate_scalar_bytes,
                            MAX_CANDIDATE_SCALAR_BYTES,
                        )?;
                        kept.push(Value::Json(owned));
                    }
                    Some(row_codec::ScalarRef::Numeric(d)) => {
                        kept.push(Value::Numeric(d));
                    }
                    Some(row_codec::ScalarRef::Uuid(u)) => {
                        kept.push(Value::Uuid(u));
                    }
                }
            }
            kept
        };
        // `slot` は「これから push される行の添字」（アリーナ側の契約）。ここでの
        // push により `candidate_columns[slot] == kept` が成立する。両者がずれた場合は
        // 後続の投影で誤った行を返しうるため、デバッグビルドで不変条件を固定する。
        debug_assert_eq!(candidate_columns.len(), slot);
        candidate_columns.push(kept);
        Ok(true)
    };

    // キャッシュヒット時、SCALAR 段（`on_visible_row`）が全行を無条件に通過させる
    // だけ（＝恒等写像）であると静的に判定できる場合、行単位ループ自体
    // （`push_visible_row` の `tenant_id.to_string()` 複製・`GrowableArenaBuffers`
    // への再構築）を丸ごと省略し、キャッシュ済みスナップショットの `VectorArena` を
    // 直接借用する（Issue #363 追補: `build_from_cached_rls_rows` 経由の複製でも
    // redb 走査・デコードは避けられるが、行数が多いテーブルではこの per-row
    // コピー自体が支配的コストになることが実測〔`make bench-c1` 相当〕で判明した）。
    // 3 条件がすべて成り立つ場合のみ SCALAR 段が恒等写像になる（`on_visible_row`
    // の実装〔本関数冒頭〕参照）: (a) メタデータフィルタなし (b) 式述語なし
    // (c) hybrid でない（疎コーパス蓄積が不要）。この場合 `HINT ORDER` の内容
    // （`plan.scalar_prefilter`）によらず結果は変わらない（SCALAR 段が空判定で
    // ある以上、先行でも後行でも同じ）。
    //
    // Issue #453 以前は「投影が候補スカラー列を一切参照しない
    // （`needed_column_indices` が空）」も 4 つ目の条件だった。投影列を参照する
    // クエリはこの高速経路から外れ、`SELECT id ...`（Issue #314）以外は毎回
    // `build_from_cached_rls_rows` 経由の全行再構築を経ていた。`defer_projection`
    // （投影で参照するスカラー列のデコード自体を Top-k 確定後へ遅延する。上記
    // ドキュメント参照）の導入により、投影列の有無は SCALAR 段の恒等写像性に
    // 影響しなくなった（`defer_projection` が真のとき `on_visible_row` は
    // `needed_column_indices` の中身に関係なく空 `Vec` を積むだけの早期リターンに
    // なる）ため、この条件は不要になり撤去した。
    //
    // Issue #660 の改善候補 P1: (c) の「hybrid でない」は、hybrid が
    // `on_visible_row` で疎コーパス（本文 `TEXT` 列）を蓄積する必要があることに
    // 由来する条件だった。`SparseIndexCache`（Issue #357）がヒットした場合
    // （`skip_sparse_accumulation`）はその蓄積自体が省略される
    // （`on_visible_row` 本体の `if is_hybrid && !skip_sparse_accumulation`）ため、
    // hybrid でも SCALAR 段は恒等写像になり高速経路へ載せられる。ヒットした
    // 疎索引の `DocId` は構築時のスロット番号であり、高速経路が借用する
    // スナップショットのスロット番号と同一（同一 `(table, ctx)` × 同一テーブル
    // 世代のキャッシュキーで揃う。複製経路でも `build_from_cached_rls_rows` は
    // 全行を順に通すためスロット写像は恒等）。`skip_sparse_accumulation` が偽
    // （疎キャッシュミス・`sparse_cache` 非配線）の場合は従来どおり複製経路。
    //
    // 投影が候補スカラー列を参照する場合（`needed_column_indices` が非空）は
    // `defer_projection`（Issue #453・P2 で hybrid へも拡張）が Top-k 確定後の
    // 遅延デコードを担うため、高速経路側に追加条件は要らない
    // （`defer_projection` と本条件はいずれも同じ「SCALAR 段が恒等写像」を
    // 判定しており、hybrid 時の `skip_sparse_accumulation` 要求も共通）。
    let cache_fast_path_eligible =
        !bound.has_where_filters() && (!is_hybrid || skip_sparse_accumulation);

    // Issue #474: SCALAR 事前フィルタの索引対応述語形状の静的判定
    // （`sql::scalar_plan::classify_scalar_plan`）。索引の gated 構築（下記）と
    // 候補削減（「ヒットだが SCALAR 段に実質的な処理がある」分岐）の双方が参照
    // するため、`bound.metadata_filters`／`expr_filters` が変わらないこの
    // クエリ内で 1 回だけ計算する（`sql::hnsw_cache::classify_ann_plan` を
    // 一度だけ呼ぶ既存の流儀と同じ）。
    let scalar_plan_kind =
        crate::sql::scalar_plan::classify_scalar_plan(&crate::sql::scalar_plan::ScalarShapeInput {
            scalar_prefilter: plan.scalar_prefilter,
            metadata_filters: &bound.metadata_filters,
            expr_filters: &bound.expr_filters,
            or_filters: &bound.or_filters,
        });

    let rls_hook = ImplicitRlsHook::new(ctx);
    // 借用元を関数スコープ末尾まで生かすための保持先（`arena` はこのいずれかを
    // 指す `&VectorArena` になる。ミス／フォールバック経路は `owned_arena` へ、
    // 高速経路は `cache_hit_snapshot`〔`Arc` でキャッシュ本体と共有〕へ格納する）。
    // `Option::insert` で値を格納しつつそのまま `&mut T`（ここでは即座に `&T` へ
    // 落とす）を受け取ることで、受信データ経路での `unwrap`/`expect` 禁止
    // （.claude/rules/coding-rust.md）に抵触する「直後に取り出す」形の
    // `expect("just assigned")` を書かずに済む。
    let mut owned_arena: Option<VectorArena> = None;
    let mut cache_hit_snapshot: Option<std::sync::Arc<crate::sql::arena_cache::SqlArenaSnapshot>> =
        None;
    // Issue #473: スカラー列二次索引（`scalar_index::ScalarIndex`）の gated 構築
    // （§4.4）が使う `SqlArenaSnapshot` の保持先。索引は `arena_cache` 経由で
    // スナップショットが手に入った経路（ヒット・ミスいずれも）でのみ構築する
    // （`arena_cache` が `None` の場合はこのクエリでは構築しない。索引の消費
    // 〔候補削減〕は本 Issue のスコープ外。モジュールドキュメント参照）。
    let mut scalar_snapshot_for_index: Option<
        std::sync::Arc<crate::sql::arena_cache::SqlArenaSnapshot>,
    > = None;
    // Issue #474: 索引消費の候補削減（下記「ヒットだが SCALAR 段に実質的な
    // 処理がある」分岐）が `ScalarIndexCache::lookup` を既に呼んだ場合、その
    // 結果（`Some(index)` を得られたか）をここへ記録する。直後の gated 構築
    // ブロック（`already_cached` の判定）が同じクエリ中に重複して `lookup` を
    // 呼ぶと、`lookup` 自体が副作用として `hits`/`misses` 統計を加算するため、
    // 観測用カウンタが実際の照会回数（1 回）より多く計上されてしまう
    // （`ScalarIndexCacheStats` はテナント存在情報を含まない機微性の低い値だが、
    // 二重計上は観測契約を破る。`tests/scalar_index_cache.rs` が固定）。
    let mut scalar_index_lookup_hit: Option<bool> = None;
    // Issue #654: 候補削減が「絞る」だけに使われ、`VectorArena` への複製
    // （`build_from_cached_rls_rows_subset`）を経ずキャッシュ済みスナップショットの
    // `VectorArena` を借用したままスロットマスクで直接探索できた場合、通過した
    // 元スロット番号（`arena`＝`snapshot.arena()` に対する添字。狭義昇順）を
    // ここへ記録する。`Some` の場合、下の `slot_ids` 生成・DISTANCE 段は
    // `provider.search`（複製前提）ではなく `provider.search_subset`（マスク）を
    // 使う。対象は `Ranking::Distance`（`!is_hybrid`）——HNSW `Subset` 形状
    // （`hnsw_subset_eligible`）も含む（Issue #676。DISTANCE 段が
    // `sql::hnsw_cache::prepare_subset_from_slots` で plain scan 縮退と判明した
    // 場合のみ本フィールドをそのまま使い、ANN 探索へ進む場合はそこで初めて
    // `build_from_cached_rls_rows_subset` を呼び複製する。hybrid `Subset` 形状
    // （`hnsw_hybrid_subset_eligible`）は対象外のまま従来どおり複製経路。
    // モジュールドキュメント冒頭・`docs/design/scalar-index-mask-search.md`・
    // `docs/design/hnsw-rls-cardinality-switch.md` 参照）。
    let mut mask_kept_slots: Option<Vec<u32>> = None;
    // Issue #660 系（Step 4）: 索引信頼マスク経路（下記）を通った場合に立てる。
    // この経路は `on_visible_row` を一度も呼ばないため `candidate_columns` が
    // 空のままになる。投影がスカラー列を参照する場合の取得元は
    // `defer_projection`（Issue #453）と同じ `ScalarSource::Deferred`
    // （キャッシュヒット済みスナップショットの metadata をスロット添字で引く）
    // に委ねる必要があるため、投影段の分岐で `defer_projection` と同値に扱う。
    // `defer_projection` 自体は `WHERE` なしを要求するためこの経路では常に偽。
    let mut mask_trusted_defer = false;
    let arena: &VectorArena = match arena_cache {
        None => owned_arena.insert(
            VectorArena::build_filtered_with_rows_in_txn(
                read_txn,
                &bound.table,
                rls_hook.predicate(),
                on_visible_row,
            )
            .map_err(|e| map_arena_error(&bound.table, e))?,
        ),
        Some(access) => {
            let sql_cache = access.cache;
            let storage = access.storage;
            if let Some(snapshot) = sql_cache.lookup(storage, read_txn, &bound.table, ctx) {
                if cache_fast_path_eligible {
                    // 高速経路: SCALAR 段が恒等写像なので `on_visible_row` を
                    // 一切呼ばず（呼んでも常に `Ok(true)` を返すだけで副作用が
                    // ない）、`candidate_columns` だけスロット数分の空 `Vec` で
                    // 埋める（`on_visible_row` 本体の「`needed_column_indices` が
                    // 空なら `Vec::new()` を積むだけ」という既存分岐と同じ結果を、
                    // 呼び出しなしで再現する。空 `Vec` はヒープ確保しないため
                    // 行数分の確保コストも発生しない）。
                    candidate_columns
                        .try_reserve_exact(snapshot.arena().len())
                        .map_err(|e| {
                            map_arena_error(
                                &bound.table,
                                ArenaError::AllocationFailed(format!(
                                    "failed to reserve candidate column slots: {e}"
                                )),
                            )
                        })?;
                    for _ in 0..snapshot.arena().len() {
                        candidate_columns.push(Vec::new());
                    }
                    // Issue #453: `cache_hit_snapshot` を `.insert()`（排他借用）
                    // ではなく通常代入 + `.as_ref()`（共有借用）で埋める。投影段
                    // （`defer_projection`）が Top-k 確定後に `cache_hit_snapshot`
                    // の `metadata()` を再度参照するため、`arena` 変数の生存期間中
                    // ずっと排他借用が居座る `.insert()` の戻り値を使うと、後続の
                    // 共有借用（`scalar_source` の組み立て）と競合する（E0502）。
                    // 共有借用同士は共存できるためこの形にする。
                    scalar_snapshot_for_index = Some(std::sync::Arc::clone(&snapshot));
                    // 借用高速経路の観測用カウンタ（`SqlArenaCacheStats::
                    // fast_path_borrows`）。hybrid がこの経路へ載ったことの
                    // 非 vacuous な証跡としてテストが参照する。
                    sql_cache.record_fast_path_borrow();
                    cache_hit_snapshot = Some(snapshot);
                    cache_hit_snapshot
                        .as_ref()
                        .ok_or_else(|| {
                            map_arena_error(
                                &bound.table,
                                ArenaError::AllocationFailed(
                                    "cache hit snapshot missing immediately after assignment"
                                        .to_string(),
                                ),
                            )
                        })?
                        .arena()
                } else {
                    // ヒットだが SCALAR 段に実質的な処理がある。
                    //
                    // Issue #474: `bound.metadata_filters`/`expr_filters` が
                    // 索引対応述語（`sql::scalar_plan::classify_scalar_plan`）
                    // のみからなる場合、`ScalarIndex::resolve_candidates` が
                    // 削減した候補スロットだけを
                    // `build_from_cached_rls_rows_subset` へ渡し、行単位の
                    // `scan_scalar_columns`/`matches_all`/式述語評価
                    // （`on_visible_row`）を可視行全件ではなく候補行のみに
                    // 適用する。非対応形状・索引未消費（列未索引・`id_index`
                    // が `None`・選択度超過・索引↔スナップショット同一性
                    // 不一致のいずれか）の場合は、従来どおりキャッシュ済み
                    // スナップショットへ `on_visible_row` を全行再適用する
                    // （モジュールドキュメント「RLS → SCALAR → DISTANCE」の
                    // 責務境界・クエリ結果はいずれの経路でも不変。索引は
                    // 候補行を「絞る」ことしかできず「通す」ことはできない
                    // ため、`on_visible_row` を省略しない）。
                    let expected_dim = schema
                        .vector_dim()
                        .ok_or(ArenaError::InvalidDim)
                        .map_err(|e| map_arena_error(&bound.table, e))?;
                    scalar_snapshot_for_index = Some(std::sync::Arc::clone(&snapshot));

                    let mut index_candidate_slots: Option<Vec<u32>> = None;
                    if scalar_plan_kind != crate::sql::scalar_plan::ScalarPlan::PlainScan {
                        if let Some(scalar_access) = scalar_cache.as_ref() {
                            let looked_up = scalar_access.cache.lookup(
                                scalar_access.storage,
                                read_txn,
                                &bound.table,
                                ctx,
                            );
                            scalar_index_lookup_hit = Some(looked_up.is_some());
                            if let Some(index) = looked_up {
                                // 索引↔スナップショット同一性ガード（ADR
                                // 「実行時の同一性検査」節）: `ScalarIndexCache`
                                // のキーは `(table, ctx)` × 世代でありこの
                                // クエリの `snapshot` そのものの同一性ではない
                                // ため、行数・構築世代の両方が一致する場合に
                                // 限って候補を使う。不一致時は「絞る」経路を
                                // 使わず安全側（全走査）へ縮退するだけで
                                // クエリの正しさには影響しない。
                                if index.row_count() == snapshot.arena().len()
                                    && index.built_table_generation()
                                        == snapshot.built_table_generation_for_index()
                                {
                                    let id_preds: Vec<crate::sql::scalar_plan::IdPredicate> = bound
                                        .expr_filters
                                        .iter()
                                        .filter_map(crate::sql::scalar_plan::id_predicate_from_expr)
                                        .collect();
                                    // 数値・日時・`NUMERIC`・`UUID` 列の範囲
                                    // 述語（`FilterOp::TypedCompare`。Issue
                                    // #891・TASK-199 で production 結線済み）は
                                    // `bound.metadata_filters` に混在したまま
                                    // 渡り、`ScalarIndex::candidates_for` が
                                    // 内部で振り分ける（Issue #893 production
                                    // 接続）。
                                    match index.resolve_candidates(
                                        &bound.metadata_filters,
                                        &id_preds,
                                    ) {
                                        crate::sql::scalar_index::CandidateResolution::Use(
                                            slots,
                                        ) => {
                                            index_candidate_slots = Some(slots);
                                        }
                                        crate::sql::scalar_index::CandidateResolution::FallbackNoIndex
                                        | crate::sql::scalar_index::CandidateResolution::FallbackSelectivity => {
                                            scalar_access.cache.record_plain_scan_fallback();
                                        }
                                    }
                                } else {
                                    scalar_access.cache.record_plain_scan_fallback();
                                }
                            }
                        }
                    }

                    // Issue #654・#676: 候補削減が発火し（`index_candidate_slots`
                    // が `Some`）、かつこのクエリが `Ranking::Distance` の場合、
                    // `VectorArena` への複製を経ずキャッシュ済みスナップショットの
                    // `VectorArena` をそのまま借用し、`filter_cached_rls_rows_subset`
                    // が返す元スロット番号だけを DISTANCE 段の候補マスクとして使う
                    // （§モジュールドキュメント「Issue #654」参照）。HNSW `Subset`
                    // 経路（`hnsw_subset_eligible`）も Issue #676 でここへ合流した
                    // ——DISTANCE 段が `sql::hnsw_cache::prepare_subset_from_slots`
                    // で plain scan 縮退と判定した場合はこのマスクをそのまま使い、
                    // ANN 探索へ進む場合はそこで初めて複製する（下の DISTANCE 段
                    // 参照）。hybrid の `Subset` 形状（疎コーパスの `DocId` が
                    // スロット番号に依存するため）のみ対象外で従来どおり複製経路
                    // を使う。
                    if let Some(slots) = index_candidate_slots.take() {
                        if !is_hybrid {
                            // Issue #660 系（Step 4）: 索引の候補集合を一致集合と
                            // して信頼し、候補スロットへの `on_visible_row` 再適用
                            // （masked decode ＋ `matches_all` ＋ 式述語評価）を
                            // 省く。信頼してよい不変条件は
                            // `sql::aggregate::count_star_only` のドキュメント
                            // （1〜3）と同型で、DISTANCE 経路では次をすべて満たす
                            // 場合に限る:
                            //
                            // 1. `WHERE` が索引で完全被覆されている——この分岐は
                            //    `scalar_plan_kind != PlainScan`（残余述語ゼロが
                            //    `sql::scalar_plan` の契約により静的に保証される）
                            //    かつ `resolve_candidates` が `Use` を返した場合
                            //    にしか到達しない。
                            // 2. 索引↔スナップショット同一性ガード（行数・構築
                            //    世代の一致）を通過している（直上の分岐で検査済み）。
                            // 3. hybrid でない（疎コーパスの `DocId` 割当が絡まない）
                            //    かつ HNSW `Subset` 形状でない——後者は ANN 探索へ
                            //    進む場合に `candidate_columns` をスロット写像
                            //    （`ScalarSource::EagerSubset`）で引く既存契約
                            //    （Issue #676）があり、本 Step の対象外として
                            //    従来どおり再適用経路を通す。
                            //
                            // 崩れた場合は下の従来経路（`filter_cached_rls_rows_
                            // subset` による候補行への `on_visible_row` 再適用）へ
                            // 落ちるだけで、クエリの正しさ・テナント境界は不変。
                            let kept = if !hnsw_subset_eligible {
                                if let Some(scalar_access) = scalar_cache.as_ref() {
                                    scalar_access.cache.record_index_trusted_mask_scan();
                                }
                                // 投影段は常に `ScalarSource::Deferred(Snapshot)`
                                // 側へ倒す。`EagerSubset` は
                                // `candidate_columns` が候補行ぶん埋まっている
                                // ことを前提に `kept_slots` 経由で引く契約
                                // （Issue #654）であり、この経路では
                                // `on_visible_row` を一度も呼ばないため空のまま
                                // ＝ fail-closed な内部エラーになる。投影列が
                                // 無い（`SELECT id`）場合も `needed_mask` が
                                // 全 false になるだけで Top-k 行のデコードは
                                // 実質発生しない（`tests/scalar_index_mask_search.rs::
                                // trusted_mask_matches_plain_scan_for_each_predicate_shape`
                                // の `SELECT id` ケースが固定する）。
                                mask_trusted_defer = true;
                                slots
                            } else {
                                VectorArena::filter_cached_rls_rows_subset(
                                    expected_dim,
                                    snapshot.arena(),
                                    snapshot.metadata(),
                                    &slots,
                                    on_visible_row,
                                    crate::arena::MAX_ARENA_ROWS,
                                    crate::arena::MAX_ARENA_TOTAL_BYTES,
                                )
                                .map_err(|e| map_arena_error(&bound.table, e))?
                            };
                            if let Some(scalar_access) = scalar_cache.as_ref() {
                                scalar_access.cache.record_index_scan();
                                scalar_access.cache.record_index_mask_scan();
                            }
                            mask_kept_slots = Some(kept);
                            cache_hit_snapshot = Some(snapshot);
                            cache_hit_snapshot
                                .as_ref()
                                .ok_or_else(|| {
                                    map_arena_error(
                                        &bound.table,
                                        ArenaError::AllocationFailed(
                                            "cache hit snapshot missing immediately after assignment"
                                                .to_string(),
                                        ),
                                    )
                                })?
                                .arena()
                        } else {
                            let built = VectorArena::build_from_cached_rls_rows_subset(
                                &bound.table,
                                expected_dim,
                                snapshot.arena(),
                                snapshot.metadata(),
                                &slots,
                                on_visible_row,
                                crate::arena::MAX_ARENA_ROWS,
                                crate::arena::MAX_ARENA_TOTAL_BYTES,
                            )
                            .map_err(|e| map_arena_error(&bound.table, e))?;
                            if let Some(scalar_access) = scalar_cache.as_ref() {
                                scalar_access.cache.record_index_scan();
                            }
                            owned_arena.insert(built)
                        }
                    } else {
                        // 可視全行の複製経路（`SqlArenaCacheStats::
                        // full_rebuild_copies`）。高速経路が働いていれば
                        // hybrid ではこのカウンタが増えない。
                        sql_cache.record_full_rebuild_copy();
                        let built = VectorArena::build_from_cached_rls_rows(
                            &bound.table,
                            expected_dim,
                            snapshot.arena(),
                            snapshot.metadata(),
                            on_visible_row,
                            crate::arena::MAX_ARENA_ROWS,
                            crate::arena::MAX_ARENA_TOTAL_BYTES,
                        )
                        .map_err(|e| map_arena_error(&bound.table, e))?;
                        owned_arena.insert(built)
                    }
                }
            } else {
                // ミス: 従来どおり redb を走査するが、`rls_capture` で RLS 通過行
                // （SCALAR 段の判定結果に関係なく全件）を同一走査の中で同時に採取し、
                // 次回以降のクエリが再利用できるスナップショットとしてキャッシュへ
                // 登録する（`SqlArenaCaptureBuilder` のドキュメント参照。容量超過等で
                // 採取を断念した場合はキャッシュへ反映しないだけで、このクエリ自体の
                // 応答には影響しない）。
                let expected_dim = schema
                    .vector_dim()
                    .ok_or(ArenaError::InvalidDim)
                    .map_err(|e| map_arena_error(&bound.table, e))?;
                // `max_bytes`（第 3 引数）・`metadata_byte_cap`（第 4 引数）はいずれも
                // `MAX_ARENA_TOTAL_BYTES` を渡すが、クエリ本体（応答用アリーナ）と
                // 独立した別枠の予算ではない。`SqlArenaCaptureBuilder::push` へ渡す
                // `external_bytes_in_use`（応答用アリーナの現在使用量）と合算した
                // 単一のピーク予算として扱われる（Issue #379 codex-review P1 対応。
                // 型・`push` のドキュメント参照。以前は応答用アリーナ・採取用
                // embedding・採取用 metadata がそれぞれ独立に `MAX_ARENA_TOTAL_BYTES`
                // まで確保でき、単一クエリのピークメモリが最大で約 3 倍〔付随バッファ
                // 除く〕に膨らみ得た）。
                let mut capture = crate::arena::SqlArenaCaptureBuilder::new(
                    expected_dim,
                    crate::arena::MAX_ARENA_ROWS,
                    crate::arena::MAX_ARENA_TOTAL_BYTES,
                    crate::arena::MAX_ARENA_TOTAL_BYTES,
                );
                let mut rls_capture = |id: u64,
                                       tenant_id: &str,
                                       visibility,
                                       embedding: &[f32],
                                       metadata: &[u8],
                                       response_arena_bytes_in_use: usize|
                 -> std::result::Result<(), ArenaError> {
                    capture.push(
                        id,
                        tenant_id,
                        visibility,
                        embedding,
                        metadata,
                        response_arena_bytes_in_use,
                    );
                    Ok(())
                };
                let built = VectorArena::build_filtered_with_rows_in_txn_capturing(
                    read_txn,
                    &bound.table,
                    rls_hook.predicate(),
                    on_visible_row,
                    &mut rls_capture,
                )
                .map_err(|e| map_arena_error(&bound.table, e))?;
                // キャッシュ登録用のテーブル世代は、クエリ実行そのものに使った
                // `read_txn`（＝この走査が観測したスナップショット）から読む。
                // `SqlArenaCache::insert` 側が挿入直前に別途フレッシュな世代と
                // 再照合するため、ここでの読み取りは「採取したスナップショットが
                // どの世代由来か」を記録するだけでよい。読み取り自体に失敗した場合は
                // キャッシュへ登録せず、クエリ応答（`built`）はそのまま返す
                // （fail-closed: 判定できないなら常駐させない。クエリ自体は失敗させない）。
                if let Ok(built_table_generation) =
                    crate::catalog::table_generation_in_txn(read_txn, &bound.table)
                {
                    if let Some((cache_arena, cache_metadata)) = capture.finish(&bound.table) {
                        let snapshot = crate::sql::arena_cache::SqlArenaSnapshot::new(
                            cache_arena,
                            cache_metadata,
                            ctx.clone(),
                            built_table_generation,
                        );
                        // Issue #473: `sql_cache.insert` は世代整合済みか否かに
                        // 関わらず常に構築済みスナップショットの `Arc` を返す
                        // （`SqlArenaCache::insert` のドキュメント参照。呼び出し元
                        // がこのクエリ自身の `read_txn` から構築した結果のため、
                        // このクエリ限りで使う分には stale にならない）。従来
                        // 捨てていた戻り値をスカラー索引の構築材料として保持する。
                        let inserted = sql_cache.insert(storage, &bound.table, ctx, snapshot);
                        scalar_snapshot_for_index = Some(inserted);
                    }
                }
                owned_arena.insert(built)
            }
        }
    };

    // Issue #473: スカラー列二次索引（`scalar_index::ScalarIndex`）の gated 構築。
    // Issue #474 で建てゲート条件を `bound.metadata_filters` の非空判定から
    // `classify_scalar_plan(..) != PlainScan` へ拡張した（`id` 単純比較のみ
    // （`metadata_filters` は空）のクエリでも索引が構築されないと、上記の
    // 候補削減分岐が `lookup` で永久にミスし続け索引を一切消費できないため）。
    // `arena_cache` 経由で得たスナップショットから索引を構築しキャッシュへ登録
    // する。構築済み索引は「ヒットだが SCALAR 段に実質的な処理がある」分岐
    // （上記）で候補削減にも消費される。構築・登録の失敗はこのクエリを失敗させ
    // ない（fail-soft な派生キャッシュ。`scalar_index::ScalarIndexCache` の
    // ドキュメント参照）。
    if let (Some(scalar_access), Some(snapshot_for_scalar)) =
        (scalar_cache.as_ref(), scalar_snapshot_for_index.as_ref())
    {
        if scalar_plan_kind != crate::sql::scalar_plan::ScalarPlan::PlainScan {
            // Issue #474: 上記の候補削減分岐が同じクエリ中に既に `lookup` を
            // 呼んでいれば（`scalar_index_lookup_hit`）その結果を再利用し、
            // `lookup` の重複呼び出しによる `hits`/`misses` 統計の二重計上を
            // 避ける（呼んでいなければ通常どおり照会する）。
            let already_cached = scalar_index_lookup_hit.unwrap_or_else(|| {
                scalar_access
                    .cache
                    .lookup(scalar_access.storage, read_txn, &bound.table, ctx)
                    .is_some()
            });
            if !already_cached {
                match crate::sql::scalar_index::ScalarIndex::build(schema, snapshot_for_scalar) {
                    Ok(index) => {
                        let _ = scalar_access.cache.insert(
                            scalar_access.storage,
                            &bound.table,
                            ctx,
                            index,
                        );
                    }
                    Err(_) => {
                        scalar_access.cache.record_build_failure();
                    }
                }
            }
        }
    }

    // provider へ渡す id は行 `id` ではなく**アリーナのスロット番号**（0..n）にする。
    // 行 `id` の一意性スコープはテナント内（対象ビヘイビア: TABLE-12）であり、1 つの
    // 可視集合に同じ `id` の行が複数含まれうるため、`id` のままでは
    // (a) provider 結果の重複検証、(b) RRF 融合の結合キー、(c) 投影段の行同定 の
    // いずれもが別テナントの行を取り違えうる（Bugbot High/Medium 指摘）。スロット番号は
    // 可視集合内で一意かつ `(tenant_id, id)` の行と 1 対 1 に対応するため、これらの
    // 契約が構造的に回復する。クライアントへ返す `ResultRow.id` は投影段で
    // `arena.ids()[slot]`（本来の行 id）へ戻す。
    // Issue #654: `mask_kept_slots` が `Some` の場合、`arena` はキャッシュ済み
    // スナップショットの `VectorArena` を丸ごと借用したまま（複製していない）で
    // あり、候補は `mask_kept_slots` が指す元スロット番号だけに絞る。`None` の
    // 場合は従来どおり `arena` の全スロット（`0..arena.ids().len()`）を候補にする
    // （`arena` 自体が候補選択済みの縮約アリーナであるため）。
    let mut slot_ids: Vec<u64> = Vec::new();
    match mask_kept_slots.as_ref() {
        Some(kept) => {
            slot_ids
                .try_reserve_exact(kept.len())
                .map_err(|e| SqlSurfaceError::Internal {
                    detail: format!("failed to reserve candidate slot ids: {e}"),
                })?;
            for &slot in kept {
                slot_ids.push(slot as u64);
            }
        }
        None => {
            slot_ids.try_reserve_exact(arena.ids().len()).map_err(|e| {
                SqlSurfaceError::Internal {
                    detail: format!("failed to reserve candidate slot ids: {e}"),
                }
            })?;
            for slot in 0..arena.ids().len() {
                let slot_id = u64::try_from(slot).map_err(|_| SqlSurfaceError::Internal {
                    detail: "candidate slot index does not fit in u64".to_string(),
                })?;
                slot_ids.push(slot_id);
            }
        }
    }
    // スロット番号は重複しないため、多重集合の各件数は必ず 1 になる
    // （`core::provider_result_is_valid` の (3)(4) 検証はそのまま使える）。
    let visible_id_counts = core::visible_id_counts(&slot_ids);

    // `precision` 時は DISTANCE 段の取得件数を `bound.limit` より広く取る
    // （`LIMIT 1` でも Top-2 を取得し、`precision::apply_gate` のマージン判定を
    // 行えるようにする。`crate::precision` モジュールドキュメント参照）。`recall`
    // 時は従来どおり `bound.limit`。
    //
    // `!plan.scalar_prefilter`（DISTANCE 先行・SCALAR 事後フィルタ経路）では
    // `bound.limit.max(2)` 件だけを取得すると、事後フィルタで候補が間引かれた
    // 結果「Top-2 が最初から存在しなかった」のか「取得件数不足で Top-2 を
    // 取りこぼしただけ」なのかを区別できない。後者を前者と誤認すると
    // `precision::apply_gate` が「Top-2 不在＝マージン条件成立」と誤判定し、
    // 本来は僅差で拒否すべき候補を通す fail-open 経路になる（codex-review・
    // Bugbot 指摘。SEARCH-9 は確信度不足時の fail-closed を要求する）。この
    // 経路のみ可視集合全体（`arena.ids().len()`。`ImplicitRlsHook` で RLS 済み）
    // を取得し、事後フィルタ後の残存件数が「WHERE を満たす候補の完全な順位列」
    // になるようにする。`core::MAX_SEARCH_K` でクランプする（下記 `const _` で
    // `2 <= core::MAX_SEARCH_K` を固定済みのため `k_eff` は常に provider・RRF
    // 設定の許容範囲に収まる）。
    let k_eff = if is_precision {
        if plan.scalar_prefilter {
            bound.limit.max(2)
        } else {
            arena.ids().len().clamp(2, core::MAX_SEARCH_K)
        }
    } else {
        bound.limit
    };

    // `!plan.scalar_prefilter`（DISTANCE 先行・SCALAR 事後フィルタ）の precision
    // 経路で `arena.ids().len()` が `core::MAX_SEARCH_K` を超える場合、直上の
    // `k_eff` は `MAX_SEARCH_K` へクランプされ、DISTANCE 段は可視集合全体ではなく
    // 先頭 `MAX_SEARCH_K` 件しか取得できない（provider の `k` 契約上限）。この場合
    // `MAX_SEARCH_K` 件目より後ろに WHERE を満たす僅差の Top-2 相当が存在しても
    // 取得できず、SCALAR 事後フィルタ後に「Top-2 を取りこぼしただけ」なのか
    // 「Top-2 が最初から存在しない」のかを再び区別できなくなる（直上のコメントが
    // 解消しようとした fail-open の再発。codex-review 指摘・PRRT_kwDOUAKASM6cPLHE）。
    // 「WHERE を満たす候補の完全な順位列を取得できる」という本経路の前提が崩れる
    // 以上、DISTANCE 検索自体を実行せず空集合へ倒す（`crate::precision` モジュール
    // ドキュメントが定める「確信度が判定できない＝空集合の通常応答」という既存の
    // fail-closed パターンに合わせる。完全性を保証できないこと自体は仕様上の
    // fail-closed 応答であり `SqlSurfaceError` への昇格対象ではない）。
    let completeness_unbounded =
        precision_completeness_unbounded(is_precision, plan.scalar_prefilter, arena.ids().len());

    // DISTANCE 段。
    let hits: Vec<(u64, f64)> = if completeness_unbounded {
        Vec::new()
    } else {
        match &bound.ranking {
            Ranking::Distance { query } => {
                // Issue #408・#409: `hnsw_cache`（`None` の場合は `execute_statement`
                // 経由。従来どおり全件 brute-force のみ）が `Some` の場合、適用条件
                // （`hnsw_full_visible_eligible`／`hnsw_subset_eligible` 参照）に応じて
                // `FullVisible`（索引済みノード＋未索引分 brute-force の併用。#408）
                // または `Subset`（per-query 写像・キャッシュ非登録。#409）の経路を
                // 試みる。
                let raw = if hnsw_full_visible_eligible {
                    match hnsw_cache.as_ref() {
                        Some(access) => crate::sql::hnsw_cache::search_or_fallback(
                            access,
                            read_txn,
                            &bound.table,
                            ctx,
                            arena,
                            &slot_ids,
                            provider,
                            query,
                            k_eff,
                        )
                        .map_err(map_kernel_error)?,
                        None => {
                            let input = SearchInput {
                                ids: &slot_ids,
                                vectors: arena.vectors(),
                                dim: arena.dim(),
                                query,
                                k: k_eff,
                            };
                            provider.search(input).map_err(map_kernel_error)?
                        }
                    }
                } else if hnsw_subset_eligible {
                    // Issue #676: SCALAR 段の候補削減（Issue #654）が `VectorArena`
                    // を複製せず借用したまま候補スロットだけを絞り込めていた場合
                    // （`mask_kept_slots`／`cache_hit_snapshot` が揃っている場合）、
                    // まず `sql::hnsw_cache::prepare_subset_from_slots` で
                    // 複製する**前**に plain scan／ANN を判定する。plain scan なら
                    // 複製せず候補 id マスク経路（`search_subset`）へ委譲し、ANN へ
                    // 進む場合のみこの時点で初めて `build_from_cached_rls_rows_subset`
                    // を呼んで複製する（従来どおりの経路）。索引が候補削減を発火
                    // できなかった場合（`mask_kept_slots` が `None`）は、従来どおり
                    // `search_subset_or_fallback`（`arena` は行フィルタ済みの複製
                    // アリーナ）を使う。
                    match (
                        hnsw_cache.as_ref(),
                        mask_kept_slots.as_ref(),
                        cache_hit_snapshot.as_ref(),
                    ) {
                        (Some(access), Some(kept), Some(snapshot)) => {
                            match crate::sql::hnsw_cache::prepare_subset_from_slots(
                                access,
                                read_txn,
                                &bound.table,
                                ctx,
                                snapshot.arena(),
                                kept,
                                k_eff,
                            ) {
                                crate::sql::hnsw_cache::SubsetSlotPlan::MaskScan => provider
                                    .search_subset(crate::kernel::SubsetSearchInput {
                                        slots: kept,
                                        vectors: snapshot.arena().vectors(),
                                        dim: snapshot.arena().dim(),
                                        query,
                                        k: k_eff,
                                    })
                                    .map_err(map_kernel_error)?,
                                crate::sql::hnsw_cache::SubsetSlotPlan::Ann(prepared) => {
                                    // ANN 探索へ進む場合のみ、ここで初めて `kept` から
                                    // subset アリーナを複製する（従来の HNSW `Subset`
                                    // 経路と同じ複製。`sql/exec.rs` モジュールドキュメント
                                    // 「Issue #654」節・`docs/design/
                                    // scalar-index-mask-search.md`「Issue #676」節参照）。
                                    let built = VectorArena::build_from_cached_rls_rows_subset(
                                        &bound.table,
                                        snapshot.arena().dim(),
                                        snapshot.arena(),
                                        snapshot.metadata(),
                                        kept,
                                        |_, _, _, _| Ok(true),
                                        crate::arena::MAX_ARENA_ROWS,
                                        crate::arena::MAX_ARENA_TOTAL_BYTES,
                                    )
                                    .map_err(|e| map_arena_error(&bound.table, e))?;
                                    access.cache.record_subset_arena_copy();
                                    let subset_slot_ids: Vec<u64> =
                                        (0..built.ids().len() as u64).collect();
                                    let raw_ann = crate::sql::hnsw_cache::search_prepared(
                                        access,
                                        &prepared,
                                        provider,
                                        &built,
                                        &subset_slot_ids,
                                        query,
                                        k_eff,
                                    )
                                    .map_err(map_kernel_error)?;
                                    // `search_prepared` が返す `id` は subset アリーナ
                                    // 内の連番（ordinal）——`Overlay::compute_over_slots`
                                    // が `kept` の列挙順で座標系を作った（§`Overlay::
                                    // compute_over_slots` ドキュメンテーションコメント
                                    // 参照）ため、`kept.get(ordinal)` で元スロット番号
                                    // （`snapshot.arena()` に対する添字）へ写像し戻す。
                                    // 範囲外は索引・マージロジックの不整合を意味するため
                                    // 黙って落とさず fail-closed に拒否する。
                                    let mut mapped = Vec::with_capacity(raw_ann.len());
                                    for hit in raw_ann {
                                        let ordinal = usize::try_from(hit.id).map_err(|_| {
                                            SqlSurfaceError::Internal {
                                                detail: "hnsw subset ordinal does not fit in usize"
                                                    .to_string(),
                                            }
                                        })?;
                                        let orig_slot = kept.get(ordinal).copied().ok_or(
                                            SqlSurfaceError::Internal {
                                                detail:
                                                    "hnsw subset ordinal out of range of candidate slots"
                                                        .to_string(),
                                            },
                                        )?;
                                        mapped.push(crate::kernel::CandidateHit {
                                            id: orig_slot as u64,
                                            score: hit.score,
                                        });
                                    }
                                    mapped
                                }
                            }
                        }
                        _ => match hnsw_cache.as_ref() {
                            Some(access) => crate::sql::hnsw_cache::search_subset_or_fallback(
                                access,
                                read_txn,
                                &bound.table,
                                ctx,
                                arena,
                                &slot_ids,
                                provider,
                                query,
                                k_eff,
                            )
                            .map_err(map_kernel_error)?,
                            None => {
                                let input = SearchInput {
                                    ids: &slot_ids,
                                    vectors: arena.vectors(),
                                    dim: arena.dim(),
                                    query,
                                    k: k_eff,
                                };
                                provider.search(input).map_err(map_kernel_error)?
                            }
                        },
                    }
                } else if let Some(kept) = mask_kept_slots.as_ref() {
                    // Issue #654: `arena` はキャッシュ済みスナップショットの
                    // `VectorArena` を丸ごと借用したまま（複製していない）。`slot_ids`
                    // （＝`kept`）は `arena.vectors()` に対する行番号そのものであり、
                    // 位置ベースの `SearchInput`（`ids[idx]` ではなく配列中の位置
                    // `idx` で `vectors` の行を決める契約。`kernel.rs::CpuScalarProvider`
                    // 参照）へそのまま渡すと誤った行を候補にしてしまう。行番号を
                    // 明示的な候補マスクとして扱う `search_subset` を使う。
                    provider
                        .search_subset(crate::kernel::SubsetSearchInput {
                            slots: kept,
                            vectors: arena.vectors(),
                            dim: arena.dim(),
                            query,
                            k: k_eff,
                        })
                        .map_err(map_kernel_error)?
                } else {
                    let input = SearchInput {
                        ids: &slot_ids,
                        vectors: arena.vectors(),
                        dim: arena.dim(),
                        query,
                        k: k_eff,
                    };
                    provider.search(input).map_err(map_kernel_error)?
                };
                if !core::provider_result_is_valid(&raw, k_eff, &visible_id_counts) {
                    return Err(SqlSurfaceError::Internal {
                        detail: "search provider returned a result violating the top-k contract"
                            .to_string(),
                    });
                }
                raw.into_iter().map(|h| (h.id, h.score as f64)).collect()
            }
            Ranking::Hybrid {
                query,
                query_text,
                text_column_index: hybrid_text_column_index,
                ..
            } => {
                let pool_depth = k_eff.max(DEFAULT_HYBRID_POOL_DEPTH);
                let cfg = RrfConfig::new(60.0, 1.0, 1.0, pool_depth).map_err(|_| {
                    SqlSurfaceError::Internal {
                        detail: "invalid hybrid RRF config".to_string(),
                    }
                })?;
                let input = SearchInput {
                    ids: &slot_ids,
                    vectors: arena.vectors(),
                    dim: arena.dim(),
                    query,
                    k: k_eff,
                };
                // Issue #410: hybrid 密側の再取得ループ（下記の密のみ縮退経路・
                // `hybrid::hybrid_search` の両方）へ HNSW を結線するアダプタ。
                // `hnsw_hybrid_full_visible_eligible`／`hnsw_hybrid_subset_eligible`
                // のいずれかが真かつ `hnsw_cache` が `Some` の場合のみ構築する。
                // `prepare_full_visible`／`prepare_subset`（`Overlay::compute`／
                // `IndexedBase::build` を伴う重い解決）はここで 1 回だけ呼び、
                // 複数ラウンドにわたる `k` 依存の探索は `HnswDenseProvider::search`
                // が都度 `search_prepared` で担う（`sql::hnsw_hybrid` モジュール
                // ドキュメント参照）。`hybrid.rs`・`SearchProvider` trait は無変更。
                let hnsw_dense_provider: Option<crate::sql::hnsw_hybrid::HnswDenseProvider<'_>> =
                    if hnsw_hybrid_full_visible_eligible {
                        hnsw_cache.as_ref().map(|access| {
                            let prepared = crate::sql::hnsw_cache::prepare_full_visible(
                                access,
                                read_txn,
                                &bound.table,
                                ctx,
                                arena,
                            );
                            crate::sql::hnsw_hybrid::HnswDenseProvider::new(
                                access, arena, &slot_ids, provider, prepared,
                            )
                        })
                    } else if hnsw_hybrid_subset_eligible {
                        hnsw_cache.as_ref().map(|access| {
                            let prepared = crate::sql::hnsw_cache::prepare_subset(
                                access,
                                read_txn,
                                &bound.table,
                                ctx,
                                arena,
                            );
                            crate::sql::hnsw_hybrid::HnswDenseProvider::new(
                                access, arena, &slot_ids, provider, prepared,
                            )
                        })
                    } else {
                        None
                    };
                // 索引経路が適用条件を満たさない・`hnsw_cache` が `None`（
                // `SearchEngineKind::Hnsw` opt-in 構築時以外）のいずれの場合も
                // `provider`（従来どおり全件 brute-force）へフォールバックする。
                let dense_provider: &dyn SearchProvider = match &hnsw_dense_provider {
                    Some(adapter) => adapter,
                    None => provider,
                };
                // Issue #357: キャッシュヒット済みの索引があればそれをそのまま使う。
                // ミスの場合のみ `sparse_docs`（このクエリのスナップショットから
                // 蓄積した本文）から新規構築し、`sparse_cache_eligible` なら
                // キャッシュへの挿入を試みる（`SparseIndexCache::insert` は挿入の
                // 成否に関わらず構築済みの索引そのものを返す契約。
                // `sql/sparse_cache.rs` モジュールドキュメント参照）。
                let sparse_index_for_fusion: Option<Arc<SparseIndex>> = if let Some(idx) =
                    &cached_sparse_index
                {
                    Some(Arc::clone(idx))
                } else if sparse_docs.is_empty() {
                    // 疎文書が 0 件（可視行に本文を持つ行が 1 件もない）の場合は
                    // `SparseIndex::build` が空コーパスを拒否する
                    // （`SparseError::EmptyCorpus`）ため、密のみへ縮退させる
                    // （下の `None` 分岐）。空コーパスはキャッシュへも積まない
                    // （`SparseIndex::build` 自体が拒否するため構築できない）。
                    None
                } else {
                    let doc_refs: Vec<(DocId, &str)> = sparse_docs
                        .iter()
                        .map(|(id, text)| (*id, text.as_str()))
                        .collect();
                    let built = SparseIndex::build(&doc_refs)
                        .map_err(HybridError::Sparse)
                        .map_err(map_hybrid_error)?;
                    match (sparse_cache_eligible, sparse_cache.as_ref()) {
                        (true, Some(access)) => {
                            // `built_generation` はこのクエリの `read_txn`
                            // スナップショットにおけるテーブル世代
                            // （`SparseIndexCache::insert` の契約。読み取れない
                            // 場合はキャッシュへ積まず、この場で構築した索引を
                            // そのまま使う）。
                            match crate::catalog::table_generation_in_txn(read_txn, &bound.table) {
                                Ok(built_generation) => Some(access.cache.insert(
                                    access.storage,
                                    &bound.table,
                                    ctx,
                                    *hybrid_text_column_index,
                                    built,
                                    built_generation,
                                )),
                                Err(_) => Some(Arc::new(built)),
                            }
                        }
                        _ => Some(Arc::new(built)),
                    }
                };
                let fused: Vec<HybridHit> = match &sparse_index_for_fusion {
                    None => {
                        // 疎側の寄与を 0 件として扱い、密側のみを `rrf_fuse` に通して
                        // 密のみの順序契約へ縮退させる（密側の可視性検証は
                        // `hybrid::hybrid_search` と同じ理由でここでも行う）。
                        let dense_input = SearchInput {
                            ids: &slot_ids,
                            vectors: arena.vectors(),
                            dim: arena.dim(),
                            query,
                            k: cfg.pool_depth(),
                        };
                        let dense_hits = dense_provider
                            .search(dense_input)
                            .map_err(map_kernel_error)?;
                        // 可視集合への包含判定（件数ではなく所属のみを見るため多重集合の
                        // キー存在で判定する。TABLE-12 の重複 id については
                        // `core::provider_result_is_valid` のドキュメント参照）。
                        if dense_hits
                            .iter()
                            .any(|h| !visible_id_counts.contains_key(&h.id))
                        {
                            return Err(SqlSurfaceError::Internal {
                                detail: "search provider returned a hit outside the visible id set"
                                    .to_string(),
                            });
                        }
                        let mut fused =
                            hybrid::rrf_fuse(&dense_hits, &[], &cfg).map_err(map_hybrid_error)?;
                        fused.truncate(k_eff);
                        fused
                    }
                    Some(idx) => {
                        hybrid::hybrid_search(dense_provider, input, idx, query_text, k_eff, &cfg)
                            .map_err(map_hybrid_error)?
                    }
                };
                // このクエリで観測された密側再取得ラウンド数を `HnswIndexCache` の
                // 統計へ反映する（索引経路が一度も使われなかった場合は no-op。
                // §`HnswDenseProvider::finish` ドキュメンテーションコメント参照）。
                if let Some(adapter) = &hnsw_dense_provider {
                    adapter.finish();
                }
                fused.into_iter().map(|h| (h.id, h.score)).collect()
            }
        }
    };

    // SCALAR 事後フィルタ（DISTANCE 先行時のみ。TASK-76・SQL-7）。`on_visible_row` は
    // `plan.scalar_prefilter == false` の間、等価条件を判定せず可視行を通過させて
    // いるため、ここで `candidate_columns`（`needed_column_indices` により対象列を
    // 保持済み）と突合する。不一致・値の取得不能（データ不整合。fail-closed）は
    // 除去する。返却件数が `limit` 未満になり得る（under-fetch。オーバーサンプルに
    // よる救済は行わない）。
    let hits: Vec<(u64, f64)> = if plan.scalar_prefilter {
        hits
    } else {
        // TASK-79（SQL-9）: `WHERE` の式述語も既存のメタデータフィルタ（TASK-147・
        // EXT-3）と同じ SCALAR 事後フィルタの一部として扱う（DISTANCE 先行時。
        // §モジュールドキュメント参照）。評価に使う embedding は候補選択と同一
        // スナップショットの `arena` から（投影段と同じ経路。再取得なし）。
        // 評価エラーはこの場で fail-closed にクエリ全体の失敗として伝播する
        // （当該行だけを黙ってスキップしない。`on_visible_row` の事前フィルタ経路と
        // 同じ方針）。
        let mut filtered = Vec::with_capacity(hits.len());
        // Issue #353: DISTANCE 段の後で事後適用する `expr_filter_programs` の
        // 明示スタック（`on_visible_row` とは別ループのため個別に確保。行数分
        // 使い回す）。
        let mut expr_scratch: Vec<crate::sql::expr_program::StackValue> = Vec::new();
        for (slot_id, score) in hits {
            // `slot_id` はアリーナのスロット番号（上記参照）。範囲外はデータ不整合
            // として fail-closed に除去する。
            let Some(slot) = usize::try_from(slot_id).ok() else {
                continue;
            };
            let Some(columns) = candidate_columns.get(slot) else {
                continue;
            };
            let scanned: Vec<Option<row_codec::ScalarRef<'_>>> = columns
                .iter()
                .map(|v| match v {
                    Value::Text(t) => Some(row_codec::ScalarRef::Text(t.as_str())),
                    Value::Real(r) => Some(row_codec::ScalarRef::Real(*r)),
                    Value::Double(d) => Some(row_codec::ScalarRef::Double(*d)),
                    Value::Enum(label) => Some(row_codec::ScalarRef::Enum(label.as_str())),
                    Value::Bool(b) => Some(row_codec::ScalarRef::Bool(*b)),
                    Value::Bytes(b) => Some(row_codec::ScalarRef::Bytes(b.as_slice())),
                    Value::Json(t) => Some(row_codec::ScalarRef::Json(t.as_str())),
                    Value::Numeric(d) => Some(row_codec::ScalarRef::Numeric(*d)),
                    Value::Uuid(u) => Some(row_codec::ScalarRef::Uuid(*u)),
                    Value::Date(d) => Some(row_codec::ScalarRef::Date(*d)),
                    Value::Timestamp(t) => Some(row_codec::ScalarRef::Timestamp(*t)),
                    // 配列列は宣言的フィルタ（TEXT 前提）の対象外。`Vector`・
                    // `Integer`／`BigInt` と同じく型不一致として `None` へ倒す
                    // （D-A8）。
                    Value::Null
                    | Value::Vector(_)
                    | Value::Array(_)
                    | Value::Integer(_)
                    | Value::BigInt(_) => None,
                })
                .collect();
            if !declarative_filter::matches_all(&bound.metadata_filters, &scanned) {
                continue;
            }
            // TASK-208・Issue #912: OR 群も式述語と同じく embedding・行 `id` を
            // 要する（式が embedding を参照しうるため）。式述語・OR 群の
            // いずれかが非空の場合にのみ取得する（従来どおり、両方空なら
            // ここで一切参照しない）。
            if !bound.expr_filter_programs.is_empty() || !bound.or_filters.is_empty() {
                let Some(embedding) = arena.vector(slot) else {
                    continue;
                };
                let Some(row_id) = arena.ids().get(slot).copied() else {
                    continue;
                };
                let mut expr_ok = true;
                for program in &bound.expr_filter_programs {
                    match program.eval(row_id, embedding, &mut expr_scratch)? {
                        udf_call::ExprValue::Bool(true) => {}
                        // NULL（UNKNOWN）は非該当として扱う（対象ビヘイビア:
                        // SQL-26。Issue #921）。
                        udf_call::ExprValue::Bool(false) | udf_call::ExprValue::Null => {
                            expr_ok = false;
                            break;
                        }
                        // 束縛段が型を `Bool` に限定済みのため到達しない。
                        _ => {
                            return Err(SqlSurfaceError::Internal {
                                detail: "WHERE expression did not evaluate to a boolean"
                                    .to_string(),
                            })
                        }
                    }
                }
                if expr_ok {
                    for group in &bound.or_filters {
                        if !group.matches(
                            &scanned,
                            row_id,
                            embedding,
                            embedding.len(),
                            &mut expr_scratch,
                        )? {
                            expr_ok = false;
                            break;
                        }
                    }
                }
                if !expr_ok {
                    continue;
                }
            }
            filtered.push((slot_id, score));
        }
        filtered
    };

    // PRECISION ゲート段（TASK-162・SEARCH-9）。SCALAR 事後フィルタの**後**・
    // `RlsSafetyNet::apply` の**前**に置く（`crate::precision` モジュール
    // ドキュメント参照）。`WHERE` を満たす行だけを対象に Top-1／Top-2 の確信度を
    // 見るため SCALAR 段より後、`RlsSafetyNet` は行を「減らす」ことしかしないため
    // ゲート通過後に安全網が行を落としても確信のない行が増える方向にはならず、
    // この位置より前に置く必要がある。候補集合自体は `ImplicitRlsHook` により
    // 事前フィルタ済み（`arena` 構築時）のため、他テナント不可視行が Top-1／Top-2
    // の比較対象に混入することはない。`HINT ORDER` の内容に関係なく無条件に適用
    // する（`is_precision` は `bound.mode` のみに依存し、`plan.scalar_prefilter`
    // の分岐に触れない）。`recall`（既定）はこのブロックを一切通らないため、
    // 挙動は変わらない（SEARCH-1〜8 不変）。
    let hits: Vec<(u64, f64)> = if is_precision {
        // 確信度指標はランキング方式ごとに正規化する（`crate::precision` モジュール
        // ドキュメント参照）: dense はクエリ・候補 embedding の cosine 類似度、
        // hybrid は融合スコアを理論最大値で割った正規化 RRF スコア。確信度は
        // 先頭 `min(limit, max_results) + 1` 件分だけ計算する（有界・小規模。
        // DoS 対策）。
        let thresholds = match &bound.ranking {
            Ranking::Distance { .. } => precision_policy.dense(),
            Ranking::Hybrid { .. } => precision_policy.hybrid(),
        };
        let want = bound
            .limit
            .min(precision_policy.max_results())
            .saturating_add(1);
        let take_n = want.min(hits.len());
        let mut conf: Vec<f64> = Vec::with_capacity(take_n);
        match &bound.ranking {
            Ranking::Distance { query } => {
                for (slot_id, _score) in hits.iter().take(take_n) {
                    let slot =
                        usize::try_from(*slot_id).map_err(|_| SqlSurfaceError::Internal {
                            detail: "candidate slot index out of range".to_string(),
                        })?;
                    let embedding =
                        arena
                            .vector(slot)
                            .ok_or_else(|| SqlSurfaceError::Internal {
                                detail: "candidate arena index out of range".to_string(),
                            })?;
                    // ノルム 0・次元不一致・非有限値は「確信なし」（`0.0`）として
                    // 扱う。`ConfidenceThresholds::new` が閾値を厳密に正へ限定して
                    // いるため、`0.0` は常に `min_top1` 未満となり空集合へ倒れる
                    // （fail-closed。`crate::precision::cosine_similarity` の
                    // ドキュメント参照）。
                    conf.push(crate::precision::cosine_similarity(query, embedding).unwrap_or(0.0));
                }
            }
            Ranking::Hybrid { .. } => {
                // 正規化に使う重み・ランク減衰定数は DISTANCE 段で使ったものと
                // 同一の固定値（`60.0, 1.0, 1.0`）で、`pool_depth` は正規化の
                // 計算式に現れないため既定値で構わない
                // （`crate::precision::rrf_normalized` のドキュメント参照）。
                let cfg = RrfConfig::default();
                for (_slot_id, score) in hits.iter().take(take_n) {
                    conf.push(crate::precision::rrf_normalized(*score, &cfg).unwrap_or(0.0));
                }
            }
        }
        let n = crate::precision::apply_gate(
            &conf,
            &thresholds,
            bound.limit,
            precision_policy.max_results(),
        )
        .map_err(|e| SqlSurfaceError::Internal {
            detail: format!("precision gate contract violation: {e}"),
        })?;
        let mut truncated = hits;
        truncated.truncate(n);
        truncated
    } else {
        hits
    };

    // RLS 実行時安全網（TASK-136・RLS-5）。`HINT ORDER` の内容に関係なく常に適用する
    // （モジュールドキュメント参照）。`ExecutionPlan` にはこの適用を分岐させる
    // フィールドを持たせておらず（`plan.rs` のドキュメント参照）、呼び出しを
    // 迂回する経路が型として存在しない。`arena` は候補構築と同一スナップショットの
    // テナント・可視性ラベルを保持しているため、`storage` の再取得なしに安全網を
    // 評価できる。現状は事前フィルタと同じ `arena` から再判定するため、この安全網
    // 単体で不可視行を追加で落とすことはない（defense-in-depth。モジュール
    // ドキュメント参照）。戻り値の [`RlsVerifiedHits`]（witness 型）は
    // [`project_rows`] へのみ渡り、安全網を経由しない生の `hits` から投影へ
    // 到達する経路は型として存在しない（`rls.rs` の型ドキュメント参照）。
    // `hits` の第 1 要素はアリーナのスロット番号。範囲外のスロット（provider の契約
    // 違反・データ不整合）は `None` を返し、`RlsSafetyNet::apply` が fail-closed に
    // 除去する（従来の「id が索引に無い」case と同じ扱い）。
    let verified: RlsVerifiedHits = RlsSafetyNet::new(ctx).apply(hits, |slot_id| {
        let slot = usize::try_from(slot_id).ok()?;
        let tenant = arena.tenant_id(slot)?;
        let visibility = arena.visibility(slot)?;
        Some((tenant, visibility))
    });

    // Issue #453: `defer_projection` が真のとき、`candidate_columns` は
    // 全スロットぶん空 `Vec` が積まれているだけ（`on_visible_row` の早期リターン・
    // 高速経路の空 `Vec` 埋めのいずれか）で使えない。投影で参照する列のマスクと、
    // Top-k 行の metadata 取得元（キャッシュヒットなら `SqlArenaSnapshot`、
    // それ以外は候補選択と同一 `read_txn` 上での行テーブル再取得）を
    // `project_rows` へ渡す。
    let deferred_projection_source = defer_projection || mask_trusted_defer;
    let needed_mask: Vec<bool> = if deferred_projection_source {
        let mut mask = vec![false; schema.columns.len()];
        for idx in &needed_column_indices {
            if let Some(slot) = mask.get_mut(*idx) {
                *slot = true;
            }
        }
        mask
    } else {
        Vec::new()
    };
    let scalar_source = if deferred_projection_source {
        match &cache_hit_snapshot {
            Some(snapshot) => {
                ScalarSource::Deferred(DeferredScalars::Snapshot(snapshot.metadata()))
            }
            None => ScalarSource::Deferred(DeferredScalars::Redb {
                read_txn,
                row_table_name: crate::catalog::user_rows_table_name(&bound.table),
            }),
        }
    } else if let Some(kept) = mask_kept_slots.as_ref() {
        // Issue #654: `arena` は複製されていない借用アリーナのため、`hits` の
        // スロット番号（元スロット）と `candidate_columns` の添字（出力側連番）が
        // 一致しない。`project_rows` 側で `kept` を通じて写像する。
        ScalarSource::EagerSubset {
            columns: &candidate_columns,
            kept_slots: kept,
        }
    } else {
        ScalarSource::Eager(&candidate_columns)
    };

    let rows = project_rows(
        verified,
        &bound.projection,
        schema,
        arena,
        scalar_source,
        &needed_mask,
    )?;

    let columns = bound
        .projection
        .iter()
        .map(|col| match col {
            ProjectedColumn::Id => ColumnMeta::Id,
            ProjectedColumn::Column { index, name } => ColumnMeta::Scalar {
                name: name.clone(),
                ty: schema
                    .columns
                    .get(*index)
                    .map(|c| c.ty.clone())
                    .unwrap_or(ColumnType::Text),
            },
            ProjectedColumn::Computed { name, .. } => ColumnMeta::Computed { name: name.clone() },
        })
        .collect();

    Ok(QueryResult { columns, rows })
}

/// [`BoundStatement`] を実行する（TASK-75 の公開 API）。従来の 6 引数シグネチャを
/// 維持する薄いラッパー（Issue #357・#363 レビュー指摘対応・AGENTS.md「公開 API・
/// エラー契約の互換性（P1）」: [`execute_statement_with_cache`] へ
/// `SparseCacheAccess`・`ArenaCacheAccess` を追加した際、両型が属する
/// `sql::sparse_cache`／`sql::arena_cache` モジュールがいずれも `pub(crate)` の
/// ため crate 外から構築不能になり、本関数を呼べなくなる破壊的変更を招いていた。
/// 本ラッパーは両方に `None` を渡し、それぞれのキャッシュ最適化を経由しない
/// 従来どおりの新規構築のみの経路として動作する）。crate 内部のキャッシュ経路
/// （`core.rs`）は [`execute_statement_with_cache`] を直接呼ぶ。
pub fn execute_statement(
    read_txn: &redb::ReadTransaction,
    provider: &dyn SearchProvider,
    ctx: &PolicyContext,
    schema: &TableSchema,
    bound: &BoundStatement,
    precision_policy: &crate::precision::PrecisionPolicy,
) -> Result<QueryResult, SqlSurfaceError> {
    execute_statement_with_cache(
        read_txn,
        provider,
        ctx,
        schema,
        bound,
        precision_policy,
        None,
        None,
        None,
        None,
    )
}

/// 投影段（TASK-136・RLS-5）が参照するスカラー列の取得元（Issue #453・#654）。
///
/// `Eager` は従来どおり `on_visible_row` が全可視行分あらかじめ複製しておいた
/// [`Value`] を参照するだけ（`WHERE`・式述語・hybrid のいずれかが絡むクエリ）。
/// `columns` は「出力側の連番」（`push_visible_row`／`filter_cached_rls_rows_subset`
/// が管理するカウンタ）で添字づけられており、`arena` が候補選択済みの縮約
/// アリーナ（＝スロット番号が出力側連番とそのまま一致する）である場合にのみ、
/// [`project_rows`] の `slot`（`hits` の第 1 要素＝アリーナのスロット番号）を
/// 直接 `columns.get(slot)` の添字として使える。
///
/// `EagerSubset` は Issue #654 のマスク経路専用: `arena` がキャッシュ済み
/// スナップショットの `VectorArena` を複製せず借用したままのため、`slot`
/// （＝スナップショットの元スロット番号）と `columns` の添字（出力側連番）が
/// 一致しない。`kept_slots`（`filter_cached_rls_rows_subset` の戻り値。狭義昇順）
/// に対する `slot` の位置（`binary_search`）を出力側連番として使い、
/// `columns.get(その位置)` を引く。
///
/// `Deferred` は SCALAR 段の走査・複製そのものを省略しており、`project_rows` の
/// 行ループが `RlsSafetyNet::apply` 通過後の Top-k 行に限って必要列だけを都度
/// デコードする（`defer_projection` のドキュメント参照。`sql::exec` モジュール
/// 冒頭）。
enum ScalarSource<'a> {
    Eager(&'a [Vec<Value>]),
    EagerSubset {
        columns: &'a [Vec<Value>],
        kept_slots: &'a [u32],
    },
    Deferred(DeferredScalars<'a>),
}

/// 1 行分のスカラー列（[`ScalarSource`] のどちらから取得したかを吸収する）。
/// `Eager` は `candidate_columns` を借用するだけ、`Deferred` は
/// [`decode_deferred_scalars`]／[`decode_deferred_scalars_from_redb`] が返す
/// 新規複製の `Vec<Value>` を保持する。
enum RowScalars<'a> {
    Borrowed(&'a [Value]),
    Owned(Vec<Value>),
}

impl RowScalars<'_> {
    fn get(&self, index: usize) -> Option<&Value> {
        match self {
            RowScalars::Borrowed(values) => values.get(index),
            RowScalars::Owned(values) => values.get(index),
        }
    }
}

/// [`ScalarSource::Deferred`] の 2 通りの取得元（いずれも候補選択と同一
/// スナップショットに閉じる。モジュールドキュメント「単一スナップショット」契約
/// 参照）。
enum DeferredScalars<'a> {
    /// キャッシュヒット時（`cache_fast_path_eligible`）。応答 `arena` が
    /// `snapshot.arena()` の直接借用であるため、スロット添字が
    /// `SqlArenaSnapshot::metadata()` とそのまま一致する。
    Snapshot(&'a [Vec<u8>]),
    /// キャッシュミス／`arena_cache == None`（公開ラッパー `execute_statement`）。
    /// 候補選択に使ったのと同一の `read_txn` 上で行テーブルを再度開き、
    /// `(tenant_id, id)` の複合キーで metadata を再取得する（TABLE-12）。
    Redb {
        read_txn: &'a redb::ReadTransaction,
        row_table_name: String,
    },
}

/// [`ScalarSource::Deferred`] の metadata バイト列から、`needed_mask` で示された
/// 列だけを [`Value`] として複製する（`row_codec::scan_scalar_columns_masked` の
/// マスク契約: マスク外列も UTF-8 妥当性検証は行うが `&str` 化・複製は省略する。
/// PR #369 の契約はここでも弱めない）。`budget`（呼び出し元が Top-k 全体で使い回す
/// 累計カウンタ）は [`MAX_CANDIDATE_SCALAR_BYTES`] を超えたら確保前に fail-closed
/// で拒否する（`try_alloc_text_for_budget` と同じ契約。Top-k 行だけが対象のため
/// 実際の複製量は従来の全可視行版よりも小さくなる）。
fn decode_deferred_scalars(
    schema: &TableSchema,
    metadata: &[u8],
    needed_mask: &[bool],
    budget: &mut usize,
) -> Result<Vec<Value>, SqlSurfaceError> {
    let scanned = row_codec::scan_scalar_columns_masked(schema, metadata, Some(needed_mask))
        .map_err(SqlSurfaceError::from)?;
    let mut out = Vec::with_capacity(scanned.len());
    for (idx, slot) in scanned.into_iter().enumerate() {
        if !needed_mask.get(idx).copied().unwrap_or(false) {
            out.push(Value::Null);
            continue;
        }
        match slot {
            None => out.push(Value::Null),
            Some(row_codec::ScalarRef::Text(t)) => {
                let owned = try_alloc_text_for_budget(t, budget, MAX_CANDIDATE_SCALAR_BYTES)
                    .map_err(|_| {
                        SqlSurfaceError::payload_too_large(
                            "deferred scalar projection exceeds candidate budget",
                        )
                    })?;
                out.push(Value::Text(owned));
            }
            Some(row_codec::ScalarRef::Integer(v)) => out.push(Value::Integer(v)),
            Some(row_codec::ScalarRef::BigInt(v)) => out.push(Value::BigInt(v)),
            Some(row_codec::ScalarRef::Real(v)) => out.push(Value::Real(v)),
            Some(row_codec::ScalarRef::Double(v)) => out.push(Value::Double(v)),
            Some(row_codec::ScalarRef::Enum(label)) => {
                let owned = try_alloc_text_for_budget(label, budget, MAX_CANDIDATE_SCALAR_BYTES)
                    .map_err(|_| {
                        SqlSurfaceError::payload_too_large(
                            "deferred scalar projection exceeds candidate budget",
                        )
                    })?;
                out.push(Value::Enum(owned));
            }
            Some(row_codec::ScalarRef::Bool(b)) => out.push(Value::Bool(b)),
            Some(row_codec::ScalarRef::Date(d)) => out.push(Value::Date(d)),
            Some(row_codec::ScalarRef::Timestamp(t)) => out.push(Value::Timestamp(t)),
            Some(row_codec::ScalarRef::Array(array_ref)) => {
                let value =
                    try_alloc_array_for_budget(array_ref, budget, MAX_CANDIDATE_SCALAR_BYTES)
                        .map_err(|_| {
                            SqlSurfaceError::payload_too_large(
                                "deferred scalar projection exceeds candidate budget",
                            )
                        })?;
                out.push(value);
            }
            Some(row_codec::ScalarRef::Bytes(bytes)) => {
                let owned = try_alloc_bytes_for_budget(bytes, budget, MAX_CANDIDATE_SCALAR_BYTES)
                    .map_err(|_| {
                    SqlSurfaceError::payload_too_large(
                        "deferred scalar projection exceeds candidate budget",
                    )
                })?;
                out.push(Value::Bytes(owned));
            }
            Some(row_codec::ScalarRef::Json(t)) => {
                let owned = try_alloc_text_for_budget(t, budget, MAX_CANDIDATE_SCALAR_BYTES)
                    .map_err(|_| {
                        SqlSurfaceError::payload_too_large(
                            "deferred scalar projection exceeds candidate budget",
                        )
                    })?;
                out.push(Value::Json(owned));
            }
            Some(row_codec::ScalarRef::Numeric(d)) => out.push(Value::Numeric(d)),
            Some(row_codec::ScalarRef::Uuid(u)) => out.push(Value::Uuid(u)),
        }
    }
    Ok(out)
}

/// [`ScalarSource::Deferred(DeferredScalars::Redb { .. })`] 専用: `slot` に対応する
/// 行を `(tenant_id, id)` の複合キーで再取得し、metadata を
/// [`decode_deferred_scalars`] へ渡す。TABLE-12 のキー/ヘッダ tenant 整合検査
/// （`storage::verify_row_key_tenant`）をここで行う——候補選択のアリーナ構築経路
/// （`arena.rs`）は可視行を格納する時点で同じ検査により整合性を保証しているが、
/// 遅延投影の再取得側でも defense-in-depth として独立に再検査する（受け入れ条件 2
/// 「現状どおり維持」を弱めず、むしろ強化する）。
/// 行欠落・デコード失敗・tenant 不整合はいずれも当該行を黙ってスキップせず
/// `SqlSurfaceError::Internal`（固定文言。テナント・id を含めない）として
/// クエリ全体を fail-closed に拒否する。
#[allow(clippy::too_many_arguments)]
fn decode_deferred_scalars_from_redb(
    table: &redb::ReadOnlyTable<(&'static str, u64), &'static [u8]>,
    schema: &TableSchema,
    arena: &VectorArena,
    slot: usize,
    row_id: u64,
    needed_mask: &[bool],
    budget: &mut usize,
) -> Result<Vec<Value>, SqlSurfaceError> {
    let key_tenant = arena
        .tenant_id(slot)
        .ok_or_else(|| SqlSurfaceError::Internal {
            detail: "candidate arena index out of range".to_string(),
        })?;
    let guard = table
        .get((key_tenant, row_id))
        .map_err(|_| SqlSurfaceError::Internal {
            detail: "deferred scalar projection lookup failed".to_string(),
        })?
        .ok_or_else(|| SqlSurfaceError::Internal {
            detail: "deferred scalar projection row not found".to_string(),
        })?;
    let buf = guard.value();
    let (header_tenant, _visibility) = crate::storage::decode_row_tenant_and_visibility(buf)
        .map_err(|_| SqlSurfaceError::Internal {
            detail: "deferred scalar projection header decode failed".to_string(),
        })?;
    crate::storage::verify_row_key_tenant(key_tenant, header_tenant).map_err(|_| {
        SqlSurfaceError::Internal {
            detail: "deferred scalar projection tenant mismatch".to_string(),
        }
    })?;
    let metadata = crate::storage::decode_row_metadata_borrowed(buf).map_err(|_| {
        SqlSurfaceError::Internal {
            detail: "deferred scalar projection metadata decode failed".to_string(),
        }
    })?;
    decode_deferred_scalars(schema, metadata, needed_mask, budget)
}

/// 投影段（TASK-136・RLS-5）。引数を [`RlsVerifiedHits`]（witness 型）に固定する
/// ことで、[`RlsSafetyNet::apply`] を経由しない生の `Vec<(u64, f64)>` から本関数へ
/// 到達する経路を型として作れなくする（`execute_statement` からのみ呼ばれる）。
/// `storage` への再取得は行わず、候補選択（`build_filtered_with_rows`）と同一
/// スナップショットで保持しておいた embedding（`arena`）・デコード済みスカラー列
/// （`scalar_source`）から返却行を構築する（候補選択後に対象行が更新・削除
/// されても、投影は候補選択時点の値を返すため、RLS・スカラー WHERE・embedding が
/// 候補選択と投影とで食い違うことはない）。`scalar_source` が
/// [`ScalarSource::Deferred`] の場合のみ Top-k 確定後にここで再取得・デコードが
/// 発生する（`needed_mask` はその場合にのみ意味を持ち、`Eager` では無視する）。
fn project_rows(
    verified: RlsVerifiedHits,
    projection: &[ProjectedColumn],
    schema: &TableSchema,
    arena: &VectorArena,
    scalar_source: ScalarSource<'_>,
    needed_mask: &[bool],
) -> Result<Vec<ResultRow>, SqlSurfaceError> {
    // `ScalarSource::Deferred(DeferredScalars::Redb { .. })` のときだけ行テーブルを
    // 1 回開く。行を 1 件も持たないテーブル（`user_rows/{table}` 未作成）は
    // `TableDoesNotExist` になるが、この場合ヒットも必ず 0 件（可視行が存在
    // しなければ候補も存在しない）なので、下の行ループで実際に参照されることは
    // ない。ここでは `None` として通し、実際に参照されたときにだけ
    // fail-closed に拒否する（`redb_table.as_ref().ok_or_else(...)` 参照）。
    let redb_table = match &scalar_source {
        ScalarSource::Deferred(DeferredScalars::Redb {
            read_txn,
            row_table_name,
        }) => match read_txn.open_table(crate::catalog::user_rows_table_def(row_table_name)) {
            Ok(table) => Some(table),
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(_) => {
                return Err(SqlSurfaceError::Internal {
                    detail: "deferred scalar projection table open failed".to_string(),
                })
            }
        },
        ScalarSource::Eager(_)
        | ScalarSource::EagerSubset { .. }
        | ScalarSource::Deferred(DeferredScalars::Snapshot(_)) => None,
    };
    let mut deferred_budget: usize = 0;
    let hits = verified.into_hits();
    let mut rows = Vec::with_capacity(hits.len());
    // Issue #353: `ProjectedColumn`（`pub` enum）の形状は変えず、`Computed` 列の
    // 式を行ループの**外**（本関数の入口）で 1 回だけステップ列コンパイルする。
    // `projection` と添字が 1 対 1 対応する（`Computed` 以外は `None`）。行ループは
    // 明示スタック（`expr_scratch`）を使い回し、`sql::udf_call::eval` の再帰
    // 呼び出しを一切行わない。
    let computed_programs: Vec<Option<crate::sql::expr_program::ExprProgram>> = projection
        .iter()
        .map(|col| match col {
            ProjectedColumn::Computed { expr, .. } => {
                Some(crate::sql::expr_program::ExprProgram::compile(expr))
            }
            ProjectedColumn::Id | ProjectedColumn::Column { .. } => None,
        })
        .collect();
    let mut expr_scratch: Vec<crate::sql::expr_program::StackValue> = Vec::new();
    for (slot_id, score) in hits {
        // ヒットの第 1 要素はアリーナのスロット番号（`execute_statement` の
        // `slot_ids` 参照）。embedding・スカラー列・行 `id` の 3 者すべてを同じ
        // スロットから引くため、同一 `id` の別テナント行が混ざる余地がない
        // （対象ビヘイビア: TABLE-12。Bugbot High 指摘への対応）。
        let slot = usize::try_from(slot_id).map_err(|_| SqlSurfaceError::Internal {
            detail: "candidate slot index out of range".to_string(),
        })?;
        let embedding = arena
            .vector(slot)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "candidate arena index out of range".to_string(),
            })?;
        // クライアントへ返す id は本来の行 `id`（スロット番号ではない）。`decoded`
        // の取得元によっては（`Deferred(Redb)`）行の再取得キーとしても使うため
        // `decoded` の前に確定させる。
        let id = *arena
            .ids()
            .get(slot)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "candidate arena index out of range".to_string(),
            })?;
        // Issue #453: `Eager` は `on_visible_row` が全可視行分あらかじめ複製して
        // おいた値を借用するだけ（従来どおり）。`Deferred` はここで初めて
        // metadata を取得し、投影に必要な列だけをデコードする（Top-k 確定後の
        // 行だけが対象になるため、`WHERE`・hybrid が絡まないクエリでは全可視行
        // 分のデコードコストが `limit` に依存しない固定コストではなくなる）。
        let decoded: RowScalars<'_> = match &scalar_source {
            ScalarSource::Eager(candidate_columns) => {
                RowScalars::Borrowed(candidate_columns.get(slot).ok_or_else(|| {
                    SqlSurfaceError::Internal {
                        detail: "search hit is missing from candidate scalar columns".to_string(),
                    }
                })?)
            }
            ScalarSource::EagerSubset {
                columns,
                kept_slots,
            } => {
                // Issue #654: `slot`（アリーナのスロット番号＝元スロット）を
                // `kept_slots`（狭義昇順。`filter_cached_rls_rows_subset` の戻り値）
                // 上の位置（＝出力側連番）へ写像してから `columns` を引く。
                // `kept_slots` に無いスロットは呼び出し規約違反・データ不整合として
                // fail-closed に拒否する（黙って別行の列を返さない）。
                let slot_u32 = u32::try_from(slot).map_err(|_| SqlSurfaceError::Internal {
                    detail: "candidate arena index does not fit in u32".to_string(),
                })?;
                let ordinal =
                    kept_slots
                        .binary_search(&slot_u32)
                        .map_err(|_| SqlSurfaceError::Internal {
                            detail: "search hit slot is missing from mask candidate slots"
                                .to_string(),
                        })?;
                RowScalars::Borrowed(columns.get(ordinal).ok_or_else(|| {
                    SqlSurfaceError::Internal {
                        detail: "search hit is missing from candidate scalar columns".to_string(),
                    }
                })?)
            }
            ScalarSource::Deferred(DeferredScalars::Snapshot(metadata)) => {
                let meta = metadata
                    .get(slot)
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "search hit is missing from candidate scalar columns".to_string(),
                    })?;
                RowScalars::Owned(decode_deferred_scalars(
                    schema,
                    meta,
                    needed_mask,
                    &mut deferred_budget,
                )?)
            }
            ScalarSource::Deferred(DeferredScalars::Redb { .. }) => {
                let table = redb_table
                    .as_ref()
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "deferred scalar projection table missing".to_string(),
                    })?;
                RowScalars::Owned(decode_deferred_scalars_from_redb(
                    table,
                    schema,
                    arena,
                    slot,
                    id,
                    needed_mask,
                    &mut deferred_budget,
                )?)
            }
        };
        let mut cells = Vec::with_capacity(projection.len());
        for (col_idx, col) in projection.iter().enumerate() {
            match col {
                ProjectedColumn::Id => cells.push(Cell::Integer(id)),
                ProjectedColumn::Column { index, .. } => {
                    let column =
                        schema
                            .columns
                            .get(*index)
                            .ok_or_else(|| SqlSurfaceError::Internal {
                                detail: "projected column index out of range".to_string(),
                            })?;
                    match &column.ty {
                        ColumnType::Vector(_) => {
                            cells.push(Cell::Vector(try_clone_embedding(embedding)?))
                        }
                        ColumnType::Text => match decoded.get(*index) {
                            Some(Value::Text(t)) => cells.push(Cell::Text(try_clone_text(t)?)),
                            Some(Value::Null) | None => cells.push(Cell::Null),
                            Some(Value::Integer(_))
                            | Some(Value::BigInt(_))
                            | Some(Value::Vector(_))
                            | Some(Value::Real(_))
                            | Some(Value::Double(_))
                            | Some(Value::Bool(_))
                            | Some(Value::Date(_))
                            | Some(Value::Timestamp(_))
                            | Some(Value::Array(_))
                            | Some(Value::Bytes(_))
                            | Some(Value::Json(_))
                            | Some(Value::Enum(_))
                            | Some(Value::Numeric(_))
                            | Some(Value::Uuid(_)) => {
                                return Err(SqlSurfaceError::Internal {
                                    detail: "scalar payload type mismatch".to_string(),
                                })
                            }
                        },
                        ColumnType::Integer => match decoded.get(*index) {
                            Some(Value::Integer(v)) => {
                                cells.push(Cell::SignedInteger(i64::from(*v)))
                            }
                            Some(Value::Null) | None => cells.push(Cell::Null),
                            Some(Value::BigInt(_))
                            | Some(Value::Text(_))
                            | Some(Value::Vector(_))
                            | Some(Value::Real(_))
                            | Some(Value::Double(_))
                            | Some(Value::Bool(_))
                            | Some(Value::Date(_))
                            | Some(Value::Timestamp(_))
                            | Some(Value::Array(_))
                            | Some(Value::Bytes(_))
                            | Some(Value::Json(_))
                            | Some(Value::Enum(_))
                            | Some(Value::Numeric(_))
                            | Some(Value::Uuid(_)) => {
                                return Err(SqlSurfaceError::Internal {
                                    detail: "scalar payload type mismatch".to_string(),
                                })
                            }
                        },
                        ColumnType::BigInt => match decoded.get(*index) {
                            Some(Value::BigInt(v)) => cells.push(Cell::SignedInteger(*v)),
                            Some(Value::Null) | None => cells.push(Cell::Null),
                            Some(Value::Integer(_))
                            | Some(Value::Text(_))
                            | Some(Value::Vector(_))
                            | Some(Value::Real(_))
                            | Some(Value::Double(_))
                            | Some(Value::Bool(_))
                            | Some(Value::Date(_))
                            | Some(Value::Timestamp(_))
                            | Some(Value::Array(_))
                            | Some(Value::Bytes(_))
                            | Some(Value::Json(_))
                            | Some(Value::Enum(_))
                            | Some(Value::Numeric(_))
                            | Some(Value::Uuid(_)) => {
                                return Err(SqlSurfaceError::Internal {
                                    detail: "scalar payload type mismatch".to_string(),
                                })
                            }
                        },
                        // F8（Issue #882 計画）: REAL は f64 への無損失拡大、DOUBLE
                        // はそのまま `Cell::Float` へ投影する（`Cell` enum への
                        // variant 追加は #882 計画で見送り、既存の Float 経路を
                        // 共有する設計判断）。
                        ColumnType::Real => match decoded.get(*index) {
                            Some(Value::Real(v)) => cells.push(Cell::Float(f64::from(*v))),
                            Some(Value::Null) | None => cells.push(Cell::Null),
                            _ => {
                                return Err(SqlSurfaceError::Internal {
                                    detail: "scalar payload type mismatch".to_string(),
                                })
                            }
                        },
                        ColumnType::Double => match decoded.get(*index) {
                            Some(Value::Double(v)) => cells.push(Cell::Float(*v)),
                            Some(Value::Null) | None => cells.push(Cell::Null),
                            _ => {
                                return Err(SqlSurfaceError::Internal {
                                    detail: "scalar payload type mismatch".to_string(),
                                })
                            }
                        },
                        // ENUM 列の投影は既存の `Cell::Text` に写像する（Issue #890
                        // D7。表示形式が TEXT と同一のため wire 側の追加変更を
                        // 抑えられる。`RETURNING`／NoSQL 応答も同じ写像を共有する）。
                        ColumnType::Enum(_) => match decoded.get(*index) {
                            Some(Value::Enum(label)) => {
                                cells.push(Cell::Text(try_clone_text(label)?))
                            }
                            Some(Value::Null) | None => cells.push(Cell::Null),
                            Some(Value::Integer(_))
                            | Some(Value::BigInt(_))
                            | Some(Value::Vector(_))
                            | Some(Value::Real(_))
                            | Some(Value::Double(_))
                            | Some(Value::Bool(_))
                            | Some(Value::Date(_))
                            | Some(Value::Timestamp(_))
                            | Some(Value::Array(_))
                            | Some(Value::Bytes(_))
                            | Some(Value::Json(_))
                            | Some(Value::Text(_))
                            | Some(Value::Numeric(_))
                            | Some(Value::Uuid(_)) => {
                                return Err(SqlSurfaceError::Internal {
                                    detail: "scalar payload type mismatch".to_string(),
                                })
                            }
                        },
                        ColumnType::Boolean => match decoded.get(*index) {
                            Some(Value::Bool(b)) => cells.push(Cell::Bool(*b)),
                            Some(Value::Null) | None => cells.push(Cell::Null),
                            Some(Value::Integer(_))
                            | Some(Value::BigInt(_))
                            | Some(Value::Vector(_))
                            | Some(Value::Real(_))
                            | Some(Value::Double(_))
                            | Some(Value::Text(_))
                            | Some(Value::Date(_))
                            | Some(Value::Timestamp(_))
                            | Some(Value::Array(_))
                            | Some(Value::Bytes(_))
                            | Some(Value::Json(_))
                            | Some(Value::Enum(_))
                            | Some(Value::Numeric(_))
                            | Some(Value::Uuid(_)) => {
                                return Err(SqlSurfaceError::Internal {
                                    detail: "scalar payload type mismatch".to_string(),
                                })
                            }
                        },
                        ColumnType::Date => match decoded.get(*index) {
                            Some(Value::Date(d)) => cells.push(Cell::Date(*d)),
                            Some(Value::Null) | None => cells.push(Cell::Null),
                            Some(Value::Integer(_))
                            | Some(Value::BigInt(_))
                            | Some(Value::Vector(_))
                            | Some(Value::Real(_))
                            | Some(Value::Double(_))
                            | Some(Value::Text(_))
                            | Some(Value::Bool(_))
                            | Some(Value::Timestamp(_))
                            | Some(Value::Array(_))
                            | Some(Value::Bytes(_))
                            | Some(Value::Json(_))
                            | Some(Value::Enum(_))
                            | Some(Value::Numeric(_))
                            | Some(Value::Uuid(_)) => {
                                return Err(SqlSurfaceError::Internal {
                                    detail: "scalar payload type mismatch".to_string(),
                                })
                            }
                        },
                        ColumnType::Timestamp => match decoded.get(*index) {
                            Some(Value::Timestamp(t)) => cells.push(Cell::Timestamp(*t)),
                            Some(Value::Null) | None => cells.push(Cell::Null),
                            Some(Value::Integer(_))
                            | Some(Value::BigInt(_))
                            | Some(Value::Vector(_))
                            | Some(Value::Real(_))
                            | Some(Value::Double(_))
                            | Some(Value::Text(_))
                            | Some(Value::Bool(_))
                            | Some(Value::Date(_))
                            | Some(Value::Array(_))
                            | Some(Value::Bytes(_))
                            | Some(Value::Json(_))
                            | Some(Value::Enum(_))
                            | Some(Value::Numeric(_))
                            | Some(Value::Uuid(_)) => {
                                return Err(SqlSurfaceError::Internal {
                                    detail: "scalar payload type mismatch".to_string(),
                                })
                            }
                        },
                        ColumnType::Array(_) => match decoded.get(*index) {
                            Some(Value::Array(array_value)) => {
                                cells.push(Cell::Array(array_value.clone()))
                            }
                            Some(Value::Null) | None => cells.push(Cell::Null),
                            Some(Value::Integer(_))
                            | Some(Value::BigInt(_))
                            | Some(Value::Vector(_))
                            | Some(Value::Real(_))
                            | Some(Value::Double(_))
                            | Some(Value::Text(_))
                            | Some(Value::Bool(_))
                            | Some(Value::Date(_))
                            | Some(Value::Timestamp(_))
                            | Some(Value::Bytes(_))
                            | Some(Value::Json(_))
                            | Some(Value::Enum(_))
                            | Some(Value::Numeric(_))
                            | Some(Value::Uuid(_)) => {
                                return Err(SqlSurfaceError::Internal {
                                    detail: "scalar payload type mismatch".to_string(),
                                })
                            }
                        },
                        ColumnType::Bytea => match decoded.get(*index) {
                            Some(Value::Bytes(b)) => {
                                let mut owned: Vec<u8> = Vec::new();
                                owned.try_reserve_exact(b.len()).map_err(|_| {
                                    SqlSurfaceError::Internal {
                                        detail: "failed to reserve bytea projection cell"
                                            .to_string(),
                                    }
                                })?;
                                owned.extend_from_slice(b);
                                cells.push(Cell::Bytes(owned));
                            }
                            Some(Value::Null) | None => cells.push(Cell::Null),
                            Some(Value::Integer(_))
                            | Some(Value::BigInt(_))
                            | Some(Value::Vector(_))
                            | Some(Value::Real(_))
                            | Some(Value::Double(_))
                            | Some(Value::Text(_))
                            | Some(Value::Bool(_))
                            | Some(Value::Date(_))
                            | Some(Value::Timestamp(_))
                            | Some(Value::Array(_))
                            | Some(Value::Json(_))
                            | Some(Value::Enum(_))
                            | Some(Value::Numeric(_))
                            | Some(Value::Uuid(_)) => {
                                return Err(SqlSurfaceError::Internal {
                                    detail: "scalar payload type mismatch".to_string(),
                                })
                            }
                        },
                        ColumnType::Json | ColumnType::Jsonb => match decoded.get(*index) {
                            Some(Value::Json(t)) => cells.push(Cell::Json(try_clone_text(t)?)),
                            Some(Value::Null) | None => cells.push(Cell::Null),
                            Some(Value::Integer(_))
                            | Some(Value::BigInt(_))
                            | Some(Value::Vector(_))
                            | Some(Value::Real(_))
                            | Some(Value::Double(_))
                            | Some(Value::Text(_))
                            | Some(Value::Bool(_))
                            | Some(Value::Date(_))
                            | Some(Value::Timestamp(_))
                            | Some(Value::Array(_))
                            | Some(Value::Bytes(_))
                            | Some(Value::Enum(_))
                            | Some(Value::Numeric(_))
                            | Some(Value::Uuid(_)) => {
                                return Err(SqlSurfaceError::Internal {
                                    detail: "scalar payload type mismatch".to_string(),
                                })
                            }
                        },
                        ColumnType::Numeric { .. } => match decoded.get(*index) {
                            Some(Value::Numeric(d)) => cells.push(Cell::Numeric(*d)),
                            Some(Value::Null) | None => cells.push(Cell::Null),
                            Some(Value::Integer(_))
                            | Some(Value::BigInt(_))
                            | Some(Value::Vector(_))
                            | Some(Value::Real(_))
                            | Some(Value::Double(_))
                            | Some(Value::Text(_))
                            | Some(Value::Bool(_))
                            | Some(Value::Date(_))
                            | Some(Value::Timestamp(_))
                            | Some(Value::Array(_))
                            | Some(Value::Bytes(_))
                            | Some(Value::Json(_))
                            | Some(Value::Enum(_))
                            | Some(Value::Uuid(_)) => {
                                return Err(SqlSurfaceError::Internal {
                                    detail: "scalar payload type mismatch".to_string(),
                                })
                            }
                        },
                        ColumnType::Uuid => match decoded.get(*index) {
                            Some(Value::Uuid(u)) => cells.push(Cell::Uuid(*u)),
                            Some(Value::Null) | None => cells.push(Cell::Null),
                            Some(Value::Integer(_))
                            | Some(Value::BigInt(_))
                            | Some(Value::Vector(_))
                            | Some(Value::Real(_))
                            | Some(Value::Double(_))
                            | Some(Value::Text(_))
                            | Some(Value::Bool(_))
                            | Some(Value::Date(_))
                            | Some(Value::Timestamp(_))
                            | Some(Value::Array(_))
                            | Some(Value::Bytes(_))
                            | Some(Value::Json(_))
                            | Some(Value::Enum(_))
                            | Some(Value::Numeric(_)) => {
                                return Err(SqlSurfaceError::Internal {
                                    detail: "scalar payload type mismatch".to_string(),
                                })
                            }
                        },
                    }
                }
                ProjectedColumn::Computed { .. } => {
                    // TASK-79（SQL-9）: 結果列位置の式項目。候補選択と同一スナップショット
                    // の `id`・embedding で評価する（投影段は再取得を行わない既存契約と
                    // 同方針）。評価エラーはクエリ全体の失敗として fail-closed に伝播する
                    // （行値・テナントを含まない固定文言。`sql::udf_call::eval` 参照）。
                    // Issue #353: `expr` の再帰評価ではなく、入口で 1 回だけ
                    // コンパイルした `computed_programs[col_idx]` を線形実行する。
                    let program = computed_programs
                        .get(col_idx)
                        .and_then(|p| p.as_ref())
                        .ok_or_else(|| SqlSurfaceError::Internal {
                            detail: "computed projection program missing at evaluation time"
                                .to_string(),
                        })?;
                    match program.eval(id, embedding, &mut expr_scratch)? {
                        udf_call::ExprValue::Scalar(v) => cells.push(Cell::Float(v)),
                        // 対象ビヘイビア: SQL-26（Issue #921）。
                        udf_call::ExprValue::Null => cells.push(Cell::Null),
                        udf_call::ExprValue::Vector(v) => {
                            // Issue #352: `VectorRef` 単体評価は行データを借用する
                            // だけになった（確保ゼロ）ため、応答行として行データより
                            // 長く保持する必要があるここ（投影段）でのみ
                            // `into_owned_vector` により fail-closed な確保・複製を
                            // 行う（`Cow::Owned`＝`vec_div` 等の構築結果はそのまま
                            // move し再確保しない）。
                            cells.push(Cell::Vector(udf_call::into_owned_vector(v)?))
                        }
                        udf_call::ExprValue::Bool(b) => cells.push(Cell::Bool(b)),
                    }
                }
            }
        }
        rows.push(ResultRow { id, score, cells });
    }
    Ok(rows)
}

/// **ベンチ計測専用**（非既定 feature `bench-internals`。Issue #660・PR #668
/// codex-review 指摘対応）。[`project_rows`] のうち `ProjectedColumn::Id` のみを
/// 対象とした投影経路——スロット番号から行 `id` への解決・[`ResultRow`]／
/// [`Cell`] の構築——を、`RlsSafetyNet::apply` 通過後の hits に対して
/// 単体で再現する。`crates/engine/benches/hybrid_profile_bench.rs` の S8
/// 計測（`SELECT id ... LIMIT k` 相当のハイブリッドクエリの末尾処理）が、
/// `RlsSafetyNet::apply`（Top-k のみ）だけでなく本来の投影コストも含められる
/// ようにするための限定公開で、`execute_statement` からは呼ばれない
/// （production 経路は引き続き [`project_rows`] のみを通る）。
///
/// スカラー列投影・`Computed` 列は対象外（本ベンチが計測する SQL は
/// `SELECT id` 固定のため、[`project_rows`] のうち該当分岐のみを複製する）。
#[cfg(feature = "bench-internals")]
pub fn project_id_only_rows(
    verified: RlsVerifiedHits,
    arena: &VectorArena,
) -> Result<Vec<ResultRow>, SqlSurfaceError> {
    let hits = verified.into_hits();
    let mut rows = Vec::with_capacity(hits.len());
    for (slot_id, score) in hits {
        // [`project_rows`] と同じ「hits の第 1 要素はアリーナのスロット番号」
        // 契約（モジュールドキュメント参照）。範囲外は fail-closed に拒否する。
        let slot = usize::try_from(slot_id).map_err(|_| SqlSurfaceError::Internal {
            detail: "candidate slot index out of range".to_string(),
        })?;
        let id = *arena
            .ids()
            .get(slot)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "candidate arena index out of range".to_string(),
            })?;
        rows.push(ResultRow {
            id,
            score,
            cells: vec![Cell::Integer(id)],
        });
    }
    Ok(rows)
}

/// [`crate::sql::parser::BoundInsert`] を実行する（SQL-10、TASK-80。TASK-186・
/// NOSQL-6 の前提として Issue #730 で公開 API へ昇格。`BoundScan`／`BoundAggregate`
/// と同じ「同作法」で `execute_scan`／`execute_aggregate` に続く）。`core.rs::
/// EngineCore::execute_insert_sql` からもクレート外からも呼べる（`execute_statement`
/// と対称の役割）。
///
/// **公開してよい根拠**（ファイル形 [`execute_file_insert`] が `pub(crate)` に
/// 閉じられている〔codex-review P1 指摘・PR #221〕のとの対比）: `operation_id`
/// 必須化ガード（TASK-92・RECOVER-1）は本関数の**内部**で
/// `ledger_mode.resolve(bound.operation_id.as_ref())` として適用される
/// （呼び出し元の `sql::allowlist::validate_insert` に依存しない。下記
/// ドキュメント参照）。`execute_file_insert` はこのガードを関数外
/// （`sql::allowlist::validate_insert`）に依存する設計のため `pub(crate)` の
/// まま維持する（`BoundFileInsert` を直接構築してガードを迂回できてしまうため。
/// 対象外）。可視性は常に `Visibility::Private` に固定し、テナントは
/// `ctx.tenant_id()` からのみ導出される（`BoundInsert` にテナント・可視性
/// フィールドは存在しない）。重複検出は呼び出し元テナントの名前空間内に閉じる
/// （TABLE-12・RLS-9。存在オラクルにならない）。`ledger_mode` に
/// [`crate::recovery::required_op_id::LedgerMode::CompareOnlyWithoutLedger`]
/// を渡す構成は本 Issue で新規に開いたバイパスではない
/// （`core.rs::EngineCore::with_ledger_mode` で既にクレート外から選択可能）。
/// `batch_limits.rs`（INDEX-4）のバッチ上限は本関数（単行 API）では適用しない
/// （上限判定は呼び出し元の責務）。
///
/// 行の書き込みはガードなし実体 [`crate::tenant::insert_typed_row_unchecked`]（`pub(crate)`）
/// へ委譲する（TASK-95・TABLE-12・RLS-9）。ガード（`operation_id` 必須化）は
/// `crate::tenant::insert_typed_row` のような呼び出し先ではなく本関数自身が
/// `ledger_mode.resolve(..)` で適用する（下記ドキュメント参照。TASK-92・
/// RECOVER-1・codex-review P1 指摘・PR #217）ため、ガード付き公開版
/// `crate::tenant::insert_typed_row` は経由しない。SQL 表層経由（`core.rs`）
/// では `sql::allowlist::validate_insert` の事前検査が同じ判定を本関数の
/// 呼び出し前に重ねて行うため、本関数内の判定は SQL 表層にとっては冗長な
/// 多層防御になるが、engine クレート外の呼び出し元（Issue #730）にとっては
/// これが**唯一**のガードである。`catalog.rs` の生の挿入 API は `pub(crate)` かつ
/// テナント名前空間の指定を呼び出し元任せにするため、SQL 表層からは使わない
/// （ガードを迂回できる書き込み入口を増やさない。security.md P0）。テナントは
/// `ctx.tenant_id()` からサーバー側で導出され（クライアントが列リストへテナント
/// 相当の値を指定しても無視される）、可視性は常に `Visibility::Private` に固定する
/// （`PolicyContext::is_visible` は `Public` 行を他テナントへも可視とするため、
/// 既定 `Public` は越境露出になる。fail-closed に `Private` を採用する）。
///
/// 単一の write トランザクションで完結し、既存 `id` への黙った上書きは行わない。
/// 重複検出のスコープは**呼び出し元テナントの名前空間内**に閉じる（行ストアの物理
/// キーが `(tenant_id, id)` であるため。TABLE-12・RLS-9）。したがって:
///
/// - 同一テナント内の `id` 重複は [`SqlSurfaceError::IdConflict`]（`23505`）
/// - 同一 `(tenant_id, table, operation_id)` への 2 回目以降の書き込みは
///   [`SqlSurfaceError::DuplicateOperationId`]（`23505`。TASK-94・対象ビヘイビア:
///   RECOVER-3）。台帳追記が行書き込みより先に同一 write トランザクション内で行われる
///   ため（`tenant.rs` モジュールドキュメント参照）、同一文の再送はこちらが先に検出する
/// - 他テナントが同じ `id`／`operation_id` を保持していても本経路は成功する
///   （応答・実行経路のいずれも他テナントの存在で分岐しないため、存在オラクルに
///   ならない）
///
/// `23505` は行キー `(tenant_id, id)` の衝突と `operation_id` の重複という 2 つの
/// 別原因を共有する（`error_format.rs::ErrorClass::UniqueViolation` 参照）。
/// 内容不一致検出（同一 `operation_id`・異なる内容）は TASK-101、対象ビヘイビア:
/// RECOVER-10 の管轄で未提供。
///
/// `ledger_mode` は呼び出し元が保持する構成をそのまま受け取り、
/// `ledger_mode.resolve(bound.operation_id.as_ref())` で
/// [`crate::recovery::ledger::LedgerWrite`] へ変換したうえで
/// [`crate::tenant::insert_typed_row_unchecked`] へ渡す（TASK-93、対象ビヘイビア:
/// RECOVER-2。台帳への追記が行書き込みと同一 write トランザクションで行われる点は
/// `tenant.rs` モジュールドキュメント参照）。この `resolve` 呼び出しが
/// `bound.operation_id` の必須化（TASK-92・RECOVER-1）そのものであり、
/// `LedgerMode::Ledgered`（既定）で `None` を渡すと `resolve` が `Err` を返し
/// [`SqlSurfaceError::MissingOperationId`]（`23502`）へ写像される——SQL 表層
/// 経由（`core.rs`）では `sql::allowlist::validate_insert` の事前検査が同じ
/// 判定を本関数の呼び出し前に既に行っているため通常は到達しないが、
/// engine クレート外の呼び出し元（Issue #730）にとってはこの経路が
/// **唯一到達可能な** fail-closed ガードである（`execute_insert_rejects_
/// missing_operation_id_under_ledgered` で固定）。
/// `LedgerMode::CompareOnlyWithoutLedger` では `None` でも `resolve` は
/// `LedgerWrite::Disabled` を返して成功し、台帳へは触れない。
pub fn execute_insert(
    storage: &crate::storage::Storage,
    ctx: &PolicyContext,
    bound: &crate::sql::parser::BoundInsert,
    ledger_mode: crate::recovery::required_op_id::LedgerMode,
) -> Result<InsertOutcome, SqlSurfaceError> {
    execute_insert_with_schema(storage, ctx, bound, ledger_mode, None)
}

/// [`execute_insert`] の実体（`pub(crate)`）。`expected_schema` を追加で受け取る点
/// のみが異なる（codex-review P1 指摘・PR #823）。[`execute_insert`] 自身は
/// Issue #730 で公開 API へ昇格済みのため、シグネチャ変更（`check_core_api.sh`
/// スナップショット更新）を避けてこの内部版へ切り出した。呼び出し元は
/// [`execute_insert`]（`None` を渡す既存動作）と、`Some(&schema)` を渡す
/// [`execute_insert_batch_with_schema`]（`bounds.len() == 1` の委譲先）・
/// `core::EngineCore::execute_bound_insert_in_session`（束縛時スキーマと
/// write トランザクション内で再取得したスキーマの不一致検査。詳細は
/// `tenant::insert_typed_row_unchecked` のドキュメント参照）。
pub(crate) fn execute_insert_with_schema(
    storage: &crate::storage::Storage,
    ctx: &PolicyContext,
    bound: &crate::sql::parser::BoundInsert,
    ledger_mode: crate::recovery::required_op_id::LedgerMode,
    expected_schema: Option<&crate::catalog::TableSchema>,
) -> Result<InsertOutcome, SqlSurfaceError> {
    execute_insert_with_schema_in(
        crate::tenant::WriteTarget::Autocommit(storage),
        ctx,
        bound,
        ledger_mode,
        expected_schema,
    )
}

/// [`execute_insert_with_schema`] の本体（明示トランザクション対応版。SQL-31・
/// TASK-221）。`target` が `InTxn` の場合は呼び出し元（`sql::transaction`）が
/// 保持する共有 `redb::WriteTransaction` へ書き込み、commit は行わない
/// （`COMMIT`/`ROLLBACK` 文が一括で行う）。`Autocommit` 経路は
/// [`execute_insert_with_schema`] とビット同一の挙動を保つ。
pub(crate) fn execute_insert_with_schema_in(
    target: crate::tenant::WriteTarget<'_>,
    ctx: &PolicyContext,
    bound: &crate::sql::parser::BoundInsert,
    ledger_mode: crate::recovery::required_op_id::LedgerMode,
    expected_schema: Option<&crate::catalog::TableSchema>,
) -> Result<InsertOutcome, SqlSurfaceError> {
    use crate::storage::Visibility;

    let ledger_write = ledger_mode
        .resolve(bound.operation_id.as_ref())
        .map_err(|_| SqlSurfaceError::MissingOperationId)?;

    crate::tenant::insert_typed_row_unchecked(
        target,
        &bound.table,
        ctx,
        bound.id,
        Visibility::Private,
        &bound.values,
        ledger_write,
        expected_schema,
    )
    .map_err(map_insert_write_error)?;

    Ok(InsertOutcome {
        rows_affected: 1,
        incremental: None,
    })
}

/// SQL 表層 `TRUNCATE TABLE <table> USING OPERATION_ID '<id>'`
/// （SQL-22、TASK-195）の実行入口。`validated`（`sql::allowlist::validate_truncate`
/// 済み構造）の `operation_id` を `ledger_mode` で台帳書き込み指示へ解決してから
/// [`crate::tenant::truncate_table_unchecked`] へ委譲する（[`execute_insert`] と
/// 同じ設計）。
///
/// `LedgerMode::Ledgered`（既定）で `operation_id` が `None` の場合は `resolve` が
/// `Err` を返し [`SqlSurfaceError::MissingOperationId`]（`23502`）へ写像される——
/// SQL 表層経由（`core.rs`）では `sql::allowlist::validate_truncate` の事前検査が
/// 同じ判定を本関数の呼び出し前に既に行っているため通常は到達しないが、
/// `execute_insert` と同じく本関数単体でも fail-closed を保つ。
pub fn execute_truncate(
    storage: &crate::storage::Storage,
    ctx: &PolicyContext,
    validated: &crate::sql::allowlist::ValidatedTruncate,
    ledger_mode: crate::recovery::required_op_id::LedgerMode,
) -> Result<TruncateOutcome, SqlSurfaceError> {
    execute_truncate_in(
        crate::tenant::WriteTarget::Autocommit(storage),
        ctx,
        validated,
        ledger_mode,
    )
}

/// [`execute_truncate`] の本体（明示トランザクション対応版。SQL-31・TASK-221。
/// [`execute_insert_with_schema_in`] と同じ設計）。
pub(crate) fn execute_truncate_in(
    target: crate::tenant::WriteTarget<'_>,
    ctx: &PolicyContext,
    validated: &crate::sql::allowlist::ValidatedTruncate,
    ledger_mode: crate::recovery::required_op_id::LedgerMode,
) -> Result<TruncateOutcome, SqlSurfaceError> {
    let ledger_write = ledger_mode
        .resolve(validated.operation_id.as_ref())
        .map_err(|_| SqlSurfaceError::MissingOperationId)?;

    crate::tenant::truncate_table_unchecked(target, &validated.table_name, ctx, ledger_write)
        .map_err(map_insert_write_error)?;

    Ok(TruncateOutcome {})
}

/// SQL 表層 `DELETE FROM <table> WHERE id = <n> USING OPERATION_ID '<id>'`
/// （SQL-18、TASK-191、#867）の実行入口。`bound`（`sql::parser::bind_delete`
/// 束縛済み）の `operation_id` を `ledger_mode` で台帳書き込み指示へ解決してから
/// [`crate::tenant::delete_row_ledgered_unchecked`] へ委譲する（[`execute_insert`]・
/// [`execute_truncate`] と同じ設計）。
///
/// **0 行成功への写像（RLS-9・RLS-10）**: `delete_row_ledgered_unchecked` は
/// 「対象行が不存在」と「対象行が存在するが他テナント所有」を区別せず
/// [`crate::tenant::DeleteRowOutcome::NotFound`] を返す契約（`tenant.rs`
/// ドキュメント参照）。本関数はこれを `Ok(DeleteOutcome { rows_affected: 0 })`
/// へ写像する。他テナント保持 id・未存在 id のいずれでも成否・件数・
/// `wire_code`・文言・副作用が完全に同一になるため、これらのケースを応答から
/// 区別できない。
///
/// **台帳記録（`NotFound` を含む。codex-review P1 指摘・PR #983 対応）**:
/// `delete_row_ledgered_unchecked` は `NotFound` の場合でも台帳への tentative
/// 追記を同一トランザクションで commit する（テーブル世代は進行させない）。
/// これにより、0 行成功として応答した `operation_id` を後から実在する `id`
/// （新規 INSERT 後）へ再送しても、台帳照合（TASK-101・RECOVER-10）が
/// `DuplicateOperationId`（`23505`）／`OperationIdContentMismatch`（`22023`）
/// のいずれかとして検出し、意図しない実削除を防ぐ（`docs/design/
/// sql-delete-single-row.md` 参照。旧実装は `delete_row_unchecked` を流用し
/// `NotFound` で台帳を commit しなかったため、この再送安全性違反があった）。
///
/// `LedgerMode::Ledgered`（既定）で `operation_id` が `None` の場合は `resolve` が
/// `Err` を返し [`SqlSurfaceError::MissingOperationId`]（`23502`）へ写像される——
/// SQL 表層経由（`core.rs`）では `sql::allowlist::validate_delete` の事前検査が
/// 同じ判定を本関数の呼び出し前に既に行っているため通常は到達しないが、
/// `execute_insert`／`execute_truncate` と同じく本関数単体でも fail-closed を保つ。
pub fn execute_delete(
    storage: &crate::storage::Storage,
    ctx: &PolicyContext,
    bound: &crate::sql::parser::BoundDelete,
    ledger_mode: crate::recovery::required_op_id::LedgerMode,
) -> Result<DeleteOutcome, SqlSurfaceError> {
    let ledger_write = ledger_mode
        .resolve(bound.operation_id.as_ref())
        .map_err(|_| SqlSurfaceError::MissingOperationId)?;

    match crate::tenant::delete_row_ledgered_unchecked(
        storage,
        &bound.table,
        ctx,
        bound.id,
        ledger_write,
    ) {
        Ok(crate::tenant::DeleteRowOutcome::Deleted) => Ok(DeleteOutcome { rows_affected: 1 }),
        Ok(crate::tenant::DeleteRowOutcome::NotFound) => Ok(DeleteOutcome { rows_affected: 0 }),
        Err(e) => Err(map_write_error(e, "delete")),
    }
}

/// `RETURNING` 句（Issue #873・SQL-21）付き単一行 `DELETE`（SQL-18・TASK-191 の
/// 唯一到達経路。述語形 `DELETE` は許可リスト段〔`sql::allowlist::
/// validate_delete_statement_tokens`〕で `RETURNING` を一律 `42601` 拒否する
/// ため本関数へは到達しない）の実行入口。[`execute_delete`] と同じ
/// `tenant::delete_row_ledgered_capturing_unchecked` を `capture: Some(schema)`
/// で呼び、削除**前**の行内容を `crate::tenant::CapturedRow`（クレート内部型）
/// として捕捉する。
///
/// **RLS 再判定（多層防御）**: 捕捉行は書き込み経路（テナント名前空間キー
/// `(tenant_id, id)`・TABLE-12）由来のため通常は `ctx` から可視だが、
/// `ctx.is_visible(row_tenant, row_visibility)`（RLS-7・RLS-8 と同じ判定）を
/// 再適用し、不可視の場合は `result.rows` を空にする——`rows_affected` は
/// 実際に削除した件数のまま変えない（[`ReturningOutcome`] のドキュメント
/// 参照）。`schema` は呼び出し元が束縛した時点のスキーマで、`tenant.rs` の
/// TOCTOU 対策（束縛後・削除実行前のスキーマ変更検出）に使われる。
///
/// **投影は commit 前・同一トランザクション内で行う（codex-review P1 指摘・
/// PR #991 対応）**: [`execute_insert_returning`] は書き込み予定値
/// （`bounds[*].values`）が呼び出し前から既知なので `column_meta`・
/// `project_row` を書き込みより先に呼べるが、DELETE の場合は捕捉行の実体
/// （`CapturedRow::values`）が `tenant::delete_row_ledgered_capturing_unchecked`
/// の write トランザクション内でしか得られない。そこで `project_row`
/// （文字列・ベクトルの `try_reserve_exact` 失敗や結果容量超過で失敗しうる）
/// は `project` コールバックとして渡し、`tenant::delete_row_impl` に
/// **commit の前**に呼ばせる（`tenant.rs` の `project` ドキュメント参照）。
/// これにより、投影の失敗は `write_txn` の abort（行削除・台帳追記ともに
/// 破棄）を伴うため、「DELETE は失敗応答なのに行は既に永続化されている」と
/// いう commit 成功境界違反が起きない——旧実装は commit 後に `project_row` を
/// 呼んでいたため、この違反が起こり得た。`column_meta` 自体は `projection`・
/// `schema` のみに依存する純粋な計算（redb I/O を伴わない）であり、書き込み
/// より前に呼んでも同じ理由で安全なため従来どおり先に呼ぶ。
pub fn execute_delete_returning(
    storage: &crate::storage::Storage,
    ctx: &PolicyContext,
    bound: &crate::sql::parser::BoundDelete,
    ledger_mode: crate::recovery::required_op_id::LedgerMode,
    returning: &[crate::sql::parser::ProjectedColumn],
    schema: &TableSchema,
) -> Result<ReturningOutcome, SqlSurfaceError> {
    let ledger_write = ledger_mode
        .resolve(bound.operation_id.as_ref())
        .map_err(|_| SqlSurfaceError::MissingOperationId)?;

    let columns = crate::sql::returning::column_meta(returning, schema)?;

    // `project` コールバック（`tenant::delete_row_impl` が commit 前に呼ぶ）の
    // 結果をここへ書き戻す。クロージャは `FnMut` のため 1 回しか呼ばれない
    // 前提でも所有権を返せず、外側の可変変数へ書き込む形にする。
    let mut projected_row: Option<ResultRow> = None;
    let mut budget = 0usize;
    let mut project =
        |row: &crate::tenant::CapturedRow| -> Result<(), crate::tenant::TenantWriteError> {
            if !ctx.is_visible(&row.tenant_id, row.visibility) {
                return Ok(());
            }
            match crate::sql::returning::project_row(row.id, &row.values, returning, &mut budget) {
                Ok(result_row) => {
                    projected_row = Some(result_row);
                    Ok(())
                }
                Err(SqlSurfaceError::PayloadTooLarge { detail }) => {
                    Err(crate::tenant::TenantWriteError::ReturningProjectionTooLarge(detail))
                }
                Err(other) => Err(crate::tenant::TenantWriteError::ReturningProjectionFailed(
                    other.to_string(),
                )),
            }
        };

    let (outcome, _captured) = crate::tenant::delete_row_ledgered_capturing_unchecked(
        storage,
        &bound.table,
        ctx,
        bound.id,
        ledger_write,
        Some(schema),
        Some(&mut project),
    )
    // [`execute_delete`]（RETURNING なし）と同じ `"delete"` 語彙で写像する
    // （Cursor Bugbot Low 指摘・PR #991）。本関数は DELETE の実行であり、
    // `map_insert_write_error`（固定文言 `"insert"`）を使うと束縛後のスキーマ
    // 変更・捕捉行デコード失敗・ストレージエラー等の一般的な書き込み失敗で
    // 「insert rejected」という誤った操作名がクライアントへ返っていた。
    .map_err(|e| map_write_error(e, "delete"))?;

    let rows_affected = match outcome {
        crate::tenant::DeleteRowOutcome::Deleted => 1,
        crate::tenant::DeleteRowOutcome::NotFound => 0,
    };

    let mut rows = Vec::new();
    if let Some(row) = projected_row {
        rows.push(row);
    }

    Ok(ReturningOutcome {
        command: crate::sql::returning::DmlCommand::Delete,
        rows_affected,
        result: QueryResult { columns, rows },
    })
}

/// SQL 表層 `UPDATE <table> SET <col> = <lit>[, ...] WHERE id = <n>
/// USING OPERATION_ID '<id>'`（SQL-17、TASK-191。Issue #865）の実行入口。
/// `bound`（`sql::parser::bind_update` 済み）の `operation_id` を `ledger_mode` で
/// 台帳書き込み指示へ解決してから [`crate::tenant::update_row_columns_unchecked`]
/// へ委譲する（[`execute_truncate`] と同じ設計）。
///
/// `LedgerMode::Ledgered`（既定）で `operation_id` が `None` の場合は `resolve` が
/// `Err` を返し [`SqlSurfaceError::MissingOperationId`]（`23502`）へ写像される——
/// SQL 表層経由（`core.rs`）では `sql::allowlist::validate_update` の事前検査が
/// 同じ判定を本関数の呼び出し前に既に行っているため通常は到達しないが、
/// `execute_insert`／`execute_truncate` と同じく本関数単体でも fail-closed を保つ。
pub fn execute_update(
    storage: &crate::storage::Storage,
    ctx: &PolicyContext,
    bound: &crate::sql::parser::BoundUpdate,
    ledger_mode: crate::recovery::required_op_id::LedgerMode,
) -> Result<UpdateOutcome, SqlSurfaceError> {
    execute_update_with_schema(storage, ctx, bound, ledger_mode, None)
}

/// [`execute_update`] の実体（`pub(crate)`）。`expected_schema` を追加で受け取る点
/// のみが異なる（`execute_insert_with_schema` と同型。`core.rs::
/// execute_update_form` が `InsertSchemaLookup` でキャッシュ済みのスキーマを渡す
/// 経路として使う）。
pub(crate) fn execute_update_with_schema(
    storage: &crate::storage::Storage,
    ctx: &PolicyContext,
    bound: &crate::sql::parser::BoundUpdate,
    ledger_mode: crate::recovery::required_op_id::LedgerMode,
    expected_schema: Option<&crate::catalog::TableSchema>,
) -> Result<UpdateOutcome, SqlSurfaceError> {
    let ledger_write = ledger_mode
        .resolve(bound.operation_id.as_ref())
        .map_err(|_| SqlSurfaceError::MissingOperationId)?;

    let rows_affected = crate::tenant::update_row_columns_unchecked(
        storage,
        &bound.table,
        ctx,
        bound.id,
        &bound.assignments,
        ledger_write,
        expected_schema,
    )
    .map_err(|e| map_write_error(e, "update"))?;

    Ok(UpdateOutcome {
        rows_affected: rows_affected as u64,
    })
}

/// [`map_write_error`] の `"insert"` 版ラッパー（既存呼び出し元との後方互換用。
/// Issue #865 で `op` パラメータ化した本体へ切り出した。挙動は切り出し前と
/// 完全に同一）。
pub(crate) fn map_insert_write_error(e: crate::tenant::TenantWriteError) -> SqlSurfaceError {
    map_write_error(e, "insert")
}

/// [`execute_insert`]・[`execute_insert_batch`]・[`execute_truncate`]・
/// [`execute_update`]・[`execute_delete`]・述語つき `DELETE`／`UPDATE`
/// 実行結線〔[`execute_predicate_delete`]・[`execute_predicate_update`]。
/// Issue #871〕が共有する `TenantWriteError` → `SqlSurfaceError` の写像本体
/// （Issue #771・TASK-178・NOSQL-6 で切り出し、Issue #865 で `op` を追加
/// パラメータ化した）。`TenantWriteError::Catalog(CatalogError::Invalid(_))` /
/// `TenantWriteError::Storage(StorageError::Codec(_))` アームの detail 文言
/// （`"{op} rejected: invalid row"`）に呼び出し元の操作名を埋め込む点のみが
/// 変更点で、`wire_code` 自体は不変。UPDATE の既存行デコード失敗・列値の不正
/// （TEXT の `MAX_TEXT_FIELD_LEN` 超過等）・DELETE の失敗を「insert が拒否
/// された」という誤った文言でクライアントへ返さないための変更（`client_message()`
/// はこの detail をそのまま含める）。呼び出し元ごとに専用の写像本体は追加しない
/// （`TenantWriteError` の variant 集合は insert／update／truncate／delete で
/// 共通のため）。`execute_truncate` は本 Issue のスコープ外のため引き続き
/// `map_insert_write_error`（固定文言 `"insert"`）を呼んでおり、TRUNCATE 失敗時の
/// detail が `"insert rejected"`／`"insert failed"` になる既存の不整合は本 PR
/// 時点でも未解消のまま残る（新規のリグレッションではない。是正は別 Issue の
/// 担当）。
fn map_write_error(e: crate::tenant::TenantWriteError, op: &'static str) -> SqlSurfaceError {
    use crate::catalog::CatalogError;
    use crate::storage::StorageError;
    use crate::tenant::TenantWriteError;

    match e {
        // 同一テナント内の id 重複（`23505`）。SQL-10 の再送判定が識別できるよう、
        // 値不正（`22000`）へ丸めずに専用の wire_code を維持する。
        TenantWriteError::IdConflict => SqlSurfaceError::IdConflict,
        // `PRIMARY KEY`（Issue #903）・UNIQUE 制約（Issue #905。TABLE-16・TASK-204）のテナント内一意性制約
        // 違反。行キー衝突（`IdConflict`）と原因は異なるが `23505` は共有する。
        TenantWriteError::UniqueViolation => SqlSurfaceError::unique_violation(),
        // `CHECK` 制約違反（TABLE-16・TASK-204、Issue #906）を `_` 節（`XX000`）へ
        // 丸めると、クライアントが「内部事象」と「宣言済み制約への違反」を
        // 判別できなくなる。制約名のみを含む固定文言（行の値・テナントは含まない）。
        TenantWriteError::CheckViolation { constraint } => {
            SqlSurfaceError::check_violation(constraint)
        }
        TenantWriteError::CheckEvaluationFailed => SqlSurfaceError::Internal {
            detail: "check constraint evaluation failed".to_string(),
        },
        // `FOREIGN KEY` 制約違反（`23503`。TABLE-17・TASK-205、Issue #907）。`_` 節
        // （`XX000`）へ丸めると、クライアントが参照整合性違反をサーバー内部事象と
        // 取り違える。値・参照先の有無の理由を含まない固定文言。
        TenantWriteError::ForeignKeyViolation => SqlSurfaceError::ForeignKeyViolation,
        // `tenant::insert_typed_row_unchecked`／`insert_typed_rows_unchecked`／
        // `update_row_columns_unchecked` 自体は `operation_id` 必須化ガード
        // （`recovery::required_op_id::LedgerMode`）を持たない（`tenant.rs`
        // モジュールドキュメント参照）。本経路（SQL `INSERT`／`UPDATE`・
        // NoSQL `insert` op）ではガードを呼び出し元（[`execute_insert`]・
        // [`execute_insert_batch`]・[`execute_update`]）が書き込みトランザクション
        // 開始前に既に適用済みのため、この写像アームは実際には到達しない。ただし
        // `TenantWriteError` の網羅性を保ち、`23502` を返す正しい写像を明示して
        // おく（TASK-92・対象ビヘイビア: RECOVER-1）。
        TenantWriteError::MissingOperationId => SqlSurfaceError::MissingOperationId,
        // 台帳照合（TASK-101・RECOVER-10）: 同一 operation_id・同一内容の再送は
        // `23505`、内容不一致（v1 レガシーエントリへの再送を含む）は `22023` へ
        // 写像する。`_` 節（`XX000`）へ丸めると、再送検知の応答をクライアントが
        // 判別できなくなる。
        TenantWriteError::DuplicateOperationId => SqlSurfaceError::DuplicateOperationId,
        TenantWriteError::OperationIdContentMismatch => SqlSurfaceError::OperationIdContentMismatch,
        // 明示トランザクション（SQL-31・TASK-221）が `writer_gate` を保持している間、
        // 別セッションの自動コミット INSERT/UPDATE/DELETE/TRUNCATE が待機タイムアウト
        // した（`55P03`）。`_` 節（`XX000`）へ丸めると、クライアントがリトライ可能な
        // ロック競合とサーバー内部エラーを判別できなくなる。`catalog::table_lookup_error`・
        // `tenant::ClassifiedError` と同じ写像先（codex-review 指摘）。
        TenantWriteError::WriteLockTimeout => SqlSurfaceError::LockNotAvailable,
        // `RETURNING` 句（Issue #873・SQL-21）の捕捉行投影が容量上限超過で失敗した
        // （[`execute_delete_returning`] の `project` コールバック。commit **前**に
        // 検出し `write_txn` を abort させる設計のため、この写像に到達する時点で
        // 削除は永続化されていない）。`_` 節（`XX000`）へ丸めると `54000` を失う。
        TenantWriteError::ReturningProjectionTooLarge(_) => {
            SqlSurfaceError::payload_too_large("RETURNING result exceeds capacity")
        }
        // 述語つき UPDATE/DELETE の候補列挙（`tenant::enumerate_dml_candidates`）が
        // 総走査行数上限（`tenant::MAX_SCANNED_ROWS`）に達した（codex-review P1
        // 指摘・Issue #871）。`_` 節（`XX000`）へ丸めると、一致件数上限超過
        // （`PredicateDmlOutcome::LimitExceeded` 由来の `54000`）と別コードに
        // なってしまい、クライアントが同種の「上限超過」を判別できなくなる。
        TenantWriteError::TooManyRowsScanned => {
            SqlSurfaceError::payload_too_large("too many rows scanned")
        }
        // 同じく commit 前 abort の内部事象版（型不整合等。untrusted 入力起因では
        // ないため `XX000`。`_` 節と同じ分類だが意図を明示する）。
        TenantWriteError::ReturningProjectionFailed(_) => SqlSurfaceError::Internal {
            detail: "RETURNING projection failed".to_string(),
        },
        TenantWriteError::Catalog(CatalogError::TableNotFound(name)) => {
            SqlSurfaceError::UndefinedTable { name }
        }
        // 入力値に起因すると型で確認できるものだけを「受理構文だが値が不正」として
        // `22000` へ丸め込む（`CatalogError::Invalid` は識別子・次元・スキーマ検証の
        // 失敗、`StorageError::Codec` は行エンコード時の不正値）。エラー文言に行内容・
        // 所有テナントは含めない（security.md「エラー・ログ経由で他テナントのデータ・
        // 存在情報を漏らさない」）。`TenantWriteError::LedgerCorrupted`（`op_ledger` の
        // 未知フォーマット・バックエンド障害）はここに含めない: 台帳破損は送信された
        // 行データとは無関係のサーバー内部事象であり、クライアントの行を「不正」と
        // 誤認させると誤った再試行を誘発する（Cursor Bugbot 指摘・PR #226）。下の
        // `_` 節（`XX000`）へ委ねる。
        TenantWriteError::Catalog(CatalogError::Invalid(_))
        | TenantWriteError::Storage(StorageError::Codec(_)) => {
            SqlSurfaceError::invalid_input(format!("{op} rejected: invalid row"))
        }
        // それ以外（redb バックエンド障害・commit 失敗・カタログ破損・認可失敗・
        // 台帳破損等）はサーバー側の内部事象として `XX000` へ写像する
        // （codex-review P0/P1 指摘・PR #189: バックエンド障害を入力不正として返すと
        // 再試行・障害判定を誤らせる。また `TenantWriteError` の `Display`/`Debug` は
        // 原因を秘匿する契約のため、detail には原因を一切展開せず固定文言に留める）。
        _ => SqlSurfaceError::Internal {
            detail: format!("{op} failed"),
        },
    }
}

/// 述語つき `DELETE FROM <table> WHERE ... USING OPERATION_ID '<id>'`
/// （SQL-19・TASK-192、Issue #871）の実行本体。唯一の到達経路は
/// `core.rs::EngineCore` の `DELETE` 分岐（述語形。
/// `EngineCore::execute_predicate_delete_form`）。`pub(crate)`（`execute_file_insert`
/// と同じ理由でクレート外へ公開しない）。
///
/// `content_hash_value` は呼び出し元（`core.rs`）が束縛の直前に
/// `recovery::content_hash::for_delete_where` で 1 回だけ計算した値を渡す
/// （計算位置の詳細は `docs/design/predicate-dml-exec.md` 参照）。`schema` は
/// 呼び出し元が既に取得済みのスキーマ（`bind_predicate_delete` と同一のもの）を
/// 渡し、`tenant::delete_rows_where_unchecked` の `expected_schema` 照合へ
/// 引き継ぐ（並行 `ALTER TABLE` 検知。`upsert_typed_rows_unchecked` と同じ判断）。
///
/// `WHERE` 述語の評価は `sql/scan.rs::execute_scan` の走査ループと同一の意味論
/// （`declarative_filter::matches_all` → 各 `expr_filters` を `ExprProgram::eval`。
/// `dim == 0`（`VECTOR` 列が NULL）の行を式が実際に参照する場合は `eval` 自身が
/// `ExprValue::Null` を返し非該当扱いになる。第 2 の述語評価器を作らない）を、
/// 候補行列挙のクロージャとして `tenant::delete_rows_where_unchecked` へ注入する。
pub(crate) fn execute_predicate_delete(
    storage: &crate::storage::Storage,
    ctx: &PolicyContext,
    bound: &BoundPredicateDelete,
    ledger_mode: crate::recovery::required_op_id::LedgerMode,
    schema: &TableSchema,
    content_hash_value: &crate::recovery::content_hash::ContentHash,
) -> Result<DeleteOutcome, SqlSurfaceError> {
    let ledger_write = ledger_mode
        .resolve(bound.operation_id())
        .map_err(|_| SqlSurfaceError::MissingOperationId)?;

    let metadata_filters = bound.metadata_filters();
    let expr_filters = bound.expr_filters();
    let or_filters = bound.or_filters();
    let needs_embedding = expr_filters.iter().any(udf_call::references_embedding)
        || or_filters
            .iter()
            .any(crate::sql::where_tree::BoundOrGroup::references_embedding);
    let expr_programs: Vec<crate::sql::expr_program::ExprProgram> = expr_filters
        .iter()
        .map(crate::sql::expr_program::ExprProgram::compile)
        .collect();
    let mut scratch: Vec<crate::sql::expr_program::StackValue> = Vec::new();

    let predicate = |candidate: &crate::tenant::DmlCandidate<'_>| -> Result<bool, SqlSurfaceError> {
        let scanned = row_codec::scan_scalar_columns(schema, candidate.metadata)?;
        if !declarative_filter::matches_all(metadata_filters, &scanned) {
            return Ok(false);
        }
        for (expr, program) in expr_filters.iter().zip(&expr_programs) {
            let references_embedding = udf_call::references_embedding(expr);
            // `dim == 0`（`VECTOR` 列が NULL）の行の NULL 伝播は `program.eval`
            // 自身（`ExprStep::PushVector` の空スライス判定。`sql::expr_program`
            // 参照）が行う（codex-review P1 指摘対応: 静的な式木走査による事前
            // 除外は `CASE` の選ばれない分岐に embedding 参照があるだけの行まで
            // 誤って除外していたため撤去し、評価時点の判定へ一本化した）。
            let embedding: &[f32] = if references_embedding {
                candidate.embedding
            } else {
                &[]
            };
            match program.eval(candidate.id, embedding, &mut scratch)? {
                udf_call::ExprValue::Bool(true) => {}
                // NULL（UNKNOWN）は非該当として扱う（対象ビヘイビア: SQL-26。
                // Issue #921）。
                udf_call::ExprValue::Bool(false) | udf_call::ExprValue::Null => return Ok(false),
                _ => {
                    return Err(SqlSurfaceError::invalid_input(
                        "WHERE expression did not evaluate to a boolean",
                    ))
                }
            }
        }
        // TASK-208・SQL-24（Issue #912）: `WHERE` の OR 群を、既存のメタデータ
        // フィルタ・式述語と同じ述語評価の一部として適用する。
        for group in or_filters {
            let group_embedding: &[f32] = if group.references_embedding() {
                candidate.embedding
            } else {
                &[]
            };
            if !group.matches(
                &scanned,
                candidate.id,
                group_embedding,
                candidate.dim as usize,
                &mut scratch,
            )? {
                return Ok(false);
            }
        }
        Ok(true)
    };

    let limit = bound.max_affected_rows();
    match crate::tenant::delete_rows_where_unchecked(
        storage,
        bound.table(),
        ctx,
        ledger_write,
        content_hash_value,
        Some(schema),
        needs_embedding,
        limit,
        predicate,
    ) {
        Ok(crate::tenant::PredicateDmlOutcome::Applied { rows_affected }) => Ok(DeleteOutcome {
            rows_affected: rows_affected as u64,
        }),
        Ok(crate::tenant::PredicateDmlOutcome::LimitExceeded { count }) => {
            crate::sql::parser::check_affected_row_count(count, limit)?;
            Err(SqlSurfaceError::Internal {
                detail: "predicate DELETE limit check did not reject an over-limit count"
                    .to_string(),
            })
        }
        Err(crate::tenant::PredicateDmlError::Predicate(e)) => Err(e),
        Err(crate::tenant::PredicateDmlError::Write(e)) => Err(map_write_error(e, "delete")),
    }
}

/// 述語つき `UPDATE <table> SET ... WHERE ... USING OPERATION_ID '<id>'`
/// （SQL-19・TASK-192、Issue #871）の実行本体。唯一の到達経路は
/// `core.rs::EngineCore` の `UPDATE` 分岐（述語形。
/// `EngineCore::execute_predicate_update_form`）。[`execute_predicate_delete`]
/// と同じ設計（候補行列挙のクロージャ注入・`content_hash_value`／`schema` を
/// 呼び出し元から受け取る）で、適用段のみ `tenant::update_rows_where_unchecked`
/// の read-merge-write（`upsert_typed_rows_unchecked` の `DoUpdate` 腕と同型）に
/// 委譲する。
pub(crate) fn execute_predicate_update(
    storage: &crate::storage::Storage,
    ctx: &PolicyContext,
    bound: &BoundPredicateUpdate,
    ledger_mode: crate::recovery::required_op_id::LedgerMode,
    schema: &TableSchema,
    content_hash_value: &crate::recovery::content_hash::ContentHash,
) -> Result<UpdateOutcome, SqlSurfaceError> {
    let ledger_write = ledger_mode
        .resolve(bound.operation_id())
        .map_err(|_| SqlSurfaceError::MissingOperationId)?;

    let metadata_filters = bound.metadata_filters();
    let expr_filters = bound.expr_filters();
    let or_filters = bound.or_filters();
    let needs_embedding = expr_filters.iter().any(udf_call::references_embedding)
        || or_filters
            .iter()
            .any(crate::sql::where_tree::BoundOrGroup::references_embedding);
    let expr_programs: Vec<crate::sql::expr_program::ExprProgram> = expr_filters
        .iter()
        .map(crate::sql::expr_program::ExprProgram::compile)
        .collect();
    let mut scratch: Vec<crate::sql::expr_program::StackValue> = Vec::new();

    let predicate = |candidate: &crate::tenant::DmlCandidate<'_>| -> Result<bool, SqlSurfaceError> {
        let scanned = row_codec::scan_scalar_columns(schema, candidate.metadata)?;
        if !declarative_filter::matches_all(metadata_filters, &scanned) {
            return Ok(false);
        }
        for (expr, program) in expr_filters.iter().zip(&expr_programs) {
            let references_embedding = udf_call::references_embedding(expr);
            // `dim == 0`（`VECTOR` 列が NULL）の行の NULL 伝播は `program.eval`
            // 自身（`ExprStep::PushVector` の空スライス判定。`sql::expr_program`
            // 参照）が行う（codex-review P1 指摘対応: 静的な式木走査による事前
            // 除外は `CASE` の選ばれない分岐に embedding 参照があるだけの行まで
            // 誤って除外していたため撤去し、評価時点の判定へ一本化した）。
            let embedding: &[f32] = if references_embedding {
                candidate.embedding
            } else {
                &[]
            };
            match program.eval(candidate.id, embedding, &mut scratch)? {
                udf_call::ExprValue::Bool(true) => {}
                // NULL（UNKNOWN）は非該当として扱う（対象ビヘイビア: SQL-26。
                // Issue #921）。
                udf_call::ExprValue::Bool(false) | udf_call::ExprValue::Null => return Ok(false),
                _ => {
                    return Err(SqlSurfaceError::invalid_input(
                        "WHERE expression did not evaluate to a boolean",
                    ))
                }
            }
        }
        // TASK-208・SQL-24（Issue #912）: `WHERE` の OR 群を、既存のメタデータ
        // フィルタ・式述語と同じ述語評価の一部として適用する。
        for group in or_filters {
            let group_embedding: &[f32] = if group.references_embedding() {
                candidate.embedding
            } else {
                &[]
            };
            if !group.matches(
                &scanned,
                candidate.id,
                group_embedding,
                candidate.dim as usize,
                &mut scratch,
            )? {
                return Ok(false);
            }
        }
        Ok(true)
    };

    // `MAX_DML_AFFECTED_ROWS`／`check_dml_affected_rows`（ADR §6「上限 API の
    // 並立（申し送り）」。両上限 API の統合は本 Issue の対象外のまま）。
    let limit = crate::sql::parser::MAX_DML_AFFECTED_ROWS;
    match crate::tenant::update_rows_where_unchecked(
        storage,
        bound.table(),
        ctx,
        ledger_write,
        content_hash_value,
        Some(schema),
        bound.assignments(),
        needs_embedding,
        limit,
        predicate,
    ) {
        Ok(crate::tenant::PredicateDmlOutcome::Applied { rows_affected }) => Ok(UpdateOutcome {
            rows_affected: rows_affected as u64,
        }),
        Ok(crate::tenant::PredicateDmlOutcome::LimitExceeded { count }) => {
            crate::sql::parser::check_dml_affected_rows(count)?;
            Err(SqlSurfaceError::Internal {
                detail: "predicate UPDATE limit check did not reject an over-limit count"
                    .to_string(),
            })
        }
        Err(crate::tenant::PredicateDmlError::Predicate(e)) => Err(e),
        Err(crate::tenant::PredicateDmlError::Write(e)) => Err(map_write_error(e, "update")),
    }
}

/// NoSQL 表層（`wire-server::http::query::insert`。Issue #771・TASK-178・NOSQL-6）
/// の `insert` op が `rows` 配列全体を 1 つの `operation_id` に対応づけて書き込む
/// ための複数行版。SQL-10 の [`execute_insert`]（1 呼び出し = 1 台帳エントリ）とは
/// 異なり、`bounds` 全体を単一の write トランザクション・単一の台帳エントリへ
/// まとめる（`crate::tenant::insert_typed_rows_unchecked`／
/// `crate::recovery::content_hash::for_typed_insert_batch` のドキュメント参照）。
///
/// `bounds` は呼び出し元（wire-server）が JSON `rows[*]` を `BoundInsert` へ
/// 束縛した結果。以下を呼び出し元の責務としてではなく、ここで検証する
/// （engine クレート外から `BoundInsert` を直接構築してガード・整合性検査を
/// 迂回できる経路を作らないため。`execute_insert` と同じ設計判断）:
///
/// - `bounds` が空 → `22000`（`insert_typed_rows_unchecked` 自体は空バッチを
///   `Ok(())` で受理するが、`rows: []` は NoSQL 表層の意味論として拒否する。
///   `core.rs::execute_bound_insert_in_session` の判定順序ドキュメント参照）
/// - 全要素の `table`・`operation_id` が一致しない → `22000`（fail-closed。
///   1 要求 = 1 テーブル・1 `operation_id` という契約を守る）
///
/// `operation_id` 必須化ガード（TASK-92・RECOVER-1）は本関数が自己完結して
/// 適用する（`bounds[0].operation_id` を代表値として使う。上記の一致検証により
/// 全要素で同一であることが保証されている）。`bounds.len() == 1` は
/// [`execute_insert`] へ委譲し、単行の NoSQL insert が SQL-10 単行 INSERT と
/// 同一の台帳ハッシュ空間に属するようにする（表層を跨いだ再送も同じ
/// `(tenant, table, operation_id)` キーで判定される）。
///
/// 可視性は [`execute_insert`] と同じく常に [`crate::storage::Visibility::Private`]
/// に固定する（`Public` 既定は越境露出になるため。上記ドキュメント参照）。
pub fn execute_insert_batch(
    storage: &crate::storage::Storage,
    ctx: &PolicyContext,
    bounds: &[crate::sql::parser::BoundInsert],
    ledger_mode: crate::recovery::required_op_id::LedgerMode,
) -> Result<InsertOutcome, SqlSurfaceError> {
    execute_insert_batch_with_schema(storage, ctx, bounds, ledger_mode, None)
}

/// [`execute_insert_batch`] の実体（`pub(crate)`）。`expected_schema` を追加で
/// 受け取る点のみが異なる（codex-review P1 指摘・PR #823。[`execute_insert_with_schema`]
/// のドキュメント参照。[`execute_insert_batch`] 自身は Issue #771 で公開 API へ
/// 昇格済みのためシグネチャは変更しない）。呼び出し元は
/// `core::EngineCore::execute_bound_insert_in_session`（NoSQL 表層 `rows[]`・
/// NOSQL-6・TASK-178）と `core::EngineCore::execute_insert_form` の `RowBatch`
/// 分岐（SQL 表層の複数行 `VALUES`・SQL-16・TASK-190・Issue #863）の 2 箇所
/// （第 2 の書き込み経路を作らず本関数を共有する設計。両呼び出し元とも
/// `operation_id` 必須化ガード・INDEX-4 上限は各自が本関数呼び出しの前段で
/// 適用済みの `bounds` を渡す）。
pub(crate) fn execute_insert_batch_with_schema(
    storage: &crate::storage::Storage,
    ctx: &PolicyContext,
    bounds: &[crate::sql::parser::BoundInsert],
    ledger_mode: crate::recovery::required_op_id::LedgerMode,
    expected_schema: Option<&crate::catalog::TableSchema>,
) -> Result<InsertOutcome, SqlSurfaceError> {
    use crate::storage::Visibility;

    let Some(first) = bounds.first() else {
        return Err(SqlSurfaceError::invalid_input(
            "INSERT batch must contain at least one row",
        ));
    };
    if bounds.len() == 1 {
        return execute_insert_with_schema(storage, ctx, first, ledger_mode, expected_schema);
    }

    let table = first.table.as_str();
    if bounds
        .iter()
        .any(|b| b.table != table || b.operation_id != first.operation_id)
    {
        return Err(SqlSurfaceError::invalid_input(
            "INSERT batch rows must share the same table and operation_id",
        ));
    }

    let ledger_write = ledger_mode
        .resolve(first.operation_id.as_ref())
        .map_err(|_| SqlSurfaceError::MissingOperationId)?;

    let rows: Vec<(u64, &[crate::row_codec::Value])> =
        bounds.iter().map(|b| (b.id, b.values.as_slice())).collect();

    crate::tenant::insert_typed_rows_unchecked(
        storage,
        table,
        ctx,
        Visibility::Private,
        &rows,
        ledger_write,
        expected_schema,
    )
    .map_err(map_insert_write_error)?;

    let rows_affected = u64::try_from(bounds.len()).map_err(|_| SqlSurfaceError::Internal {
        detail: "insert batch row count overflow".to_string(),
    })?;
    Ok(InsertOutcome {
        rows_affected,
        incremental: None,
    })
}

/// `RETURNING` 句（Issue #873・SQL-21）付き行形 `INSERT`（単一行・複数行
/// `VALUES` の双方）の実行入口。ファイル形（`BoundInsertForm::File`）は
/// 呼び出し元（`core.rs`）が束縛段で `42601` 拒否するため本関数へは到達しない
/// 契約（サーバー側チャンク化行を返す応答形が未定義のため fail-closed）。
/// 既存の書き込み経路（[`execute_insert_batch_with_schema`]）をそのまま通した
/// うえで、書き込んだ値そのもの（`bounds[*].values`。既定値・トリガの類は
/// 存在しないため常に書き込んだ値と一致する）を `RowDescription` へ再読み込み
/// なしで投影する。
///
/// **RLS 再判定（多層防御）**: 挿入行の tenant は常に `ctx.tenant_id()`・
/// visibility は常に `crate::storage::Visibility::Private` 固定
/// （`execute_insert_with_schema` と同じ契約）のため `ctx.is_visible` は通常
/// 常に真だが、[`execute_delete_returning`] と同じ形で再適用し判定経路を
/// 統一する（security.md「テナント境界」多層防御方針）。
pub fn execute_insert_returning(
    storage: &crate::storage::Storage,
    ctx: &PolicyContext,
    bounds: &[crate::sql::parser::BoundInsert],
    ledger_mode: crate::recovery::required_op_id::LedgerMode,
    returning: &[crate::sql::parser::ProjectedColumn],
    schema: &TableSchema,
) -> Result<ReturningOutcome, SqlSurfaceError> {
    // codex-review Low 指摘（PR #873）対応: `column_meta`・`project_row` は
    // いずれも `bounds`（呼び出し元が既に束縛済みの書き込み予定値）・
    // `projection`・`schema`・`ctx` のみに依存する純粋な計算（redb I/O を
    // 伴わない）であり、`returning_bug`／`54000`（バイト予算超過）以外では
    // 失敗しない。書き込み（commit 境界）の *前* に呼ぶことで、投影の構築
    // 失敗が「書き込みは成功したのにエラー応答を返す」（commit 成功境界後の
    // 失敗）経路に紛れ込むのを防ぐ（[`execute_delete_returning`] は捕捉行の
    // 実体が write トランザクション内でしか得られないため `project_row` を
    // 書き込みより前には呼べず、`tenant::delete_row_impl` の `project`
    // コールバックとして commit の**前**（トランザクション内）に呼ばせる、
    // 同じ目的の別経路を使う。codex-review P1 指摘・PR #991 対応。
    // ドキュメント参照）。
    let columns = crate::sql::returning::column_meta(returning, schema)?;
    let is_visible = ctx.is_visible(ctx.tenant_id(), crate::storage::Visibility::Private);
    let mut rows = Vec::new();
    if is_visible {
        let mut budget = 0usize;
        for bound in bounds {
            rows.push(crate::sql::returning::project_row(
                bound.id,
                &bound.values,
                returning,
                &mut budget,
            )?);
        }
    }

    let insert_outcome =
        execute_insert_batch_with_schema(storage, ctx, bounds, ledger_mode, Some(schema))?;

    Ok(ReturningOutcome {
        command: crate::sql::returning::DmlCommand::Insert,
        rows_affected: insert_outcome.rows_affected,
        result: QueryResult { columns, rows },
    })
}

/// SQL 表層 `INSERT INTO <table> (...) VALUES (...)[, ...] ON CONFLICT (id)
/// DO NOTHING | DO UPDATE SET ... USING OPERATION_ID '<id>'`（SQL-20、
/// TASK-193、Issue #872）の実行入口。`bound`（`sql::parser::bind_insert_form`
/// の `Upsert` 分岐が束縛済み）を `crate::tenant::upsert_typed_rows_unchecked`
/// へ委譲する（[`execute_insert`]・[`execute_insert_batch`] と同じ設計）。
///
/// **衝突判定スコープ・応答意味論**: `docs/design/sql-upsert.md` 参照。行 `id`
/// 衝突は本構文では常に `DO NOTHING`／`DO UPDATE` のいずれかへ分岐し、行制約
/// 由来の [`SqlSurfaceError::IdConflict`]（`23505`）は構造的に到達しない
/// （テナント名前空間化された物理キー内の衝突は必ずこの経路で処理される）。
/// `InsertOutcome::rows_affected` は「新規挿入した行数＋更新した行数」（全行
/// `DO NOTHING` で衝突した場合は `0`）。
///
/// `ledger_mode` の解決・`operation_id` 必須化ガード（TASK-92・RECOVER-1）は
/// [`execute_insert`] と同じ契約。
///
/// `bound_schema` は `bound`（`BoundUpsert`）の列インデックス（`DO UPDATE SET`
/// の対象列・`EXCLUDED` 参照列を含む）を解決した時点のスキーマを**必須**で
/// 受け取る（`Option` ではない。codex-review P1 指摘・PR #990。
/// [`execute_insert_with_schema`]・[`execute_insert_batch_with_schema`] は
/// `Option<&TableSchema>`（`None` を許す `pub` 経由路が存在するが、それらは
/// Issue #730・#823 で既に確定した既存契約であり本 PR の対象外）だが、
/// `execute_upsert` は本 PR（Issue #872）で新設される唯一の公開入口であり
/// 後方互換の制約が無いため、スキーマ照合を省略できる経路をそもそも作らない
/// 設計にする。これが無いと、束縛後・書き込み前に同名テーブルが `DROP`・
/// 再作成され列の宣言順・型が入れ替わった場合、`DO UPDATE SET` の列インデックス
/// が別の列を指したまま `crate::tenant::upsert_typed_rows_unchecked` へ渡り、
/// 意図しない列を更新し得る（TOCTOU）。呼び出し元は必ず「束縛直前に取得した
/// スキーマ」を渡すこと（`core::EngineCore::execute_insert_sql` の唯一の
/// 呼び出し元を参照）。
pub fn execute_upsert(
    storage: &crate::storage::Storage,
    ctx: &PolicyContext,
    bound: &BoundUpsert,
    ledger_mode: crate::recovery::required_op_id::LedgerMode,
    bound_schema: &crate::catalog::TableSchema,
) -> Result<InsertOutcome, SqlSurfaceError> {
    use crate::storage::Visibility;

    let ledger_write = ledger_mode
        .resolve(bound.operation_id.as_ref())
        .map_err(|_| SqlSurfaceError::MissingOperationId)?;

    let rows: Vec<(u64, &[crate::row_codec::Value])> = bound
        .rows
        .iter()
        .map(|b| (b.id, b.values.as_slice()))
        .collect();

    // `sql::parser::BoundConflictAction`／`BoundUpsertValue` を
    // `tenant::UpsertAction`／`UpsertSetValue`（`sql` へ依存しない最小表現）へ
    // 変換する（`tenant.rs` モジュールドキュメント参照）。
    let tenant_assignments: Vec<(usize, crate::tenant::UpsertSetValue<'_>)> = match &bound.action {
        BoundConflictAction::DoNothing => Vec::new(),
        BoundConflictAction::DoUpdate(assignments) => assignments
            .iter()
            .map(|(col_idx, value)| {
                let v = match value {
                    BoundUpsertValue::Excluded(src_idx) => {
                        crate::tenant::UpsertSetValue::Excluded(*src_idx)
                    }
                    BoundUpsertValue::Literal(v) => crate::tenant::UpsertSetValue::Literal(v),
                };
                (*col_idx, v)
            })
            .collect(),
    };
    let tenant_action = match &bound.action {
        BoundConflictAction::DoNothing => crate::tenant::UpsertAction::DoNothing,
        BoundConflictAction::DoUpdate(_) => {
            crate::tenant::UpsertAction::DoUpdate(&tenant_assignments)
        }
    };

    let outcome = crate::tenant::upsert_typed_rows_unchecked(
        storage,
        &bound.table,
        ctx,
        Visibility::Private,
        &rows,
        &tenant_action,
        ledger_write,
        Some(bound_schema),
    )
    .map_err(map_insert_write_error)?;

    let rows_affected = outcome
        .inserted
        .checked_add(outcome.updated)
        .ok_or_else(|| SqlSurfaceError::Internal {
            detail: "upsert row count overflow".to_string(),
        })?;
    Ok(InsertOutcome {
        rows_affected,
        incremental: None,
    })
}

/// ファイル形 `INSERT`（TASK-120・対象ビヘイビア: INDEX-1, INDEX-2）を実行する。
/// `sql::parser::bind_insert_form` が判別した `BoundFileInsert` を受け取り、
/// `incremental::index_file` へ委譲する。`embedder` が未設定（`None`）の場合は
/// fail-closed に拒否する（意味のないベクトルが黙って索引化される fail-open を
/// 防ぐ。`core.rs::EngineCore` モジュールドキュメント参照）。
///
/// `IncrementalError` → `SqlSurfaceError` の写像をここに集約する（他の SQL 表層
/// エラー写像と同じ「呼び出し元の型で fail-closed に丸める」方針。security.md
/// 「エラー・ログ経由で他テナントのデータ・存在情報を漏らさない」対応）。
/// 可視性: `operation_id` 必須化ガード（TASK-92・RECOVER-1）は呼び出し元
/// `core::EngineCore::execute_insert_sql` が `sql::allowlist::validate_insert` 経由で
/// 適用済みであり、本関数自体はガードを持たない。クレート外へ公開すると
/// `BoundFileInsert` を直接構築してガードを迂回できるため `pub(crate)` に閉じる
/// （codex-review P1 指摘・PR #221。security.md P0）。行形 `INSERT`
/// （[`execute_insert`]）はガードを関数内部で自己完結して適用するため
/// Issue #730 で公開 API へ昇格した対比がある（本関数は対象外のまま）。
pub(crate) fn execute_file_insert(
    storage: &crate::storage::Storage,
    ctx: &PolicyContext,
    embedder: Option<&dyn crate::embedding::Embedder>,
    config: &crate::incremental::IncrementalConfig,
    bound: &crate::sql::parser::BoundFileInsert,
    ledger_mode: crate::recovery::required_op_id::LedgerMode,
) -> Result<InsertOutcome, SqlSurfaceError> {
    // 行形 `INSERT`（[`execute_insert`]）と同一の契約で `operation_id` を解決し、
    // 台帳書き込み指示（`LedgerWrite`）へ写像する（TASK-93・RECOVER-2）。
    // `LedgerMode::Ledgered` で `operation_id` が無い場合は `23502`（この分岐は
    // `sql::allowlist::validate_insert` が既に拒否済みのため通常到達しない）。
    let ledger_write = ledger_mode
        .resolve(bound.operation_id.as_ref())
        .map_err(|_| SqlSurfaceError::MissingOperationId)?;

    let embedder = embedder.ok_or_else(|| SqlSurfaceError::Internal {
        detail: "no embedder configured for file-form insert".to_string(),
    })?;

    let input = crate::incremental::BoundFileIndexInput {
        table: &bound.table,
        path: &bound.path,
        body: &bound.body,
        template_values: &bound.template_values,
        path_column_index: bound.path_column_index,
        body_column_index: bound.body_column_index,
        vector_column_index: bound.vector_column_index,
    };

    let outcome =
        crate::incremental::index_file(storage, ctx, embedder, config, &input, ledger_write)
            .map_err(map_incremental_error)?;

    Ok(InsertOutcome {
        rows_affected: outcome.rows_replaced as u64,
        incremental: Some(outcome),
    })
}

/// [`crate::incremental::IncrementalError`] を SQL 表層のエラー契約へ写像する
/// （`execute_file_insert` の唯一の呼び出し元。detail はサーバー内部の固定文言・
/// チャンク数等の非機密情報のみで、本文・応答本文は含めない）。
fn map_incremental_error(e: crate::incremental::IncrementalError) -> SqlSurfaceError {
    use crate::incremental::IncrementalError;
    use crate::tenant::TenantWriteError;

    match e {
        IncrementalError::ChunkingTooLarge(detail) => SqlSurfaceError::payload_too_large(detail),
        // サーバー構成の誤り・メモリ逼迫はクライアント入力起因の `54000` ではなく
        // 内部失敗 `XX000` として返す（Cursor Bugbot 指摘・PR #221）。
        IncrementalError::Internal(detail) => SqlSurfaceError::Internal {
            detail: detail.to_string(),
        },
        IncrementalError::Embed(crate::embedding::EmbedError::TooManyInputs { len, max }) => {
            SqlSurfaceError::payload_too_large(format!(
                "embedding batch too large: {len} inputs (max {max})"
            ))
        }
        IncrementalError::Embed(_) => SqlSurfaceError::Internal {
            detail: "embedding failed".to_string(),
        },
        IncrementalError::Write(TenantWriteError::Catalog(CatalogError::TableNotFound(name))) => {
            SqlSurfaceError::UndefinedTable { name }
        }
        IncrementalError::Write(TenantWriteError::Catalog(CatalogError::Invalid(_))) => {
            SqlSurfaceError::invalid_input("insert rejected: invalid row")
        }
        // 台帳照合（TASK-101・RECOVER-10。TASK-94・RECOVER-3 の重複拒否契約を包含する）:
        // ファイル形 INSERT（`replace_typed_rows_by_text_key` 経由）でも行形 INSERT
        // （[`execute_insert`]）と同じ写像を適用する。`_` 節（`XX000`）へ丸めない。
        IncrementalError::Write(TenantWriteError::DuplicateOperationId) => {
            SqlSurfaceError::DuplicateOperationId
        }
        IncrementalError::Write(TenantWriteError::OperationIdContentMismatch) => {
            SqlSurfaceError::OperationIdContentMismatch
        }
        // `PRIMARY KEY`（Issue #903）・UNIQUE 制約（Issue #905。TABLE-16・TASK-204）のテナント内一意性制約違反
        // （`replace_typed_rows_by_text_key` の採番 id 範囲検査。`constraint.rs`
        // ドキュメント参照）。行形 INSERT（[`map_write_error`]）と同じ `23505` へ
        // 写像し、`_` 節（`XX000`）へ丸めない（クライアントが一意制約違反を内部
        // エラーと取り違えないようにする）。
        IncrementalError::Write(TenantWriteError::UniqueViolation) => {
            SqlSurfaceError::unique_violation()
        }
        // `CHECK` 制約違反（TABLE-16・TASK-204、Issue #906）もファイル形 INSERT で
        // 行形 INSERT（[`map_write_error`]）と同じ `23514` へ写像する（`_` 節へ
        // 丸めない）。制約名のみを含む固定文言。
        IncrementalError::Write(TenantWriteError::CheckViolation { constraint }) => {
            SqlSurfaceError::check_violation(constraint)
        }
        IncrementalError::Write(TenantWriteError::CheckEvaluationFailed) => {
            SqlSurfaceError::Internal {
                detail: "check constraint evaluation failed".to_string(),
            }
        }
        // `FOREIGN KEY` 制約違反（`23503`。TABLE-17・TASK-205、Issue #907）: ファイル形
        // `INSERT` の置換（`replace_typed_rows_by_text_key`）で旧チャンク行を削除しようと
        // して参照先側の検査に違反した場合、または新規チャンク行の参照元列が参照元側の
        // 検査に違反した場合。行形 `INSERT`（[`map_write_error`]）と同じ写像にし、`_` 節
        // （`XX000`）へ丸めない。
        IncrementalError::Write(TenantWriteError::ForeignKeyViolation) => {
            SqlSurfaceError::ForeignKeyViolation
        }
        IncrementalError::Write(_) => SqlSurfaceError::Internal {
            detail: "incremental index write failed".to_string(),
        },
        IncrementalError::EmptyChunks => {
            SqlSurfaceError::invalid_input("insert rejected: file body produced no chunks to index")
        }
    }
}

/// [`crate::incremental::BatchIncrementalError`] を SQL 表層のエラー契約へ写像する
/// （TASK-122・対象ビヘイビア: INDEX-4。`core::EngineCore::execute_insert_sql_batch`
/// の唯一の呼び出し元）。一括投入 4 上限（`crate::batch_limits::BatchLimitsError`）は
/// 全 variant が `54000`（`payload_too_large`。ERR-2・TASK-152）へ写像する。個々の
/// ファイルに起因する非上限系の失敗（`Item`）は単一ファイル経路と同じ写像
/// （[`map_incremental_error`]）を再利用し、バッチ内のどのファイルかは detail に
/// 含めない（本文・パス・テナント情報を含めない契約は単一ファイル経路と同一）。
/// 特定ファイルに起因しないバッチ全体の内部エラー（`Internal`。一括バッファの
/// 確保拒否等）は `Internal` 系へ写像する。
pub(crate) fn map_batch_incremental_error(
    e: crate::incremental::BatchIncrementalError,
) -> SqlSurfaceError {
    use crate::incremental::BatchIncrementalError;

    match e {
        BatchIncrementalError::Limits(limits_err) => {
            SqlSurfaceError::payload_too_large(limits_err.to_string())
        }
        BatchIncrementalError::Item { source, .. } => map_incremental_error(source),
        BatchIncrementalError::Internal(detail) => SqlSurfaceError::Internal {
            detail: detail.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Issue #56 レビュー指摘対応・codex P1 の 2 件（`decode_scalar_columns` の
    // 無制限保持・`embedding.to_vec()` の abort し得る確保）に対する境界値テスト。
    // 本体（`execute_statement`）を通した巨大データでの再現は非現実的なため、
    // 上限判定を担う純粋関数を直接検証する（`arena.rs`・`sparse.rs` の
    // 境界値テストと同方針）。

    #[test]
    fn try_accumulate_budget_accepts_up_to_cap_exactly() {
        assert_eq!(try_accumulate_budget(90, 10, 100).unwrap(), 100);
    }

    /// [`try_alloc_array_for_budget`] は要素数だけでなく TEXT 要素の実体
    /// バイト量（`ArrayRef::payload_bytes`）を予算に計上する（Issue #888
    /// レビュー指摘対応: `count * size_of::<String>()` のみでは大きな TEXT
    /// 要素を持つ配列の実バイト量を過小評価し、予算検証を迂回できてしまう）。
    #[test]
    fn try_alloc_array_for_budget_counts_text_element_payload_bytes() {
        let schema = crate::catalog::TableSchema::new(
            "docs",
            vec![crate::catalog::ColumnDef::new(
                "tags",
                ColumnType::Array(
                    crate::catalog::ArrayType::new(crate::catalog::ArrayElemType::Text, 4)
                        .expect("array ty"),
                ),
                true,
            )],
        );
        // 1 要素だが本文は 10,000 バイト。`count * size_of::<String>()`
        // （24 バイト程度）だけでは検出できない大きさにする。
        let big_text = "x".repeat(10_000);
        let values = vec![Value::Array(row_codec::ArrayValue::Text(vec![big_text]))];
        let encoded = row_codec::encode_scalar_columns(&schema, &values).expect("encode scalar");
        let scanned = row_codec::scan_scalar_columns(&schema, &encoded).expect("scan");
        let array_ref = match scanned[0] {
            Some(row_codec::ScalarRef::Array(array_ref)) => array_ref,
            _ => panic!("expected ScalarRef::Array"),
        };
        let mut budget = 0usize;
        let result = try_alloc_array_for_budget(array_ref, &mut budget, 100);
        assert!(
            matches!(result, Err(ArenaError::CapacityExceeded)),
            "large TEXT array element must trip the budget cap"
        );
    }

    #[test]
    fn try_accumulate_budget_rejects_one_byte_over_cap() {
        assert!(matches!(
            try_accumulate_budget(90, 11, 100),
            Err(ArenaError::CapacityExceeded)
        ));
    }

    #[test]
    fn try_accumulate_budget_saturates_instead_of_overflowing() {
        // `saturating_add` により usize::MAX 近傍でもオーバーフローで未定義動作にならず、
        // 必ず `CapacityExceeded` として拒否される（coding-rust.md「整数演算は
        // checked_*／saturating_* を使う」対応）。
        assert!(matches!(
            try_accumulate_budget(usize::MAX - 1, 10, 100),
            Err(ArenaError::CapacityExceeded)
        ));
    }

    // codex-review 指摘（PRRT_kwDOUAKASM6cPLHE）の回帰テスト: `!scalar_prefilter`
    // の precision 経路で可視集合が `core::MAX_SEARCH_K` を超えるかどうかの判定
    // （`k_eff` クランプにより完全な順位列を構築できなくなる境界）。本体
    // （`execute_statement`）を `MAX_SEARCH_K` 超の巨大データで再現するのは
    // 非現実的なため、上限判定を担う純粋関数を直接検証する（`try_accumulate_budget`
    // と同方針）。

    // codex-review P1・Bugbot 指摘（PR #991）の回帰テスト: DELETE RETURNING が
    // 削除直前の既存行を捕捉する際のデコード失敗（`tenant::TenantWriteError::
    // CapturedRowDecodeFailed`）は、送信されたクライアント入力の不正ではなく
    // サーバー内部事象のため、`22000`（`insert rejected: invalid row`）ではなく
    // `XX000`（内部事象）へ写像されなければならない。旧実装は
    // `TenantWriteError::Storage`/`Catalog(Invalid)` を共用していたため
    // `map_insert_write_error` の `22000` アームに誤って丸め込まれていた。
    #[test]
    fn map_insert_write_error_maps_captured_row_decode_failed_to_internal_not_invalid_input() {
        let mapped =
            map_insert_write_error(crate::tenant::TenantWriteError::CapturedRowDecodeFailed(
                "test decode failure".to_string(),
            ));
        assert!(
            matches!(mapped, SqlSurfaceError::Internal { .. }),
            "CapturedRowDecodeFailed はクライアント入力不正（22000）ではなく \
             内部事象（XX000）へ写像されるべき: {mapped:?}"
        );
    }

    #[test]
    fn precision_completeness_unbounded_false_when_recall_mode() {
        // `is_precision == false`（recall）は本経路自体を通らないため常に `false`。
        assert!(!precision_completeness_unbounded(
            false,
            false,
            core::MAX_SEARCH_K + 1
        ));
    }

    #[test]
    fn precision_completeness_unbounded_false_when_scalar_prefilter() {
        // SCALAR 先行経路は `k_eff = bound.limit.max(2)` を使い、可視集合の広さに
        // 依存しないため常に `false`。
        assert!(!precision_completeness_unbounded(
            true,
            true,
            core::MAX_SEARCH_K + 1
        ));
    }

    #[test]
    fn precision_completeness_unbounded_false_at_max_search_k_boundary() {
        // 可視集合が `MAX_SEARCH_K` ちょうどなら `k_eff` はクランプされず完全な
        // 順位列を取得できるため `false`。
        assert!(!precision_completeness_unbounded(
            true,
            false,
            core::MAX_SEARCH_K
        ));
    }

    #[test]
    fn precision_completeness_unbounded_true_one_over_max_search_k() {
        // 1 件でも超過すれば `k_eff` がクランプされ完全性を保証できなくなるため
        // `true`（呼び出し元は空集合へ倒す）。
        assert!(precision_completeness_unbounded(
            true,
            false,
            core::MAX_SEARCH_K + 1
        ));
    }

    #[test]
    fn try_clone_embedding_copies_values_without_aliasing_source() {
        let source = vec![1.0f32, 2.0, 3.0];
        let cloned = try_clone_embedding(&source).expect("small copy must succeed");
        assert_eq!(cloned, source);
    }

    #[test]
    fn try_clone_embedding_handles_empty_slice() {
        let cloned = try_clone_embedding(&[]).expect("empty copy must succeed");
        assert!(cloned.is_empty());
    }

    #[test]
    fn try_clone_text_copies_without_aliasing_source() {
        let source = "hello world".to_string();
        let cloned = try_clone_text(&source).expect("small copy must succeed");
        assert_eq!(cloned, source);
    }

    #[test]
    fn try_clone_text_handles_empty_string() {
        let cloned = try_clone_text("").expect("empty copy must succeed");
        assert!(cloned.is_empty());
    }

    // codex-review P1 指摘（PR #823）の回帰テスト: `execute_insert_batch_with_schema`／
    // `execute_insert_with_schema`（`core::EngineCore::execute_bound_insert_in_session`
    // の実書き込み経路。`tenant::insert_typed_row_unchecked`／
    // `insert_typed_rows_unchecked` のドキュメント参照）が、`expected_schema` に
    // 渡した束縛時点のスキーマと、実際の write トランザクションで再取得したスキーマの
    // 不一致を検出することを固定する。
    //
    // シナリオ: `TEXT` 列 2 個（`lang`・`title`）を持つテーブルへ、`schema_before`
    // （`lang` が先・`title` が後）の列順で束縛した `values` を渡す。書き込み直前に
    // `DROP TABLE` → 同名・同じ `VECTOR` 列位置/次元だが `TEXT` 2 列の宣言順だけを
    // 入れ替えて `CREATE TABLE` した状態（`schema_after`）を実際のスキーマとして
    // 用意する。`expected_schema` チェックが無ければ `insert_typed_row_unchecked` は
    // `schema_after`（新しい列順）を使って `values` を位置ベースで再解釈し、
    // `lang` の値が `title` 列へ（逆も同様）誤って保存され得る（`VECTOR` 列の位置・
    // 次元は不変のため `validate_embedding_dim` はこの入れ替えを検出しない）。
    mod bound_schema_mismatch {
        use crate::catalog::{ColumnDef, ColumnType, TableSchema};
        use crate::policy::PolicyContext;
        use crate::recovery::required_op_id::{LedgerMode, OperationId};
        use crate::row_codec::Value;
        use crate::sql::parser::BoundInsert;
        use crate::storage::Storage;
        use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

        const TABLE: &str = "docs";

        fn schema_before() -> TableSchema {
            TableSchema::new(
                TABLE,
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("lang", ColumnType::Text, false),
                    ColumnDef::new("title", ColumnType::Text, false),
                ],
            )
        }

        /// `schema_before` と同じ `VECTOR` 列位置・次元・列名集合だが、`TEXT` 2 列
        /// （`lang`・`title`）の宣言順だけを入れ替えたスキーマ（`DROP TABLE` →
        /// 再 `CREATE TABLE` で再現する、production の TOCTOU シナリオそのもの）。
        fn schema_after_swap() -> TableSchema {
            TableSchema::new(
                TABLE,
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("title", ColumnType::Text, false),
                    ColumnDef::new("lang", ColumnType::Text, false),
                ],
            )
        }

        fn bound(id: u64, op_id: &OperationId) -> BoundInsert {
            // `schema_before` の列順（embedding は VECTOR 列のためスキップ・lang→title）
            // で束縛した値配列。
            BoundInsert {
                table: TABLE.to_string(),
                id,
                values: vec![
                    Value::Vector(vec![1.0, 0.0]),
                    Value::Text("ja".to_string()),
                    Value::Text("hello".to_string()),
                ],
                operation_id: Some(op_id.clone()),
            }
        }

        #[test]
        fn execute_insert_with_schema_rejects_stale_schema_after_column_reorder() {
            let path = unique_db_path("exec-insert-schema-mismatch-single");
            let _guard = CleanupGuard(path.clone());
            let storage = Storage::open(&path).expect("open storage");
            storage
                .create_table(&schema_before())
                .expect("create table");
            storage.drop_table(TABLE).expect("drop table");
            storage
                .create_table(&schema_after_swap())
                .expect("recreate table with swapped column order");

            let ctx = PolicyContext::new("tenant-a").expect("valid tenant ctx");
            let op_id = OperationId::parse("op-1").expect("valid operation_id");
            let bound_row = bound(1, &op_id);

            let before = schema_before();
            let err = super::super::execute_insert_with_schema(
                &storage,
                &ctx,
                &bound_row,
                LedgerMode::Ledgered,
                Some(&before),
            )
            .expect_err("stale bound schema must be rejected, not silently written");
            assert_eq!(
                err.wire_code(),
                "22000",
                "schema mismatch must map to the existing invalid-row classification"
            );
        }

        #[test]
        fn execute_insert_with_schema_accepts_matching_schema() {
            // 対照テスト: `expected_schema` が実際のスキーマと一致する（通常経路・
            // 競合なし）場合は従来どおり書き込みが成功する（fail-closed 化が
            // 通常経路を巻き込んで壊していないことを確認する）。
            let path = unique_db_path("exec-insert-schema-mismatch-single-ok");
            let _guard = CleanupGuard(path.clone());
            let storage = Storage::open(&path).expect("open storage");
            storage
                .create_table(&schema_before())
                .expect("create table");

            let ctx = PolicyContext::new("tenant-a").expect("valid tenant ctx");
            let op_id = OperationId::parse("op-1").expect("valid operation_id");
            let bound_row = bound(1, &op_id);

            let expected = schema_before();
            let outcome = super::super::execute_insert_with_schema(
                &storage,
                &ctx,
                &bound_row,
                LedgerMode::Ledgered,
                Some(&expected),
            )
            .expect("matching schema must still succeed");
            assert_eq!(outcome.rows_affected, 1);
        }

        #[test]
        fn execute_insert_batch_with_schema_rejects_stale_schema_after_column_reorder() {
            let path = unique_db_path("exec-insert-schema-mismatch-batch");
            let _guard = CleanupGuard(path.clone());
            let storage = Storage::open(&path).expect("open storage");
            storage
                .create_table(&schema_before())
                .expect("create table");
            storage.drop_table(TABLE).expect("drop table");
            storage
                .create_table(&schema_after_swap())
                .expect("recreate table with swapped column order");

            let ctx = PolicyContext::new("tenant-a").expect("valid tenant ctx");
            let op_id = OperationId::parse("op-1").expect("valid operation_id");
            let bounds = vec![bound(1, &op_id), bound(2, &op_id)];

            let before = schema_before();
            let err = super::super::execute_insert_batch_with_schema(
                &storage,
                &ctx,
                &bounds,
                LedgerMode::Ledgered,
                Some(&before),
            )
            .expect_err("stale bound schema must be rejected for multi-row batches too");
            assert_eq!(err.wire_code(), "22000");
        }

        // codex-review P1 指摘（PR #990・Issue #872）の回帰テスト: `execute_upsert`
        // は `bound_schema` を必須引数（`Option` ではない）として要求し、
        // `DO UPDATE SET` の対象列インデックスを解決した束縛時点のスキーマと、
        // write トランザクション内で再取得した実際のスキーマとの不一致を検出する。
        // 上記 `execute_insert_with_schema_rejects_stale_schema_after_column_reorder`
        // と同じシナリオ（`TEXT` 2 列の宣言順入れ替え）で、`SET title = 'ja'`
        // （束縛時点では `title` 列を指す col_idx）が再作成後のスキーマでは
        // `lang` 列を指してしまう TOCTOU を固定する。
        #[test]
        fn execute_upsert_rejects_stale_schema_after_column_reorder() {
            use crate::sql::parser::{BoundConflictAction, BoundUpsert, BoundUpsertValue};

            let path = unique_db_path("exec-upsert-schema-mismatch");
            let _guard = CleanupGuard(path.clone());
            let storage = Storage::open(&path).expect("open storage");
            storage
                .create_table(&schema_before())
                .expect("create table");
            // 既存行を 1 件用意する（DO UPDATE 分岐に到達させるため）。
            let ctx = PolicyContext::new("tenant-a").expect("valid tenant ctx");
            let op_id = OperationId::parse("op-1").expect("valid operation_id");
            super::super::execute_insert(&storage, &ctx, &bound(1, &op_id), LedgerMode::Ledgered)
                .expect("seed row insert must succeed");

            storage.drop_table(TABLE).expect("drop table");
            storage
                .create_table(&schema_after_swap())
                .expect("recreate table with swapped column order");
            // 再作成直後にテナント名前空間つきの物理キーへ既存行を再投入し
            // （`schema_after_swap` の列順で）、`DO UPDATE` の衝突分岐へ到達
            // させる（`TABLE-12` のテナント整合検査を満たすため
            // `execute_insert` を経由する）。
            let seed_op_id = OperationId::parse("op-seed").expect("valid operation_id");
            super::super::execute_insert(
                &storage,
                &ctx,
                &BoundInsert {
                    table: TABLE.to_string(),
                    id: 1,
                    values: vec![
                        Value::Vector(vec![1.0, 0.0]),
                        Value::Text("hello".to_string()),
                        Value::Text("ja".to_string()),
                    ],
                    operation_id: Some(seed_op_id),
                },
                LedgerMode::Ledgered,
            )
            .expect("reseed row insert under swapped schema must succeed");

            // 束縛時点のスキーマ（`schema_before`）で `SET title = 'stale'` を
            // 束縛する（`title` の col_idx は `schema_before` では 2）。
            let upsert_op_id = OperationId::parse("op-2").expect("valid operation_id");
            let stale_bound = BoundUpsert {
                table: TABLE.to_string(),
                rows: vec![BoundInsert {
                    table: TABLE.to_string(),
                    id: 1,
                    values: vec![
                        Value::Vector(vec![0.0, 1.0]),
                        Value::Text("en".to_string()),
                        Value::Text("world".to_string()),
                    ],
                    operation_id: Some(upsert_op_id.clone()),
                }],
                action: BoundConflictAction::DoUpdate(vec![(
                    2,
                    BoundUpsertValue::Literal(Value::Text("stale".to_string())),
                )]),
                operation_id: Some(upsert_op_id),
            };

            let before_for_call = schema_before();
            let err = super::super::execute_upsert(
                &storage,
                &ctx,
                &stale_bound,
                LedgerMode::Ledgered,
                &before_for_call,
            )
            .expect_err("stale bound schema must be rejected for upsert too");
            assert_eq!(err.wire_code(), "22000");
        }
    }

    // Issue #453・3.5 節「契約上の注記」の回帰テスト: `defer_projection` は
    // Top-k 確定後の行だけをデコードするため、Top-k **に含まれない**可視行の
    // スカラーペイロード破損は当該クエリでは検出されなくなる（`SELECT id` 高速
    // 経路・Issue #314・#363 と同じ既存の挙動）。一方 Top-k **に含まれる**行の
    // 破損は従来どおり fail-closed に検出する。`catalog::user_rows_table_def` は
    // `pub(crate)` のため結合テスト（`tests/`）からは直接行を破損できず、本テストは
    // crate 内単体テストとして `arena.rs::tests` の破損注入手法（生の write
    // トランザクションで `encode_row` 済みバイト列を直接書き込む）を踏襲する。
    mod deferred_projection_corruption {
        use crate::catalog::{self, ColumnDef, ColumnType, TableSchema};
        use crate::core::EngineCore;
        use crate::kernel::CpuScalarProvider;
        use crate::policy::PolicyContext;
        use crate::recovery::required_op_id::OperationId;
        use crate::row_codec::Value;
        use crate::sql::allowlist::SqlSurfaceError;
        use crate::storage::{RowInput, Storage, Visibility};
        use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

        const TENANT: &str = "tenant-a";

        fn op_id(label: &str) -> OperationId {
            OperationId::parse(label).expect("valid operation id")
        }

        fn schema() -> TableSchema {
            TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("body", ColumnType::Text, false),
                ],
            )
        }

        /// `id` の行を正規の `insert_typed_row` 経由で投入したうえで、その
        /// metadata（`body` 列のペイロード）だけを不正 UTF-8 へ書き換えた raw
        /// バイト列で直接上書きする（`arena.rs::tests` の次元不一致注入と同じ、
        /// 検証を経由しない生の write トランザクション）。
        fn seed_then_corrupt_body(
            storage: &Storage,
            ctx: &PolicyContext,
            id: u64,
            embedding: [f32; 2],
            seed_label: &str,
        ) {
            crate::tenant::insert_typed_row(
                storage,
                "docs",
                ctx,
                id,
                Visibility::Public,
                &[
                    Value::Vector(embedding.to_vec()),
                    Value::Text("placeholder".to_string()),
                ],
                &op_id(seed_label),
            )
            .expect("seed row before corruption");

            let mut metadata = crate::row_codec::encode_scalar_columns(
                &schema(),
                &[Value::Null, Value::Text("placeholder".to_string())],
            )
            .expect("encode scalar metadata");
            // `metadata` レイアウト: [presence=NULL (embedding列はスキップされる
            // ため実際は body 列の presence から始まる)] presence(1) + len(4 LE) +
            // text_bytes。`encode_scalar_columns` は VECTOR 列をスキップするため
            // 先頭バイトは body 列の presence タグ（`PRESENCE_VALUE`）になる。
            // 末尾のテキストバイトの 1 バイトを不正な UTF-8 継続バイトへ書き換える
            // （宣言長・presence タグは変えないため構造検証自体は通り、UTF-8
            // 検証でのみ失敗する）。
            let last = metadata.len() - 1;
            metadata[last] = 0xFF;

            let encoded = crate::storage::encode_row(&RowInput {
                tenant_id: TENANT,
                visibility: Visibility::Public,
                embedding: &embedding,
                metadata: &metadata,
            })
            .expect("encode corrupted row");

            let write_txn = storage.db().begin_write().expect("begin_write");
            {
                let row_table_name = catalog::user_rows_table_name("docs");
                let mut row_table = write_txn
                    .open_table(catalog::user_rows_table_def(&row_table_name))
                    .expect("open row table");
                row_table
                    .insert((TENANT, id), encoded.as_slice())
                    .expect("overwrite row with corrupted metadata");
            }
            write_txn.commit().expect("commit corrupted row");
        }

        #[test]
        fn corrupted_row_outside_top_k_is_not_detected_and_query_succeeds() {
            let path = unique_db_path("deferred-projection-corrupt-outside-topk");
            let _guard = CleanupGuard(path.clone());
            let storage = Storage::open(&path).expect("open storage");
            storage.create_table(&schema()).expect("create table");
            let ctx = PolicyContext::new(TENANT).expect("valid tenant");

            // id=1 は距離最小（Top-1）、id=2 は破損させたうえで距離最大（Top-k
            // 圏外）に置く。`LIMIT 1` で Top-k に含まれない id=2 の破損が
            // 観測されないことを固定する。
            crate::tenant::insert_typed_row(
                &storage,
                "docs",
                &ctx,
                1,
                Visibility::Public,
                &[
                    Value::Vector(vec![1.0, 0.0]),
                    Value::Text("winner".to_string()),
                ],
                &op_id("outside-topk-1"),
            )
            .expect("insert winning row");
            seed_then_corrupt_body(&storage, &ctx, 2, [0.0, 1.0], "outside-topk-2");

            let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
            let result = core
                .execute_sql(
                    &ctx,
                    "SELECT id, body FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 1",
                )
                .expect("query must succeed: corrupted row is outside top-k");
            assert_eq!(result.rows.len(), 1);
            assert_eq!(result.rows[0].id, 1);
        }

        #[test]
        fn corrupted_row_inside_top_k_is_rejected_fail_closed() {
            let path = unique_db_path("deferred-projection-corrupt-inside-topk");
            let _guard = CleanupGuard(path.clone());
            let storage = Storage::open(&path).expect("open storage");
            storage.create_table(&schema()).expect("create table");
            let ctx = PolicyContext::new(TENANT).expect("valid tenant");

            // id=1 を破損させたうえで距離最小（Top-1 圏内）に置く。
            seed_then_corrupt_body(&storage, &ctx, 1, [1.0, 0.0], "inside-topk-1");
            crate::tenant::insert_typed_row(
                &storage,
                "docs",
                &ctx,
                2,
                Visibility::Public,
                &[
                    Value::Vector(vec![0.0, 1.0]),
                    Value::Text("loser".to_string()),
                ],
                &op_id("inside-topk-2"),
            )
            .expect("insert losing row");

            let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
            let err = core
                .execute_sql(
                    &ctx,
                    "SELECT id, body FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 1",
                )
                .expect_err("query must fail: corrupted row is inside top-k");
            assert!(
                matches!(err, SqlSurfaceError::Internal { .. }),
                "corruption must be rejected fail-closed as an internal error, got {err:?}"
            );
        }
    }
}
