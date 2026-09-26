//! `GROUP BY <TEXT 列>` 集計（複数行結果、TASK-167・SQL-14）の実行本体。
//!
//! 責務境界: [`crate::sql::aggregate::execute_aggregate`] が `BoundAggregate::group_by`
//! を検出した場合にのみ呼ばれる（`GROUP BY` なしの単一行集計は `aggregate.rs` が
//! 引き続き担う）。行の走査・RLS 適用順序（ヘッダのみで可視性判定 → 可視行のみ
//! ヘッダのオフセットを引き継いで本体デコード → `WHERE` → 可視性の再検査 → 集計）は
//! `aggregate.rs` の単一行経路と同一の規約を踏襲する（`.claude/rules/security.md`
//! 「テナント境界（P0）」。スクラッチ再利用による二重デコード排除は Issue #349・
//! Issue #314 の横展開）。
//! **不可視行のグループキーは結果に一切現れない**（他テナントにしか存在しない
//! グループ値からの存在推測を防ぐ。RLS-7・RLS-8 の `GROUP BY` 版）。
//!
//! グループ数・グループキー文字列の累計バイト数・`MIN`/`MAX(<TEXT 列>)` 集計状態
//! （`Accumulator::TextMin`/`TextMax`）の累計バイト数は、それぞれ [`MAX_GROUPS`]・
//! [`MAX_GROUP_KEY_TOTAL_BYTES`]・[`MAX_TEXT_ACCUMULATOR_TOTAL_BYTES`] で頭打ちに
//! し、超過は [`SqlSurfaceError::payload_too_large`]（`54000`）で fail-closed に
//! 拒否する（`.claude/rules/security.md`「不安全な設計｜無制限リソース確保（DoS）」
//! 対応。`TEXT` 値は 1 件あたり最大 4 MiB 許容されるため、件数上限だけでは有界に
//! ならない。単一行集計〔`aggregate.rs`〕は `TextMin`/`TextMax` インスタンスが
//! 項目数分（高々 SELECT リスト長）で頭打ちだが、`GROUP BY` はグループ数倍に
//! なるため別途累計管理が必要。PR #230 codex-review 指摘対応）。
//!
//! 行走査ループの集計表は非 NULL グループ（`string_groups: BTreeMap<String, _>`）と
//! NULL グループ（`null_group: Option<_>`）に分割する（Issue #351）。`String:
//! Borrow<str>` により標準 API のまま借用キー（`&str`）でのルックアップができる
//! ため、既存グループへ累積するだけの行では追加のヒープ確保が発生せず、マップ
//! 探索も `get_mut` 1 回で済む（従来は `contains_key` → `get_mut` の 2 回探索＋
//! 新規グループ挿入時の二重確保だった）。新規グループが発生した行のみ
//! [`check_new_group_budget`] の予算検査を経てからキーを 1 回所有化する
//! （[`new_accumulators`]・[`accumulate_row`] 参照）。FINISH 段では
//! `string_groups` の昇順走査のあとに `null_group` を末尾へ連結することで、
//! 分割前の `GroupKey::Ord`（非 NULL 昇順 → NULL 末尾）と同一の走査順を保つ。

use crate::catalog::{self, TableSchema};
use crate::declarative_filter;
use crate::policy::PolicyContext;
use crate::row_codec;
use crate::sql::aggregate::{
    accumulator_bug, storage_internal, try_clone_str, Accumulator, DecodeTier, ReferencedColumns,
    RowVector,
};
use crate::sql::allowlist::{SqlSurfaceError, MAX_GROUP_BY_COLUMNS};
use crate::sql::exec::{Cell, ColumnMeta, QueryResult, ResultRow};
use crate::sql::expr_program::StackValue;
use crate::sql::parser::{BoundAggregate, OrderTarget, ProjectionColumn};
use crate::sql::udf_call::{self, BinOp, ExprValue};
use crate::storage;
use redb::ReadableTable;
use std::borrow::Borrow;
use std::collections::BTreeMap;

/// `GROUP BY` が生成してよいグループ数の上限（無制限 `BTreeMap` 確保を避ける）。
///
/// `pub`（TASK-186・NOSQL-5）: `crates/wire-server/tests/nosql5_group_by.rs` が
/// この値ちょうど＋1 グループのフィクスチャを組み立てて上限超過（`54000`）を
/// 検証するために参照する（値・挙動そのものは不変）。
pub const MAX_GROUPS: usize = 10_000;

/// グループキー文字列（`Some` 側）が累計で保持してよいバイト数の上限。`TEXT` 列は
/// 1 件あたり最大 4 MiB を許容するため、[`MAX_GROUPS`] 件数だけでは有界にならない。
const MAX_GROUP_KEY_TOTAL_BYTES: usize = 16 * 1024 * 1024;

/// クエリ全体で `MIN`/`MAX(<TEXT 列>)` 集計項目（`Accumulator::TextMin`/`TextMax`）が
/// 保持してよい文字列の累計バイト数の上限。`TEXT` 列は 1 件あたり最大 4 MiB を
/// 許容し、`GROUP BY` は最大 [`MAX_GROUPS`] グループ×集計項目数だけ独立した
/// アキュムレータを保持しうるため、件数上限だけでは有界にならない
/// （[`MAX_GROUP_KEY_TOTAL_BYTES`] と同じ予算規模を採用）。
const MAX_TEXT_ACCUMULATOR_TOTAL_BYTES: usize = 16 * 1024 * 1024;

/// PR #603 codex-review P1 指摘対応: [`accumulate_row`] が TEXT 集計容量超過を
/// 報告する際の固定 detail 文言。索引経路（[`observe_group_enumeration`]・
/// [`observe_candidate_slots_grouped`]）がこの文言かどうかで
/// 「全走査へフォールバックすべき容量超過」（索引の走査順に依存する一時的な
/// 超過）と、それ以外の `SqlSurfaceError`（走査順に依存しない即時失敗）とを
/// 区別するために使う（[`is_text_accumulator_budget_error`] 参照）。
const TEXT_BUDGET_EXCEEDED_DETAIL: &str =
    "GROUP BY TEXT aggregate state exceeds the allowed total size";
/// 同上。`checked_add` のオーバーフロー（実運用では到達しないが `usize` 境界を
/// 明示的に扱うための防御）側の detail 文言。
const TEXT_BUDGET_ACCOUNTING_OVERFLOW_DETAIL: &str =
    "GROUP BY TEXT aggregate size accounting overflowed";
/// PR #1049 レビュー指摘 P0／codex P1 対応: [`ResultBudget`]（[`accumulate_row`]・
/// [`check_new_group_budget`] から呼ばれる）が、グループ数に比例する固定分・
/// グループキー累計バイト数（[`MAX_GROUP_KEY_TOTAL_BYTES`]）・TEXT 集計状態累計
/// バイト数（[`MAX_TEXT_ACCUMULATOR_TOTAL_BYTES`]）の合算を、呼び出し元が指定する結果
/// バイト予算（`execute_grouped_aggregate` の `max_result_bytes`。通常は大きな
/// 既定値、`sql::cursor::CursorStatement::Declare` の内側実行だけが
/// `sql::cursor::MAX_CURSOR_BYTES_PER_SESSION`＝16 MiB）で頭打ちにする際の
/// detail 文言。両者はそれぞれ独立に最大 16 MiB まで許容されるため、合計は
/// 最大 32 MiB に達しうる——`CursorRegistry::declare` が想定する 16 MiB の
/// カーソル容量制限にはならない。[`TEXT_BUDGET_EXCEEDED_DETAIL`] と同じく
/// `MIN`/`MAX(TEXT)` の縮小方向更新を含むため走査順に依存する一時的な超過が
/// ありうる（[`is_text_accumulator_budget_error`] のドキュメント参照）。
const RESULT_BUDGET_EXCEEDED_DETAIL: &str = "GROUP BY result exceeds capacity";
/// [`ResultBudget`] の超過を新規グループ追加時（[`check_new_group_budget`]）に
/// 検出した場合の detail 文言。[`RESULT_BUDGET_EXCEEDED_DETAIL`] と異なり
/// [`is_text_accumulator_budget_error`] の対象に**含めない**（索引経路から全走査への
/// フォールバックを起こさず、どの経路でも同一の即時失敗 `54000` とする）。
///
/// 根拠（PR #1049 レビュー指摘 Cursor Bugbot Medium 対応）: グループ追加時の見積りの
/// うちグループ数・キー累計は単調増加で処理順序に依存しない。`TEXT` 集計状態累計は
/// 処理順序に依存しうるが、`TEXT` の `MIN`/`MAX` を含むクエリは列挙形を使わず
/// （`execute_grouped_aggregate` の `text_min_max_blocks_enumeration`）、候補走査形は
/// 全走査と同一の物理行順で処理するため、いずれの索引経路で超過しても全走査で同じ
/// 時点に超過する——フォールバックしても結果は変わらず全表走査の無駄になるだけ。
const RESULT_BUDGET_GROUPS_EXCEEDED_DETAIL: &str =
    "GROUP BY groups exceed the allowed result capacity";

/// 生成中の結果 1 行（1 グループ）あたりの固定オーバーヘッド見積り（`id`・`score`
/// 相当）。`sql::cursor::estimate_row_bytes` と同じ見積り規約（DoS 対策の概算で
/// あり厳密なメモリ使用量ではない）。
const RESULT_ROW_FIXED_BYTES: usize = 16;
/// 結果セル 1 個あたりの固定見積り（非 `TEXT` の集計値〔整数・浮動小数・日時等〕の
/// 8 バイト、`TEXT` セルの長さ以外の固定分 8 バイト。`sql::cursor::
/// estimate_cell_bytes` と同じ規約）。
const RESULT_CELL_FIXED_BYTES: usize = 8;

/// 生成中の `GROUP BY` 結果全体に対する結果バイト予算（`execute_grouped_aggregate`
/// の `max_result_bytes`）の判定器。
///
/// PR #1049 レビュー指摘（codex P1）対応: 見積りは「グループ数 × 1 グループあたりの
/// 固定分（行オーバーヘッド＋グループキーセル・全集計セルの固定分）」＋グループキー
/// 累計バイト数＋`TEXT` 集計状態累計バイト数。非 `TEXT` の集計値（最大
/// [`MAX_GROUPS`] グループ × 集計項目数）もグループ数に比例する固定分として含める。
/// 判定は新規グループ追加時（[`check_new_group_budget`]）と `TEXT` 集計状態の増加時
/// （[`accumulate_row`]）の双方で行い、どちらの順で増えても生成途中で打ち切る
/// （既存グループへの非 `TEXT` 集計値の更新は固定サイズのため見積りを変えない）。
/// 通常の（カーソル非経由の）呼び出しは十分大きい予算
/// （`sql::aggregate::MAX_AGGREGATE_RESULT_BYTES`）を渡すため既存挙動は変わらない。
#[derive(Debug, Clone, Copy)]
struct ResultBudget {
    max_result_bytes: usize,
    per_group_bytes: usize,
}

impl ResultBudget {
    /// `bound` の結果形状（投影列数・集計項目数・`GROUP BY` キー列数）から
    /// 1 グループあたりの固定分を求める。メモリ上はグループキー
    /// `key_count`（SQL-25 (d) で複数列に一般化）個＋集計項目ごとの
    /// アキュムレータを保持し、結果は投影列数ぶんのセルになるため、両者の
    /// 大きい方で見積もる（`key_count == 1` では従来と同じ見積り値になる）。
    fn new(bound: &BoundAggregate, max_result_bytes: usize) -> Result<Self, SqlSurfaceError> {
        let key_count = bound
            .group_by
            .as_ref()
            .map(|g| g.column_indices.len())
            .unwrap_or(1);
        let cells = bound
            .projection
            .len()
            .max(bound.items.len().saturating_add(key_count));
        let per_group_bytes = cells
            .checked_mul(RESULT_CELL_FIXED_BYTES)
            .and_then(|b| b.checked_add(RESULT_ROW_FIXED_BYTES))
            .ok_or_else(|| {
                SqlSurfaceError::payload_too_large(TEXT_BUDGET_ACCOUNTING_OVERFLOW_DETAIL)
            })?;
        Ok(Self {
            max_result_bytes,
            per_group_bytes,
        })
    }

    /// `group_count` グループ・キー累計 `total_key_bytes`・`TEXT` 集計状態累計
    /// `total_text_bytes` の生成中結果が予算内かを判定する（超過は `detail` を
    /// 持つ `54000`。`TEXT` 増加時は [`RESULT_BUDGET_EXCEEDED_DETAIL`]、新規グループ
    /// 追加時は [`RESULT_BUDGET_GROUPS_EXCEEDED_DETAIL`]）。
    fn check(
        &self,
        group_count: usize,
        total_key_bytes: usize,
        total_text_bytes: usize,
        detail: &'static str,
    ) -> Result<(), SqlSurfaceError> {
        let estimated = group_count
            .checked_mul(self.per_group_bytes)
            .and_then(|b| b.checked_add(total_key_bytes))
            .and_then(|b| b.checked_add(total_text_bytes))
            .ok_or_else(|| {
                SqlSurfaceError::payload_too_large(TEXT_BUDGET_ACCOUNTING_OVERFLOW_DETAIL)
            })?;
        if estimated > self.max_result_bytes {
            return Err(SqlSurfaceError::payload_too_large(detail));
        }
        Ok(())
    }
}

/// PR #603 codex-review P1 指摘対応: `err` が [`accumulate_row`] の TEXT 集計
/// 容量超過（[`TEXT_BUDGET_EXCEEDED_DETAIL`]／[`TEXT_BUDGET_ACCOUNTING_OVERFLOW_DETAIL`]）
/// かどうかを判定する。
///
/// `GROUP BY` を `ScalarIndex` 経由で処理する索引経路（列挙形・候補走査形）は
/// キー順・候補の索引内順序で行を処理するため、全走査（`user_rows/{table}` の
/// 物理行順）と処理順序が異なる。`MIN`/`MAX(TEXT)` の縮小方向更新
/// （[`accumulate_row`] の `before`/`after` 比較）を含む累計バイト数は
/// 非単調（増加も減少もありうる）であり、その一時的な最大値は処理順序に
/// 依存する。そのため、全走査なら成功するクエリが索引経路の処理順序では
/// 一時的に予算を超過し `54000` として失敗しうる——索引選択によってクエリの
/// 成否が変わってはならない（AGENTS.md「公開 API・エラー契約の互換性」）ため、
/// 索引経路の呼び出し元はこの超過を検出したら索引経路の結果を破棄し、全走査
/// （処理順序に依存しない基準実装）へフォールバックする（[`execute_grouped_aggregate`]
/// 参照）。
///
/// 一方、[`check_new_group_budget`] が管理するグループ数・キーバイト数の予算は
/// 加算のみで減算されない（単調増加）ため、その一時的な最大値は最終合計以下に
/// 抑えられ処理順序に依存しない。したがって当該予算超過はこの判定の対象に含めず、
/// 索引経路・全走査のいずれでも同一の即時失敗として扱ってよい。
fn is_text_accumulator_budget_error(err: &SqlSurfaceError) -> bool {
    matches!(
        err,
        SqlSurfaceError::PayloadTooLarge { detail }
            if detail == TEXT_BUDGET_EXCEEDED_DETAIL
                || detail == TEXT_BUDGET_ACCOUNTING_OVERFLOW_DETAIL
                || detail == RESULT_BUDGET_EXCEEDED_DETAIL
    )
}

