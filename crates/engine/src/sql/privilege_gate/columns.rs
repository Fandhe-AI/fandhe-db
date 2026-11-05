//! 列単位の権限判定の入力（要求する列の仕様・解決・充足判定。SQL-40 ポインタ。Issue #2083。
//! ADR `docs/design/column-level-privilege.md` の C1〜C5）。
//!
//! 役割: relation への要求が「どの列に」かかるかを [`ColumnSpec`] で表し、relation の形
//! （`RelationShape`）と文のトークン列から [`ColumnDemand`] へ解決して、列単位の付与
//! （`RelationGrants` の列成分）で満たせるかを [`cover_bits`] が判定する。許否の判定そのものは
//! `privilege_gate.rs` の [`super::check`] だけが持つ（本ファイルは `column_cover_bits` を計算するだけ）。
//!
//! 呼び出し元: `read.rs` の ACL 照会点（`evaluate_in_txn`）。`write.rs`・`copy.rs`・HTTP 表層の入口は
//! [`Want`] に [`ColumnSpec`] を載せて同じ照会点へ渡す。
//!
//! 契約（いずれも厳しい側へ倒す）:
//! - 列集合は束縛の結果ではなく、名前解決済みトークン列の識別子と relation の列名の照合による上位近似
//!   （同名の別名・関数名・他表の列まで要求に入ることはあっても、取りこぼしは起きない）。この近似を使うのは
//!   単一 relation の読み取りの形（呼び出し元が `Scan` を指定した場合）だけで、それ以外は全列（`All`）。
//! - `*`・`t.*`（`count(*)` を除く）・`NATURAL`・`USING PLAN` は暗黙に全列を読むので `All`。
//! - 疑似列 `id`（従来方式の表の行キー）とビュー・matview は列単位の付与を持てないので `TableOnly`。
//! - 列を1つも参照しない形（`count(*)`・`SELECT 1 FROM t`）は `Any`（いずれかの列への付与が要る）。
//! - 列名の数は [`MAX_COLUMN_DEMANDS`] で抑える（`54000`・固定文言）。

use std::collections::BTreeSet;

use crate::catalog::privilege::{
    RelationShape, PRIV_INSERT, PRIV_REFERENCES, PRIV_SELECT, PRIV_UPDATE,
};
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::lexer::Token;

/// 列単位の付与で満たせる権限ビット（`GRANT` の列形が受理する集合と同じ）。
pub(crate) const COLUMN_PRIVILEGES: u16 = PRIV_SELECT | PRIV_INSERT | PRIV_UPDATE | PRIV_REFERENCES;

/// 1 文の列要求（重複除去後の列名の数）の上限。導出: `relation::MAX_TABLE_REFS`（8）×
/// `catalog::MAX_COLUMN_COUNT`（256）。超過は `54000`（固定文言）。
pub(crate) const MAX_COLUMN_DEMANDS: usize = 2048;

/// 超過時の固定文言（列名・relation 名を含めない）。
const TOO_MANY_COLUMNS: &str = "too many column references";

/// relation への要求が、どの列にかかるか（未解決。解決は [`resolve`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ColumnSpec {
    /// 表単位の権限だけが満たす（`DELETE`・`TRUNCATE` など列で満たせないビット）。
    TableOnly,
    /// 対象の全列（疑似列 `id` を含む）。判別できない形の既定。
    All,
    /// いずれかの 1 列（列を参照しない読み取り、行ロック句の `UPDATE`）。
    Any,
    /// 構文木が確定した列名（`INSERT` の列リスト・`UPDATE` の `SET` 対象）。
    Names(Vec<String>),
    /// 文全体のトークン列から列を集める（単一 relation の読み取りの形だけ）。
    Scan,
    /// [`Self::Scan`] から、書き込み先の列の宣言位置（`INSERT` の列リスト・`UPDATE` の `SET` 左辺）を除く。
    ScanExcludingWrites,
}

/// 1 relation への 1 組の要求（権限ビット × 列の仕様）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Want {
    pub(crate) relation: String,
    pub(crate) bits: u16,
    pub(crate) cols: ColumnSpec,
}

impl Want {
    /// 全列への要求（判別できない形・HTTP 表層の既定）。列で満たせないビットは表単位だけが満たす。
    pub(crate) fn all(relation: &str, bits: u16) -> Vec<Want> {
        Self::split(relation, bits, ColumnSpec::All)
    }

