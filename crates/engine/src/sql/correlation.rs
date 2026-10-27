//! 相関参照（部分クエリの内側から外側の列を参照する形）の検出と外側スコープへの束縛
//! （SQL-61 ポインタ、Issue #1841、親 #1840）。
//!
//! 責務境界: `sql::subquery` の内側クエリ事前検査（`inner_scan_prechecks`／
//! `inner_aggregate_prechecks`）が、内側の参照名集合・内側スキーマ・外側スコープ連鎖を渡して
//! [`bind_correlation`] を呼び、`sql::qualified_column` が外側表で修飾された参照
//! （`a.id` 等）を本モジュールの予約内部名（[`outer_ref_placeholder`]）へ置き換える。
//! 名前解決は PostgreSQL と同じく「内側優先、そこから外側へ近い順」で行い、結果を
//! [`Correlation`]（相関の有無と外側列参照の一覧）として返す。これが後続の半結合・反結合への
//! 書き換え（#1842 以降）が受け取る中間表現（IR）である。
//!
//! エラー契約（fail-closed）:
//! - 同一スコープで 2 表以上に該当する非修飾名は `42702`。
//! - 外側表で修飾した参照の列が存在しなければ `42703`。
//! - どのスコープにも無い非修飾名はここでは拒否しない（内側の束縛が従来どおり `42703` にする。
//!   `ORDER BY` の出力別名など列ではない識別子がここへ来るため）。
//! - 相関 `EXISTS`（Issue #1842）・`NOT EXISTS`（Issue #1843）・`IN`／`NOT IN`（Issue #1844）は `sql::subquery` が本 IR を使って
//!   半結合・反結合・行ごとの再評価へ書き換えて実行する（`sql::correlated_exists`）。それ以外
//!   （スカラー・`ANY`／`ALL`・述語形 DML・集計の `FILTER`）は、束縛に成功した相関を [`reject_if_correlated`] が
//!   `42601` で拒否する。
//!
//! エラー文言は静的文字列と利用者自身が書いた列名だけを含め、予約接頭辞（制御文字）・
//! 他テナントの情報は含めない。束縛はスキーマだけを参照し、行データは読まない
//! （RLS-10 ポインタ。相関の判定が他テナントの可視行に依存しない）。

use std::collections::HashSet;

use crate::catalog::{ColumnType, TableSchema};
use crate::sql::allowlist::SqlSurfaceError;

/// 外側表で修飾された参照を表す予約内部名の接頭辞。先頭の制御文字（U+0002）により、
/// 利用者の識別子とは衝突せず、既存の予約接頭辞（U+0000／U+0001 始まり）とも重ならない。
pub(crate) const OUTER_REF_PREFIX: &str = "\u{2}outer";

/// 外側参照の予約内部名 `"\u{2}outer<levels_up>.<name>"` を組み立てる。
/// `levels_up` は直近の外側を 1 とする段数。`sql::qualified_column` が呼ぶ。
pub(crate) fn outer_ref_placeholder(levels_up: usize, name: &str) -> String {
    format!("{OUTER_REF_PREFIX}{levels_up}.{name}")
}

/// 予約内部名を `(levels_up, 列名)` へ分解する。予約名でない・数値が不正・
/// `levels_up == 0` は `None`（利用者の識別子は予約名として解釈されない）。
pub(crate) fn parse_outer_ref_placeholder(ident: &str) -> Option<(usize, &str)> {
    let rest = ident.strip_prefix(OUTER_REF_PREFIX)?;
    let (digits, name) = rest.split_once('.')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let levels_up: usize = digits.parse().ok()?;
    if levels_up == 0 {
        return None;
    }
    Some((levels_up, name))
}

/// 外側スコープの列への 1 つの参照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OuterColumnRef {
    /// 直近の外側を 1 とする段数。
    pub(crate) levels_up: usize,
    /// 外側の列名（疑似列 `id` を含む）。
    pub(crate) column: String,
    /// 疑似列 `id` への参照か。
    pub(crate) pseudo_id: bool,
    /// 外側の列型。疑似列 `id` は `None`。
    pub(crate) column_type: Option<ColumnType>,
}

/// 束縛結果（相関の IR）。`(levels_up, column)` で重複を除き、並びは決定的。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Correlation {
    outer_refs: Vec<OuterColumnRef>,
}

impl Correlation {
    /// 外側への参照が 1 つでもあるか。
    pub(crate) fn is_correlated(&self) -> bool {
        !self.outer_refs.is_empty()
    }

    /// 外側列への参照一覧（`levels_up`・列名の昇順）。
    pub(crate) fn outer_refs(&self) -> &[OuterColumnRef] {
        &self.outer_refs
    }
}