/// PR #603 codex-review P1 指摘対応: [`observe_candidate_slots_grouped`] 内部
/// （候補走査形。索引の候補順に依存する処理順序を持つ）専用のエラー型。
/// [`is_text_accumulator_budget_error`] による TEXT 集計容量超過とそれ以外の
/// `SqlSurfaceError` を、`?` 演算子で自然に伝播させつつ区別する
/// （[`From<SqlSurfaceError>`] で自動変換されるため、内部実装は既存どおり `?`
/// を使うだけでよい）。
enum GroupAccumulateError {
    /// キー順・候補順に依存する一時的な TEXT 集計容量超過（[`observe_group_enumeration`]
    /// の同名ドキュメント参照）。呼び出し元は索引経路の途中結果を破棄し全走査へ
    /// フォールバックする。
    TextBudgetExceeded,
    /// それ以外の `SqlSurfaceError`（走査順に依存しない即時失敗）。そのまま伝播する。
    Other(SqlSurfaceError),
}

impl From<SqlSurfaceError> for GroupAccumulateError {
    fn from(err: SqlSurfaceError) -> Self {
        if is_text_accumulator_budget_error(&err) {
            GroupAccumulateError::TextBudgetExceeded
        } else {
            GroupAccumulateError::Other(err)
        }
    }
}

/// グループキー（`GROUP BY` 対象列の組の値。SQL-25 (d) で単一列から複数列
/// タプルへ一般化した）。各成分の `None` は NULL 値のグループ（`TEXT` 列の
/// NULL は 1 つのグループへまとめる。PostgreSQL 互換）。`Ord` は成分ごとの
/// 辞書式比較で、各成分は `Some` 同士ならバイト順、`Some` は常に `None` より
/// 小さい（NULL は末尾。既定の昇順ソート・[`crate::sql::exec::ColumnMeta`] へ
/// 渡す前の表示順を決定的にする）。単一成分（`vec![Some(_)]`／`vec![None]`）
/// では旧 `GroupKey(Option<String>)` と完全に同じ順序になる。派生 `Ord`
/// （`Option` は `None` が先頭）とは逆順になるため手動実装する（PR #230
/// codex-review/Bugbot 指摘: 派生 `Ord` のままだと既定順序・`ORDER BY` 未指定時に
/// `NULL` グループが先頭に来て `LIMIT` が意図した先頭の非 `NULL` グループを
/// 取りこぼす）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct GroupKey(Vec<Option<String>>);

/// [`GroupKey`]（所有）と、複数列 `GROUP BY` の行走査ループが構築する借用成分列
/// （[`BorrowedGroupKey`]）を同一の比較規約で扱うためのビュー。`GroupKey::cmp`・
/// [`cmp_group_key_views`] は本トレイトの同じ実装へ委譲するため両者の順序は
/// 構造的に一致する（`Borrow` の契約である「借用後も `Ord` が変わらない」を
/// 保証する）。
///
/// PR #1099 レビュー指摘（Cursor Bugbot・codex-review、複数列 `GROUP BY` 経路）
/// 対応: 単一列経路（`string_groups: BTreeMap<String, _>`。`String: Borrow<str>`）
/// と同様に、複数列経路でも `multi_groups: BTreeMap<GroupKey, _>` を借用キーで
/// 先に検索できるようにする（[`Borrow<dyn GroupKeyView>`] impl 参照）。これにより
/// 既存グループへの累積行では成分の所有化（[`try_clone_str`]）が発生せず、新規
/// グループが確定した行のみ [`check_new_group_budget`] の予算検査を経てから
/// キーを 1 回所有化する。
trait GroupKeyView {
    /// キーの成分数（`GROUP BY` 対象列数）。
    fn len(&self) -> usize;
    /// `i` 番目の成分（`None` は NULL 値のグループ）。範囲外は NULL 相当。
    fn component(&self, i: usize) -> Option<&str>;
}

impl GroupKeyView for GroupKey {
    fn len(&self) -> usize {
        self.0.len()
    }
    fn component(&self, i: usize) -> Option<&str> {
        self.0.get(i).and_then(|c| c.as_deref())
    }
}

/// 行走査ループが構築する借用成分列（各成分は `scanned` から借用した `&str`）。
/// 所有化前に [`GroupKeyView`] 経由で既存グループを検索するための一時ビュー。
struct BorrowedGroupKey<'a>(&'a [Option<&'a str>]);

impl GroupKeyView for BorrowedGroupKey<'_> {
    fn len(&self) -> usize {
        self.0.len()
    }
    fn component(&self, i: usize) -> Option<&str> {
        self.0.get(i).copied().flatten()
    }
}

/// [`GroupKeyView`] 実装同士の辞書式比較（成分ごとに `Some` は常に `None` より
/// 小さい＝NULL は末尾）。所有 [`GroupKey`] 同士の比較（`Ord`）・所有と借用の
/// 比較（`BTreeMap` 探索、`Borrow<dyn GroupKeyView>` 経由）の両方がこの 1 つの
/// 実装に委譲するため、順序が食い違うことはない。
fn cmp_group_key_views(a: &dyn GroupKeyView, b: &dyn GroupKeyView) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let len = a.len().min(b.len());
    for i in 0..len {
        let component_order = match (a.component(i), b.component(i)) {
            (Some(x), Some(y)) => x.cmp(y),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        };
        if component_order != Ordering::Equal {
            return component_order;
        }
    }
    // 成分数は同一クエリ内では常に揃う（`bound.group_by.column_indices` の
    // 宣言列数で固定されるため）。念のため長さの違いも決定的に扱う。
    a.len().cmp(&b.len())
}

impl PartialEq for dyn GroupKeyView + '_ {
    fn eq(&self, other: &Self) -> bool {
        cmp_group_key_views(self, other) == std::cmp::Ordering::Equal
    }
}

impl Eq for dyn GroupKeyView + '_ {}

impl PartialOrd for dyn GroupKeyView + '_ {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for dyn GroupKeyView + '_ {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        cmp_group_key_views(self, other)
    }
}

/// `BTreeMap<GroupKey, _>::get_mut` を所有化前の借用キー（[`BorrowedGroupKey`]）
/// で呼ぶための `Borrow` 実装。`&self` の生存期間のまま `&dyn GroupKeyView` を
/// 返すだけで新たな確保は発生しない。
impl<'a> Borrow<dyn GroupKeyView + 'a> for GroupKey {
    fn borrow(&self) -> &(dyn GroupKeyView + 'a) {
        self
    }
}

impl PartialOrd for GroupKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for GroupKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        cmp_group_key_views(self, other)
    }
}

/// グループ表への新規グループキー追加前に有界性を検査する（[`MAX_GROUPS`]・
/// [`MAX_GROUP_KEY_TOTAL_BYTES`]）。呼び出し元が「このキーは表に存在しない」ことを
/// 確認済みの場合にのみ呼ぶ（既存キーの更新では追加コストが発生しないため呼ばない）。
/// 成功時は `total_key_bytes` へ今回のキー分のバイト数を加算する。加えて、追加後の
/// グループ数・キー累計と現時点の `TEXT` 集計状態累計で生成中の結果全体を
/// [`ResultBudget`] と照合する（PR #1049 レビュー指摘 codex P1 対応）。
///
/// `key_len` はグループキーのバイト数（NULL グループは 0）。Issue #351 で行走査
/// ループが借用キー（`&str`）主体に変わったため、所有 `String`/`Option<String>`
/// ではなくバイト数のみを引数に取る（予算検査の時点ではまだキーを所有化しない）。
fn check_new_group_budget(
    current_group_count: usize,
    total_key_bytes: &mut usize,
    key_len: usize,
    total_text_accumulator_bytes: usize,
    budget: &ResultBudget,
) -> Result<(), SqlSurfaceError> {
    if current_group_count >= MAX_GROUPS {
        return Err(SqlSurfaceError::payload_too_large(
            "GROUP BY result exceeds the allowed number of groups",
        ));
    }
    let next = total_key_bytes.checked_add(key_len).ok_or_else(|| {
        SqlSurfaceError::payload_too_large("GROUP BY key size accounting overflowed")
    })?;
    if next > MAX_GROUP_KEY_TOTAL_BYTES {
        return Err(SqlSurfaceError::payload_too_large(
            "GROUP BY key values exceed the allowed total size",
        ));
    }
    let next_group_count = current_group_count.checked_add(1).ok_or_else(|| {
        SqlSurfaceError::payload_too_large("GROUP BY group count accounting overflowed")
    })?;
    budget.check(
        next_group_count,
        next,
        total_text_accumulator_bytes,
        RESULT_BUDGET_GROUPS_EXCEEDED_DETAIL,
    )?;
    *total_key_bytes = next;
    Ok(())
}

/// 新規グループ 1 件分のアキュムレータ列を確保する（`bound.items` の項目ごとに
/// [`Accumulator::new`]）。既存グループへの累積では呼ばない（グループ発生行のみ
/// のコストに留める。Issue #351）。
fn new_accumulators(
    items: &[crate::sql::parser::BoundAggregateItem],
) -> Result<Vec<Accumulator>, SqlSurfaceError> {
    let mut accs = Vec::new();
    accs.try_reserve_exact(items.len()).map_err(|_| {
        SqlSurfaceError::payload_too_large(
            "aggregate accumulator allocation exceeds available memory",
        )
    })?;
    for item in items {
        accs.push(Accumulator::for_item(item)?);
    }
    Ok(accs)
}

