//! ウィンドウ関数（`ROW_NUMBER`/`RANK`/`DENSE_RANK` と `OVER` 付き集計。SQL-30・
//! TASK-214、Issue #930）の実行本体。広域取得（[`crate::sql::scan`]）の投影に
//! `<func>(...) OVER (PARTITION BY ... ORDER BY ...)` を加えたもので、
//! `sql::scan::execute_scan_with_budget` から `bound.windows()` が空でない場合に
//! dispatch される（`sql::scan` モジュール本体への変更を dispatch 1 行に留める
//! ことで、他 PR との衝突を最小化する設計判断）。
//!
//! 責務境界: [`crate::sql::parser::bind_scan_with_dummy_flags`] が返す
//! [`crate::sql::parser::BoundScan`]（`windows` が非空）を受け取り、2 段階で
//! 実行する。
//!
//! 1. **materialize 段**（[`materialize_rows`]）: 対象テーブルを 1 回、`LIMIT` に
//!    よる早期終了なし（結果行数上限 `result_cap` の超過確定時のみ `54000` で中断。
//!    Issue #1797）で走査し、可視かつ `WHERE` を満たす行**全体**について
//!    ウィンドウ項目ごとの `PARTITION BY`／`ORDER BY` キー・集計引数の値を集める。
//!    RLS 適用順序（ヘッダのみで可視性判定 → TABLE-12 のキー/ヘッダ tenant 整合
//!    検査 → 必要範囲のみのデコード → SCALAR 段〔`WHERE`〕→ 可視性の再適用）は
//!    `sql::scan`／`sql::aggregate` の走査ループと同一の規約を踏襲する
//!    （`.claude/rules/security.md`「テナント境界（P0）」）。不可視行・`WHERE`
//!    不一致行はウィンドウ計算の母集合に一切現れない（RLS-7・RLS-8 のウィンドウ
//!    版）。
//! 2. **投影段**（[`build_result`]）: ウィンドウ以外の投影・`LIMIT`／`OFFSET` の
//!    適用は、同じ `WHERE`・`limit`・`offset` を持つ `windows` 空の [`BoundScan`]
//!    複製を [`crate::sql::scan::execute_scan`] へそのまま渡すことで、既存の
//!    実行器（RLS 適用順序・早期終了・結果バイト予算どれも同一）をそのまま再利用
//!    する（第 2 の実行器を作らない）。同じ `read_txn`（同一スナップショット）で
//!    呼ぶため、物理走査順は materialize 段と一致する。ウィンドウ列は
//!    [`crate::sql::allowlist::WindowSelectItem::position`] に基づいて元の
//!    SELECT リスト位置へ差し込む。
//!
//! 集計の数値計算（`SUM`/`AVG`/`MIN`/`MAX` の NULL 契約・桁あふれ検査・NUMERIC の
//! scale 保持）は [`crate::sql::aggregate::Accumulator`] を再利用し、集計 SELECT
//! （`sql::aggregate`）と同一の規約を共有する（第 2 の集計実装を作らない）。
//! ウィンドウ項目の集計引数は構文段（`sql::allowlist::Parser::parse_window_item`）で
//! 裸の列参照・`*` のみに制限しているため（複合式は対象外）、束縛結果
//! （[`AggregateInput`]）に `ScalarExpr` は現れない——本モジュールはこの前提のもとで
//! のみ `Accumulator::observe` を呼ぶ。`observe` は借用形（`scanned: &[Option<
//! row_codec::ScalarRef>]`・`vector: &RowVector`）を要求するため、materialize 段で
//! owned 化した値を、観測のたびにその場で最小限の借用形へ組み立て直す
//! （[`observe_window_row`]）。
//!
//! 文全体のスカラー `ORDER BY`（Issue #1189）は、materialize 段で並べ替えキーを
//! 集め、[`compute_output_order`] が base scan と同一の比較器で出力順（順列）を
//! 決め、投影段がその順列で base scan の出力行とウィンドウ値を対応付ける。
//!
//! `LAG`／`LEAD`（SQL-64 (1)・Issue #1881）は、materialize 段が行ごとに「参照先になったとき
//! 返す値」「現在行で評価したオフセット」「現在行で評価した既定値」（[`OffsetRowValues`]）を
//! 集め、評価段（[`evaluate_offset_partition`]）がパーティション内の並び（`ORDER BY`→`id`→
//! `seq`）の上でオフセット先の行を引く。peer・フレームには依存しない。PostgreSQL の
//! 評価規則（オフセットは全行で評価・値は参照先の行で評価・既定値はパーティション外のときだけ
//! 現在行で評価）に合わせるため、値と既定値の評価エラー（`22xxx`）は保持して使用時に返す。
//! 母集合は他のウィンドウ関数と同じ（RLS・`WHERE` を通った行のみ）で、不可視行は参照先に
//! ならない。
//!
//! `ROWS` フレーム（SQL-64 (2)・Issue #1889）は、実行開始時（行を引く前）に
//! [`resolve_frames`] がオフセット式を評価して解決済みフレーム（[`ResolvedRowsFrame`]）にし、
//! 集計だけが [`evaluate_rows_frame_partition`] で行ごとのフレーム（`EXCLUDE` 込み）を評価する。
//! 順位関数と `LAG`／`LEAD` はフレームを無視する（PostgreSQL と同じ）が、`LAG`／`LEAD` は
//! オフセットの NULL／負を検査する。位置範囲の計算（[`rows_frame_bounds`]）はモード非依存の
//! 形で、first_value 等（#1883）や RANGE／GROUPS（#1890／#1891）が再利用する。
//!
//! `RANGE` フレーム（SQL-64 (2)・Issue #1890）は、同じく [`resolve_frames`] が型付きのオフセット
//! （[`RangeOffset`]。束縛段が `ORDER BY` キー型との照合と強制変換を済ませている）を解決し、
//! 集計だけが [`evaluate_range_frame_partition`] で評価する。境界位置は [`range_frame_bounds`] が
//! peer グループと、非 NULL 区間上の二分探索（PostgreSQL の `in_range` 判定の移植）で求める。
//! 負・NaN のオフセット（`22013`）は PostgreSQL と同じく非 NULL キーの行で境界を求める時点まで遅延する。
//!
//! `GROUPS` フレーム（SQL-64 (2)・Issue #1891）は `ROWS` と同じく [`resolve_frames`] がオフセットを
//! 解決・検査（検査時点も `ROWS` と同じ）し、集計だけが [`evaluate_groups_frame_partition`] で評価する。
//! オフセットは peer グループ数で、境界は [`groups_frame_bounds`] が前進のみの 2 本のカーソル
//! （[`GroupCursor`]）で求める。グループ境界の配列は確保しないため状態予算は変わらない。
//!
//! `first_value`／`last_value`／`nth_value`（SQL-64 (3)・Issue #1883）は、`LAG`／`LEAD` と同じ
//! 引数経路（[`OffsetRowValues`]。`nth_value` の `n` はオフセット欄）で参照先の値を集め、評価段
//! （[`evaluate_value_frame_partition`]）が行ごとのフレーム（既定は `RANGE BETWEEN UNBOUNDED
//! PRECEDING AND CURRENT ROW`。明示の `ROWS`／`RANGE`／`GROUPS`・`EXCLUDE` も評価する）から
//! `EXCLUDE` 対象を除いた先頭・末尾・n 番目の行を [`pick_in_frame`] で O(1) に引く。境界の計算は
//! 集計と同じ [`rows_frame_bounds`]／[`groups_frame_bounds`]／[`range_frame_bounds`] を再利用する。
//!
//! 対象外（`42601`／`54000`／`0A000` で fail-closed に拒否。allowlist・parser 側で既に拒否
//! 済みの形も含む）:
//! 名前付きウィンドウ・`FILTER (...)`・関数内 `DISTINCT`・複合式の引数・
//! ベクトル順位付け（`ORDER BY embedding <=> ...`）／`USING PLAN` との併用・
//! ウィンドウ別名による文全体の `ORDER BY`・`GROUP BY`／集計 SELECT との併用。

use crate::catalog::{self, ColumnType, TableSchema};
use crate::declarative_filter;
use crate::policy::PolicyContext;
use crate::row_codec;
use crate::sql::aggregate::{self, Accumulator, DecodeTier, RowVector};
use crate::sql::allowlist::{SqlSurfaceError, WindowFrameExclusion, WindowFrameMode, WindowFunc};
use crate::sql::exec::{Cell, ColumnMeta, QueryResult, ResultRow};
use crate::sql::expr_program::{ExprProgram, StackValue};
use crate::sql::order_value::{
    compare_nullable, compare_statement_order, extract_order_value_ref, scalar_key_ref_to_owned,
    NullPlacement, OrderValue, ScalarKeyRef,
};
use crate::sql::parser::{
    AggregateInput, BoundFrameBound, BoundFrameOffset, BoundScan, BoundWindowItem,
    BoundWindowOffsetArgs, BoundWindowValue, ProjectedColumn, RangeOffsetKind, WindowKeyKind,
    WindowKeyRef,
};
use crate::sql::scan::{computed_value_to_cell, push_scalar_column_cell};
use crate::sql::udf_call::{self, ExprValue};
use crate::storage::{self, StorageError};
use redb::ReadableTable;
use std::collections::HashMap;

/// パーティション数の上限（受入基準 2）。`sql::group_by` の `GROUP BY` グループ数
/// 上限（SQL-14）と同値の実装既定値を流用する。
const MAX_WINDOW_PARTITIONS: usize = crate::sql::group_by::MAX_GROUPS;

/// クエリ全体でウィンドウ計算のために materialize してよい行数の合計上限
/// （受入基準 2。無制限確保を避ける。`.claude/rules/security.md`「不安全な設計」
/// 対応）。実装既定値。
const MAX_WINDOW_ROWS: usize = 1_000_000;

/// 1 パーティションが保持してよい行数の上限（受入基準 2）。`MAX_WINDOW_ROWS` と
/// 同値の実装既定値。
const MAX_WINDOW_FRAME_ROWS: usize = 1_000_000;

/// キー値・引数値・行ごとの固定オーバーヘッドの累計バイト数上限（受入基準 2）。
const MAX_WINDOW_STATE_BYTES: usize = 64 * 1024 * 1024;

/// 型不整合・実装バグの検出用（untrusted 入力起因ではないため `wire_code` は
/// `XX000`。[`crate::sql::aggregate::accumulator_bug`] と同方針）。
fn window_bug(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: format!("window scan tier/value mismatch: {detail}"),
    }
}

fn storage_internal(e: impl Into<StorageError>) -> SqlSurfaceError {
    let _: StorageError = e.into();
    SqlSurfaceError::Internal {
        detail: "window scan row scan failed".to_string(),
    }
}

/// `current` に `add` を加えた累計が `cap` を超えないことを確保前に検証する
/// （`sql::scan` の同名ヘルパーと同方針）。
fn try_accumulate_state_budget(
    current: usize,
    add: usize,
    cap: usize,
) -> Result<usize, SqlSurfaceError> {
    let next = current
        .checked_add(add)
        .ok_or_else(|| SqlSurfaceError::payload_too_large("window state size overflowed"))?;
    if next > cap {
        return Err(SqlSurfaceError::payload_too_large(
            "window state exceeds the allowed total size",
        ));
    }
    Ok(next)
}

/// `current` に `add` を加えた累計が `cap`（[`build_result`] へ渡された
/// `max_result_bytes`）を超えないことを確保前に検証する（`sql::scan` の
/// `try_accumulate_budget` と同方針。[`try_accumulate_state_budget`] とは対象の
/// 予算が異なる別枠のため、混同を避けるため別関数にしている）。
fn try_accumulate_window_result_budget(
    current: usize,
    add: usize,
    cap: usize,
) -> Result<usize, SqlSurfaceError> {
    let next = current.saturating_add(add);
    if next > cap {
        return Err(SqlSurfaceError::payload_too_large(
            "window scan result exceeds capacity",
        ));
    }
    Ok(next)
}

/// 新規パーティション追加前に [`MAX_WINDOW_PARTITIONS`] 上限を検査する
/// （受入基準 2）。`cap` を引数化しているのは、実データを大量投入せずに
/// 上限判定ロジックそのものを単体テストできるようにするため（本体は常に
/// [`MAX_WINDOW_PARTITIONS`] を渡す。PR #930 最終レビュー指摘 2）。
fn check_new_partition_capacity(
    existing_partition_count: usize,
    cap: usize,
) -> Result<(), SqlSurfaceError> {
    if existing_partition_count >= cap {
        return Err(SqlSurfaceError::payload_too_large(
            "window PARTITION BY produces too many partitions",
        ));
    }
    Ok(())
}

/// 1 パーティションの行数が [`MAX_WINDOW_FRAME_ROWS`] 上限を超えないことを
/// 検査する（受入基準 2。`cap` 引数化の理由は
/// [`check_new_partition_capacity`] と同じ）。
fn check_partition_row_count(count: usize, cap: usize) -> Result<(), SqlSurfaceError> {
    if count > cap {
        return Err(SqlSurfaceError::payload_too_large(
            "window partition exceeds the allowed row count",
        ));
    }
    Ok(())
}

/// materialize 段で集計済みの総行数が [`MAX_WINDOW_ROWS`] 上限を超えないことを
/// 検査する（受入基準 2。`cap` 引数化の理由は [`check_new_partition_capacity`]
/// と同じ）。
fn check_total_row_count(count: usize, cap: usize) -> Result<(), SqlSurfaceError> {
    if count > cap {
        return Err(SqlSurfaceError::payload_too_large(
            "window scan materializes too many rows",
        ));
    }
    Ok(())
}

/// ウィンドウの `PARTITION BY`／`ORDER BY` キー 1 つの値（[`WindowKeyRef`] の
/// 実行時表現。NULL は `Option::None` で表す）。
#[derive(Debug, Clone)]
enum WindowKeyValue {
    Id(u64),
    Text(String),
    Integer(i32),
    BigInt(i64),
    Real(f32),
    Double(f64),
    Boolean(bool),
    Date(i32),
    Timestamp(i64),
    Time(i64),
    /// `TIMESTAMPTZ`（UTC マイクロ秒。Issue #1576）。
    TimestampTz(i64),
    Numeric(crate::numeric::Decimal),
    Uuid(crate::uuid::Uuid),
}

/// ウィンドウ集計項目の引数値（実行時表現。SQL-30・TASK-214）。構文段が引数を
/// 裸の列参照・`*` のみに制限しているため、束縛結果は必ずこの網羅で尽くされる
/// （複合式は現れない）。
#[derive(Debug, Clone)]
enum WindowInputValue {
    /// 順位関数（`ROW_NUMBER`/`RANK`/`DENSE_RANK`）。引数を持たない。
    None,
    /// `COUNT(*)`・`SUM`/`AVG`/`MIN`/`MAX(id)` 以外の `COUNT(id)`。
    AllVisibleOrId,
    /// `VECTOR` 列の裸参照（`COUNT` 限定）。embedding 自体は不要で dim のみ。
    VectorPresence(u32),
    Text(Option<String>),
    Integer(Option<i32>),
    BigInt(Option<i64>),
    Real(Option<f32>),
    Double(Option<f64>),
    Date(Option<i32>),
    Timestamp(Option<i64>),
    Time(Option<i64>),
    TimestampTz(Option<i64>),
    Numeric(Option<crate::numeric::Decimal>),
    /// `BOOLEAN`／`ARRAY`／`BYTEA`／`JSON`／`JSONB`／`ENUM`／`UUID` 列の裸参照
    /// （いずれも `COUNT` 限定。`Accumulator::observe` はこれらの型で値の中身を
    /// 見ず非 NULL の有無だけを見るため、実際の値は保持しない）。
    PresenceOnly(bool),
}

/// `LAG`／`LEAD` 項目 1 つについての 1 行分の入力（Issue #1881）。この行が他の行から参照
/// されたときに返す値、この行で評価したオフセット、この行で評価した既定値を持つ。
#[derive(Debug)]
struct OffsetRowValues {
    /// この行が参照先になったときの値。評価エラー（`22xxx`）は使用時まで保持する。
    value: Result<Cell, SqlSurfaceError>,
    /// 現在行で評価したオフセット（`None` は NULL。範囲外は評価時点で `22003`）。
    offset: Option<i32>,
    /// 現在行で評価した既定値（`None` は既定値なし＝NULL。評価エラーは使用時まで保持）。
    default: Option<Result<Cell, SqlSurfaceError>>,
}

/// `LAG`／`LEAD` 項目の式を項目ごとに 1 回だけコンパイルした結果。
struct OffsetPrograms {
    value: Option<ExprProgram>,
    offset: Option<ExprProgram>,
    default: Option<ExprProgram>,
}

/// `LAG`／`LEAD` の行ごとの入力を抽出する評価器（コンパイル済みプログラムと式スタックを持つ）。
/// [`materialize_rows`]（ストレージ走査）と [`CellWindowEvaluator`] が共有する。
struct OffsetEvaluator {
    programs: Vec<Option<OffsetPrograms>>,
    scratch: Vec<StackValue>,
}

impl OffsetEvaluator {
    fn new(items: &[BoundWindowItem]) -> Self {
        let programs = items
            .iter()
            .map(|item| {
                item.offset_args.as_ref().map(|args| OffsetPrograms {
                    value: match &args.value {
                        BoundWindowValue::Expr { bound, .. } => Some(ExprProgram::compile(bound)),
                        BoundWindowValue::Column { .. } | BoundWindowValue::PseudoId => None,
                    },
                    offset: args.offset.as_ref().map(ExprProgram::compile),
                    default: args.default.as_ref().map(ExprProgram::compile),
                })
            })
            .collect();
        Self {
            programs,
            scratch: Vec::new(),
        }
    }

    /// 項目 `item_index` の 1 行分の入力を評価する。保持するセルは状態予算へ計上する。
    #[allow(clippy::too_many_arguments)]
    fn extract(
        &mut self,
        item_index: usize,
        args: &BoundWindowOffsetArgs,
        id: u64,
        embedding: &[f32],
        scanned: &[Option<row_codec::ScalarRef<'_>>],
        state_bytes: &mut usize,
        state_cap: usize,
    ) -> Result<OffsetRowValues, SqlSurfaceError> {
        let programs = self
            .programs
            .get(item_index)
            .and_then(|p| p.as_ref())
            .ok_or_else(|| window_bug("offset window programs missing for an item"))?;
        let scratch = &mut self.scratch;
        let result_ty = args.result_type.as_ref();

        // オフセットは全行で評価する（PostgreSQL と同じ。エラーは即時）。
        let offset = match (&args.offset, &programs.offset) {
            (None, _) => Some(1),
            (Some(_), Some(program)) => {
                offset_from_expr_value(program.eval(id, embedding, scanned, scratch)?)?
            }
            (Some(_), None) => return Err(window_bug("offset program missing at evaluation")),
        };

        // 値はこの行が参照先になったときに返すもの。データ例外（22xxx）だけ遅延する。
        let value = match &args.value {
            BoundWindowValue::Column { index, ty } => {
                Ok(scalar_cell(ty, *index, scanned, state_bytes, state_cap)?)
            }
            BoundWindowValue::PseudoId => {
                defer_data_exception(pseudo_id_cell(id), state_bytes, state_cap)?
            }
            BoundWindowValue::Expr { .. } => {
                let program = programs
                    .value
                    .as_ref()
                    .ok_or_else(|| window_bug("value program missing at evaluation"))?;
                let evaluated = program
                    .eval(id, embedding, scanned, scratch)
                    .and_then(|v| computed_value_to_cell(v, result_ty, state_bytes, state_cap));
                defer_data_exception(evaluated, state_bytes, state_cap)?
            }
        };

        // 既定値は現在行で評価し、パーティション外を指したときだけ使う。
        let default = if let Some((index, ty)) = &args.default_column {
            Some(Ok(scalar_cell(
                ty,
                *index,
                scanned,
                state_bytes,
                state_cap,
            )?))
        } else if args.default_pseudo_id {
            Some(defer_data_exception(
                pseudo_id_cell(id),
                state_bytes,
                state_cap,
            )?)
        } else if let (Some(_), Some(program)) = (&args.default, &programs.default) {
            let evaluated = program
                .eval(id, embedding, scanned, scratch)
                .and_then(|v| computed_value_to_cell(v, result_ty, state_bytes, state_cap));
            Some(defer_data_exception(evaluated, state_bytes, state_cap)?)
        } else {
            None
        };
        Ok(OffsetRowValues {
            value,
            offset,
            default,
        })
    }
}

/// 裸の疑似列 `id` を BIGINT セルへ直接変換する（f64 を経由しない）。`i64` に収まらない
/// `id` は `22003`（データ例外。参照された行でだけ表面化する）。
fn pseudo_id_cell(id: u64) -> Result<Cell, SqlSurfaceError> {
    i64::try_from(id)
        .map(Cell::SignedInteger)
        .map_err(|_| SqlSurfaceError::numeric_out_of_range("bigint out of range"))
}

/// スカラー列 1 つをセル化する（[`push_scalar_column_cell`] の薄い包み。テキスト長は
/// 状態予算へ計上する）。
fn scalar_cell(
    ty: &ColumnType,
    index: usize,
    scanned: &[Option<row_codec::ScalarRef<'_>>],
    state_bytes: &mut usize,
    state_cap: usize,
) -> Result<Cell, SqlSurfaceError> {
    let mut cells: Vec<Cell> = Vec::with_capacity(1);
    push_scalar_column_cell(&mut cells, ty, index, scanned, state_bytes, state_cap)?;
    cells
        .pop()
        .ok_or_else(|| window_bug("scalar column cell was not produced"))
}

/// データ例外（`22xxx`）は `Err` のまま保持して返し、それ以外（予算超過・中断・内部エラー）は
/// 即時に伝播する。値・既定値は参照されない行では評価されなかったものとして扱うため。
///
/// 保持するエラーの所有文字列（`detail` 等。入力の一部を含みうる）は行ごとに残るため、
/// 表示文字列長（所有文字列の長さ以上）を状態予算へ計上し、超過は `54000`（fail-closed）。
/// 値側・既定値側のエラー保持はすべて本関数を通り、計上経路はここ 1 か所に集約する。
fn defer_data_exception(
    r: Result<Cell, SqlSurfaceError>,
    state_bytes: &mut usize,
    state_cap: usize,
) -> Result<Result<Cell, SqlSurfaceError>, SqlSurfaceError> {
    match r {
        Ok(c) => Ok(Ok(c)),
        Err(e) if e.wire_code().starts_with("22") => {
            *state_bytes =
                try_accumulate_state_budget(*state_bytes, e.to_string().len(), state_cap)?;
            Ok(Err(e))
        }
        Err(e) => Err(e),
    }
}

/// オフセット式の評価値を `i32`（`None` は NULL）へ変換する。範囲外は `22003`。
/// 束縛段が整数型に限定済みのため、非整数・他の variant は不変条件違反（`XX000`）。
fn offset_from_expr_value(value: ExprValue<'_>) -> Result<Option<i32>, SqlSurfaceError> {
    match value {
        ExprValue::Null => Ok(None),
        ExprValue::Scalar(f) => {
            if !(f.is_finite() && f.fract() == 0.0) {
                return Err(window_bug("LAG/LEAD offset is not an integer value"));
            }
            if f < f64::from(i32::MIN) || f > f64::from(i32::MAX) {
                return Err(SqlSurfaceError::numeric_out_of_range(
                    "integer out of range",
                ));
            }
            Ok(Some(f as i32))
        }
        _ => Err(window_bug(
            "LAG/LEAD offset evaluated to a non-integer value",
        )),
    }
}

/// パーティション内の位置 `pos`（0 始まり・長さ `len`）から見たオフセット先の位置。
/// `LAG` は `pos - offset`、`LEAD`（`forward`）は `pos + offset`。範囲外は `None`。
/// 計算は `i64` で行い、`i32::MIN` でもあふれない。
fn offset_target_index(pos: usize, len: usize, offset: i32, forward: bool) -> Option<usize> {
    let p = i64::try_from(pos).ok()?;
    let delta = i64::from(offset);
    let target = if forward {
        p.checked_add(delta)?
    } else {
        p.checked_sub(delta)?
    };
    let target = usize::try_from(target).ok()?;
    (target < len).then_some(target)
}

/// 解決済みのフレーム境界（オフセットは評価・検査済みの非負位置数。i64 超は飽和）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolvedBound {
    UnboundedPreceding,
    Preceding(usize),
    CurrentRow,
    Following(usize),
    UnboundedFollowing,
}

/// 解決済みの `ROWS`／`GROUPS` フレーム（集計だけが保持する。[`resolve_frames`] が作る）。
/// オフセットの検査（NULL＝`22004`・負＝`22013`）は両モード共通のため同じ形を共用し、
/// 単位だけが違う（`ROWS` は行数、`GROUPS` は peer グループ数。Issue #1891）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ResolvedRowsFrame {
    start: ResolvedBound,
    end: ResolvedBound,
    exclusion: WindowFrameExclusion,
}

/// 評価済み（未検査）のオフセット。`None` は NULL。
#[derive(Debug, Clone, Copy)]
enum EvaluatedBound {
    UnboundedPreceding,
    Preceding(Option<i64>),
    CurrentRow,
    Following(Option<i64>),
    UnboundedFollowing,
}

/// フレームオフセットの評価値を `i64`（`None` は NULL）へ変換する。`offset_from_expr_value`
/// （`LAG` の int4 契約）とは別で、フレームオフセットは int8 全域を受理する（PostgreSQL と同じ）。
/// 束縛段が整数型に限定済みのため、非整数・他の variant は不変条件違反（`XX000`）。
fn frame_offset_from_expr_value(value: ExprValue<'_>) -> Result<Option<i64>, SqlSurfaceError> {
    match value {
        ExprValue::Null => Ok(None),
        ExprValue::Scalar(f) => {
            if !(f.is_finite() && f.fract() == 0.0) {
                return Err(window_bug("window frame offset is not an integer value"));
            }
            // `as` は飽和変換（2^63 以上は `i64::MAX`）。それを超えるオフセットは事実上 unbounded。
            Ok(Some(f as i64))
        }
        _ => Err(window_bug(
            "window frame offset evaluated to a non-integer value",
        )),
    }
}

