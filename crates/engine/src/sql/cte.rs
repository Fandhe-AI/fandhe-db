//! 非再帰 `WITH` 句（CTE）の名前解決・インライン展開（SQL-29 (b)・RLS-10 (b)、
//! TASK-213、Issue #928）。
//!
//! 責務境界: `sql::allowlist::validate_sql_tokens` の `WITH` 分岐が
//! `sql::allowlist::parse_with_clause` で切り出した各 CTE 定義本文
//! （[`super::allowlist::ParsedViewBody`]、`CREATE VIEW` 本文と同一の許可
//! パーサーを再利用する）を、本モジュールの [`resolve_relation`] を通じて
//! `sql::view::resolve_from` と同型の名前解決へ合流させる。CTE は「クエリの
//! 中だけで有効な名前なしビュー」として扱い、第 2 の SQL パーサー・実行器は
//! 作らない。畳み込み後は既存の [`super::allowlist::ValidatedScan`] と完全に
//! 同じ形になり、束縛・実行・RLS 適用はすべて既存経路をそのまま通る。
//!
//! RLS-10 (b) の不変条件: [`CteDef`] は `tenant_id`／`PolicyContext`／作成者の
//! 情報を一切保持しない。畳み込み後の文は、参照した**セッション自身**の
//! `PolicyContext` を使う既存の実行経路でしか評価されない。
//!
//! 名前解決の順序（PostgreSQL と同じ意味論）: CTE 名はカタログのテーブル・
//! ビューを隠す。i 番目の CTE の本文から見えるのは 0..i-1 番目の CTE だけで、
//! 見つからなければ [`super::view::resolve_from`]（ビュー→テーブル）へ進む。
//! このため自己参照や循環は構造的に作れない（自己参照に見える名前は同名の
//! 実リレーションへ解決される）。

use super::allowlist::{ParsedViewBody, Projection, SqlSurfaceError, TableLookup};
use super::view::{check_columns_within_view, resolve_from, Resolved};

/// CTE の定義数上限（超過は `54000`。SQL-29 (b) の受入基準 3）。
pub(crate) const MAX_CTE_DEFINITIONS: usize = 16;

/// CTE→CTE の連鎖の深さ上限（`catalog::MAX_VIEW_NESTING_DEPTH` と同じ値。
/// 連鎖の先がビューに到達した後は既存の VIEW ネスト上限がそのまま効く）。
pub(crate) const MAX_CTE_NESTING_DEPTH: u32 = 4;

/// CTE 名の解決回数（1 回の [`resolve_relation`] 呼び出し系列の中で名前が
/// CTE として解決された回数）の上限。単一リレーションの世界では構造的に
/// 1 回の呼び出し系列あたり高々 `MAX_CTE_NESTING_DEPTH + 1` 段だが、将来
/// JOIN が入ることに備えて明示的な上限として持つ。
///
/// [`ResolveBudget`] は「文全体」ではなく「1 回のトップレベル呼び出し（1 つの
/// CTE 定義の事前検証、または主クエリの解決）」単位でリセットして使うこと
/// （呼び出し元の規約。Issue #928 レビュー指摘）。1 つのカウンタを文全体で
/// 使い回すと、事前検証ループが各定義のチェーンを再帰的に辿るたびに参照回数が
/// 名前ごとではなく呼び出し回数分累積し、定義数（`MAX_CTE_DEFINITIONS`）・
/// 連鎖の深さ（`MAX_CTE_NESTING_DEPTH`）のどちらも上限内の有効なクエリを
/// 誤って `54000` で拒否してしまう。
pub(crate) const MAX_CTE_REFERENCES: usize = 32;

/// `WITH <name> AS (<body>)` 1 件分の定義（`sql::allowlist::parse_with_clause`
/// が構築する）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CteDef {
    pub(crate) name: String,
    pub(crate) body: ParsedViewBody,
}

/// CTE 名の解決回数を数え、上限超過を `54000` として検出するための可変カウンタ。
/// `resolve_relation` の再帰呼び出しをまたいで共有する（呼び出し元が
/// `&mut` で保持する）。呼び出し元は「1 回のトップレベル呼び出し系列
/// （1 つの CTE 定義の事前検証、または主クエリの解決）」ごとに新しい
/// インスタンスを生成すること（[`MAX_CTE_REFERENCES`] のドキュメント参照）。
pub(crate) struct ResolveBudget {
    references: usize,
}

impl ResolveBudget {
    pub(crate) fn new() -> Self {
        Self { references: 0 }
    }

