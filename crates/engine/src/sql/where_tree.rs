//! `WHERE` 句の `OR` 結合・括弧グルーピングの束縛表現と評価
//! （TASK-208・SQL-24、Issue #912）。
//!
//! 責務境界: `sql::allowlist::Parser` が構文木（[`crate::sql::allowlist::
//! WherePredicate::Or`]）を組み立て、`sql::parser::bind_where_predicates` が
//! 本モジュールの型（[`BoundOrGroup`]・[`BoundConjunction`]）へ再帰的に束縛する。
//! `sql::exec`・`sql::scan`・`sql::aggregate`・`sql::group_by` の各実行経路は
//! [`BoundOrGroup::matches`] のみを呼び、第 2 の評価器を作らない（CLAUDE.md
//! 「委譲方針」）。
//!
//! 評価意味論: 分岐（`branches`）は宣言順に評価し、最初に真になった分岐で
//! 短絡する（`OR` の標準意味論）。1 分岐の中は `AND` と同じ短絡評価
//! （`metadata_filters` → `expr_filters` → 入れ子の `or_groups` の順）。
//! NULL・埋め込み欠如（`dim == 0` の行で `VECTOR` 列を実際に参照する式が
//! `ExprProgram::eval` で `ExprValue::Null` を返すケース。`sql::expr_program`
//! の `ExprStep::PushVector` 参照）は既存の葉と同じく「不一致（false）」として
//! その葉だけを false にする（`AND` のように行全体を除外しない。分岐の他の
//! 葉・他の分岐は評価を続ける）。

use crate::declarative_filter::{self, MetadataFilter};
use crate::row_codec::ScalarRef;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::expr_program::{ExprProgram, StackValue};
use crate::sql::udf_call::{self, BoundExpr, ExprValue};

/// `OR` で結ぶ分岐の集合（束縛済み）。分岐は 2 個以上（構文段
/// （[`crate::sql::allowlist::Parser::parse_where_or`]）が 1 個の場合は
/// 親の列へ平坦化するため、束縛対象として渡ってくる時点で常に 2 個以上）。
///
/// `pub`（TASK-208・Issue #912）: [`crate::sql::scalar_plan::ScalarShapeInput`]
/// （既に `pub`）が `&'a [BoundOrGroup]` フィールドを持つため、本型も少なくとも
/// 同じ可視性が要る（private-in-public を避ける。`MetadataFilter`・`BoundExpr`
/// と同じ理由）。フィールドは非公開のままで、外部からは
/// [`crate::sql::parser::BoundStatement::or_filters`] 等のアクセサー経由でのみ
/// スライスとして参照できる（構造体リテラルでの直接構築は不可）。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundOrGroup {
    pub(crate) branches: Vec<BoundConjunction>,
}

/// `AND` で結ぶ 1 分岐（束縛済み）。分岐の中にさらに `OR` 群を含められる
/// （`or_groups`。ネスト可）。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundConjunction {
    pub(crate) metadata_filters: Vec<MetadataFilter>,
    pub(crate) expr_filters: Vec<BoundExpr>,
    /// `expr_filters` の各要素を [`BoundConjunction::new`]（束縛時）に 1 回だけ
    /// [`ExprProgram::compile`] した結果。`expr_filters` と同じ添字で対応し、
    /// `matches` は行ごとに再コンパイルせずここを `zip` して評価する
    /// （`sql::exec::execute_predicate_delete` 等が持つ `expr_filter_programs`
    /// と同じ「束縛時コンパイル・行ループでは eval のみ」契約。Issue #912
    /// codex-review 指摘対応）。
    expr_programs: Vec<ExprProgram>,
    pub(crate) or_groups: Vec<BoundOrGroup>,
}

impl BoundOrGroup {
    pub(crate) fn new(branches: Vec<BoundConjunction>) -> Self {
        Self { branches }
    }

