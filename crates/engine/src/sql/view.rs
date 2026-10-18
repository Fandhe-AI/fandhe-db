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
    classify_view_body, parse_view_body, AggregateArg, AggregateSelectItem, GroupByClause,
    Projection, ScalarOrderKey, ScanOrderKey, SelectItem, SqlSurfaceError, Statement, TableLookup,
    ViewBodyCheck, ViewBodyKind, WherePredicate, WindowSelectItem,
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
        /// 列別名リスト付きビュー（TABLE-41 (1)・Issue #1659）が連鎖に含まれるときの、
        /// `view_columns`（公開名）と同じ並びの「公開名 → 基底表の物理列名」対応。連鎖に別名の段が
        /// 無ければ `None`（従来どおり公開名＝物理名）。`Some` のときの消費側は、列スコープ検査を
        /// 公開名に対して先に行ってから `sql::view_alias` で物理名へ書き換えるか、書き換えに
        /// 未対応の形を `0A000` で拒否する（fail-closed）。
        column_map: Option<Vec<(String, String)>>,
    },
    /// 評価後射影形ビュー（Issue #1192。`LIMIT`・`ORDER BY`・集計・JOIN を含む
    /// 本文）。インライン展開できない（外側の `WHERE` を合成すると `LIMIT`／集計の
    /// 意味が変わる）ため、本文をそのまま 1 文として実行し、結果に対して外側の
    /// クエリ（`sql::view_buffered`）を適用する。
    ///
    /// 連鎖（Issue #1411）: クエリが直接参照した名前と評価後射影形ビューの間に単純形
    /// ビューの段がある場合、各段は下位の本文の上に重ねた行形の
    /// [`Statement::BufferedView`]（`LIMIT` なし）として `body` に畳み込まれる。評価後射影形
    /// 本文の FROM が別の評価後射影形ビューなら、本文自体が [`Statement::BufferedView`]
    /// になる。いずれも実行・Describe は既存の再帰（本文の実行）に乗る。
    Buffered {
        /// クエリが参照したビュー名。
        view_name: String,
        /// 参照時に再検証済みの本文（`Scan`／`Aggregate`／`Join`／`SetOperation`／
        /// `BufferedView` のいずれか）。
        body: Box<Statement>,
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
    let mut chain: Vec<(String, super::allowlist::ParsedViewBody, Vec<String>)> = Vec::new();
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
                column_aliases,
            }) => {
                let parsed = match reparse_stored_body(&body_sql) {
                    Ok(parsed) => parsed,
                    Err(_) => {
                        // 単純形として再パースできない本文は評価後射影形（Issue #1192）。
                        // 本文を再検証し（本文の FROM が別の評価後射影形ビューなら再帰的に
                        // 解決される。深さは `BufferedBodyLookup` が上限で打ち切る。
                        // Issue #1411）、クエリとの間にある単純形ビューの段は内側から順に
                        // 行形の外側として重ねる。
                        let mut stmt = reparse_buffered_body(lookup, &body_sql)?;
                        // 列別名（TABLE-41・Issue #1659）は、その段の出力列を先頭から改名する
                        // 素通しの層として本文・各段の直後に重ねる（`plan_outer` が改名する）。
                        if !column_aliases.is_empty() {
                            stmt = super::allowlist::buffered_alias_layer(
                                &current,
                                stmt,
                                column_aliases,
                            );
                        }
                        for (layer_name, layer, layer_aliases) in chain.into_iter().rev() {
                            stmt = super::allowlist::buffered_layer_statement(
                                &layer_name,
                                stmt,
                                layer,
                            )?;
                            if !layer_aliases.is_empty() {
                                stmt = super::allowlist::buffered_alias_layer(
                                    &layer_name,
                                    stmt,
                                    layer_aliases,
                                );
                            }
                        }
                        return Ok(Resolved::Buffered {
                            view_name: name.to_string(),
                            body: Box::new(stmt),
                        });
                    }
                };
                depth = depth.checked_add(1).ok_or_else(corrupt_view_error_fn)?;
                if depth > MAX_VIEW_NESTING_DEPTH {
                    return Err(SqlSurfaceError::payload_too_large(
                        "view nesting depth exceeds limit",
                    ));
                }
                chain.push((current.clone(), parsed, column_aliases));
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
    // 列別名の段（TABLE-41・Issue #1659）を含むときの「公開名 → 物理名」対応。別名の段が
    // まだ現れていない間は `None`（公開名＝物理名）。
    let mut column_map: Option<Vec<(String, String)>> = None;
    let mut acc_predicates: Vec<WherePredicate> = Vec::new();
    for (_, mut view, aliases) in chain.into_iter().rev() {
        // ビュー本文（`ParsedViewBody`）は構文上 `ORDER BY` を持たない
        // （[`parse_view_body`] のドキュメント参照）ため、連鎖の各段の検査には
        // 空スライスを渡す（クエリ自身の ORDER BY 検査は呼び出し元
        // `sql::allowlist::validate_sql_tokens` が別途行う）。
        check_columns_within_view(
            exposed.as_deref(),
            &view.projection,
            &view.where_predicates,
            &[],
        )?;
        // この段の `WHERE` は親の公開名で書かれている。親が列別名を持つなら、別名を重ねる前の
        // 対応（親の `column_map`）で物理名へ書き換えてから合成する（TABLE-41・Issue #1659）。
        let layer_predicates = match &column_map {
            Some(map) => super::view_alias::rewrite_where(
                &super::view_alias::ColumnMap::new(map),
                std::mem::take(&mut view.where_predicates),
            )?,
            None => std::mem::take(&mut view.where_predicates),
        };
        if aliases.is_empty() {
            if let Projection::Columns(cols) = view.projection {
                if let Some(map) = &column_map {
                    // 親の別名対応を通して物理名を引き継ぐ（親に無い列は検査済みのため到達しない）。
                    let next = cols
                        .iter()
                        .map(|c| (c.clone(), physical_of(map, c)))
                        .collect::<Vec<_>>();
                    column_map = Some(next);
                }
                exposed = Some(cols);
            }
        } else {
            let out_names: Vec<String> = match &view.projection {
                Projection::Columns(cols) => cols.clone(),
                Projection::All => match &exposed {
                    Some(cols) => cols.clone(),
                    None => star_columns_of_table(lookup, &base_table)?,
                },
                // 単純形の段は列射影のみを許す（式項目への別名は未対応。fail-closed）。
                Projection::Items(_) => {
                    return Err(SqlSurfaceError::feature_not_supported(
                        "view column aliases over a computed projection are not supported",
                    ));
                }
            };
            if aliases.len() > out_names.len() {
                return Err(SqlSurfaceError::unsupported(
                    "view column alias list is longer than the view's output columns",
                ));
            }
            let mut next: Vec<(String, String)> = Vec::with_capacity(out_names.len());
            for (i, out) in out_names.iter().enumerate() {
                let physical = match &column_map {
                    Some(map) => physical_of(map, out),
                    None => out.clone(),
                };
                let public = aliases.get(i).cloned().unwrap_or_else(|| out.clone());
                // 公開名の重複は曖昧なまま片方を選ばず拒否する（`42702`。fail-closed）。
                if next.iter().any(|(p, _)| p == &public) {
                    return Err(SqlSurfaceError::ambiguous_column(public));
                }
                next.push((public, physical));
            }
            exposed = Some(next.iter().map(|(p, _)| p.clone()).collect());
            column_map = Some(next);
        }
        // 内側（より深い）のビューの述語を先頭に置く（§設計「述語合成」）。
        // ここでは内側から外側へ順に処理しているため、単純な追記でよい。
        acc_predicates.extend(layer_predicates);
    }

    Ok(Resolved::View {
        base_table,
        view_predicates: acc_predicates,
        view_columns: exposed,
        column_map,
    })
}

