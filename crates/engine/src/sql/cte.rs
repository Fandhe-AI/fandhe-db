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
//! 具体化経路（SQL-62 (1)(3)(5)(6)・Issue #1860）: 明示 `MATERIALIZED` の CTE を主クエリの
//! FROM が直接参照するとき、`sql::allowlist` の `WITH` 分岐は本文を 1 文（`Statement::Scan`）
//! として組み、既存の評価後射影形ビュー経路（`Statement::BufferedView`）へ載せる。本文は
//! 参照したセッション自身の `PolicyContext`・外側と同一の `read_txn`（同一スナップショット）
//! で評価され（RLS-10 (b)）、評価済みの結果行は文のスタック上にだけ存在して文の終了で破棄
//! される（他の文・他セッションと共有しない）。保持行数は本文が暗黙 `LIMIT` を持つため
//! `--max-result-rows` で頭打ちとなり、超過は `54000`（SQL-34。黙って切り詰めない）。
//! 発火はセッションの最上位 SELECT の入口だけで、ビュー本文・cursor・COPY・CTAS 等は
//! 従来どおりインライン展開（結果は同じ）。
//!
//! 名前解決の順序（PostgreSQL と同じ意味論）: CTE 名はカタログのテーブル・
//! ビューを隠す。i 番目の CTE の本文から見えるのは 0..i-1 番目の CTE だけで、
//! 見つからなければ [`super::view::resolve_from`]（ビュー→テーブル）へ進む。
//! このため自己参照や循環は構造的に作れない（自己参照に見える名前は同名の
//! 実リレーションへ解決される）。

use super::allowlist::{ParsedViewBody, Projection, SqlSurfaceError, TableLookup};
use super::view::{
    apply_column_aliases, check_columns_within_view, physical_of, resolve_from, Resolved,
};
use super::view_alias::{rewrite_where, ColumnMap};

/// CTE の定義数の**ハード上限（＝既定値）**（超過は `54000`。SQL-29 (b) の受入基準 3）。
/// Issue #1552: 運用者は起動時に [`crate::nesting_limits::NestingLimits`] で下げられる。
pub(crate) const MAX_CTE_DEFINITIONS: usize = 16;

/// CTE→CTE の連鎖の深さ上限（`catalog::MAX_VIEW_NESTING_DEPTH` と同じ値。
/// 連鎖の先がビューに到達した後は既存の VIEW ネスト上限がそのまま効く）。
/// **ハード上限（＝既定値）**で、Issue #1552 により起動時に下げられる。
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

/// `WITH <name> [(<列名>, ...)] AS (<body>)` 1 件分の定義（`sql::allowlist::parse_with_clause`
/// が構築する）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CteDef {
    pub(crate) name: String,
    /// 列名リスト `WITH <name>(a, b)`（SQL-62 (2)・Issue #1857）。空ならリストなし。
    pub(crate) column_aliases: Vec<String>,
    pub(crate) body: ParsedViewBody,
    /// `[NOT] MATERIALIZED` の指定（SQL-62 (3)・Issue #1860）。`CteDef` はテナント情報を
    /// 持たない不変条件（RLS-10 (b)）を保つため、ここにも評価方式のヒントだけを置く。
    pub(crate) materialization: CteMaterialization,
}

/// CTE の評価方式ヒント（`WITH <name> AS [NOT] MATERIALIZED (...)`。SQL-62 (3)・Issue #1860）。
/// 結果は評価方式で変わらない。`Materialized` のみ、セッションの最上位 SELECT の入口で
/// 主クエリが直接参照したとき具体化経路（`Statement::BufferedView`）で評価される
/// （本文末尾「具体化経路」節）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CteMaterialization {
    /// 指定なし（インライン展開。複数回参照時の既定の具体化は Issue #1861）。
    Default,
    /// `MATERIALIZED`（具体化経路の対象）。
    Materialized,
    /// `NOT MATERIALIZED`（インライン展開）。
    NotMaterialized,
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
    max_depth: usize,
    budget: &mut ResolveBudget,
) -> Result<Resolved, SqlSurfaceError> {
    // `max_depth` は起動時設定値（Issue #1552。呼び出し元は
    // `NestingLimits::max_cte_nesting_depth`）。変換不能ならハード上限へ倒す（fail-closed）。
    let max_depth = u32::try_from(max_depth)
        .unwrap_or(MAX_CTE_NESTING_DEPTH)
        .min(MAX_CTE_NESTING_DEPTH);
    if depth > max_depth {
        return Err(SqlSurfaceError::payload_too_large(format!(
            "CTE nesting depth exceeds limit {max_depth}"
        )));
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
                SqlSurfaceError::payload_too_large(format!(
                    "CTE nesting depth exceeds limit {max_depth}"
                ))
            })?,
            max_depth_usize(max_depth),
            budget,
        )?;
        return compose(lookup, inner, def);
    }

    resolve_from(lookup, name)
}