fn evaluate_frame_bound(
    bound: &BoundFrameBound,
    scratch: &mut Vec<StackValue>,
) -> Result<EvaluatedBound, SqlSurfaceError> {
    // 識別子は束縛段（42P10）で拒否済みの定数式のため、行なしで評価できる。
    let mut offset = |o: &BoundFrameOffset| -> Result<Option<i64>, SqlSurfaceError> {
        match o {
            BoundFrameOffset::Null => Ok(None),
            BoundFrameOffset::Expr(e) => {
                frame_offset_from_expr_value(ExprProgram::compile(e).eval(0, &[], &[], scratch)?)
            }
        }
    };
    Ok(match bound {
        BoundFrameBound::UnboundedPreceding => EvaluatedBound::UnboundedPreceding,
        BoundFrameBound::Preceding(o) => EvaluatedBound::Preceding(offset(o)?),
        BoundFrameBound::CurrentRow => EvaluatedBound::CurrentRow,
        BoundFrameBound::Following(o) => EvaluatedBound::Following(offset(o)?),
        BoundFrameBound::UnboundedFollowing => EvaluatedBound::UnboundedFollowing,
    })
}

/// オフセットの NULL（`22004`）・負（`22013`）を検査して [`ResolvedBound`] にする。
fn check_frame_bound(
    bound: EvaluatedBound,
    is_start: bool,
) -> Result<ResolvedBound, SqlSurfaceError> {
    let checked = |v: Option<i64>| -> Result<usize, SqlSurfaceError> {
        let n = v.ok_or(SqlSurfaceError::NullValueNotAllowed {
            detail: if is_start {
                "frame starting offset must not be null"
            } else {
                "frame ending offset must not be null"
            },
        })?;
        if n < 0 {
            return Err(SqlSurfaceError::InvalidPrecedingOrFollowingSize {
                detail: if is_start {
                    "frame starting offset must not be negative"
                } else {
                    "frame ending offset must not be negative"
                },
            });
        }
        Ok(usize::try_from(n).unwrap_or(usize::MAX))
    };
    Ok(match bound {
        EvaluatedBound::UnboundedPreceding => ResolvedBound::UnboundedPreceding,
        EvaluatedBound::Preceding(v) => ResolvedBound::Preceding(checked(v)?),
        EvaluatedBound::CurrentRow => ResolvedBound::CurrentRow,
        EvaluatedBound::Following(v) => ResolvedBound::Following(checked(v)?),
        EvaluatedBound::UnboundedFollowing => ResolvedBound::UnboundedFollowing,
    })
}

/// 解決済みの `RANGE` オフセット 1 つ（Issue #1890。種別は `ORDER BY` キー型で決まり、束縛段が
/// 型照合と強制変換を済ませている）。
#[derive(Debug, Clone, Copy)]
enum RangeOffset {
    /// 整数キー（`id` を含む）の整数オフセット。
    Int(i64),
    /// `REAL`／`DOUBLE PRECISION` キーの浮動小数点オフセット。
    Float(f64),
    /// `NUMERIC` キーの正確な小数オフセット。
    Numeric(crate::numeric::Decimal),
    /// 日付・日時キーの `INTERVAL` オフセット。
    Interval(crate::datetime::Interval),
}

/// 解決済みの `RANGE` フレーム境界（オフセットは評価済み・NULL 検査済み。負・NaN の検査は
/// 遅延する。[`range_frame_bounds`] 参照）。
#[derive(Debug, Clone, Copy)]
enum ResolvedRangeBound {
    UnboundedPreceding,
    Preceding(RangeOffset),
    CurrentRow,
    Following(RangeOffset),
    UnboundedFollowing,
}

/// 解決済みの `RANGE` フレーム（[`resolve_frames`] が作る）。
#[derive(Debug, Clone, Copy)]
struct ResolvedRangeFrame {
    start: ResolvedRangeBound,
    end: ResolvedRangeBound,
    exclusion: WindowFrameExclusion,
}

impl ResolvedRangeFrame {
    fn offsets(&self) -> impl Iterator<Item = &RangeOffset> {
        [&self.start, &self.end]
            .into_iter()
            .filter_map(|b| match b {
                ResolvedRangeBound::Preceding(o) | ResolvedRangeBound::Following(o) => Some(o),
                _ => None,
            })
    }
}

/// 項目ごとの解決済みフレーム（`ROWS`／`RANGE`／`GROUPS`）。
#[derive(Debug, Clone, Copy)]
enum ResolvedFrame {
    Rows(ResolvedRowsFrame),
    Groups(ResolvedRowsFrame),
    Range(ResolvedRangeFrame),
}

/// 評価済み（未検査）の `RANGE` 境界。オフセットの `None` は NULL。
#[derive(Debug, Clone, Copy)]
enum EvaluatedRangeBound {
    UnboundedPreceding,
    Preceding(Option<RangeOffset>),
    CurrentRow,
    Following(Option<RangeOffset>),
    UnboundedFollowing,
}

/// 評価済みの 1 項目分のフレーム（検査前）。
enum EvaluatedFrame {
    Rows(EvaluatedBound, EvaluatedBound, WindowFrameExclusion),
    Groups(EvaluatedBound, EvaluatedBound, WindowFrameExclusion),
    Range(
        EvaluatedRangeBound,
        EvaluatedRangeBound,
        WindowFrameExclusion,
    ),
}

/// `NUMERIC` を `f64` へ（浮動小数点キーの `RANGE` オフセットが `NUMERIC` 値のとき）。
fn decimal_to_f64(d: &crate::numeric::Decimal) -> f64 {
    d.unscaled() as f64 / 10f64.powi(i32::from(d.scale()))
}

/// `RANGE` オフセットの評価値を種別 `kind` の [`RangeOffset`]（`None` は NULL）へ変換する。
/// 束縛段が型を照合済みのため、想定外の組は不変条件違反（`XX000`）。
fn range_offset_from_expr_value(
    kind: RangeOffsetKind,
    value: ExprValue<'_>,
) -> Result<Option<RangeOffset>, SqlSurfaceError> {
    match (kind, value) {
        (_, ExprValue::Null) => Ok(None),
        (RangeOffsetKind::Int, v) => Ok(frame_offset_from_expr_value(v)?.map(RangeOffset::Int)),
        (RangeOffsetKind::Float, ExprValue::Scalar(f)) => Ok(Some(RangeOffset::Float(f))),
        (RangeOffsetKind::Float, ExprValue::Numeric(d)) => {
            Ok(Some(RangeOffset::Float(decimal_to_f64(&d))))
        }
        (RangeOffsetKind::Numeric, ExprValue::Numeric(d)) => Ok(Some(RangeOffset::Numeric(d))),
        (RangeOffsetKind::Interval, ExprValue::Interval(iv)) => Ok(Some(RangeOffset::Interval(iv))),
        _ => Err(window_bug(
            "RANGE frame offset evaluated to an unexpected value type",
        )),
    }
}

fn evaluate_range_bound(
    bound: &BoundFrameBound,
    kind: Option<RangeOffsetKind>,
    scratch: &mut Vec<StackValue>,
) -> Result<EvaluatedRangeBound, SqlSurfaceError> {
    let mut offset = |o: &BoundFrameOffset| -> Result<Option<RangeOffset>, SqlSurfaceError> {
        match o {
            BoundFrameOffset::Null => Ok(None),
            BoundFrameOffset::Expr(e) => {
                let kind =
                    kind.ok_or_else(|| window_bug("RANGE frame offset without an offset kind"))?;
                range_offset_from_expr_value(
                    kind,
                    ExprProgram::compile(e).eval(0, &[], &[], scratch)?,
                )
            }
        }
    };
    Ok(match bound {
        BoundFrameBound::UnboundedPreceding => EvaluatedRangeBound::UnboundedPreceding,
        BoundFrameBound::Preceding(o) => EvaluatedRangeBound::Preceding(offset(o)?),
        BoundFrameBound::CurrentRow => EvaluatedRangeBound::CurrentRow,
        BoundFrameBound::Following(o) => EvaluatedRangeBound::Following(offset(o)?),
        BoundFrameBound::UnboundedFollowing => EvaluatedRangeBound::UnboundedFollowing,
    })
}

/// `RANGE` オフセットの NULL（`22004`）を検査して [`ResolvedRangeBound`] にする。負・NaN
/// （`22013`）は PostgreSQL の in_range 関数内の検査に合わせ、ここでは行わない。
fn check_range_bound(
    bound: EvaluatedRangeBound,
    is_start: bool,
) -> Result<ResolvedRangeBound, SqlSurfaceError> {
    let present = |v: Option<RangeOffset>| -> Result<RangeOffset, SqlSurfaceError> {
        v.ok_or(SqlSurfaceError::NullValueNotAllowed {
            detail: if is_start {
                "frame starting offset must not be null"
            } else {
                "frame ending offset must not be null"
            },
        })
    };
    Ok(match bound {
        EvaluatedRangeBound::UnboundedPreceding => ResolvedRangeBound::UnboundedPreceding,
        EvaluatedRangeBound::Preceding(v) => ResolvedRangeBound::Preceding(present(v)?),
        EvaluatedRangeBound::CurrentRow => ResolvedRangeBound::CurrentRow,
        EvaluatedRangeBound::Following(v) => ResolvedRangeBound::Following(present(v)?),
        EvaluatedRangeBound::UnboundedFollowing => ResolvedRangeBound::UnboundedFollowing,
    })
}

/// 項目ごとのフレームを実行開始時に解決する（`execute_window_scan_with_caps` と
/// [`CellWindowEvaluator::new`] が行を引く前に 1 回呼ぶ。PostgreSQL がプラン時・WindowAgg 開始時に
/// オフセットを評価するのと同じ位置で、0 行・`WHERE` 不一致でもエラーを返す）。
/// 評価エラー（`22012`・`22003` 等）は `ntile`（フレーム式を評価しない）以外の関数種別で返す。NULL／`ROWS`・`GROUPS` の負の検査は集計と
/// `LAG`／`LEAD` だけが行う（順位関数は PostgreSQL 16 以降フレームを最適化で置き換えるため
/// 検査しない）。`RANGE` の負・NaN は検査を遅延する（集計と、オフセット付きの `LAG`／`LEAD` が
/// 境界計算時に行う）。戻り値を保持するのは集計（と `RANGE` オフセット付きの `LAG`／`LEAD`）で、
/// 順位関数・`ROWS`／`GROUPS` の `LAG`／`LEAD` は `None`（フレームを無視する）。
fn resolve_frames(
    items: &[BoundWindowItem],
) -> Result<Vec<Option<ResolvedFrame>>, SqlSurfaceError> {
    let mut scratch: Vec<StackValue> = Vec::new();
    let mut evaluated: Vec<Option<EvaluatedFrame>> = Vec::with_capacity(items.len());
    for item in items {
        // `ntile` はフレームを無視し、オフセット式も評価・検査しない（Issue #1884。
        // `ROWS BETWEEN (1 / 0) PRECEDING ...` でも `22012` を返さない）。
        if item.func == WindowFunc::Ntile {
            evaluated.push(None);
            continue;
        }
        evaluated.push(match &item.frame {
            None => None,
            Some(frame) => match frame.mode {
                WindowFrameMode::Rows => Some(EvaluatedFrame::Rows(
                    evaluate_frame_bound(&frame.start, &mut scratch)?,
                    evaluate_frame_bound(&frame.end, &mut scratch)?,
                    frame.exclusion,
                )),
                WindowFrameMode::Range => Some(EvaluatedFrame::Range(
                    evaluate_range_bound(&frame.start, frame.range_offset, &mut scratch)?,
                    evaluate_range_bound(&frame.end, frame.range_offset, &mut scratch)?,
                    frame.exclusion,
                )),
                WindowFrameMode::Groups => Some(EvaluatedFrame::Groups(
                    evaluate_frame_bound(&frame.start, &mut scratch)?,
                    evaluate_frame_bound(&frame.end, &mut scratch)?,
                    frame.exclusion,
                )),
            },
        });
    }
    let mut out = Vec::with_capacity(items.len());
    for (item, ev) in items.iter().zip(evaluated) {
        let Some(ev) = ev else {
            out.push(None);
            continue;
        };
        if item.func.ignores_frame() {
            out.push(None);
            continue;
        }
        let is_offset_fn = matches!(item.func, WindowFunc::Lag | WindowFunc::Lead);
        match ev {
            EvaluatedFrame::Rows(start, end, exclusion) => {
                let start = check_frame_bound(start, true)?;
                let end = check_frame_bound(end, false)?;
                out.push(if is_offset_fn {
                    None
                } else {
                    Some(ResolvedFrame::Rows(ResolvedRowsFrame {
                        start,
                        end,
                        exclusion,
                    }))
                });
            }
            EvaluatedFrame::Groups(start, end, exclusion) => {
                let start = check_frame_bound(start, true)?;
                let end = check_frame_bound(end, false)?;
                out.push(if is_offset_fn {
                    None
                } else {
                    Some(ResolvedFrame::Groups(ResolvedRowsFrame {
                        start,
                        end,
                        exclusion,
                    }))
                });
            }
            EvaluatedFrame::Range(start, end, exclusion) => {
                let frame = ResolvedRangeFrame {
                    start: check_range_bound(start, true)?,
                    end: check_range_bound(end, false)?,
                    exclusion,
                };
                // `LAG`／`LEAD` はフレームを使わないが、オフセットの負・NaN の検査（遅延）のために
                // オフセット付きのときだけ保持する。
                out.push(if is_offset_fn && frame.offsets().next().is_none() {
                    None
                } else {
                    Some(ResolvedFrame::Range(frame))
                });
            }
        }
    }
    Ok(out)
}

/// 位置 `pos`（0 始まり・パーティション長 `len`）の `ROWS` フレームを半開区間
/// `[start, end)` で返す（`start >= end` は空フレーム）。位置数のみに依存するモード非依存の計算で、
/// すべて飽和演算。`UNBOUNDED FOLLOWING` を始点、`UNBOUNDED PRECEDING` を終点にする形は
/// 構文層が拒否済みだが、空フレームへ倒す。
fn rows_frame_bounds(pos: usize, len: usize, frame: &ResolvedRowsFrame) -> (usize, usize) {
    let start = match frame.start {
        ResolvedBound::UnboundedPreceding => 0,
        ResolvedBound::Preceding(n) => pos.saturating_sub(n),
        ResolvedBound::CurrentRow => pos,
        ResolvedBound::Following(n) => pos.saturating_add(n),
        ResolvedBound::UnboundedFollowing => len,
    };
    let end = match frame.end {
        ResolvedBound::UnboundedPreceding => 0,
        ResolvedBound::Preceding(n) => pos.saturating_add(1).saturating_sub(n),
        ResolvedBound::CurrentRow => pos.saturating_add(1),
        ResolvedBound::Following(n) => pos.saturating_add(n).saturating_add(1),
        ResolvedBound::UnboundedFollowing => len,
    };
    (start.min(len), end.min(len))
}

/// 可視かつ WHERE を満たす 1 行の materialize 結果（SQL-30・TASK-214）。
/// `partition_keys`／`order_keys`／`agg_values` は `bound.windows()` と同じ順序の
/// `Vec`（外側添字がウィンドウ項目の添字）。
struct MaterializedRow {
    /// 走査順（`(tenant_id, id)` 物理順）の通し番号。同順位のタイブレークに使う。
    seq: usize,
    id: u64,
    partition_keys: Vec<Vec<Option<WindowKeyValue>>>,
    order_keys: Vec<Vec<Option<WindowKeyValue>>>,
    agg_values: Vec<WindowInputValue>,
    /// `LAG`／`LEAD` 項目の入力（外側添字がウィンドウ項目。それ以外の項目は `None`。Issue #1881）。
    offset_values: Vec<Option<OffsetRowValues>>,
    /// 文全体のスカラー `ORDER BY` キー値（`bound.order_by` と同順。`ORDER BY`
    /// なしなら空。Issue #1189）。出力順の計算（[`compute_output_order`]）専用で、
    /// 出力には現れない。
    stmt_order_keys: Vec<Option<OrderValue>>,
    /// タイブレーク用の tenant 識別子（[`MaterializeOutput::tenants`] への添字。
    /// `ORDER BY` なしなら 0 固定で未使用）。
    tenant_idx: u32,
}

/// 評価段の成果物（[`build_result`] への入力）。`output_order` は文全体の
/// `ORDER BY` 適用後の出力順（`materialized` への添字列）、`window_values` は
/// ウィンドウ項目ごとの `materialized` と同じ添字で引ける値（Issue #1189）。
struct EvaluatedWindows<'a> {
    output_order: Option<&'a [usize]>,
    window_values: &'a [Vec<Cell>],
}

/// [`materialize_rows`] の結果。`tenants` は行の tenant_id の intern 表で、
/// 物理走査順が `(tenant_id, id)` 昇順のため連続する同一 tenant を 1 要素に
/// まとめる（`ORDER BY` なしなら空）。
struct MaterializeOutput {
    rows: Vec<MaterializedRow>,
    tenants: Vec<String>,
    state_bytes: usize,
}

/// [`BoundScan`] を実行する（`windows` が非空。`sql::scan::execute_scan_with_budget`
/// の dispatch から呼ばれる）。`max_result_bytes` は投影段（[`crate::sql::scan::
/// execute_scan`] への委譲）の結果バイト予算（`sql::scan` と同じ契約）で、
/// ウィンドウ状態の予算（[`MAX_WINDOW_STATE_BYTES`] 等）とは独立に計上する。
/// 本体は常に [`MAX_WINDOW_STATE_BYTES`] を状態予算の上限として渡す
/// （[`execute_window_scan_with_caps`] 参照）。
pub(crate) fn execute_window_scan(
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    schema: &TableSchema,
    bound: &BoundScan,
    max_result_bytes: usize,
) -> Result<QueryResult, SqlSurfaceError> {
    execute_window_scan_with_caps(
        read_txn,
        ctx,
        schema,
        bound,
        max_result_bytes,
        MAX_WINDOW_STATE_BYTES,
    )
}

/// [`execute_window_scan`] の本体。`state_cap` を引数化しているのは、
/// 評価段（[`evaluate_window_item`]）の確保も materialize 段と同じ予算で有界に
/// なっていることを、実データを大量投入せず小さい cap で単体テストできるように
/// するため（PR #930 レビュー指摘 3）。
fn execute_window_scan_with_caps(
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    schema: &TableSchema,
    bound: &BoundScan,
    max_result_bytes: usize,
    state_cap: usize,
) -> Result<QueryResult, SqlSurfaceError> {
    // フレームのオフセットは行を引く前に評価する（PostgreSQL の評価位置。0 行でも検査する）。
    let frames = resolve_frames(bound.windows())?;
    let MaterializeOutput {
        rows: materialized,
        tenants,
        mut state_bytes,
    } = materialize_rows(read_txn, ctx, schema, bound, state_cap)?;

    // ウィンドウ項目ごとに独立してパーティション分割・安定ソート・peer 評価を行い、
    // `materialized` と同じ添字（走査順）で引ける `Vec<Cell>` を作る（他テナントの
    // 可視 Public 行と衝突しうる `id` をキーにしない。security.md P0「テナント境界」
    // レビュー指摘対応。§モジュールドキュメント参照）。評価段の確保
    // （`partitions`・結果ベクタ）も materialize 段と同じ `state_bytes`／`state_cap`
    // へ計上を続ける（レビュー指摘 3）。
    let mut window_values: Vec<Vec<Cell>> = Vec::with_capacity(bound.windows().len());
    for (item_index, item) in bound.windows().iter().enumerate() {
        window_values.push(evaluate_window_item(
            item,
            item_index,
            &materialized,
            frames.get(item_index).and_then(|f| f.as_ref()),
            &mut state_bytes,
            state_cap,
        )?);
    }

    // Issue #1189: 文全体の `ORDER BY` の出力順（`ORDER BY` なしは恒等順列）。
    let output_order =
        compute_output_order(bound, &materialized, &tenants, &mut state_bytes, state_cap)?;

    build_result(
        read_txn,
        ctx,
        schema,
        bound,
        &materialized,
        EvaluatedWindows {
            output_order: output_order.as_deref(),
            window_values: &window_values,
        },
        max_result_bytes,
    )
}

/// 文全体の `ORDER BY` を適用した出力順（`materialized` への添字列）を返す
/// （Issue #1189・SQL-25・SQL-30）。`ORDER BY` なしは `None`（物理走査順の恒等。順列を確保しない）。
/// ありの場合は base scan（`sql::scan`）と同一の比較器
/// （[`compare_statement_order`]。キー → `id` → `tenant_id`）で安定ソートする。
/// 経路 (A)（先頭キー `id` の早期打ち切り）は自テナント 1 つの範囲走査で `id` が
/// テナント内一意なため、この順序と一致する。順列とソート一時領域は確保前に state 予算へ計上する。
fn compute_output_order(
    bound: &BoundScan,
    materialized: &[MaterializedRow],
    tenants: &[String],
    state_bytes: &mut usize,
    state_cap: usize,
) -> Result<Option<Vec<usize>>, SqlSurfaceError> {
    // `ORDER BY` なしは物理走査順（添字そのもの）を使うため順列を確保しない
    // （従来通っていた状態予算ぎりぎりの取得を新たに 54000 にしない。PR #1233 指摘）。
    if bound.order_by.is_empty() {
        return Ok(None);
    }
    // 順列本体に加え、安定ソート（`sort_by`）の一時領域は最大で要素数分（順列と
    // 同サイズ）確保されうるため、上限を確保前に合わせて状態予算へ計上する。
    let bytes = materialized
        .len()
        .checked_mul(std::mem::size_of::<usize>())
        .and_then(|b| b.checked_mul(2))
        .ok_or_else(|| SqlSurfaceError::payload_too_large("window state size overflowed"))?;
    *state_bytes = try_accumulate_state_budget(*state_bytes, bytes, state_cap)?;
    let mut order: Vec<usize> = Vec::new();
    order
        .try_reserve_exact(materialized.len())
        .map_err(|_| SqlSurfaceError::payload_too_large("window state allocation failed"))?;
    order.extend(0..materialized.len());
    let tenant_bytes = |row: &MaterializedRow| -> &[u8] {
        tenants
            .get(row.tenant_idx as usize)
            .map(|t| t.as_bytes())
            .unwrap_or(&[])
    };
    // 安定ソート（`sort_unstable*` は決定性検査 `check_sort_determinism.sh` が禁止）。
    order.sort_by(|&a, &b| match (materialized.get(a), materialized.get(b)) {
        (Some(x), Some(y)) => compare_statement_order(
            &bound.order_by,
            &x.stmt_order_keys,
            x.id,
            tenant_bytes(x),
            &y.stmt_order_keys,
            y.id,
            tenant_bytes(y),
        ),
        _ => std::cmp::Ordering::Equal,
    });
    Ok(Some(order))
}