/// 1 行分の値を、対象グループのアキュムレータ列へ反映する（`observe`）と同時に
/// `MIN`/`MAX(<TEXT 列>)` の累計バイト数予算（[`MAX_TEXT_ACCUMULATOR_TOTAL_BYTES`]）
/// を更新する。既存グループ・新規グループどちらの行からも呼ばれる共通 helper
/// （Issue #351 で行ループから抽出。before/after 比較による加算・減算ロジックは
/// 抽出前と完全に同一）。`vector` は呼び出し元が `tier`（[`DecodeTier`]）に応じて
/// 組み立てた行 1 件分の `VECTOR` 列ビュー（Issue #350。embedding 未デコード時は
/// `values: None`）。`expr_scratch` は [`Accumulator::observe`] が内部で
/// `ExprProgram::eval` を呼ぶ際の明示スタック（Issue #353。行に依存する借用を
/// 保持しないため、呼び出し元が行ループの外で 1 回だけ確保したバッファを
/// 使い回せる。`aggregate.rs::execute_aggregate` と同じ方針）。
#[allow(clippy::too_many_arguments)]
fn accumulate_row(
    accs: &mut [Accumulator],
    items: &[crate::sql::parser::BoundAggregateItem],
    id: u64,
    vector: &RowVector<'_>,
    scanned: &[Option<row_codec::ScalarRef<'_>>],
    total_text_accumulator_bytes: &mut usize,
    // PR #1049 レビュー指摘 P0／codex P1 対応: 呼び出し時点のグループ数（この行が
    // 属するグループを含む。新規グループはまだ表へ挿入していなくても数に含める）と
    // グループキー累計バイト数（いずれも読み取り専用。本関数はグループ・キーを
    // 追加しない）。`TEXT` 集計状態の増加時に生成中の結果全体を [`ResultBudget`]
    // と照合するために使う。
    group_count: usize,
    total_key_bytes: usize,
    budget: &ResultBudget,
    expr_scratch: &mut Vec<StackValue>,
    // SQL-25 (c)・TASK-209: `COUNT(DISTINCT)` の中間状態はクエリ全体
    // （全グループ・全項目の合計）で 1 つ。呼び出し元
    // （`execute_grouped_aggregate` の直接呼び出し・`observe_group_slots`・
    // `observe_candidate_slots_grouped_inner` のいずれも同一インスタンスを
    // 使い回す）。
    distinct_budget: &mut crate::sql::distinct::DistinctBudget,
) -> Result<(), SqlSurfaceError> {
    for (accumulator, item) in accs.iter_mut().zip(items) {
        // `MIN`/`MAX(<TEXT 列>)` は 1 グループ・1 項目あたり高々 1 本の
        // `String` を保持するが、`GROUP BY` はグループ数倍に増えるため
        // クエリ全体の累計バイト数を予算管理する（before/after 比較で、
        // 増加方向は加算・縮小方向〔より短い極値への更新〕は減算し、
        // 実際の保持量を正確に反映する）。
        let before = accumulator.text_len();
        let (distinct_entries_before, distinct_bytes_before) = accumulator.distinct_footprint();
        accumulator.observe(&item.input, id, vector, scanned, expr_scratch)?;
        let after = accumulator.text_len();
        let (distinct_entries_after, distinct_bytes_after) = accumulator.distinct_footprint();
        if distinct_entries_after > distinct_entries_before {
            let delta = distinct_bytes_after
                .checked_sub(distinct_bytes_before)
                .ok_or_else(|| {
                    accumulator_bug("COUNT(DISTINCT) footprint bytes decreased unexpectedly")
                })?;
            distinct_budget.charge(delta)?;
        }
        if after > before {
            let delta = after - before;
            *total_text_accumulator_bytes = total_text_accumulator_bytes
                .checked_add(delta)
                .ok_or_else(|| {
                    SqlSurfaceError::payload_too_large(TEXT_BUDGET_ACCOUNTING_OVERFLOW_DETAIL)
                })?;
            if *total_text_accumulator_bytes > MAX_TEXT_ACCUMULATOR_TOTAL_BYTES {
                return Err(SqlSurfaceError::payload_too_large(
                    TEXT_BUDGET_EXCEEDED_DETAIL,
                ));
            }
            // PR #1049 レビュー指摘 P0／codex P1 対応: グループキー累計・TEXT
            // 集計状態累計はそれぞれ独立に [`MAX_GROUP_KEY_TOTAL_BYTES`]・
            // [`MAX_TEXT_ACCUMULATOR_TOTAL_BYTES`]（各 16 MiB）で頭打ちに
            // なるのみで、合計（＋グループ数に比例する非 TEXT 集計値の固定分）は
            // それを超えうる。生成中の結果全体を呼び出し元が指定する予算
            // （[`ResultBudget`]）で追加検査し、`sql::cursor::CursorStatement::
            // Declare` の内側実行では `CursorRegistry::declare` の 16 MiB 判定へ
            // 到達する前に生成中の段階で打ち切る（新規グループ追加時の判定は
            // [`check_new_group_budget`] が担う）。
            budget.check(
                group_count,
                total_key_bytes,
                *total_text_accumulator_bytes,
                RESULT_BUDGET_EXCEEDED_DETAIL,
            )?;
        } else if after < before {
            // MIN/MAX(TEXT) の極値がより短い文字列へ更新された縮小方向。
            // 実際の保持量を正確に反映するため減算する（`checked_sub` の
            // 失敗＝内部不整合は `XX000` の accumulator_bug へ落とし、
            // fail-open にはしない）。減算しないと過去の増加量が
            // 累積し続け、実保持量が予算内でも正常なクエリを
            // 誤って 54000 で拒否してしまう。
            let delta = before - after;
            *total_text_accumulator_bytes = total_text_accumulator_bytes
                .checked_sub(delta)
                .ok_or_else(|| {
                    accumulator_bug("GROUP BY TEXT aggregate size accounting underflowed")
                })?;
        }
    }
    Ok(())
}

/// Issue #475: `WHERE` なしの `GROUP BY` を、`ScalarIndex` が保持する値グループ
/// （[`crate::sql::scalar_index::ScalarIndex::column_groups`]）へそのまま写像
/// する（列挙形）。`WHERE` が無いため候補の再検証は不要——`ScalarIndex::build`
/// は当該列の可視行が持つ非 `NULL` 値を**すべて**索引化する契約（同モジュール
/// ドキュメント参照）であり、一部の値だけを取りこぼして索引を返すことはない。
/// `Ok(false)` は列挙形が使えない（`GROUP BY` キー列が `TEXT` でない・未索引・
/// NULL 補完不能）ことを示し、呼び出し元は全走査へフォールバックする。
/// `MAX_GROUPS`／`MAX_GROUP_KEY_TOTAL_BYTES` の予算超過は（全走査と同じく）
/// `Err`（`54000`）として伝播する——索引が使えたかどうかに関わらずクエリの
/// 容量契約は変えない。ただし `MAX_TEXT_ACCUMULATOR_TOTAL_BYTES` の超過
/// （[`is_text_accumulator_budget_error`]）に限っては、キー順の列挙が全走査の
/// 物理行順と異なる一時的な超過を誤検出しうるため `Err` を伝播せず、
/// 索引経路の途中結果を破棄して `Ok(false)`（全走査へフォールバック）を返す
/// （PR #603 codex-review P1 指摘対応）。
#[allow(clippy::too_many_arguments)]
fn observe_group_enumeration(
    snapshot: &crate::sql::arena_cache::SqlArenaSnapshot,
    index: &crate::sql::scalar_index::ScalarIndex,
    schema: &TableSchema,
    bound: &BoundAggregate,
    referenced: &ReferencedColumns,
    group_by: &crate::sql::parser::BoundGroupBy,
    string_groups: &mut BTreeMap<String, Vec<Accumulator>>,
    null_group: &mut Option<Vec<Accumulator>>,
    total_key_bytes: &mut usize,
    total_text_accumulator_bytes: &mut usize,
    budget: &ResultBudget,
    distinct_budget: &mut crate::sql::distinct::DistinctBudget,
) -> Result<bool, SqlSurfaceError> {
    // 列挙形（[`observe_group_enumeration`]）は単一キー専用（呼び出し元
    // `execute_grouped_aggregate` が `column_indices.len() == 1` の場合のみ
    // 呼ぶ。複数キーは全走査〔[`execute_grouped_aggregate_multi_key`]〕に
    // 一本化する。§計画 3.5）。束縛段（`bind_group_by_clause`）が
    // `column_indices` を必ず 1 件以上で構築するため、空は到達しない想定だが
    // 添字アクセスを避け `.first()` で明示的に扱う。
    let column_index = group_by
        .column_indices
        .first()
        .copied()
        .ok_or_else(|| accumulator_bug("single-key GROUP BY path called with no columns"))?;
    let Some(groups) = index.column_groups(column_index) else {
        return Ok(false);
    };
    let Some(null_slots) = index.slots_without_value(column_index) else {
        return Ok(false);
    };

    // Issue #660 系（索引経路の残存コスト削減）: 集計項目がすべて `COUNT(*)` のとき、
    // 列挙形の各グループの結果は「そのグループに属する可視行の件数」だけで
    // 確定し行の内容を参照しない。`ScalarIndex::column_groups`／
    // `slots_without_value` は当該列の可視行を漏れなく値グループ／NULL 群へ
    // 分割する契約（`sql::scalar_index` モジュールドキュメント・本関数冒頭の
    // 説明参照）であり、かつ本経路は `WHERE` なし（列挙形の前提）・索引↔
    // スナップショット同一性ガード通過済みのため、スロット列の長さがそのまま
    // グループ件数になる（`sql::aggregate::count_star_only` の不変条件 1〜3）。
    // この場合だけ `observe_group_slots`（全スロットの
    // `scan_scalar_columns_masked`）を丸ごと省く。
    let count_star_only = crate::sql::aggregate::count_star_only(&bound.items);

    // `groups`（索引本体への借用イテレータ）はループの間ずっと `index` を借用
    // したままにし、キー・スロットいずれも `check_new_group_budget` の容量検査
    // を通過した分だけ所有データへ複製する（fail-closed 契約。容量超過時は
    // 索引全体を無条件で `to_string`/`to_vec` する既存の infallible な複製を
    // 避け、容量検査より前に確保が起きないようにする）。スロット列
    // （`&[u32]`）は借用のまま [`observe_group_slots`] へ渡せるため複製不要。
    for (value, slots) in groups {
        let current_group_count = string_groups.len() + usize::from(null_group.is_some());
        check_new_group_budget(
            current_group_count,
            total_key_bytes,
            value.len(),
            *total_text_accumulator_bytes,
            budget,
        )?;
        let mut accs = new_accumulators(&bound.items)?;
        if count_star_only {
            observe_group_count_only(slots, &mut accs)?;
            string_groups.insert(try_clone_str(value)?, accs);
            continue;
        }
        match observe_group_slots(
            snapshot,
            slots,
            schema,
            bound,
            referenced,
            &mut accs,
            total_text_accumulator_bytes,
            current_group_count.saturating_add(1),
            *total_key_bytes,
            budget,
            distinct_budget,
        ) {
            Ok(()) => {}
            Err(err) if is_text_accumulator_budget_error(&err) => {
                // PR #603 codex-review P1 指摘対応: キー順の列挙による一時的な
                // TEXT 容量超過の誤検出。ここまでに構築した索引経路の途中結果
                // （このグループを含め）を破棄し、呼び出し元に全走査への
                // フォールバックを促す。SQL-25 (c)・TASK-209:
                // `distinct_budget`（クエリ全体で共有）もここまでの部分計上を
                // 巻き戻す（全走査の再実行は 0 から数え直すため、二重計上を
                // 避ける）。
                string_groups.clear();
                *null_group = None;
                *total_key_bytes = 0;
                *total_text_accumulator_bytes = 0;
                *distinct_budget = crate::sql::distinct::DistinctBudget::new();
                return Ok(false);
            }
            Err(err) => return Err(err),
        }
        string_groups.insert(try_clone_str(value)?, accs);
    }
    if !null_slots.is_empty() {
        let current_group_count = string_groups.len() + usize::from(null_group.is_some());
        check_new_group_budget(
            current_group_count,
            total_key_bytes,
            0,
            *total_text_accumulator_bytes,
            budget,
        )?;
        let mut accs = new_accumulators(&bound.items)?;
        if count_star_only {
            observe_group_count_only(&null_slots, &mut accs)?;
            *null_group = Some(accs);
            return Ok(true);
        }
        match observe_group_slots(
            snapshot,
            &null_slots,
            schema,
            bound,
            referenced,
            &mut accs,
            total_text_accumulator_bytes,
            current_group_count.saturating_add(1),
            *total_key_bytes,
            budget,
            distinct_budget,
        ) {
            Ok(()) => {}
            Err(err) if is_text_accumulator_budget_error(&err) => {
                string_groups.clear();
                *null_group = None;
                *total_key_bytes = 0;
                *total_text_accumulator_bytes = 0;
                *distinct_budget = crate::sql::distinct::DistinctBudget::new();
                return Ok(false);
            }
            Err(err) => return Err(err),
        }
        *null_group = Some(accs);
    }
    Ok(true)
}

/// Issue #660 系（索引経路の残存コスト削減）: `COUNT(*)` のみの列挙形専用。`slots`
/// （そのグループに属する可視行スロットの正確な集合）の件数だけを `accs` へ
/// 加算し、行のデコード（`scan_scalar_columns_masked`）を一切行わない。
/// 適用条件（不変条件）は `sql::aggregate::count_star_only` のドキュメント参照。
fn observe_group_count_only(
    slots: &[u32],
    accs: &mut [Accumulator],
) -> Result<(), SqlSurfaceError> {
    let hits = u64::try_from(slots.len())
        .map_err(|_| accumulator_bug("group slot count does not fit in u64"))?;
    for acc in accs.iter_mut() {
        acc.observe_present_n(hits)?;
    }
    Ok(())
}

/// Issue #475: `slots`（`snapshot` 上の添字。同一グループに属することが呼び
/// 出し元で確定済み）を走査し、`accs`（1 グループ分のアキュムレータ列）へ
/// 累積する。`WHERE` が無い列挙形専用のため候補の再検証は行わない
/// （[`observe_group_enumeration`] のドキュメント参照）。
#[allow(clippy::too_many_arguments)]
fn observe_group_slots(
    snapshot: &crate::sql::arena_cache::SqlArenaSnapshot,
    slots: &[u32],
    schema: &TableSchema,
    bound: &BoundAggregate,
    referenced: &ReferencedColumns,
    accs: &mut [Accumulator],
    total_text_accumulator_bytes: &mut usize,
    // このグループを含むグループ数（[`accumulate_row`] の同名引数参照）。
    group_count: usize,
    total_key_bytes: usize,
    budget: &ResultBudget,
    distinct_budget: &mut crate::sql::distinct::DistinctBudget,
) -> Result<(), SqlSurfaceError> {
    let arena = snapshot.arena();
    let mut expr_scratch: Vec<StackValue> = Vec::new();
    for &slot in slots {
        let slot_idx = usize::try_from(slot)
            .map_err(|_| accumulator_bug("candidate slot does not fit in usize"))?;
        let &id = arena
            .ids()
            .get(slot_idx)
            .ok_or_else(|| accumulator_bug("candidate slot out of bounds (ids)"))?;
        let metadata = snapshot
            .metadata()
            .get(slot_idx)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let scanned = row_codec::scan_scalar_columns_masked(
            schema,
            metadata,
            Some(referenced.scalar_mask()),
        )?;
        let vector = RowVector {
            dim: arena.dim(),
            values: if referenced.needs_embedding() {
                Some(
                    arena
                        .vector(slot_idx)
                        .ok_or_else(|| accumulator_bug("candidate slot out of bounds (vector)"))?,
                )
            } else {
                None
            },
        };
        accumulate_row(
            accs,
            &bound.items,
            id,
            &vector,
            &scanned,
            total_text_accumulator_bytes,
            group_count,
            total_key_bytes,
            budget,
            &mut expr_scratch,
            distinct_budget,
        )?;
    }
    Ok(())
}

/// Issue #475: `ScalarIndex::resolve_candidates` が絞った候補（`WHERE` が
/// 索引対応述語のみで構成される場合の候補走査形）を走査し、既存の全走査ループ
/// と同一の GROUP 段ロジック（借用キー探索 → 新規グループのみ所有化）で
/// `string_groups`／`null_group` へ振り分ける。索引は候補を「絞る」ことしか
/// できないため、`matches_all`・式述語（`classify_scalar_plan` の gate により
/// `expr_filters` は常に `id` 単純比較のみ）を候補行にも再適用する
/// （`aggregate.rs::observe_candidate_slots` と同じ多層防御）。
///
/// `Ok(true)` は候補走査形を最後まで使えたことを示す。`Ok(false)` は
/// [`is_text_accumulator_budget_error`] が指す走査順依存の一時的な TEXT 集計
/// 容量超過を検出したことを示し、`string_groups`／`null_group`／
/// `total_key_bytes`／`total_text_accumulator_bytes` はすべて呼び出し前の
/// 空状態へ戻したうえで返す——呼び出し元は全走査へフォールバックする
/// （PR #603 codex-review P1 指摘対応。走査順に依存しないそれ以外の予算超過
/// （`MAX_GROUPS`／`MAX_GROUP_KEY_TOTAL_BYTES`）は従来どおり `Err`（`54000`）
/// として伝播する）。
#[allow(clippy::too_many_arguments)]
fn observe_candidate_slots_grouped(
    snapshot: &crate::sql::arena_cache::SqlArenaSnapshot,
    slots: &[u32],
    schema: &TableSchema,
    bound: &BoundAggregate,
    referenced: &ReferencedColumns,
    group_by: &crate::sql::parser::BoundGroupBy,
    string_groups: &mut BTreeMap<String, Vec<Accumulator>>,
    null_group: &mut Option<Vec<Accumulator>>,
    total_key_bytes: &mut usize,
    total_text_accumulator_bytes: &mut usize,
    budget: &ResultBudget,
    distinct_budget: &mut crate::sql::distinct::DistinctBudget,
) -> Result<bool, SqlSurfaceError> {
    match observe_candidate_slots_grouped_inner(
        snapshot,
        slots,
        schema,
        bound,
        referenced,
        group_by,
        string_groups,
        null_group,
        total_key_bytes,
        total_text_accumulator_bytes,
        budget,
        distinct_budget,
    ) {
        Ok(()) => Ok(true),
        Err(GroupAccumulateError::TextBudgetExceeded) => {
            string_groups.clear();
            *null_group = None;
            *total_key_bytes = 0;
            *total_text_accumulator_bytes = 0;
            // SQL-25 (c)・TASK-209: 全走査への退避時は `distinct_budget` も
            // 巻き戻す（`observe_group_enumeration` と同じ理由）。
            *distinct_budget = crate::sql::distinct::DistinctBudget::new();
            Ok(false)
        }
        Err(GroupAccumulateError::Other(err)) => Err(err),
    }
}

/// [`observe_candidate_slots_grouped`] の実処理本体。`?` は
/// [`GroupAccumulateError::from`] により `SqlSurfaceError` から自動変換される。
#[allow(clippy::too_many_arguments)]
fn observe_candidate_slots_grouped_inner(
    snapshot: &crate::sql::arena_cache::SqlArenaSnapshot,
    slots: &[u32],
    schema: &TableSchema,
    bound: &BoundAggregate,
    referenced: &ReferencedColumns,
    group_by: &crate::sql::parser::BoundGroupBy,
    string_groups: &mut BTreeMap<String, Vec<Accumulator>>,
    null_group: &mut Option<Vec<Accumulator>>,
    total_key_bytes: &mut usize,
    total_text_accumulator_bytes: &mut usize,
    budget: &ResultBudget,
    distinct_budget: &mut crate::sql::distinct::DistinctBudget,
) -> Result<(), GroupAccumulateError> {
    let arena = snapshot.arena();
    let mut expr_scratch: Vec<StackValue> = Vec::new();
    'candidates: for &slot in slots {
        let slot_idx = usize::try_from(slot)
            .map_err(|_| accumulator_bug("candidate slot does not fit in usize"))?;
        let &id = arena
            .ids()
            .get(slot_idx)
            .ok_or_else(|| accumulator_bug("candidate slot out of bounds (ids)"))?;
        let metadata = snapshot
            .metadata()
            .get(slot_idx)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let scanned =
            row_codec::scan_scalar_columns_masked(schema, metadata, Some(referenced.scalar_mask()))
                .map_err(SqlSurfaceError::from)?;
        if !declarative_filter::matches_all(&bound.metadata_filters, &scanned) {
            continue;
        }
        for (expr, program) in bound.expr_filters.iter().zip(&bound.expr_filter_programs) {
            let embedding: &[f32] = if udf_call::references_embedding(expr) {
                arena
                    .vector(slot_idx)
                    .ok_or_else(|| accumulator_bug("candidate slot out of bounds (vector)"))?
            } else {
                &[]
            };
            match program.eval(id, embedding, &mut expr_scratch)? {
                ExprValue::Bool(true) => {}
                ExprValue::Bool(false) => continue 'candidates,
                _ => {
                    return Err(GroupAccumulateError::Other(SqlSurfaceError::invalid_input(
                        "WHERE expression did not evaluate to a boolean",
                    )))
                }
            }
        }
        // TASK-208・Issue #912: `classify_scalar_plan` は OR 群を含む述語を常に
        // `PlainScan` へ縮退させるため通常到達しないが、多層防御として評価する
        // （`aggregate::observe_candidate_slots` と同じ判断）。
        for group in &bound.or_filters {
            let group_embedding: &[f32] = if group.references_embedding() {
                arena
                    .vector(slot_idx)
                    .ok_or_else(|| accumulator_bug("candidate slot out of bounds (vector)"))?
            } else {
                &[]
            };
            if !group.matches(
                &scanned,
                id,
                group_embedding,
                arena.dim() as usize,
                &mut expr_scratch,
            )? {
                continue 'candidates;
            }
        }

        // 候補走査形（[`observe_candidate_slots_grouped`]）も列挙形と同じく
        // 単一キー専用（呼び出し元の呼び分けは [`observe_group_enumeration`]
        // と同じ）。
        let column_index =
            group_by.column_indices.first().copied().ok_or_else(|| {
                accumulator_bug("single-key GROUP BY path called with no columns")
            })?;
        // GROUP BY キー列は束縛段（`sql::parser::bind_group_by_clause`）で TEXT
        // 列に限定済み（BOOLEAN 列は `22000` で拒否）のため常に `Text` のはずだが、
        // untrusted な格納済みデータに由来する不変条件のため念のため
        // fail-closed に扱う（`as_text()` が `None` を返す＝NULL 相当として扱う）。
        let key_value = scanned
            .get(column_index)
            .copied()
            .flatten()
            .and_then(|v| v.as_text());
        let vector = RowVector {
            dim: arena.dim(),
            values: if referenced.needs_embedding() {
                Some(
                    arena
                        .vector(slot_idx)
                        .ok_or_else(|| accumulator_bug("candidate slot out of bounds (vector)"))?,
                )
            } else {
                None
            },
        };
        let total_group_count = string_groups.len() + usize::from(null_group.is_some());
        match key_value {
            Some(key_str) => {
                if let Some(accs) = string_groups.get_mut(key_str) {
                    accumulate_row(
                        accs,
                        &bound.items,
                        id,
                        &vector,
                        &scanned,
                        total_text_accumulator_bytes,
                        total_group_count,
                        *total_key_bytes,
                        budget,
                        &mut expr_scratch,
                        distinct_budget,
                    )?;
                } else {
                    check_new_group_budget(
                        total_group_count,
                        total_key_bytes,
                        key_str.len(),
                        *total_text_accumulator_bytes,
                        budget,
                    )?;
                    let mut accs = new_accumulators(&bound.items)?;
                    accumulate_row(
                        &mut accs,
                        &bound.items,
                        id,
                        &vector,
                        &scanned,
                        total_text_accumulator_bytes,
                        total_group_count.saturating_add(1),
                        *total_key_bytes,
                        budget,
                        &mut expr_scratch,
                        distinct_budget,
                    )?;
                    string_groups.insert(try_clone_str(key_str)?, accs);
                }
            }
            None => {
                if null_group.is_none() {
                    check_new_group_budget(
                        total_group_count,
                        total_key_bytes,
                        0,
                        *total_text_accumulator_bytes,
                        budget,
                    )?;
                    *null_group = Some(new_accumulators(&bound.items)?);
                }
                // NULL グループは直前で存在が確定しているため、グループ数は
                // 非 NULL グループ数＋1。
                let group_count = string_groups.len().saturating_add(1);
                let accs = null_group.as_mut().ok_or_else(|| {
                    accumulator_bug("null group entry disappeared after insertion")
                })?;
                accumulate_row(
                    accs,
                    &bound.items,
                    id,
                    &vector,
                    &scanned,
                    total_text_accumulator_bytes,
                    group_count,
                    *total_key_bytes,
                    budget,
                    &mut expr_scratch,
                    distinct_budget,
                )?;
            }
        }
    }
    Ok(())
}

