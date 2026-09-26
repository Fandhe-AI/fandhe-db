//! 非マテリアライズド `VIEW` の参照時展開（TABLE-18・SQL-23・TASK-205、
//! Issue #909）。
//!
//! 責務境界: `sql::allowlist::validate_sql_tokens` の広域取得
//! （`Statement::Scan`）分岐が、FROM に指定された名前を [`resolve_from`] へ
//! 通すことで「テーブルへの通常参照」と「ビュー経由の参照」を単一の経路へ
//! 統一する。ビューはカタログに保存された定義（[`crate::catalog::ViewDef`]）を
//! 同じ許可リストパーサー（`sql::allowlist::parse_view_body`）で再検証し、
//! 連鎖（ビューがビューを参照する）を内側から畳み込んで最終的に 1 つの
//! 基底テーブル名＋合成済み `WHERE` 述語へ書き換える。第 2 の SQL パーサー・
//! 実行器は作らない（畳み込み後は既存の [`crate::sql::allowlist::ValidatedScan`]
//! と完全に同じ形になり、束縛・実行・RLS 適用はすべて既存経路をそのまま通る）。
//!
//! RLS-10 (b) の不変条件: ビュー定義は `tenant_id`／`PolicyContext`／作成者の
//! 情報を一切保持しない。畳み込み後の文は、参照した**セッション自身**の
//! `PolicyContext` を使う既存の実行経路でしか評価されないため、作成者の
//! 可視性が参照者へ引き継がれることは構造的に起こらない。

use super::allowlist::{
    parse_view_body, AggregateArg, Projection, SelectItem, SqlSurfaceError, TableLookup,
    WherePredicate, WindowSelectItem,
};
use crate::catalog::{ViewDef, MAX_VIEW_NESTING_DEPTH};
use crate::sql::udf_call::Expr;

/// FROM に指定された名前の解決結果。
pub(crate) enum Resolved {
    /// 通常のテーブル参照。
    Table,
    /// ビュー参照（連鎖を内側から畳み込んだ結果）。
    View {
        /// 連鎖を辿った先の実テーブル名。
        base_table: String,
        /// ビュー定義由来の `WHERE` 述語（内側のビューが先頭。クエリ自身の
        /// 述語はこの後ろに追加する）。
        view_predicates: Vec<WherePredicate>,
        /// クエリ直接の参照先（最も外側のビュー）が最終的に公開する列集合
        /// （連鎖の各段の投影を内側から積み上げた結果。`None` は連鎖のどの
        /// 段も列を絞り込んでいない——全段が `SELECT *`——ことを意味する）。
        view_columns: Option<Vec<String>>,
    },
}

/// カタログ破損時の連鎖走査を打ち切る安全上限。正常経路では循環を構造的に
/// 構築できない（`CREATE VIEW` が参照先の存在を作成前に要求するため）が、
/// 破損したカタログ値に対しても無限ループにならないことを保証する。
const MAX_VIEW_RESOLVE_STEPS: usize = 10_000;