/// 対象テーブルを 1 回、`LIMIT` による早期終了なし（`result_cap` 超過確定時のみ `54000` で中断。
/// Issue #1797）で走査し、可視かつ `WHERE` を
/// 満たす行**全体**について [`MaterializedRow`] を集める（§モジュールドキュメント
/// 「materialize 段」参照）。`state_cap` は [`MAX_WINDOW_STATE_BYTES`] 相当の上限
/// （本体は常にその定数を渡す。単体テストが小さい cap を注入できるよう引数化した。
/// PR #930 レビュー指摘対応）。戻り値の `usize` は最終累計バイト数で、評価段
/// （[`evaluate_window_item`]）が同じ予算を続けて計上できるよう呼び出し元へ返す
/// （materialize 段だけでなく評価段の確保も同一の [`MAX_WINDOW_STATE_BYTES`]
/// 予算で有界にする。レビュー指摘 3）。
fn materialize_rows(
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    schema: &TableSchema,
    bound: &BoundScan,
    state_cap: usize,
) -> Result<MaterializeOutput, SqlSurfaceError> {
    let expected_dim = schema.vector_dim();
    let (tier, scalar_mask) = decode_tier_for_window(schema, bound);
    let expr_filter_programs: Vec<ExprProgram> = bound
        .expr_filters()
        .iter()
        .map(ExprProgram::compile)
        .collect();

    let row_table_name = catalog::user_rows_table_name(bound.table());
    let table = match read_txn.open_table(catalog::user_rows_table_def(&row_table_name)) {
        Ok(t) => Some(t),
        Err(redb::TableError::TableDoesNotExist(_)) => None,
        Err(e) => {
            return Err(SqlSurfaceError::Internal {
                detail: format!(
                    "window scan row scan failed: {}",
                    catalog::map_row_table_error(e)
                ),
            })
        }
    };

    let mut embedding_scratch: Vec<f32> = Vec::new();
    let mut expr_scratch: Vec<StackValue> = Vec::new();
    let mut offset_eval = OffsetEvaluator::new(bound.windows());
    let mut materialized: Vec<MaterializedRow> = Vec::new();
    let mut tenants: Vec<String> = Vec::new();
    let mut seq: usize = 0;
    let stop_threshold =
        crate::sql::scan::early_stop_threshold(bound.result_cap(), bound.offset())?;
    let mut state_bytes: usize = 0;
    let mut partition_counts: Vec<HashMap<Vec<u8>, usize>> =
        vec![HashMap::new(); bound.windows().len()];

    if let Some(table) = table {
        'rows: for entry in table.iter().map_err(storage_internal)? {
            let (k, v) = entry.map_err(storage_internal)?;
            let (key_tenant, id) = k.value();
            let buf = v.value();

            // Issue #1798: 協調的中断の確認点（commit 前の読み取りループ）。
            crate::interrupt::check()?;

            // RLS 段（無条件・デコード前）。`sql::scan`・`sql::aggregate` の走査
            // ループと同一の順序（security.md P0「テナント境界」）。
            let (tenant_id, visibility, offset) =
                storage::decode_row_header(buf).map_err(storage_internal)?;
            if !ctx.is_visible(tenant_id, visibility) {
                continue;
            }

            storage::verify_row_key_tenant(key_tenant, tenant_id).map_err(storage_internal)?;

            let (dim, metadata): (u32, &[u8]) = match tier {
                DecodeTier::Fast | DecodeTier::DimAndScalar => {
                    storage::decode_row_dim_and_metadata_borrowed(buf).map_err(storage_internal)?
                }
                DecodeTier::Embedding => {
                    storage::decode_row_body_into(buf, offset, &mut embedding_scratch)
                        .map_err(storage_internal)?
                }
            };
            if let Some(expected) = expected_dim {
                if dim != 0 && dim != expected {
                    return Err(SqlSurfaceError::Internal {
                        detail: "window scan row scan failed: embedding dimension mismatch"
                            .to_string(),
                    });
                }
            }

            let scanned: Vec<Option<row_codec::ScalarRef<'_>>> = match tier {
                DecodeTier::Fast => {
                    row_codec::validate_scalar_columns(schema, metadata)?;
                    Vec::new()
                }
                DecodeTier::DimAndScalar | DecodeTier::Embedding => {
                    row_codec::scan_scalar_columns_masked(schema, metadata, Some(&scalar_mask))?
                }
            };

            // Issue #919・SQL-26: `sql::scan::execute_scan_with_budget` と同じ
            // 契約（`decode_tier_for_window` が `TextColumnRef` を `scalar_mask`
            // へ反映済みのため、`scanned` をそのまま `.as_text()` へ写せばよい）。

            if !declarative_filter::matches_all(bound.metadata_filters(), &scanned) {
                continue;
            }
            for (expr, program) in bound.expr_filters().iter().zip(&expr_filter_programs) {
                let references_embedding = udf_call::references_embedding(expr);
                if references_embedding && dim == 0 {
                    continue 'rows;
                }
                let embedding: &[f32] = if references_embedding {
                    match tier {
                        DecodeTier::Embedding => embedding_scratch.as_slice(),
                        DecodeTier::Fast | DecodeTier::DimAndScalar => {
                            return Err(window_bug(
                                "WHERE expression references the VECTOR column but tier did not decode it",
                            ))
                        }
                    }
                } else {
                    &[]
                };
                match program.eval(id, embedding, &scanned, &mut expr_scratch)? {
                    ExprValue::Bool(true) => {}
                    // Issue #919・SQL-26（AC2）: NULL（UNKNOWN）は `WHERE` では
                    // 偽と同義に扱う（`sql::scan` と同じ三値論理契約）。
                    ExprValue::Bool(false) | ExprValue::Null => continue 'rows,
                    _ => {
                        return Err(SqlSurfaceError::invalid_input(
                            "WHERE expression did not evaluate to a boolean",
                        ))
                    }
                }
            }
            for group in bound.or_filters() {
                let group_embedding: &[f32] = match tier {
                    DecodeTier::Embedding => embedding_scratch.as_slice(),
                    DecodeTier::Fast | DecodeTier::DimAndScalar => &[],
                };
                if !group.matches(
                    &scanned,
                    id,
                    group_embedding,
                    dim as usize,
                    &mut expr_scratch,
                )? {
                    continue 'rows;
                }
            }

            // defense-in-depth（`sql::scan` と同趣旨）。
            if !ctx.is_visible(tenant_id, visibility) {
                continue;
            }

            // Issue #1797・SQL-34: ウィンドウ関数は行数を減らさないため、可視かつ WHERE 一致の
            // 行が `offset + cap` を超えた時点で `cap + 1` 件目が確定する。状態予算の計上・
            // キー抽出の前に拒否し、以降の走査を止める。
            if matches!(stop_threshold, Some(t) if materialized.len() >= t) {
                return Err(crate::sql::scan::row_limit_exceeded());
            }

            state_bytes = try_accumulate_state_budget(
                state_bytes,
                std::mem::size_of::<MaterializedRow>(),
                state_cap,
            )?;

            let value_embedding: &[f32] = match tier {
                DecodeTier::Embedding => embedding_scratch.as_slice(),
                DecodeTier::Fast | DecodeTier::DimAndScalar => &[],
            };
            let WindowRowValues {
                partition_keys,
                order_keys,
                agg_values,
                offset_values,
            } = collect_window_row_values(
                bound.windows(),
                &mut offset_eval,
                id,
                dim,
                value_embedding,
                &scanned,
                &mut partition_counts,
                &mut state_bytes,
                state_cap,
            )?;

            // Issue #1189: 文全体の `ORDER BY` キー値と tenant 識別子。確保前に
            // 借用長から見積もって state 予算へ計上する（`sql::scan` の経路 (B) と
            // 同方針）。`ORDER BY` なしなら何も確保しない。
            let mut stmt_order_keys: Vec<Option<OrderValue>> = Vec::new();
            let mut tenant_idx: u32 = 0;
            if !bound.order_by.is_empty() {
                let mut refs: Vec<Option<ScalarKeyRef<'_>>> =
                    Vec::with_capacity(bound.order_by.len());
                for key in &bound.order_by {
                    refs.push(extract_order_value_ref(schema, key, id, &scanned)?);
                }
                let mut add = refs
                    .len()
                    .saturating_mul(std::mem::size_of::<Option<OrderValue>>());
                for r in &refs {
                    if let Some(ScalarKeyRef::Bytes(b) | ScalarKeyRef::Collated { text: b, .. }) = r
                    {
                        add = add.saturating_add(b.len());
                    }
                }
                let is_new_tenant = tenants.last().map(String::as_str) != Some(tenant_id);
                if is_new_tenant {
                    add = add
                        .saturating_add(std::mem::size_of::<String>())
                        .saturating_add(tenant_id.len());
                }
                state_bytes = try_accumulate_state_budget(state_bytes, add, state_cap)?;
                stmt_order_keys.reserve_exact(refs.len());
                for r in refs {
                    stmt_order_keys.push(match r {
                        Some(v) => Some(scalar_key_ref_to_owned(v)?),
                        None => None,
                    });
                }
                if is_new_tenant {
                    tenants.push(tenant_id.to_string());
                }
                let last = tenants
                    .len()
                    .checked_sub(1)
                    .ok_or_else(|| window_bug("tenant intern table unexpectedly empty"))?;
                tenant_idx = u32::try_from(last).map_err(|_| {
                    SqlSurfaceError::payload_too_large("window scan tenant count overflowed")
                })?;
            }

            materialized.push(MaterializedRow {
                seq,
                id,
                partition_keys,
                order_keys,
                agg_values,
                offset_values,
                stmt_order_keys,
                tenant_idx,
            });
            check_total_row_count(materialized.len(), MAX_WINDOW_ROWS)?;
            seq = seq.checked_add(1).ok_or_else(|| {
                SqlSurfaceError::payload_too_large("window scan row count overflowed")
            })?;
        }
    }

    Ok(MaterializeOutput {
        rows: materialized,
        tenants,
        state_bytes,
    })
}

/// 1 行分の、ウィンドウ項目ごとの `PARTITION BY`／`ORDER BY` キー・集計引数の値
/// （[`MaterializedRow`] の構成要素）。
struct WindowRowValues {
    partition_keys: Vec<Vec<Option<WindowKeyValue>>>,
    order_keys: Vec<Vec<Option<WindowKeyValue>>>,
    agg_values: Vec<WindowInputValue>,
    offset_values: Vec<Option<OffsetRowValues>>,
}

/// `scanned`（1 行分のスカラー列）から `items`（`bound.windows()`）ごとのキー・引数の値を
/// 取り出し、状態予算（`state_bytes`／`state_cap`）・パーティション数・パーティション行数の
/// 上限（いずれも超過は `54000`）へ計上する。ストレージ走査（[`materialize_rows`]）と
/// 評価済みセル（[`CellWindowEvaluator`]。Issue #1411）が共有する唯一の実装で、第 2 の
/// ウィンドウ値抽出を作らない。
#[allow(clippy::too_many_arguments)]
fn collect_window_row_values(
    items: &[BoundWindowItem],
    offset_eval: &mut OffsetEvaluator,
    id: u64,
    dim: u32,
    embedding: &[f32],
    scanned: &[Option<row_codec::ScalarRef<'_>>],
    partition_counts: &mut [HashMap<Vec<u8>, usize>],
    state_bytes: &mut usize,
    state_cap: usize,
) -> Result<WindowRowValues, SqlSurfaceError> {
    let mut partition_keys = Vec::with_capacity(items.len());
    let mut order_keys = Vec::with_capacity(items.len());
    let mut agg_values = Vec::with_capacity(items.len());
    // offset 関数を持たない項目も `None` の固定領域を行ごとに持つため、確保前に全項目分を
    // 状態予算へ計上する（`OffsetEvaluator::extract` 側では計上しない＝二重計上しない）。
    let offset_slots_bytes = std::mem::size_of::<Option<OffsetRowValues>>()
        .checked_mul(items.len())
        .ok_or_else(|| window_bug("offset slot size overflow"))?;
    *state_bytes = try_accumulate_state_budget(*state_bytes, offset_slots_bytes, state_cap)?;
    let mut offset_values = Vec::with_capacity(items.len());
    for (item_index, item) in items.iter().enumerate() {
        let mut pkeys = Vec::with_capacity(item.partition_by.len());
        for key in &item.partition_by {
            let value = extract_window_key(key, id, scanned)?;
            *state_bytes = try_accumulate_state_budget(
                *state_bytes,
                window_key_value_bytes(&value),
                state_cap,
            )?;
            pkeys.push(value);
        }
        let partition_bytes = partition_key_bytes(&pkeys);
        let counts = partition_counts
            .get_mut(item_index)
            .ok_or_else(|| window_bug("partition count table missing for a window item"))?;
        let is_new_partition = !counts.contains_key(&partition_bytes);
        if is_new_partition {
            check_new_partition_capacity(counts.len(), MAX_WINDOW_PARTITIONS)?;
        }
        let count = counts.entry(partition_bytes).or_insert(0);
        *count += 1;
        check_partition_row_count(*count, MAX_WINDOW_FRAME_ROWS)?;

        let mut okeys = Vec::with_capacity(item.order_by.len());
        for ordering in &item.order_by {
            let key = &ordering.key;
            let value = extract_window_key(key, id, scanned)?;
            *state_bytes = try_accumulate_state_budget(
                *state_bytes,
                window_key_value_bytes(&value),
                state_cap,
            )?;
            okeys.push(value);
        }

        let agg_value = match &item.input {
            None => WindowInputValue::None,
            Some(input) => {
                let value = extract_window_input(input, dim, scanned)?;
                *state_bytes = try_accumulate_state_budget(
                    *state_bytes,
                    window_input_value_bytes(&value),
                    state_cap,
                )?;
                value
            }
        };

        let offset_value = match &item.offset_args {
            Some(args) => Some(offset_eval.extract(
                item_index,
                args,
                id,
                embedding,
                scanned,
                state_bytes,
                state_cap,
            )?),
            None => None,
        };

        partition_keys.push(pkeys);
        order_keys.push(okeys);
        agg_values.push(agg_value);
        offset_values.push(offset_value);
    }
    Ok(WindowRowValues {
        partition_keys,
        order_keys,
        agg_values,
        offset_values,
    })
}

/// 評価済みのセル由来の行を母集合としてウィンドウ項目を評価する実行器（Issue #1411・
/// TABLE-18。`sql::view_buffered` が評価後射影形ビューの本文結果に対する外側のウィンドウ関数で
/// 使う）。ストレージを読まないため RLS・可視性判定には関与しない（渡される行は呼び出し元
/// ＝本文の実行経路が参照セッションの `PolicyContext` で確定済みで、さらに外側 `WHERE` を
/// 通った行だけ）。キー・引数の抽出・状態予算・パーティション／行数の上限・パーティション
/// 評価（安定ソート → peer 分割 → 順位／集計）は [`materialize_rows`]／[`evaluate_window_item`] と
/// 同じ実装を共有する。行の同順位のタイブレークは投入順（`seq`）で、行 `id` は持たない
/// （疑似列 `id` をキーに使う項目は構築時に拒否する。ビューが公開する `id` は実列として束縛される）。
pub(crate) struct CellWindowEvaluator<'a> {
    items: &'a [BoundWindowItem],
    rows: Vec<MaterializedRow>,
    state_bytes: usize,
    partition_counts: Vec<HashMap<Vec<u8>, usize>>,
    offset_eval: OffsetEvaluator,
    /// 実行開始時に解決した `ROWS`／`RANGE` フレーム（項目ごと。[`resolve_frames`]）。
    frames: Vec<Option<ResolvedFrame>>,
}

impl<'a> CellWindowEvaluator<'a> {
    /// `items` は合成スキーマ（本文の結果列と同じ並び）に束縛済みのウィンドウ項目。
    pub(crate) fn new(
        items: &'a [BoundWindowItem],
        column_types: &[Option<ColumnType>],
    ) -> Result<Self, SqlSurfaceError> {
        for item in items {
            // バッファ済みビュー経路のセル→スカラー変換（`cell_scalar`）が写せない型（TIME／
            // TIMESTAMPTZ／INTERVAL 等）の列を値・既定値に取る LAG／LEAD は、内部エラーでなく
            // 固定 `0A000` で拒否する（fail-closed。データ有無に依らない）。判定は `cell_scalar` と
            // 同じ集合を持つ `cell_scalar_supports` に一本化し、乖離を防ぐ。
            if let Some(args) = &item.offset_args {
                let value_ty = match &args.value {
                    BoundWindowValue::Column { ty, .. } => Some(ty),
                    _ => None,
                };
                let default_ty = args.default_column.as_ref().map(|(_, ty)| ty);
                // 式形の引数（value／offset／default）が読む本文列も同じ型検査にかける
                // （参照列は `mark_referenced_columns` と同じ走査で集める）。
                let mut mask = vec![false; column_types.len()];
                args.mark_referenced_columns(&mut mask);
                let expr_unsupported = mask.iter().zip(column_types.iter()).any(|(used, ty)| {
                    *used
                        && ty
                            .as_ref()
                            .is_some_and(|t| !crate::sql::join::values::cell_scalar_supports(t))
                });
                if expr_unsupported
                    || value_ty
                        .into_iter()
                        .chain(default_ty)
                        .any(|ty| !crate::sql::join::values::cell_scalar_supports(ty))
                {
                    return Err(SqlSurfaceError::FeatureNotSupported {
                        detail: "window value functions over TIME, TIMESTAMPTZ or INTERVAL columns of a buffered view are not supported"
                            .to_string(),
                    });
                }
            }
            let uses_pseudo_id = item
                .partition_by
                .iter()
                .chain(item.order_by.iter().map(|o| &o.key))
                .any(|k| matches!(k, WindowKeyRef::Id))
                || matches!(item.input, Some(AggregateInput::IdU64))
                || item
                    .offset_args
                    .as_ref()
                    .is_some_and(|a| a.references_pseudo_id());
            if uses_pseudo_id {
                return Err(window_bug(
                    "window item references the pseudo column id over evaluated cells",
                ));
            }
        }
        let frames = resolve_frames(items)?;
        Ok(Self {
            items,
            rows: Vec::new(),
            state_bytes: 0,
            partition_counts: vec![HashMap::new(); items.len()],
            offset_eval: OffsetEvaluator::new(items),
            frames,
        })
    }

