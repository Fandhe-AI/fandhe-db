//! FROM 句の派生表 `(SELECT ...) [[AS] 別名 [(列名, ...)]]` の抽出と静的解決
//! （SQL-62・Issue #1865。親 #1864。実行は #1866・JOIN 入力と深さ上限は #1867）。
//!
//! 呼び出し元は `sql::allowlist::validate_sql_tokens_impl`（SELECT。[`gate`]）と `validate_with_statement`
//! （非再帰 `WITH`。[`gate_with`]）の冒頭で、セッション最上位の
//! 入口（`CteEvaluation::AllowMaterialize`。`EngineCore` の
//! セッション SQL 入口だけが該当）に限って呼ばれる。ビュー本文・cursor・COPY・CTAS・
//! サブクエリ内側・`EXPLAIN`・`WITH RECURSIVE` はこの経路に入らず、従来どおり既存パーサーが
//! `42601` で拒否する（fail-closed）。
//!
//! 方式は「前置き抽出」。深さ 0 の FROM 句から派生表を検出して本文を切り出し、外側のトークン列
//! では派生表を内部名のプレースホルダ 1 トークンへ置き換える（#1848 のスカラーサブクエリと同型）。
//! 外側の文の構文検証は本 Issue では行わず、#1866 が置換後のトークン列で行う。本文の解析・検証は
//! CTE 本文と同じ部品（`parse_cte_select_body`・`validate_cte_definition_at`）を共有する。
//!
//! 検証を通過した「FROM 直後の単一の派生表」（#1866）は、合成 CTE として既存の単一表 `WITH`
//! 主クエリ経路でインライン展開または具体化して実行する（第 3 の実行器を作らない）。JOIN／カンマ区切り・
//! 複数件（#1867）は、同じ合成 CTE を `WITH`＋JOIN 主クエリ経路（`allowlist::validate_with_join_main`）
//! の slot として評価する。派生表の本文に入れ子の派生表を置けるが、深さは運用者設定の
//! サブクエリ深さ上限（`NestingLimits::max_subquery_depth`）で打ち切り、超過は `54000`。外側へは
//! 別名だけを残し、派生表の別名を実表名として走査へ流す経路は存在しない（合成名は `?` を含む。
//! JOIN パーサーが合成名だけをカタログ検証の対象外にする例外は [`is_derived_placeholder`] で判定）。
//!
//! `LATERAL (SELECT ...)`（Issue #3321。設計記録 `docs/design/lateral-derived-table.md`）は、構文
//! （コンマ区切り・`CROSS JOIN`・`[INNER] JOIN ... ON true`・`LEFT [OUTER] JOIN ... ON true`・FROM 先頭）
//! を本モジュールが受理し、`LATERAL` 語と `ON true` を外側のトークン列から落として通常の派生表と
//! 同じプレースホルダへ置き換える。本文が参照する左側 relation の列は `sql::lateral_bind` が束縛する。
//! 左側を参照しない LATERAL は通常の派生表（合成 CTE）として実行する。参照する INNER／CROSS／コンマの形は
//! 基底と後処理の 2 つの合成 CTE と相関述語へ分け、外側の行ごとに `sql::join::lateral` が再評価する
//! （Issue #3322）。`LEFT JOIN LATERAL`・`ON true` 以外の ON（#3323）も同じ再評価で実行し、ON は
//! 内側の `LIMIT`／`OFFSET` の後に効く。`ON true` は `CROSS JOIN` へ書き換えたうえで、結合段の種別を
//! 束縛前に Left／Right／Full へ戻す（[`attach_lateral_inputs`]）。左側を参照する RIGHT／FULL は
//! 束縛段が `42P10`、再評価の形へ組めない形は外側の文の検証を通したあと固定文言の `0A000` で止める。
//!
//! RLS-10 (b) の不変条件: 本モジュールの型はテナント・`PolicyContext` を持たない。解決時の
//! `42P01`／`42P10` 等はカタログの構造だけで決まり、行やテナントに依存しない。

use crate::nesting_limits::NestingLimits;
use crate::sql::allowlist::Statement;
use crate::sql::allowlist::{
    derived_output_names, is_set_operator_ident, join_relations_of, looks_like_join,
    parse_cte_select_body, parse_derived_alias_head, parse_view_body, parse_with_clause_and_main,
    set_operator_is_followed_by_branch, single_relation_main_over_ctes, split_parenthesized,
    validate_cte_definition_at, validate_cte_definitions, validate_with_join_main, SqlSurfaceError,
    TableLookup,
};
use crate::sql::allowlist::{JoinCondition, JoinKind};
use crate::sql::allowlist::{JoinLateral, LateralUnnest};
use crate::sql::cte::{CteDef, CteMaterialization};
use crate::sql::lateral_bind::{
    bind_lateral_body, bind_lateral_unnest_arg, split_for_execution, strip_inner_alias,
    LateralBinding,
};
use crate::sql::lexer::{Keyword, Token};
use crate::sql::qualified_column::normalize_single_table_qualifiers;
use crate::sql::relation::TableRef;

/// 派生表の内部名の接頭辞。`catalog::validate_identifier` を通らない（`?` を含む）ため利用者が
/// 書けず、カタログにも当たらない。
const PLACEHOLDER_PREFIX: &str = "?derived";

/// 派生表が FROM 句のどの位置に現れたか（#1866・#1867 が外側の束縛に使う）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DerivedPosition {
    /// `FROM (SELECT ...)`。
    From,
    /// `... JOIN (SELECT ...)`。
    Join,
    /// `FROM a, (SELECT ...)`（カンマ区切り）。
    CommaList,
}

/// LATERAL 派生表の書かれ方（Issue #3321）。結合の種類で左側 relation の範囲と、後続 Issue の実行方式が
/// 変わるため抽出段で確定させる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LateralForm {
    /// `FROM LATERAL (...)`（左側 relation なし）。
    Leading,
    /// `FROM a, LATERAL (...)`。
    Comma,
    /// `a CROSS JOIN LATERAL (...)`。
    Cross,
    /// `a [INNER] JOIN LATERAL (...) ON ...`。`on_true` は `ON true`（定数）だったか。
    Inner { on_true: bool },
    /// `a LEFT [OUTER] JOIN LATERAL (...) ON ...`。
    Left { on_true: bool },
    /// `a RIGHT [OUTER] JOIN LATERAL (...) ON ...`（Issue #3323）。左側を参照すると `42P10`。
    Right { on_true: bool },
    /// `a FULL [OUTER] JOIN LATERAL (...) ON ...`（Issue #3323）。左側を参照すると `42P10`。
    Full { on_true: bool },
}

/// 解決済みの派生表 1 件。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DerivedTable {
    /// 利用者が書いた別名（無ければ `None`。名前を合成しない）。
    pub(crate) alias: Option<String>,
    /// 列名リスト（空ならリストなし）。
    pub(crate) column_aliases: Vec<String>,
    pub(crate) position: DerivedPosition,
    /// `LATERAL` で書かれた派生表なら、その形（通常の派生表は `None`）。
    pub(crate) lateral: Option<LateralForm>,
    /// LATERAL の左側 relation への参照の束縛結果（LATERAL 以外は `None`）。
    pub(crate) lateral_binding: Option<LateralBinding>,
    /// 合成 CTE の内部名（`?derived<番号>`。外側の FROM の relation 名と一致する）。
    pub(crate) placeholder: String,
    /// 左側の列を参照する LATERAL を、外側の行ごとに再評価する形へ組めたときの実行情報
    /// （Issue #3322）。この場合の合成 CTE は基底（`?derived<番号>_base`）と後処理の 2 定義になる。
    pub(crate) lateral_exec: Option<JoinLateral>,
    /// 左側を参照する LATERAL のうち、再評価の形へ組めなかった理由（固定文言。外側の文の検証を
    /// 通したあとに `0A000` で拒否する）。
    pub(crate) lateral_reject: Option<String>,
}

/// 抽出・解決の結果。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DerivedFromClause {
    /// 最上位の FROM 句にある派生表（書かれた順）。
    pub(crate) tables: Vec<DerivedTable>,
    /// 合成 CTE（本文を CTE 定義の形で保持。`name` は内部プレースホルダ）。入れ子の内側が必ず
    /// 外側より前に並ぶ依存順で、最上位の派生表も含む。利用者の CTE の後ろへ付けて使う。
    pub(crate) scope_defs: Vec<CteDef>,
    /// 派生表を内部名の `Token::Ident` 1 つへ置き換えた外側のトークン列。
    pub(crate) outer_tokens: Vec<Token>,
}