/// 外側スコープ 1 段分。現状の各段は 1 表だが、同一段の複数表（`42702`）を表せる形にする。
pub(crate) struct ScopeLevel<'a> {
    pub(crate) relations: Vec<&'a TableSchema>,
}

/// 段内で `name` に該当する表の数（疑似列 `id` を含む）と、最初に該当した表。
fn matches_in_level<'a>(level: &ScopeLevel<'a>, name: &str) -> (usize, Option<&'a TableSchema>) {
    let mut count = 0usize;
    let mut first = None;
    for rel in &level.relations {
        let has =
            rel.columns.iter().any(|c| c.name == name) || (name == "id" && rel.has_pseudo_id());
        if has {
            count += 1;
            if first.is_none() {
                first = Some(*rel);
            }
        }
    }
    (count, first)
}

fn outer_ref_from(levels_up: usize, name: &str, rel: &TableSchema) -> OuterColumnRef {
    match rel.columns.iter().find(|c| c.name == name) {
        Some(col) => OuterColumnRef {
            levels_up,
            column: name.to_string(),
            pseudo_id: false,
            column_type: Some(col.ty.clone()),
        },
        None => OuterColumnRef {
            levels_up,
            column: name.to_string(),
            pseudo_id: true,
            column_type: None,
        },
    }
}