    /// 1 行（`scanned` は合成スキーマ長のセル由来スカラー）を母集合へ加える。
    pub(crate) fn push_row(
        &mut self,
        scanned: &[Option<row_codec::ScalarRef<'_>>],
    ) -> Result<(), SqlSurfaceError> {
        self.state_bytes = try_accumulate_state_budget(
            self.state_bytes,
            std::mem::size_of::<MaterializedRow>(),
            MAX_WINDOW_STATE_BYTES,
        )?;
        let WindowRowValues {
            partition_keys,
            order_keys,
            agg_values,
            offset_values,
        } = collect_window_row_values(
            self.items,
            &mut self.offset_eval,
            0,
            0,
            &[],
            scanned,
            &mut self.partition_counts,
            &mut self.state_bytes,
            MAX_WINDOW_STATE_BYTES,
        )?;
        let seq = self.rows.len();
        self.rows.push(MaterializedRow {
            seq,
            id: 0,
            partition_keys,
            order_keys,
            agg_values,
            offset_values,
            stmt_order_keys: Vec::new(),
            tenant_idx: 0,
        });
        check_total_row_count(self.rows.len(), MAX_WINDOW_ROWS)
    }

    /// ウィンドウ値を確定する。戻り値は `[項目][投入順の行]`。
    pub(crate) fn finish(mut self) -> Result<Vec<Vec<Cell>>, SqlSurfaceError> {
        let mut out = Vec::with_capacity(self.items.len());
        for (item_index, item) in self.items.iter().enumerate() {
            out.push(evaluate_window_item(
                item,
                item_index,
                &self.rows,
                self.frames.get(item_index).and_then(|f| f.as_ref()),
                &mut self.state_bytes,
                MAX_WINDOW_STATE_BYTES,
            )?);
        }
        Ok(out)
    }
}

/// [`decode_tier_for`](crate::sql::scan) と同じ意図（Issue #350）だが、ウィンドウ
/// 項目の `PARTITION BY`／`ORDER BY` キー・集計引数も参照集合へ反映する
/// （SQL-30・TASK-214）。
fn decode_tier_for_window(schema: &TableSchema, bound: &BoundScan) -> (DecodeTier, Vec<bool>) {
    let mut scalar_mask = vec![false; schema.columns.len()];
    let mut needs_embedding = false;
    let mut has_scalar_reference = false;

    for col in bound.projection() {
        match col {
            ProjectedColumn::Column { index, .. } => {
                if let Some(column) = schema.columns.get(*index) {
                    match &column.ty {
                        ColumnType::Vector(_) => needs_embedding = true,
                        _ => {
                            has_scalar_reference = true;
                            if let Some(slot) = scalar_mask.get_mut(*index) {
                                *slot = true;
                            }
                        }
                    }
                }
            }
            ProjectedColumn::Computed { expr, .. } => {
                if udf_call::references_embedding(expr) {
                    needs_embedding = true;
                }
            }
            ProjectedColumn::Id | ProjectedColumn::AliasedId { .. } => {}
        }
    }
    if !bound.metadata_filters().is_empty() {
        has_scalar_reference = true;
    }
    for filter in bound.metadata_filters() {
        if let Some(slot) = scalar_mask.get_mut(filter.column_index()) {
            *slot = true;
        }
    }
    for expr in bound.expr_filters() {
        if udf_call::references_embedding(expr) {
            needs_embedding = true;
        }
        // Issue #919・SQL-26: `WHERE` 式が参照する `TEXT` 列を `scalar_mask` へ
        // 反映する（`sql::scan::decode_tier_for` と同一契約。取りこぼすと
        // マスク外参照＝実 NULL の誤判定になる fail-closed 違反）。
        if udf_call::mark_referenced_scalar_columns(expr, &mut scalar_mask) {
            has_scalar_reference = true;
        }
    }
    if !bound.or_filters().is_empty() {
        has_scalar_reference = true;
    }
    for group in bound.or_filters() {
        group.visit_column_indices(&mut |idx| {
            if let Some(slot) = scalar_mask.get_mut(idx) {
                *slot = true;
            }
        });
        if group.references_embedding() {
            needs_embedding = true;
        }
    }

    // Issue #1189: 文全体の `ORDER BY` キー列もデコード対象へ含める
    // （`sql::scan::decode_tier_for` と同契約。漏れると NULL として並ぶ）。
    for key in &bound.order_by {
        if let crate::sql::parser::BoundOrderTarget::Column(index) = key.target {
            has_scalar_reference = true;
            if let Some(slot) = scalar_mask.get_mut(index) {
                *slot = true;
            }
        }
    }

    for item in bound.windows() {
        // `LAG`／`LEAD` の値・オフセット・既定値が参照する列も反映する（Issue #1880。
        // 取りこぼすとマスク外の列を NULL と誤判定する fail-closed 違反になるため、評価を
        // 接続する Issue #1881 より前に先回りで入れておく）。
        if let Some(args) = &item.offset_args {
            if args.mark_referenced_columns(&mut scalar_mask) {
                has_scalar_reference = true;
            }
            if args.references_embedding() {
                needs_embedding = true;
            }
        }
        for key in item
            .partition_by
            .iter()
            .chain(item.order_by.iter().map(|o| &o.key))
        {
            if let WindowKeyRef::Column { index, .. } = key {
                has_scalar_reference = true;
                if let Some(slot) = scalar_mask.get_mut(*index) {
                    *slot = true;
                }
            }
        }
        match &item.input {
            None | Some(AggregateInput::AllVisible) | Some(AggregateInput::IdU64) => {}
            Some(AggregateInput::VectorColumnPresence) => {
                has_scalar_reference = true;
            }
            // 照合付きの MIN/MAX は束縛段（`bind_window_item`）が `0A000` で拒否するため実際には
            // 到達しないが、列参照としてはテキスト列と同じ扱いにしておく。
            Some(
                AggregateInput::TextColumn(index)
                | AggregateInput::CollatedTextColumn { index, .. }
                | AggregateInput::BooleanColumn(index)
                | AggregateInput::DateColumn(index)
                | AggregateInput::TimestampColumn(index)
                | AggregateInput::TimeColumn(index)
                | AggregateInput::IntervalColumn(index)
                | AggregateInput::TimestampTzColumn(index)
                | AggregateInput::ArrayColumn(index)
                | AggregateInput::ByteaColumn(index)
                | AggregateInput::JsonColumn(index)
                | AggregateInput::EnumColumn(index)
                | AggregateInput::UuidColumn(index)
                | AggregateInput::IntegerColumn(index)
                | AggregateInput::SmallIntColumn(index)
                | AggregateInput::BigIntColumn(index)
                | AggregateInput::RealColumn(index)
                | AggregateInput::DoubleColumn(index),
            ) => {
                has_scalar_reference = true;
                if let Some(slot) = scalar_mask.get_mut(*index) {
                    *slot = true;
                }
            }
            Some(AggregateInput::NumericColumn { index, .. }) => {
                has_scalar_reference = true;
                if let Some(slot) = scalar_mask.get_mut(*index) {
                    *slot = true;
                }
            }
            // 構文段が複合式の引数を受理しないため到達しない
            // （`Parser::parse_window_item` 参照）。
            Some(AggregateInput::ScalarExpr { .. }) => {
                has_scalar_reference = true;
            }
            // `string_agg` はウィンドウ関数ではない（`WindowFunc::from_name` が拒否済み）。
            Some(
                AggregateInput::StringAgg(_)
                | AggregateInput::ArrayAgg(_)
                | AggregateInput::BoolExpr { .. }
                | AggregateInput::TypedExpr { .. }
                | AggregateInput::GroupingKeys(_),
            ) => {
                has_scalar_reference = true;
            }
        }
    }

    let tier = if needs_embedding {
        DecodeTier::Embedding
    } else if has_scalar_reference
        || scalar_mask.iter().any(|&wanted| wanted)
        || !bound.expr_filters().is_empty()
    {
        DecodeTier::DimAndScalar
    } else {
        DecodeTier::Fast
    };
    (tier, scalar_mask)
}

/// [`WindowKeyRef`] の値を `scanned`（同じ行の `scan_scalar_columns_masked`
/// 結果）から取り出す。`kind` はスキーマ束縛時（`sql::parser::resolve_window_key`）
/// に確定済みで、`scanned` は同じ `schema` から得ているため型不一致は実装バグと
/// して fail-closed に拒否する。
fn extract_window_key(
    key: &WindowKeyRef,
    id: u64,
    scanned: &[Option<row_codec::ScalarRef<'_>>],
) -> Result<Option<WindowKeyValue>, SqlSurfaceError> {
    let (index, kind) = match key {
        WindowKeyRef::Id => return Ok(Some(WindowKeyValue::Id(id))),
        WindowKeyRef::Column { index, kind } => (*index, *kind),
    };
    let raw = scanned.get(index).copied().flatten();
    Ok(match (kind, raw) {
        (WindowKeyKind::Text, Some(row_codec::ScalarRef::Text(s))) => {
            Some(WindowKeyValue::Text(aggregate::try_clone_str(s)?))
        }
        (WindowKeyKind::Text, None) => None,
        (WindowKeyKind::Integer, Some(row_codec::ScalarRef::Integer(v))) => {
            Some(WindowKeyValue::Integer(v))
        }
        (WindowKeyKind::Integer, None) => None,
        (WindowKeyKind::BigInt, Some(row_codec::ScalarRef::BigInt(v))) => {
            Some(WindowKeyValue::BigInt(v))
        }
        (WindowKeyKind::BigInt, None) => None,
        (WindowKeyKind::Real, Some(row_codec::ScalarRef::Real(v))) => Some(WindowKeyValue::Real(v)),
        (WindowKeyKind::Real, None) => None,
        (WindowKeyKind::Double, Some(row_codec::ScalarRef::Double(v))) => {
            Some(WindowKeyValue::Double(v))
        }
        (WindowKeyKind::Double, None) => None,
        (WindowKeyKind::Boolean, Some(row_codec::ScalarRef::Bool(v))) => {
            Some(WindowKeyValue::Boolean(v))
        }
        (WindowKeyKind::Boolean, None) => None,
        (WindowKeyKind::Date, Some(v)) => match v.as_date() {
            Some(d) => Some(WindowKeyValue::Date(d)),
            None => return Err(window_bug("DATE key scan yielded a non-Date scalar value")),
        },
        (WindowKeyKind::Date, None) => None,
        (WindowKeyKind::Timestamp, Some(v)) => match v.as_timestamp() {
            Some(t) => Some(WindowKeyValue::Timestamp(t)),
            None => {
                return Err(window_bug(
                    "TIMESTAMP key scan yielded a non-Timestamp scalar value",
                ))
            }
        },
        (WindowKeyKind::Timestamp, None) => None,
        (WindowKeyKind::Time, Some(v)) => match v.as_time() {
            Some(t) => Some(WindowKeyValue::Time(t)),
            None => return Err(window_bug("TIME key scan yielded a non-Time scalar value")),
        },
        (WindowKeyKind::Time, None) => None,
        (WindowKeyKind::TimestampTz, Some(v)) => match v.as_timestamptz() {
            Some(t) => Some(WindowKeyValue::TimestampTz(t)),
            None => {
                return Err(window_bug(
                    "TIMESTAMPTZ key scan yielded a non-TimestampTz scalar value",
                ))
            }
        },
        (WindowKeyKind::TimestampTz, None) => None,
        (WindowKeyKind::Numeric, Some(v)) => match v.as_numeric() {
            Some(d) => Some(WindowKeyValue::Numeric(d)),
            None => {
                return Err(window_bug(
                    "NUMERIC key scan yielded a non-Numeric scalar value",
                ))
            }
        },
        (WindowKeyKind::Numeric, None) => None,
        (WindowKeyKind::Uuid, Some(v)) => match v.as_uuid() {
            Some(u) => Some(WindowKeyValue::Uuid(u)),
            None => return Err(window_bug("UUID key scan yielded a non-Uuid scalar value")),
        },
        (WindowKeyKind::Uuid, None) => None,
        (_, Some(_)) => return Err(window_bug("window key scan type mismatch")),
    })
}

/// [`AggregateInput`] の値を `scanned`／`dim` から取り出す（SQL-30・TASK-214）。
/// 構文段が複合式を受理しないため `ScalarExpr` は到達しない
/// （到達した場合は実装バグとして fail-closed に拒否する）。
fn extract_window_input(
    input: &AggregateInput,
    dim: u32,
    scanned: &[Option<row_codec::ScalarRef<'_>>],
) -> Result<WindowInputValue, SqlSurfaceError> {
    Ok(match input {
        AggregateInput::AllVisible | AggregateInput::IdU64 => WindowInputValue::AllVisibleOrId,
        AggregateInput::VectorColumnPresence => WindowInputValue::VectorPresence(dim),
        AggregateInput::CollatedTextColumn { .. } => {
            return Err(window_bug("collated MIN/MAX reached the window executor"))
        }
        AggregateInput::TextColumn(index) => WindowInputValue::Text(
            scanned
                .get(*index)
                .copied()
                .flatten()
                .and_then(|v| v.as_text())
                .map(aggregate::try_clone_str)
                .transpose()?,
        ),
        AggregateInput::IntegerColumn(index) | AggregateInput::SmallIntColumn(index) => {
            match scanned.get(*index).copied().flatten() {
                Some(row_codec::ScalarRef::Integer(v)) => WindowInputValue::Integer(Some(v)),
                Some(_) => return Err(window_bug("INTEGER input scan type mismatch")),
                None => WindowInputValue::Integer(None),
            }
        }
        AggregateInput::BigIntColumn(index) => match scanned.get(*index).copied().flatten() {
            Some(row_codec::ScalarRef::BigInt(v)) => WindowInputValue::BigInt(Some(v)),
            Some(_) => return Err(window_bug("BIGINT input scan type mismatch")),
            None => WindowInputValue::BigInt(None),
        },
        AggregateInput::RealColumn(index) => match scanned.get(*index).copied().flatten() {
            Some(row_codec::ScalarRef::Real(v)) => WindowInputValue::Real(Some(v)),
            Some(_) => return Err(window_bug("REAL input scan type mismatch")),
            None => WindowInputValue::Real(None),
        },
        AggregateInput::DoubleColumn(index) => match scanned.get(*index).copied().flatten() {
            Some(row_codec::ScalarRef::Double(v)) => WindowInputValue::Double(Some(v)),
            Some(_) => return Err(window_bug("DOUBLE input scan type mismatch")),
            None => WindowInputValue::Double(None),
        },
        AggregateInput::DateColumn(index) => {
            match scanned.get(*index).copied().flatten() {
                Some(v) => WindowInputValue::Date(Some(v.as_date().ok_or_else(|| {
                    window_bug("DATE input scan yielded a non-Date scalar value")
                })?)),
                None => WindowInputValue::Date(None),
            }
        }
        AggregateInput::TimestampColumn(index) => match scanned.get(*index).copied().flatten() {
            Some(v) => WindowInputValue::Timestamp(Some(v.as_timestamp().ok_or_else(|| {
                window_bug("TIMESTAMP input scan yielded a non-Timestamp scalar value")
            })?)),
            None => WindowInputValue::Timestamp(None),
        },
        AggregateInput::TimestampTzColumn(index) => match scanned.get(*index).copied().flatten() {
            Some(v) => {
                WindowInputValue::TimestampTz(Some(v.as_timestamptz().ok_or_else(|| {
                    window_bug("TIMESTAMPTZ input scan yielded a non-TimestampTz scalar value")
                })?))
            }
            None => WindowInputValue::TimestampTz(None),
        },
        // INTERVAL 列のウィンドウ集計は束縛段（`bind_window_item`）が `0A000` で拒否済みのため
        // ここへ到達するのは配線不備のみ（Issue #1580。fail-closed）。
        AggregateInput::IntervalColumn(_) => {
            return Err(window_bug(
                "INTERVAL window aggregate input must be rejected at bind time",
            ))
        }
        AggregateInput::TimeColumn(index) => {
            match scanned.get(*index).copied().flatten() {
                Some(v) => WindowInputValue::Time(Some(v.as_time().ok_or_else(|| {
                    window_bug("TIME input scan yielded a non-Time scalar value")
                })?)),
                None => WindowInputValue::Time(None),
            }
        }
        AggregateInput::NumericColumn { index, .. } => match scanned.get(*index).copied().flatten()
        {
            Some(v) => WindowInputValue::Numeric(Some(v.as_numeric().ok_or_else(|| {
                window_bug("NUMERIC input scan yielded a non-Numeric scalar value")
            })?)),
            None => WindowInputValue::Numeric(None),
        },
        AggregateInput::BooleanColumn(index)
        | AggregateInput::ArrayColumn(index)
        | AggregateInput::ByteaColumn(index)
        | AggregateInput::JsonColumn(index)
        | AggregateInput::EnumColumn(index)
        | AggregateInput::UuidColumn(index) => {
            WindowInputValue::PresenceOnly(scanned.get(*index).copied().flatten().is_some())
        }
        // 構文段（`Parser::parse_window_item`）が裸の列参照・`*` のみを受理する
        // ため到達しない。
        AggregateInput::ScalarExpr { .. }
        | AggregateInput::StringAgg(_)
        | AggregateInput::ArrayAgg(_)
        | AggregateInput::BoolExpr { .. }
        | AggregateInput::TypedExpr { .. }
        | AggregateInput::GroupingKeys(_) => {
            return Err(window_bug(
                "window aggregate input unexpectedly carries a compound expression",
            ))
        }
    })
}

/// `Option<WindowKeyValue>` 1 つの概算メモリコスト（[`MAX_WINDOW_STATE_BYTES`]
/// 予算計上用）。
fn window_key_value_bytes(value: &Option<WindowKeyValue>) -> usize {
    let base = std::mem::size_of::<Option<WindowKeyValue>>();
    match value {
        Some(WindowKeyValue::Text(s)) => base + s.len(),
        _ => base,
    }
}

/// [`WindowInputValue`] 1 つの概算メモリコスト（同上）。
fn window_input_value_bytes(value: &WindowInputValue) -> usize {
    let base = std::mem::size_of::<WindowInputValue>();
    match value {
        WindowInputValue::Text(Some(s)) => base + s.len(),
        _ => base,
    }
}

/// 評価段（[`evaluate_partition`]）が peer グループごとに複製する [`Cell`] 1 つの
/// 概算メモリコスト（[`MAX_WINDOW_STATE_BYTES`] 予算計上用。上記 2 関数と同方針）。
/// `MIN`/`MAX(<TEXT 列>)` は `Cell::Text` を保持しうるため文字列長も計上する
/// （`resolve_aggregate_input` が TEXT を `MIN`/`MAX` に受理する。レビュー指摘）。
fn window_cell_state_bytes(cell: &Cell) -> usize {
    let base = std::mem::size_of::<Cell>();
    match cell {
        Cell::Text(s) => base.saturating_add(s.len()),
        _ => base,
    }
}

/// パーティションキー（複数列）の同値判定用の正準バイト列を作る。NULL 同士は
/// 常に同じタグ（`0`）にエンコードされるため、`GROUP BY` と同じ「NULL 同士は
/// 同値」の規則を自然に満たす。ハッシュ照合専用の識別子であり、順序は保証しない
/// （順序が必要な箇所は [`cmp_window_key`] を使う）。
fn partition_key_bytes(keys: &[Option<WindowKeyValue>]) -> Vec<u8> {
    let mut out = Vec::new();
    for key in keys {
        match key {
            None => out.push(0u8),
            Some(WindowKeyValue::Id(v)) => {
                out.push(1);
                out.extend_from_slice(&v.to_be_bytes());
            }
            Some(WindowKeyValue::Text(s)) => {
                out.push(2);
                out.extend_from_slice(&(s.len() as u64).to_be_bytes());
                out.extend_from_slice(s.as_bytes());
            }
            Some(WindowKeyValue::Integer(v)) => {
                out.push(3);
                out.extend_from_slice(&v.to_be_bytes());
            }
            Some(WindowKeyValue::BigInt(v)) => {
                out.push(4);
                out.extend_from_slice(&v.to_be_bytes());
            }
            Some(WindowKeyValue::Real(v)) => {
                out.push(5);
                out.extend_from_slice(&v.to_bits().to_be_bytes());
            }
            Some(WindowKeyValue::Double(v)) => {
                out.push(6);
                out.extend_from_slice(&v.to_bits().to_be_bytes());
            }
            Some(WindowKeyValue::Boolean(v)) => {
                out.push(7);
                out.push(u8::from(*v));
            }
            Some(WindowKeyValue::Date(v)) => {
                out.push(8);
                out.extend_from_slice(&v.to_be_bytes());
            }
            Some(WindowKeyValue::Timestamp(v)) => {
                out.push(9);
                out.extend_from_slice(&v.to_be_bytes());
            }
            Some(WindowKeyValue::Time(v)) => {
                out.push(13);
                out.extend_from_slice(&v.to_be_bytes());
            }
            Some(WindowKeyValue::TimestampTz(v)) => {
                out.push(14);
                out.extend_from_slice(&v.to_be_bytes());
            }
            Some(WindowKeyValue::Numeric(d)) => {
                out.push(10);
                out.extend_from_slice(&d.unscaled().to_be_bytes());
                out.push(d.scale());
            }
            Some(WindowKeyValue::Uuid(u)) => {
                out.push(11);
                out.extend_from_slice(u.as_bytes());
            }
        }
    }
    out
}

/// `PARTITION BY`／`ORDER BY` の同値・大小比較で使う型ごとの比較（NULL は
/// PostgreSQL 既定〔ASC は末尾・DESC は先頭〕）。同じ添字のキーは束縛時
/// （`resolve_window_key`）にすべて同じ `kind` を持つため、異なる variant 同士の
/// 比較は到達しない（安定ソートを壊さないよう `Equal` へ倒す）。
fn cmp_window_key(
    a: &Option<WindowKeyValue>,
    b: &Option<WindowKeyValue>,
    descending: bool,
    nulls: NullPlacement,
) -> std::cmp::Ordering {
    compare_nullable(
        a.as_ref(),
        b.as_ref(),
        descending,
        nulls,
        cmp_window_key_value,
    )
}

fn cmp_window_key_value(a: &WindowKeyValue, b: &WindowKeyValue) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (WindowKeyValue::Id(x), WindowKeyValue::Id(y)) => x.cmp(y),
        // 照合の比較器（既定照合）に 1 本化。非バイト列順の照合（ICU）のキー・MIN/MAX は束縛段が
        // 拒否する（`resolve_window_key`・`bind_window_item`。TABLE-28・Issue #2190）。
        (WindowKeyValue::Text(x), WindowKeyValue::Text(y)) => {
            crate::collation::Collation::DEFAULT.compare(x.as_bytes(), y.as_bytes())
        }
        (WindowKeyValue::Integer(x), WindowKeyValue::Integer(y)) => x.cmp(y),
        (WindowKeyValue::BigInt(x), WindowKeyValue::BigInt(y)) => x.cmp(y),
        (WindowKeyValue::Real(x), WindowKeyValue::Real(y)) => {
            f64::from(*x).total_cmp(&f64::from(*y))
        }
        (WindowKeyValue::Double(x), WindowKeyValue::Double(y)) => x.total_cmp(y),
        (WindowKeyValue::Boolean(x), WindowKeyValue::Boolean(y)) => x.cmp(y),
        (WindowKeyValue::Date(x), WindowKeyValue::Date(y)) => x.cmp(y),
        (WindowKeyValue::Timestamp(x), WindowKeyValue::Timestamp(y)) => x.cmp(y),
        (WindowKeyValue::Time(x), WindowKeyValue::Time(y)) => x.cmp(y),
        (WindowKeyValue::TimestampTz(x), WindowKeyValue::TimestampTz(y)) => x.cmp(y),
        (WindowKeyValue::Numeric(x), WindowKeyValue::Numeric(y)) => crate::numeric::cmp_exact(x, y),
        (WindowKeyValue::Uuid(x), WindowKeyValue::Uuid(y)) => x.as_bytes().cmp(y.as_bytes()),
        _ => Ordering::Equal,
    }
}

/// ウィンドウ項目 1 つ（`bound.windows()[item_index]`）を評価し、`materialized` と
/// 同じ添字（走査順）で引ける `Vec<Cell>` を返す（SQL-30・TASK-214）。他テナントの
/// 可視 Public 行と衝突しうる `id` をキーにした写像にしないのは、`materialize_rows`
/// の物理走査順で行を一意に区別するため（レビュー指摘対応。`build_result` 側の
/// コメント参照）。`state_bytes`／`state_cap` は materialize 段から引き継いだ
/// [`MAX_WINDOW_STATE_BYTES`] 予算で、本関数が新たに確保する `partitions` マップ・
/// 結果ベクタもここへ計上する（レビュー指摘 3）。
fn evaluate_window_item(
    item: &BoundWindowItem,
    item_index: usize,
    materialized: &[MaterializedRow],
    frame: Option<&ResolvedFrame>,
    state_bytes: &mut usize,
    state_cap: usize,
) -> Result<Vec<Cell>, SqlSurfaceError> {
    let mut partitions: HashMap<Vec<u8>, Vec<usize>> = HashMap::new();
    for (row_idx, row) in materialized.iter().enumerate() {
        let bytes = partition_key_bytes(&row.partition_keys[item_index]);
        // `partitions` の `Vec<u8>` キー・`Vec<usize>` 値本体（行ごとに 1
        // `usize` を追加）の確保も他の materialize 段の確保と同じ予算で
        // 有界にする（レビュー指摘 3。キー本体は同一パーティションの 2 行目
        // 以降は再利用されるが、安全側に倒して行ごとに計上する）。
        *state_bytes = try_accumulate_state_budget(
            *state_bytes,
            bytes.len().saturating_add(std::mem::size_of::<usize>()),
            state_cap,
        )?;
        partitions.entry(bytes).or_default().push(row_idx);
    }

    // 結果ベクタ本体の確保も同じ予算へ計上する（レビュー指摘 3）。
    *state_bytes = try_accumulate_state_budget(
        *state_bytes,
        materialized
            .len()
            .saturating_mul(std::mem::size_of::<Option<Cell>>()),
        state_cap,
    )?;
    let mut result: Vec<Option<Cell>> = vec![None; materialized.len()];
    for indices in partitions.into_values() {
        let part = SortedPartition::new(item, item_index, materialized, indices);
        evaluate_partition(&part, frame, &mut result, state_bytes, state_cap)?;
    }
    result
        .into_iter()
        .enumerate()
        .map(|(row_idx, cell)| {
            cell.ok_or_else(|| {
                window_bug(&format!(
                    "evaluate_partition left row index {row_idx} without a computed value"
                ))
            })
        })
        .collect()
}

/// `LAG`／`LEAD` の 1 パーティション分（`indices` は並べ替え済み）を評価する（Issue #1881）。
/// オフセットが NULL なら NULL、オフセット先がパーティション内ならその行の値（値が NULL なら
/// NULL。既定値へは落とさない）、範囲外なら現在行で評価した既定値（なければ NULL）。値・既定値の
/// 評価エラーは使用時に初めて返す（PostgreSQL と同じく、参照されない行のエラーを出さない）。
fn evaluate_offset_partition(
    part: &SortedPartition<'_>,
    out: &mut [Option<Cell>],
    state_bytes: &mut usize,
    state_cap: usize,
) -> Result<(), SqlSurfaceError> {
    let item = part.item;
    let item_index = part.item_index;
    let materialized = part.materialized;
    let indices = part.row_indices();
    let forward = matches!(item.func, WindowFunc::Lead);
    let values_of = |row_idx: usize| -> Result<&OffsetRowValues, SqlSurfaceError> {
        materialized
            .get(row_idx)
            .and_then(|r| r.offset_values.get(item_index))
            .and_then(|v| v.as_ref())
            .ok_or_else(|| window_bug("offset window values missing for a row"))
    };
    for (pos, &row_idx) in indices.iter().enumerate() {
        let current = values_of(row_idx)?;
        let picked: Option<&Cell> = match current.offset {
            None => None,
            Some(o) => match offset_target_index(pos, indices.len(), o, forward) {
                Some(target) => {
                    let target_row = part.row_index(target)?;
                    Some(
                        values_of(target_row)?
                            .value
                            .as_ref()
                            .map_err(Clone::clone)?,
                    )
                }
                None => match &current.default {
                    None => None,
                    Some(d) => Some(d.as_ref().map_err(Clone::clone)?),
                },
            },
        };
        let cell = match picked {
            None => Cell::Null,
            Some(c) => {
                // 複製の前に状態予算へ計上する（確保前の検証）。
                *state_bytes = try_accumulate_state_budget(
                    *state_bytes,
                    window_cell_state_bytes(c),
                    state_cap,
                )?;
                c.clone()
            }
        };
        let slot = out.get_mut(row_idx).ok_or_else(|| {
            window_bug("evaluate_offset_partition row index out of range for the result slice")
        })?;
        *slot = Some(cell);
    }
    Ok(())
}

/// フレーム内から引く位置の種別（[`pick_in_frame`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FramePick {
    First,
    Last,
    /// 0 始まりの序数（`nth_value(v, n)` の `n - 1`）。
    Nth(usize),
}

/// フレーム `[start, end)` から除外区間 `excluded`（半開区間。フレームの外にあってもよい）を除いた
/// 行の中で、`pick` が指す位置を返す（無ければ `None`＝空フレーム・序数超過）。除外は高々 2 区間
/// （`EXCLUDE TIES`）で、残りは高々 3 区間なので固定長配列と飽和演算だけで O(1) に求める。
fn pick_in_frame(
    start: usize,
    end: usize,
    excluded: &[(usize, usize)],
    pick: FramePick,
) -> Option<usize> {
    const MAX_PIECES: usize = 4;
    let mut pieces = [(0usize, 0usize); MAX_PIECES];
    let mut count = 0usize;
    if start < end {
        pieces[0] = (start, end);
        count = 1;
    }
    for &(ex_start, ex_end) in excluded {
        if ex_start >= ex_end {
            continue;
        }
        let mut next = [(0usize, 0usize); MAX_PIECES];
        let mut next_count = 0usize;
        for &(a, b) in pieces.iter().take(count) {
            for (lo, hi) in [(a, b.min(ex_start)), (a.max(ex_end), b)] {
                if lo < hi && next_count < MAX_PIECES {
                    next[next_count] = (lo, hi);
                    next_count += 1;
                }
            }
        }
        pieces = next;
        count = next_count;
    }
    let live = pieces.get(..count)?;
    match pick {
        FramePick::First => live.first().map(|&(lo, _)| lo),
        FramePick::Last => live.last().map(|&(_, hi)| hi.saturating_sub(1)),
        FramePick::Nth(k) => {
            let mut k = k;
            for &(lo, hi) in live {
                let len = hi.saturating_sub(lo);
                if k < len {
                    return Some(lo.saturating_add(k));
                }
                k -= len;
            }
            None
        }
    }
}

/// `ntile` の 1 パーティション分の状態（PostgreSQL の `window_ntile` と同じ状態機械。SQL-64 (4)・
/// Issue #1884）。バケット数に比例する確保をしないよう、スカラー 4 つだけを持つ。
#[derive(Debug, Default)]
struct NtileState {
    /// 現在のバケット番号（0 は未確定＝まだバケット数を読んでいない）。
    ntile: i64,
    rows_per_bucket: i64,
    boundary: i64,
    remainder: i64,
}

impl NtileState {
    /// バケット数 `n`（1 以上）と総行数 `total` から初期状態を確定する。
    fn start(&mut self, total: i64, n: i64) {
        self.boundary = total / n;
        if self.boundary == 0 {
            self.boundary = 1;
            self.remainder = 0;
        } else {
            self.remainder = total % n;
            if self.remainder != 0 {
                self.boundary += 1;
            }
        }
        self.ntile = 1;
        self.rows_per_bucket = 0;
    }

    /// 1 行分進め、その行のバケット番号を返す（[`NtileState::start`] 済みであること）。
    fn advance(&mut self) -> i64 {
        self.rows_per_bucket += 1;
        if self.boundary < self.rows_per_bucket {
            if self.remainder != 0 && self.ntile == self.remainder {
                self.remainder = 0;
                self.boundary -= 1;
            }
            self.ntile += 1;
            self.rows_per_bucket = 1;
        }
        self.ntile
    }
}

/// `ntile(num_buckets)` の 1 パーティション分の評価（SQL-64 (4)・Issue #1884。
/// [`evaluate_partition`] から呼ばれる）。PostgreSQL の `window_ntile` と同じく、並べ替えた順に
/// 行の位置ごとに進め（peer・フレームは区別しない）、バケット数が未確定の行でだけ引数を評価する。
/// 引数が NULL ならその行は NULL で、次の行で再評価する。0 以下は `22014`、`integer` 範囲外は
/// `22003`。0 行のパーティションは引数を評価しないためエラーにならない。母集合は materialize 段を
/// 通った行のみで、不可視行・他テナントの Private 行はバケット境界に影響しない。
fn evaluate_ntile_partition(
    part: &SortedPartition<'_>,
    out: &mut [Option<Cell>],
    state_bytes: &mut usize,
    state_cap: usize,
) -> Result<(), SqlSurfaceError> {
    let item_index = part.item_index;
    let total = i64::try_from(part.len()).unwrap_or(i64::MAX);
    let mut state = NtileState::default();
    for pos in 0..part.len() {
        crate::interrupt::check()?;
        let row_idx = part.row_index(pos)?;
        let cell = if state.ntile == 0 {
            let values = part
                .materialized
                .get(row_idx)
                .and_then(|r| r.offset_values.get(item_index))
                .and_then(|v| v.as_ref())
                .ok_or_else(|| window_bug("ntile argument missing for a row"))?;
            let arg = values.value.as_ref().map_err(Clone::clone)?;
            let n =
                match arg {
                    Cell::Null => None,
                    Cell::SignedInteger(v) => Some(i32::try_from(*v).map_err(|_| {
                        SqlSurfaceError::numeric_out_of_range("integer out of range")
                    })?),
                    _ => return Err(window_bug("ntile argument is not an integer cell")),
                };
            match n {
                None => Cell::Null,
                Some(n) if n <= 0 => {
                    return Err(SqlSurfaceError::InvalidArgumentForNtileFunction {
                        detail: "argument of ntile must be greater than zero",
                    });
                }
                Some(n) => {
                    state.start(total, i64::from(n));
                    Cell::SignedInteger(state.advance())
                }
            }
        } else {
            Cell::SignedInteger(state.advance())
        };
        *state_bytes =
            try_accumulate_state_budget(*state_bytes, window_cell_state_bytes(&cell), state_cap)?;
        let slot = out
            .get_mut(row_idx)
            .ok_or_else(|| window_bug("ntile row index out of range for the result slice"))?;
        *slot = Some(cell);
    }
    Ok(())
}

/// `first_value`／`last_value`／`nth_value` の 1 パーティション分の評価（SQL-64 (3)・Issue #1883。
/// [`evaluate_partition`] から呼ばれる）。行ごとにフレーム（`frame` が `None` なら既定の
/// `RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW`＝先頭から現在行の peer グループ末尾まで）を
/// 求め、`EXCLUDE` 対象を除いた先頭・末尾・n 番目の行の値（materialize 段が集めた
/// [`OffsetRowValues::value`]）を返す。空フレーム・序数超過は NULL。`nth_value` の `n` は現在行で
/// 評価した値で、NULL なら NULL、0 以下なら `22016`（行ごとに判定するため 0 行では出ない）。
/// 値の評価エラー（`22xxx`）は参照された行のときだけ返す。母集合は materialize 段を通った行のみで、
/// 不可視行・他テナントの Private 行は参照先にならない。
fn evaluate_value_frame_partition(
    part: &SortedPartition<'_>,
    frame: Option<&ResolvedFrame>,
    out: &mut [Option<Cell>],
    state_bytes: &mut usize,
    state_cap: usize,
) -> Result<(), SqlSurfaceError> {
    let item = part.item;
    let item_index = part.item_index;
    let len = part.len();
    let values_of = |row_idx: usize| -> Result<&OffsetRowValues, SqlSurfaceError> {
        part.materialized
            .get(row_idx)
            .and_then(|r| r.offset_values.get(item_index))
            .and_then(|v| v.as_ref())
            .ok_or_else(|| window_bug("value window values missing for a row"))
    };
    let non_null = match frame {
        Some(ResolvedFrame::Range(range)) if range.offsets().next().is_some() => {
            non_null_region(part)?
        }
        _ => (0, 0),
    };
    let mut head = GroupCursor::new(part)?;
    let mut tail = GroupCursor::new(part)?;

    for (c, group) in part.peer_groups().enumerate() {
        let (g_start, g_end) = group?;
        let (mut start, mut end, exclusion) = match frame {
            None => (0, g_end, WindowFrameExclusion::NoOthers),
            Some(ResolvedFrame::Rows(_)) | Some(ResolvedFrame::Range(_)) => {
                (0, 0, WindowFrameExclusion::NoOthers)
            }
            Some(ResolvedFrame::Groups(f)) => {
                let (s, e) =
                    groups_frame_bounds(part, c, (g_start, g_end), f, &mut head, &mut tail)?;
                (s, e, f.exclusion)
            }
        };
        for pos in g_start..g_end {
            crate::interrupt::check()?;
            let mut exclusion = exclusion;
            match frame {
                Some(ResolvedFrame::Rows(f)) => {
                    (start, end) = rows_frame_bounds(pos, len, f);
                    exclusion = f.exclusion;
                }
                Some(ResolvedFrame::Range(f)) => {
                    (start, end) = range_frame_bounds(part, non_null, pos, (g_start, g_end), f)?;
                    exclusion = f.exclusion;
                }
                _ => {}
            }
            let row_idx = part.row_index(pos)?;
            let current = values_of(row_idx)?;
            let pick = match item.func {
                WindowFunc::FirstValue => Some(FramePick::First),
                WindowFunc::LastValue => Some(FramePick::Last),
                WindowFunc::NthValue => match current.offset {
                    None => None,
                    Some(n) if n <= 0 => {
                        return Err(SqlSurfaceError::InvalidArgumentForNthValue {
                            detail: "argument of nth_value must be greater than zero",
                        });
                    }
                    Some(n) => {
                        let k = usize::try_from(n.saturating_sub(1)).unwrap_or(usize::MAX);
                        Some(FramePick::Nth(k))
                    }
                },
                _ => {
                    return Err(window_bug(
                        "non value-frame function reached the value frame",
                    ))
                }
            };
            let excluded: [(usize, usize); 2] = match exclusion {
                WindowFrameExclusion::NoOthers => [(0, 0), (0, 0)],
                WindowFrameExclusion::CurrentRow => [(pos, pos.saturating_add(1)), (0, 0)],
                WindowFrameExclusion::Group => [(g_start, g_end), (0, 0)],
                WindowFrameExclusion::Ties => [(g_start, pos), (pos.saturating_add(1), g_end)],
            };
            let target = pick.and_then(|p| pick_in_frame(start, end, &excluded, p));
            let cell = match target {
                None => Cell::Null,
                Some(target_pos) => {
                    let target_row = part.row_index(target_pos)?;
                    let cell = values_of(target_row)?
                        .value
                        .as_ref()
                        .map_err(Clone::clone)?;
                    // 複製の前に状態予算へ計上する（確保前の検証）。
                    *state_bytes = try_accumulate_state_budget(
                        *state_bytes,
                        window_cell_state_bytes(cell),
                        state_cap,
                    )?;
                    cell.clone()
                }
            };
            let slot = out.get_mut(row_idx).ok_or_else(|| {
                window_bug(
                    "evaluate_value_frame_partition row index out of range for the result slice",
                )
            })?;
            *slot = Some(cell);
        }
    }
    Ok(())
}