/// 抽出直後（解決前）の派生表。
#[derive(Debug)]
struct RawDerived {
    /// 文全体で一意な番号（プレースホルダ `?derived<番号>` になる。入れ子でも重ならない）。
    number: usize,
    body: Vec<Token>,
    alias: Option<String>,
    column_aliases: Vec<String>,
    position: DerivedPosition,
    lateral: Option<LateralForm>,
    /// LATERAL の結合が `USING (...)` または `NATURAL` か（左側を参照する形は束縛後に固定 `0A000`。
    /// 参照しない形は通常の派生表と同じく既存の結合パーサーが処理する。Issue #3323）。
    using_or_natural: bool,
    /// `LATERAL unnest(<引数>) [WITH ORDINALITY]` の関数形（Issue #3360。`body` は引数のトークン列で、
    /// 本文の SELECT ではない）。通常の派生表・本文つき LATERAL は `None`。
    func: Option<RawUnnest>,
}

/// `LATERAL unnest(...)` の抽出情報（Issue #3360）。
#[derive(Debug)]
struct RawUnnest {
    ordinality: bool,
    /// 利用者が別名を書いたか（無ければ relation 名・値の列名は PostgreSQL と同じ `unnest`）。
    alias_given: bool,
}

/// 抽出結果（派生表の並びと、派生表をプレースホルダへ置き換えた外側のトークン列）。
type Extracted = (Vec<RawDerived>, Vec<Token>);

/// 抽出中の文全体で共有する状態（番号の採番・総数と入れ子深さの上限）。
struct ExtractState {
    /// これまでに切り出した派生表の総数（入れ子を含む。番号の採番にも使う）。
    count: usize,
    max_tables: usize,
    max_depth: usize,
}

impl ExtractState {
    /// 運用者設定の入れ子上限と、表参照数の上限（`relation::MAX_TABLE_REFS`）で初期化する。
    fn for_limits(limits: NestingLimits) -> Self {
        Self {
            count: 0,
            max_tables: crate::sql::relation::MAX_TABLE_REFS,
            max_depth: limits.max_subquery_depth(),
        }
    }
}

/// `name` が派生表の合成名（`?derived` ＋ 数字）か。`allowlist` の JOIN パーサーが、字句解析器が
/// 作れない `?` を含むこの名前だけをカタログ検証の対象外にするために使う（利用者は引用形でも
/// 書けない）。
pub(crate) fn is_derived_placeholder(name: &str) -> bool {
    name.strip_prefix(PLACEHOLDER_PREFIX)
        .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
}

/// 複数リレーションの FROM にある別名なしの派生表を示す固定エラー（`0A000`）。公開名が内部名に
/// なるため受け付けない。入力の断片を含めない。
fn alias_required_in_join() -> SqlSurfaceError {
    SqlSurfaceError::feature_not_supported("a derived table in a JOIN must have an alias")
}

fn is_ident(t: Option<&Token>, word: &str) -> bool {
    matches!(t, Some(Token::Ident(n)) if n.eq_ignore_ascii_case(word))
}

/// `tokens[idx]` の直前が派生表を置ける位置（`FROM`／`JOIN`／FROM 句内の `,`）なら、その種別を返す。
fn position_before(tokens: &[Token], idx: usize) -> Option<DerivedPosition> {
    let prev = idx.checked_sub(1).and_then(|p| tokens.get(p))?;
    match prev {
        Token::Keyword(Keyword::From) => Some(DerivedPosition::From),
        Token::Ident(n) if n.eq_ignore_ascii_case("JOIN") => Some(DerivedPosition::Join),
        Token::Punct(',') => Some(DerivedPosition::CommaList),
        _ => None,
    }
}

/// `LATERAL` の直前（`JOIN` の前の修飾語）から [`LateralForm`] と、外側トークン列から落とす/書き換える
/// 修飾語の個数（`JOIN` 自身を除く。`NATURAL` は数えない）、`NATURAL` の有無を求める（Issue #3323 で
/// RIGHT／FULL／NATURAL を受理。左側を参照する RIGHT／FULL は束縛段が `42P10`）。`i` は `LATERAL` の位置。
fn lateral_form(
    tokens: &[Token],
    i: usize,
    position: DerivedPosition,
) -> Result<(LateralForm, usize, bool), SqlSurfaceError> {
    match position {
        DerivedPosition::From => Ok((LateralForm::Leading, 0, false)),
        DerivedPosition::CommaList => Ok((LateralForm::Comma, 0, false)),
        DerivedPosition::Join => {
            let before = |n: usize| i.checked_sub(n).and_then(|p| tokens.get(p));
            // 修飾語は直前に FROM／JOIN／`,` が無い位置の語だけ（`FROM left JOIN LATERAL` の `left` は
            // 左側のテーブル名であり結合修飾子ではない）。
            let word = |n: usize, w: &str| {
                is_ident(before(n), w)
                    && i.checked_sub(n)
                        .is_none_or(|p| position_before(tokens, p).is_none())
            };
            let outer_word = |n: usize| -> Result<(LateralForm, usize), SqlSurfaceError> {
                // `<種別> OUTER JOIN`（`n` は OUTER の位置）。
                if word(n.saturating_add(1), "LEFT") {
                    Ok((LateralForm::Left { on_true: false }, 2))
                } else if word(n.saturating_add(1), "RIGHT") {
                    Ok((LateralForm::Right { on_true: false }, 2))
                } else if word(n.saturating_add(1), "FULL") {
                    Ok((LateralForm::Full { on_true: false }, 2))
                } else {
                    Err(SqlSurfaceError::feature_not_supported(
                        "this JOIN form with LATERAL is not supported",
                    ))
                }
            };
            let (form, extra): (LateralForm, usize) = if word(2, "INNER") {
                (LateralForm::Inner { on_true: false }, 1)
            } else if word(2, "CROSS") {
                (LateralForm::Cross, 1)
            } else if word(2, "LEFT") {
                (LateralForm::Left { on_true: false }, 1)
            } else if word(2, "RIGHT") {
                (LateralForm::Right { on_true: false }, 1)
            } else if word(2, "FULL") {
                (LateralForm::Full { on_true: false }, 1)
            } else if word(2, "OUTER") {
                outer_word(2)?
            } else {
                (LateralForm::Inner { on_true: false }, 0)
            };
            let natural = word(extra.saturating_add(2), "NATURAL");
            Ok((form, extra, natural))
        }
    }
}

/// 別名部の直後 `after` が `ON true` で FROM 項目が終わる形か（`ON true` を落としてよいか）。
fn is_on_true(after: &[Token]) -> bool {
    if !(is_ident(after.first(), "ON") && is_ident(after.get(1), "true")) {
        return false;
    }
    let end = 2usize;
    match after.get(end) {
        None | Some(Token::Punct(',' | ')' | ';')) => true,
        Some(Token::Ident(n)) => {
            ["JOIN", "INNER", "LEFT", "RIGHT", "FULL", "CROSS", "NATURAL"]
                .iter()
                .any(|w| n.eq_ignore_ascii_case(w))
                || ends_from_clause(after, end)
        }
        Some(_) => ends_from_clause(after, end),
    }
}

/// `tokens` に FROM／JOIN／`,` の直後の `LATERAL (` があるか（`EXPLAIN` 入口が LATERAL を明示拒否する
/// ための検出。抽出の拒否・受理に依存せず固定 `0A000` にする。Issue #3321・#3324 の受け皿）。
pub(crate) fn contains_lateral(tokens: &[Token]) -> bool {
    tokens.iter().enumerate().any(|(i, t)| {
        let next = |n: usize| tokens.get(i.saturating_add(n));
        // `LATERAL (`・`LATERAL <関数名> (`・`LATERAL ROWS FROM`（Issue #3360）。
        let item = matches!(next(1), Some(Token::Punct('(')))
            || (matches!(next(1), Some(Token::Ident(_)))
                && matches!(next(2), Some(Token::Punct('('))))
            || (is_ident(next(1), "ROWS")
                && matches!(next(2), Some(Token::Keyword(Keyword::From))));
        is_ident(Some(t), "LATERAL") && item && position_before(tokens, i).is_some()
    })
}

/// 深さ 0 の FROM 句を終わらせるトークンか。
fn ends_from_clause(tokens: &[Token], idx: usize) -> bool {
    match tokens.get(idx) {
        Some(Token::Keyword(Keyword::Where | Keyword::Order | Keyword::Limit)) => true,
        Some(Token::Punct(';')) => true,
        Some(Token::Ident(n)) => {
            ["GROUP", "HAVING", "OFFSET", "FETCH", "WINDOW"]
                .iter()
                .any(|w| n.eq_ignore_ascii_case(w))
                // `JOIN ... USING (a, b)` の `USING (` は終端ではない。`USING PLAN`・
                // `USING OPERATION_ID` が終端。
                || (n.eq_ignore_ascii_case("USING")
                    && !matches!(tokens.get(idx.saturating_add(1)), Some(Token::Punct('('))))
        }
        _ => false,
    }
}