/// FROM に指定された `name` を解決する（[`crate::sql::allowlist::validate_sql_tokens`]
/// の `Statement::Scan` 分岐から呼ばれる）。テーブル・ビューのいずれにも
/// 存在しない場合は `Err(SqlSurfaceError::UndefinedTable)`。カタログに保存された
/// ビュー本文の再検証・デコードに失敗した場合（カタログ破損）は
/// `Err(SqlSurfaceError::Internal)`（固定文言。body のリテラル値を含めない。
/// security.md P0）。
pub(crate) fn resolve_from(
    lookup: &impl TableLookup,
    name: &str,
) -> Result<Resolved, SqlSurfaceError> {
    // 第 1 パス: 外側（クエリが直接参照した名前）から内側へ向けて連鎖を
    // 辿り、各段の本文をそのまま集める（列スコープの検証・畳み込みはまだ
    // 行わない——内側の露出列が確定するまでは外側の参照が妥当かどうか
    // 判断できないため、確定は第 2 パスへ分離する）。
    let mut current = name.to_string();
    let mut chain: Vec<super::allowlist::ParsedViewBody> = Vec::new();
    let mut depth: u32 = 0;
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();

    let base_table = loop {
        if chain.len() >= MAX_VIEW_RESOLVE_STEPS {
            return Err(corrupt_view_error());
        }
        if !visited.insert(current.clone()) {
            return Err(corrupt_view_error());
        }
        match lookup.view_definition(&current)? {
            Some(ViewDef {
                base_relation,
                body_sql,
            }) => {
                let parsed = reparse_stored_body(&body_sql)?;
                depth = depth.checked_add(1).ok_or_else(corrupt_view_error_fn)?;
                if depth > MAX_VIEW_NESTING_DEPTH {
                    return Err(SqlSurfaceError::payload_too_large(
                        "view nesting depth exceeds limit",
                    ));
                }
                chain.push(parsed);
                current = base_relation;
            }
            None => {
                if !lookup.table_exists(&current)? {
                    return Err(SqlSurfaceError::undefined_table(name));
                }
                break current;
            }
        }
    };

    if chain.is_empty() {
        return Ok(Resolved::Table);
    }

    // 第 2 パス: 最も内側（base table に最も近い）のビューから外側へ向けて
    // 走査し、各段の投影・`WHERE` が「その段の FROM が指す関係が公開する
    // 列集合」に収まっていることを [`check_columns_within_view`]（クエリ
    // 自身の列スコープ検査と同じ実装）で検証してから、その段自身の投影で
    // 公開列集合を更新する。検証を先に行うことで、外側ビューが `SELECT *`
    // で内側ビューを包んだ場合（内側の列制限をそのまま引き継ぐ）だけでなく、
    // 外側ビューが明示列指定・`WHERE` で内側ビューが公開しない列を直接
    // 参照した場合（`CREATE VIEW` 自体はカタログ照会を行わないため作成時
    // には検出されない）も、連鎖のどの段であれ参照時に一様に拒否できる。
    let mut exposed: Option<Vec<String>> = None;
    let mut acc_predicates: Vec<WherePredicate> = Vec::new();
    for view in chain.into_iter().rev() {
        check_columns_within_view(exposed.as_deref(), &view.projection, &view.where_predicates)?;
        if let Projection::Columns(cols) = view.projection {
            exposed = Some(cols);
        }
        // 内側（より深い）のビューの述語を先頭に置く（§設計「述語合成」）。
        // ここでは内側から外側へ順に処理しているため、単純な追記でよい。
        acc_predicates.extend(view.where_predicates);
    }

    Ok(Resolved::View {
        base_table,
        view_predicates: acc_predicates,
        view_columns: exposed,
    })
}

/// 格納済み `body_sql`（`sql::allowlist::render_view_body` の出力）を再トークン化・
/// 再パースする。第 2 の SQL パーサーを作らず、`CREATE VIEW` 時と完全に同じ
/// [`parse_view_body`] を経由する。失敗（カタログ破損・非互換な将来フォーマット
/// 変更）は `body_sql` の内容をエラーへ含めず、固定文言の
/// `SqlSurfaceError::Internal` へ丸める（security.md P0）。
fn reparse_stored_body(
    body_sql: &str,
) -> Result<super::allowlist::ParsedViewBody, SqlSurfaceError> {
    let tokens = super::lexer::tokenize(body_sql).map_err(|_| corrupt_view_error())?;
    parse_view_body(&tokens).map_err(|_| corrupt_view_error())
}

fn corrupt_view_error() -> SqlSurfaceError {
    // body のリテラル値・詳細な破損理由をクライアントへ運ばない固定文言
    // （security.md P0「エラー・ログ経由で他テナントのデータ・存在情報を
    // 漏らさない」対応）。
    SqlSurfaceError::Internal {
        detail: "invalid view definition".to_string(),
    }
}

fn corrupt_view_error_fn() -> SqlSurfaceError {
    corrupt_view_error()
}