/// 1 パーティション分のウィンドウ値を計算する（並べ替え済み [`SortedPartition`] を
/// peer グループ単位で走査し、順位/集計を評価する。§モジュールドキュメント「評価の意味」参照）。
/// `evaluate_window_item` から呼ばれる。
fn evaluate_partition(
    part: &SortedPartition<'_>,
    frame: Option<&ResolvedFrame>,
    out: &mut [Option<Cell>],
    state_bytes: &mut usize,
    state_cap: usize,
) -> Result<(), SqlSurfaceError> {
    let item = part.item;
    let item_index = part.item_index;

    // `ntile` はフレームを無視し行の位置だけで進むため、peer 分割へ入れず専用の評価へ委譲する。
    if item.func == WindowFunc::Ntile {
        return evaluate_ntile_partition(part, out, state_bytes, state_cap);
    }
    // `LAG`／`LEAD` は peer・フレームに依存しないため、peer 分割へ入れず専用の評価へ委譲する。
    if item.func.is_frame_value() {
        return evaluate_value_frame_partition(part, frame, out, state_bytes, state_cap);
    }
    if item.func.is_value_offset() {
        // `RANGE` のオフセットの負・NaN は、非 NULL キーの行を持つパーティションで初めて
        // エラーになる（PostgreSQL の in_range 関数内の検査。NULL キーだけ・0 行では出ない）。
        if let Some(ResolvedFrame::Range(range)) = frame {
            check_range_offsets_for_partition(part, range)?;
        }
        return evaluate_offset_partition(part, out, state_bytes, state_cap);
    }
    // 解決済みのフレームを持つのは集計だけ。フレームなし（既定の peer 意味論）の経路は
    // 以下の peer ループのまま変えない。
    match frame {
        Some(ResolvedFrame::Rows(frame)) => {
            return evaluate_rows_frame_partition(part, frame, out, state_bytes, state_cap);
        }
        Some(ResolvedFrame::Groups(frame)) => {
            return evaluate_groups_frame_partition(part, frame, out, state_bytes, state_cap);
        }
        Some(ResolvedFrame::Range(frame)) => {
            return evaluate_range_frame_partition(part, frame, out, state_bytes, state_cap);
        }
        None => {}
    }

    let mut scratch: Vec<StackValue> = Vec::new();

    let mut acc: Option<Accumulator> = match &item.input {
        None => None,
        Some(input) => Some(Accumulator::new(
            window_func_to_aggregate_func(item.func)?,
            input,
        )?),
    };

    let part_len = part.len();
    for (dense_rank, group) in part.peer_groups().enumerate() {
        let (idx, peer_end) = group?;
        // 順位関数の `rank` は peer グループ先頭の 0 始まり位置（= 先行行数）。
        let rank = idx;

        if let (Some(acc_mut), Some(input)) = (acc.as_mut(), &item.input) {
            for pos in idx..peer_end {
                let row = part.row(pos)?;
                let agg_value = row
                    .agg_values
                    .get(item_index)
                    .ok_or_else(|| window_bug("aggregate window values missing for a row"))?;
                observe_window_row(acc_mut, input, row.id, agg_value, &mut scratch)?;
            }
        }

        // 集計関数（`Count`/`Sum`/`Avg`/`Min`/`Max`）の値は peer グループ内の
        // 全行で同一（`acc` は peer グループ単位でしか変化しない累積器のため）。
        // 以前は行ごとに `acc.clone().finish()` を呼んでいたが、peer グループに
        // つき 1 回だけ計算し `Cell::clone()` で配るよう変更（挙動不変・
        // レビュー指摘対応。PR #930 最終レビュー指摘 3）。順位関数
        // （`RowNumber`/`Rank`/`DenseRank`）は行ごとに値が変わるため対象外。
        let aggregate_cell: Option<Cell> = match item.func {
            WindowFunc::RowNumber
            | WindowFunc::Rank
            | WindowFunc::DenseRank
            | WindowFunc::PercentRank
            | WindowFunc::CumeDist => None,
            // `LAG`／`LEAD` は上で専用の評価へ委譲済みのため到達しない（不変条件違反は fail-closed）。
            WindowFunc::Lag
            | WindowFunc::Lead
            | WindowFunc::FirstValue
            | WindowFunc::LastValue
            | WindowFunc::NthValue
            | WindowFunc::Ntile => {
                return Err(window_bug("offset window function reached the peer loop"));
            }
            WindowFunc::Count
            | WindowFunc::Sum
            | WindowFunc::Avg
            | WindowFunc::Min
            | WindowFunc::Max => {
                let acc_ref = acc.clone().ok_or_else(|| {
                    window_bug("aggregate window function is missing its accumulator")
                })?;
                Some(acc_ref.finish()?)
            }
        };

        let row_number_base = idx;
        for (offset, pos) in (idx..peer_end).enumerate() {
            let row_idx = part.row_index(pos)?;
            let row_number = row_number_base + offset + 1;
            let cell = match item.func {
                WindowFunc::RowNumber => Cell::Integer(row_number as u64),
                WindowFunc::Rank => Cell::Integer((rank + 1) as u64),
                WindowFunc::DenseRank => Cell::Integer((dense_rank + 1) as u64),
                // 分布関数は peer グループ内で一定（SQL-64・Issue #1885）。
                WindowFunc::PercentRank => Cell::Float(percent_rank_value(rank, part_len)),
                WindowFunc::CumeDist => Cell::Float(cume_dist_value(peer_end, part_len)),
                WindowFunc::Lag
                | WindowFunc::Lead
                | WindowFunc::FirstValue
                | WindowFunc::LastValue
                | WindowFunc::NthValue
                | WindowFunc::Ntile => {
                    return Err(window_bug("offset window function reached the peer loop"));
                }
                WindowFunc::Count
                | WindowFunc::Sum
                | WindowFunc::Avg
                | WindowFunc::Min
                | WindowFunc::Max => aggregate_cell.clone().ok_or_else(|| {
                    window_bug("aggregate window function is missing its finished cell")
                })?,
            };
            // `MIN`/`MAX(<TEXT 列>)` は複製した `String` を保持しうるため
            // （`resolve_aggregate_input` は TEXT を MIN/MAX に受理する）、行ごとの
            // 複製値も materialize 段と同じ状態予算へ計上する（レビュー指摘:
            // 評価段の確保が `MAX_WINDOW_STATE_BYTES` から漏れていた）。
            *state_bytes = try_accumulate_state_budget(
                *state_bytes,
                window_cell_state_bytes(&cell),
                state_cap,
            )?;
            let slot = out.get_mut(row_idx).ok_or_else(|| {
                window_bug("evaluate_partition row index out of range for the result slice")
            })?;
            *slot = Some(cell);
        }
    }

    Ok(())
}

/// `percent_rank` の値（`(rank - 1) / (n - 1)`。`n == 1` は 0）。PostgreSQL の
/// `window_percent_rank` と同じく整数を先に `f64` へ変換してから除算する（SQL-64・Issue #1885）。
fn percent_rank_value(idx: usize, n: usize) -> f64 {
    if n > 1 {
        idx as f64 / (n - 1) as f64
    } else {
        0.0
    }
}

/// `cume_dist` の値（`peer グループ末尾までの行数 / n`。SQL-64・Issue #1885）。
fn cume_dist_value(peer_end: usize, n: usize) -> f64 {
    if n == 0 {
        return 0.0;
    }
    peer_end as f64 / n as f64
}

/// 集計の `ROWS` フレーム評価（SQL-64 (2)・Issue #1889。[`evaluate_partition`] から呼ばれる）。
/// 行ごとに [`rows_frame_bounds`] のフレームから `EXCLUDE` 対象を除いた行を前方順に観測する
/// （PostgreSQL は逆遷移を持たない集計〔float・MIN/MAX 等〕で同じく再集計するため、前方加算の
/// 順序まで一致する。整数・NUMERIC は逆遷移でも正確で同値）。始点が `UNBOUNDED PRECEDING` で
/// `EXCLUDE` なしなら終点は単調非減少なので、1 つの累積器へ新規行だけを足す O(n) の経路を取る。
/// 空フレームは累積器の空集合契約（`COUNT`＝0、他は NULL）で返る。母集合は materialize 段を通った
/// 行のみで、不可視行はフレームに現れない。
fn evaluate_rows_frame_partition(
    part: &SortedPartition<'_>,
    frame: &ResolvedRowsFrame,
    out: &mut [Option<Cell>],
    state_bytes: &mut usize,
    state_cap: usize,
) -> Result<(), SqlSurfaceError> {
    let item = part.item;
    let item_index = part.item_index;
    let input = item
        .input
        .as_ref()
        .ok_or_else(|| window_bug("framed window function is missing its aggregate input"))?;
    let agg_func = window_func_to_aggregate_func(item.func)?;
    let len = part.len();
    let mut scratch: Vec<StackValue> = Vec::new();

    let observe_at = |acc: &mut Accumulator,
                      pos: usize,
                      scratch: &mut Vec<StackValue>|
     -> Result<(), SqlSurfaceError> {
        let row = part.row(pos)?;
        let agg_value = row
            .agg_values
            .get(item_index)
            .ok_or_else(|| window_bug("aggregate window values missing for a row"))?;
        observe_window_row(acc, input, row.id, agg_value, scratch)
    };

    let cumulative = frame.start == ResolvedBound::UnboundedPreceding
        && frame.exclusion == WindowFrameExclusion::NoOthers;
    let mut running = Accumulator::new(agg_func, input)?;
    let mut observed = 0usize;

    for group in part.peer_groups() {
        let (g_start, g_end) = group?;
        for pos in g_start..g_end {
            crate::interrupt::check()?;
            let (start, end) = rows_frame_bounds(pos, len, frame);
            let cell = if cumulative {
                while observed < end {
                    observe_at(&mut running, observed, &mut scratch)?;
                    observed += 1;
                }
                running.clone().finish()?
            } else {
                let mut acc = Accumulator::new(agg_func, input)?;
                for q in start..end {
                    let excluded = match frame.exclusion {
                        WindowFrameExclusion::NoOthers => false,
                        WindowFrameExclusion::CurrentRow => q == pos,
                        WindowFrameExclusion::Group => (g_start..g_end).contains(&q),
                        WindowFrameExclusion::Ties => q != pos && (g_start..g_end).contains(&q),
                    };
                    if !excluded {
                        crate::interrupt::check()?;
                        observe_at(&mut acc, q, &mut scratch)?;
                    }
                }
                acc.finish()?
            };
            // 行ごとの複製も他の評価段の確保と同じ状態予算へ計上する（確保前の検証）。
            *state_bytes = try_accumulate_state_budget(
                *state_bytes,
                window_cell_state_bytes(&cell),
                state_cap,
            )?;
            let row_idx = part.row_index(pos)?;
            let slot = out.get_mut(row_idx).ok_or_else(|| {
                window_bug(
                    "evaluate_rows_frame_partition row index out of range for the result slice",
                )
            })?;
            *slot = Some(cell);
        }
    }
    Ok(())
}

/// peer グループ上の前進のみのカーソル（`GROUPS` の境界計算用。Issue #1891）。
/// `idx` 番目のグループの `[start, end)` を指し、`start == len` は最終グループの後ろ。
/// 現在行のグループ番号は単調非減少なので、目標グループも単調となりカーソルは後退しない。
#[derive(Debug, Clone, Copy)]
struct GroupCursor {
    idx: usize,
    start: usize,
    end: usize,
}

impl GroupCursor {
    /// 先頭グループを指すカーソル（0 行なら `len` の位置）。
    fn new(part: &SortedPartition<'_>) -> Result<Self, SqlSurfaceError> {
        let end = if part.len() == 0 {
            0
        } else {
            part.peer_group_end(0)?
        };
        Ok(Self {
            idx: 0,
            start: 0,
            end,
        })
    }

    /// グループ番号 `target` まで進める（最終グループを越える目標は `start == len` で止まる）。
    fn advance_to(
        &mut self,
        part: &SortedPartition<'_>,
        target: usize,
    ) -> Result<(), SqlSurfaceError> {
        let len = part.len();
        while self.idx < target && self.start < len {
            self.start = self.end;
            self.idx += 1;
            self.end = if self.start < len {
                part.peer_group_end(self.start)?
            } else {
                len
            };
        }
        Ok(())
    }
}

/// 現在行のグループ番号 `c`（範囲 `current`）の `GROUPS` フレームを半開区間 `[start, end)` で返す
/// （`start >= end` は空フレーム）。`head`／`tail` は始点用・終点用のカーソルで、`c` を
/// 昇順に渡す限り後退しない。オフセットの加減算はすべて飽和演算で、最後に `len` へクランプする。
fn groups_frame_bounds(
    part: &SortedPartition<'_>,
    c: usize,
    current: (usize, usize),
    frame: &ResolvedRowsFrame,
    head: &mut GroupCursor,
    tail: &mut GroupCursor,
) -> Result<(usize, usize), SqlSurfaceError> {
    let len = part.len();
    let start = match frame.start {
        ResolvedBound::UnboundedPreceding => 0,
        ResolvedBound::Preceding(n) => {
            head.advance_to(part, c.saturating_sub(n))?;
            head.start
        }
        ResolvedBound::CurrentRow => current.0,
        ResolvedBound::Following(n) => {
            head.advance_to(part, c.saturating_add(n))?;
            head.start
        }
        ResolvedBound::UnboundedFollowing => len,
    };
    let end = match frame.end {
        ResolvedBound::UnboundedPreceding => 0,
        ResolvedBound::Preceding(n) => {
            if n > c {
                0
            } else {
                tail.advance_to(part, c - n)?;
                tail.end
            }
        }
        ResolvedBound::CurrentRow => current.1,
        ResolvedBound::Following(n) => {
            tail.advance_to(part, c.saturating_add(n))?;
            tail.end
        }
        ResolvedBound::UnboundedFollowing => len,
    };
    Ok((start.min(len), end.min(len)))
}

/// 集計の `GROUPS` フレーム評価（SQL-64 (2)・Issue #1891。[`evaluate_partition`] から呼ばれる）。
/// 骨格は [`evaluate_rows_frame_partition`] と同じで、境界だけを [`groups_frame_bounds`] で求める。
/// 始点が `UNBOUNDED PRECEDING` かつ `EXCLUDE` なしなら終点は単調非減少なので累積器の O(n) 経路を取る。
/// 母集合は materialize 段を通った行のみで、不可視行・他テナントの Private 行は
/// グループ数にもフレームにも現れない。
fn evaluate_groups_frame_partition(
    part: &SortedPartition<'_>,
    frame: &ResolvedRowsFrame,
    out: &mut [Option<Cell>],
    state_bytes: &mut usize,
    state_cap: usize,
) -> Result<(), SqlSurfaceError> {
    let item = part.item;
    let item_index = part.item_index;
    let input = item
        .input
        .as_ref()
        .ok_or_else(|| window_bug("framed window function is missing its aggregate input"))?;
    let agg_func = window_func_to_aggregate_func(item.func)?;
    let mut scratch: Vec<StackValue> = Vec::new();

    let observe_at = |acc: &mut Accumulator,
                      pos: usize,
                      scratch: &mut Vec<StackValue>|
     -> Result<(), SqlSurfaceError> {
        let row = part.row(pos)?;
        let agg_value = row
            .agg_values
            .get(item_index)
            .ok_or_else(|| window_bug("aggregate window values missing for a row"))?;
        observe_window_row(acc, input, row.id, agg_value, scratch)
    };

    let cumulative = frame.start == ResolvedBound::UnboundedPreceding
        && frame.exclusion == WindowFrameExclusion::NoOthers;
    let mut running = Accumulator::new(agg_func, input)?;
    let mut observed = 0usize;
    let mut head = GroupCursor::new(part)?;
    let mut tail = GroupCursor::new(part)?;

    for (c, group) in part.peer_groups().enumerate() {
        let (g_start, g_end) = group?;
        let (start, end) =
            groups_frame_bounds(part, c, (g_start, g_end), frame, &mut head, &mut tail)?;
        for pos in g_start..g_end {
            crate::interrupt::check()?;
            let cell = if cumulative && end >= observed {
                while observed < end {
                    observe_at(&mut running, observed, &mut scratch)?;
                    observed += 1;
                }
                running.clone().finish()?
            } else {
                let mut acc = Accumulator::new(agg_func, input)?;
                for q in start..end {
                    let excluded = match frame.exclusion {
                        WindowFrameExclusion::NoOthers => false,
                        WindowFrameExclusion::CurrentRow => q == pos,
                        WindowFrameExclusion::Group => (g_start..g_end).contains(&q),
                        WindowFrameExclusion::Ties => q != pos && (g_start..g_end).contains(&q),
                    };
                    if !excluded {
                        crate::interrupt::check()?;
                        observe_at(&mut acc, q, &mut scratch)?;
                    }
                }
                acc.finish()?
            };
            // 行ごとの複製も他の評価段の確保と同じ状態予算へ計上する（確保前の検証）。
            *state_bytes = try_accumulate_state_budget(
                *state_bytes,
                window_cell_state_bytes(&cell),
                state_cap,
            )?;
            let row_idx = part.row_index(pos)?;
            let slot = out.get_mut(row_idx).ok_or_else(|| {
                window_bug(
                    "evaluate_groups_frame_partition row index out of range for the result slice",
                )
            })?;
            *slot = Some(cell);
        }
    }
    Ok(())
}
/// 集計の `RANGE` フレーム評価（SQL-64 (2)・Issue #1890。[`evaluate_partition`] から呼ばれる）。
/// 行ごとに [`range_frame_bounds`] のフレームから `EXCLUDE` 対象（現在行・peer グループ）を除いた行を
/// 前方順に観測する。`ROWS` の評価（[`evaluate_rows_frame_partition`]）と同じ形で、始点が
/// `UNBOUNDED PRECEDING` かつ `EXCLUDE` なしで終点が単調非減少のときは 1 つの累積器へ新規行だけを足す
/// O(n) の経路を取る（PostgreSQL の前方加算の順序とも一致する）。母集合は materialize 段を通った
/// 行のみで、不可視行・他テナントの Private 行は境界計算にも現れない。
fn evaluate_range_frame_partition(
    part: &SortedPartition<'_>,
    frame: &ResolvedRangeFrame,
    out: &mut [Option<Cell>],
    state_bytes: &mut usize,
    state_cap: usize,
) -> Result<(), SqlSurfaceError> {
    let item = part.item;
    let item_index = part.item_index;
    let input = item
        .input
        .as_ref()
        .ok_or_else(|| window_bug("framed window function is missing its aggregate input"))?;
    let agg_func = window_func_to_aggregate_func(item.func)?;
    let mut scratch: Vec<StackValue> = Vec::new();
    let non_null = if frame.offsets().next().is_some() {
        non_null_region(part)?
    } else {
        (0, 0)
    };

    let observe_at = |acc: &mut Accumulator,
                      pos: usize,
                      scratch: &mut Vec<StackValue>|
     -> Result<(), SqlSurfaceError> {
        let row = part.row(pos)?;
        let agg_value = row
            .agg_values
            .get(item_index)
            .ok_or_else(|| window_bug("aggregate window values missing for a row"))?;
        observe_window_row(acc, input, row.id, agg_value, scratch)
    };

    let cumulative = matches!(frame.start, ResolvedRangeBound::UnboundedPreceding)
        && frame.exclusion == WindowFrameExclusion::NoOthers;
    let mut running = Accumulator::new(agg_func, input)?;
    let mut observed = 0usize;

    for group in part.peer_groups() {
        let (g_start, g_end) = group?;
        for pos in g_start..g_end {
            crate::interrupt::check()?;
            let (start, end) = range_frame_bounds(part, non_null, pos, (g_start, g_end), frame)?;
            let cell = if cumulative && end >= observed {
                while observed < end {
                    observe_at(&mut running, observed, &mut scratch)?;
                    observed += 1;
                }
                running.clone().finish()?
            } else {
                let mut acc = Accumulator::new(agg_func, input)?;
                for q in start..end {
                    let excluded = match frame.exclusion {
                        WindowFrameExclusion::NoOthers => false,
                        WindowFrameExclusion::CurrentRow => q == pos,
                        WindowFrameExclusion::Group => (g_start..g_end).contains(&q),
                        WindowFrameExclusion::Ties => q != pos && (g_start..g_end).contains(&q),
                    };
                    if !excluded {
                        crate::interrupt::check()?;
                        observe_at(&mut acc, q, &mut scratch)?;
                    }
                }
                acc.finish()?
            };
            // 行ごとの複製も他の評価段の確保と同じ状態予算へ計上する（確保前の検証）。
            *state_bytes = try_accumulate_state_budget(
                *state_bytes,
                window_cell_state_bytes(&cell),
                state_cap,
            )?;
            let row_idx = part.row_index(pos)?;
            let slot = out.get_mut(row_idx).ok_or_else(|| {
                window_bug(
                    "evaluate_range_frame_partition row index out of range for the result slice",
                )
            })?;
            *slot = Some(cell);
        }
    }
    Ok(())
}

/// 位置 `pos` の `ORDER BY` 第 1 キー（`None` は NULL）。キーが無いのは不変条件違反（`XX000`）。
fn first_order_key<'a>(
    part: &SortedPartition<'a>,
    pos: usize,
) -> Result<Option<&'a WindowKeyValue>, SqlSurfaceError> {
    let row = part.row(pos)?;
    row.order_keys
        .get(part.item_index)
        .and_then(|keys| keys.first())
        .map(|k| k.as_ref())
        .ok_or_else(|| window_bug("RANGE frame ORDER BY key missing for a row"))
}