/// 深さ 0 の FROM 句から派生表を検出・切り出す。見つからなければ `Ok(None)`。
/// 深さ 0 に集合演算子がある文は対象外（`Ok(None)`。従来どおり既存パーサーが拒否する）。
/// `LATERAL (SELECT ...)` は固定 `0A000`（SQL-62 の対象外）。走査は O(トークン数)・添字は `get()`。
#[cfg(test)]
fn extract(tokens: &[Token], max_tables: usize) -> Result<Option<Extracted>, SqlSurfaceError> {
    let mut st = ExtractState {
        count: 0,
        max_tables,
        max_depth: crate::sql::allowlist::MAX_SUBQUERY_DEPTH,
    };
    extract_at(tokens, &mut st, 1, true)
}

/// [`extract`] の本体。`depth_level` はここで見つかる派生表の入れ子深さ（最上位は 1）で、運用者設定の
/// 上限を超えたら本文の解析より前に `54000` で拒否する。`keep_alias` が偽なら別名を外側へ残さない
/// （派生表の本文側。CTE 本文は修飾列の正規化を持たない）。
fn extract_at(
    tokens: &[Token],
    st: &mut ExtractState,
    depth_level: usize,
    keep_alias: bool,
) -> Result<Option<Extracted>, SqlSurfaceError> {
    let mut found: Vec<RawDerived> = Vec::new();
    let mut outer: Vec<Token> = Vec::new();
    let mut depth: usize = 0;
    let mut in_from = false;
    let mut i: usize = 0;
    while let Some(tok) = tokens.get(i) {
        if depth == 0 {
            // 後続に枝（`SELECT` 等）が続く場合のみ集合演算子とみなす（`union` 等は通常の識別子）。
            if matches!(tok, Token::Ident(n) if is_set_operator_ident(n))
                && set_operator_is_followed_by_branch(tokens, i)
            {
                return Ok(None);
            }
            if matches!(tok, Token::Keyword(Keyword::From)) {
                // `IS [NOT] DISTINCT FROM` は FROM 句の開始ではない。
                let after_distinct = i
                    .checked_sub(1)
                    .and_then(|p| tokens.get(p))
                    .is_some_and(|t| is_ident(Some(t), "DISTINCT"));
                in_from = !after_distinct;
            } else if in_from && ends_from_clause(tokens, i) {
                in_from = false;
            }
            if in_from {
                let lateral_item =
                    is_ident(Some(tok), "LATERAL") && position_before(tokens, i).is_some();
                let lateral_kw = lateral_item
                    && matches!(tokens.get(i.saturating_add(1)), Some(Token::Punct('(')));
                // `LATERAL unnest(` の関数形（Issue #3360）。`ROWS FROM (...)`・unnest 以外の関数名は
                // 固定文言の `0A000`（外側の文の構文より前に拒否する）。
                let mut func_kw = false;
                if lateral_item {
                    if is_ident(tokens.get(i.saturating_add(1)), "ROWS")
                        && matches!(
                            tokens.get(i.saturating_add(2)),
                            Some(Token::Keyword(Keyword::From))
                        )
                    {
                        return Err(SqlSurfaceError::feature_not_supported(
                            "ROWS FROM is not supported",
                        ));
                    }
                    if let (Some(Token::Ident(name)), Some(Token::Punct('('))) = (
                        tokens.get(i.saturating_add(1)),
                        tokens.get(i.saturating_add(2)),
                    ) {
                        if name.eq_ignore_ascii_case("unnest") {
                            func_kw = true;
                        } else if !["JOIN", "ON", "USING"]
                            .iter()
                            .any(|w| name.eq_ignore_ascii_case(w))
                        {
                            return Err(SqlSurfaceError::feature_not_supported(
                                "only unnest is supported as a set-returning function with LATERAL",
                            ));
                        }
                    }
                }
                // `LATERAL (` の `(`、`LATERAL unnest (` の `(` の位置（LATERAL でなければ `i` 自身）。
                let paren_at = if lateral_kw {
                    i.saturating_add(1)
                } else if func_kw {
                    i.saturating_add(2)
                } else {
                    i
                };
                if func_kw && depth_level > 1 {
                    return Err(SqlSurfaceError::feature_not_supported(
                        "LATERAL inside a derived table is not supported yet",
                    ));
                }
                if lateral_kw {
                    if !matches!(
                        tokens.get(i.saturating_add(2)),
                        Some(Token::Keyword(Keyword::Select))
                    ) {
                        return Err(SqlSurfaceError::feature_not_supported(
                            "LATERAL is only supported for a parenthesized SELECT",
                        ));
                    }
                    if depth_level > 1 {
                        return Err(SqlSurfaceError::feature_not_supported(
                            "LATERAL inside a derived table is not supported yet",
                        ));
                    }
                }
                if lateral_kw
                    || func_kw
                    || (matches!(tok, Token::Punct('('))
                        && matches!(
                            tokens.get(i.saturating_add(1)),
                            Some(Token::Keyword(Keyword::Select))
                        ))
                {
                    if let Some(position) = position_before(tokens, i) {
                        let (mut lateral, lateral_words, natural) = if lateral_kw || func_kw {
                            let (f, w, n) = lateral_form(tokens, i, position)?;
                            (Some(f), w, n)
                        } else {
                            (None, 0, false)
                        };
                        let rest = tokens.get(paren_at..).unwrap_or(&[]);
                        let (inner, after_paren) = split_parenthesized(rest)?;
                        // `unnest(...) WITH ORDINALITY`（Issue #3360）。`WITH` の後が `ORDINALITY` 以外は `42601`。
                        let mut after = after_paren;
                        let mut ordinality = false;
                        if func_kw && is_ident(after.first(), "WITH") {
                            if !is_ident(after.get(1), "ORDINALITY") {
                                return Err(SqlSurfaceError::unsupported(
                                    "expected ORDINALITY after WITH in a FROM function",
                                ));
                            }
                            ordinality = true;
                            after = after.get(2..).unwrap_or(&[]);
                        }
                        if inner.iter().any(|t| matches!(t, Token::Punct(';'))) {
                            return Err(SqlSurfaceError::unsupported(
                                "derived table body must not contain a statement separator",
                            ));
                        }
                        if depth_level > st.max_depth {
                            return Err(SqlSurfaceError::payload_too_large(format!(
                                "derived table nesting exceeds limit {}",
                                st.max_depth
                            )));
                        }
                        if st.count >= st.max_tables {
                            return Err(SqlSurfaceError::payload_too_large(
                                "too many derived tables",
                            ));
                        }
                        let (alias, column_aliases, mut consumed) =
                            parse_derived_alias_head(after)?;
                        // 別名の無い `unnest(...)` の relation 名は PostgreSQL と同じ `unnest`。外側の JOIN
                        // パーサーが別名なしの派生表を拒否するため、別名を合成して外側へ残す。
                        let alias_given = alias.is_some();
                        // 同一 FROM 内の 2 個目以降の別名なし `unnest` は PostgreSQL が許す。公開名は常に `unnest` で、
                        // 外側のトークン列へ残す内部名（`unnest_<連番>`）は、文全体のどの識別子とも一致しない綴りを選び、
                        // 明示した別名（`AS unnest_2` 等）と衝突させず、利用者が内部名を参照して解決させることもない。
                        // 公開名 `unnest` の修飾参照が曖昧になる場合は抽出の最後に `42712` で拒否する。
                        let alias = if func_kw && alias.is_none() {
                            let unnamed = found
                                .iter()
                                .filter(|d| d.func.as_ref().is_some_and(|f| !f.alias_given))
                                .count();
                            if found.iter().any(|d| {
                                d.func.as_ref().is_none_or(|f| f.alias_given)
                                    && d.alias
                                        .as_deref()
                                        .is_some_and(|o| o.eq_ignore_ascii_case("unnest"))
                            }) {
                                return Err(SqlSurfaceError::unsupported(
                                    "table name specified more than once",
                                ));
                            }
                            if unnamed == 0 {
                                Some("unnest".to_string())
                            } else {
                                let mut seq = unnamed.saturating_add(1);
                                let name = loop {
                                    let cand = format!("unnest_{seq}");
                                    let used = tokens.iter().any(|t| match t {
                                        Token::Ident(n) => n.eq_ignore_ascii_case(&cand),
                                        Token::QualifiedIdent { qualifier, name } => {
                                            qualifier.eq_ignore_ascii_case(&cand)
                                                || name.eq_ignore_ascii_case(&cand)
                                        }
                                        _ => false,
                                    }) || found.iter().any(|d| {
                                        d.alias
                                            .as_deref()
                                            .is_some_and(|o| o.eq_ignore_ascii_case(&cand))
                                    });
                                    if !used {
                                        break cand;
                                    }
                                    seq = seq.saturating_add(1);
                                };
                                Some(name)
                            }
                        } else {
                            alias
                        };
                        // `[INNER|LEFT|RIGHT|FULL] JOIN LATERAL (...) t ON true` は `ON true` を落とし、
                        // `CROSS JOIN`（直積）へ書き換えて既存の結合パーサーを変えない。LEFT／RIGHT／FULL の
                        // 種別は `build_statement` が結合段へ設定し直す（`on_true` を保持する。Issue #3323）。
                        let after_alias = after.get(consumed..).unwrap_or(&[]);
                        let mut rewrite_join = false;
                        let using_or_natural = natural
                            || (lateral.is_some() && is_ident(after_alias.first(), "USING"));
                        if let Some(
                            LateralForm::Inner { on_true }
                            | LateralForm::Left { on_true }
                            | LateralForm::Right { on_true }
                            | LateralForm::Full { on_true },
                        ) = lateral.as_mut()
                        {
                            if is_on_true(after_alias) {
                                *on_true = true;
                                consumed = consumed.saturating_add(2);
                                rewrite_join = true;
                            }
                        }
                        if rewrite_join {
                            // `JOIN` と直前の修飾語を落として `CROSS JOIN` にする。
                            for _ in 0..lateral_words.saturating_add(1) {
                                outer.pop();
                            }
                            outer.push(Token::Ident("CROSS".to_string()));
                            outer.push(Token::Ident("JOIN".to_string()));
                        }
                        // 同じ FROM 句内の別名重複は `42601`（公開名の重複と同じ方針）。
                        if let Some(a) = &alias {
                            if found.iter().any(|d| {
                                d.alias
                                    .as_deref()
                                    .is_some_and(|o| o.eq_ignore_ascii_case(a))
                            }) {
                                return Err(SqlSurfaceError::unsupported(
                                    "table name specified more than once",
                                ));
                            }
                        }
                        st.count = st.count.saturating_add(1);
                        let number = st.count;
                        outer.push(Token::Ident(format!("{PLACEHOLDER_PREFIX}{number}")));
                        // 別名は暗黙別名の形で外側へ残す（`t.col` の修飾参照を外側の既存の
                        // 修飾列正規化が解決できるようにする。列名リストは合成 CTE が適用する）。
                        if let (Some(a), true) = (&alias, keep_alias) {
                            outer.push(Token::Ident(a.clone()));
                        }
                        found.push(RawDerived {
                            number,
                            body: inner.to_vec(),
                            alias,
                            column_aliases,
                            position,
                            lateral,
                            using_or_natural,
                            func: func_kw.then_some(RawUnnest {
                                ordinality,
                                alias_given,
                            }),
                        });
                        // 本文（`(` 〜 `)`）・`WITH ORDINALITY`・別名部を読み飛ばす。
                        let skipped = rest
                            .len()
                            .saturating_sub(after_paren.len())
                            .saturating_add(if ordinality { 2 } else { 0 })
                            .saturating_add(consumed);
                        i = paren_at.saturating_add(skipped);
                        continue;
                    }
                }
            }
        }
        match tok {
            Token::Punct('(') => depth = depth.saturating_add(1),
            Token::Punct(')') => depth = depth.saturating_sub(1),
            _ => {}
        }
        outer.push(tok.clone());
        i = i.saturating_add(1);
    }
    if found.is_empty() {
        return Ok(None);
    }
    // 別名なしの `unnest` が 2 個以上あるとき、公開名 `unnest` による修飾参照（`unnest.col`）は曖昧（`42712`）。
    if keep_alias
        && found
            .iter()
            .filter(|d| d.func.as_ref().is_some_and(|f| !f.alias_given))
            .count()
            > 1
        && outer.iter().any(|t| {
            matches!(t, Token::QualifiedIdent { qualifier, .. } if qualifier.eq_ignore_ascii_case("unnest"))
        })
    {
        return Err(SqlSurfaceError::duplicate_alias("unnest"));
    }
    Ok(Some((found, outer)))
}

