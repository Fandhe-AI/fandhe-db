//! 列別名付きビュー（`CREATE VIEW v (a, b) AS ...`。TABLE-41 (1)・Issue #1659）の参照時に、
//! 外側クエリが使う公開名（別名）を基底表の物理列名へ書き換える。
//!
//! 責務境界: `sql::view::resolve_from` が連鎖を畳み込んで得た「公開名 → 物理名」対応
//! （[`crate::sql::view::Resolved::View::column_map`]）を、`sql::allowlist` の広域取得
//! （`build_scan_from_resolved`）と集計（`build_aggregate_from_resolved`）の組み立てが本モジュール
//! 経由で外側クエリへ適用する。書き換えの走査器は `sql::check_constraint` の列改名と共有する
//! （[`map_predicate_columns`]・[`map_expr_idents`]。第 2 の走査器は作らない）。
//!
//! 順序の不変条件: 列スコープ検査（`sql::view::check_*_within_view`）は**公開名に対して先に**
//! 呼び出し元が行う。物理名・`tenant_id`・`visibility` での絞り込みや並べ替えは、この時点で
//! `22000` になり書き換えまで到達しない（filter oracle の防止。security.md「アクセス制御の不備」）。
//! 出力名は SQL-42（Issue #1540）の「別名付き列参照」で保つ。書き換えに未対応の形は黙って
//! 通さず `0A000` で拒否する（fail-closed）。
//!
//! RLS-10 (b): 本モジュールは名前の写像だけを行い、行の可視性判定には関与しない。

use std::collections::HashSet;

use super::allowlist::{
    AggregateArg, AggregateSelectItem, GroupByClause, Projection, ScalarOrderKey, ScanOrderKey,
    SelectItem, SqlSurfaceError, WherePredicate,
};
use super::check_constraint::{map_expr_idents, map_predicate_columns};
use super::udf_call::Expr;

/// 公開名 → 物理名の対応（`Resolved::View::column_map` の借用ビュー）。
pub(crate) struct ColumnMap<'a>(&'a [(String, String)]);

impl<'a> ColumnMap<'a> {
    pub(crate) fn new(map: &'a [(String, String)]) -> Self {
        ColumnMap(map)
    }

    /// 公開名に対応する物理名（対応が無ければ同名のまま返す。呼び出し元が公開列の範囲検査を
    /// 済ませているため、無い場合は未知の列であり後段の束縛が拒否する）。
    fn physical(&self, public: &str) -> String {
        self.0
            .iter()
            .find(|(p, _)| p == public)
            .map(|(_, phys)| phys.clone())
            .unwrap_or_else(|| public.to_string())
    }

    fn contains(&self, public: &str) -> bool {
        self.0.iter().any(|(p, _)| p == public)
    }

    /// 疑似列 `id` へ別名で写る公開名が `idents` に含まれていれば `0A000`。疑似列 `id` は
    /// 述語・並べ替え・集計で扱いが異なる（`IdCompare` 等）ため、外側の投影以外での参照は
    /// 意味を取り違えないよう未対応にする（fail-closed）。
    fn reject_aliased_id_refs(&self, idents: &HashSet<String>) -> Result<(), SqlSurfaceError> {
        let hit = self
            .0
            .iter()
            .any(|(public, phys)| phys == "id" && public != "id" && idents.contains(public));
        if hit {
            return Err(unsupported_position());
        }
        Ok(())
    }

    /// 利用者列を公開名 `id` で公開しているか。このとき疑似列 `id`（行 identity）と公開名 `id`
    /// （別名元の利用者列）は別物になる。
    fn exposes_user_column_as_id(&self) -> bool {
        self.0
            .iter()
            .any(|(public, phys)| public == "id" && phys != "id")
    }
}

/// 述語木に行 identity 比較の葉 [`WherePredicate::IdCompare`]（`sql::subquery` の解決段が
/// 疑似列 `id` 宛てに生成する内部専用の葉）が含まれるか。`Not`／`Or` の内側も再帰する。
fn contains_id_compare(predicates: &[WherePredicate]) -> bool {
    predicates.iter().any(|p| match p {
        WherePredicate::IdCompare { .. } => true,
        WherePredicate::Not(inner) => contains_id_compare(std::slice::from_ref(inner.as_ref())),
        WherePredicate::Or(branches) => branches.iter().any(|b| contains_id_compare(b)),
        _ => false,
    })
}