    /// `bits` のうち列で満たせるビットに `cols` を、それ以外に `TableOnly` を課す。
    pub(crate) fn split(relation: &str, bits: u16, cols: ColumnSpec) -> Vec<Want> {
        let mut out = Vec::new();
        let colbits = bits & COLUMN_PRIVILEGES;
        let rest = bits & !COLUMN_PRIVILEGES;
        if colbits != 0 {
            out.push(Want {
                relation: relation.to_string(),
                bits: colbits,
                cols,
            });
        }
        if rest != 0 {
            out.push(Want {
                relation: relation.to_string(),
                bits: rest,
                cols: ColumnSpec::TableOnly,
            });
        }
        out
    }
}

/// 解決済みの列要求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ColumnDemand {
    TableOnly,
    All,
    Any,
    /// 具体的な列名（relation の列に実在するものだけ。非空）。
    Columns(BTreeSet<String>),
}

fn too_many() -> SqlSurfaceError {
    SqlSurfaceError::payload_too_large(TOO_MANY_COLUMNS)
}

/// 構文木が確定した列名の数を上限と照らす（ACL の照会より前の検査）。
pub(crate) fn check_explicit_count(wants: &[Want]) -> Result<(), SqlSurfaceError> {
    let mut total = 0usize;
    for w in wants {
        if let ColumnSpec::Names(v) = &w.cols {
            total = total.saturating_add(v.len());
            if total > MAX_COLUMN_DEMANDS {
                return Err(too_many());
            }
        }
    }
    Ok(())
}

/// [`ColumnSpec`] を relation の形とトークン列から [`ColumnDemand`] へ解決する。`single_relation` は文が
/// 参照する実在の relation が 1 つだけか（偽なら `Scan` は精密化せず `All`）。
pub(crate) fn resolve(
    spec: &ColumnSpec,
    shape: &RelationShape,
    tokens: &[Token],
    single_relation: bool,
) -> Result<ColumnDemand, SqlSurfaceError> {
    if shape.view_like {
        return Ok(ColumnDemand::TableOnly);
    }
    match spec {
        ColumnSpec::TableOnly => Ok(ColumnDemand::TableOnly),
        ColumnSpec::All => Ok(ColumnDemand::All),
        ColumnSpec::Any => Ok(ColumnDemand::Any),
        ColumnSpec::Names(names) => {
            let mut set = BTreeSet::new();
            for n in names {
                if shape.pseudo_id && n == "id" {
                    return Ok(ColumnDemand::TableOnly);
                }
                // 実在しない名前は要求に入れない（実行段の束縛が `42703` にする）。
                if shape.columns.iter().any(|c| c == n) {
                    set.insert(n.clone());
                }
            }
            Ok(finish(set))
        }
        ColumnSpec::Scan | ColumnSpec::ScanExcludingWrites => {
            if !single_relation {
                return Ok(ColumnDemand::All);
            }
            let excluded = if matches!(spec, ColumnSpec::ScanExcludingWrites) {
                write_target_exclusions(tokens)
            } else {
                Vec::new()
            };
            scan(tokens, &excluded, shape)
        }
    }
}

fn finish(set: BTreeSet<String>) -> ColumnDemand {
    if set.is_empty() {
        ColumnDemand::Any
    } else {
        ColumnDemand::Columns(set)
    }
}

/// トークンが列名として参照しうる文字列（識別子系のトークンだけ。キーワードは構造語で列名にならない）。
fn ident_names(tok: &Token) -> Vec<&str> {
    match tok {
        Token::Ident(s) | Token::QuotedNull(s) | Token::QuotedBoolWord(s) => vec![s.as_str()],
        Token::QualifiedIdent { qualifier, name } => vec![qualifier.as_str(), name.as_str()],
        _ => Vec::new(),
    }
}

fn is_ident(tok: Option<&Token>, word: &str) -> bool {
    matches!(tok, Some(Token::Ident(s)) if s.eq_ignore_ascii_case(word))
}

/// `tokens[i]`（`*`）が `count(*)` の引数か。
fn is_count_star(tokens: &[Token], i: usize) -> bool {
    i >= 2
        && matches!(tokens.get(i - 1), Some(Token::Punct('(')))
        && matches!(tokens.get(i + 1), Some(Token::Punct(')')))
        && is_ident(tokens.get(i - 2), "count")
}