/// `[lo, hi)` で述語が「偽…偽・真…真」と単調に変わるとき、最初に真になる位置（無ければ `hi`）。
fn first_true(
    lo: usize,
    hi: usize,
    mut pred: impl FnMut(usize) -> Result<bool, SqlSurfaceError>,
) -> Result<usize, SqlSurfaceError> {
    let (mut lo, mut hi) = (lo, hi);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if pred(mid)? {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    Ok(lo)
}

/// パーティション内で `ORDER BY` 第 1 キーが非 NULL の区間 `[start, end)`。NULL は並びの先頭
/// （`NULLS FIRST`）か末尾（`NULLS LAST`）に連続するため、二分探索で求める。
fn non_null_region(part: &SortedPartition<'_>) -> Result<(usize, usize), SqlSurfaceError> {
    let len = part.len();
    let nulls_first = part
        .item
        .order_by
        .first()
        .is_some_and(|o| o.nulls == NullPlacement::First);
    if nulls_first {
        let start = first_true(0, len, |p| Ok(first_order_key(part, p)?.is_some()))?;
        Ok((start, len))
    } else {
        let end = first_true(0, len, |p| Ok(first_order_key(part, p)?.is_none()))?;
        Ok((0, end))
    }
}

/// `RANGE` オフセットが負（`NaN` を含む）か。`TIME` キーは PostgreSQL の `in_range_time_interval`
/// に合わせ `INTERVAL` の時間成分だけで判定し（月・日は無視）、他は正規化キー（月 = 30 日）で判定する。
fn range_offset_is_negative(off: &RangeOffset, key: &WindowKeyValue) -> bool {
    match off {
        RangeOffset::Int(n) => *n < 0,
        RangeOffset::Float(f) => f.is_nan() || *f < 0.0,
        RangeOffset::Numeric(d) => d.unscaled() < 0,
        RangeOffset::Interval(iv) => match key {
            WindowKeyValue::Time(_) => iv.micros < 0,
            _ => crate::datetime::interval_cmp_key(iv) < 0,
        },
    }
}

fn invalid_range_offset() -> SqlSurfaceError {
    SqlSurfaceError::InvalidPrecedingOrFollowingSize {
        detail: "invalid preceding or following size in window function",
    }
}

/// `LAG`／`LEAD` が `RANGE` のオフセット付きフレームを持つとき、非 NULL キーの行があるパーティション
/// でだけ負・NaN のオフセットを `22013` にする（PostgreSQL と同じ遅延検査）。NULL は連続して
/// 先頭か末尾に並ぶため、両端の行を見れば非 NULL 行の有無が分かる。
fn check_range_offsets_for_partition(
    part: &SortedPartition<'_>,
    frame: &ResolvedRangeFrame,
) -> Result<(), SqlSurfaceError> {
    let len = part.len();
    if len == 0 {
        return Ok(());
    }
    let key = match first_order_key(part, 0)? {
        Some(k) => Some(k),
        None => first_order_key(part, len - 1)?,
    };
    if let Some(key) = key {
        if frame.offsets().any(|o| range_offset_is_negative(o, key)) {
            return Err(invalid_range_offset());
        }
    }
    Ok(())
}

/// 境界の比較対象（`基準値 ± オフセット`）。現在行ごと・境界ごとに 1 回だけ計算する
/// （[`range_limit`]）。
enum RangeLimit {
    /// 整数キー（`i128` で正確に比較する。PostgreSQL の桁あふれ時の分岐と結果が一致する）。
    Int(i128),
    /// 浮動小数点キー。`base` は NaN の分岐用、`skip` は基準値・オフセットがともに無限大で
    /// 加減算が NaN になる組（PostgreSQL は常に範囲内とする）。
    Float {
        base: f64,
        sum: f64,
        skip: bool,
    },
    Numeric(crate::numeric::Decimal),
    /// 日付・日時・時刻キーのマイクロ秒（`DATE` は日付を日時へ昇格した値）。
    Micros(i128),
}

fn key_as_i128(v: &WindowKeyValue) -> Option<i128> {
    match v {
        WindowKeyValue::Id(x) => Some(i128::from(*x)),
        WindowKeyValue::Integer(x) => Some(i128::from(*x)),
        WindowKeyValue::BigInt(x) => Some(i128::from(*x)),
        _ => None,
    }
}

fn key_as_f64(v: &WindowKeyValue) -> Option<f64> {
    match v {
        WindowKeyValue::Real(x) => Some(f64::from(*x)),
        WindowKeyValue::Double(x) => Some(*x),
        _ => None,
    }
}

fn key_as_micros(v: &WindowKeyValue) -> Option<i128> {
    match v {
        WindowKeyValue::Date(d) => {
            Some(i128::from(*d) * i128::from(crate::datetime::MICROS_PER_DAY))
        }
        WindowKeyValue::Timestamp(t) | WindowKeyValue::TimestampTz(t) | WindowKeyValue::Time(t) => {
            Some(i128::from(*t))
        }
        _ => None,
    }
}

/// `base ± offset`（`sub` なら減算）を比較対象として計算する。PostgreSQL 17 の
/// `in_range_*`（整数・float・numeric・date/timestamp/timestamptz/time ± interval）の移植。
/// 日時は月 → 日 → マイクロ秒の順に加算し、最終値の 0001〜9999 年検査は行わない（PG の timestamp
/// 範囲外だけ `22008`）。`TIMESTAMPTZ` の月・日はセッション TZ の壁時計で加算する。
/// `TIME` は `INTERVAL` の時間成分だけを日またぎの折り返しなしで加減算する。
fn range_limit(
    base: &WindowKeyValue,
    off: &RangeOffset,
    sub: bool,
) -> Result<RangeLimit, SqlSurfaceError> {
    use crate::sql::interval_arith;
    match off {
        RangeOffset::Int(n) => {
            let b = key_as_i128(base)
                .ok_or_else(|| window_bug("RANGE integer offset on a non-integer key"))?;
            let n = i128::from(*n);
            Ok(RangeLimit::Int(if sub { b - n } else { b + n }))
        }
        RangeOffset::Float(o) => {
            let b = key_as_f64(base)
                .ok_or_else(|| window_bug("RANGE float offset on a non-float key"))?;
            let skip = o.is_infinite() && b.is_infinite() && (if sub { b > 0.0 } else { b < 0.0 });
            let sum = if sub { b - o } else { b + o };
            Ok(RangeLimit::Float { base: b, sum, skip })
        }
        RangeOffset::Numeric(o) => {
            let WindowKeyValue::Numeric(b) = base else {
                return Err(window_bug("RANGE numeric offset on a non-numeric key"));
            };
            let op = if sub {
                crate::numeric::DecimalOp::Sub
            } else {
                crate::numeric::DecimalOp::Add
            };
            crate::numeric::decimal_arith(op, b, o)
                .map(RangeLimit::Numeric)
                .map_err(|_| SqlSurfaceError::numeric_out_of_range("value out of range"))
        }
        RangeOffset::Interval(iv) => {
            let iv = if sub {
                interval_arith::negate_interval(iv)?
            } else {
                *iv
            };
            match base {
                WindowKeyValue::Date(_) | WindowKeyValue::Timestamp(_) => {
                    let micros = key_as_micros(base)
                        .and_then(|m| i64::try_from(m).ok())
                        .ok_or_else(|| window_bug("RANGE date key out of range"))?;
                    interval_arith::timestamp_shift_wide(micros, &iv).map(RangeLimit::Micros)
                }
                WindowKeyValue::TimestampTz(t) => {
                    crate::datetime::with_current_session_time_zone(|tz| {
                        interval_arith::timestamptz_shift_wide(*t, &iv, tz)
                    })
                    .map(RangeLimit::Micros)
                }
                WindowKeyValue::Time(t) => {
                    // `negate_interval` 済みの時間成分を足す（日またぎで折り返さない）。
                    Ok(RangeLimit::Micros(i128::from(*t) + i128::from(iv.micros)))
                }
                _ => Err(window_bug("RANGE interval offset on a non-datetime key")),
            }
        }
    }
}

/// `val` が比較対象 `limit` に対して範囲内か（`less` なら `val <= limit`、でなければ `val >= limit`）。
/// 浮動小数点は PostgreSQL の規則（NaN は最大値として NaN 同士は等しい）に従う。
fn in_range_limit(
    val: &WindowKeyValue,
    limit: &RangeLimit,
    less: bool,
) -> Result<bool, SqlSurfaceError> {
    use std::cmp::Ordering;
    let ord_ok = |o: Ordering| {
        if less {
            o != Ordering::Greater
        } else {
            o != Ordering::Less
        }
    };
    match limit {
        RangeLimit::Int(l) => {
            let v = key_as_i128(val).ok_or_else(|| window_bug("RANGE integer key expected"))?;
            Ok(ord_ok(v.cmp(l)))
        }
        RangeLimit::Float { base, sum, skip } => {
            let v = key_as_f64(val).ok_or_else(|| window_bug("RANGE float key expected"))?;
            Ok(if v.is_nan() {
                if base.is_nan() {
                    true
                } else {
                    !less
                }
            } else if base.is_nan() {
                less
            } else if *skip {
                true
            } else if less {
                v <= *sum
            } else {
                v >= *sum
            })
        }
        RangeLimit::Numeric(l) => {
            let WindowKeyValue::Numeric(v) = val else {
                return Err(window_bug("RANGE numeric key expected"));
            };
            Ok(ord_ok(crate::numeric::cmp_exact(v, l)))
        }
        RangeLimit::Micros(l) => {
            let v = key_as_micros(val).ok_or_else(|| window_bug("RANGE datetime key expected"))?;
            Ok(ord_ok(v.cmp(l)))
        }
    }
}

/// 値オフセット付き境界 1 つの位置を、非 NULL 区間 `non_null` 上の二分探索で求める。
/// 始点は「境界以上（`DESC` は以下）の最初の行」、終点は「境界以下（`DESC` は以上）を満たす
/// 最後の行の次」。`DESC` は PostgreSQL と同じく加減と大小の向きをともに反転する。
fn offset_bound_position(
    part: &SortedPartition<'_>,
    non_null: (usize, usize),
    cur: &WindowKeyValue,
    off: &RangeOffset,
    preceding: bool,
    is_start: bool,
) -> Result<usize, SqlSurfaceError> {
    let desc = part.item.order_by.first().is_some_and(|o| o.descending);
    let sub = preceding != desc;
    let limit = range_limit(cur, off, sub)?;
    let less = if is_start { desc } else { !desc };
    let in_range_at = |p: usize| -> Result<bool, SqlSurfaceError> {
        let key = first_order_key(part, p)?
            .ok_or_else(|| window_bug("NULL key inside the non-NULL region"))?;
        in_range_limit(key, &limit, less)
    };
    let (lo, hi) = non_null;
    if is_start {
        first_true(lo, hi, in_range_at)
    } else {
        first_true(lo, hi, |p| Ok(!in_range_at(p)?))
    }
}

/// 位置 `pos`（peer グループ `peers` = `[g_start, g_end)`）の `RANGE` フレームを半開区間
/// `[start, end)` で返す（`start >= end` は空フレーム。Issue #1890）。`UNBOUNDED`／`CURRENT ROW`
/// （peer グループ単位）は位置だけで決まり、`ORDER BY` なし・複数キーでも使える。値オフセットは
/// 現在行のキーが NULL なら NULL の peer グループ、非 NULL なら非 NULL 区間上の二分探索で決まる
/// （NULL 行はオフセット境界を満たさない）。負・NaN のオフセットは、非 NULL キーの現在行で
/// 境界を求める時点で初めて `22013`（PostgreSQL の in_range 内の検査と同じ時点）。
fn range_frame_bounds(
    part: &SortedPartition<'_>,
    non_null: (usize, usize),
    pos: usize,
    peers: (usize, usize),
    frame: &ResolvedRangeFrame,
) -> Result<(usize, usize), SqlSurfaceError> {
    let (g_start, g_end) = peers;
    let len = part.len();
    let cur = if frame.offsets().next().is_some() {
        first_order_key(part, pos)?
    } else {
        None
    };
    if let Some(cur) = cur {
        if frame.offsets().any(|o| range_offset_is_negative(o, cur)) {
            return Err(invalid_range_offset());
        }
    }
    let position = |b: &ResolvedRangeBound, is_start: bool| -> Result<usize, SqlSurfaceError> {
        match b {
            ResolvedRangeBound::UnboundedPreceding => Ok(0),
            ResolvedRangeBound::UnboundedFollowing => Ok(len),
            ResolvedRangeBound::CurrentRow => Ok(if is_start { g_start } else { g_end }),
            ResolvedRangeBound::Preceding(off) | ResolvedRangeBound::Following(off) => match cur {
                None => Ok(if is_start { g_start } else { g_end }),
                Some(cur) => offset_bound_position(
                    part,
                    non_null,
                    cur,
                    off,
                    matches!(b, ResolvedRangeBound::Preceding(_)),
                    is_start,
                ),
            },
        }
    };
    let start = position(&frame.start, true)?;
    let end = position(&frame.end, false)?;
    Ok((start.min(len), end.min(len)))
}

/// `ORDER BY` キーだけの比較（タイブレークなし）。`Equal` が peer（同順位）の定義で、
/// 並べ替え（[`cmp_partition_rows`]）と peer 判定（[`SortedPartition::are_peers`]）が
/// 共有する唯一の比較器。RANGE フレームのオフセット比較（Issue #1890）の土台でもある。
/// キーを取り出せないのは不変条件違反だが、`sort_by` のクロージャはエラーを返せないため
/// `Equal` へ倒す（`compute_output_order` と同じ流儀）。
fn cmp_order_keys(
    item: &BoundWindowItem,
    item_index: usize,
    a: &MaterializedRow,
    b: &MaterializedRow,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    for (key_idx, ordering) in item.order_by.iter().enumerate() {
        let ka = a.order_keys.get(item_index).and_then(|k| k.get(key_idx));
        let kb = b.order_keys.get(item_index).and_then(|k| k.get(key_idx));
        let (Some(ka), Some(kb)) = (ka, kb) else {
            return Ordering::Equal;
        };
        let ord = cmp_window_key(ka, kb, ordering.descending, ordering.nulls);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// パーティション内の並べ替え順。比較順は ORDER BY キー → `id` 昇順 → `seq` 昇順
/// （走査順のタイブレーク）。`sort_by`〔安定〕のみを使い `sort_unstable*` は使わない
/// ——`scripts/check_sort_determinism.sh` が CI で検知する。
fn cmp_partition_rows(
    item: &BoundWindowItem,
    item_index: usize,
    a: &MaterializedRow,
    b: &MaterializedRow,
) -> std::cmp::Ordering {
    cmp_order_keys(item, item_index, a, b)
        .then(a.id.cmp(&b.id))
        .then(a.seq.cmp(&b.seq))
}

/// 並べ替え済みの 1 パーティション（Issue #1888。SQL-64 ポインタ）。
///
/// `evaluate_window_item` がパーティション分割後に作り、`evaluate_partition`／
/// `evaluate_offset_partition` が「位置 → 行」の引き当てと peer 範囲の取得に使う。
/// フレーム評価（ROWS／RANGE／GROUPS。Issue #1889〜#1891）は位置アクセス
/// （`row`／`row_index`／`len`）と peer 範囲（`peer_groups`／`peer_group_end`／`are_peers`）を
/// 土台にする。状態予算を変えないため、peer の境界は確保せず都度走査で求める
/// （`indices` は `evaluate_window_item` が計上済みの Vec をそのまま保持する）。
struct SortedPartition<'a> {
    item: &'a BoundWindowItem,
    item_index: usize,
    materialized: &'a [MaterializedRow],
    /// 並べ替え済み（位置 → `materialized` の添字）。
    indices: Vec<usize>,
}

impl<'a> SortedPartition<'a> {
    /// `indices` を [`cmp_partition_rows`] で安定ソートして保持する。
    fn new(
        item: &'a BoundWindowItem,
        item_index: usize,
        materialized: &'a [MaterializedRow],
        mut indices: Vec<usize>,
    ) -> Self {
        indices.sort_by(|&a, &b| match (materialized.get(a), materialized.get(b)) {
            (Some(ra), Some(rb)) => cmp_partition_rows(item, item_index, ra, rb),
            _ => std::cmp::Ordering::Equal,
        });
        Self {
            item,
            item_index,
            materialized,
            indices,
        }
    }

    fn len(&self) -> usize {
        self.indices.len()
    }

    fn row_indices(&self) -> &[usize] {
        &self.indices
    }

    /// 位置 `pos` の行の `materialized` 添字（範囲外は不変条件違反として `XX000`）。
    fn row_index(&self, pos: usize) -> Result<usize, SqlSurfaceError> {
        self.indices
            .get(pos)
            .copied()
            .ok_or_else(|| window_bug("partition position out of range"))
    }

    /// 位置 `pos` の行（範囲外は不変条件違反として `XX000`）。
    fn row(&self, pos: usize) -> Result<&'a MaterializedRow, SqlSurfaceError> {
        let idx = self.row_index(pos)?;
        self.materialized
            .get(idx)
            .ok_or_else(|| window_bug("partition row index out of range"))
    }

    fn has_order_by(&self) -> bool {
        !self.item.order_by.is_empty()
    }

    /// 位置 `p` と `q` が peer か。ORDER BY なしならパーティション全体が 1 グループ。
    fn are_peers(&self, p: usize, q: usize) -> Result<bool, SqlSurfaceError> {
        if !self.has_order_by() {
            return Ok(true);
        }
        let (rp, rq) = (self.row(p)?, self.row(q)?);
        Ok(cmp_order_keys(self.item, self.item_index, rp, rq) == std::cmp::Ordering::Equal)
    }

    /// `start` から始まる peer グループの終わり（排他的）。
    fn peer_group_end(&self, start: usize) -> Result<usize, SqlSurfaceError> {
        if !self.has_order_by() {
            return Ok(self.len());
        }
        let mut end = start.saturating_add(1);
        while end < self.len() && self.are_peers(start, end)? {
            end += 1;
        }
        Ok(end)
    }

    /// peer グループの `(start, end)`（end は排他的）を位置順に返す。
    fn peer_groups(&self) -> PeerGroups<'_, 'a> {
        PeerGroups {
            part: self,
            next: 0,
        }
    }
}

/// [`SortedPartition::peer_groups`] のイテレータ。
struct PeerGroups<'p, 'a> {
    part: &'p SortedPartition<'a>,
    next: usize,
}

impl Iterator for PeerGroups<'_, '_> {
    type Item = Result<(usize, usize), SqlSurfaceError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next >= self.part.len() {
            return None;
        }
        let start = self.next;
        match self.part.peer_group_end(start) {
            Ok(end) => {
                self.next = end;
                Some(Ok((start, end)))
            }
            Err(e) => {
                // エラー後は打ち切る。
                self.next = self.part.len();
                Some(Err(e))
            }
        }
    }
}

/// ウィンドウ項目の結果列の静的型を返す（Issue #1344。WIRE-13・SQL-30 のポインタ）。
///
/// 実行経路（[`build_result`]）と Describe（`sql::describe::scan_columns`）が共有する
/// 唯一の写像で、wire-server の RowDescription 型 OID の元になる。順位関数と
/// `COUNT` は `BigInt`、`SUM`／`AVG`／`MIN`／`MAX` は `Accumulator::finish` が返す
/// セル型と対応する集計 SELECT の規則（`aggregate::aggregate_result_type`）を流用する。
/// 判定不能な場合は `None`（wire では text 公告へ倒れる fail-closed 側）。
pub(crate) fn window_result_type(item: &BoundWindowItem) -> Option<ColumnType> {
    use crate::sql::allowlist::AggregateFunc;
    let func = match item.func {
        WindowFunc::RowNumber | WindowFunc::Rank | WindowFunc::DenseRank | WindowFunc::Count => {
            return Some(ColumnType::BigInt);
        }
        // `ntile` は PostgreSQL と同じ `integer`（SQL-64 (4)・Issue #1884）。
        WindowFunc::Ntile => return Some(ColumnType::Integer),
        // `percent_rank`／`cume_dist` は PostgreSQL と同じ `double precision`（SQL-64・Issue #1885）。
        WindowFunc::PercentRank | WindowFunc::CumeDist => return Some(ColumnType::Double),
        WindowFunc::Sum => AggregateFunc::Sum,
        WindowFunc::Avg => AggregateFunc::Avg,
        WindowFunc::Min => AggregateFunc::Min,
        WindowFunc::Max => AggregateFunc::Max,
        // `LAG`／`LEAD` の結果型は値（既定値があれば統一後）の型（SQL-64 (1)・Issue #1880）。
        WindowFunc::Lag
        | WindowFunc::Lead
        | WindowFunc::FirstValue
        | WindowFunc::LastValue
        | WindowFunc::NthValue => {
            return item.offset_args.as_ref()?.result_type.clone();
        }
    };
    aggregate::aggregate_result_type(func, item.input.as_ref()?)
}

fn window_func_to_aggregate_func(
    func: WindowFunc,
) -> Result<crate::sql::allowlist::AggregateFunc, SqlSurfaceError> {
    use crate::sql::allowlist::AggregateFunc;
    match func {
        WindowFunc::Count => Ok(AggregateFunc::Count),
        WindowFunc::Sum => Ok(AggregateFunc::Sum),
        WindowFunc::Avg => Ok(AggregateFunc::Avg),
        WindowFunc::Min => Ok(AggregateFunc::Min),
        WindowFunc::Max => Ok(AggregateFunc::Max),
        WindowFunc::RowNumber
        | WindowFunc::Rank
        | WindowFunc::DenseRank
        | WindowFunc::PercentRank
        | WindowFunc::CumeDist => Err(window_bug(
            "ranking window function unexpectedly requires an accumulator",
        )),
        WindowFunc::Lag
        | WindowFunc::Lead
        | WindowFunc::FirstValue
        | WindowFunc::LastValue
        | WindowFunc::NthValue
        | WindowFunc::Ntile => Err(window_bug(
            "offset window function unexpectedly requires an accumulator",
        )),
    }
}

/// `index + 1` の長さで `index` にのみ値を持つ `scanned` を組み立てる
/// （[`Accumulator::observe`] が `scanned.get(index)` でしか参照しないため、
/// 他の添字はすべて `None` でよい）。
fn build_scanned(
    index: usize,
    value: Option<row_codec::ScalarRef<'_>>,
) -> Vec<Option<row_codec::ScalarRef<'_>>> {
    let mut out = vec![None; index + 1];
    if let Some(slot) = out.get_mut(index) {
        *slot = value;
    }
    out
}

/// [`WindowInputValue`]（materialize 段で owned 化した値）を、
/// [`Accumulator::observe`] が期待する借用形（`scanned: &[Option<ScalarRef>]`・
/// `vector: &RowVector`）へその場で組み立てて 1 回観測する。owned 値は本関数の
/// スコープ内でのみ借用されるため、借用の生存期間問題は起きない。
fn observe_window_row(
    acc: &mut Accumulator,
    input: &AggregateInput,
    id: u64,
    value: &WindowInputValue,
    scratch: &mut Vec<StackValue>,
) -> Result<(), SqlSurfaceError> {
    let no_vector = RowVector {
        dim: 0,
        values: None,
    };
    match (input, value) {
        (AggregateInput::AllVisible | AggregateInput::IdU64, _) => {
            acc.observe(input, id, &no_vector, &[], scratch)
        }
        (AggregateInput::VectorColumnPresence, WindowInputValue::VectorPresence(dim)) => {
            let vector = RowVector {
                dim: *dim,
                values: None,
            };
            acc.observe(input, id, &vector, &[], scratch)
        }
        (AggregateInput::TextColumn(index), WindowInputValue::Text(v)) => {
            let scanned = build_scanned(*index, v.as_deref().map(row_codec::ScalarRef::Text));
            acc.observe(input, id, &no_vector, &scanned, scratch)
        }
        (
            AggregateInput::IntegerColumn(index) | AggregateInput::SmallIntColumn(index),
            WindowInputValue::Integer(v),
        ) => {
            let scanned = build_scanned(*index, v.map(row_codec::ScalarRef::Integer));
            acc.observe(input, id, &no_vector, &scanned, scratch)
        }
        (AggregateInput::BigIntColumn(index), WindowInputValue::BigInt(v)) => {
            let scanned = build_scanned(*index, v.map(row_codec::ScalarRef::BigInt));
            acc.observe(input, id, &no_vector, &scanned, scratch)
        }
        (AggregateInput::RealColumn(index), WindowInputValue::Real(v)) => {
            let scanned = build_scanned(*index, v.map(row_codec::ScalarRef::Real));
            acc.observe(input, id, &no_vector, &scanned, scratch)
        }
        (AggregateInput::DoubleColumn(index), WindowInputValue::Double(v)) => {
            let scanned = build_scanned(*index, v.map(row_codec::ScalarRef::Double));
            acc.observe(input, id, &no_vector, &scanned, scratch)
        }
        (AggregateInput::DateColumn(index), WindowInputValue::Date(v)) => {
            let scanned = build_scanned(*index, v.map(row_codec::ScalarRef::Date));
            acc.observe(input, id, &no_vector, &scanned, scratch)
        }
        (AggregateInput::TimestampColumn(index), WindowInputValue::Timestamp(v)) => {
            let scanned = build_scanned(*index, v.map(row_codec::ScalarRef::Timestamp));
            acc.observe(input, id, &no_vector, &scanned, scratch)
        }
        (AggregateInput::TimestampTzColumn(index), WindowInputValue::TimestampTz(v)) => {
            let scanned = build_scanned(*index, v.map(row_codec::ScalarRef::TimestampTz));
            acc.observe(input, id, &no_vector, &scanned, scratch)
        }
        (AggregateInput::TimeColumn(index), WindowInputValue::Time(v)) => {
            let scanned = build_scanned(*index, v.map(row_codec::ScalarRef::Time));
            acc.observe(input, id, &no_vector, &scanned, scratch)
        }
        (AggregateInput::NumericColumn { index, .. }, WindowInputValue::Numeric(v)) => {
            let scanned = build_scanned(*index, v.map(row_codec::ScalarRef::Numeric));
            acc.observe(input, id, &no_vector, &scanned, scratch)
        }
        (
            AggregateInput::BooleanColumn(index)
            | AggregateInput::ArrayColumn(index)
            | AggregateInput::ByteaColumn(index)
            | AggregateInput::JsonColumn(index)
            | AggregateInput::EnumColumn(index)
            | AggregateInput::UuidColumn(index),
            WindowInputValue::PresenceOnly(present),
        ) => {
            // これらの型は `Accumulator::observe` が非 NULL の有無だけを見る
            // （値の中身は見ない）ため、任意の `Some`/`None` の
            // `ScalarRef::Bool` で代用できる（`sql::aggregate` の該当分岐参照）。
            let scanned = build_scanned(
                *index,
                if *present {
                    Some(row_codec::ScalarRef::Bool(true))
                } else {
                    None
                },
            );
            acc.observe(input, id, &no_vector, &scanned, scratch)
        }
        _ => Err(window_bug(
            "window aggregate input/value combination mismatch",
        )),
    }
}

/// 出力位置 `pos` に対応する `materialized` の添字を返す。`order` が `None`
/// （`ORDER BY` なし）なら物理走査順の恒等、`Some` なら順列経由で引く。
fn resolve_output_index(order: Option<&[usize]>, pos: usize) -> Option<usize> {
    match order {
        None => Some(pos),
        Some(o) => o.get(pos).copied(),
    }
}