fn unsupported_position() -> SqlSurfaceError {
    SqlSurfaceError::feature_not_supported(
        "this reference to a view with column aliases is not supported",
    )
}

/// 外側 `WHERE` の述語を物理名へ書き換える。サブクエリを含む述語は `0A000`。
pub(crate) fn rewrite_where(
    map: &ColumnMap<'_>,
    predicates: Vec<WherePredicate>,
) -> Result<Vec<WherePredicate>, SqlSurfaceError> {
    let mut idents = HashSet::new();
    super::parser::collect_where_predicate_idents(&predicates, &mut idents);
    map.reject_aliased_id_refs(&idents)?;
    // 利用者列を `id` として公開するビューでは、疑似列 `id`（行 identity）宛ての比較葉は
    // 公開名 `id`（別名元の利用者列）へ写せない。物理名への書き換えを素通りして行 identity で
    // 絞り込むと誤った行を返すため、書き換えず拒否する（fail-closed）。
    if map.exposes_user_column_as_id() && contains_id_compare(&predicates) {
        return Err(unsupported_position());
    }
    predicates
        .iter()
        .map(|p| map_predicate_columns(p, &|c| map.physical(c), &unsupported_position))
        .collect()
}

/// 書き換え後の投影が持つ明示別名（`SelectItem::Expr` の `alias`）の集合。外側クエリの
/// `ORDER BY` はこの名前を出力別名として解決する（`sql::parser::resolve_scan_order_aliases`）。
fn explicit_aliases(projection: &Projection) -> HashSet<&str> {
    match projection {
        Projection::Items(items) => items
            .iter()
            .filter_map(|i| match i {
                SelectItem::Expr { alias: Some(a), .. } => Some(a.as_str()),
                _ => None,
            })
            .collect(),
        _ => HashSet::new(),
    }
}

/// `ORDER BY` の列名キー 1 件を書き換える。戻り値の第 2 要素は「物理名へ書き換えた」か。
///
/// キー名が書き換え後の投影の明示別名に一致するときは**書き換えない**: 束縛段の出力別名解決が
/// その別名（生成投影は `物理名 AS 公開名`）を基底列へ辿るため、公開名のままで正しい列になる。
/// 一致しないとき（投影に現れない公開列）は物理名へ書き換え、呼び出し元が位置を記録して
/// 束縛段に別名として再解決させない（物理名が別の生成別名と一致する入れ替え別名対策）。
fn rewrite_order_column(
    map: &ColumnMap<'_>,
    aliases: &HashSet<&str>,
    key: ScalarOrderKey,
) -> (ScalarOrderKey, bool) {
    if aliases.contains(key.column.as_str()) {
        return (key, false);
    }
    let physical = map.physical(&key.column);
    (
        ScalarOrderKey {
            column: physical,
            descending: key.descending,
        },
        true,
    )
}

/// 列名キーの `ORDER BY` を物理名へ書き換える。第 2 要素は物理名へ書き換えたキーの位置
/// （[`crate::sql::allowlist::ValidatedScan::physical_order_keys`] の元）。
/// `projection` は書き換え済みの外側投影。
pub(crate) fn rewrite_order_by(
    map: &ColumnMap<'_>,
    projection: &Projection,
    keys: Vec<ScalarOrderKey>,
) -> Result<(Vec<ScalarOrderKey>, Vec<usize>), SqlSurfaceError> {
    let idents: HashSet<String> = keys.iter().map(|k| k.column.clone()).collect();
    map.reject_aliased_id_refs(&idents)?;
    let aliases = explicit_aliases(projection);
    let mut positions = Vec::new();
    let mut out = Vec::with_capacity(keys.len());
    for (i, key) in keys.into_iter().enumerate() {
        let (key, physical) = rewrite_order_column(map, &aliases, key);
        if physical {
            positions.push(i);
        }
        out.push(key);
    }
    Ok((out, positions))
}