/// CTE 本文 1 段分を、内側（`inner`）の解決結果へ合成する。`sql::view::
/// resolve_from` 第 2 パスの「内側から外側へ畳み込む」処理を CTE 向けに
/// 1 段分だけ切り出したもの。`sql::allowlist::validate_sql_tokens` の `WITH`
/// 分岐も、参照されない CTE の検証（列名リストの長さ検査を含む）に結果を捨てて呼ぶ。
///
/// 列名リスト（SQL-62 (2)・Issue #1857）は [`super::view::apply_column_aliases`]
/// （ビューの列別名と共有）で「公開名 → 物理名」の対応にし、`column_map` として
/// 外側へ渡す。列スコープは公開名で書き換えの**前**に検査する（`view_alias` の順序の
/// 不変条件）。内側が `column_map` を持つ（別名付き CTE・ビューの上に重なる）ときは、
/// この段の `WHERE` を物理名へ書き換えてから合成し、対応を伝播する。長すぎる列名リストは
/// `42P10`（[`SqlSurfaceError::CteColumnListTooLong`]）。
pub(crate) fn compose(
    lookup: &impl TableLookup,
    inner: Resolved,
    def: &CteDef,
) -> Result<Resolved, SqlSurfaceError> {
    let (base_table, mut acc_predicates, exposed, column_map, base_allocated) = match inner {
        Resolved::Table => {
            let base_table = def.body.table_name.clone();
            let allocated = lookup.table_is_allocated(&base_table)?;
            (base_table, Vec::new(), None, None, allocated)
        }
        Resolved::View {
            base_table,
            view_predicates,
            view_columns,
            column_map,
            base_allocated,
            ..
        } => (
            base_table,
            view_predicates,
            view_columns,
            column_map,
            base_allocated,
        ),
        // Issue #1192: 評価後射影形ビューは CTE から参照できない
        // （外側の合成ができないため）。
        Resolved::Buffered { .. } => {
            return Err(SqlSurfaceError::unsupported(
                "a view with an aggregate/LIMIT body cannot be used in a CTE",
            ));
        }
    };

    // `ParsedViewBody`（CTE 本文）は構文上 `ORDER BY` を持たない
    // （`sql::allowlist::parse_view_body` のドキュメント参照）ため、
    // Issue #915・SQL-25 のスカラー ORDER BY 検査は空スライスで無効化する
    // （主クエリ自身の ORDER BY は `sql::allowlist::build_scan_from_resolved`
    // 呼び出し側で別途検証する）。
    check_columns_within_view(
        exposed.as_deref(),
        &def.body.projection,
        &def.body.where_predicates,
        &[],
    )?;

    // この段の `WHERE` は内側の公開名で書かれている。内側が列別名を持つなら物理名へ書き換える。
    let layer_predicates = match &column_map {
        Some(map) => rewrite_where(
            &ColumnMap::new(map, !base_allocated),
            def.body.where_predicates.clone(),
        )?,
        None => def.body.where_predicates.clone(),
    };

    let (next_exposed, next_map) = if !def.column_aliases.is_empty() {
        let name = def.name.clone();
        let specified = def.column_aliases.len();
        let map = apply_column_aliases(
            lookup,
            &base_table,
            &def.body.projection,
            exposed.as_deref(),
            column_map.as_deref(),
            &def.column_aliases,
            |available| SqlSurfaceError::CteColumnListTooLong {
                name,
                available,
                specified,
            },
        )?;
        (
            Some(map.iter().map(|(p, _)| p.clone()).collect()),
            Some(map),
        )
    } else if let Projection::Columns(cols) = &def.body.projection {
        // 内側の別名対応を通して物理名を引き継ぐ（内側に無い列は検査済みのため到達しない）。
        let map = column_map.as_ref().map(|m| {
            cols.iter()
                .map(|c| (c.clone(), physical_of(m, c)))
                .collect::<Vec<_>>()
        });
        (Some(cols.clone()), map)
    } else {
        (exposed, column_map)
    };

    acc_predicates.extend(layer_predicates);

    Ok(Resolved::View {
        base_table,
        view_predicates: acc_predicates,
        view_columns: next_exposed,
        column_map: next_map,
        // CTE 経由の参照は読み取り専用（書き込み検査の対象外）。
        check_predicates: Vec::new(),
        // `column_map` の疑似列 `id` の扱い（`ColumnMap::new` の第 2 引数）が参照する。
        base_allocated,
    })
}

/// [`resolve_relation`] の再帰呼び出しへ上限を引き継ぐための `u32`→`usize` 変換
/// （変換不能は起こり得ないが、失敗時は 0＝最も厳しい側へ倒す）。
fn max_depth_usize(max_depth: u32) -> usize {
    usize::try_from(max_depth).unwrap_or(0)
}

/// 定義数の上限を検査する（`sql::allowlist::parse_with_clause` から呼ばれる）。
/// `limit` は起動時設定値（Issue #1552。ハード上限 [`MAX_CTE_DEFINITIONS`] 以下）で、
/// 超過は `54000`。ハード上限を超える値が渡されても、ハード上限で頭打ちにする。
pub(crate) fn check_definition_count(count: usize, limit: usize) -> Result<(), SqlSurfaceError> {
    let effective = limit.min(MAX_CTE_DEFINITIONS);
    if count > effective {
        return Err(SqlSurfaceError::payload_too_large(format!(
            "CTE definition count exceeds limit {effective}"
        )));
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
