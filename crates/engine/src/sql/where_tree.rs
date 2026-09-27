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

    /// `self`（またはネストした分岐）が式述語（[`BoundConjunction::expr_filters`]）
    /// を 1 つでも持つか（再帰的に判定。codex-review 指摘対応・Issue #913
    /// マージ時レビュー是正）。`sql::exec` の DISTANCE 先行 SCALAR 事後フィルタが、
    /// 式述語を含まない（＝評価がエラーを返し得ない）OR 群だけを
    /// `on_visible_row` で即時確定させ、式述語を含む OR 群は
    /// [`Self::metadata_verdict`]／[`Self::matches_deferred`] 経由で DISTANCE 段の
    /// 後まで評価を遅延させるかどうかを判定するために使う。
    pub(crate) fn contains_expr(&self) -> bool {
        self.branches.iter().any(BoundConjunction::contains_expr)
    }

    /// 生の `scanned`（`row_codec::scan_scalar_columns` 由来。実 NULL と型不一致を
    /// 区別できる）に対して、宣言的（メタデータ）述語の部分だけを評価した結果を
    /// 木として保持する（`self` と同じ形状。[`Self`]・[`BoundConjunction`] と
    /// 1 対 1）。式述語は一切評価しない（エラーを返さない）ため、DISTANCE 先行時
    /// （`sql::exec::execute_statement_with_cache` の `on_visible_row`）が
    /// 全可視行に対して安全に呼べる（codex-review 指摘対応: 式評価を全可視行へ
    /// 前倒しすると、Top-k 外の行のゼロ除算等がクエリ全体の失敗になってしまう。
    /// また `Value` へ複製してから DISTANCE 段の後で `ScalarRef` へ逆変換すると
    /// `postfilter_verdicts`〔`sql::exec`〕のコメントと同じ理由で `IS NULL` の
    /// fail-open が再発するため、生の `scanned` を見られるこの時点でのみ判定する）。
    pub(crate) fn metadata_verdict(
        &self,
        scanned: &[Option<ScalarRef<'_>>],
    ) -> OrGroupMetadataVerdict {
        OrGroupMetadataVerdict {
            branches: self
                .branches
                .iter()
                .map(|b| b.metadata_verdict(scanned))
                .collect(),
        }
    }

    /// [`Self::metadata_verdict`] が確定させた宣言的判定と、DISTANCE 段の後に
    /// 確定する行コンテキスト（`id`・`embedding`・`text_columns`）を使って
    /// 最終判定する（式述語をここで初めて評価する。エラーを返しうる）。
    /// `verdict` は同じ `self` に対して呼んだ [`Self::metadata_verdict`] の
    /// 戻り値を渡す契約（形状は常に一致する。同一の束縛済み `BoundOrGroup`
    /// から導出するため）。`text_columns`（Issue #919・SQL-26）は
    /// [`BoundConjunction::matches_deferred`] のドキュメント参照。
    pub(crate) fn matches_deferred(
        &self,
        verdict: &OrGroupMetadataVerdict,
        id: u64,
        embedding: &[f32],
        text_columns: &[Option<&str>],
        dim: usize,
        scratch: &mut Vec<StackValue>,
    ) -> Result<bool, SqlSurfaceError> {
        for (branch, branch_verdict) in self.branches.iter().zip(verdict.branches.iter()) {
            if branch.matches_deferred(branch_verdict, id, embedding, text_columns, dim, scratch)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// [`BoundOrGroup::metadata_verdict`] が返す、宣言的（メタデータ）述語だけを
/// 事前評価した結果の木（形状は元の `BoundOrGroup` と 1 対 1）。式述語の
/// 評価結果は含まない（[`BoundOrGroup::matches_deferred`] がこれと行コンテキスト
/// を合わせて最終判定する）。
#[derive(Debug, Clone)]
pub(crate) struct OrGroupMetadataVerdict {
    branches: Vec<ConjunctionMetadataVerdict>,
}

impl OrGroupMetadataVerdict {
    /// この OR 群が、式述語の値に関わらず不一致であることが宣言的部分だけで
    /// 確定しているか（`OR` の全分岐が `metadata_ok == false`。`AND` の短絡評価
    /// により、分岐が持つ式述語・ネストした OR 群の値は結果に影響しない）。
    /// `true` を返す行は、式述語を一切評価せず安全に（エラーを起こさず）除外
    /// できる（`sql::exec` の DISTANCE 先行 SCALAR 事後フィルタが使う）。
    pub(crate) fn is_definitely_false(&self) -> bool {
        self.branches.iter().all(|b| !b.metadata_ok)
    }
}

/// [`BoundConjunction::metadata_verdict`] が返す 1 分岐ぶんの宣言的判定。
#[derive(Debug, Clone)]
struct ConjunctionMetadataVerdict {
    /// [`BoundConjunction::metadata_filters`] を `matches_all` で判定した結果。
    /// `false` の場合、この分岐は式述語の値に関わらず不一致が確定する
    /// （`AND` の短絡評価。[`BoundConjunction::matches_deferred`] 参照）。
    metadata_ok: bool,
    or_groups: Vec<OrGroupMetadataVerdict>,
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
        // Issue #919・SQL-26: `visit_column_indices` が `TEXT` 参照を反映済みの
        // マスクで呼び出し元がデコードした `scanned` を、そのまま `.as_text()` へ
        // 写す。
        let text_columns: Vec<Option<&str>> = scanned
            .iter()
            .map(|v| v.and_then(|s| s.as_text()))
            .collect();
        for (expr, program) in self.expr_filters.iter().zip(&self.expr_programs) {
            if let Some(false) =
                eval_expr_predicate(expr, program, id, embedding, &text_columns, scratch)?
            {
                return Ok(false);
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
        // Issue #919・SQL-26: OR 分岐内の式述語が参照する `TEXT` 列も
        // デコード対象へ含める（欠けるとマスク外参照＝実 NULL との取り違えに
        // なる。`sql::scan`／`sql::aggregate` 等の `scalar_mask` 導出と同じ理由）。
        for expr in &self.expr_filters {
            udf_call::visit_referenced_scalar_columns(expr, out);
        }
        for group in &self.or_groups {
            group.visit_column_indices(out);
        }
    }

    fn contains_expr(&self) -> bool {
        !self.expr_filters.is_empty() || self.or_groups.iter().any(BoundOrGroup::contains_expr)
    }

    /// [`BoundOrGroup::metadata_verdict`] の分岐単位の本体。`scanned` に対して
    /// `metadata_filters` だけを評価する（式述語には触れない。エラーを返さない）。
    fn metadata_verdict(&self, scanned: &[Option<ScalarRef<'_>>]) -> ConjunctionMetadataVerdict {
        ConjunctionMetadataVerdict {
            metadata_ok: declarative_filter::matches_all(&self.metadata_filters, scanned),
            or_groups: self
                .or_groups
                .iter()
                .map(|g| g.metadata_verdict(scanned))
                .collect(),
        }
    }

    /// [`BoundOrGroup::matches_deferred`] の分岐単位の本体。`verdict.metadata_ok`
    /// が `false` なら（`AND` の短絡評価により）式述語を評価せず不一致を返す。
    /// `true` の場合のみ式述語・ネストした OR 群を評価する（[`Self::matches`] と
    /// 同じ評価順序・NULL 意味論。式述語だけがここで初めて評価されうる）。
    /// `text_columns`（Issue #919・SQL-26 の文字列関数が参照する `TEXT` 列）は
    /// 呼び出し元（`sql::exec`）が `candidate_columns`（`Value::Text` のみ
    /// `Some` になる、型不一致のない安全な変換。`postfilter_verdicts`
    /// 宣言のコメントが警告する `IS NULL` fail-open は `Value::Integer`／
    /// `BigInt`／`Array` を巻き込む変換に限られ、`Value::Text` の判別は
    /// 曖昧にならない）から導出して渡す。
    fn matches_deferred(
        &self,
        verdict: &ConjunctionMetadataVerdict,
        id: u64,
        embedding: &[f32],
        text_columns: &[Option<&str>],
        dim: usize,
        scratch: &mut Vec<StackValue>,
    ) -> Result<bool, SqlSurfaceError> {
        if !verdict.metadata_ok {
            return Ok(false);
        }
        for (expr, program) in self.expr_filters.iter().zip(&self.expr_programs) {
            if let Some(false) =
                eval_expr_predicate(expr, program, id, embedding, text_columns, scratch)?
            {
                return Ok(false);
            }
        }
        for (group, group_verdict) in self.or_groups.iter().zip(verdict.or_groups.iter()) {
            if !group.matches_deferred(group_verdict, id, embedding, text_columns, dim, scratch)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// [`BoundConjunction::expr_filters`] の 1 要素を評価する（[`BoundConjunction::
/// matches`]・[`BoundConjunction::matches_deferred`] が共有し、第 2 の評価器を
/// 作らない）。`Ok(Some(false))` は分岐全体を不一致として打ち切るべきことを
/// 示し、`Ok(None)` は一致（呼び出し元は次の式述語へ進む）を示す。
fn eval_expr_predicate(
    expr: &BoundExpr,
    program: &ExprProgram,
    id: u64,
    embedding: &[f32],
    text_columns: &[Option<&str>],
    scratch: &mut Vec<StackValue>,
) -> Result<Option<bool>, SqlSurfaceError> {
    let references_embedding = udf_call::references_embedding(expr);
    // `dim == 0`（`VECTOR` 列が NULL）の行の NULL 伝播は `program.eval` 自身
    // （`ExprStep::PushVector` の空スライス判定。`sql::expr_program` 参照）が
    // 行う（codex-review P1 指摘対応: 静的な式木走査（`references_embedding`）
    // による事前除外は `CASE` の選ばれない分岐に embedding 参照があるだけの
    // 葉まで誤って偽にしていたため撤去し、評価時点の判定へ一本化した）。
    let row_embedding: &[f32] = if references_embedding { embedding } else { &[] };
    match program.eval(id, row_embedding, text_columns, scratch)? {
        ExprValue::Bool(true) => Ok(None),
        // NULL（UNKNOWN）は非該当として扱う（対象ビヘイビア: SQL-26。Issue #921・
        // Issue #919（AC2）。PostgreSQL の 3 値論理と同じ扱い）。
        ExprValue::Bool(false) | ExprValue::Null => Ok(Some(false)),
        // 束縛段（`sql::parser::bind_where_predicates`）が `WHERE` 式述語の型を
        // `Bool` に限定済みのため到達しない。
        _ => Err(SqlSurfaceError::invalid_input(
            "WHERE expression did not evaluate to a boolean",
        )),
    }
}