/// 投影・`WHERE` が、参照先が公開する列集合（`view_columns`）に収まって
/// いるかを検査する。呼び出し元は 2 箇所: (1) クエリ自身の投影・`WHERE` を
/// [`Resolved::View::view_columns`]（連鎖全体を畳み込んだ最終的な公開列
/// 集合）に対して検査する箇所（`sql::allowlist::validate_sql_tokens`）、
/// (2) [`resolve_from`] が連鎖の各段自身の投影・`WHERE` を「その段の FROM
/// が指す関係の公開列集合」に対して検査する箇所。`view_columns` が `None`
/// の場合は検査不要（対象の関係がどの段でも列を絞り込んでいない——基底
/// テーブルの束縛段がそのまま列存在を検査する）。範囲外の列参照は
/// `SqlSurfaceError::InvalidInput`（`22000`。既存の「未知の列」束縛エラーと
/// 同じ分類）。
///
/// `Projection::Items`（TASK-79・SQL-9 の式項目）・`WherePredicate::Expression`
/// （同）は列参照を式木の内側に持つため、[`expr_columns_within`] で式木を
/// 再帰的に走査し、含まれるすべての列参照（`Expr::Ident`）を検査する
/// （codex-review 指摘・PR #1048: 単純な `column` フィールドしか見ない旧実装は
/// `SELECT body || '' FROM v` や式述語経由でビューの非公開列を素通しにしていた。
/// RLS 境界そのものには影響しない――基底テーブルへ書き換え済みの式は RLS 判定を
/// 経由する既存経路でそのまま評価される――が、ビューが宣言した列スコープ契約が
/// 破れていたため、他の形と同じ扱いへ揃える）。
pub(crate) fn check_columns_within_view(
    view_columns: Option<&[String]>,
    projection: &Projection,
    where_predicates: &[WherePredicate],
) -> Result<(), SqlSurfaceError> {
    let Some(columns) = view_columns else {
        return Ok(());
    };
    match projection {
        Projection::All => {}
        Projection::Columns(cols) => {
            for c in cols {
                if !columns.iter().any(|vc| vc == c) {
                    return Err(SqlSurfaceError::InvalidInput {
                        detail: format!("unknown column: {c}"),
                    });
                }
            }
        }
        Projection::Items(items) => {
            for item in items {
                match item {
                    SelectItem::Column(c) => {
                        if !columns.iter().any(|vc| vc == c) {
                            return Err(SqlSurfaceError::InvalidInput {
                                detail: format!("unknown column: {c}"),
                            });
                        }
                    }
                    SelectItem::Expr { expr, .. } => expr_columns_within(columns, expr)?,
                }
            }
        }
    }
    for pred in where_predicates {
        check_predicate_columns_within(columns, pred)?;
    }
    Ok(())
}

/// `pred` が参照する列がすべて `columns`（ビューが公開する列集合）に収まることを
/// 検査する（TASK-208・SQL-24、Issue #912）。[`WherePredicate::Or`] の分岐へ
/// **再帰する**ことが本関数の存在理由: 再帰しないと、ビューが公開していない列を
/// `WHERE exposed = 'x' OR hidden = 'secret'` のようにフィルタへ使え、非公開列の
/// 値を推測する手段（filter oracle）になる（security.md「アクセス制御の不備」
/// P0）。[`predicate_column`] は単純な `column` フィールドを持つ形のみを扱うため、
/// `Or`・`Expression` はここで個別に分岐する。
fn check_predicate_columns_within(
    columns: &[String],
    pred: &WherePredicate,
) -> Result<(), SqlSurfaceError> {
    match pred {
        WherePredicate::Expression(expr) => expr_columns_within(columns, expr),
        WherePredicate::Or(branches) => {
            for branch in branches {
                for leaf in branch {
                    check_predicate_columns_within(columns, leaf)?;
                }
            }
            Ok(())
        }
        _ => {
            if let Some(c) = predicate_column(pred) {
                if !columns.iter().any(|vc| vc == c) {
                    return Err(SqlSurfaceError::InvalidInput {
                        detail: format!("unknown column: {c}"),
                    });
                }
            }
            Ok(())
        }
    }
}

/// 式木（[`Expr`]）が参照する列（[`Expr::Ident`]）をすべて再帰的に検査し、
/// `columns`（ビューが公開する列集合）に含まれない列参照があれば
/// `SqlSurfaceError::InvalidInput` で拒否する（[`check_columns_within_view`] の
/// 式項目・式述語向け実装）。`Expr::Number` は列参照を持たず、`Expr::Call`・
/// `Expr::Binary` は子孫を再帰的に辿る。
fn expr_columns_within(columns: &[String], expr: &Expr) -> Result<(), SqlSurfaceError> {
    match expr {
        Expr::Number(_) => Ok(()),
        Expr::Ident(name) => {
            if columns.iter().any(|vc| vc == name) {
                Ok(())
            } else {
                Err(SqlSurfaceError::InvalidInput {
                    detail: format!("unknown column: {name}"),
                })
            }
        }
        Expr::Call { args, .. } => {
            for arg in args {
                expr_columns_within(columns, arg)?;
            }
            Ok(())
        }
        Expr::Binary { lhs, rhs, .. } => {
            expr_columns_within(columns, lhs)?;
            expr_columns_within(columns, rhs)
        }
    }
}