/// 単一 relation の読み取りの形の列集合（上位近似）。
fn scan(
    tokens: &[Token],
    excluded: &[usize],
    shape: &RelationShape,
) -> Result<ColumnDemand, SqlSurfaceError> {
    let mut set: BTreeSet<String> = BTreeSet::new();
    let mut id_hit = false;
    for (i, tok) in tokens.iter().enumerate() {
        if excluded.contains(&i) {
            continue;
        }
        match tok {
            Token::QualifiedStar { .. } => return Ok(ColumnDemand::All),
            Token::Punct('*') if !is_count_star(tokens, i) => return Ok(ColumnDemand::All),
            _ => {}
        }
        if is_ident(Some(tok), "natural") {
            return Ok(ColumnDemand::All);
        }
        if is_ident(Some(tok), "plan") && i > 0 && is_ident(tokens.get(i - 1), "using") {
            // `USING PLAN(...)` は文字列から検索対象の列を決める。
            return Ok(ColumnDemand::All);
        }
        for name in ident_names(tok) {
            if shape.pseudo_id && name.eq_ignore_ascii_case("id") {
                id_hit = true;
            }
            for c in &shape.columns {
                if c.eq_ignore_ascii_case(name) {
                    set.insert(c.clone());
                }
            }
            if set.len() > MAX_COLUMN_DEMANDS {
                return Err(too_many());
            }
        }
    }
    if id_hit {
        return Ok(ColumnDemand::TableOnly);
    }
    Ok(finish(set))
}

/// 書き込み先の列の宣言位置（`INSERT INTO t (a, b)` の列リスト・`UPDATE t SET a = ..., b = ...` の左辺）の
/// トークン添字。位置を確定できない形は何も除かない（読み取りの要求が増える厳しい側）。
fn write_target_exclusions(tokens: &[Token]) -> Vec<usize> {
    let mut out = Vec::new();
    if is_ident(tokens.first(), "insert") {
        if is_ident(tokens.get(1), "into")
            && matches!(
                tokens.get(2),
                Some(Token::Ident(_) | Token::QualifiedIdent { .. })
            )
            && matches!(tokens.get(3), Some(Token::Punct('(')))
        {
            let mut i = 4;
            while let Some(t) = tokens.get(i) {
                match t {
                    Token::Punct(')') => break,
                    Token::Ident(_) => out.push(i),
                    _ => {}
                }
                i += 1;
            }
        }
    } else if is_ident(tokens.first(), "update") {
        let mut depth = 0usize;
        let mut set_at = None;
        for (i, t) in tokens.iter().enumerate().skip(1) {
            match t {
                Token::Punct('(') => depth += 1,
                Token::Punct(')') => depth = depth.saturating_sub(1),
                Token::Ident(s) if depth == 0 && s.eq_ignore_ascii_case("set") => {
                    set_at = Some(i);
                    break;
                }
                _ => {}
            }
        }
        if let Some(set_at) = set_at {
            let mut i = set_at + 1;
            let mut depth = 0usize;
            let mut expect_lhs = true;
            while let Some(t) = tokens.get(i) {
                match t {
                    Token::Keyword(_) if depth == 0 => break,
                    Token::Ident(s) if depth == 0 && s.eq_ignore_ascii_case("returning") => break,
                    Token::Punct('(') => depth += 1,
                    Token::Punct(')') => depth = depth.saturating_sub(1),
                    Token::Punct(',') if depth == 0 => expect_lhs = true,
                    Token::Ident(_)
                        if depth == 0
                            && expect_lhs
                            && matches!(tokens.get(i + 1), Some(Token::Punct('='))) =>
                    {
                        out.push(i);
                        expect_lhs = false;
                    }
                    _ => expect_lhs = false,
                }
                i += 1;
            }
        }
    }
    out
}