/// `CREATE VIEW` 本文の出力列の導出結果（[`body_output`]）。
pub(crate) enum BodyOutput {
    /// 出力列名が確定した（単純形本文。`SELECT *` は `id` 先頭＋利用者列）。
    Names(Vec<String>),
    /// 評価後射影形の本文（または評価後射影形ビューの上に重なる単純形の段）。列名は文の
    /// Describe（本文を実行しない）で得る。
    Statement(Box<Statement>),
}

/// `CREATE VIEW v (a, b) AS <body>`（TABLE-41 (1)・Issue #1659）の列別名の長さ検査・最終名の
/// 重複検査のために、本文の出力列を実カタログ `lookup` で導出する。単純形本文は参照時の別名適用と
/// 同じ規則（[`star_columns_of_table`]・[`resolve_from`]）で名前を確定し、`*` の展開規則を 2 か所に
/// 複製しない。参照先が存在しない場合は `42P01`（`resolve_from` と同じ）。
pub(crate) fn body_output(
    lookup: &impl TableLookup,
    kind: super::allowlist::ViewBodyKind,
) -> Result<BodyOutput, SqlSurfaceError> {
    let parsed = match kind {
        ViewBodyKind::Buffered { stmt, .. } => return Ok(BodyOutput::Statement(stmt)),
        ViewBodyKind::Simple(parsed) => parsed,
    };
    let parent: Vec<String> = match resolve_from(lookup, &parsed.table_name)? {
        Resolved::Table => star_columns_of_table(lookup, &parsed.table_name)?,
        Resolved::View {
            base_table,
            view_columns,
            ..
        } => match view_columns {
            Some(cols) => cols,
            None => star_columns_of_table(lookup, &base_table)?,
        },
        Resolved::Buffered { body, .. } => {
            let layer = super::allowlist::buffered_layer_statement(
                &parsed.table_name,
                *body,
                parsed.clone(),
            )?;
            return Ok(BodyOutput::Statement(Box::new(layer)));
        }
    };
    match &parsed.projection {
        Projection::Columns(cols) => Ok(BodyOutput::Names(cols.clone())),
        Projection::All => Ok(BodyOutput::Names(parent)),
        Projection::Items(_) => Err(SqlSurfaceError::feature_not_supported(
            "view column aliases over a computed projection are not supported",
        )),
    }
}