/// 式キーを含む `ORDER BY` を物理名へ書き換える（位置の意味は [`rewrite_order_by`] と同じ）。
pub(crate) fn rewrite_order_keys(
    map: &ColumnMap<'_>,
    projection: &Projection,
    keys: Vec<ScanOrderKey>,
) -> Result<(Vec<ScanOrderKey>, Vec<usize>), SqlSurfaceError> {
    let mut idents = HashSet::new();
    for key in &keys {
        match key {
            ScanOrderKey::Column(k) => {
                idents.insert(k.column.clone());
            }
            ScanOrderKey::Expr { expr, .. } => {
                super::parser::collect_expr_idents(expr, &mut idents)
            }
        }
    }
    map.reject_aliased_id_refs(&idents)?;
    let aliases = explicit_aliases(projection);
    let mut positions = Vec::new();
    let mut out = Vec::with_capacity(keys.len());
    for (i, key) in keys.into_iter().enumerate() {
        out.push(match key {
            ScanOrderKey::Column(k) => {
                let (k, physical) = rewrite_order_column(map, &aliases, k);
                if physical {
                    positions.push(i);
                }
                ScanOrderKey::Column(k)
            }
            ScanOrderKey::Expr { expr, descending } => ScanOrderKey::Expr {
                expr: map_expr_idents(&expr, &|c| map.physical(c)),
                descending,
            },
        });
    }
    Ok((out, positions))
}

/// 公開列 `public` を 1 件の投影項目へ変換する。公開名と物理名が違うときは SQL-42 の
/// 「別名付き列参照」で出力名を公開名に保つ（値・型は物理列のまま）。
fn column_item(map: &ColumnMap<'_>, public: &str) -> SelectItem {
    let physical = map.physical(public);
    if physical == public {
        SelectItem::Column(public.to_string())
    } else {
        SelectItem::Expr {
            expr: Expr::Ident(physical),
            alias: Some(public.to_string()),
        }
    }
}

/// 全項目が別名なしの列参照なら従来どおり `Projection::Columns`、そうでなければ `Items`。
fn finish_items(items: Vec<SelectItem>) -> Projection {
    if items.iter().all(|i| matches!(i, SelectItem::Column(_))) {
        Projection::Columns(
            items
                .into_iter()
                .filter_map(|i| match i {
                    SelectItem::Column(c) => Some(c),
                    SelectItem::Expr { .. } => None,
                })
                .collect(),
        )
    } else {
        Projection::Items(items)
    }
}

/// 外側の投影を物理名へ書き換え、出力列名を公開名のまま保つ。`SELECT *` はビューの公開列
/// 全部（`tenant_id`・`visibility` を含まない）へ展開する。
pub(crate) fn rewrite_projection(
    map: &ColumnMap<'_>,
    projection: Projection,
) -> Result<Projection, SqlSurfaceError> {
    match projection {
        Projection::All => Ok(finish_items(
            map.0.iter().map(|(p, _)| column_item(map, p)).collect(),
        )),
        Projection::Columns(cols) => {
            let mut items = Vec::with_capacity(cols.len());
            for c in &cols {
                if !map.contains(c) {
                    return Err(SqlSurfaceError::undefined_column(c));
                }
                items.push(column_item(map, c));
            }
            Ok(finish_items(items))
        }
        Projection::Items(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    SelectItem::Column(c) => {
                        if !map.contains(&c) {
                            return Err(SqlSurfaceError::undefined_column(c));
                        }
                        out.push(column_item(map, &c));
                    }
                    SelectItem::Expr {
                        expr: Expr::Ident(name),
                        alias,
                    } => {
                        if !map.contains(&name) {
                            return Err(SqlSurfaceError::undefined_column(name));
                        }
                        let physical = map.physical(&name);
                        // 別名なしの裸の列参照は出力名を公開名に保つ。
                        let alias = alias.or_else(|| (physical != name).then(|| name.clone()));
                        out.push(match alias {
                            Some(a) => SelectItem::Expr {
                                expr: Expr::Ident(physical),
                                alias: Some(a),
                            },
                            None => SelectItem::Column(physical),
                        });
                    }
                    SelectItem::Expr { expr, alias } => {
                        let mut idents = HashSet::new();
                        super::parser::collect_expr_idents(&expr, &mut idents);
                        map.reject_aliased_id_refs(&idents)?;
                        // 既定の出力名は書き換え前の式（公開名）から確定する。物理名へ書き換えた後の
                        // 式から導くと `CAST(x AS TEXT)`・`x[1]` の出力名が隠れた物理名になる。
                        let alias =
                            Some(alias.unwrap_or_else(|| super::parser::default_expr_alias(&expr)));
                        out.push(SelectItem::Expr {
                            expr: map_expr_idents(&expr, &|c| map.physical(c)),
                            alias,
                        });
                    }
                }
            }
            Ok(finish_items(out))
        }
    }
}