/// 抽出した派生表を解決する。`ctes` は先行する利用者の CTE で、本文から見える。本文に入れ子の
/// 派生表があれば内側を先に解決して `scope_defs` へ積み、プレースホルダへ置き換えた本文で外側を
/// 解決する（外側の合成 CTE が必ず内側より後ろに並ぶ）。`42P10` は派生表用の variant へ写す。
fn resolve(
    raw: Vec<RawDerived>,
    outer_tokens: Vec<Token>,
    ctes: &[CteDef],
    st: &mut ExtractState,
    lookup: &impl TableLookup,
    limits: NestingLimits,
) -> Result<DerivedFromClause, SqlSurfaceError> {
    let mut scope_defs: Vec<CteDef> = Vec::new();
    // 左側を持つ LATERAL（先頭以外）があるときだけ、置換後の外側トークン列を構文解析して FROM の
    // relation 一覧を得る（構文 `42601` → 束縛の順序を保つ）。
    let needs_left = raw
        .iter()
        .any(|r| matches!(r.lateral, Some(f) if f != LateralForm::Leading));
    let outer_rels = if needs_left {
        Some(join_relations_of(&outer_tokens)?)
    } else {
        None
    };
    let tables = resolve_level(
        raw,
        ctes,
        &mut scope_defs,
        st,
        LevelCtx {
            depth_level: 1,
            outer_rels: outer_rels.as_deref(),
        },
        lookup,
        limits,
    )?;
    Ok(DerivedFromClause {
        tables,
        scope_defs,
        outer_tokens,
    })
}

/// [`resolve_level`] の階層の文脈（引数の個数を抑えるための束ね）。
#[derive(Clone, Copy)]
struct LevelCtx<'a> {
    /// 入れ子の深さ（最上位は 1）。
    depth_level: usize,
    /// 置換後の外側トークン列を構文解析した FROM の relation 一覧（LATERAL の左側を求める。
    /// 最上位で左側を持つ LATERAL があるときだけ `Some`）。
    outer_rels: Option<&'a [TableRef]>,
}