/// 公開名 `public` に対応する物理列名（対応表に無ければ同名。呼び出し元は公開列の範囲検査を
/// 済ませているため、無い場合は別名を持たない段の列）。
fn physical_of(map: &[(String, String)], public: &str) -> String {
    map.iter()
        .find(|(p, _)| p == public)
        .map(|(_, phys)| phys.clone())
        .unwrap_or_else(|| public.to_string())
}

/// 基底表に対する `SELECT *` の出力列名（`id` が先頭で、利用者列がスキーマ順に続く。
/// `parser::bind_projection` の `Projection::All` と同じ並び。`tenant_id`・`visibility` は含めない）。
/// 列を導出できない lookup（構造検証専用など）は `0A000`（fail-closed）。
/// `sql::view::star_output_columns`（CREATE 時の長さ検査）と参照時の別名適用が共有する。
pub(crate) fn star_columns_of_table(
    lookup: &impl TableLookup,
    table: &str,
) -> Result<Vec<String>, SqlSurfaceError> {
    let Some(out) = lookup.table_star_columns(table)? else {
        return Err(SqlSurfaceError::feature_not_supported(
            "view column aliases need the table's columns, which are unavailable here",
        ));
    };
    // 実カラム `id` を宣言する表の `*` は `id` が 2 回現れる（疑似列と実カラム）。投影は名前で
    // 物理列を引くため疑似列を実カラムと区別して写せない。値を取り違えないよう拒否する（fail-closed）。
    if out.iter().filter(|c| c.as_str() == "id").count() > 1 {
        return Err(SqlSurfaceError::feature_not_supported(
            "view column aliases over SELECT * of a table with a declared id column are not supported",
        ));
    }
    Ok(out)
}

/// 格納済み `body_sql`（`sql::allowlist::render_view_body` の出力）を再トークン化・
/// 再パースする。第 2 の SQL パーサーを作らず、`CREATE VIEW` 時と完全に同じ
/// [`parse_view_body`] を経由する。失敗（カタログ破損・非互換な将来フォーマット
/// 変更）は `body_sql` の内容をエラーへ含めず、固定文言の
/// `SqlSurfaceError::Internal` へ丸める（security.md P0）。
fn reparse_stored_body(
    body_sql: &str,
) -> Result<super::allowlist::ParsedViewBody, SqlSurfaceError> {
    // 旧版が未引用で保存した列参照 `true`／`false` を列述語へ戻す（Issue #1526）。
    let tokens =
        super::lexer::tokenize_stored_view_body(body_sql).map_err(|_| corrupt_view_error())?;
    parse_view_body(&tokens).map_err(|_| corrupt_view_error())
}