/// 投影段（§モジュールドキュメント参照）: ウィンドウ以外の投影・`LIMIT`／
/// `OFFSET` は `windows` を空にした複製を [`crate::sql::scan::execute_scan`] へ
/// 渡すことで既存の実行器をそのまま再利用し、その結果へウィンドウ列を
/// [`crate::sql::parser::BoundWindowItem::position`] に基づいて差し込む。
///
/// `materialized`／`window_values` はどちらも同じ添字（`materialize_rows` の
/// 物理走査順）で対応する。`base_result`（`execute_scan_with_budget` への委譲）は
/// 同じ `read_txn`・同じ `WHERE`／可視性条件を独立に走査するため、物理走査順は
/// `materialized` と一致する（`OFFSET` も「可視かつ `WHERE` 一致の行を物理順で
/// 数える」点で同一の意味論。`sql::scan` の該当コメント参照）。この不変条件を前提に
/// `bound.offset()` から始まる添字で `base_result` の n 番目の出力行を
/// `materialized[offset + n]` と対応付ける。他テナントの可視 `Public` 行と
/// 衝突しうる `id` を対応付けキーに使わない（P1 レビュー指摘対応。以前は
/// `id -> Cell` の `HashMap` で対応付けており、異なるテナントの可視行が同じ
/// `id` を持つと後勝ちで値が上書きされ、順位・集計値が誤った行に付く欠陥が
/// あった）。`id` の一致は defense-in-depth として引き続き検証する。
fn build_result(
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    schema: &TableSchema,
    bound: &BoundScan,
    materialized: &[MaterializedRow],
    evaluated: EvaluatedWindows<'_>,
    max_result_bytes: usize,
) -> Result<QueryResult, SqlSurfaceError> {
    let EvaluatedWindows {
        output_order,
        window_values,
    } = evaluated;
    let offset = bound.offset();
    let limit = bound.limit();
    let output_end = materialized.len().min(offset.saturating_add(limit));

    // レビュー指摘対応: `execute_scan_with_budget`（`base_result`）の
    // `max_result_bytes` はウィンドウ以外の投影列のみを計上し、後から差し込む
    // ウィンドウ列の `Cell` 構造体・可変長データ（`MIN`/`MAX(<TEXT 列>)` の
    // `Cell::Text`）を計上しないため、その分だけ結果サイズ上限を超えられて
    // しまっていた。出力候補行数（`offset..output_end`）に対して発生しうる
    // ウィンドウ列のバイト数を先に見積り、同じ `max_result_bytes` 予算から
    // 差し引いた残りを `base_result` へ渡すことで、投影列とウィンドウ列を
    // 合わせた合計が常に `max_result_bytes` 以内になるようにする。
    let mut window_bytes: usize = 0;
    if offset < output_end {
        let row_count = output_end - offset;
        let per_row_struct_bytes = window_values
            .len()
            .saturating_mul(std::mem::size_of::<Cell>());
        window_bytes = try_accumulate_window_result_budget(
            window_bytes,
            per_row_struct_bytes.saturating_mul(row_count),
            max_result_bytes,
        )?;
        for cells in window_values {
            for pos in offset..output_end {
                let Some(mat_idx) = resolve_output_index(output_order, pos) else {
                    continue;
                };
                if let Some(Cell::Text(s)) = cells.get(mat_idx) {
                    window_bytes = try_accumulate_window_result_budget(
                        window_bytes,
                        s.len(),
                        max_result_bytes,
                    )?;
                }
            }
        }
    }
    let remaining_result_bytes = max_result_bytes.saturating_sub(window_bytes);

    let mut base_bound = bound.clone();
    base_bound.windows = Vec::new();
    let base_result = crate::sql::scan::execute_scan_with_budget(
        read_txn,
        ctx,
        schema,
        &base_bound,
        remaining_result_bytes,
    )?;

    let plain_len = base_result.columns.len();
    let total_len = plain_len + window_values.len();

    // ウィンドウ項目が占める SELECT リスト内位置（`position`）を除いた残りへ、
    // 通常項目を出現順に割り当てる（`Projection` 自体はウィンドウ項目の位置情報を
    // 保持しないため、ここで復元する）。
    let mut window_positions: Vec<usize> = bound.windows().iter().map(|w| w.position).collect();
    window_positions.sort_unstable();
    let mut plain_positions: Vec<usize> = Vec::with_capacity(plain_len);
    {
        let mut wpos_iter = window_positions.iter().peekable();
        for pos in 0..total_len {
            if wpos_iter.peek() == Some(&&pos) {
                wpos_iter.next();
            } else {
                plain_positions.push(pos);
            }
        }
    }
    if plain_positions.len() != plain_len {
        // 通常項目の位置数と `base_result` の列数が一致しない状態は、束縛段
        // （`sql::parser::bind_scan_with_dummy_flags`）の不変条件違反であり、
        // untrusted 入力起因ではないため fail-closed に内部エラーとする。
        return Err(SqlSurfaceError::Internal {
            detail: "window scan projection position mismatch".to_string(),
        });
    }

    let mut final_columns: Vec<Option<ColumnMeta>> = vec![None; total_len];
    for (pos, meta) in plain_positions.iter().zip(base_result.columns.iter()) {
        if let Some(slot) = final_columns.get_mut(*pos) {
            *slot = Some(meta.clone());
        }
    }
    for item in bound.windows() {
        if let Some(slot) = final_columns.get_mut(item.position) {
            // ウィンドウ関数の結果型は入力型に従う静的型（Issue #1344）。
            *slot = Some(ColumnMeta::Computed {
                name: item.name.clone(),
                ty: window_result_type(item),
            });
        }
    }
    let columns: Vec<ColumnMeta> = final_columns
        .into_iter()
        .enumerate()
        .map(|(pos, c)| {
            c.ok_or_else(|| SqlSurfaceError::Internal {
                detail: format!("window scan column metadata missing at position {pos}"),
            })
        })
        .collect::<Result<_, _>>()?;

    let mut rows = Vec::with_capacity(base_result.rows.len());
    // `materialized`／`window_values` の対応する行への添字（`offset` から開始し、
    // 出力行 1 件ごとに 1 つ進める。§関数ドキュメントの物理走査順の不変条件参照）。
    // Issue #1189: 対応する `materialized` の添字は `output_order`（文全体の
    // `ORDER BY` 適用後の順列。なしなら恒等）経由で引く。
    let mut output_pos = offset;
    for base_row in base_result.rows {
        let materialized_idx = resolve_output_index(output_order, output_pos)
            .ok_or_else(|| window_bug("window scan row correlation index exceeded output order"))?;
        let mat_row = materialized.get(materialized_idx).ok_or_else(|| {
            window_bug("window scan row correlation index exceeded materialized rows")
        })?;
        // defense-in-depth: 対応付けは添字（物理走査順）で行うが、`materialize_rows`
        // と `execute_scan_with_budget` は独立実装であるため、将来のドリフトで
        // 行集合が食い違った場合に静かに誤った行へウィンドウ値を付けてしまわない
        // よう `id` の一致も確認する（他テナントの可視 `Public` 行と衝突しうる
        // `id` 自体を対応付けキーにはしない。P1 レビュー指摘対応）。
        // 採番方式の表（TABLE-23）は基底走査の `id` が `0` にマスク済みで生 rowid と比較できない
        // ため、この `id` 突合は従来方式の表に限る（`base_row.id` はマスク後、`mat_row.id` は生値）。
        if !schema.is_allocated() && mat_row.id != base_row.id {
            return Err(window_bug(
                "window scan row correlation id mismatch between materialize and base scan",
            ));
        }
        let mut cells: Vec<Cell> = Vec::with_capacity(total_len);
        let mut plain_iter = base_row.cells.into_iter();
        for pos in 0..total_len {
            if let Some(item_index) = bound.windows().iter().position(|w| w.position == pos) {
                let cell = window_values
                    .get(item_index)
                    .ok_or_else(|| window_bug("window item index out of range in build_result"))?
                    .get(materialized_idx)
                    .cloned()
                    .ok_or_else(|| {
                        window_bug("window_values missing entry for a materialized row index")
                    })?;
                cells.push(cell);
            } else {
                match plain_iter.next() {
                    Some(cell) => cells.push(cell),
                    None => {
                        return Err(SqlSurfaceError::Internal {
                            detail: "window scan projection cell count mismatch".to_string(),
                        })
                    }
                }
            }
        }
        rows.push(ResultRow {
            id: base_row.id,
            score: base_row.score,
            cells,
        });
        output_pos = output_pos.checked_add(1).ok_or_else(|| {
            SqlSurfaceError::payload_too_large("window scan row correlation index overflowed")
        })?;
    }

    Ok(QueryResult { columns, rows })
}

#[cfg(test)]
mod limit_tests {
    //! [`MAX_WINDOW_PARTITIONS`]／[`MAX_WINDOW_ROWS`]／[`MAX_WINDOW_FRAME_ROWS`]／
    //! [`MAX_WINDOW_STATE_BYTES`] の実行時超過が `54000`
    //! （[`SqlSurfaceError::payload_too_large`]）で fail-closed に拒否されることの
    //! 単体テスト（PR #930 最終レビュー指摘 2）。`MAX_WINDOW_ROWS`／
    //! `MAX_WINDOW_FRAME_ROWS` は実装既定値が 100 万行と大きく、実データでの
    //! 結合テストは重すぎるため、判定ロジックを切り出した
    //! [`check_new_partition_capacity`]／[`check_partition_row_count`]／
    //! [`check_total_row_count`] を小さい `cap` を渡して直接検証する
    //! （本体は常に実装既定の `MAX_WINDOW_*` 定数を渡すため、この単体テストは
    //! 定数値そのものではなく比較ロジック〔`>=`/`>`〕の正しさを固定する）。
    //! `MAX_WINDOW_PARTITIONS`（`sql::group_by::MAX_GROUPS` = 10,000）超過は
    //! 実データでも現実的なため `tests/sql30_window.rs` に別途結合テストを持つ
    //! （本モジュールの単体テストと二重に固定することで、判定ロジックと実行系
    //! 統合の両方を回帰対象にする）。

    use super::*;