/// [`resolve`] の 1 階層分（`depth_level` は `raw` の入れ子深さ）。再帰の段数は
/// `ExtractState::max_depth`（運用者設定のサブクエリ深さ上限。ハード上限 4）で抑えられる。
fn resolve_level(
    raw: Vec<RawDerived>,
    ctes: &[CteDef],
    acc: &mut Vec<CteDef>,
    st: &mut ExtractState,
    level: LevelCtx<'_>,
    lookup: &impl TableLookup,
    limits: NestingLimits,
) -> Result<Vec<DerivedTable>, SqlSurfaceError> {
    let LevelCtx {
        depth_level,
        outer_rels,
    } = level;
    let mut tables: Vec<DerivedTable> = Vec::new();
    for r in raw {
        // 入れ子の派生表は本文の解析より前に切り出す（深さ超過 `54000` が本文の構文 `42601` に先行）。
        let body_tokens = match if r.func.is_some() {
            None
        } else {
            extract_at(&r.body, st, depth_level.saturating_add(1), false)?
        } {
            Some((inner, replaced)) => {
                resolve_level(
                    inner,
                    ctes,
                    acc,
                    st,
                    LevelCtx {
                        depth_level: depth_level.saturating_add(1),
                        outer_rels: None,
                    },
                    lookup,
                    limits,
                )?;
                replaced
            }
            None => r.body.clone(),
        };
        let placeholder = format!("{PLACEHOLDER_PREFIX}{}", r.number);
        let mut correlated_body: Option<Vec<Token>> = None;
        let mut inner_columns: Option<Vec<String>> = None;
        let mut unnest_spec: Option<(LateralUnnest, crate::sql::lateral_bind::LateralOuterRef)> =
            None;
        let (body_tokens, lateral_binding) = match r.lateral {
            None => (body_tokens, None),
            Some(form) => {
                let left: &[TableRef] = if form == LateralForm::Leading {
                    &[]
                } else {
                    let rels = outer_rels.unwrap_or(&[]);
                    let pos = rels
                        .iter()
                        .position(|t| t.table() == placeholder)
                        .ok_or_else(|| {
                            SqlSurfaceError::unsupported("LATERAL derived table is not a FROM item")
                        })?;
                    rels.get(..pos).unwrap_or(&[])
                };
                // 左側 relation の列集合（先行の派生表・利用者の CTE・実表）。
                let known: Vec<CteDef> = ctes.iter().chain(acc.iter()).cloned().collect();
                let mut columns = |t: &TableRef| left_relation_columns(t, &known, lookup, limits);
                // RIGHT／FULL は左側 relation を参照した時点で `42P10`（Issue #3323）。
                let reject_left =
                    matches!(form, LateralForm::Right { .. } | LateralForm::Full { .. });
                if let Some(f) = &r.func {
                    // `LATERAL unnest(<左側の配列列>)`（Issue #3360。L18〜L20）。引数を左側の列へ束縛し、
                    // 合成 CTE は「左側 relation の当該列を出力列数だけ投影する本文」にする（評価はしない。
                    // 公開列名・relation の解決と既存の検証を通すための足場で、行は外側タプルごとに
                    // `sql::join::lateral` が配列セルから作る）。
                    let arg = bind_lateral_unnest_arg(&r.body, left, &mut columns, reject_left)?;
                    let width = if f.ordinality { 2 } else { 1 };
                    if r.column_aliases.len() > width {
                        return Err(SqlSurfaceError::DerivedTableColumnListTooLong {
                            alias: r.alias.clone().unwrap_or_default(),
                            available: width,
                            specified: r.column_aliases.len(),
                        });
                    }
                    let rel = left
                        .get(arg.rel_index)
                        .ok_or_else(|| SqlSurfaceError::Internal {
                            detail: "unnest argument relation out of range".to_string(),
                        })?;
                    let mut names: Vec<String> = Vec::with_capacity(width);
                    names.push(match r.column_aliases.first() {
                        Some(n) => n.clone(),
                        None if f.alias_given => r.alias.clone().unwrap_or_default(),
                        None => "unnest".to_string(),
                    });
                    if f.ordinality {
                        names.push(
                            r.column_aliases
                                .get(1)
                                .cloned()
                                .unwrap_or_else(|| "ordinality".to_string()),
                        );
                    }
                    let mut scaffold: Vec<Token> = vec![Token::Keyword(Keyword::Select)];
                    for k in 0..width {
                        if k > 0 {
                            scaffold.push(Token::Punct(','));
                        }
                        scaffold.push(Token::Ident(arg.column.clone()));
                    }
                    scaffold.push(Token::Keyword(Keyword::From));
                    scaffold.push(Token::Ident(rel.table().to_string()));
                    unnest_spec = Some((
                        LateralUnnest {
                            ordinality: f.ordinality,
                            column_names: names,
                        },
                        arg.clone(),
                    ));
                    (
                        scaffold,
                        Some(LateralBinding {
                            outer_refs: vec![arg],
                        }),
                    )
                } else {
                    let bound = bind_lateral_body(&body_tokens, left, &mut columns, reject_left)?;
                    // 内側の別名による修飾（`t.col`）は本文の検証の前に非修飾へ正規化する。
                    let normalized = normalize_single_table_qualifiers(&bound.tokens)?.into_owned();
                    // 取り除いた相関連言の本文も、同じ正規化・別名除去を通す（Issue #3322）。
                    correlated_body = match bound.correlated_body {
                        Some(t) => Some(strip_inner_alias(
                            normalize_single_table_qualifiers(&t)?.into_owned(),
                        )),
                        None => None,
                    };
                    inner_columns = bound.inner_columns;
                    (strip_inner_alias(normalized), Some(bound.binding))
                }
            }
        };
        let (body, tail) = parse_cte_select_body(&body_tokens, limits)?;
        let mut def = CteDef {
            name: placeholder.clone(),
            column_aliases: match &unnest_spec {
                Some((u, _)) => u.column_names.clone(),
                None => r.column_aliases.clone(),
            },
            body,
            materialization: CteMaterialization::Default,
            tail,
        };
        // 左側の列を参照する LATERAL は、基底と後処理の 2 定義へ分ける（Issue #3322。L2）。組めない形は
        // 理由を控え、外側の文の検証を通したあとに固定文言の `0A000` で拒否する（`lateral_not_executable`）。
        let mut base_def: Option<CteDef> = None;
        let mut lateral_exec: Option<JoinLateral> = None;
        let mut lateral_reject: Option<String> = None;
        if let (Some(_), Some(binding)) = (r.lateral, lateral_binding.as_ref()) {
            if binding.is_correlated() && r.using_or_natural {
                // 相関 LATERAL の USING／NATURAL は PostgreSQL が受理するが、本実装は未対応（固定 `0A000`）。
                lateral_reject = Some(
                    "USING and NATURAL JOIN with a LATERAL subquery referencing outer columns are not supported yet"
                        .to_string(),
                );
            } else if let Some((u, arg)) = unnest_spec.take() {
                lateral_exec = Some(JoinLateral {
                    outer_refs: vec![(arg.rel_index, arg.column)],
                    correlated: Vec::new(),
                    unnest: Some(u),
                });
            } else if binding.is_correlated() {
                match plan_correlated_execution(
                    &placeholder,
                    &r.column_aliases,
                    &def,
                    &body_tokens,
                    correlated_body.as_deref(),
                    inner_columns.as_deref(),
                    binding,
                    limits,
                ) {
                    Ok((base, post, exec)) => {
                        base_def = Some(base);
                        def = post;
                        lateral_exec = Some(exec);
                    }
                    Err(reason) => lateral_reject = Some(reason),
                }
            }
        }
        let remap = |e: SqlSurfaceError| match e {
            SqlSurfaceError::CteColumnListTooLong {
                name,
                available,
                specified,
            } if name == placeholder => SqlSurfaceError::DerivedTableColumnListTooLong {
                alias: r.alias.clone().unwrap_or_default(),
                available,
                specified,
            },
            other => other,
        };
        let mut pending: Vec<CteDef> = base_def.into_iter().collect();
        pending.push(def);
        for def in pending {
            if ctes.len().saturating_add(acc.len()).saturating_add(1) > limits.max_cte_definitions()
            {
                return Err(SqlSurfaceError::payload_too_large(
                    "too many CTE definitions",
                ));
            }
            let mut scope: Vec<CteDef> = ctes.to_vec();
            scope.extend(acc.iter().cloned());
            scope.push(def);
            let idx = scope.len().saturating_sub(1);
            validate_cte_definition_at(&scope, idx, lookup, limits).map_err(remap)?;
            // 公開列名の静的な決定可否もここで検査する（解決できない列は `42703` 等）。
            derived_output_names(&scope, idx, lookup, limits).map_err(remap)?;
            let def = scope.pop().ok_or_else(|| SqlSurfaceError::Internal {
                detail: "derived table scope was empty".to_string(),
            })?;
            acc.push(def);
        }
        tables.push(DerivedTable {
            alias: r.alias,
            column_aliases: r.column_aliases,
            position: r.position,
            lateral: r.lateral,
            lateral_binding,
            placeholder,
            lateral_exec,
            lateral_reject,
        });
    }
    Ok(tables)
}

/// 左側 relation `t` の列集合（先行の派生表・利用者の CTE は公開列名、実表は列名）。ビューは列集合を
/// 返さず（`Ok(None)`。参照時に fail-closed）、左側 relation の存在は束縛より先に判定する（`42P01` が
/// `0A000` に先行）。`bind_lateral_body`・`bind_lateral_unnest_arg` が共有する。
fn left_relation_columns(
    t: &TableRef,
    known: &[CteDef],
    lookup: &impl TableLookup,
    limits: NestingLimits,
) -> Result<Option<Vec<String>>, SqlSurfaceError> {
    match known
        .iter()
        .position(|d| d.name.eq_ignore_ascii_case(t.table()))
    {
        Some(idx) => Ok(derived_output_names(known, idx, lookup, limits)?
            .into_iter()
            .collect::<Option<Vec<String>>>()),
        None => {
            if !lookup.table_exists(t.table())? && lookup.view_definition(t.table())?.is_none() {
                return Err(SqlSurfaceError::undefined_table(t.table()));
            }
            lookup.table_columns(t.table())
        }
    }
}