    fn consume_reference(&mut self) -> Result<(), SqlSurfaceError> {
        self.references = self.references.checked_add(1).ok_or_else(|| {
            SqlSurfaceError::payload_too_large("CTE reference count exceeds limit")
        })?;
        if self.references > MAX_CTE_REFERENCES {
            return Err(SqlSurfaceError::payload_too_large(
                "CTE reference count exceeds limit",
            ));
        }
        Ok(())
    }
}

/// `name` を、`ctes[..visible_upto]`（i 番目の CTE の本文からは 0..i-1 番目
/// だけが見える、という可視範囲を表す）の中の CTE 名として解決を試み、
/// 一致しなければ [`super::view::resolve_from`]（ビュー→テーブル）へ委譲する。
///
/// 一致した場合は、その CTE 本文の FROM を `depth + 1` として再帰的に
/// 解決し、各段で [`check_columns_within_view`] を適用してから、内側の
/// 述語を先頭に置いた合成結果を返す（`sql::view::resolve_from` の
/// 「内側から外側へ畳み込む」設計と同じ順序）。
///
/// 深さ・参照回数の上限超過は `SqlSurfaceError::payload_too_large`（`54000`）。
pub(crate) fn resolve_relation(
    lookup: &impl TableLookup,
    ctes: &[CteDef],
    visible_upto: usize,
    name: &str,
    depth: u32,
    budget: &mut ResolveBudget,
) -> Result<Resolved, SqlSurfaceError> {
    if depth > MAX_CTE_NESTING_DEPTH {
        return Err(SqlSurfaceError::payload_too_large(
            "CTE nesting depth exceeds limit",
        ));
    }

    // 後ろ（最も内側で定義された CTE）から順に名前を照合する。`visible_upto`
    // 件目より後ろの CTE は「まだ定義されていない」ため対象外（この境界に
    // より自己参照・前方参照は構造的に起こらず、同名の実テーブルへ解決される）。
    let visible = ctes.get(..visible_upto).unwrap_or(&[]);
    if let Some((idx, def)) = visible
        .iter()
        .enumerate()
        .rev()
        .find(|(_, def)| def.name.eq_ignore_ascii_case(name))
    {
        budget.consume_reference()?;
        let inner = resolve_relation(
            lookup,
            ctes,
            idx,
            &def.body.table_name,
            depth.checked_add(1).ok_or_else(|| {
                SqlSurfaceError::payload_too_large("CTE nesting depth exceeds limit")
            })?,
            budget,
        )?;
        return compose(inner, def);
    }

    resolve_from(lookup, name)
}

/// CTE 本文 1 段分を、内側（`inner`）の解決結果へ合成する。`sql::view::
/// resolve_from` 第 2 パスの「内側から外側へ畳み込む」処理を CTE 向けに
/// 1 段分だけ切り出したもの。
fn compose(inner: Resolved, def: &CteDef) -> Result<Resolved, SqlSurfaceError> {
    let (base_table, mut acc_predicates, exposed) = match inner {
        Resolved::Table => (def.body.table_name.clone(), Vec::new(), None),
        Resolved::View {
            base_table,
            view_predicates,
            view_columns,
        } => (base_table, view_predicates, view_columns),
    };

    check_columns_within_view(
        exposed.as_deref(),
        &def.body.projection,
        &def.body.where_predicates,
    )?;

    let next_exposed = if let Projection::Columns(cols) = &def.body.projection {
        Some(cols.clone())
    } else {
        exposed
    };

    acc_predicates.extend(def.body.where_predicates.clone());

    Ok(Resolved::View {
        base_table,
        view_predicates: acc_predicates,
        view_columns: next_exposed,
    })
}

/// 定義数の上限を検査する（`sql::allowlist::parse_with_clause` から呼ばれる）。
pub(crate) fn check_definition_count(count: usize) -> Result<(), SqlSurfaceError> {
    if count > MAX_CTE_DEFINITIONS {
        return Err(SqlSurfaceError::payload_too_large(
            "CTE definition count exceeds limit",
        ));
    }
    Ok(())
}

/// CTE 名の重複を検査する（`sql::allowlist::parse_with_clause` から呼ばれる）。
pub(crate) fn check_no_duplicate_name(ctes: &[CteDef], name: &str) -> Result<(), SqlSurfaceError> {
    if ctes.iter().any(|c| c.name.eq_ignore_ascii_case(name)) {
        return Err(SqlSurfaceError::unsupported(format!(
            "duplicate CTE name: {name}"
        )));
    }
    Ok(())
}