/// [`bind_correlation`] の段リスト版（`levels` は最外側が先頭、直近の外側が末尾）。
pub(crate) fn bind_correlation_levels(
    referenced: &HashSet<String>,
    inner: &TableSchema,
    levels: &[ScopeLevel<'_>],
) -> Result<Correlation, SqlSurfaceError> {
    // HashSet の走査順に依存しないよう、名前順に処理する（エラーの選択を決定的にする）。
    let mut names: Vec<&String> = referenced.iter().collect();
    names.sort();
    let mut refs: Vec<OuterColumnRef> = Vec::new();
    for ident in names {
        if let Some((levels_up, column)) = parse_outer_ref_placeholder(ident) {
            // 段数が連鎖の範囲外なら内部不整合として fail-closed にする。
            let level = levels
                .len()
                .checked_sub(levels_up)
                .and_then(|idx| levels.get(idx))
                .ok_or_else(|| {
                    SqlSurfaceError::unsupported("correlated column reference is not supported")
                })?;
            match matches_in_level(level, column) {
                (0, _) | (_, None) => return Err(SqlSurfaceError::undefined_column(column)),
                (_, Some(rel)) => refs.push(outer_ref_from(levels_up, column, rel)),
            }
            continue;
        }
        let in_inner = inner.columns.iter().any(|c| &c.name == ident)
            || (ident == "id" && inner.has_pseudo_id());
        if in_inner {
            continue;
        }
        for (steps, level) in levels.iter().rev().enumerate() {
            match matches_in_level(level, ident) {
                (0, _) | (_, None) => {}
                (1, Some(rel)) => {
                    refs.push(outer_ref_from(steps + 1, ident, rel));
                    break;
                }
                _ => return Err(SqlSurfaceError::ambiguous_column(ident.as_str())),
            }
        }
    }
    refs.sort_by(|a, b| (a.levels_up, &a.column).cmp(&(b.levels_up, &b.column)));
    refs.dedup_by(|a, b| a.levels_up == b.levels_up && a.column == b.column);
    Ok(Correlation { outer_refs: refs })
}

/// 内側クエリの参照名集合を、内側スキーマと外側スコープ連鎖（最外側が先頭、直近の外側が末尾。
/// `sql::subquery::resolve_where_predicates` の `outer_scopes` と同じ並び）に対して束縛する。
pub(crate) fn bind_correlation(
    referenced: &HashSet<String>,
    inner: &TableSchema,
    outer_scopes: &[&TableSchema],
) -> Result<Correlation, SqlSurfaceError> {
    let levels: Vec<ScopeLevel<'_>> = outer_scopes
        .iter()
        .map(|s| ScopeLevel {
            relations: vec![*s],
        })
        .collect();
    bind_correlation_levels(referenced, inner, &levels)
}

/// 相関の実行に対応しない形では、相関があれば `42601` で拒否する（fail-closed）。
/// `sql::subquery` の事前検査が束縛の直後に呼ぶ。肯定形の相関 `EXISTS` は #1842 で別経路
/// （`sql::correlated_exists`）が実行するため、ここへは届かない。エラー文言は静的文字列のみ。
pub(crate) fn reject_if_correlated(correlation: &Correlation) -> Result<(), SqlSurfaceError> {
    if correlation.is_correlated() {
        return Err(SqlSurfaceError::unsupported(
            "correlated subqueries are not supported",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ColumnDef;

    fn schema(name: &str, cols: &[(&str, ColumnType)]) -> TableSchema {
        TableSchema::new(
            name,
            cols.iter()
                .map(|(n, t)| ColumnDef::new(*n, t.clone(), true))
                .collect(),
        )
    }

    fn set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn ph(levels_up: usize, name: &str) -> String {
        outer_ref_placeholder(levels_up, name)
    }

    #[test]
    fn placeholder_round_trip_and_rejects_user_idents() {
        let p = outer_ref_placeholder(2, "lang");
        assert_eq!(parse_outer_ref_placeholder(&p), Some((2, "lang")));
        assert_eq!(parse_outer_ref_placeholder("outer1.x"), None);
        assert_eq!(parse_outer_ref_placeholder("lang"), None);
        assert_eq!(parse_outer_ref_placeholder("\u{2}outer0.x"), None);
        assert_eq!(parse_outer_ref_placeholder("\u{2}outer.x"), None);
        assert_eq!(parse_outer_ref_placeholder("\u{2}outer-1.x"), None);
        assert_eq!(
            parse_outer_ref_placeholder("\u{2}outer99999999999999999999999.x"),
            None
        );
    }

    #[test]
    fn inner_wins_then_nearest_outer() {
        let inner = schema("i", &[("a", ColumnType::Text)]);
        let mid = schema("m", &[("a", ColumnType::Integer), ("b", ColumnType::Text)]);
        let top = schema("t", &[("b", ColumnType::BigInt), ("c", ColumnType::Text)]);
        let c = bind_correlation(&set(&["a", "b", "c"]), &inner, &[&top, &mid]).expect("bind");
        assert_eq!(
            c.outer_refs()
                .iter()
                .map(|r| (r.levels_up, r.column.as_str()))
                .collect::<Vec<_>>(),
            vec![(1, "b"), (2, "c")]
        );
        assert_eq!(c.outer_refs()[0].column_type, Some(ColumnType::Text));
        assert!(c.is_correlated());
    }

    #[test]
    fn pseudo_id_inner_then_outer() {
        let inner = schema("i", &[("a", ColumnType::Text)]);
        let outer = schema("o", &[("x", ColumnType::Text)]);
        let c = bind_correlation(&set(&["id"]), &inner, &[&outer]).expect("bind");
        assert!(!c.is_correlated());
    }

    #[test]
    fn unknown_names_are_left_to_inner_binder() {
        let inner = schema("i", &[("a", ColumnType::Text)]);
        let outer = schema("o", &[("x", ColumnType::Text)]);
        let c = bind_correlation(&set(&["zz"]), &inner, &[&outer]).expect("bind");
        assert!(!c.is_correlated());
        assert!(reject_if_correlated(&c).is_ok());
    }

    #[test]
    fn ambiguous_in_one_level_is_42702() {
        let inner = schema("i", &[("a", ColumnType::Text)]);
        let o1 = schema("o1", &[("x", ColumnType::Text)]);
        let o2 = schema("o2", &[("x", ColumnType::Text)]);
        let levels = vec![ScopeLevel {
            relations: vec![&o1, &o2],
        }];
        let e = bind_correlation_levels(&set(&["x"]), &inner, &levels).expect_err("ambiguous");
        assert_eq!(e.wire_code(), "42702");
    }

    #[test]
    fn qualified_outer_ref_binds_or_is_42703_without_control_chars() {
        let inner = schema("i", &[("a", ColumnType::Text)]);
        let outer = schema("o", &[("x", ColumnType::Text)]);
        let ok = bind_correlation(&set(&[&ph(1, "x")]), &inner, &[&outer]).expect("bind");
        assert_eq!(ok.outer_refs().len(), 1);
        let e = bind_correlation(&set(&[&ph(1, "zz")]), &inner, &[&outer]).expect_err("missing");
        assert_eq!(e.wire_code(), "42703");
        assert!(!format!("{e:?}").contains('\u{2}'));
    }

    #[test]
    fn out_of_range_levels_fail_closed() {
        let inner = schema("i", &[("a", ColumnType::Text)]);
        let outer = schema("o", &[("x", ColumnType::Text)]);
        let e = bind_correlation(&set(&[&ph(2, "x")]), &inner, &[&outer]).expect_err("range");
        assert_eq!(e.wire_code(), "42601");
        let e = bind_correlation(&set(&[&ph(1, "x")]), &inner, &[]).expect_err("empty");
        assert_eq!(e.wire_code(), "42601");
    }

    #[test]
    fn dedup_is_deterministic() {
        let inner = schema("i", &[("a", ColumnType::Text)]);
        let outer = schema("o", &[("x", ColumnType::Text)]);
        let c = bind_correlation(&set(&["x", &ph(1, "x")]), &inner, &[&outer]).expect("bind");
        assert_eq!(c.outer_refs().len(), 1);
    }
}