/// 左側の列を参照する LATERAL（`def` は外側参照を含む連言を取り除いた単一定義）を、基底と後処理の
/// 2 つの合成 CTE と、外側の行ごとにかける相関述語へ組む（Issue #3322。設計記録 L1・L2・L5・L8）。
/// 戻りは `(基底, 後処理, 実行情報)`。組めない形（集計・`DISTINCT`・窓・サブクエリ・
/// `SELECT *`）は固定文言の理由を `Err` で返す（呼び出し側が外側の文の検証後に `0A000` へ写す）。
#[allow(clippy::too_many_arguments)]
fn plan_correlated_execution(
    placeholder: &str,
    column_aliases: &[String],
    def: &CteDef,
    body_tokens: &[Token],
    correlated_body: Option<&[Token]>,
    inner_columns: Option<&[String]>,
    binding: &LateralBinding,
    limits: NestingLimits,
) -> Result<(CteDef, CteDef, JoinLateral), String> {
    let shape_unsupported = || {
        "aggregates, DISTINCT and window functions in a LATERAL subquery referencing outer columns are not supported yet"
            .to_string()
    };
    if def.select_body().is_none()
        || def
            .tail
            .as_ref()
            .is_some_and(|t| !t.window_items.is_empty())
    {
        return Err(shape_unsupported());
    }
    let (Some(correlated_body), Some(inner_columns)) = (correlated_body, inner_columns) else {
        return Err(
            "columns of the relation referenced by a LATERAL subquery cannot be resolved"
                .to_string(),
        );
    };
    // 内側の相関サブクエリは外側参照の再評価予算（相関サブクエリの再評価の上限）と二重に数えうる
    // ため、再評価する LATERAL の本文にはサブクエリを置かせない（L8。合計が上限の 2 倍になる経路を作らない）。
    let has_subquery = |t: &[Token]| {
        t.windows(2)
            .any(|w| matches!(w, [Token::Punct('('), Token::Keyword(Keyword::Select)]))
    };
    if has_subquery(body_tokens) || has_subquery(correlated_body) {
        return Err(
            "subqueries in a LATERAL subquery referencing outer columns are not supported yet"
                .to_string(),
        );
    }
    let base_name = format!("{placeholder}_base");
    let bodies = split_for_execution(body_tokens, &base_name, inner_columns, correlated_body)
        .map_err(|e| e.client_message())?;
    // 解析の失敗は理由を含めず固定文言にする（入力の断片・内部名を出さない）。
    let fixed = |_: SqlSurfaceError| "this LATERAL subquery shape is not supported yet".to_string();
    let (base_body, base_tail) = parse_cte_select_body(&bodies.base, limits).map_err(fixed)?;
    let (post_body, post_tail) = parse_cte_select_body(&bodies.post, limits).map_err(fixed)?;
    // 相関述語は列どうしの比較などを含むため、ビュー本文と同じ（広い）述語の判定表で解析する。評価できない形は
    // 実行段の束縛（`sql::join::lateral`）が拒否する。
    let corr_select = parse_view_body(correlated_body).map_err(fixed)?;
    if base_tail.is_some()
        || !matches!(base_body, crate::sql::cte::CteBody::Select(_))
        || !matches!(post_body, crate::sql::cte::CteBody::Select(_))
    {
        return Err(shape_unsupported());
    }
    let base = CteDef {
        name: base_name,
        column_aliases: Vec::new(),
        body: base_body,
        materialization: CteMaterialization::Default,
        tail: None,
    };
    let post = CteDef {
        name: placeholder.to_string(),
        column_aliases: column_aliases.to_vec(),
        body: post_body,
        materialization: CteMaterialization::Default,
        tail: post_tail,
    };
    let exec = JoinLateral {
        outer_refs: binding
            .outer_refs
            .iter()
            .map(|r| (r.rel_index, r.column.clone()))
            .collect(),
        correlated: corr_select.where_predicates,
        unnest: None,
    };
    Ok((base, post, exec))
}

/// 派生表を含むか（`$n` 束縛の fail-closed ガード用。`sql::params` から呼ばれる。第 2 の検出器を
/// 作らず [`extract`] を共有する。抽出が拒否した形も「含む」とみなす）。
pub(crate) fn contains_derived_table(tokens: &[Token]) -> bool {
    let mut st = ExtractState {
        count: 0,
        max_tables: crate::sql::relation::MAX_TABLE_REFS,
        max_depth: crate::sql::allowlist::MAX_SUBQUERY_DEPTH,
    };
    !matches!(extract_at(tokens, &mut st, 1, true), Ok(None))
}

/// 解決済みの派生表を実行形へ組む（#1866・#1867）。派生表は名前を利用者が書けない合成 CTE として、
/// 利用者の CTE の後ろへ付けて既存の経路へ流す（第 3 の実行器を作らない）。「派生表 1 件・`FROM` 直後・
/// 外側が JOIN／カンマ区切りでない」文は単一表 `WITH` 主クエリ経路
/// （`allowlist::single_relation_main_over_ctes`）、それ以外（派生表と実表・派生表どうしの JOIN、
/// カンマ区切り、複数件）は `WITH`＋JOIN 主クエリ経路（`allowlist::validate_with_join_main`。slot の
/// 評価・RLS・`JoinBudget` を CTE と共有）へ流す。いずれも参照した側セッションの `PolicyContext` で
/// 評価され、テナント情報は持ち込まない。
fn build_statement(
    clause: DerivedFromClause,
    ctes: &[CteDef],
    lookup: &impl TableLookup,
    limits: NestingLimits,
) -> Result<Statement, SqlSurfaceError> {
    let DerivedFromClause {
        tables,
        scope_defs,
        outer_tokens,
    } = clause;
    let mut scope: Vec<CteDef> = ctes.to_vec();
    scope.extend(scope_defs);
    let single = matches!(tables.as_slice(), [t] if t.position == DerivedPosition::From);
    let statement = if single && !looks_like_join(&outer_tokens) {
        single_relation_main_over_ctes(&scope, &outer_tokens, lookup, limits)
    } else if tables.iter().any(|t| t.alias.is_none()) {
        // 別名なしの派生表は公開名が内部名になるため、複数リレーションの FROM では受け付けない。
        Err(alias_required_in_join())
    } else {
        validate_with_join_main(&outer_tokens, &scope, lookup, limits)
    }?;
    // 実行できない LATERAL は、外側の文の構文・束縛まで通したあと固定 `0A000` で止める（書き込みなし）。
    if let Some(err) = lateral_not_executable(&tables) {
        return Err(err);
    }
    attach_lateral_inputs(statement, &tables)
}

/// LATERAL の `ON true` を書き換えた `CROSS JOIN` の段へ本来の結合種別（LEFT／RIGHT／FULL。Issue #3323）を
/// 設定し、外側の行ごとに再評価する LATERAL の実行情報（[`DerivedTable::lateral_exec`]。Issue #3322）を
/// JOIN の CTE 入力へリレーション位置を合わせて載せる。該当する LATERAL が無ければ何もしない。プレースホルダの
/// リレーションが見つからない・段が `CROSS`／`Inner` でない・CTE 入力が無い・単一表経路に来た場合は
/// `Internal`（fail-closed。到達しない不変条件）。非相関の LEFT／RIGHT／FULL も通常の派生表として、この
/// 種別設定だけで既存の結合実行器（直積経路の NULL 補完）に乗る。
fn attach_lateral_inputs(
    statement: Statement,
    tables: &[DerivedTable],
) -> Result<Statement, SqlSurfaceError> {
    let kind_of = |t: &DerivedTable| match t.lateral {
        Some(LateralForm::Left { on_true: true }) => Some(JoinKind::Left),
        Some(LateralForm::Right { on_true: true }) => Some(JoinKind::Right),
        Some(LateralForm::Full { on_true: true }) => Some(JoinKind::Full),
        _ => None,
    };
    if tables
        .iter()
        .all(|t| t.lateral_exec.is_none() && kind_of(t).is_none())
    {
        return Ok(statement);
    }
    let internal = || SqlSurfaceError::Internal {
        detail: "LATERAL execution inputs could not be attached".to_string(),
    };
    let Statement::Join(mut join) = statement else {
        return Err(internal());
    };
    let relations = join.relations.clone();
    for t in tables {
        let Some(kind) = kind_of(t) else { continue };
        let pos = relations
            .iter()
            .position(|r| r.table() == t.placeholder)
            .ok_or_else(internal)?;
        let step = pos
            .checked_sub(1)
            .and_then(|k| join.steps.get_mut(k))
            .ok_or_else(internal)?;
        if step.kind != JoinKind::Inner || step.condition != JoinCondition::Cross {
            return Err(internal());
        }
        step.kind = kind;
    }
    if tables.iter().all(|t| t.lateral_exec.is_none()) {
        return Ok(Statement::Join(join));
    }
    let Some(inputs) = join.cte_inputs.as_mut() else {
        return Err(internal());
    };
    inputs.lateral = vec![None; relations.len()];
    for t in tables {
        let Some(exec) = &t.lateral_exec else {
            continue;
        };
        let pos = relations
            .iter()
            .position(|r| r.table() == t.placeholder)
            .ok_or_else(internal)?;
        match inputs.lateral.get_mut(pos) {
            Some(slot) => *slot = Some(exec.clone()),
            None => return Err(internal()),
        }
    }
    Ok(Statement::Join(join))
}

/// 現時点で実行できない LATERAL の固定エラー。再評価の形へ組めなかったもの（`lateral_reject`。
/// 集計・`DISTINCT`・窓・サブクエリ・`SELECT *`・相関の USING／NATURAL）は拒否する。左側を参照しない
/// LATERAL と、参照する INNER／LEFT／CROSS／コンマの形は実行できるため `None`（Issue #3322・#3323）。
fn lateral_not_executable(tables: &[DerivedTable]) -> Option<SqlSurfaceError> {
    tables
        .iter()
        .find_map(|t| t.lateral_reject.as_ref())
        .map(|reason| SqlSurfaceError::feature_not_supported(reason.clone()))
}