/// 単純形として再パースできなかった格納本文を評価後射影形（Issue #1192）として
/// 再検証する。第 2 のパーサーは作らず、`CREATE VIEW` 時と同じ
/// [`classify_view_body`] の経路（許可リスト構造検証＋本文形状検査）を、
/// 参照時の実カタログ（[`BufferedBodyLookup`] 越し）で通す。本文の FROM が別の評価後射影形
/// ビューを指す場合は再帰的に解決する（Issue #1411）が、入れ子の深さが
/// `MAX_VIEW_NESTING_DEPTH` を超えたら `54000`。それ以外の失敗（破損・非互換な形状）は
/// 固定文言の `XX000` へ丸める（本文のリテラルを含めない）。
fn reparse_buffered_body(
    lookup: &impl TableLookup,
    body_sql: &str,
) -> Result<Statement, SqlSurfaceError> {
    let tokens =
        super::lexer::tokenize_stored_view_body(body_sql).map_err(|_| corrupt_view_error())?;
    let depth = lookup
        .buffered_depth()
        .checked_add(1)
        .ok_or_else(corrupt_view_error_fn)?;
    // 正常経路では作成時の深さ検査（`catalog::Storage::create_view`）により上限内。
    // 破損したカタログの循環・過大な連鎖でも再帰を有限にする。
    if depth > MAX_VIEW_NESTING_DEPTH as usize {
        return Err(SqlSurfaceError::payload_too_large(
            "view nesting depth exceeds limit",
        ));
    }
    let guarded = BufferedBodyLookup {
        inner: lookup,
        depth,
    };
    match classify_view_body(&tokens, &guarded, ViewBodyCheck::Resolved) {
        Ok(ViewBodyKind::Buffered { stmt, .. }) => Ok(*stmt),
        Ok(ViewBodyKind::Simple(_)) => Err(corrupt_view_error()),
        // 入れ子の深さ超過は規則に基づく拒否として呼び出し元へ伝える。
        Err(e @ SqlSurfaceError::PayloadTooLarge { .. }) => Err(e),
        Err(_) => Err(corrupt_view_error()),
    }
}

/// 評価後射影形の本文を参照時に再検証する際の [`TableLookup`] ラッパー
/// （Issue #1192・Issue #1411）。ビュー定義の取得はそのまま委譲し、入れ子の深さ
/// （[`TableLookup::buffered_depth`]）だけを 1 段ずつ進める。これにより評価後射影形ビューの
/// 連鎖の再検証が `MAX_VIEW_NESTING_DEPTH` で打ち切られ、カタログ破損で循環があっても
/// 無限再帰にならない。
struct BufferedBodyLookup<'a> {
    inner: &'a dyn TableLookup,
    depth: usize,
}