/// `Cell::Integer`（`u64`。`COUNT`/`SUM` 等の集計結果で `2^53` を超えうる）と
/// HAVING リテラル（`f64`。構文段 `parse_number_literal` が非有限値を拒否済み）
/// を精度損失なく比較し、両者の大小関係を返す（PR #230 codex-review 指摘対応:
/// 以前は `Cell::Integer` を無条件に `f64` へキャストしていたため、`2^53` 超の
/// 集計値が丸められ `HAVING` の等号・不等号比較が誤判定しうた）。
fn cmp_integer_to_literal(n: u64, literal: f64) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    if literal.is_nan() {
        // 到達しない想定（構文段が非有限値を拒否済み）。fail-closed に「常に
        // 不一致」となるよう Equal 以外を返す（呼び出し元の Eq 判定が false
        // になれば十分なため、方向は問わない）。
        return Ordering::Greater;
    }
    if literal < 0.0 {
        // `u64` は常に 0 以上のため、負のリテラルより常に大きい。
        return Ordering::Greater;
    }
    if literal >= 18_446_744_073_709_551_616.0 {
        // 2^64（`u64` の表現域の上限超）。`n` は常にこれより小さい。
        return Ordering::Less;
    }
    // 上の範囲チェックにより `literal.floor()` は [0, 2^64) に収まるため、
    // `as u64` は精度・範囲の両面で安全（Rust の float→int キャストは
    // 飽和変換であり未定義動作にならない）。
    let floor_u64 = literal.floor() as u64;
    match n.cmp(&floor_u64) {
        Ordering::Equal if literal.fract() != 0.0 => {
            // n == floor(literal) だが literal 自体は非整数 → 実際には n < literal。
            Ordering::Less
        }
        other => other,
    }
}

/// `HAVING`/`WHERE` 相当ではなく、完了済みグループの集計結果 [`Cell`] と数値
/// リテラルを比較する（TASK-167・SQL-14）。`Cell::Null`（空グループはあり得ないが
/// `SUM`/`MIN`/`MAX` 等の空集合契約由来で `NULL` になりうる）との比較は常に偽
/// （PostgreSQL の `NULL` 比較契約と同じ）。`Cell::Integer` は
/// [`cmp_integer_to_literal`] で精度損失なく比較する（`SUM(id)` 等 `2^53` を
/// 超えうる値を無条件に `f64` へキャストしない）。`Cell::Float` は `total_cmp`
/// 相当の通常比較（非有限値は [`Accumulator`] 側が既に拒否済みのため到達しない）。
fn having_matches(cell: &Cell, op: BinOp, literal: f64) -> bool {
    match cell {
        Cell::Integer(n) => {
            use std::cmp::Ordering;
            let ord = cmp_integer_to_literal(*n, literal);
            match op {
                BinOp::Gt => ord == Ordering::Greater,
                BinOp::Lt => ord == Ordering::Less,
                BinOp::Ge => ord != Ordering::Less,
                BinOp::Le => ord != Ordering::Greater,
                BinOp::Eq => ord == Ordering::Equal,
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => false,
            }
        }
        Cell::Float(f) => match op {
            BinOp::Gt => *f > literal,
            BinOp::Lt => *f < literal,
            BinOp::Ge => *f >= literal,
            BinOp::Le => *f <= literal,
            BinOp::Eq => *f == literal,
            // 構文段（`allowlist::Parser::expect_cmp_op`）が算術演算子を HAVING の
            // 比較演算子として構造上生成しないため到達しない。
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => false,
        },
        // Issue #892（D8）: `SUM`/`AVG`/`MIN`/`MAX(<INTEGER>/<BIGINT>)` の結果
        // （`Cell::SignedInteger`）。`Cell::Integer`（`u64`。疑似列 `id`・
        // `COUNT`）とは符号付き/符号なしの境界が異なるため、専用の比較関数
        // （[`cmp_signed_to_literal`]）で `f64` リテラルと厳密に比較する。
        Cell::SignedInteger(n) => {
            use std::cmp::Ordering;
            let ord = cmp_signed_to_literal(*n, literal);
            match op {
                BinOp::Gt => ord == Ordering::Greater,
                BinOp::Lt => ord == Ordering::Less,
                BinOp::Ge => ord != Ordering::Less,
                BinOp::Le => ord != Ordering::Greater,
                BinOp::Eq => ord == Ordering::Equal,
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => false,
            }
        }
        // 束縛段（`sql::parser::check_having_target_is_numeric`。Issue #892・
        // D8）が TEXT・NUMERIC 型の集計結果・`DATE`/`TIMESTAMP` の `MIN`/`MAX`
        // を HAVING の対象として拒否済みのため到達しない（`f64` リテラルとの
        // 厳密な数値比較に意味論が無いため）。ARRAY/BYTEA/JSON/UUID も同様に
        // 集計対象外。fail-closed に「不一致」として扱う。
        Cell::Null
        | Cell::Text(_)
        | Cell::Vector(_)
        | Cell::Bool(_)
        | Cell::Date(_)
        | Cell::Timestamp(_)
        | Cell::Array(_)
        | Cell::Bytes(_)
        | Cell::Json(_)
        | Cell::Numeric(_)
        | Cell::Uuid(_) => false,
    }
}

/// `Cell::SignedInteger`（`i64`）と `HAVING` の `f64` リテラルを厳密に比較する
/// （Issue #892・D8）。[`cmp_integer_to_literal`]（`u64` 版）と同じ規約
/// （`literal` が非有限・範囲外・非整数の場合の扱い）を符号付きへ拡張したもの。
fn cmp_signed_to_literal(n: i64, literal: f64) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    if literal.is_nan() {
        // `cmp_integer_to_literal` と同じ fail-closed 方針: `Eq` が false に
        // なれば十分なため、`Equal` 以外であれば方向は問わない。
        return Ordering::Greater;
    }
    // `i64` の表現域は `[-2^63, 2^63 - 1]`。範囲外のリテラルは符号だけで確定する。
    if literal < -9_223_372_036_854_775_808.0 {
        return Ordering::Greater;
    }
    if literal >= 9_223_372_036_854_775_808.0 {
        return Ordering::Less;
    }
    // 上の範囲チェックにより `literal.floor()` は `i64` の表現域に収まる。
    let floor_i64 = literal.floor() as i64;
    match n.cmp(&floor_i64) {
        Ordering::Equal if literal.fract() != 0.0 => {
            // n == floor(literal) だが literal 自体は非整数 → 実際には n < literal。
            Ordering::Less
        }
        other => other,
    }
}

/// `ORDER BY` 対象 1 つの並び替えキーのうち、非 `NULL` 値どうしの比較のみを行う
/// （`Cell`（集計結果）を共通の [`Ordering`](std::cmp::Ordering) へ写像する）。
/// `NULL` 配置（常に末尾）の判定は呼び出し元 [`order_with_nulls_last`] が方向反転
/// より外側で行うため、ここでは非 `NULL` 値どうしの大小関係のみを返す（PR #230
/// codex-review 指摘: 以前は `Cell::Null` の末尾配置を含めた `Ordering` 全体を
/// `ORDER BY ... DESC` で `.reverse()` していたため、`NULL` が先頭に来て `LIMIT`
/// が非 `NULL` グループを取りこぼしていた）。
fn cmp_cell_values(a: &Cell, b: &Cell) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Cell::Integer(x), Cell::Integer(y)) => x.cmp(y),
        (Cell::Integer(x), Cell::Float(y)) => (*x as f64).total_cmp(y),
        (Cell::Float(x), Cell::Integer(y)) => x.total_cmp(&(*y as f64)),
        (Cell::Float(x), Cell::Float(y)) => x.total_cmp(y),
        (Cell::Text(x), Cell::Text(y)) => x.cmp(y),
        // Issue #892（D9）: `ORDER BY` が新スカラー型の集計結果を並べ替える際に
        // 使う。`ORDER BY` 対象は同一集計項目（`OrderTarget::Aggregate`）の
        // 複数グループ分の結果であり、`Cell` の variant は常に揃っている
        // （束縛段が集計関数・入力列型から一意に決まる結果型を割り当てるため）。
        (Cell::SignedInteger(x), Cell::SignedInteger(y)) => x.cmp(y),
        (Cell::Date(x), Cell::Date(y)) => x.cmp(y),
        (Cell::Timestamp(x), Cell::Timestamp(y)) => x.cmp(y),
        // `NUMERIC` は同一列由来なら常に同じ scale を持つ（`Accumulator`
        // 各 variant が列の scale をそのまま保持する契約）。scale が一致しない
        // 組み合わせは到達しない想定の防御的フォールバックとして Equal を返す
        // （並び替え全体を破綻させない）。
        (Cell::Numeric(x), Cell::Numeric(y)) if x.scale() == y.scale() => {
            x.unscaled().cmp(&y.unscaled())
        }
        // `Cell::Null` は呼び出し元が別途処理するため、ここへ渡ってきても
        // （防御的フォールバックとして）到達しない想定。型不一致も同様に
        // Equal を返す（並び替え全体が破綻しないようにする防御的フォールバック）。
        _ => Ordering::Equal,
    }
}

/// `ORDER BY` の並び順を、`NULL` 配置（常に末尾）を [`BoundOrderBy::descending`]
/// による方向反転の外側で確定させたうえで返す（PR #230 codex-review 指摘対応）。
/// `a_is_null`/`b_is_null` は比較対象（`GroupKey` の `None` または
/// `Cell::Null`）が `NULL` かどうか、`value_cmp` は両者が非 `NULL` の場合の
/// 大小関係（[`cmp_cell_values`] 等）。`DESC` 指定時も `NULL` は常に末尾に残る
/// （`GroupKey`/[`cmp_order_value`] 系がこれまで守ってきた既定順序規約と同じ）。
fn order_with_nulls_last(
    a_is_null: bool,
    b_is_null: bool,
    value_cmp: std::cmp::Ordering,
    descending: bool,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a_is_null, b_is_null) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => {
            if descending {
                value_cmp.reverse()
            } else {
                value_cmp
            }
        }
    }
}