/// 表単位では満たせない要求ビット `required` のうち、解決済みの要求 `demand` を列単位の付与
/// （`column_bits`）で満たせるビットを返す。満たせるのは [`COLUMN_PRIVILEGES`] のビットだけで、そのビットを
/// 持つ列が 1 つも無いときは要求の中身によらず満たさない（付与の無いロールに列の解決結果を見せない）。
pub(crate) fn cover_bits(
    required: u16,
    table_bits: u16,
    column_bits: &[(String, u16)],
    demand: &ColumnDemand,
    shape: &RelationShape,
) -> u16 {
    let mut cover = 0u16;
    for bit in [PRIV_SELECT, PRIV_INSERT, PRIV_UPDATE, PRIV_REFERENCES] {
        if required & bit == 0 || table_bits & bit != 0 {
            continue;
        }
        let has: BTreeSet<&str> = column_bits
            .iter()
            .filter(|(_, b)| b & bit != 0)
            .map(|(c, _)| c.as_str())
            .collect();
        if has.is_empty() {
            continue;
        }
        let ok = match demand {
            ColumnDemand::TableOnly => false,
            ColumnDemand::Any => true,
            ColumnDemand::All => {
                !shape.view_like
                    && !shape.pseudo_id
                    && !shape.columns.is_empty()
                    && shape.columns.iter().all(|c| has.contains(c.as_str()))
            }
            ColumnDemand::Columns(set) => set.iter().all(|c| has.contains(c.as_str())),
        };
        if ok {
            cover |= bit;
        }
    }
    cover
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::tokenize;

    fn shape() -> RelationShape {
        RelationShape {
            columns: vec!["a".into(), "b".into(), "c".into()],
            pseudo_id: false,
            view_like: false,
        }
    }

    fn demand(sql: &str, spec: ColumnSpec, shape: &RelationShape) -> ColumnDemand {
        let tokens = tokenize(sql).expect("tokenize");
        resolve(&spec, shape, &tokens, true).expect("resolve")
    }

    fn cols(names: &[&str]) -> ColumnDemand {
        ColumnDemand::Columns(names.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn scan_collects_named_columns_and_widens_on_implicit_reads() {
        let s = shape();
        assert_eq!(
            demand("SELECT a FROM t WHERE b = 1 LIMIT 5", ColumnSpec::Scan, &s),
            cols(&["a", "b"])
        );
        assert_eq!(
            demand("SELECT * FROM t LIMIT 5", ColumnSpec::Scan, &s),
            ColumnDemand::All
        );
        assert_eq!(
            demand("SELECT count(*) FROM t", ColumnSpec::Scan, &s),
            ColumnDemand::Any
        );
        assert_eq!(
            demand("SELECT A FROM t", ColumnSpec::Scan, &s),
            cols(&["a"]),
            "case-insensitive superset"
        );
    }

    #[test]
    fn pseudo_id_view_and_multi_relation_are_strict() {
        let mut s = shape();
        s.pseudo_id = true;
        assert_eq!(
            demand("SELECT id FROM t", ColumnSpec::Scan, &s),
            ColumnDemand::TableOnly
        );
        assert_eq!(
            demand("x", ColumnSpec::Names(vec!["id".into()]), &s),
            ColumnDemand::TableOnly
        );
        let view = RelationShape {
            columns: Vec::new(),
            pseudo_id: false,
            view_like: true,
        };
        assert_eq!(
            demand("SELECT a FROM v", ColumnSpec::Scan, &view),
            ColumnDemand::TableOnly
        );
        let tokens = tokenize("SELECT a FROM t").expect("tokenize");
        assert_eq!(
            resolve(&ColumnSpec::Scan, &shape(), &tokens, false).expect("resolve"),
            ColumnDemand::All
        );
    }

    #[test]
    fn write_target_positions_are_excluded_from_the_read_scan() {
        let s = shape();
        assert_eq!(
            demand(
                "UPDATE t SET b = 1 WHERE a = 2",
                ColumnSpec::ScanExcludingWrites,
                &s
            ),
            cols(&["a"])
        );
        assert_eq!(
            demand(
                "UPDATE t SET b = b + 1 WHERE a = 2",
                ColumnSpec::ScanExcludingWrites,
                &s
            ),
            cols(&["a", "b"])
        );
        assert_eq!(
            demand(
                "INSERT INTO t (a, b) VALUES (1, 2) RETURNING c",
                ColumnSpec::ScanExcludingWrites,
                &s
            ),
            cols(&["c"])
        );
    }

    #[test]
    fn cover_requires_a_grant_on_every_demanded_column() {
        let s = shape();
        let granted = vec![
            ("a".to_string(), PRIV_SELECT),
            ("b".to_string(), PRIV_SELECT),
        ];
        assert_eq!(
            cover_bits(PRIV_SELECT, 0, &granted, &cols(&["a", "b"]), &s),
            PRIV_SELECT
        );
        assert_eq!(
            cover_bits(PRIV_SELECT, 0, &granted, &cols(&["a", "c"]), &s),
            0
        );
        assert_eq!(
            cover_bits(PRIV_SELECT, 0, &granted, &ColumnDemand::All, &s),
            0
        );
        assert_eq!(
            cover_bits(PRIV_SELECT, 0, &granted, &ColumnDemand::Any, &s),
            PRIV_SELECT
        );
        // 付与の無いビットは、要求の中身によらず満たさない。
        assert_eq!(
            cover_bits(PRIV_UPDATE, 0, &granted, &ColumnDemand::Any, &s),
            0
        );
        // 列で満たせないビット（DELETE）は決して満たさない。
        let del = crate::catalog::privilege::PRIV_DELETE;
        assert_eq!(cover_bits(del, 0, &granted, &ColumnDemand::Any, &s), 0);
        assert_eq!(
            cover_bits(PRIV_SELECT, 0, &granted, &ColumnDemand::TableOnly, &s),
            0
        );
    }

    #[test]
    fn explicit_column_count_is_capped_before_any_acl_lookup() {
        let names: Vec<String> = (0..=MAX_COLUMN_DEMANDS).map(|i| format!("c{i}")).collect();
        let wants = Want::split("t", PRIV_INSERT, ColumnSpec::Names(names));
        let err = check_explicit_count(&wants).expect_err("over cap");
        assert_eq!(err.wire_code(), "54000");
        assert!(!err.client_message().contains("c0"));
    }
}