/// セッション最上位の入口で、FROM に派生表を持つ `SELECT` 文を検出して検証し、実行形へ組む。
/// 派生表を含まない文は `Ok(None)`（呼び出し元は従来の検証へ進む）。派生表を含む文は
/// 構文 `42601` → 解決 `42P01`／`42703`／`42P10`／`42702` 等 → 実行対象外の `0A000` の順に判定する。
/// 先頭が `SELECT` でない文は対象外（`WITH` は [`gate_with`]）。
pub(crate) fn gate(
    tokens: &[Token],
    lookup: &impl TableLookup,
    limits: NestingLimits,
) -> Result<Option<Statement>, SqlSurfaceError> {
    if !matches!(tokens.first(), Some(Token::Keyword(Keyword::Select))) {
        return Ok(None);
    }
    let mut st = ExtractState::for_limits(limits);
    let Some((raw, outer)) = extract_at(tokens, &mut st, 1, true)? else {
        return Ok(None);
    };
    let clause = resolve(raw, outer, &[], &mut st, lookup, limits)?;
    build_statement(clause, &[], lookup, limits).map(Some)
}

/// [`gate`] の非再帰 `WITH` 版。呼び出し元は `allowlist::validate_with_statement`（セッション最上位の
/// 入口だけが `AllowMaterialize` で呼ぶ。`EngineCore` の WITH 入口は `validate_sql_tokens_impl` を
/// 経由しないため、ここで受ける）。主クエリの FROM から派生表を抽出し、利用者の CTE を先に検証してから
/// 派生表ごとに解決する。データ変更 CTE を含む文は、書き込みを発生させないよう解決の前に固定
/// `0A000` で止める。
pub(crate) fn gate_with(
    tokens: &[Token],
    lookup: &impl TableLookup,
    limits: NestingLimits,
) -> Result<Option<Statement>, SqlSurfaceError> {
    let (ctes, main_tokens) = parse_with_clause_and_main(tokens, limits)?;
    let mut st = ExtractState::for_limits(limits);
    let Some((raw, outer)) = extract_at(&main_tokens, &mut st, 1, true)? else {
        return Ok(None);
    };
    if ctes.iter().any(|d| d.is_data_modifying()) {
        return Err(crate::sql::cte::data_modifying_cte_unsupported());
    }
    validate_cte_definitions(&ctes, lookup, limits)?;
    let clause = resolve(raw, outer, &ctes, &mut st, lookup, limits)?;
    build_statement(clause, &ctes, lookup, limits).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::tokenize;

    fn found(sql: &str) -> Option<Vec<RawDerived>> {
        let tokens = tokenize(sql).expect("tokenize");
        extract(&tokens, 8).expect("extract").map(|(r, _)| r)
    }

    #[test]
    fn detects_from_join_and_comma_positions() {
        let r = found("SELECT * FROM (SELECT a FROM t) x").expect("from");
        assert_eq!(r.first().map(|d| d.position), Some(DerivedPosition::From));
        let r = found("SELECT * FROM t JOIN (SELECT a FROM t) x ON t.a = x.a").expect("join");
        assert_eq!(r.first().map(|d| d.position), Some(DerivedPosition::Join));
        let r = found("SELECT * FROM t, (SELECT a FROM t) AS x(b)").expect("comma");
        let d = r.first().expect("one");
        assert_eq!(d.position, DerivedPosition::CommaList);
        assert_eq!(d.alias.as_deref(), Some("x"));
        assert_eq!(d.column_aliases, vec!["b".to_string()]);
    }

    #[test]
    fn replaces_derived_table_with_placeholder() {
        let tokens = tokenize("SELECT * FROM (SELECT a FROM t) x WHERE b = 1").expect("tokenize");
        let (_, outer) = extract(&tokens, 8).expect("extract").expect("found");
        assert!(outer
            .iter()
            .any(|t| matches!(t, Token::Ident(n) if n.starts_with(PLACEHOLDER_PREFIX))));
        // 別名は暗黙別名の形でプレースホルダの直後に残る。
        let pos = outer
            .iter()
            .position(|t| matches!(t, Token::Ident(n) if n.starts_with(PLACEHOLDER_PREFIX)))
            .expect("placeholder");
        assert!(matches!(outer.get(pos + 1), Some(Token::Ident(n)) if n == "x"));
    }

    #[test]
    fn does_not_misdetect_other_subquery_positions() {
        for sql in [
            "SELECT (SELECT 1), a FROM t",
            "SELECT a, (SELECT 1) FROM t",
            "SELECT extract(year FROM ts) FROM t",
            "SELECT * FROM t WHERE a IN (SELECT a FROM t)",
            "SELECT * FROM t WHERE EXISTS (SELECT 1 FROM t)",
            "SELECT * FROM t WHERE a IS DISTINCT FROM (SELECT 1)",
            "SELECT * FROM t JOIN u USING (a)",
            "SELECT * FROM t UNION SELECT * FROM (SELECT a FROM t) x",
        ] {
            assert!(found(sql).is_none(), "{sql}");
        }
    }

    #[test]
    fn table_limit_is_rejected() {
        let tokens = tokenize("SELECT * FROM (SELECT 1 FROM t) a, (SELECT 1 FROM t) b").expect("t");
        assert_eq!(extract(&tokens, 1).expect_err("limit").wire_code(), "54000");
    }

    fn lateral_forms(sql: &str) -> Vec<Option<LateralForm>> {
        found(sql)
            .expect("found")
            .iter()
            .map(|d| d.lateral)
            .collect()
    }

    #[test]
    fn table_named_like_join_qualifier_is_not_a_qualifier() {
        for name in [
            "left", "inner", "right", "full", "cross", "outer", "natural",
        ] {
            assert_eq!(
                lateral_forms(&format!(
                    "SELECT * FROM {name} JOIN LATERAL (SELECT 1 FROM t) a ON true"
                )),
                vec![Some(LateralForm::Inner { on_true: true })],
                "{name}"
            );
        }
        let tokens =
            tokenize("SELECT * FROM left JOIN LATERAL (SELECT 1 FROM t) a ON true").expect("t");
        let (_, outer) = extract(&tokens, 5).expect("extract").expect("found");
        assert!(outer
            .iter()
            .any(|t| matches!(t, Token::Ident(n) if n.eq_ignore_ascii_case("left"))));
    }

    #[test]
    fn lateral_forms_are_recognized_and_keyword_is_dropped() {
        assert_eq!(
            lateral_forms("SELECT * FROM LATERAL (SELECT 1 FROM t) a"),
            vec![Some(LateralForm::Leading)]
        );
        assert_eq!(
            lateral_forms("SELECT * FROM t, LATERAL (SELECT 1 FROM t) a"),
            vec![Some(LateralForm::Comma)]
        );
        assert_eq!(
            lateral_forms("SELECT * FROM t CROSS JOIN LATERAL (SELECT 1 FROM t) a"),
            vec![Some(LateralForm::Cross)]
        );
        assert_eq!(
            lateral_forms("SELECT * FROM t JOIN LATERAL (SELECT 1 FROM t) a ON true"),
            vec![Some(LateralForm::Inner { on_true: true })]
        );
        assert_eq!(
            lateral_forms(
                "SELECT * FROM t INNER JOIN LATERAL (SELECT 1 FROM t) a(x) ON true WHERE 1 = 1"
            ),
            vec![Some(LateralForm::Inner { on_true: true })]
        );
        assert_eq!(
            lateral_forms("SELECT * FROM t LEFT OUTER JOIN LATERAL (SELECT 1 FROM t) a ON true"),
            vec![Some(LateralForm::Left { on_true: true })]
        );
        assert_eq!(
            lateral_forms("SELECT * FROM t LEFT JOIN LATERAL (SELECT 1 FROM t) a ON a.x = t.x"),
            vec![Some(LateralForm::Left { on_true: false })]
        );
        let tokens =
            tokenize("SELECT * FROM t INNER JOIN LATERAL (SELECT 1 FROM t) a ON true").expect("t");
        let (_, outer) = extract(&tokens, 8).expect("extract").expect("found");
        assert!(!outer
            .iter()
            .any(|t| matches!(t, Token::Ident(n) if n.eq_ignore_ascii_case("LATERAL") || n.eq_ignore_ascii_case("ON") || n.eq_ignore_ascii_case("INNER"))));
        assert!(outer
            .iter()
            .any(|t| matches!(t, Token::Ident(n) if n.eq_ignore_ascii_case("CROSS"))));
    }

    /// RIGHT／FULL／NATURAL／USING は抽出段では受理し、形と印だけを記録する（左側を参照する RIGHT／FULL は
    /// 束縛段が `42P10`、相関の USING／NATURAL は束縛後に固定 `0A000`。Issue #3323）。
    #[test]
    fn right_full_natural_and_using_are_recorded_by_the_extractor() {
        assert_eq!(
            lateral_forms("SELECT * FROM t RIGHT JOIN LATERAL (SELECT 1 FROM t) a ON true"),
            vec![Some(LateralForm::Right { on_true: true })]
        );
        assert_eq!(
            lateral_forms("SELECT * FROM t FULL OUTER JOIN LATERAL (SELECT 1 FROM t) a ON true"),
            vec![Some(LateralForm::Full { on_true: true })]
        );
        assert_eq!(
            lateral_forms("SELECT * FROM t FULL JOIN LATERAL (SELECT 1 FROM t) a ON a.x = t.x"),
            vec![Some(LateralForm::Full { on_true: false })]
        );
        for sql in [
            "SELECT * FROM t NATURAL JOIN LATERAL (SELECT 1 FROM t) a",
            "SELECT * FROM t NATURAL LEFT JOIN LATERAL (SELECT 1 FROM t) a",
            "SELECT * FROM t JOIN LATERAL (SELECT 1 AS k FROM t) a USING (k)",
            "SELECT * FROM t LEFT JOIN LATERAL (SELECT 1 AS k FROM t) a USING (k)",
        ] {
            assert!(
                found(sql)
                    .expect("found")
                    .iter()
                    .all(|d| d.using_or_natural),
                "{sql}"
            );
        }
        assert!(
            found("SELECT * FROM t LEFT JOIN LATERAL (SELECT 1 FROM t) a ON true")
                .expect("found")
                .iter()
                .all(|d| !d.using_or_natural)
        );
        // `ON true` は RIGHT／FULL でも `CROSS JOIN` へ書き換える（種別は束縛前に設定し直す）。
        let tokens =
            tokenize("SELECT * FROM t RIGHT JOIN LATERAL (SELECT 1 FROM t) a ON true").expect("t");
        let (_, outer) = extract(&tokens, 8).expect("extract").expect("found");
        assert!(outer
            .iter()
            .any(|t| matches!(t, Token::Ident(n) if n.eq_ignore_ascii_case("CROSS"))));
        assert!(!outer
            .iter()
            .any(|t| matches!(t, Token::Ident(n) if n.eq_ignore_ascii_case("RIGHT"))));
    }

    // tracked: #3324
    #[test]
    fn unsupported_lateral_shapes_are_rejected_with_0a000() {
        for sql in [
            "SELECT * FROM t, LATERAL (VALUES (1)) a",
            "SELECT * FROM t, LATERAL (WITH c AS (SELECT 1) SELECT 1) a",
        ] {
            let tokens = tokenize(sql).expect("t");
            assert_eq!(
                extract(&tokens, 8).expect_err(sql).wire_code(),
                "0A000",
                "{sql}"
            );
        }
    }

    /// Issue #3360: `LATERAL unnest(...)` は関数形として抽出され、`WITH ORDINALITY`・別名・列名リストを
    /// 受理する。別名なしは relation 名 `unnest` を合成する。`ON true` は `CROSS JOIN` へ書き換える。
    #[test]
    fn lateral_unnest_is_extracted_as_a_function_item() {
        let r = found("SELECT * FROM t CROSS JOIN LATERAL unnest(t.a) WITH ORDINALITY AS u(v, n)")
            .expect("found");
        let d = r.first().expect("one");
        assert_eq!(d.lateral, Some(LateralForm::Cross));
        assert_eq!(d.alias.as_deref(), Some("u"));
        assert_eq!(d.column_aliases, vec!["v".to_string(), "n".to_string()]);
        let f = d.func.as_ref().expect("function item");
        assert!(f.ordinality);
        assert!(f.alias_given);
        let r = found("SELECT * FROM t, LATERAL unnest(t.a)").expect("found");
        let d = r.first().expect("one");
        assert_eq!(d.alias.as_deref(), Some("unnest"));
        assert!(!d.func.as_ref().expect("function item").alias_given);
        assert!(!d.func.as_ref().expect("function item").ordinality);
        let tokens =
            tokenize("SELECT * FROM t LEFT JOIN LATERAL unnest(t.a) AS u(v) ON true WHERE 1 = 1")
                .expect("t");
        let (r, outer) = extract(&tokens, 8).expect("extract").expect("found");
        assert_eq!(
            r.first().map(|d| d.lateral),
            Some(Some(LateralForm::Left { on_true: true }))
        );
        assert!(outer
            .iter()
            .any(|t| matches!(t, Token::Ident(n) if n.eq_ignore_ascii_case("CROSS"))));
        assert!(!outer.iter().any(|t| matches!(t, Token::Punct('('))));
    }

    /// Issue #3360: `ROWS FROM`・`unnest` 以外の関数・`WITH` の後が `ORDINALITY` 以外・入れ子の中の
    /// 関数形は拒否する。`lateral` という名前の表は誤検出しない。
    #[test]
    fn unsupported_lateral_function_shapes_are_rejected() {
        for (sql, code) in [
            ("SELECT * FROM t, LATERAL ROWS FROM (unnest(t.a))", "0A000"),
            (
                "SELECT * FROM t, LATERAL generate_series(1, t.a) g",
                "0A000",
            ),
            (
                "SELECT * FROM t, LATERAL unnest(t.a) WITH foo AS u",
                "42601",
            ),
        ] {
            let tokens = tokenize(sql).expect("t");
            assert_eq!(
                extract(&tokens, 8).expect_err(sql).wire_code(),
                code,
                "{sql}"
            );
        }
        assert!(found("SELECT * FROM lateral JOIN t ON lateral.a = t.a").is_none());
        assert!(contains_lateral(
            &tokenize("SELECT * FROM t, LATERAL unnest(t.a)").expect("t")
        ));
        assert!(contains_lateral(
            &tokenize("SELECT * FROM t, LATERAL ROWS FROM (f(1))").expect("t")
        ));
        assert!(!contains_lateral(
            &tokenize("SELECT * FROM lateral JOIN t ON true").expect("t")
        ));
    }

    #[test]
    fn nested_lateral_is_rejected_at_depth_two() {
        let body = tokenize("SELECT 1 FROM t, LATERAL (SELECT 1 FROM t) a").expect("t");
        let mut st = ExtractState {
            count: 0,
            max_tables: 8,
            max_depth: 4,
        };
        let err = extract_at(&body, &mut st, 2, false).expect_err("nested lateral");
        assert_eq!(err.wire_code(), "0A000");
    }

    #[test]
    fn placeholder_detection_is_narrow() {
        assert!(is_derived_placeholder("?derived1"));
        assert!(is_derived_placeholder("?derived42"));
        for n in [
            "?derived",
            "?derivedx",
            "derived1",
            "?derived1 ",
            "docs",
            "?Derived1",
        ] {
            assert!(!is_derived_placeholder(n), "{n}");
        }
    }

    #[test]
    fn users_cannot_write_a_placeholder_name() {
        // 字句解析器は `?` を含む識別子を作らず、引用形も受理しない（JOIN パーサーの例外の根拠）。
        assert!(tokenize("SELECT * FROM \"?derived1\"").is_err());
        let toks = tokenize("SELECT * FROM ?derived1");
        assert!(toks.map_or(true, |t| !t
            .iter()
            .any(|x| matches!(x, Token::Ident(n) if is_derived_placeholder(n)))));
    }

    #[test]
    fn nested_extraction_numbers_are_unique_and_body_drops_alias() {
        let tokens =
            tokenize("SELECT * FROM (SELECT a FROM (SELECT a FROM t) y) x, (SELECT 1 FROM t) z")
                .expect("tokenize");
        let mut st = ExtractState {
            count: 0,
            max_tables: 8,
            max_depth: 4,
        };
        let (raws, _) = extract_at(&tokens, &mut st, 1, true)
            .expect("extract")
            .expect("found");
        assert_eq!(
            raws.iter().map(|r| r.number).collect::<Vec<_>>(),
            vec![1, 2]
        );
        let (inner, replaced) = extract_at(&raws[0].body, &mut st, 2, false)
            .expect("nested")
            .expect("found");
        // 入れ子の番号は最上位と重ならず、本文側にはプレースホルダだけが残る（別名なし）。
        assert_eq!(inner.iter().map(|r| r.number).collect::<Vec<_>>(), vec![3]);
        assert!(!replaced
            .iter()
            .any(|t| matches!(t, Token::Ident(n) if n == "y")));
        assert!(replaced
            .iter()
            .any(|t| matches!(t, Token::Ident(n) if n == "?derived3")));
    }

    #[test]
    fn depth_limit_is_checked_before_parsing_the_body() {
        let tokens = tokenize("SELECT * FROM (SELECT a FROM t) x").expect("tokenize");
        let mut st = ExtractState {
            count: 0,
            max_tables: 8,
            max_depth: 1,
        };
        assert!(extract_at(&tokens, &mut st, 1, true).is_ok());
        let err = extract_at(&tokens, &mut st, 2, true).expect_err("depth 2 > 1");
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn contains_derived_table_sees_join_and_nested_forms() {
        for sql in [
            "SELECT * FROM t JOIN (SELECT a FROM t) x ON t.a = x.a",
            "SELECT * FROM (SELECT a FROM (SELECT a FROM t) y) x",
        ] {
            assert!(contains_derived_table(&tokenize(sql).expect("t")), "{sql}");
        }
        assert!(!contains_derived_table(
            &tokenize("SELECT * FROM t WHERE a IN (SELECT a FROM t)").expect("t")
        ));
    }
}