    /// `row_codec::scan_scalar_columns`（またはマスク版）が返した `scanned` と
    /// 行コンテキスト（`id`・`embedding`・`dim`）に対して本 OR 群を評価する。
    /// `scratch` は呼び出し元が行ループの外で 1 回だけ確保したスクラッチ
    /// バッファ（[`ExprProgram::eval`] の契約と同じ。行ごとに使い回してよい）。
    ///
    /// 索引経路（`sql::scalar_plan`・`sql::scalar_index`）は OR 群を含む述語を
    /// 一律 `ScalarPlan::PlainScan` へ縮退させるため（TASK-208 時点のスコープ、
    /// Issue #912）、行ループでは本メソッドが行ごとに呼ばれる。式述語の
    /// [`ExprProgram`] は [`BoundConjunction::new`]（束縛時）に 1 回だけ
    /// コンパイル済み（`expr_programs`）で、本メソッドはそれを `eval` するだけ
    /// （索引和集合の実装は将来の Issue で扱う）。
    pub(crate) fn matches(
        &self,
        scanned: &[Option<ScalarRef<'_>>],
        id: u64,
        embedding: &[f32],
        dim: usize,
        scratch: &mut Vec<StackValue>,
    ) -> Result<bool, SqlSurfaceError> {
        for branch in &self.branches {
            if branch.matches(scanned, id, embedding, dim, scratch)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// `self`（またはネストした分岐）が `WHERE` の式述語で `VECTOR` 列
    /// （embedding）を参照するかどうか（`sql::udf_call::references_embedding`
    /// を再帰的に適用する）。`sql::exec` 等が候補構築時に embedding を保持
    /// すべきか判定するために使う。
    pub(crate) fn references_embedding(&self) -> bool {
        self.branches
            .iter()
            .any(BoundConjunction::references_embedding)
    }

    /// `self`（またはネストした分岐）が参照する列インデックス（`metadata_filters`
    /// の [`MetadataFilter::column_index`]）を `out` へ追加する（重複除去は
    /// 呼び出し元の集合型に委ねる）。SCALAR 段のデコード対象列選択
    /// （`sql::exec::needed_column_indices` 等）が使う。
    pub(crate) fn visit_column_indices(&self, out: &mut dyn FnMut(usize)) {
        for branch in &self.branches {
            branch.visit_column_indices(out);
        }
    }
}

impl BoundConjunction {
    pub(crate) fn new(
        metadata_filters: Vec<MetadataFilter>,
        expr_filters: Vec<BoundExpr>,
        or_groups: Vec<BoundOrGroup>,
    ) -> Self {
        let expr_programs = expr_filters.iter().map(ExprProgram::compile).collect();
        Self {
            metadata_filters,
            expr_filters,
            expr_programs,
            or_groups,
        }
    }

    fn matches(
        &self,
        scanned: &[Option<ScalarRef<'_>>],
        id: u64,
        embedding: &[f32],
        dim: usize,
        scratch: &mut Vec<StackValue>,
    ) -> Result<bool, SqlSurfaceError> {
        if !declarative_filter::matches_all(&self.metadata_filters, scanned) {
            return Ok(false);
        }
        for (expr, program) in self.expr_filters.iter().zip(&self.expr_programs) {
            let references_embedding = udf_call::references_embedding(expr);
            // `dim == 0`（`VECTOR` 列が NULL）の行の NULL 伝播は `program.eval`
            // 自身（`ExprStep::PushVector` の空スライス判定。`sql::expr_program`
            // 参照）が行う（codex-review P1 指摘対応: 静的な式木走査
            // （`references_embedding`）による事前除外は `CASE` の選ばれない
            // 分岐に embedding 参照があるだけの葉まで誤って偽にしていたため撤去
            // し、評価時点の判定へ一本化した）。
            let row_embedding: &[f32] = if references_embedding { embedding } else { &[] };
            match program.eval(id, row_embedding, scratch)? {
                ExprValue::Bool(true) => {}
                // NULL（UNKNOWN）は非該当として扱う（対象ビヘイビア: SQL-26。
                // Issue #921。PostgreSQL の 3 値論理と同じ扱い）。
                ExprValue::Bool(false) | ExprValue::Null => return Ok(false),
                // 束縛段（`sql::parser::bind_where_predicates`）が `WHERE` 式述語の
                // 型を `Bool` に限定済みのため到達しない。
                _ => {
                    return Err(SqlSurfaceError::invalid_input(
                        "WHERE expression did not evaluate to a boolean",
                    ))
                }
            }
        }
        for group in &self.or_groups {
            if !group.matches(scanned, id, embedding, dim, scratch)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn references_embedding(&self) -> bool {
        self.expr_filters.iter().any(udf_call::references_embedding)
            || self
                .or_groups
                .iter()
                .any(BoundOrGroup::references_embedding)
    }

    fn visit_column_indices(&self, out: &mut dyn FnMut(usize)) {
        for filter in &self.metadata_filters {
            out(filter.column_index());
        }
        for group in &self.or_groups {
            group.visit_column_indices(out);
        }
    }
}