/// [`rewrite_aggregate`] の結果（項目・`WHERE`・`GROUP BY`）。
pub(crate) type RewrittenAggregate = (
    Vec<AggregateSelectItem>,
    Vec<WherePredicate>,
    Option<GroupByClause>,
);

/// 集計 SELECT の項目・`GROUP BY`・`WHERE` を物理名へ書き換える。グループキーの出力名は公開名の
/// まま保つ。集計式項目（`sum(n)+1` 等）・式の `HAVING`・式キー／項目名でない列を指す `ORDER BY`
/// は未対応で `0A000`（fail-closed）。
pub(crate) fn rewrite_aggregate(
    map: &ColumnMap<'_>,
    items: Vec<AggregateSelectItem>,
    where_predicates: Vec<WherePredicate>,
    group_by: Option<GroupByClause>,
) -> Result<RewrittenAggregate, SqlSurfaceError> {
    let mut idents = HashSet::new();
    for item in &items {
        item.collect_referenced_idents(&mut idents);
    }
    if let Some(gb) = &group_by {
        idents.extend(gb.columns.iter().cloned());
    }
    map.reject_aliased_id_refs(&idents)?;

    let effective_names: Vec<String> = items.iter().map(|i| i.effective_name()).collect();
    let mut new_items = Vec::with_capacity(items.len());
    for item in items {
        new_items.push(match item {
            AggregateSelectItem::GroupKey { column, alias } => {
                let physical = map.physical(&column);
                let alias = alias.or_else(|| (physical != column).then(|| column.clone()));
                AggregateSelectItem::GroupKey {
                    column: physical,
                    alias,
                }
            }
            AggregateSelectItem::Aggregate(mut agg) => {
                if let AggregateArg::Expr(e) = &agg.arg {
                    agg.arg = AggregateArg::Expr(map_expr_idents(e, &|c| map.physical(c)));
                }
                AggregateSelectItem::Aggregate(agg)
            }
            AggregateSelectItem::Expr { .. } => return Err(unsupported_position()),
        });
    }
    let new_group_by = match group_by {
        None => None,
        Some(mut gb) => {
            if !gb.having_exprs.is_empty() {
                return Err(unsupported_position());
            }
            for key in &gb.order_by {
                // 項目の実効名を指すキーだけ受理する（公開列を直接指すキーは物理名との対応が
                // 項目名と曖昧になるため未対応）。
                if key.expr.is_some() || !effective_names.iter().any(|n| n == &key.target) {
                    return Err(unsupported_position());
                }
            }
            gb.columns = gb.columns.iter().map(|c| map.physical(c)).collect();
            Some(gb)
        }
    };
    let new_where = rewrite_where(map, where_predicates)?;
    Ok((new_items, new_where, new_group_by))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::udf_call::BinOp;

    fn id_compare() -> WherePredicate {
        WherePredicate::IdCompare {
            op: BinOp::Eq,
            value: 1,
        }
    }

    /// 利用者列を `id` として公開するビューでは、行 identity 比較の葉を（ネスト越しでも）拒否する。
    #[test]
    fn id_compare_is_rejected_when_user_column_is_exposed_as_id() {
        let pairs = vec![("id".to_string(), "lang".to_string())];
        let map = ColumnMap::new(&pairs);
        for preds in [
            vec![id_compare()],
            vec![WherePredicate::Not(Box::new(id_compare()))],
            vec![WherePredicate::Or(vec![vec![id_compare()]])],
        ] {
            assert!(rewrite_where(&map, preds).is_err());
        }
    }

    /// `id` を別名で公開しないビューでは従来どおり通る（行 identity のまま）。
    #[test]
    fn id_compare_passes_when_id_is_not_user_column() {
        let pairs = vec![("a".to_string(), "lang".to_string())];
        let map = ColumnMap::new(&pairs);
        assert!(rewrite_where(&map, vec![id_compare()]).is_ok());
    }
}