/// クエリのウィンドウ項目（SQL-30・TASK-214、Issue #930）が参照する列
/// （`PARTITION BY`・`ORDER BY`・集計引数）が、参照先が公開する列集合
/// （`view_columns`）に収まっているかを検査する（[`check_columns_within_view`]の
/// ウィンドウ項目向け実装。RLS-10 (b) の趣旨: ビューの非公開列を順位・累積集計の
/// キー・引数に使い、値の推測手段にすることを防ぐ）。順位関数（`ROW_NUMBER`／
/// `RANK`／`DENSE_RANK`）・`COUNT(*)` は列参照を持たないため対象外。
pub(crate) fn check_window_columns_within_view(
    view_columns: Option<&[String]>,
    window_items: &[WindowSelectItem],
) -> Result<(), SqlSurfaceError> {
    let Some(columns) = view_columns else {
        return Ok(());
    };
    for item in window_items {
        for col in &item.partition_by {
            if !columns.iter().any(|vc| vc == col) {
                return Err(SqlSurfaceError::InvalidInput {
                    detail: format!("unknown column: {col}"),
                });
            }
        }
        for (col, _) in &item.order_by {
            if !columns.iter().any(|vc| vc == col) {
                return Err(SqlSurfaceError::InvalidInput {
                    detail: format!("unknown column: {col}"),
                });
            }
        }
        if let Some(AggregateArg::Expr(Expr::Ident(name))) = &item.arg {
            if !columns.iter().any(|vc| vc == name) {
                return Err(SqlSurfaceError::InvalidInput {
                    detail: format!("unknown column: {name}"),
                });
            }
        }
    }
    Ok(())
}

/// 単純な `column` フィールドを持つ述語からその列名を取り出す。`PredicateCall`は
/// 名前だけの述語呼び出し形（列参照を持たない）のため対象外。`Expression`
/// （式ベース）は呼び出し元（[`check_columns_within_view`]）が
/// [`expr_columns_within`] で個別に検査するためここには渡らない。
fn predicate_column(pred: &WherePredicate) -> Option<&str> {
    match pred {
        WherePredicate::Equality { column, .. } => Some(column),
        WherePredicate::Prefix { column, .. } => Some(column),
        WherePredicate::BoolEquality { column, .. } => Some(column),
        WherePredicate::BoolColumn { column } => Some(column),
        WherePredicate::Compare { column, .. } => Some(column),
        WherePredicate::PredicateCall { .. } | WherePredicate::Expression(_) => None,
        // `check_predicate_columns_within` が `Or` を個別に再帰処理するため
        // 到達しない（本関数へは単純形の述語のみが渡る）。
        WherePredicate::Or(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::allowlist::render_view_body;
    use super::super::lexer;
    use super::*;

    /// render_view_body → 再トークン化 → parse_view_body が元と等価な AST を
    /// 復元することを固定する（TABLE-18・SQL-23・TASK-205、Issue #909 の
    /// round-trip 契約）。
    #[test]
    fn render_body_round_trips_through_reparse() {
        let cases = [
            "SELECT * FROM docs",
            "SELECT id, body FROM docs WHERE lang = 'ja'",
            "SELECT id FROM docs WHERE body LIKE 'foo%' AND flag",
            "SELECT id FROM docs WHERE score > '1' AND active = true",
            "SELECT id FROM docs WHERE note = 'it''s ok'",
        ];
        for sql in cases {
            let tokens = lexer::tokenize(sql).expect("tokenize");
            let parsed = parse_view_body(&tokens).expect("parse");
            let rendered = render_view_body(&parsed);
            let retokens = lexer::tokenize(&rendered).expect("retokenize");
            let reparsed = parse_view_body(&retokens).expect("reparse");
            assert_eq!(parsed, reparsed, "sql={sql} rendered={rendered}");
        }
    }
}