impl TableLookup for BufferedBodyLookup<'_> {
    fn table_exists(&self, name: &str) -> Result<bool, SqlSurfaceError> {
        self.inner.table_exists(name)
    }

    fn view_definition(&self, name: &str) -> Result<Option<ViewDef>, SqlSurfaceError> {
        self.inner.view_definition(name)
    }

    fn table_columns(&self, name: &str) -> Result<Option<Vec<String>>, SqlSurfaceError> {
        self.inner.table_columns(name)
    }

    fn table_star_columns(&self, name: &str) -> Result<Option<Vec<String>>, SqlSurfaceError> {
        self.inner.table_star_columns(name)
    }

    fn buffered_depth(&self) -> usize {
        self.depth
    }
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
    order_by: &[ScalarOrderKey],
) -> Result<(), SqlSurfaceError> {
    let Some(columns) = view_columns else {
        return Ok(());
    };
    match projection {
        Projection::All => {}
        Projection::Columns(cols) => {
            for c in cols {
                if !columns.iter().any(|vc| vc == c) {
                    return Err(SqlSurfaceError::undefined_column(c));
                }
            }
        }
        Projection::Items(items) => {
            for item in items {
                match item {
                    SelectItem::Column(c) => {
                        if !columns.iter().any(|vc| vc == c) {
                            return Err(SqlSurfaceError::undefined_column(c));
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
    // Issue #915・SQL-25: スカラー ORDER BY のキー列（疑似列 `id` は
    // `columns` に含まれないため常に許可リスト外——ビュー経由の広域取得で
    // `id` 順は使えない。将来の拡張候補として `docs/design/
    // scalar-order-by-scan.md` に記録する）。
    for key in order_by {
        if !columns.iter().any(|vc| vc == &key.column) {
            return Err(SqlSurfaceError::undefined_column(&key.column));
        }
    }
    Ok(())
}

/// 式キーを含む広域取得 `ORDER BY`（Issue #1188）の列参照が、ビューの公開列集合に収まる
/// ことを検査する。列名キーは疑似列 `id` を含め [`check_columns_within_view`] と同じ規則
/// （公開列のみ）、式キーは [`expr_columns_within`] で式木内の全列参照を検査する。
pub(crate) fn check_order_exprs_within_view(
    view_columns: Option<&[String]>,
    order_keys: &[ScanOrderKey],
) -> Result<(), SqlSurfaceError> {
    let Some(columns) = view_columns else {
        return Ok(());
    };
    for key in order_keys {
        match key {
            ScanOrderKey::Column(k) => {
                if !columns.iter().any(|vc| vc == &k.column) {
                    return Err(SqlSurfaceError::undefined_column(&k.column));
                }
            }
            ScanOrderKey::Expr { expr, .. } => expr_columns_within(columns, expr)?,
        }
    }
    Ok(())
}

/// 集計 SELECT（`SELECT DISTINCT` の脱糖形を含む）が参照する列が、参照先ビューの
/// 公開列集合（`view_columns`）に収まっているかを検査する（Issue #1192・TABLE-18・
/// RLS-10 (b)。[`check_columns_within_view`] の集計向け実装）。グループキー・
/// 集計引数（式木は [`expr_columns_within`] で再帰検査）・`GROUP BY` 列・
/// `WHERE`（[`check_predicate_columns_within`]。`OR`／`NOT` も再帰）を検査する。
/// これらを検査しないと、ビューが公開しない列を集計キー・引数・フィルタに使って
/// 値を推測できてしまう（filter oracle。security.md「アクセス制御の不備」）。
/// `ORDER BY` の対象は SELECT リスト項目の実効名（別名または既定名）か、公開列の
/// いずれかに限る（式キーは式内の識別子が同じ集合に収まること）。従来形の `HAVING` は
/// 項目名への参照のため対象外、式述語の `HAVING`（Issue #1188）は式内の識別子を検査する。
/// `view_columns` が `None`（どの段も列を絞り込んでいない）なら検査不要。
pub(crate) fn check_aggregate_columns_within_view(
    view_columns: Option<&[String]>,
    items: &[AggregateSelectItem],
    group_by: Option<&GroupByClause>,
    where_predicates: &[WherePredicate],
) -> Result<(), SqlSurfaceError> {
    let Some(columns) = view_columns else {
        return Ok(());
    };
    let unknown = |c: &str| SqlSurfaceError::undefined_column(c);
    let mut effective_names: Vec<String> = Vec::new();
    for item in items {
        match item {
            AggregateSelectItem::GroupKey { column, alias } => {
                if !columns.iter().any(|vc| vc == column) {
                    return Err(unknown(column));
                }
                effective_names.push(alias.clone().unwrap_or_else(|| column.clone()));
            }
            AggregateSelectItem::Aggregate(agg) => {
                if let AggregateArg::Expr(expr) = &agg.arg {
                    expr_columns_within(columns, expr)?;
                }
                effective_names.push(
                    agg.alias
                        .clone()
                        .unwrap_or_else(|| agg.func.default_alias().to_string()),
                );
            }
            // Issue #1541: 集計式項目は、式中の集計呼び出しの引数と集計の外の裸識別子の
            // 双方を公開列に限定する（非公開列を式経由で参照させない）。
            AggregateSelectItem::Expr {
                expr, aggregates, ..
            } => {
                for agg in aggregates {
                    if let AggregateArg::Expr(e) = &agg.arg {
                        expr_columns_within(columns, e)?;
                    }
                }
                for c in crate::sql::allowlist::expr_bare_idents(expr) {
                    if !columns.iter().any(|vc| vc == &c) {
                        return Err(unknown(&c));
                    }
                }
                effective_names.push(item.effective_name());
            }
        }
    }
    if let Some(gb) = group_by {
        for c in &gb.columns {
            if !columns.iter().any(|vc| vc == c) {
                return Err(unknown(c));
            }
        }
        // Issue #1188: 式キーの `ORDER BY`・式述語の `HAVING` は、式内の識別子が
        // 項目の実効名か公開列のいずれかであることを検査する（束縛段もグループ出力の
        // 名前にしか解決しないが、非公開列を式経由で参照させない多層防御）。
        let mut allowed: Vec<String> = columns.to_vec();
        allowed.extend(effective_names.iter().cloned());
        for key in &gb.order_by {
            if let Some(expr) = &key.expr {
                expr_columns_within(&allowed, expr)?;
                continue;
            }
            let known = effective_names.iter().any(|n| n == &key.target)
                || columns.iter().any(|vc| vc == &key.target);
            if !known {
                return Err(unknown(&key.target));
            }
        }
        for h in &gb.having_exprs {
            expr_columns_within(&allowed, &h.lhs)?;
            expr_columns_within(&allowed, &h.rhs)?;
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
                    return Err(SqlSurfaceError::undefined_column(c));
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
        Expr::Number(_) | Expr::String(_) => Ok(()),
        Expr::Ident(name) => {
            if columns.iter().any(|vc| vc == name) {
                Ok(())
            } else {
                Err(SqlSurfaceError::undefined_column(name))
            }
        }
        Expr::Call { args, .. } => {
            for arg in args {
                expr_columns_within(columns, arg)?;
            }
            Ok(())
        }
        Expr::Binary { lhs, rhs, .. } | Expr::JsonAccess { lhs, rhs, .. } => {
            expr_columns_within(columns, lhs)?;
            expr_columns_within(columns, rhs)
        }
        // `CASE`／`COALESCE`／`NULLIF`（対象ビヘイビア: SQL-26。Issue #921）は
        // 列参照を子に持ちうるため再帰的に検査する。`Expr::Null` は列参照を
        // 持たない。
        Expr::Null | Expr::ValueFunction { .. } => Ok(()),
        // 照合指定（TABLE-28・Issue #1609）は内側の式の列参照を検査する。
        Expr::Collate { expr, .. } | Expr::Cast { expr, .. } => expr_columns_within(columns, expr),
        // 配列コンストラクタ・添字（TABLE-35・Issue #1602）は子の列参照を検査する。
        Expr::ArrayConstructor(_) | Expr::Subscript { .. } | Expr::Quantified { .. } => {
            for c in expr.array_expr_children().into_iter().flatten() {
                expr_columns_within(columns, c)?;
            }
            Ok(())
        }
        Expr::SqlJsonQuery(q) => {
            for c in q.children() {
                expr_columns_within(columns, c)?;
            }
            Ok(())
        }
        Expr::Case { whens, else_result } => {
            for (cond, result) in whens {
                expr_columns_within(columns, cond)?;
                expr_columns_within(columns, result)?;
            }
            if let Some(else_result) = else_result {
                expr_columns_within(columns, else_result)?;
            }
            Ok(())
        }
        Expr::Coalesce(args) => {
            for a in args {
                expr_columns_within(columns, a)?;
            }
            Ok(())
        }
        Expr::NullIf(lhs, rhs) => {
            expr_columns_within(columns, lhs)?;
            expr_columns_within(columns, rhs)
        }
        // `DATE`／`TIMESTAMP` 型付きリテラル（対象ビヘイビア: SQL-26。
        // Issue #920）は列参照を持たない。
        Expr::DateLiteral(_)
        | Expr::TimestampLiteral(_)
        | Expr::IntervalLiteral(_)
        | Expr::TimeLiteral(_)
        | Expr::TimestampTzLiteral { .. } => Ok(()),
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
                return Err(SqlSurfaceError::undefined_column(col));
            }
        }
        for (col, _) in &item.order_by {
            if !columns.iter().any(|vc| vc == col) {
                return Err(SqlSurfaceError::undefined_column(col));
            }
        }
        if let Some(AggregateArg::Expr(Expr::Ident(name))) = &item.arg {
            if !columns.iter().any(|vc| vc == name) {
                return Err(SqlSurfaceError::undefined_column(name));
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
        // SQL-24（TASK-208 ポインタ）。
        WherePredicate::InList { column, .. } => Some(column),
        WherePredicate::Between { column, .. } => Some(column),
        WherePredicate::IsNull { column, .. } => Some(column),
        // `NOT` は列スコープ検査の対象外にできない: 再帰して内側の列を見ないと
        // `NOT hidden_col = 'x'` のような否定越しに非公開列の存在情報が漏れる
        // （A01 アクセス制御の不備。`.claude/rules/security.md` 対応）。
        WherePredicate::Not(inner) => predicate_column(inner),
        WherePredicate::PredicateCall { .. } | WherePredicate::Expression(_) => None,
        // `check_predicate_columns_within` が `Or` を個別に再帰処理するため
        // 到達しない（本関数へは単純形の述語のみが渡る）。
        WherePredicate::Or(_) => None,
        // Issue #927・SQL-29 (a)・TASK-213: `<列> IN (SELECT ...)` の `column` は
        // 外側クエリが参照する実在の列（ビューの公開列範囲チェック対象）。
        // `EXISTS (...)` は外側の列を参照しないため対象外。
        WherePredicate::InSubquery { column, .. } => Some(column),
        WherePredicate::ScalarSubqueryCompare { column, .. } => Some(column),
        WherePredicate::Exists { .. } => None,
        // 解決段が生成する内部専用の葉（疑似列 `id` の比較）。列参照を持たない。
        WherePredicate::IdCompare { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::allowlist::render_view_body;
    use super::super::lexer;
    use super::*;

    /// SQL-24（TASK-208 ポインタ）・A01（`.claude/rules/security.md`）回帰:
    /// `NOT hidden_col = 'x'` のように `WHERE` 述語が `Not` で列参照を包んでも、
    /// 列スコープ検査（`check_columns_within_view`）が再帰して内側の列を検出し、
    /// ビューが公開しない列への参照を拒否する（否定越しに非公開列の存在情報が
    /// 漏れない）。
    #[test]
    fn check_columns_within_view_rejects_hidden_column_through_not() {
        let view_columns = vec!["id".to_string()]; // "hidden" は非公開。
        let where_predicates = vec![WherePredicate::Not(Box::new(WherePredicate::Equality {
            column: "hidden".to_string(),
            value: "x".to_string(),
        }))];
        let err = check_columns_within_view(
            Some(&view_columns),
            &Projection::All,
            &where_predicates,
            &[],
        )
        .expect_err("NOT-wrapped reference to a hidden column must be rejected");
        assert!(matches!(err, SqlSurfaceError::UndefinedColumn { .. }));
    }

    /// 固定のビュー定義群を返すテスト用 lookup（テーブルは `docs` のみ）。
    struct MapLookup(Vec<(&'static str, ViewDef)>);

    impl TableLookup for MapLookup {
        fn table_exists(&self, name: &str) -> Result<bool, SqlSurfaceError> {
            Ok(name == "docs")
        }

        fn view_definition(&self, name: &str) -> Result<Option<ViewDef>, SqlSurfaceError> {
            Ok(self
                .0
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, d)| d.clone()))
        }
    }

    fn def(base: &str, body: &str) -> ViewDef {
        ViewDef {
            base_relation: base.to_string(),
            body_sql: body.to_string(),
            column_aliases: Vec::new(),
        }
    }

    /// Issue #1192: 評価後射影形の本文を参照すると `Resolved::Buffered` になる。
    #[test]
    fn resolves_buffered_view_body() {
        let lookup = MapLookup(vec![(
            "v",
            def("docs", "SELECT COUNT ( * ) AS n FROM docs"),
        )]);
        match resolve_from(&lookup, "v") {
            Ok(Resolved::Buffered { body, .. }) => {
                assert!(matches!(*body, Statement::Aggregate(_)));
            }
            other => panic!("expected Buffered, got ok={}", other.is_ok()),
        }
    }

    /// 破損した本文は本文のリテラルを含まない固定文言の `XX000` になる。
    #[test]
    fn corrupt_buffered_body_is_internal_error_without_literals() {
        let lookup = MapLookup(vec![(
            "v",
            def("docs", "SELECT secret_literal FROM ( docs ) 'leak-me'"),
        )]);
        let err = resolve_from(&lookup, "v").err().expect("must fail");
        assert_eq!(err.wire_code(), "XX000");
        assert!(!format!("{err:?}").contains("leak-me"));
    }

    /// 評価後射影形ビューの連鎖（Issue #1411）。評価後射影形本文の FROM が評価後射影形
    /// ビューなら本文自体が `BufferedView`、単純形ビューが評価後射影形ビューを包む場合は
    /// その段が行形の `BufferedView`（`LIMIT` なし）として畳み込まれる。
    #[test]
    fn nested_buffered_views_resolve_into_layered_statements() {
        let lookup = MapLookup(vec![
            ("inner_b", def("docs", "SELECT id FROM docs LIMIT 3")),
            ("outer_b", def("inner_b", "SELECT id FROM inner_b LIMIT 2")),
            ("outer_s", def("inner_b", "SELECT id FROM inner_b")),
            (
                "outer_s2",
                def("outer_s", "SELECT id FROM outer_s WHERE id = '1'"),
            ),
        ]);
        for name in ["outer_b", "outer_s", "outer_s2"] {
            match resolve_from(&lookup, name) {
                Ok(Resolved::Buffered { view_name, body }) => {
                    assert_eq!(view_name, name);
                    assert!(
                        matches!(*body, Statement::BufferedView(_)),
                        "name={name}: expected a layered statement"
                    );
                }
                other => panic!("name={name}: expected Buffered, got ok={}", other.is_ok()),
            }
        }
    }

    /// 破損カタログで循環していても、入れ子の深さ上限で `54000` になり無限再帰にならない。
    #[test]
    fn cyclic_buffered_view_hits_the_depth_limit() {
        let lookup = MapLookup(vec![(
            "loop_b",
            def("loop_b", "SELECT id FROM loop_b LIMIT 2"),
        )]);
        let err = resolve_from(&lookup, "loop_b").err().expect("must fail");
        assert_eq!(err.wire_code(), "54000");
    }

    /// 集計の列スコープ検査は `OR`／`NOT`／式を越えて非公開列を拒否する。
    #[test]
    fn aggregate_column_scope_rejects_hidden_columns() {
        let tokens = lexer::tokenize(
            "SELECT lang, SUM(n) AS s FROM docs WHERE NOT hidden = 'x' GROUP BY lang ORDER BY s LIMIT 5",
        )
        .expect("tokenize");
        let stmt = crate::sql::allowlist::validate_sql_tokens(
            &tokens,
            &crate::sql::allowlist::StructuralOnlyLookup,
        )
        .expect("validate");
        let Statement::Aggregate(agg) = stmt else {
            panic!("expected aggregate");
        };
        let cols = vec!["lang".to_string(), "n".to_string()];
        let err = check_aggregate_columns_within_view(
            Some(&cols),
            agg.items(),
            agg.group_by(),
            agg.where_predicates(),
        )
        .expect_err("hidden column through NOT");
        assert!(matches!(err, SqlSurfaceError::UndefinedColumn { .. }));
        // 公開列のみ（`s` は SELECT リスト項目名）なら通る。
        check_aggregate_columns_within_view(Some(&cols), agg.items(), agg.group_by(), &[])
            .expect("visible columns and item alias");
    }

    /// 値関数予約語と同名の列は引用付きで保存・再解析され、旧版の未引用保存本文も列参照へ戻る
    /// （SQL-37・Issue #1586）。
    #[test]
    fn value_function_named_column_survives_view_storage() {
        let tokens = lexer::tokenize("SELECT \"current_date\", id FROM q").expect("tokenize");
        let parsed = parse_view_body(&tokens).expect("parse");
        let rendered = render_view_body(&parsed);
        assert!(rendered.contains("\"current_date\""), "rendered={rendered}");
        let retokens = lexer::tokenize_stored_view_body(&rendered).expect("retokenize");
        assert_eq!(parse_view_body(&retokens).expect("reparse"), parsed);
        let legacy = lexer::tokenize_stored_view_body("SELECT current_date, id FROM q")
            .expect("legacy tokenize");
        assert_eq!(parse_view_body(&legacy).expect("legacy parse"), parsed);
    }

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