    #[test]
    fn new_partition_capacity_accepts_up_to_cap_and_rejects_beyond() {
        let cap = 3usize;
        // 既存パーティション数が cap 未満なら新規パーティションを受理する。
        assert!(check_new_partition_capacity(0, cap).is_ok());
        assert!(check_new_partition_capacity(cap - 1, cap).is_ok());
        // 既存パーティション数が cap に達した状態で新規パーティションを
        // 追加しようとすると拒否する（`>=` 比較。ちょうど cap 件までは
        // 既存パーティションとして許容し、cap+1 件目の新規作成を拒否する）。
        let err = check_new_partition_capacity(cap, cap).expect_err("cap 到達時は拒否");
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn partition_row_count_accepts_up_to_cap_and_rejects_beyond() {
        let cap = 5usize;
        assert!(check_partition_row_count(cap, cap).is_ok());
        let err = check_partition_row_count(cap + 1, cap).expect_err("cap 超過時は拒否");
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn total_row_count_accepts_up_to_cap_and_rejects_beyond() {
        let cap = 7usize;
        assert!(check_total_row_count(cap, cap).is_ok());
        let err = check_total_row_count(cap + 1, cap).expect_err("cap 超過時は拒否");
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn state_budget_accepts_up_to_cap_and_rejects_beyond() {
        let cap = 100usize;
        assert_eq!(try_accumulate_state_budget(90, 10, cap).unwrap(), 100);
        let err = try_accumulate_state_budget(90, 11, cap).expect_err("cap 超過時は拒否");
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn state_budget_rejects_on_addition_overflow() {
        let err = try_accumulate_state_budget(usize::MAX, 1, usize::MAX)
            .expect_err("オーバーフローは拒否");
        assert_eq!(err.wire_code(), "54000");
    }
}
#[cfg(test)]
mod budget_regression_tests {
    //! `execute_window_scan`（`pub(crate)`）を低レベル API で直接呼ぶ回帰テスト
    //! （PR #930 2 回目レビュー指摘 2・3）。`sql::scan::
    //! execute_scan_with_budget_honors_caller_supplied_cap`／`sql::aggregate::
    //! execute_aggregate_with_cache_honors_caller_supplied_result_byte_budget_
    //! for_text_min_max` と同じ検証形（既定に近い予算では成功・呼び出し元が
    //! 指定した小さい予算では `54000` で早期失敗）を踏襲する。

    use super::*;
    use crate::catalog::ColumnDef;
    use crate::sql::allowlist::AggregateFunc;
    use crate::storage::{RowInput, Storage, Visibility};
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
    use redb::ReadableDatabase;

    fn item(func: WindowFunc, input: Option<AggregateInput>) -> BoundWindowItem {
        BoundWindowItem {
            frame: None,
            position: 0,
            func,
            input,
            offset_args: None,
            partition_by: Vec::new(),
            order_by: Vec::new(),
            name: "w".to_string(),
        }
    }

    /// `LAG`／`LEAD` の結果型は値（既定値があれば統一後）の型（SQL-64 (1)・Issue #1880）。
    #[test]
    fn window_result_type_for_lag_lead_follows_value_type() {
        use crate::sql::parser::{BoundWindowOffsetArgs, BoundWindowValue};
        for func in [WindowFunc::Lag, WindowFunc::Lead] {
            let mut it = item(func, None);
            it.offset_args = Some(BoundWindowOffsetArgs {
                value: BoundWindowValue::Column {
                    index: 0,
                    ty: ColumnType::Uuid,
                },
                offset: None,
                default: None,
                default_column: None,
                default_pseudo_id: false,
                result_type: Some(ColumnType::Uuid),
                uses_pseudo_id: false,
            });
            assert_eq!(window_result_type(&it), Some(ColumnType::Uuid));
            // 引数が無い（束縛前の）項目は型を公告しない（fail-closed）。
            assert_eq!(window_result_type(&item(func, None)), None);
        }
    }

    /// `ntile` の状態機械は PostgreSQL の `window_ntile` と同じバケット境界を返す（Issue #1884）。
    #[test]
    fn ntile_state_machine_matches_postgres_boundaries() {
        let run = |total: i64, n: i64| -> Vec<i64> {
            let mut st = NtileState::default();
            if total == 0 {
                return Vec::new();
            }
            st.start(total, n);
            (0..total).map(|_| st.advance()).collect()
        };
        assert_eq!(run(5, 2), vec![1, 1, 1, 2, 2]);
        assert_eq!(run(7, 3), vec![1, 1, 1, 2, 2, 3, 3]);
        assert_eq!(run(10, 4), vec![1, 1, 1, 2, 2, 2, 3, 3, 4, 4]);
        assert_eq!(run(3, 5), vec![1, 2, 3]);
        assert_eq!(run(4, 1), vec![1, 1, 1, 1]);
        assert_eq!(run(6, 3), vec![1, 1, 2, 2, 3, 3]);
        assert!(run(0, 3).is_empty());
    }

    /// 順位関数と `COUNT` は `BigInt`、集計系は集計 SELECT と同じ規則（Issue #1344）。
    /// SQL-64・Issue #1885: 分布関数の値と float8 テキスト表現。
    #[test]
    fn distribution_values_and_text() {
        let pr = |i, n| crate::scalar_float::format_double(percent_rank_value(i, n));
        let cd = |e, n| crate::scalar_float::format_double(cume_dist_value(e, n));
        assert_eq!(pr(0, 1), "0");
        assert_eq!(pr(1, 5), "0.25");
        assert_eq!(pr(4, 5), "1");
        assert_eq!(pr(1, 3), "0.5");
        assert_eq!(cd(1, 3), "0.3333333333333333");
        assert_eq!(cd(2, 3), "0.6666666666666666");
        assert_eq!(cd(3, 5), "0.6");
        assert_eq!(cd(5, 5), "1");
    }

    #[test]
    fn window_result_type_maps_ranking_count_and_aggregates() {
        for f in [
            WindowFunc::RowNumber,
            WindowFunc::Rank,
            WindowFunc::DenseRank,
        ] {
            assert_eq!(window_result_type(&item(f, None)), Some(ColumnType::BigInt));
        }
        for f in [WindowFunc::PercentRank, WindowFunc::CumeDist] {
            assert_eq!(window_result_type(&item(f, None)), Some(ColumnType::Double));
        }
        assert_eq!(
            window_result_type(&item(WindowFunc::Count, Some(AggregateInput::AllVisible))),
            Some(ColumnType::BigInt)
        );
        let inputs = || {
            vec![
                AggregateInput::IdU64,
                AggregateInput::IntegerColumn(0),
                AggregateInput::BigIntColumn(0),
                AggregateInput::RealColumn(0),
                AggregateInput::DoubleColumn(0),
            ]
        };
        for (wf, af) in [
            (WindowFunc::Sum, AggregateFunc::Sum),
            (WindowFunc::Avg, AggregateFunc::Avg),
            (WindowFunc::Min, AggregateFunc::Min),
            (WindowFunc::Max, AggregateFunc::Max),
        ] {
            for input in inputs() {
                let expected = aggregate::aggregate_result_type(af, &input);
                assert!(expected.is_some());
                assert_eq!(window_result_type(&item(wf, Some(input))), expected);
            }
        }
        assert_eq!(window_result_type(&item(WindowFunc::Sum, None)), None);
    }

    /// `body`（TEXT・nullable）1 列のみのテーブル（`MIN(body) OVER (...)` の
    /// 検証専用）。
    fn schema_with_text() -> TableSchema {
        TableSchema::new("docs", vec![ColumnDef::new("body", ColumnType::Text, true)])
    }

    /// 低レベル API（`storage::encode_row`）で `body` 列の値を直接書き込む
    /// （`sql::aggregate` の同種テストヘルパと同方針。公開 INSERT 経路を経由しない）。
    fn write_text_row(
        storage: &Storage,
        schema: &TableSchema,
        tenant_id: &str,
        id: u64,
        body: &str,
    ) {
        let metadata =
            row_codec::encode_scalar_columns(schema, &[row_codec::Value::Text(body.to_string())])
                .expect("encode scalar columns");
        let buf = crate::storage::encode_row(&RowInput {
            tenant_id,
            visibility: Visibility::Public,
            embedding: &[],
            metadata: &metadata,
        })
        .expect("encode row");
        let write_txn = storage.db().begin_write().expect("begin_write");
        {
            let mut table = write_txn
                .open_table(catalog::user_rows_table_def(
                    &catalog::user_rows_table_name("docs"),
                ))
                .expect("open row table");
            table
                .insert((tenant_id, id), buf.as_slice())
                .expect("insert row");
        }
        crate::storage::bump_generation_and_commit(write_txn).expect("commit");
    }

    /// `SELECT id, MIN(body) OVER () FROM docs LIMIT 10` 相当の `BoundScan`
    /// （`AggregateFunc` は不使用。`window_func_to_aggregate_func` が変換する）。
    fn bound_min_body_over() -> BoundScan {
        BoundScan {
            result_cap: None,
            table: "docs".to_string(),
            projection: vec![ProjectedColumn::Id],
            metadata_filters: Vec::new(),
            expr_filters: Vec::new(),
            expr_filter_programs: Vec::new(),
            or_filters: Vec::new(),
            limit: 10,
            order_by: Vec::new(),
            offset: 0,
            windows: vec![BoundWindowItem {
                frame: None,
                position: 1,
                func: WindowFunc::Min,
                input: Some(AggregateInput::TextColumn(0)),
                offset_args: None,
                partition_by: Vec::new(),
                order_by: Vec::new(),
                name: "min_body".to_string(),
            }],
            order_exprs: Vec::new(),
            ties_row_cap: None,
        }
    }

    /// レビュー指摘 2 の回帰: `execute_window_scan` の `max_result_bytes` は
    /// 従来ウィンドウ以外の投影列（本テストでは疑似列 `id` のみ）しか計上せず、
    /// 後から差し込む `MIN(<TEXT 列>)` の `Cell::Text` を無視していた。十分に
    /// 長い `body` 値では、「`id` 単体なら収まるが `MIN(body)` を足すと収まらない」
    /// 予算で `54000` になることを固定する（`AggregateFunc` を経由しない点以外は
    /// `execute_aggregate_with_cache_honors_caller_supplied_result_byte_budget_
    /// for_text_min_max` と同じ検証形）。
    #[test]
    fn window_result_byte_budget_accounts_for_added_text_cell() {
        let path = unique_db_path("window-result-budget-text");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = schema_with_text();
        storage.create_table(&schema).expect("create table");
        let long_body = "x".repeat(10_000);
        write_text_row(&storage, &schema, "tenant-a", 1, &long_body);

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let read_txn = storage.db().begin_read().expect("begin_read");
        let bound = bound_min_body_over();

        // 既定に近い十分な予算では成功する。
        execute_window_scan(&read_txn, &ctx, &schema, &bound, 1_000_000)
            .expect("ample budget should succeed");

        // `id` 単体の投影であれば収まるが、`MIN(body)`（10,000 バイトの TEXT）を
        // 足すと収まらない予算では `54000` を返す（修正前は window セルの
        // バイト数を一切計上しないため、この予算でも誤って成功していた）。
        let err = execute_window_scan(&read_txn, &ctx, &schema, &bound, 256)
            .expect_err("budget that only fits the plain id column must still reject");
        assert_eq!(err.wire_code(), "54000");
    }

    /// レビュー指摘 3 の回帰: `MAX_WINDOW_STATE_BYTES` は materialize 段だけを
    /// 計上し、評価段（`evaluate_window_item`／`evaluate_partition`）が確保する
    /// `partitions` マップ・結果ベクタ・複製した `Cell` を計上していなかった。
    /// materialize 段だけでちょうど収まる `state_cap`（評価段の余地がゼロ）を
    /// 注入すると、評価段の確保で `54000` になることを固定する。
    #[test]
    fn evaluation_stage_allocations_are_charged_against_state_budget() {
        let path = unique_db_path("window-state-budget-eval");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = schema_with_text();
        storage.create_table(&schema).expect("create table");
        let body = "y".repeat(4_000);
        write_text_row(&storage, &schema, "tenant-a", 1, &body);

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let read_txn = storage.db().begin_read().expect("begin_read");
        let bound = bound_min_body_over();

        let materialize_only_bytes = materialize_rows(&read_txn, &ctx, &schema, &bound, usize::MAX)
            .expect("materialize should succeed with an unbounded cap")
            .state_bytes;

        // materialize 段だけでちょうど収まる cap（評価段の追加確保の余地がない）。
        // 修正前は評価段が state_bytes を一切計上しなかったため、この cap でも
        // 誤って成功していた。
        let err = execute_window_scan_with_caps(
            &read_txn,
            &ctx,
            &schema,
            &bound,
            1_000_000,
            materialize_only_bytes,
        )
        .expect_err("cap with no headroom for the evaluation stage must reject");
        assert_eq!(err.wire_code(), "54000");

        // 評価段の分の余裕を足せば成功する。
        execute_window_scan_with_caps(
            &read_txn,
            &ctx,
            &schema,
            &bound,
            1_000_000,
            materialize_only_bytes.saturating_add(1_000_000),
        )
        .expect("cap with headroom for the evaluation stage should succeed");
    }

    /// `offset_target_index`: 正・0・負のオフセットと範囲外・`i32` の両端でもあふれない。
    #[test]
    fn offset_target_index_handles_boundaries_without_overflow() {
        // LAG（forward = false）は pos - offset、LEAD（forward = true）は pos + offset。
        assert_eq!(offset_target_index(2, 5, 1, false), Some(1));
        assert_eq!(offset_target_index(2, 5, 1, true), Some(3));
        assert_eq!(offset_target_index(2, 5, 0, false), Some(2));
        assert_eq!(offset_target_index(2, 5, -1, false), Some(3));
        assert_eq!(offset_target_index(2, 5, -1, true), Some(1));
        assert_eq!(offset_target_index(0, 5, 1, false), None);
        assert_eq!(offset_target_index(4, 5, 1, true), None);
        assert_eq!(offset_target_index(4, 5, 0, true), Some(4));
        assert_eq!(offset_target_index(0, 0, 0, true), None);
        for forward in [false, true] {
            assert_eq!(offset_target_index(3, 5, i32::MAX, forward), None);
            assert_eq!(offset_target_index(3, 5, i32::MIN, forward), None);
        }
    }

    /// `offset_from_expr_value`: NULL・整数・`int4` 範囲外（`22003`）・非整数（`XX000`）。
    #[test]
    fn offset_from_expr_value_validates_range_and_shape() {
        assert_eq!(offset_from_expr_value(ExprValue::Null).unwrap(), None);
        assert_eq!(
            offset_from_expr_value(ExprValue::Scalar(2.0)).unwrap(),
            Some(2)
        );
        assert_eq!(
            offset_from_expr_value(ExprValue::Scalar(-2_147_483_648.0)).unwrap(),
            Some(i32::MIN)
        );
        assert_eq!(
            offset_from_expr_value(ExprValue::Scalar(2_147_483_647.0)).unwrap(),
            Some(i32::MAX)
        );
        for v in [2_147_483_648.0, -2_147_483_649.0] {
            let err = offset_from_expr_value(ExprValue::Scalar(v)).expect_err("out of range");
            assert_eq!(err.wire_code(), "22003");
        }
        for v in [1.5, f64::NAN, f64::INFINITY] {
            let err = offset_from_expr_value(ExprValue::Scalar(v)).expect_err("not an integer");
            assert_eq!(err.wire_code(), "XX000");
        }
        let err = offset_from_expr_value(ExprValue::Bool(true)).expect_err("wrong variant");
        assert_eq!(err.wire_code(), "XX000");
    }

    /// `frame_offset_from_expr_value`: NULL・int8 全域（飽和）・`-0.0`・非整数（`XX000`）。
    #[test]
    fn frame_offset_from_expr_value_accepts_int8_range() {
        assert_eq!(frame_offset_from_expr_value(ExprValue::Null).unwrap(), None);
        assert_eq!(
            frame_offset_from_expr_value(ExprValue::Scalar(3.0)).unwrap(),
            Some(3)
        );
        assert_eq!(
            frame_offset_from_expr_value(ExprValue::Scalar(-1.0)).unwrap(),
            Some(-1)
        );
        // `i32` を超える値も受理する（`LAG` の int4 契約とは別）。
        assert_eq!(
            frame_offset_from_expr_value(ExprValue::Scalar(2_147_483_648.0)).unwrap(),
            Some(2_147_483_648)
        );
        assert_eq!(
            frame_offset_from_expr_value(ExprValue::Scalar(9.3e18)).unwrap(),
            Some(i64::MAX)
        );
        // `-0.0` は負として扱わない（検査側が `n < 0` で判定するため 0 になる）。
        assert_eq!(
            frame_offset_from_expr_value(ExprValue::Scalar(-0.0)).unwrap(),
            Some(0)
        );
        for v in [1.5, f64::NAN, f64::INFINITY] {
            let err = frame_offset_from_expr_value(ExprValue::Scalar(v)).expect_err("not integer");
            assert_eq!(err.wire_code(), "XX000");
        }
        let err = frame_offset_from_expr_value(ExprValue::Bool(true)).expect_err("wrong variant");
        assert_eq!(err.wire_code(), "XX000");
    }

    /// `check_frame_bound`: NULL は `22004`、負は `22013`、巨大値は飽和して受理される。
    #[test]
    fn check_frame_bound_classifies_null_and_negative() {
        let err = check_frame_bound(EvaluatedBound::Preceding(None), true).expect_err("null");
        assert_eq!(err.wire_code(), "22004");
        let err = check_frame_bound(EvaluatedBound::Following(Some(-1)), false).expect_err("neg");
        assert_eq!(err.wire_code(), "22013");
        assert_eq!(
            check_frame_bound(EvaluatedBound::Following(Some(i64::MAX)), false).unwrap(),
            ResolvedBound::Following(usize::try_from(i64::MAX).unwrap_or(usize::MAX))
        );
        assert_eq!(
            check_frame_bound(EvaluatedBound::CurrentRow, true).unwrap(),
            ResolvedBound::CurrentRow
        );
    }

    /// `rows_frame_bounds`: 先頭・末尾・空パーティション・飽和・始点が終点を超える空フレーム。
    #[test]
    fn rows_frame_bounds_clamps_and_detects_empty_frames() {
        use ResolvedBound::*;
        let f = |start, end| ResolvedRowsFrame {
            start,
            end,
            exclusion: WindowFrameExclusion::NoOthers,
        };
        let sliding = f(Preceding(1), Following(1));
        assert_eq!(rows_frame_bounds(0, 5, &sliding), (0, 2));
        assert_eq!(rows_frame_bounds(2, 5, &sliding), (1, 4));
        assert_eq!(rows_frame_bounds(4, 5, &sliding), (3, 5));
        assert_eq!(rows_frame_bounds(0, 1, &sliding), (0, 1));
        // 空パーティション（呼ばれないが安全に畳む）。
        assert_eq!(rows_frame_bounds(0, 0, &sliding), (0, 0));
        // 巨大オフセットは飽和してパーティション全体。
        let huge = f(Preceding(usize::MAX), Following(usize::MAX));
        assert_eq!(rows_frame_bounds(3, 10, &huge), (0, 10));
        // 始点が終点以降なら空（`start >= end`）。
        let (s, e) = rows_frame_bounds(2, 5, &f(Preceding(2), Preceding(3)));
        assert!(s >= e);
        let (s, e) = rows_frame_bounds(4, 5, &f(Following(1), UnboundedFollowing));
        assert!(s >= e);
        assert_eq!(
            rows_frame_bounds(2, 5, &f(UnboundedPreceding, CurrentRow)),
            (0, 3)
        );
        assert_eq!(
            rows_frame_bounds(2, 5, &f(CurrentRow, UnboundedFollowing)),
            (2, 5)
        );
    }

    /// `LAG` が保持する値（TEXT）も状態予算へ計上され、cap を下回ると `54000` になる。
    #[test]
    fn lag_value_retention_is_charged_against_state_budget() {
        use crate::sql::parser::{BoundWindowOffsetArgs, BoundWindowValue};
        let path = unique_db_path("window-state-budget-lag");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = schema_with_text();
        storage.create_table(&schema).expect("create table");
        let body = "y".repeat(4_000);
        write_text_row(&storage, &schema, "tenant-a", 1, &body);

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut bound = bound_min_body_over();
        bound.windows[0].func = WindowFunc::Lag;
        bound.windows[0].input = None;
        bound.windows[0].offset_args = Some(BoundWindowOffsetArgs {
            value: BoundWindowValue::Column {
                index: 0,
                ty: ColumnType::Text,
            },
            offset: None,
            default: None,
            default_column: None,
            default_pseudo_id: false,
            result_type: Some(ColumnType::Text),
            uses_pseudo_id: false,
        });

        let needed = materialize_rows(&read_txn, &ctx, &schema, &bound, usize::MAX)
            .expect("materialize should succeed with an unbounded cap")
            .state_bytes;
        assert!(needed >= body.len(), "the retained text must be counted");
        let err = materialize_rows(&read_txn, &ctx, &schema, &bound, needed - 1)
            .err()
            .expect("cap below the retained size must reject");
        assert_eq!(err.wire_code(), "54000");
    }

    /// 値・既定値の評価で保持する `22xxx` エラーの所有文字列も状態予算へ計上される
    /// （不正な文字列の CAST エラーが全行で残る形。cap を下回ると `54000`）。
    #[test]
    fn lag_deferred_error_strings_are_charged_against_state_budget() {
        let path = unique_db_path("window-state-budget-lag-err");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = schema_with_text();
        storage.create_table(&schema).expect("create table");
        let body = "z".repeat(60);
        for id in 1..=3u64 {
            write_text_row(&storage, &schema, "tenant-a", id, &body);
        }
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let read_txn = storage.db().begin_read().expect("begin_read");
        let udfs = crate::sql::udf_call::UdfRegistry::default();

        for sql in [
            // 値側のエラーが全行で保持される（既定値は NULL）。
            "SELECT id, lag(CAST(body AS integer), NULL) OVER (ORDER BY id) FROM docs LIMIT 10",
            // 既定値側のエラーが全行で保持される。
            "SELECT id, lag(7, 1, CAST(body AS integer)) OVER (ORDER BY id) FROM docs LIMIT 10",
        ] {
            let crate::sql::allowlist::Statement::Scan(validated) =
                crate::sql::allowlist::validate_sql(sql, &storage).expect("validate")
            else {
                panic!("expected scan statement");
            };
            let bound = crate::sql::parser::bind_scan(&validated, &schema, &udfs).expect("bind");
            let needed = materialize_rows(&read_txn, &ctx, &schema, &bound, usize::MAX)
                .expect("materialize should succeed with an unbounded cap")
                .state_bytes;
            // 3 行 x 保持エラー文字列（入力 60 文字を含む）が計上されていること。
            let baseline = {
                let mut b = bound.clone();
                b.windows[0].offset_args = None;
                b.windows[0].func = WindowFunc::RowNumber;
                materialize_rows(&read_txn, &ctx, &schema, &b, usize::MAX)
                    .expect("baseline")
                    .state_bytes
            };
            assert!(
                needed >= baseline + 3 * 60,
                "{sql}: error strings uncounted"
            );
            let err = materialize_rows(&read_txn, &ctx, &schema, &bound, needed - 1)
                .err()
                .expect("cap below the retained size must reject");
            assert_eq!(err.wire_code(), "54000", "{sql}");
        }
    }

    /// PR #1233 指摘の回帰: `ORDER BY` なしでは順列を確保せず状態予算にも計上しない
    /// （予算を使い切った状態でも成功し `None` を返す）。
    #[test]
    fn compute_output_order_without_order_by_allocates_nothing() {
        let bound = bound_min_body_over();
        assert!(bound.order_by.is_empty());
        let mut state_bytes = 10usize;
        let out = compute_output_order(&bound, &[], &[], &mut state_bytes, 10)
            .expect("no ORDER BY must not touch the state budget");
        assert!(out.is_none());
        assert_eq!(state_bytes, 10);
    }

    /// `window_func_to_aggregate_func` が `AggregateFunc::Min` へ変換すること
    /// （上記 2 テストが構成する `BoundWindowItem::func = WindowFunc::Min` の
    /// 前提を明示する。到達不能分岐の検出用ではなく、`use` の未使用警告回避も兼ねる）。
    #[test]
    fn min_window_func_maps_to_min_aggregate_func() {
        assert_eq!(
            window_func_to_aggregate_func(WindowFunc::Min).unwrap(),
            AggregateFunc::Min
        );
    }
}

#[cfg(test)]
mod partition_layout_tests {
    //! [`SortedPartition`]・[`cmp_order_keys`] の単体テスト（Issue #1888。SQL-64 ポインタ）。
    //! `MaterializedRow` を直接組み立て、peer 範囲・位置アクセス・タイブレークを固定する
    //! （フレーム評価 Issue #1889〜#1891 の土台。既存の挙動は不変）。

    use super::*;
    use crate::sql::parser::{BoundWindowOrderKey, WindowKeyKind, WindowKeyRef};

    fn item(keys: &[(bool, NullPlacement)]) -> BoundWindowItem {
        BoundWindowItem {
            frame: None,
            position: 0,
            func: WindowFunc::Rank,
            input: None,
            offset_args: None,
            partition_by: vec![],
            order_by: keys
                .iter()
                .enumerate()
                .map(|(i, &(descending, nulls))| BoundWindowOrderKey {
                    key: WindowKeyRef::Column {
                        index: i,
                        kind: WindowKeyKind::Integer,
                    },
                    descending,
                    nulls,
                })
                .collect(),
            name: "w".to_string(),
        }
    }

    fn row(seq: usize, id: u64, keys: &[Option<i32>]) -> MaterializedRow {
        MaterializedRow {
            seq,
            id,
            partition_keys: vec![vec![]],
            order_keys: vec![keys
                .iter()
                .map(|k| k.map(WindowKeyValue::Integer))
                .collect()],
            agg_values: vec![],
            offset_values: vec![],
            stmt_order_keys: vec![],
            tenant_idx: 0,
        }
    }

    fn groups(part: &SortedPartition<'_>) -> Vec<(usize, usize)> {
        part.peer_groups().map(|g| g.unwrap()).collect()
    }

    const ASC: (bool, NullPlacement) = (false, NullPlacement::Last);

    #[test]
    fn no_order_by_is_a_single_peer_group() {
        let it = item(&[]);
        let rows: Vec<_> = (0..3).map(|i| row(i, i as u64, &[])).collect();
        let part = SortedPartition::new(&it, 0, &rows, vec![0, 1, 2]);
        assert_eq!(groups(&part), vec![(0, 3)]);
    }

    #[test]
    fn equal_keys_form_consecutive_peer_groups() {
        let it = item(&[ASC]);
        let ks = [1, 1, 2, 3, 3, 3];
        let rows: Vec<_> = ks
            .iter()
            .enumerate()
            .map(|(i, &k)| row(i, i as u64, &[Some(k)]))
            .collect();
        let part = SortedPartition::new(&it, 0, &rows, (0..6).collect());
        assert_eq!(groups(&part), vec![(0, 2), (2, 3), (3, 6)]);
        assert!(part.are_peers(0, 1).unwrap());
        assert!(!part.are_peers(1, 2).unwrap());
    }

    #[test]
    fn nulls_are_peers_of_each_other_only() {
        for nulls in [NullPlacement::First, NullPlacement::Last] {
            for descending in [false, true] {
                let it = item(&[(descending, nulls)]);
                let rows = vec![
                    row(0, 0, &[None]),
                    row(1, 1, &[Some(1)]),
                    row(2, 2, &[None]),
                ];
                let part = SortedPartition::new(&it, 0, &rows, vec![0, 1, 2]);
                let g = groups(&part);
                assert_eq!(g.len(), 2, "nulls={nulls:?} desc={descending}");
                let sizes: Vec<_> = g.iter().map(|(s, e)| e - s).collect();
                let expect = match nulls {
                    NullPlacement::First => vec![2, 1],
                    NullPlacement::Last => vec![1, 2],
                };
                assert_eq!(sizes, expect);
            }
        }
    }

    #[test]
    fn second_key_difference_splits_peers() {
        let it = item(&[ASC, ASC]);
        let rows = vec![
            row(0, 0, &[Some(1), Some(1)]),
            row(1, 1, &[Some(1), Some(1)]),
            row(2, 2, &[Some(1), Some(2)]),
        ];
        let part = SortedPartition::new(&it, 0, &rows, vec![0, 1, 2]);
        assert_eq!(groups(&part), vec![(0, 2), (2, 3)]);
    }

    #[test]
    fn sort_tie_breaks_by_id_then_seq() {
        let it = item(&[ASC]);
        let rows = vec![
            row(0, 9, &[Some(1)]),
            row(1, 3, &[Some(1)]),
            row(2, 3, &[Some(1)]),
            row(3, 0, &[Some(0)]),
        ];
        let part = SortedPartition::new(&it, 0, &rows, vec![0, 1, 2, 3]);
        assert_eq!(part.row_indices(), &[3, 1, 2, 0]);
        // id が全て 0（CellWindowEvaluator の形）なら seq 順。
        let rows = vec![
            row(2, 0, &[Some(1)]),
            row(0, 0, &[Some(1)]),
            row(1, 0, &[Some(1)]),
        ];
        let part = SortedPartition::new(&it, 0, &rows, vec![0, 1, 2]);
        assert_eq!(part.row_indices(), &[1, 2, 0]);
    }

    #[test]
    fn out_of_range_position_is_internal_error() {
        let it = item(&[ASC]);
        let rows = vec![row(0, 0, &[Some(1)])];
        let part = SortedPartition::new(&it, 0, &rows, vec![0]);
        assert_eq!(part.row_index(1).expect_err("範囲外").wire_code(), "XX000");
        assert_eq!(part.row(1).err().expect("範囲外").wire_code(), "XX000");
    }

    #[test]
    fn empty_partition_has_no_peer_groups() {
        let it = item(&[ASC]);
        let rows: Vec<MaterializedRow> = vec![];
        let part = SortedPartition::new(&it, 0, &rows, vec![]);
        assert_eq!(part.len(), 0);
        assert!(groups(&part).is_empty());
    }

    /// `groups_frame_bounds` を全グループ昇順に呼んだ結果（Issue #1891）。
    fn group_frames(part: &SortedPartition<'_>, frame: ResolvedRowsFrame) -> Vec<(usize, usize)> {
        let mut head = GroupCursor::new(part).unwrap();
        let mut tail = GroupCursor::new(part).unwrap();
        part.peer_groups()
            .enumerate()
            .map(|(c, g)| {
                groups_frame_bounds(part, c, g.unwrap(), &frame, &mut head, &mut tail).unwrap()
            })
            .collect()
    }

    fn gframe(start: ResolvedBound, end: ResolvedBound) -> ResolvedRowsFrame {
        ResolvedRowsFrame {
            start,
            end,
            exclusion: WindowFrameExclusion::NoOthers,
        }
    }

    #[test]
    fn groups_frame_bounds_follow_peer_group_numbers() {
        use ResolvedBound::*;
        // キー [1, 2, 2, NULL(最後)] → グループ (0,1) (1,3) (3,4)。
        let it = item(&[ASC]);
        let rows = vec![
            row(0, 0, &[Some(1)]),
            row(1, 1, &[Some(2)]),
            row(2, 2, &[Some(2)]),
            row(3, 3, &[None]),
        ];
        let part = SortedPartition::new(&it, 0, &rows, vec![0, 1, 2, 3]);
        assert_eq!(
            group_frames(&part, gframe(Preceding(1), Following(1))),
            vec![(0, 3), (0, 4), (1, 4)]
        );
        // 先頭を越える PRECEDING の終点は空（0）。始点は 0 にクランプ。
        assert_eq!(
            group_frames(&part, gframe(Preceding(2), Preceding(2))),
            vec![(0, 0), (0, 0), (0, 1)]
        );
        assert_eq!(
            group_frames(&part, gframe(CurrentRow, CurrentRow)),
            vec![(0, 1), (1, 3), (3, 4)]
        );
        // 末尾を越える FOLLOWING は len へ。
        assert_eq!(
            group_frames(&part, gframe(Following(1), Following(5))),
            vec![(1, 4), (3, 4), (4, 4)]
        );
        // 巨大オフセットは飽和し、パニックしない。
        assert_eq!(
            group_frames(&part, gframe(Preceding(usize::MAX), Following(usize::MAX))),
            vec![(0, 4), (0, 4), (0, 4)]
        );
        assert_eq!(
            group_frames(&part, gframe(UnboundedPreceding, UnboundedFollowing)),
            vec![(0, 4); 3]
        );
    }

    #[test]
    fn groups_frame_bounds_handle_degenerate_partitions() {
        use ResolvedBound::*;
        let it = item(&[ASC]);
        let empty: Vec<MaterializedRow> = vec![];
        let part = SortedPartition::new(&it, 0, &empty, vec![]);
        assert!(group_frames(&part, gframe(Preceding(1), Following(1))).is_empty());
        // 1 グループ（全行が peer）。
        let rows = vec![row(0, 0, &[Some(7)]), row(1, 1, &[Some(7)])];
        let part = SortedPartition::new(&it, 0, &rows, vec![0, 1]);
        assert_eq!(
            group_frames(&part, gframe(Preceding(3), Following(3))),
            vec![(0, 2)]
        );
        // 第 2 キーの違いでグループが分かれる。
        let it2 = item(&[ASC, ASC]);
        let rows = vec![
            row(0, 0, &[Some(1), Some(1)]),
            row(1, 1, &[Some(1), Some(2)]),
        ];
        let part = SortedPartition::new(&it2, 0, &rows, vec![0, 1]);
        assert_eq!(
            group_frames(&part, gframe(CurrentRow, CurrentRow)),
            vec![(0, 1), (1, 2)]
        );
    }
}
/// `RANGE` フレームの境界計算（Issue #1890。`range_limit`／`in_range_limit`）の単体テスト。
/// 期待値は PostgreSQL 17 の `in_range_*` の挙動。結合テスト（`sql64_window_range_frame.rs`）は
/// 列に格納できない NaN・無限大と、セッション TZ をまたぐ暦計算をここで補う。
#[cfg(test)]
mod range_frame_tests {
    use super::*;
    use crate::datetime::{Interval, SessionTimeZone, SessionTimeZoneScope};

    fn limit(base: &WindowKeyValue, off: RangeOffset, sub: bool) -> RangeLimit {
        range_limit(base, &off, sub).expect("limit")
    }

    fn within(val: &WindowKeyValue, l: &RangeLimit, less: bool) -> bool {
        in_range_limit(val, l, less).expect("in_range")
    }

    fn dbl(v: f64) -> WindowKeyValue {
        WindowKeyValue::Double(v)
    }

    #[test]
    fn float_nan_sorts_last_and_equals_itself() {
        let nan = dbl(f64::NAN);
        let one = dbl(1.0);
        // 基準値が NaN: NaN は範囲内（等しい）、非 NaN は「小さい側」だけ範囲内。
        let l = limit(&nan, RangeOffset::Float(1.0), true);
        assert!(within(&nan, &l, true) && within(&nan, &l, false));
        assert!(within(&one, &l, true));
        assert!(!within(&one, &l, false));
        // 基準値が非 NaN: NaN は「大きい側」だけ範囲内。
        let l = limit(&one, RangeOffset::Float(1.0), false);
        assert!(!within(&nan, &l, true));
        assert!(within(&nan, &l, false));
    }

    #[test]
    fn float_infinite_base_and_offset_cover_everything() {
        let inf = dbl(f64::INFINITY);
        let ninf = dbl(f64::NEG_INFINITY);
        let two = dbl(2.0);
        // +inf - inf と -inf + inf は NaN になる組で、PostgreSQL は常に範囲内とする。
        let l = limit(&inf, RangeOffset::Float(f64::INFINITY), true);
        assert!(within(&two, &l, false) && within(&ninf, &l, false));
        let l = limit(&ninf, RangeOffset::Float(f64::INFINITY), false);
        assert!(within(&two, &l, true) && within(&inf, &l, true));
        // -inf - inf = -inf（通常計算）。終点（`val <= base - off`）は -inf だけが範囲内。
        let l = limit(&ninf, RangeOffset::Float(f64::INFINITY), true);
        assert!(within(&ninf, &l, true));
        assert!(!within(&two, &l, true));
        assert!(within(&two, &l, false));
        // 有限基準値 - inf は -inf へ飽和する。
        let l = limit(&two, RangeOffset::Float(f64::INFINITY), true);
        assert!(within(&ninf, &l, true) && !within(&two, &l, true));
        assert!(within(&two, &l, false));
    }

    #[test]
    fn negative_and_nan_offsets_are_detected_per_key_type() {
        let t = WindowKeyValue::Time(0);
        let ts = WindowKeyValue::Timestamp(0);
        let iv = |months, days, micros| {
            RangeOffset::Interval(Interval {
                months,
                days,
                micros,
            })
        };
        // TIME キーは時間成分だけを見る（月・日は無視）。
        assert!(!range_offset_is_negative(&iv(-1, -1, 0), &t));
        assert!(range_offset_is_negative(&iv(0, 0, -1), &t));
        // 他の日時キーは正規化値（月 = 30 日）で判定する。
        assert!(range_offset_is_negative(&iv(1, -31, 0), &ts));
        assert!(!range_offset_is_negative(&iv(1, -29, 0), &ts));
        assert!(range_offset_is_negative(
            &RangeOffset::Float(f64::NAN),
            &dbl(0.0)
        ));
        assert!(range_offset_is_negative(
            &RangeOffset::Float(-0.5),
            &dbl(0.0)
        ));
        assert!(!range_offset_is_negative(
            &RangeOffset::Float(0.0),
            &dbl(0.0)
        ));
        assert!(range_offset_is_negative(
            &RangeOffset::Int(-1),
            &WindowKeyValue::Integer(0)
        ));
    }

    #[test]
    fn integer_limits_do_not_overflow() {
        let max = WindowKeyValue::BigInt(i64::MAX);
        let l = limit(&max, RangeOffset::Int(i64::MAX), false);
        assert!(within(&max, &l, true));
        let min = WindowKeyValue::BigInt(i64::MIN);
        let l = limit(&min, RangeOffset::Int(i64::MAX), true);
        assert!(within(&min, &l, false));
        assert!(!within(&min, &l, true));
    }

    #[test]
    fn timestamptz_offsets_add_calendar_days_in_the_session_zone() {
        let ny = SessionTimeZone::parse_setting("America/New_York").expect("zone");
        let utc = SessionTimeZone::utc();
        let parse = |s: &str| crate::datetime::parse_timestamptz(s, &utc).expect("timestamptz");
        // 2024-03-10 は America/New_York の DST 開始日（その日の 2:00 に 1 時間進む）。
        let d = parse("2024-03-10 12:00:00-04:00");
        let key = WindowKeyValue::TimestampTz(d);
        let day = RangeOffset::Interval(Interval {
            months: 0,
            days: 1,
            micros: 0,
        });
        let hours24 = RangeOffset::Interval(Interval {
            months: 0,
            days: 0,
            micros: 24 * 3_600_000_000,
        });
        let _scope = SessionTimeZoneScope::enter(ny);
        // `1 day` は壁時計で前日の同時刻（EST の 12:00 = 17:00 UTC）。`24 hours` は 24 時間前。
        let RangeLimit::Micros(m) = limit(&key, day, true) else {
            panic!("expected micros limit");
        };
        assert_eq!(m, i128::from(parse("2024-03-09 12:00:00-05:00")));
        let RangeLimit::Micros(m) = limit(&key, hours24, true) else {
            panic!("expected micros limit");
        };
        assert_eq!(m, i128::from(d - 24 * 3_600_000_000));
    }

    #[test]
    fn time_offsets_do_not_wrap_and_ignore_months_and_days() {
        let t = WindowKeyValue::Time(23 * 3_600_000_000);
        let hours = |h: i64| {
            RangeOffset::Interval(Interval {
                months: 1,
                days: 1,
                micros: h * 3_600_000_000,
            })
        };
        let RangeLimit::Micros(m) = limit(&t, hours(2), false) else {
            panic!("expected micros limit");
        };
        assert_eq!(m, i128::from(25 * 3_600_000_000_i64));
        let RangeLimit::Micros(m) = limit(&t, hours(30), true) else {
            panic!("expected micros limit");
        };
        assert_eq!(m, i128::from(-7 * 3_600_000_000_i64));
    }

    #[test]
    fn numeric_limits_are_exact() {
        let d = |u: i128, s: u8| crate::numeric::Decimal::from_parts(u, s).expect("decimal");
        let base = WindowKeyValue::Numeric(d(25, 1));
        let l = limit(&base, RangeOffset::Numeric(d(15, 1)), false);
        assert!(within(&WindowKeyValue::Numeric(d(40, 1)), &l, true));
        assert!(!within(&WindowKeyValue::Numeric(d(401, 2)), &l, true));
        let l = limit(&base, RangeOffset::Numeric(d(1, 0)), true);
        assert!(within(&WindowKeyValue::Numeric(d(15, 1)), &l, false));
        assert!(!within(&WindowKeyValue::Numeric(d(149, 2)), &l, false));
    }

    #[test]
    fn first_true_finds_the_partition_point() {
        for len in 0..6usize {
            for at in 0..=len {
                let got = first_true(0, len, |p| Ok(p >= at)).expect("search");
                assert_eq!(got, at, "len={len} at={at}");
            }
        }
    }
}
/// `pick_in_frame`（SQL-64 (3)・Issue #1883）の位置計算の単体テスト。
#[cfg(test)]
mod pick_in_frame_tests {
    use super::*;

    #[test]
    fn empty_frame_and_out_of_range_ordinal_pick_nothing() {
        for pick in [FramePick::First, FramePick::Last, FramePick::Nth(0)] {
            assert_eq!(pick_in_frame(3, 3, &[], pick), None);
            assert_eq!(pick_in_frame(4, 2, &[], pick), None);
        }
        assert_eq!(pick_in_frame(0, 3, &[], FramePick::Nth(3)), None);
        assert_eq!(pick_in_frame(0, 3, &[], FramePick::Nth(usize::MAX)), None);
    }

    #[test]
    fn plain_frame_picks_first_last_nth() {
        assert_eq!(pick_in_frame(2, 6, &[], FramePick::First), Some(2));
        assert_eq!(pick_in_frame(2, 6, &[], FramePick::Last), Some(5));
        assert_eq!(pick_in_frame(2, 6, &[], FramePick::Nth(2)), Some(4));
    }

    #[test]
    fn exclusion_outside_the_frame_changes_nothing() {
        let ex = [(0, 2), (8, 9)];
        assert_eq!(pick_in_frame(2, 6, &ex, FramePick::First), Some(2));
        assert_eq!(pick_in_frame(2, 6, &ex, FramePick::Last), Some(5));
    }

    #[test]
    fn exclusion_at_the_edges_skips_to_the_next_row() {
        assert_eq!(pick_in_frame(0, 4, &[(0, 1)], FramePick::First), Some(1));
        assert_eq!(pick_in_frame(0, 4, &[(3, 4)], FramePick::Last), Some(2));
        // フレーム全体が除外されたら空。
        assert_eq!(pick_in_frame(1, 3, &[(0, 5)], FramePick::First), None);
    }

    #[test]
    fn ties_exclusion_leaves_two_pieces() {
        // フレーム [0, 6)、peer グループ [2, 5) の現在行 3 に対する `EXCLUDE TIES` は 2 と 4 を除く。
        let ex = [(2, 3), (4, 5)];
        assert_eq!(pick_in_frame(0, 6, &ex, FramePick::Nth(2)), Some(3));
        assert_eq!(pick_in_frame(0, 6, &ex, FramePick::Nth(3)), Some(5));
        assert_eq!(pick_in_frame(0, 6, &ex, FramePick::Nth(4)), None);
        assert_eq!(pick_in_frame(0, 6, &ex, FramePick::Last), Some(5));
    }

    #[test]
    fn empty_exclusion_intervals_are_ignored() {
        assert_eq!(
            pick_in_frame(0, 3, &[(2, 2), (1, 0)], FramePick::Last),
            Some(2)
        );
    }
}