/// codex-review P1 指摘（PR #603）: `bound.items` に `MIN`/`MAX(<TEXT 列>)`
/// 集計が 1 つでも含まれるかを判定する。列挙形（[`observe_group_enumeration`]）
/// はこの判定が真の場合、[`execute_grouped_aggregate`] から呼ばれない
/// （下記「列挙形を使わない理由」参照）。`pub(crate)`: `sql::aggregate::
/// classify_aggregate_access`（Issue #922・SQL-27の EXPLAIN 静的判定）が
/// executor と同じ単一情報源として本関数を共有する。
pub(crate) fn has_text_min_max_aggregate(items: &[crate::sql::parser::BoundAggregateItem]) -> bool {
    items.iter().any(|item| {
        matches!(
            item.func,
            crate::sql::allowlist::AggregateFunc::Min | crate::sql::allowlist::AggregateFunc::Max
        ) && matches!(
            item.input,
            crate::sql::parser::AggregateInput::TextColumn(_)
        )
    })
}

/// [`BoundAggregate`]（`group_by` が `Some` であることを前提。呼び出し元
/// [`crate::sql::aggregate::execute_aggregate`] が判定済み）を実行し、複数行の
/// [`QueryResult`] を返す（TASK-167・SQL-14）。RLS 適用順序・行走査は
/// `aggregate.rs::execute_aggregate` の単一行経路と同一の規約
/// （ヘッダのみで可視性判定 → 可視行のみヘッダのオフセットを引き継いで本体デコード
/// → `WHERE` → 可視性再検査）を独立して踏襲する（責務分離のためモジュールを分けた
/// ことによる意図的な複製。変更する際は両モジュールの規約を揃えること）。
pub(crate) fn execute_grouped_aggregate(
    read_txn: &redb::ReadTransaction,
    ctx: &PolicyContext,
    schema: &TableSchema,
    bound: &BoundAggregate,
    // PR #1049 レビュー指摘 P0／codex P1 対応: 生成中の結果全体（グループ数に
    // 比例する固定分＋グループキー累計＋TEXT 集計状態累計。[`ResultBudget`]）に
    // 対する結果バイト予算（[`RESULT_BUDGET_EXCEEDED_DETAIL`]・
    // `aggregate.rs::execute_aggregate_with_cache` ドキュメント参照）。通常の
    // （カーソル非経由の）呼び出し元は [`crate::sql::aggregate::
    // MAX_AGGREGATE_RESULT_BYTES`] を渡し、`sql::cursor::CursorStatement::
    // Declare` の内側実行だけがより小さい [`crate::sql::cursor::
    // MAX_CURSOR_BYTES_PER_SESSION`] を渡す。
    max_result_bytes: usize,
    // Issue #475: `sql::scalar_index::ScalarIndex` 経由の候補削減・キー列挙。
    // `WHERE` なしの `GROUP BY`（列挙形）・索引対応述語のみの `WHERE` を持つ
    // `GROUP BY`（候補走査形）に限り消費する（`aggregate.rs::
    // execute_aggregate_with_cache` から引き継ぐ。詳細はモジュールドキュメント
    // 「Issue #475」節参照）。
    arena_cache: Option<crate::sql::arena_cache::ArenaCacheAccess<'_>>,
    scalar_cache: Option<crate::sql::scalar_index::ScalarCacheAccess<'_>>,
) -> Result<QueryResult, SqlSurfaceError> {
    let group_by = bound
        .group_by
        .as_ref()
        .ok_or_else(|| accumulator_bug("execute_grouped_aggregate called without a GROUP BY"))?;

    let expected_dim = schema.vector_dim();

    // Issue #350: `GROUP BY` キー列（`group_by.column_indices`。SQL-25 (d) で
    // 複数列へ一般化）は必ず参照するため `extra_scalar_indices` へ渡す。
    // `GROUP BY` は複数グループの走査を要するため `aggregate.rs::
    // DecodeTier::Fast`（ヘッダのみ）は選ばず、embedding 参照の有無だけで
    // `DimAndScalar`／`Embedding` の 2 段階を切り替える。
    let referenced = ReferencedColumns::derive(
        schema,
        &bound.items,
        &bound.metadata_filters,
        &bound.expr_filters,
        &bound.or_filters,
        &group_by.column_indices,
    );
    let tier = if referenced.needs_embedding() {
        DecodeTier::Embedding
    } else {
        DecodeTier::DimAndScalar
    };

    // 単一キー（`column_indices.len() == 1`）は既存の索引経路・
    // `string_groups`／`null_group` 分割（Issue #351）をそのまま使う。複数キー
    // （SQL-25 (d)）は全走査限定の `multi_groups: BTreeMap<GroupKey, _>` に
    // 一本化する（§計画 3.5「複数列経路は全走査のみ」）。
    let key_count = group_by.column_indices.len();
    // 集計表を非 NULL（`string_groups`）と NULL（`null_group`）に分割する
    // （Issue #351）。`string_groups: BTreeMap<String, _>` は `String: Borrow<str>`
    // により `get_mut(&str)` の借用キー検索が標準 API のまま可能で、既存グループ
    // への累積では追加のヒープ確保・二重探索が発生しない。索引経路（Issue #475）・
    // 全走査経路のいずれも同じ変数へ書き込む共有の集計表（単一キー限定）。
    let mut string_groups: BTreeMap<String, Vec<Accumulator>> = BTreeMap::new();
    let mut null_group: Option<Vec<Accumulator>> = None;
    // 複数キー（`key_count >= 2`）専用の集計表。索引経路を使わない全走査のみが
    // 書き込む。
    let mut multi_groups: BTreeMap<GroupKey, Vec<Accumulator>> = BTreeMap::new();
    let mut total_key_bytes: usize = 0;
    let mut total_text_accumulator_bytes: usize = 0;
    // PR #1049 レビュー指摘 codex P1 対応: 生成中の結果全体に対する予算判定器
    // （[`ResultBudget`]）。索引経路・全走査経路の双方が共有する。
    let budget = ResultBudget::new(bound, max_result_bytes)?;
    // SQL-25 (c)・TASK-209: `COUNT(DISTINCT)` の中間状態はクエリ全体
    // （全グループ・全項目の合計）で 1 つ。索引経路が
    // `is_text_accumulator_budget_error` で全走査へ退避する際は呼び出し先
    // （`observe_group_enumeration`・`observe_candidate_slots_grouped`）が
    // このインスタンスを空へ巻き戻すため、下の全走査はそのまま同じ変数を
    // 使い回せる（二重計上しない）。
    let mut distinct_budget = crate::sql::distinct::DistinctBudget::new();

    // Issue #475: `WHERE` なしの `GROUP BY`（列挙形。`ScalarIndex::column_groups`/
    // `slots_without_value` で索引済みの値ごとにグループを直接構築する）、また
    // 索引対応述語のみの `WHERE` を持つ `GROUP BY`（候補走査形。
    // `resolve_candidates` の候補を読みながらグループへ振り分ける）のいずれかに
    // 該当する場合、`user_rows/{table}` の全行走査を候補削減へ置き換える。
    // `VECTOR` 列を持たないテーブル（SQL-13）・`GROUP BY` キー列が `TEXT` でない
    // か未索引・索引の構築/選択度が悪い等、あらゆる縮退は「使えなかった」
    // として以下の全走査（`used_index_path == false`）へフォールバックするだけで
    // クエリの正しさに影響しない（fail-closed。`aggregate.rs` モジュール
    // ドキュメント「Issue #475」節と同じ設計）。
    let mut used_index_path = false;
    if key_count != 1 {
        // `ScalarIndex::column_groups`／`resolve_candidates` 経由の索引経路は
        // 単一キー専用（`sql::scalar_index::ScalarIndex::column_groups` の
        // 契約）。複数列 `GROUP BY`（SQL-25 (d)）は全走査に一本化するため
        // （§計画 3.5）、使わない索引スナップショットを cold cache で構築
        // しない（`text_min_max_blocks_enumeration` 分岐と同じ判断）。
        if let Some(scalar_access) = scalar_cache.as_ref() {
            scalar_access.cache.record_aggregate_plain_scan_fallback();
        }
    } else if let (Some(expected_dim_value), Some(arena_access), Some(scalar_access)) =
        (expected_dim, arena_cache.as_ref(), scalar_cache.as_ref())
    {
        // TASK-208・Issue #912: `or_filters` を含めないと `WHERE a OR b` だけの
        // `GROUP BY`（`metadata_filters`／`expr_filters` は両方空）が
        // 「WHERE なし」と誤判定され、列挙形（`ScalarIndex::column_groups`。
        // フィルタを一切適用しない）へ流れて OR 条件が黙って無視される
        // fail-open のバグになる（security.md「不安全な設計」対応）。
        let where_less = !bound.has_where_filters();
        let scalar_shape = crate::sql::scalar_plan::ScalarShapeInput {
            scalar_prefilter: true,
            metadata_filters: &bound.metadata_filters,
            expr_filters: &bound.expr_filters,
            or_filters: &bound.or_filters,
        };
        let candidate_walk = !where_less
            && crate::sql::scalar_plan::classify_scalar_plan(&scalar_shape)
                != crate::sql::scalar_plan::ScalarPlan::PlainScan;
        // codex-review P1 指摘（PR #603）「列挙形を使わない理由」: 列挙形は
        // グループ（値）ごとに全スロットをまとめて処理するため、あるグループの
        // 処理が完了するたびに `MIN`/`MAX(<TEXT 列>)` の一時的な累計バイト数が
        // 縮小されうる。そのため「あるグループの大きい値が他グループの大きい値と
        // 時間的に重なって積み上がる」全走査の物理行順ピークを、列挙形の処理
        // 順序では決して再現できない場合がある（例: 6 グループ、各グループが
        // 大きい値の行と小さい値の行から成るとき、全走査は最初の数グループの
        // 大きい値が積み上がった時点で予算超過するが、列挙形はグループ単位で
        // 直ちに縮小するため超過を一度も観測しない）。
        // `observe_group_enumeration` 内の `is_text_accumulator_budget_error`
        // による捕捉・フォールバック（PR #603 で追加）は「索引経路の処理順序
        // でのみ超過を検出したケース」しか救えず、この「全走査なら超過するが
        // 索引経路では超過を検出できないケース」は救えない。索引が使えるか
        // どうかで `54000` の成否が変わるのは公開 API・エラー契約の互換性に
        // 反するため（AGENTS.md）、TEXT `MIN`/`MAX` を含む場合は列挙形を最初
        // から使わず全走査（物理行順）へ委ねる。候補走査形
        // （`observe_candidate_slots_grouped`）は `resolve_candidates` が
        // スロット昇順（＝物理行順）を維持し全走査と同一順序で処理するため
        // 対象外。
        let text_min_max_blocks_enumeration =
            where_less && has_text_min_max_aggregate(&bound.items);
        if text_min_max_blocks_enumeration {
            // Cursor Bugbot Medium 指摘（PR #603）: 上記の理由で列挙形を
            // 使わないと事前に確定しているにもかかわらず
            // `ensure_scalar_index_snapshot` を呼ぶと、cold cache では
            // `capture_scalar_index_snapshot` が全可視行の embedding を
            // デコードし使われない `ScalarIndex` を構築してから（無駄な
            // コスト）結局全走査へ落ちてしまう。この分岐へ来た時点で列挙形は
            // 確実に使わないため、索引スナップショットの用意自体を試みない。
            scalar_access.cache.record_aggregate_plain_scan_fallback();
        } else if where_less || candidate_walk {
            match crate::sql::aggregate::ensure_scalar_index_snapshot(
                read_txn,
                ctx,
                schema,
                &bound.table,
                expected_dim_value,
                arena_access,
                scalar_access,
            ) {
                Some((snapshot, index)) => {
                    if where_less {
                        used_index_path = observe_group_enumeration(
                            &snapshot,
                            &index,
                            schema,
                            bound,
                            &referenced,
                            group_by,
                            &mut string_groups,
                            &mut null_group,
                            &mut total_key_bytes,
                            &mut total_text_accumulator_bytes,
                            &budget,
                            &mut distinct_budget,
                        )?;
                    } else {
                        let id_preds: Vec<crate::sql::scalar_plan::IdPredicate> = bound
                            .expr_filters
                            .iter()
                            .filter_map(crate::sql::scalar_plan::id_predicate_from_expr)
                            .collect();
                        // 数値・日時・`NUMERIC`・`UUID` 列の範囲述語
                        // （`FilterOp::TypedCompare`。Issue #891・TASK-199 で
                        // production 結線済み）は `bound.metadata_filters` に
                        // 混在したまま渡り、`ScalarIndex::candidates_for` が
                        // 内部で振り分ける（Issue #893 production 接続）。
                        if let crate::sql::scalar_index::CandidateResolution::Use(slots) =
                            index.resolve_candidates(&bound.metadata_filters, &id_preds)
                        {
                            used_index_path = observe_candidate_slots_grouped(
                                &snapshot,
                                &slots,
                                schema,
                                bound,
                                &referenced,
                                group_by,
                                &mut string_groups,
                                &mut null_group,
                                &mut total_key_bytes,
                                &mut total_text_accumulator_bytes,
                                &budget,
                                &mut distinct_budget,
                            )?;
                        }
                    }
                    if used_index_path {
                        scalar_access.cache.record_aggregate_index_scan();
                    } else {
                        scalar_access.cache.record_aggregate_plain_scan_fallback();
                    }
                }
                None => {
                    scalar_access.cache.record_aggregate_plain_scan_fallback();
                }
            }
        }
    }

    // 可視行ごとの embedding デコード先スクラッチバッファ（Issue #349・Issue #314
    // 横展開。`aggregate.rs::execute_aggregate` と同じ方針）。
    let mut embedding_scratch: Vec<f32> = Vec::new();
    // Issue #353・PR #373 codex-review 指摘対応: `ExprProgram::eval` の明示
    // スタック。[`StackValue`] は行 `embedding` への借用を保持しないため、
    // `embedding_scratch` を毎行 `&mut` で上書きデコードするこのループの外でも
    // 1 回だけ確保し使い回せる（`aggregate.rs::execute_aggregate` と同じ方針）。
    let mut expr_scratch: Vec<StackValue> = Vec::new();

    if !used_index_path {
        let row_table_name = catalog::user_rows_table_name(&bound.table);
        let table = match read_txn.open_table(catalog::user_rows_table_def(&row_table_name)) {
            Ok(t) => Some(t),
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(e) => {
                return Err(SqlSurfaceError::Internal {
                    detail: format!(
                        "aggregate row scan failed: {}",
                        catalog::map_row_table_error(e)
                    ),
                })
            }
        };

        if let Some(table) = table {
            'rows: for entry in table.iter().map_err(storage_internal)? {
                let (k, v) = entry.map_err(storage_internal)?;
                let (key_tenant, id) = k.value();
                let buf = v.value();

                // RLS 段（無条件・デコード前）: `aggregate.rs` の単一行経路と同一順序。
                // `offset` は本体デコードの再開位置（Issue #349: ヘッダの二重デコード
                // 排除。`aggregate.rs::execute_aggregate` のドキュメント参照）。
                let (tenant_id, visibility, offset) =
                    storage::decode_row_header(buf).map_err(storage_internal)?;
                if !ctx.is_visible(tenant_id, visibility) {
                    continue;
                }

                // 可視行・常に: TABLE-12 のキー/ヘッダ tenant 整合検査（`aggregate.rs`
                // と同一の切り出しヘルパを使う。Issue #350）。従来
                // `storage::decode_row_for_key` の内部検査だったものを明示比較へ
                // 移設。`tier` に関わらず必ず行う。
                storage::verify_row_key_tenant(key_tenant, tenant_id).map_err(storage_internal)?;

                // 可視行・必要時のみ（Issue #350）: `tier` が要求する範囲だけ dim・
                // metadata・embedding をデコードする。`DecodeTier::Embedding` は
                // 上で読み済みの `offset` を引き継いで本体のみをデコードし、ヘッダの
                // 二重デコードを避ける（Issue #349）。
                let (dim, metadata): (u32, &[u8]) = match tier {
                    DecodeTier::DimAndScalar => storage::decode_row_dim_and_metadata_borrowed(buf)
                        .map_err(storage_internal)?,
                    DecodeTier::Embedding => {
                        storage::decode_row_body_into(buf, offset, &mut embedding_scratch)
                            .map_err(storage_internal)?
                    }
                    // `GROUP BY` はヘッダのみのファストパスを持たない
                    // （`tier` 決定ロジック参照）。
                    DecodeTier::Fast => {
                        return Err(accumulator_bug(
                            "GROUP BY execution reached DecodeTier::Fast, which it never selects",
                        ))
                    }
                };
                if let Some(expected) = expected_dim {
                    if dim != 0 && dim != expected {
                        return Err(SqlSurfaceError::Internal {
                            detail: "aggregate row scan failed: embedding dimension mismatch"
                                .to_string(),
                        });
                    }
                }

                // マスク外の列は構造検証のみで `&str` 化を省略する
                // （`row_codec::scan_scalar_columns_masked`）。`GROUP BY` キー列は
                // `ReferencedColumns::derive` の `extra_scalar_index` で常にマスクへ
                // 含まれるため、`any_scalar_column_referenced()` は常に真。
                let scanned: Vec<Option<row_codec::ScalarRef<'_>>> =
                    row_codec::scan_scalar_columns_masked(
                        schema,
                        metadata,
                        Some(referenced.scalar_mask()),
                    )?;

                // SCALAR 段（WHERE）。
                if !declarative_filter::matches_all(&bound.metadata_filters, &scanned) {
                    continue;
                }
                for (expr, program) in bound.expr_filters.iter().zip(&bound.expr_filter_programs) {
                    let embedding: &[f32] = if udf_call::references_embedding(expr) {
                        match tier {
                        DecodeTier::Embedding => embedding_scratch.as_slice(),
                        DecodeTier::Fast | DecodeTier::DimAndScalar => {
                            return Err(accumulator_bug(
                                "WHERE expression references the VECTOR column but tier did not decode it",
                            ))
                        }
                    }
                    } else {
                        &[]
                    };
                    match program.eval(id, embedding, &mut expr_scratch)? {
                        ExprValue::Bool(true) => {}
                        ExprValue::Bool(false) => continue 'rows,
                        _ => {
                            return Err(SqlSurfaceError::invalid_input(
                                "WHERE expression did not evaluate to a boolean",
                            ))
                        }
                    }
                }
                // TASK-208・SQL-24（Issue #912）: `WHERE` の OR 群を、既存の
                // メタデータフィルタ・式述語と同じ SCALAR 段の一部として適用する。
                for group in &bound.or_filters {
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

                // defense-in-depth（RlsSafetyNet と同趣旨）。ヘッダから取り出した
                // `tenant_id`・`visibility` に対して再適用する（独立した二重検証では
                // ない点を含め `aggregate.rs::execute_aggregate` の同一箇所のドキュメント
                // 参照）。
                if !ctx.is_visible(tenant_id, visibility) {
                    continue;
                }

                // GROUP 段: グループキーを確定してから、可視行のみをグループ表へ
                // 反映する（このため他テナントにしか存在しないキーはグループとして
                // 一切現れない＝RLS-7・RLS-8 の `GROUP BY` 版）。
                //
                // 行 1 件分の `VECTOR` 列ビュー（Issue #350）。`tier` が
                // `DecodeTier::Embedding` を選んだ場合のみ実体（`embedding_scratch`）を
                // 持ち、それ以外は `dim` のみで `values: None`（`Accumulator::observe`
                // 側が `ScalarExpr` の embedding 参照を fail-closed に拒否する仕組みで
                // 誤用を防ぐ）。
                let vector = RowVector {
                    dim,
                    values: match tier {
                        DecodeTier::Embedding => Some(embedding_scratch.as_slice()),
                        DecodeTier::Fast | DecodeTier::DimAndScalar => None,
                    },
                };

                if key_count == 1 {
                    // 単一キー: 借用キー（`&str`）でまず既存グループを 1 回だけ
                    // 探索し、ヒットした行では所有 `String` を一切確保しない
                    // （Issue #351）。GROUP BY キー列は束縛段
                    // （`sql::parser::bind_group_by_clause`）で TEXT 列に限定済み
                    // （BOOLEAN 列は `22000` で拒否）のため常に `Text` のはずだが、
                    // untrusted な格納済みデータに由来する不変条件のため念のため
                    // fail-closed に扱う（`as_text()` が `None` を返す＝NULL 相当
                    // として扱う）。
                    let column_index =
                        group_by.column_indices.first().copied().ok_or_else(|| {
                            accumulator_bug("single-key GROUP BY path called with no columns")
                        })?;
                    let key_value = scanned
                        .get(column_index)
                        .copied()
                        .flatten()
                        .and_then(|v| v.as_text());
                    let total_group_count = string_groups.len() + usize::from(null_group.is_some());
                    match key_value {
                        Some(key_str) => {
                            if let Some(accs) = string_groups.get_mut(key_str) {
                                // 既存グループへの累積: 探索 1 回・String 確保 0 回。
                                accumulate_row(
                                    accs,
                                    &bound.items,
                                    id,
                                    &vector,
                                    &scanned,
                                    &mut total_text_accumulator_bytes,
                                    total_group_count,
                                    total_key_bytes,
                                    &budget,
                                    &mut expr_scratch,
                                    &mut distinct_budget,
                                )?;
                            } else {
                                // 新規グループ: 予算検査 → ローカルでアキュムレータを
                                // 確保・累積 → 確定後に 1 回だけキーを所有化して挿入
                                // する（挿入後の再探索は不要）。
                                check_new_group_budget(
                                    total_group_count,
                                    &mut total_key_bytes,
                                    key_str.len(),
                                    total_text_accumulator_bytes,
                                    &budget,
                                )?;
                                let mut accs = new_accumulators(&bound.items)?;
                                accumulate_row(
                                    &mut accs,
                                    &bound.items,
                                    id,
                                    &vector,
                                    &scanned,
                                    &mut total_text_accumulator_bytes,
                                    total_group_count.saturating_add(1),
                                    total_key_bytes,
                                    &budget,
                                    &mut expr_scratch,
                                    &mut distinct_budget,
                                )?;
                                string_groups.insert(try_clone_str(key_str)?, accs);
                            }
                        }
                        None => {
                            if null_group.is_none() {
                                check_new_group_budget(
                                    total_group_count,
                                    &mut total_key_bytes,
                                    0,
                                    total_text_accumulator_bytes,
                                    &budget,
                                )?;
                                null_group = Some(new_accumulators(&bound.items)?);
                            }
                            // NULL グループは直前で存在が確定しているため、グループ数は
                            // 非 NULL グループ数＋1。
                            let group_count = string_groups.len().saturating_add(1);
                            let accs = null_group.as_mut().ok_or_else(|| {
                                accumulator_bug("null group entry disappeared after insertion")
                            })?;
                            accumulate_row(
                                accs,
                                &bound.items,
                                id,
                                &vector,
                                &scanned,
                                &mut total_text_accumulator_bytes,
                                group_count,
                                total_key_bytes,
                                &budget,
                                &mut expr_scratch,
                                &mut distinct_budget,
                            )?;
                        }
                    }
                } else {
                    // 複数キー（SQL-25 (d)）: 索引経路を持たない全走査専用の
                    // `multi_groups` へ振り分ける。各成分は単一キーと同じ規約
                    // （`TEXT` 限定・fail-closed で NULL 扱い）で解決する。
                    //
                    // PR #1099 レビュー指摘（Cursor Bugbot・codex-review）対応:
                    // 単一キー経路（`string_groups.get_mut(key_str)`。上記
                    // 484〜489 行目のコメント参照）と同じく、まず借用成分列
                    // （`scanned` から借用した `&str`。所有化なし）で
                    // `multi_groups` を検索し（[`GroupKey`] の
                    // `Borrow<dyn GroupKeyView>` impl 経由）、既存グループへの
                    // 累積だけで済む行では成分の所有化（[`try_clone_str`]）を
                    // 一切発生させない。新規グループが確定した行のみ
                    // [`check_new_group_budget`] の予算検査を経てから成分を
                    // 1 回所有化する（旧実装は探索前に毎行 `try_clone_str` で
                    // 所有化しており、既存グループ更新行でも不要な複製が発生し、
                    // かつ予算超過で拒否される行でも複製コストを先払いしていた）。
                    //
                    // PR #1099 レビュー再指摘（codex-review P2）対応: 借用成分列
                    // 自体（`Vec<Option<&str>>`）も既存グループに一致する行で
                    // 毎行ヒープ確保していた。`GROUP BY` 列数は束縛段
                    // （[`crate::sql::allowlist::check_group_by_column_count`]）で
                    // 列を `push` する前に [`MAX_GROUP_BY_COLUMNS`] 以下へ検査済み
                    // のため、固定長スタック配列で足り、行走査のたびの確保が
                    // 不要になる。束縛段の不変条件が破れて上限を超えていた場合は
                    // fail-closed で `accumulator_bug`（`XX000`）へ落とす。
                    let key_count = group_by.column_indices.len();
                    if key_count > MAX_GROUP_BY_COLUMNS {
                        return Err(accumulator_bug(
                            "GROUP BY column count exceeds MAX_GROUP_BY_COLUMNS at execution time",
                        ));
                    }
                    let mut borrowed_storage: [Option<&str>; MAX_GROUP_BY_COLUMNS] =
                        [None; MAX_GROUP_BY_COLUMNS];
                    let mut key_len: usize = 0;
                    for (slot, &column_index) in
                        borrowed_storage.iter_mut().zip(&group_by.column_indices)
                    {
                        let value = scanned
                            .get(column_index)
                            .copied()
                            .flatten()
                            .and_then(|v| v.as_text());
                        if let Some(s) = value {
                            key_len = key_len.checked_add(s.len()).ok_or_else(|| {
                                accumulator_bug("GROUP BY key length accounting overflowed")
                            })?;
                        }
                        *slot = value;
                    }
                    let borrowed_components = &borrowed_storage[..key_count];
                    let probe = BorrowedGroupKey(borrowed_components);
                    let total_group_count = multi_groups.len();
                    if let Some(accs) = multi_groups.get_mut(&probe as &dyn GroupKeyView) {
                        accumulate_row(
                            accs,
                            &bound.items,
                            id,
                            &vector,
                            &scanned,
                            &mut total_text_accumulator_bytes,
                            total_group_count,
                            total_key_bytes,
                            &budget,
                            &mut expr_scratch,
                            &mut distinct_budget,
                        )?;
                    } else {
                        check_new_group_budget(
                            total_group_count,
                            &mut total_key_bytes,
                            key_len,
                            total_text_accumulator_bytes,
                            &budget,
                        )?;
                        // 予算検査を通過した行のみ、各成分を所有化する
                        // （`str::to_string` 等の無条件のインフォリブルな確保は、
                        // untrusted な格納済み TEXT 列値のサイズに対して確保失敗時
                        // に abort し得るため使わず、単一キー経路の
                        // `try_clone_str` と同じ `try_reserve_exact` ベースの
                        // 確保にする。`.claude/rules/security.md`「不安全な設計」
                        // 対応）。
                        let mut key_components: Vec<Option<String>> = Vec::new();
                        key_components
                            .try_reserve_exact(borrowed_components.len())
                            .map_err(|_| {
                                SqlSurfaceError::payload_too_large(
                                    "GROUP BY key allocation exceeds available memory",
                                )
                            })?;
                        for value in borrowed_components {
                            key_components.push(match value {
                                Some(s) => Some(try_clone_str(s)?),
                                None => None,
                            });
                        }
                        let group_key = GroupKey(key_components);
                        let mut accs = new_accumulators(&bound.items)?;
                        accumulate_row(
                            &mut accs,
                            &bound.items,
                            id,
                            &vector,
                            &scanned,
                            &mut total_text_accumulator_bytes,
                            total_group_count.saturating_add(1),
                            total_key_bytes,
                            &budget,
                            &mut expr_scratch,
                            &mut distinct_budget,
                        )?;
                        multi_groups.insert(group_key, accs);
                    }
                }
            }
        }
    }

    // FINISH 段: 各グループを確定 Cell へ変換し、HAVING で絞り込む。`h.item_index`・
    // `ProjectionColumn::Aggregate.item_index` は束縛段
    // （`sql::parser::bind_group_by_clause`）が `items.len()` 範囲内であることを
    // 保証済みの内部添字だが、untrusted 入力に由来する添字アクセスを避ける
    // 方針（`.claude/rules/coding-rust.md`）に従い、ここでも `.get()` で明示的に
    // 扱い、万一の不整合は panic ではなく [`accumulator_bug`]（`XX000`）へ落とす。
    //
    // 単一キー（`key_count == 1`）は分割前の `GroupKey::Ord`（非 NULL はバイト
    // 昇順・NULL は常に末尾）と同一の走査順にするため、`string_groups`
    // （`BTreeMap` の昇順 `into_iter`）→ `null_group` の順で連結する
    // （Issue #351。`sort-determinism-check`・決定性テストが前提とする順序を
    // 維持）。複数キー（SQL-25 (d)）は `multi_groups`（`BTreeMap<GroupKey, _>`。
    // 既に `GroupKey::Ord` の昇順）をそのまま使う。
    let total_group_count =
        string_groups.len() + usize::from(null_group.is_some()) + multi_groups.len();
    let group_entries: Box<dyn Iterator<Item = (GroupKey, Vec<Accumulator>)>> = if key_count == 1 {
        Box::new(
            string_groups
                .into_iter()
                .map(|(k, accs)| (GroupKey(vec![Some(k)]), accs))
                .chain(
                    null_group
                        .into_iter()
                        .map(|accs| (GroupKey(vec![None]), accs)),
                ),
        )
    } else {
        Box::new(multi_groups.into_iter())
    };

    let mut finished: Vec<(GroupKey, Vec<Cell>)> = Vec::with_capacity(total_group_count);
    for (key, accs) in group_entries {
        let cells: Vec<Cell> = accs
            .into_iter()
            .map(Accumulator::finish)
            .collect::<Result<Vec<_>, _>>()?;
        let mut keep = true;
        for h in &group_by.having {
            let cell = cells
                .get(h.item_index)
                .ok_or_else(|| accumulator_bug("HAVING item_index out of bounds"))?;
            if !having_matches(cell, h.op, h.literal) {
                keep = false;
                break;
            }
        }
        if keep {
            finished.push((key, cells));
        }
    }

    // ORDER BY: 未指定時はグループキー昇順（`GroupKey` の `Ord`。NULL は末尾）。
    // `sort_by` のクロージャは `Result` を返せないため、`.get()` の失敗（内部
    // 不整合。到達しない想定）は `Cell::Null` へ安全側にフォールバックする
    // （panic させない。誤った順序になり得るが、束縛段の保証によりそもそも
    // 到達しない防御的分岐）。
    match &group_by.order_by {
        Some(order_by) => {
            finished.sort_by(|(ka, ca), (kb, cb)| {
                let primary = match order_by.target {
                    OrderTarget::GroupKey(key_index) => {
                        // `key_index` は束縛段（`bind_group_by_clause`）が
                        // `column_indices` の範囲内であることを保証済みの内部
                        // 添字だが、`.get()` で明示的に扱い範囲外は NULL 相当
                        // （末尾）へ安全側にフォールバックする（防御的分岐。
                        // 到達しない想定）。
                        let a_component = ka.0.get(key_index).and_then(Option::as_ref);
                        let b_component = kb.0.get(key_index).and_then(Option::as_ref);
                        order_with_nulls_last(
                            a_component.is_none(),
                            b_component.is_none(),
                            match (a_component, b_component) {
                                (Some(a), Some(b)) => a.cmp(b),
                                _ => std::cmp::Ordering::Equal,
                            },
                            order_by.descending,
                        )
                    }
                    OrderTarget::Aggregate(idx) => {
                        let ca_cell = ca.get(idx).unwrap_or(&Cell::Null);
                        let cb_cell = cb.get(idx).unwrap_or(&Cell::Null);
                        order_with_nulls_last(
                            matches!(ca_cell, Cell::Null),
                            matches!(cb_cell, Cell::Null),
                            cmp_cell_values(ca_cell, cb_cell),
                            order_by.descending,
                        )
                    }
                };
                // 安定した決定性のため、同値はグループキー順で tie-break する
                // （`DESC` でも `NULL` は末尾のまま。`GroupKey::Ord` を使う昇順の
                // tie-break はそもそも方向反転の対象外）。
                primary.then_with(|| ka.cmp(kb))
            });
        }
        None => finished.sort_by(|(ka, _), (kb, _)| ka.cmp(kb)),
    }

    // Issue #916・SQL-25 (b)・TASK-209: `OFFSET` はソート確定後・`LIMIT` 適用前に
    // 適用する（グループはすでに可視行のみから構成済み〔`sql::aggregate` の
    // 走査ループが RLS を適用してから集約する〕ため、この段で読み飛ばしても RLS
    // 契約は変わらない）。`drain` の範囲は `finished.len()` でクランプし、添字
    // アクセス（`[]`）を使わない（`.claude/rules/coding-rust.md`）。
    if group_by.offset > 0 {
        let drop_to = group_by.offset.min(finished.len());
        finished.drain(..drop_to);
    }

    if let Some(limit) = group_by.limit {
        finished.truncate(limit);
    }

    // PROJECT 段: `bound.projection` の列順で `GroupKey`／集計結果を組み立てる。
    let mut columns = Vec::with_capacity(bound.projection.len());
    for col in &bound.projection {
        let name = match col {
            ProjectionColumn::GroupKey { name, .. } => name.clone(),
            ProjectionColumn::Aggregate { name, .. } => name.clone(),
        };
        columns.push(ColumnMeta::Computed { name });
    }

    let mut rows = Vec::with_capacity(finished.len());
    for (key, cells) in finished {
        let mut row_cells = Vec::with_capacity(bound.projection.len());
        for col in &bound.projection {
            let cell =
                match col {
                    // `key_index` は束縛段が `column_indices`（＝ `key.0`）の範囲内で
                    // あることを保証済みの内部添字だが、`.get()` で明示的に扱い
                    // 範囲外は fail-closed に `accumulator_bug`（`XX000`）へ落とす
                    // （`.claude/rules/coding-rust.md`）。
                    ProjectionColumn::GroupKey { key_index, .. } => match key.0.get(*key_index) {
                        Some(Some(s)) => Cell::Text(s.clone()),
                        Some(None) => Cell::Null,
                        None => {
                            return Err(accumulator_bug(
                                "projection key_index out of bounds for GROUP BY key",
                            ))
                        }
                    },
                    ProjectionColumn::Aggregate { item_index, .. } => cells
                        .get(*item_index)
                        .cloned()
                        .ok_or_else(|| accumulator_bug("projection item_index out of bounds"))?,
                };
            row_cells.push(cell);
        }
        rows.push(ResultRow {
            id: 0,
            score: 0.0,
            cells: row_cells,
        });
    }

    Ok(QueryResult { columns, rows })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnDef, ColumnType, TableSchema};
    use crate::sql::allowlist::AggregateFunc;
    use crate::sql::parser::{AggregateInput, BoundAggregate, BoundAggregateItem, BoundGroupBy};
    use crate::storage::{RowInput, Storage, Visibility};
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

    /// 物理キー側 `tenant_id`（`key_tenant`）とヘッダ側 `tenant_id`
    /// （`header_tenant`）を意図的にずらして raw redb 書き込みする（TABLE-12 の
    /// 整合検査を検証するための専用ヘルパ。`sql::aggregate::tests` の同名ヘルパと
    /// 同じ方針。Issue #349）。
    fn write_row_with_mismatched_key_tenant(
        storage: &Storage,
        table_name: &str,
        key_tenant: &str,
        header_tenant: &str,
        id: u64,
        embedding: &[f32],
    ) {
        let write_txn = storage.db().begin_write().expect("begin_write");
        {
            let mut table = write_txn
                .open_table(crate::catalog::user_rows_table_def(
                    &crate::catalog::user_rows_table_name(table_name),
                ))
                .expect("open row table");
            let buf = crate::storage::encode_row(&RowInput {
                tenant_id: header_tenant,
                visibility: Visibility::Public,
                embedding,
                metadata: &[],
            })
            .expect("encode row");
            table
                .insert((key_tenant, id), buf.as_slice())
                .expect("insert row");
        }
        crate::storage::bump_generation_and_commit(write_txn).expect("commit");
    }

    fn schema_with_text_group_column() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("lang", ColumnType::Text, true),
            ],
        )
    }

    fn bound_count_star_grouped_by_lang() -> BoundAggregate {
        BoundAggregate {
            table: "docs".to_string(),
            items: vec![BoundAggregateItem {
                func: AggregateFunc::Count,
                input: AggregateInput::AllVisible,
                name: "result".to_string(),
                distinct: false,
            }],
            metadata_filters: Vec::new(),
            expr_filters: Vec::new(),
            expr_filter_programs: Vec::new(),
            or_filters: Vec::new(),
            rls_predicate_present: false,
            projection: vec![
                crate::sql::parser::ProjectionColumn::GroupKey {
                    key_index: 0,
                    name: "lang".to_string(),
                },
                crate::sql::parser::ProjectionColumn::Aggregate {
                    item_index: 0,
                    name: "result".to_string(),
                },
            ],
            group_by: Some(BoundGroupBy {
                column_indices: vec![1],
                having: Vec::new(),
                order_by: None,
                limit: None,
                offset: 0,
            }),
        }
    }

    /// Issue #916・SQL-25 (b)・TASK-209: `OFFSET` はソート確定後・`LIMIT` 適用前に
    /// 適用する（3.3 節）ことを、`ORDER BY` なし（既定のグループキー昇順ソート）の
    /// `GROUP BY` 集計で確認する。5 グループ（"a".."e"）を昇順ソート後、
    /// `OFFSET 2 LIMIT 2` は 3・4 番目（"c"・"d"）だけを返す。
    #[test]
    fn group_by_offset_skips_leading_sorted_groups_before_limit_applies() {
        let path = unique_db_path("group-by-offset-paging");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = schema_with_text_group_column();
        storage.create_table(&schema).expect("create table");
        write_lang_rows(
            &storage,
            &schema,
            &[(1, "a"), (2, "b"), (3, "c"), (4, "d"), (5, "e")],
        );

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        use redb::ReadableDatabase;
        let read_txn = storage.db().begin_read().expect("begin_read");

        let mut bound = bound_count_star_grouped_by_lang();
        if let Some(group_by) = bound.group_by.as_mut() {
            group_by.offset = 2;
            group_by.limit = Some(2);
        }

        let result = execute_grouped_aggregate(
            &read_txn,
            &ctx,
            &schema,
            &bound,
            crate::sql::aggregate::MAX_AGGREGATE_RESULT_BYTES,
            None,
            None,
        )
        .expect("grouped aggregate with OFFSET should succeed");

        let keys: Vec<&str> = result
            .rows
            .iter()
            .map(|row| match &row.cells[0] {
                Cell::Text(s) => s.as_str(),
                other => panic!("expected group key cell to be Text, got {other:?}"),
            })
            .collect();
        assert_eq!(keys, vec!["c", "d"]);
    }

    // Issue #349: TABLE-12 の整合検査（物理キー側 `tenant_id` とヘッダ側
    // `tenant_id` の不一致）が、`decode_row_for_key` 呼び出しをやめた後の
    // 明示比較でも従来どおり fail-closed（`XX000`・`SqlSurfaceError::Internal`）に
    // 拒否されることを固定する（`sql::aggregate::tests` の単一行経路と同一の
    // 回帰を `GROUP BY` 経路で検証する）。
    #[test]
    fn key_tenant_header_tenant_mismatch_is_rejected_fail_closed() {
        let path = unique_db_path("group-by-table12-mismatch");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = schema_with_text_group_column();
        storage.create_table(&schema).expect("create table");

        write_row_with_mismatched_key_tenant(
            &storage,
            "docs",
            "tenant-a",
            "tenant-b",
            1,
            &[1.0, 2.0, 3.0],
        );

        let ctx = PolicyContext::new("tenant-b").expect("valid tenant");
        use redb::ReadableDatabase;
        let read_txn = storage.db().begin_read().expect("begin_read");

        let bound = bound_count_star_grouped_by_lang();
        let err = execute_grouped_aggregate(
            &read_txn,
            &ctx,
            &schema,
            &bound,
            crate::sql::aggregate::MAX_AGGREGATE_RESULT_BYTES,
            None,
            None,
        )
        .expect_err("key/header tenant mismatch must be rejected fail-closed");
        assert_eq!(err.wire_code(), "XX000");
    }

    /// [`bound_count_star_grouped_by_lang`] と同じスキーマ・グループ列だが、
    /// `MIN`/`MAX(lang)` を集計項目に持つ `BoundAggregate`（PR #1049 レビュー
    /// 指摘 P0 対応の回帰テスト用）。グループキー累計バイト数（`lang` 自身）と
    /// `TEXT` 集計状態累計バイト数（同じく `lang`）の両方が同時に発生する形。
    fn bound_min_max_grouped_by_lang() -> BoundAggregate {
        BoundAggregate {
            table: "docs".to_string(),
            items: vec![
                BoundAggregateItem {
                    func: AggregateFunc::Min,
                    input: AggregateInput::TextColumn(1),
                    name: "result_min".to_string(),
                    distinct: false,
                },
                BoundAggregateItem {
                    func: AggregateFunc::Max,
                    input: AggregateInput::TextColumn(1),
                    name: "result_max".to_string(),
                    distinct: false,
                },
            ],
            metadata_filters: Vec::new(),
            expr_filters: Vec::new(),
            expr_filter_programs: Vec::new(),
            or_filters: Vec::new(),
            rls_predicate_present: false,
            projection: vec![
                crate::sql::parser::ProjectionColumn::GroupKey {
                    key_index: 0,
                    name: "lang".to_string(),
                },
                crate::sql::parser::ProjectionColumn::Aggregate {
                    item_index: 0,
                    name: "result_min".to_string(),
                },
                crate::sql::parser::ProjectionColumn::Aggregate {
                    item_index: 1,
                    name: "result_max".to_string(),
                },
            ],
            group_by: Some(BoundGroupBy {
                column_indices: vec![1],
                having: Vec::new(),
                order_by: None,
                limit: None,
                offset: 0,
            }),
        }
    }

    /// PR #1049 レビュー指摘 P0 対応の回帰テスト: `GROUP BY` ありの集計は
    /// グループキー累計・`TEXT` 集計状態累計をそれぞれ独立に
    /// [`MAX_GROUP_KEY_TOTAL_BYTES`]・[`MAX_TEXT_ACCUMULATOR_TOTAL_BYTES`]
    /// （各 16 MiB）で頭打ちにするだけでは、両者の合計が呼び出し元の指定する
    /// 結果バイト予算（`sql::cursor::MAX_CURSOR_BYTES_PER_SESSION` 相当の小さい
    /// 値を模した `1`）を超えうる。[`accumulate_row`] の合算検査がこれを行生成
    /// 中に打ち切ることを固定する（[`crate::sql::scan::execute_scan_with_
    /// budget_honors_caller_supplied_cap`] と同じ検証形）。
    #[test]
    fn execute_grouped_aggregate_honors_caller_supplied_result_byte_budget() {
        let path = unique_db_path("group-by-result-budget-cap");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = schema_with_text_group_column();
        storage.create_table(&schema).expect("create table");

        let write_txn = storage.db().begin_write().expect("begin_write");
        {
            let mut table = write_txn
                .open_table(crate::catalog::user_rows_table_def(
                    &crate::catalog::user_rows_table_name("docs"),
                ))
                .expect("open row table");
            let metadata = crate::row_codec::encode_scalar_columns(
                &schema,
                &[
                    crate::row_codec::Value::Null,
                    crate::row_codec::Value::Text("ja".to_string()),
                ],
            )
            .expect("encode scalar columns");
            let buf = crate::storage::encode_row(&RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[1.0, 2.0, 3.0],
                metadata: &metadata,
            })
            .expect("encode row");
            table
                .insert(("tenant-a", 1u64), buf.as_slice())
                .expect("insert row");
        }
        crate::storage::bump_generation_and_commit(write_txn).expect("commit");

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        use redb::ReadableDatabase;
        let read_txn = storage.db().begin_read().expect("begin_read");

        let bound = bound_min_max_grouped_by_lang();
        // 既定予算（`sql::aggregate::MAX_AGGREGATE_RESULT_BYTES`）では成功する。
        execute_grouped_aggregate(
            &read_txn,
            &ctx,
            &schema,
            &bound,
            crate::sql::aggregate::MAX_AGGREGATE_RESULT_BYTES,
            None,
            None,
        )
        .expect("default budget should succeed");

        // 同じデータ・同じクエリでも、呼び出し元が極端に小さい予算を渡せば
        // 行生成中に打ち切られる（`sql::cursor::CursorStatement::Declare` の
        // 内側実行が `MAX_CURSOR_BYTES_PER_SESSION` を渡す経路の回帰）。
        let err = execute_grouped_aggregate(&read_txn, &ctx, &schema, &bound, 1, None, None)
            .expect_err("tiny caller-supplied budget must reject before default cap");
        assert_eq!(err.wire_code(), "54000");
    }

    /// `(id, lang)` の行を tenant-a・Public で raw redb 書き込みする
    /// （[`schema_with_text_group_column`] 専用）。
    fn write_lang_rows(storage: &Storage, schema: &TableSchema, rows: &[(u64, &str)]) {
        let write_txn = storage.db().begin_write().expect("begin_write");
        {
            let mut table = write_txn
                .open_table(crate::catalog::user_rows_table_def(
                    &crate::catalog::user_rows_table_name("docs"),
                ))
                .expect("open row table");
            for (id, lang) in rows {
                let metadata = crate::row_codec::encode_scalar_columns(
                    schema,
                    &[
                        crate::row_codec::Value::Null,
                        crate::row_codec::Value::Text((*lang).to_string()),
                    ],
                )
                .expect("encode scalar columns");
                let buf = crate::storage::encode_row(&RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 2.0, 3.0],
                    metadata: &metadata,
                })
                .expect("encode row");
                table
                    .insert(("tenant-a", *id), buf.as_slice())
                    .expect("insert row");
            }
        }
        crate::storage::bump_generation_and_commit(write_txn).expect("commit");
    }

    /// PR #1049 レビュー指摘（codex P1）の回帰テスト: `TEXT` 集計を含まない
    /// `COUNT(*) ... GROUP BY lang` でも、グループ追加のたびに生成中の結果全体
    /// （グループ数 × 1 グループあたりの固定分＋キー累計）を予算と照合する。
    /// 修正前は `TEXT` 集計状態が増えたときしか判定しなかったため、非 `TEXT` 集計の
    /// グループ数・キー追加だけで予算を超えても打ち切られなかった。
    ///
    /// 見積り: 投影 2 列・集計 1 項目 → 1 グループ 16＋2×8＝32 バイト。3 グループ
    /// （`ja`・`en`・`fr`、キー計 6 バイト）で 3×32＋6＝102 バイト。
    #[test]
    fn grouped_non_text_aggregate_is_bounded_by_result_budget_on_group_addition() {
        let path = unique_db_path("group-by-result-budget-non-text");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = schema_with_text_group_column();
        storage.create_table(&schema).expect("create table");
        write_lang_rows(&storage, &schema, &[(1, "ja"), (2, "en"), (3, "fr")]);

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        use redb::ReadableDatabase;
        let read_txn = storage.db().begin_read().expect("begin_read");
        let bound = bound_count_star_grouped_by_lang();

        let ok = execute_grouped_aggregate(&read_txn, &ctx, &schema, &bound, 102, None, None)
            .expect("estimate exactly at the budget must succeed");
        assert_eq!(ok.rows.len(), 3);

        let err = execute_grouped_aggregate(&read_txn, &ctx, &schema, &bound, 101, None, None)
            .expect_err("adding the third group must exceed the budget");
        assert_eq!(err.wire_code(), "54000");
        // グループ追加時の超過は索引経路から全走査へのフォールバック対象
        // （走査順依存の TEXT 超過）として分類しない（Cursor Bugbot Medium 対応）。
        assert!(
            !is_text_accumulator_budget_error(&err),
            "group-addition budget overflow must not trigger an index-path fallback"
        );
        assert!(
            is_text_accumulator_budget_error(&SqlSurfaceError::payload_too_large(
                RESULT_BUDGET_EXCEEDED_DETAIL
            )),
            "TEXT-growth budget overflow stays order-dependent (fallback)"
        );
    }

    /// [`ResultBudget::check`] はグループ数に比例する固定分・キー累計・`TEXT`
    /// 集計状態累計の合計で判定し、どの成分の増加でも超過を検出する。
    #[test]
    fn result_budget_counts_groups_keys_and_text_state() {
        let bound = bound_count_star_grouped_by_lang();
        let budget = ResultBudget::new(&bound, 100).expect("budget");
        assert_eq!(budget.per_group_bytes, 32);
        let d = RESULT_BUDGET_EXCEEDED_DETAIL;
        budget.check(3, 4, 0, d).expect("3*32+4 = 100 fits");
        assert_eq!(budget.check(3, 5, 0, d).unwrap_err().wire_code(), "54000");
        assert_eq!(budget.check(3, 4, 1, d).unwrap_err().wire_code(), "54000");
        assert_eq!(budget.check(4, 0, 0, d).unwrap_err().wire_code(), "54000");
        assert_eq!(
            budget.check(usize::MAX, 0, 0, d).unwrap_err().wire_code(),
            "54000",
            "overflow must fail closed"
        );
    }

    // --- cmp_signed_to_literal（Issue #892・D8） --------------------------

    #[test]
    fn cmp_signed_to_literal_handles_exact_boundaries() {
        use std::cmp::Ordering;
        assert_eq!(cmp_signed_to_literal(0, 0.0), Ordering::Equal);
        assert_eq!(cmp_signed_to_literal(-5, -5.0), Ordering::Equal);
        // `i64::MAX` 自体は `f64` で厳密に表現できない（2^63 に丸まる）ため、
        // `f64` で厳密表現できる大きな値で境界を確認する。
        assert_eq!(
            cmp_signed_to_literal(1_000_000_000_000_000, 1_000_000_000_000_000.0),
            Ordering::Equal
        );
        // `i64::MIN`（`-2^63`）は `f64` で厳密に表現できる。
        assert_eq!(
            cmp_signed_to_literal(i64::MIN, i64::MIN as f64),
            Ordering::Equal
        );
    }

    #[test]
    fn cmp_signed_to_literal_handles_fractional_literals() {
        use std::cmp::Ordering;
        // n == floor(literal) だが literal 自体は非整数 → n < literal。
        assert_eq!(cmp_signed_to_literal(-4, -3.5), Ordering::Less);
        assert_eq!(cmp_signed_to_literal(3, 3.5), Ordering::Less);
        assert_eq!(cmp_signed_to_literal(4, 3.5), Ordering::Greater);
    }

    #[test]
    fn cmp_signed_to_literal_handles_out_of_range_literals() {
        use std::cmp::Ordering;
        // `i64` の表現域を超えるリテラルは符号だけで確定する。
        assert_eq!(cmp_signed_to_literal(i64::MAX, 1e30), Ordering::Less);
        assert_eq!(cmp_signed_to_literal(i64::MIN, -1e30), Ordering::Greater);
    }

    #[test]
    fn cmp_signed_to_literal_rejects_nan_as_not_equal() {
        use std::cmp::Ordering;
        assert_ne!(cmp_signed_to_literal(0, f64::NAN), Ordering::Equal);
    }

    // --- cmp_cell_values（Issue #892・D9） ---------------------------------

    #[test]
    fn cmp_cell_values_orders_signed_integer_date_timestamp_and_numeric() {
        use std::cmp::Ordering;
        assert_eq!(
            cmp_cell_values(&Cell::SignedInteger(-5), &Cell::SignedInteger(3)),
            Ordering::Less
        );
        assert_eq!(
            cmp_cell_values(&Cell::Date(100), &Cell::Date(50)),
            Ordering::Greater
        );
        assert_eq!(
            cmp_cell_values(&Cell::Timestamp(1_000), &Cell::Timestamp(1_000)),
            Ordering::Equal
        );
        let a = Cell::Numeric(crate::numeric::Decimal::from_parts(150, 2).unwrap());
        let b = Cell::Numeric(crate::numeric::Decimal::from_parts(200, 2).unwrap());
        assert_eq!(cmp_cell_values(&a, &b), Ordering::Less);
    }

    // --- Issue #894: 新スカラー型（TABLE-13・TASK-199）の GROUP BY 経路 --------

    fn schema_with_text_group_column_and_uuid() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("lang", ColumnType::Text, true),
                ColumnDef::new("ext_id", ColumnType::Uuid, true),
            ],
        )
    }

    fn bound_count_uuid_grouped_by_lang() -> BoundAggregate {
        BoundAggregate {
            table: "docs".to_string(),
            items: vec![BoundAggregateItem {
                func: AggregateFunc::Count,
                input: AggregateInput::UuidColumn(2),
                name: "result".to_string(),
                distinct: false,
            }],
            metadata_filters: Vec::new(),
            expr_filters: Vec::new(),
            expr_filter_programs: Vec::new(),
            or_filters: Vec::new(),
            rls_predicate_present: false,
            projection: vec![
                crate::sql::parser::ProjectionColumn::GroupKey {
                    key_index: 0,
                    name: "lang".to_string(),
                },
                crate::sql::parser::ProjectionColumn::Aggregate {
                    item_index: 0,
                    name: "result".to_string(),
                },
            ],
            group_by: Some(BoundGroupBy {
                column_indices: vec![1],
                having: Vec::new(),
                order_by: None,
                limit: None,
                offset: 0,
            }),
        }
    }

    /// `GROUP BY`（`TEXT` キー）＋新型（`UUID`）集計は `derive` を通じても
    /// embedding を要求せず（`DimAndScalar` に収まる。受入条件 3）、TABLE-12
    /// のキー／ヘッダ tenant 不一致は fail-closed で拒否される（受入条件 2。
    /// `key_tenant_header_tenant_mismatch_is_rejected_fail_closed` の新型版）。
    #[test]
    fn key_tenant_header_tenant_mismatch_is_rejected_fail_closed_with_new_type_aggregate() {
        let path = unique_db_path("group-by-table12-mismatch-new-type");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = schema_with_text_group_column_and_uuid();
        storage.create_table(&schema).expect("create table");

        write_row_with_mismatched_key_tenant(
            &storage,
            "docs",
            "tenant-a",
            "tenant-b",
            1,
            &[1.0, 2.0, 3.0],
        );

        let ctx = PolicyContext::new("tenant-b").expect("valid tenant");
        use redb::ReadableDatabase;
        let read_txn = storage.db().begin_read().expect("begin_read");

        let bound = bound_count_uuid_grouped_by_lang();
        let referenced = crate::sql::aggregate::ReferencedColumns::derive(
            &schema,
            &bound.items,
            &bound.metadata_filters,
            &bound.expr_filters,
            &bound.or_filters,
            bound
                .group_by
                .as_ref()
                .map(|g| g.column_indices.as_slice())
                .unwrap_or(&[]),
        );
        assert!(
            !referenced.needs_embedding(),
            "COUNT(<UUID column>) grouped by TEXT must not require embedding decode"
        );

        let err = execute_grouped_aggregate(
            &read_txn,
            &ctx,
            &schema,
            &bound,
            crate::sql::aggregate::MAX_AGGREGATE_RESULT_BYTES,
            None,
            None,
        )
        .expect_err("key/header tenant mismatch must be rejected fail-closed");
        assert_eq!(err.wire_code(), "XX000");
    }

    /// 新型（`UUID`）集計付き `GROUP BY` の結果が、可視行だけから算出した値と
    /// 一致することを固定する（受入条件 1・3 の正しさの確認）。
    #[test]
    fn group_by_with_new_type_count_produces_expected_counts_per_group() {
        let path = unique_db_path("group-by-new-type-count");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = schema_with_text_group_column_and_uuid();
        storage.create_table(&schema).expect("create table");

        let write_txn = storage.db().begin_write().expect("begin_write");
        {
            let mut table = write_txn
                .open_table(crate::catalog::user_rows_table_def(
                    &crate::catalog::user_rows_table_name("docs"),
                ))
                .expect("open row table");
            let rows: [(u64, &str, crate::row_codec::Value); 3] = [
                (
                    1,
                    "ja",
                    crate::row_codec::Value::Uuid(crate::uuid::Uuid::from_bytes([1u8; 16])),
                ),
                (2, "ja", crate::row_codec::Value::Null),
                (
                    3,
                    "en",
                    crate::row_codec::Value::Uuid(crate::uuid::Uuid::from_bytes([2u8; 16])),
                ),
            ];
            for (id, lang, ext_id) in rows {
                // `encode_scalar_columns` の `values` は `schema.columns` と同じ
                // 添字（`embedding` を含む）で揃える（`VECTOR` 列自体は内部で
                // スキップされ値を消費しない）。
                let metadata = crate::row_codec::encode_scalar_columns(
                    &schema,
                    &[
                        crate::row_codec::Value::Null,
                        crate::row_codec::Value::Text(lang.to_string()),
                        ext_id,
                    ],
                )
                .expect("encode scalar columns");
                let buf = crate::storage::encode_row(&RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 2.0, 3.0],
                    metadata: &metadata,
                })
                .expect("encode row");
                table
                    .insert(("tenant-a", id), buf.as_slice())
                    .expect("insert row");
            }
        }
        crate::storage::bump_generation_and_commit(write_txn).expect("commit");

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        use redb::ReadableDatabase;
        let read_txn = storage.db().begin_read().expect("begin_read");

        let bound = bound_count_uuid_grouped_by_lang();
        let result = execute_grouped_aggregate(
            &read_txn,
            &ctx,
            &schema,
            &bound,
            crate::sql::aggregate::MAX_AGGREGATE_RESULT_BYTES,
            None,
            None,
        )
        .expect("grouped COUNT(<UUID column>) should succeed");

        let mut counts: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
        for row in &result.rows {
            let lang = match &row.cells[0] {
                Cell::Text(s) => s.clone(),
                other => panic!("unexpected group key cell: {other:?}"),
            };
            let count = match &row.cells[1] {
                Cell::Integer(n) => *n,
                other => panic!("unexpected count cell: {other:?}"),
            };
            counts.insert(lang, count);
        }
        // "ja" は 2 行あるが id=2 の ext_id が NULL のため COUNT(<UUID column>) は 1。
        assert_eq!(counts.get("ja"), Some(&1));
        assert_eq!(counts.get("en"), Some(&1));
    }
}
