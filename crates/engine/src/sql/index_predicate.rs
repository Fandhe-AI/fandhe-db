//! 部分索引の述語（`CREATE INDEX ... WHERE <述語>`）の束縛・正規化・評価（INDEX-11、Issue #1654）。
//!
//! 役割と責務境界:
//! - `catalog::Storage::create_index_bound` が書き込みトランザクション内で [`bind_index_predicate`] を
//!   呼び、正規化テキストと参照列を確定してから保存する（呼び出し元の生テキストは保存しない）。
//! - `constraint::enforce_row_constraints_in_txn`（DML の検査点）が [`CompiledPartialIndexes`] を
//!   1 文につき 1 回コンパイルし、書き込んだ行ごとに述語を評価する。述語が TRUE の行だけ式要素を
//!   評価・索引化できることを確かめる（述語が NULL・FALSE の行は索引外で、式要素は評価しない）。
//! - `sql::scalar_index` が部分索引の派生キー（述語 TRUE のスロット集合）を構築するときも、同じ
//!   [`BoundIndexPredicate::eval`] を使う（索引に入る行の集合と「`WHERE <述語>` が選ぶ行の集合」を
//!   一致させるため、第 2 の評価器は作らない）。
//!
//! 述語の束縛は `WHERE` と同じ経路（`sql::parser::bind_where_predicates`）を使う。CHECK 用の束縛
//! （`bind_check_predicates`）は CHECK 固有の拒否と書き換え無効化を持ち込み、意味論が WHERE とずれる
//! ため使わない。セッションに依存する述語（TZ 依存の比較・日時の特殊値）は `42P17`、予約列
//! （`tenant_id`・`visibility`）と `visible()` は `42601` で拒否する。
//!
//! 判定表と PostgreSQL との差は `docs/design/partial-index.md` を参照。

use crate::catalog::{ColumnType, DeclaredExpressionKey, DeclaredPartialIndex, TableSchema};
use crate::sql::allowlist::{SqlSurfaceError, WherePredicate};
use crate::sql::udf_call::Expr;
use crate::sql::where_tree::{BoundConjunction, BoundOrGroup};
use crate::tenant::TenantWriteError;
use std::cell::RefCell;

/// 述語の正規化テキストの長さ上限（バイト。式要素と同値）。
const MAX_PREDICATE_SQL_LEN: usize = 4096;
/// 正規化テキストへ往復できない述語の拒否文言（固定。値を含めない）。
const MSG_NOT_STORABLE: &str = "index predicate cannot be stored in normalized form";
/// 述語が長すぎる場合の拒否文言（固定）。
const MSG_TOO_LONG: &str = "index predicate is too long";
/// 非決定的な述語の拒否文言（式索引と同じ基準）。
const MSG_NOT_IMMUTABLE: &str = "functions in index predicate must be marked IMMUTABLE";
/// 予約列を参照する述語の拒否文言。
const MSG_RESERVED: &str = "index predicate must not reference reserved columns";

/// 束縛・往復一致の確認を終えた部分索引の述語。
#[derive(Debug)]
pub(crate) struct BoundIndexPredicate {
    /// 往復一致を確認済みの正規化 SQL テキスト（カタログへ保存する値）。
    pub(crate) sql: String,
    /// 参照するスキーマ列名（重複なし・スキーマ宣言順。疑似列 `id` は含めない）。
    pub(crate) referenced_columns: Vec<String>,
    /// 評価器（単一分岐の `OR` 群。`WHERE` と同じ評価経路）。
    group: BoundOrGroup,
    /// 評価に必要な列のデコードマスク（`row_codec::scan_scalar_columns_masked` へ渡す）。
    column_mask: Vec<bool>,
    /// VECTOR 列（埋め込み）を参照するか。
    references_embedding: bool,
}

impl BoundIndexPredicate {
    /// 述語の唯一の AND 連言（含意判定 `sql::index_implication` の入力）。分岐が 1 個でなければ
    /// `None`（呼び出し側は含意しない扱いにする。束縛は常に単一分岐で作るため防御的な戻り値）。
    /// プランナ選択（`sql::index_implication::implied_partial_index_names`、Issue #1656）が呼ぶ。
    pub(crate) fn conjunction(&self) -> Option<&BoundConjunction> {
        match self.group.branches.as_slice() {
            [only] => Some(only),
            _ => None,
        }
    }

    /// 1 行の述語を評価する。TRUE のときだけ `true`（NULL・FALSE は `false`＝索引外）。評価エラー
    /// （0 除算等）は `Err` で、呼び出し元が文の拒否（DML）・構築の断念（派生キー）へ振り分ける。
    /// `scratch` は呼び出し元が行ループの外で 1 回確保して使い回すスクラッチ。
    pub(crate) fn eval(
        &self,
        schema: &TableSchema,
        id: u64,
        embedding: &[f32],
        metadata: &[u8],
        scratch: &mut Vec<crate::sql::expr_program::StackValue>,
    ) -> Result<bool, PredicateEvalError> {
        let scanned =
            crate::row_codec::scan_scalar_columns_masked(schema, metadata, Some(&self.column_mask))
                .map_err(|e| PredicateEvalError::Decode(e.to_string()))?;
        let emb: &[f32] = if self.references_embedding {
            embedding
        } else {
            &[]
        };
        self.group
            .matches(&scanned, id, emb, emb.len(), scratch)
            .map_err(PredicateEvalError::Eval)
    }
}

/// [`BoundIndexPredicate::eval`] の失敗。
#[derive(Debug)]
pub(crate) enum PredicateEvalError {
    /// 述語の評価エラー（SQLSTATE を透過する）。
    Eval(SqlSurfaceError),
    /// 行のデコード失敗（破損。内部事象として扱う）。
    Decode(String),
}

/// 述語の木が参照する列名を重複なしで集める。`map_predicate_columns` の共有走査器に恒等写像を
/// 渡して副作用で収集する（第 2 の走査器を作らない。`Or`・`Not`・式の内側も走査される）。
fn collect_column_names(preds: &[WherePredicate]) -> Result<Vec<String>, SqlSurfaceError> {
    let seen: RefCell<Vec<String>> = RefCell::new(Vec::new());
    let record = |c: &str| {
        let mut v = seen.borrow_mut();
        if !v.iter().any(|n| n == c) {
            v.push(c.to_string());
        }
        c.to_string()
    };
    for p in preds {
        crate::sql::check_constraint::map_predicate_columns(p, &record, &|| {
            SqlSurfaceError::FeatureNotSupported {
                detail: "cannot use subquery in index predicate".to_string(),
            }
        })?;
    }
    Ok(seen.into_inner())
}

/// 述語の木の `Expression` 葉を `f` で変換した複製を返す（`Or`・`Not` の内側も再帰する）。
fn map_expression_leaves(
    pred: &WherePredicate,
    f: &dyn Fn(&Expr) -> Result<Expr, SqlSurfaceError>,
) -> Result<WherePredicate, SqlSurfaceError> {
    Ok(match pred {
        WherePredicate::Expression(e) => WherePredicate::Expression(f(e)?),
        WherePredicate::Not(inner) => {
            WherePredicate::Not(Box::new(map_expression_leaves(inner, f)?))
        }
        WherePredicate::Or(branches) => WherePredicate::Or(
            branches
                .iter()
                .map(|b| {
                    b.iter()
                        .map(|p| map_expression_leaves(p, f))
                        .collect::<Result<Vec<_>, _>>()
                })
                .collect::<Result<Vec<_>, _>>()?,
        ),
        other => other.clone(),
    })
}

/// 述語の木の全 `Expression` 葉と宣言的な葉を `visit` へ渡す（`Or`・`Not` の内側も再帰する）。
fn visit_leaves(
    pred: &WherePredicate,
    visit: &mut dyn FnMut(&WherePredicate) -> Result<(), SqlSurfaceError>,
) -> Result<(), SqlSurfaceError> {
    match pred {
        WherePredicate::Not(inner) => visit_leaves(inner, visit),
        WherePredicate::Or(branches) => {
            for b in branches {
                for p in b {
                    visit_leaves(p, visit)?;
                }
            }
            Ok(())
        }
        leaf => visit(leaf),
    }
}

fn not_immutable() -> SqlSurfaceError {
    SqlSurfaceError::InvalidObjectDefinition {
        detail: MSG_NOT_IMMUTABLE,
    }
}

/// 日時系の列型か（文字列リテラルとの比較が `DateStyle`・セッション TZ・評価時刻に依存し得る型）。
fn is_datetime_column(ty: &ColumnType) -> bool {
    matches!(
        ty,
        ColumnType::Date | ColumnType::Timestamp | ColumnType::TimestampTz | ColumnType::Time
    )
}

/// 評価時刻に依存する日時の特殊値リテラルか（`'now'`・`'today'`・`'tomorrow'`・`'yesterday'`）。
fn is_volatile_datetime_literal(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "now" | "today" | "tomorrow" | "yesterday"
    )
}

/// 宣言的な葉（`=`・範囲比較・`IN`・`BETWEEN`）の、日時列と文字列リテラルの比較が決定的かを検査する
/// （INDEX-11 の束縛段の再判定。式索引の `tz_vs_string_literal` と同じ基準）。`TIMESTAMPTZ` 列と文字列
/// リテラルの比較は、ゾーン無しリテラルがセッション TZ で解釈されるため一律 `42P17`（PostgreSQL は
/// 定数畳み込みで受理する。fail-closed 側の差）。日時列と評価時刻依存の特殊値も `42P17`。
fn check_datetime_leaf(
    schema: &TableSchema,
    column: &str,
    values: &[&str],
) -> Result<(), SqlSurfaceError> {
    let Some(col) = schema.columns.iter().find(|c| c.name == column) else {
        return Ok(());
    };
    if matches!(col.ty, ColumnType::TimestampTz) {
        return Err(not_immutable());
    }
    if is_datetime_column(&col.ty) && values.iter().any(|v| is_volatile_datetime_literal(v)) {
        return Err(not_immutable());
    }
    Ok(())
}

/// 部分索引の述語を `schema` に対して束縛する（INDEX-11、Issue #1654）。判定順:
/// 1. 構文段と同じ決定性検査（`visible()`・部分クエリ等。保存テキストの再束縛でも再判定する）
/// 2. 予約列（`tenant_id`・`visibility`）の参照は `42601`
/// 3. 定義時セッション TZ でゾーン無し `TIMESTAMPTZ` リテラルを確定値へ畳む
/// 4. `WHERE` と同じ束縛（未知列 `42703`・型不整合 `42804` 等は束縛側のエラーを透過）。RLS 述語
///    呼び出しが残れば `42601`
/// 5. 型に依存する決定性の再判定（`42P17`）
/// 6. 正規化テキストへ描画（長さ上限 `54000`）し、再解析して畳み込み後の木と一致することを確認
///    （不一致は `0A000`）
/// 7. 参照列の抽出
///
/// 保存するのは 6 の正規化テキストだけ。UDF レジストリは空（構文段が組み込み以外の関数を拒否済みで、
/// セッション外で消える関数に依存しない）。
pub(crate) fn bind_index_predicate(
    preds: &[WherePredicate],
    schema: &TableSchema,
) -> Result<BoundIndexPredicate, SqlSurfaceError> {
    crate::sql::index_expr::check_index_predicate_determinism(preds)?;
    let names = collect_column_names(preds)?;
    if names
        .iter()
        .any(|n| n.eq_ignore_ascii_case("tenant_id") || n.eq_ignore_ascii_case("visibility"))
    {
        return Err(SqlSurfaceError::unsupported(MSG_RESERVED));
    }
    let folded: Vec<WherePredicate> = preds
        .iter()
        .map(|p| map_expression_leaves(p, &crate::sql::index_expr::fold_session_local_literals))
        .collect::<Result<_, _>>()?;

    let udfs = crate::sql::udf_call::UdfRegistry::default();
    let mut budget = crate::sql::udf_call::MAX_EXPR_NODES;
    let (metadata_filters, expr_filters, rls_present, or_filters) =
        crate::sql::parser::bind_where_predicates(&folded, schema, &udfs, &mut budget, &[])?;
    if rls_present {
        return Err(SqlSurfaceError::unsupported(
            "visible() is not allowed in an index predicate",
        ));
    }
    for p in &folded {
        visit_leaves(p, &mut |leaf| match leaf {
            WherePredicate::Expression(e) => {
                crate::sql::index_expr::check_bound_index_expression_determinism(e, schema)
                    // 式索引の文言（`index expression`）を述語の文言へ差し替える（SQLSTATE は同じ `42P17`）。
                    .map_err(|err| match err {
                        SqlSurfaceError::InvalidObjectDefinition { .. } => not_immutable(),
                        other => other,
                    })
            }
            WherePredicate::Equality { column, value } => {
                check_datetime_leaf(schema, column, &[value.as_str()])
            }
            WherePredicate::Compare { column, value, .. } => {
                check_datetime_leaf(schema, column, &[value.as_str()])
            }
            WherePredicate::InList { column, values } => {
                let vs: Vec<&str> = values.iter().map(String::as_str).collect();
                check_datetime_leaf(schema, column, &vs)
            }
            WherePredicate::Between { column, low, high } => {
                check_datetime_leaf(schema, column, &[low.as_str(), high.as_str()])
            }
            _ => Ok(()),
        })?;
    }

    let sql = crate::sql::check_constraint::render_predicates(&folded);
    if sql.len() > MAX_PREDICATE_SQL_LEN {
        return Err(SqlSurfaceError::payload_too_large(MSG_TOO_LONG));
    }
    let not_storable = || SqlSurfaceError::FeatureNotSupported {
        detail: MSG_NOT_STORABLE.to_string(),
    };
    let reparsed =
        crate::sql::allowlist::parse_index_predicate_text(&sql).map_err(|_| not_storable())?;
    if reparsed != folded {
        return Err(not_storable());
    }

    // 参照列。走査器が集めた名前のうち、スキーマ列に一致するものだけを宣言順で残す
    // （VECTOR 列への参照も識別子として含まれる）。疑似列 `id` はスキーマ列でないため入らない。
    let collected = collect_column_names(&folded)?;
    let referenced_columns: Vec<String> = schema
        .columns
        .iter()
        .filter(|c| collected.iter().any(|n| n == &c.name))
        .map(|c| c.name.clone())
        .collect();

    let group = BoundOrGroup::new(vec![BoundConjunction::new(
        metadata_filters,
        expr_filters,
        or_filters,
    )]);
    let mut column_mask = vec![false; schema.columns.len()];
    group.visit_column_indices(&mut |i| {
        if let Some(slot) = column_mask.get_mut(i) {
            *slot = true;
        }
    });
    let references_embedding = group.references_embedding();
    Ok(BoundIndexPredicate {
        sql,
        referenced_columns,
        group,
        column_mask,
        references_embedding,
    })
}

/// 保存済みの述語テキストを `schema` で再束縛する。`RENAME COLUMN` の追随（`rename` が
/// `Some((旧名, 新名))`。AST 上の識別子写像で、文字列置換は使わない。1 回の走査で全識別子へ一括適用
/// されるため列名の入れ替えでも連鎖置換が起きない）と、DML ゲート・派生キー構築の再束縛
/// （`rename` が `None`）が共有する唯一の経路。保存テキストは untrusted として再解析し、
/// [`bind_index_predicate`] が往復一致まで確認する。
pub(crate) fn rebind_stored_index_predicate(
    stored_sql: &str,
    schema: &TableSchema,
    rename: Option<(&str, &str)>,
) -> Result<BoundIndexPredicate, SqlSurfaceError> {
    let parsed = crate::sql::allowlist::parse_index_predicate_text(stored_sql)?;
    let preds = match rename {
        Some((old, new)) => {
            let map = |c: &str| {
                if c == old {
                    new.to_string()
                } else {
                    c.to_string()
                }
            };
            parsed
                .iter()
                .map(|p| {
                    crate::sql::check_constraint::map_predicate_columns(p, &map, &|| {
                        SqlSurfaceError::FeatureNotSupported {
                            detail: "cannot use subquery in index predicate".to_string(),
                        }
                    })
                })
                .collect::<Result<Vec<_>, _>>()?
        }
        None => parsed,
    };
    bind_index_predicate(&preds, schema)
}

/// 保存済みの述語・式要素の漂流（再解析・再束縛・往復一致の失敗）を fail-closed の `XX000` へ写像する。
fn drifted() -> TenantWriteError {
    TenantWriteError::Catalog(crate::catalog::CatalogError::CorruptSchema(
        "stored index predicate no longer binds".to_string(),
    ))
}

/// 述語の評価失敗を DML ゲート用のエラーへ写像する。評価エラーは SQLSTATE を保ったまま透過し、
/// 行のデコード失敗は破損として `XX000` にする。
fn eval_error(e: PredicateEvalError) -> TenantWriteError {
    match e {
        PredicateEvalError::Eval(inner) => TenantWriteError::IndexExpressionEvaluationFailed(inner),
        PredicateEvalError::Decode(msg) => {
            TenantWriteError::Catalog(crate::catalog::CatalogError::CorruptSchema(msg))
        }
    }
}

/// 1 本の部分索引をコンパイルした DML ゲート用の評価器。
struct CompiledPartial {
    predicate: BoundIndexPredicate,
    /// 式要素（0 個でもよい）。述語が TRUE の行でだけ評価する。
    elements: Option<crate::sql::expression_key_index::CompiledIndexExpressions>,
}

/// テーブルの部分索引群を write txn 内でコンパイルした DML ゲート用の評価器（INDEX-11、
/// Issue #1654）。1 文につき 1 回だけコンパイルし、`written_ids` の各行で [`Self::enforce`] を呼ぶ。
/// `sql::check_constraint::CompiledChecks`・`sql::expression_key_index::CompiledIndexExpressions` と同型。
pub(crate) struct CompiledPartialIndexes {
    partials: Vec<CompiledPartial>,
}

impl CompiledPartialIndexes {
    /// `decls` をコンパイルする。宣言が空なら `None`（ゲート不要）。保存テキストは untrusted で、
    /// 再解析・再束縛を通り往復で同じテキストに戻らなければ `CorruptSchema`（`XX000`）で拒否する。
    pub(crate) fn compile(
        schema: &TableSchema,
        decls: &[DeclaredPartialIndex],
    ) -> Result<Option<Self>, TenantWriteError> {
        if decls.is_empty() {
            return Ok(None);
        }
        let mut partials: Vec<CompiledPartial> = Vec::new();
        partials
            .try_reserve_exact(decls.len())
            .map_err(|_| drifted())?;
        for decl in decls {
            let predicate = rebind_stored_index_predicate(&decl.predicate_sql, schema, None)
                .map_err(|_| drifted())?;
            if predicate.sql != decl.predicate_sql {
                return Err(drifted());
            }
            let keys: Vec<DeclaredExpressionKey> = decl.expression_keys();
            let elements =
                crate::sql::expression_key_index::CompiledIndexExpressions::compile(schema, &keys)?;
            partials.push(CompiledPartial {
                predicate,
                elements,
            });
        }
        Ok(Some(Self { partials }))
    }

    /// 1 行（`id`・`embedding`・`metadata`）で全部分索引を検査する。述語の評価エラーは文の拒否
    /// （[`TenantWriteError::IndexExpressionEvaluationFailed`] で SQLSTATE を透過）。述語が TRUE の行では
    /// 式要素も評価でき索引化できることを確かめる。NULL・FALSE の行は索引外で何も評価しない
    /// （UPDATE で述語を外れる行の入退場を過剰に拒否しない）。
    pub(crate) fn enforce(
        &self,
        schema: &TableSchema,
        id: u64,
        embedding: &[f32],
        metadata: &[u8],
    ) -> Result<(), TenantWriteError> {
        let mut scratch = Vec::new();
        for partial in &self.partials {
            let member = partial
                .predicate
                .eval(schema, id, embedding, metadata, &mut scratch)
                .map_err(eval_error)?;
            if member {
                if let Some(elements) = &partial.elements {
                    elements.enforce(schema, id, embedding, metadata)?;
                }
            }
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ColumnDef;
    use crate::row_codec::Value;

    fn schema() -> TableSchema {
        TableSchema::new(
            "t",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), true),
                ColumnDef::new("n", ColumnType::Integer, true),
                ColumnDef::new("code", ColumnType::Text, true),
                ColumnDef::new("flag", ColumnType::Boolean, true),
                ColumnDef::new("at_tz", ColumnType::TimestampTz, true),
            ],
        )
    }

    fn bind(text: &str) -> Result<BoundIndexPredicate, SqlSurfaceError> {
        let preds = crate::sql::allowlist::parse_index_predicate_text(text)?;
        bind_index_predicate(&preds, &schema())
    }

    fn code(text: &str) -> String {
        use crate::error_format::ClassifiedError;
        bind(text)
            .err()
            .map(|e| e.error_class().wire_code().to_string())
            .unwrap_or_default()
    }

    fn row(n: Option<i32>, code: Option<&str>, flag: Option<bool>) -> Vec<u8> {
        crate::row_codec::encode_scalar_columns(
            &schema(),
            &[
                Value::Vector(vec![0.0, 0.0]),
                n.map_or(Value::Null, Value::Integer),
                code.map_or(Value::Null, |c| Value::Text(c.to_string())),
                flag.map_or(Value::Null, Value::Bool),
                Value::Null,
            ],
        )
        .expect("encode")
    }

    fn eval(p: &BoundIndexPredicate, id: u64, metadata: &[u8]) -> Result<bool, PredicateEvalError> {
        p.eval(&schema(), id, &[], metadata, &mut Vec::new())
    }

    /// 全 variant の描画 → 再解析 → 再束縛が往復一致し、保存テキストの再束縛が同じテキストに戻る
    /// （`render_predicate` のうち CHECK が構築前に拒否するため往復が未検証だった variant を含む）。
    #[test]
    fn every_predicate_variant_round_trips_through_the_stored_text() {
        for text in [
            "code = 'ja'",
            "code LIKE 'ja%'",
            "flag",
            "flag = true",
            "n > 3",
            "code >= 'a'",
            "code IN ('a', 'b')",
            "n BETWEEN 1 AND 5",
            "code IS NULL",
            "code IS NOT NULL",
            "NOT code = 'x'",
            "NOT (n > 1 AND code = 'x')",
            "code = 'a' OR code = 'b' AND n > 1",
            "(code = 'a' OR code = 'b') AND n > 1",
            "NOT (code = 'a' OR n < 0)",
            "lower(code) = 'ja'",
            "n + 1 > 3 AND code IS NOT NULL",
            "id > 3",
            "id BETWEEN 1 AND 5",
            "code = 'it''s'",
            "100 / (n + 5) > 1",
        ] {
            let first = bind(text).unwrap_or_else(|e| panic!("{text}: {e:?}"));
            let again = rebind_stored_index_predicate(&first.sql, &schema(), None)
                .unwrap_or_else(|e| panic!("{text} -> {}: {e:?}", first.sql));
            assert_eq!(first.sql, again.sql, "{text}");
            assert_eq!(first.referenced_columns, again.referenced_columns, "{text}");
        }
    }

    #[test]
    fn referenced_columns_follow_schema_order_and_skip_pseudo_columns() {
        let p = bind("code = 'a' AND id > 1 OR n < 5").expect("bind");
        assert_eq!(
            p.referenced_columns,
            vec!["n".to_string(), "code".to_string()]
        );
        assert!(bind("id > 1").expect("bind").referenced_columns.is_empty());
    }

    #[test]
    fn column_names_are_matched_exactly_by_the_binder() {
        // 束縛は列名を大文字小文字を区別して解決するため、スキーマ名と異なる綴りは `42703`。
        // `referenced_columns` の抽出（完全一致）は束縛が通った名前と常に一致する。
        assert_eq!(code("N > 3"), "42703");
        assert_eq!(code("CODE = 'a'"), "42703");
        let p = bind("\"n\" > 3").expect("quoted identifier");
        assert_eq!(p.referenced_columns, vec!["n".to_string()]);
    }

    #[test]
    fn binding_classifies_errors() {
        assert_eq!(code("nope = 'x'"), "42703");
        assert_eq!(code("tenant_id = 'a'"), "42601");
        assert_eq!(code("visibility = 'public'"), "42601");
        assert_eq!(code("visible()"), "42601");
        assert_eq!(code("at_tz > '2020-01-01 00:00:00'"), "42P17");
        assert_eq!(code("CAST(at_tz AS TEXT) = 'x'"), "42P17");
        assert_eq!(code("now() > 1"), "42P17");
        assert_eq!(code("lower(code) = 'a'"), "");
    }

    /// 述語の三値論理: TRUE のときだけ索引に入る（NULL・FALSE は索引外）。評価エラーは `Err`。
    #[test]
    fn evaluation_admits_only_true_rows() {
        let gt = bind("n > 3").expect("bind");
        assert!(eval(&gt, 1, &row(Some(5), None, None)).expect("eval"));
        assert!(!eval(&gt, 1, &row(Some(1), None, None)).expect("eval"));
        assert!(
            !eval(&gt, 1, &row(None, None, None)).expect("eval"),
            "NULL is not admitted"
        );
        let or = bind("code = 'a' OR n < 0").expect("bind");
        assert!(eval(&or, 1, &row(Some(1), Some("a"), None)).expect("eval"));
        assert!(eval(&or, 1, &row(Some(-1), Some("b"), None)).expect("eval"));
        assert!(!eval(&or, 1, &row(Some(1), Some("b"), None)).expect("eval"));
        let not = bind("NOT code = 'a'").expect("bind");
        assert!(eval(&not, 1, &row(None, Some("b"), None)).expect("eval"));
        assert!(
            !eval(&not, 1, &row(None, None, None)).expect("eval"),
            "NOT NULL stays NULL"
        );
        let by_id = bind("id > 3").expect("bind");
        assert!(eval(&by_id, 9, &row(None, None, None)).expect("eval"));
        assert!(!eval(&by_id, 2, &row(None, None, None)).expect("eval"));
        let div = bind("100 / (n + 5) > 1").expect("bind");
        assert!(matches!(
            eval(&div, 1, &row(Some(-5), None, None)),
            Err(PredicateEvalError::Eval(_))
        ));
    }

    /// DML ゲート: 述語が NULL・FALSE の行では式要素を評価しない。TRUE の行では式要素の評価エラーが出る。
    #[test]
    fn compiled_gate_evaluates_elements_only_inside_the_predicate() {
        use crate::catalog::PartialElementDecl;
        let expr_sql = bind_index_expression_text("100 / n");
        let decl = DeclaredPartialIndex {
            index_name: "p".to_string(),
            predicate_sql: bind("n <> 0").expect("bind").sql,
            elements: vec![PartialElementDecl::Expr(expr_sql)],
        };
        let gate = CompiledPartialIndexes::compile(&schema(), &[decl])
            .expect("compile")
            .expect("gate");
        assert!(gate
            .enforce(&schema(), 1, &[], &row(Some(5), None, None))
            .is_ok());
        assert!(
            gate.enforce(&schema(), 1, &[], &row(Some(0), None, None))
                .is_ok(),
            "n = 0 is outside the predicate, so 100 / n is never evaluated"
        );
        assert!(gate
            .enforce(&schema(), 1, &[], &row(None, None, None))
            .is_ok());
        // 述語を `n >= 0` にすると n = 0 が述語内へ入り、式要素が 22012 になる。
        let decl = DeclaredPartialIndex {
            index_name: "q".to_string(),
            predicate_sql: bind("n >= 0").expect("bind").sql,
            elements: vec![PartialElementDecl::Expr(bind_index_expression_text(
                "100 / n",
            ))],
        };
        let gate = CompiledPartialIndexes::compile(&schema(), &[decl])
            .expect("compile")
            .expect("gate");
        let err = gate
            .enforce(&schema(), 1, &[], &row(Some(0), None, None))
            .expect_err("element error inside the predicate");
        assert!(matches!(
            err,
            TenantWriteError::IndexExpressionEvaluationFailed(_)
        ));
    }

    fn bind_index_expression_text(text: &str) -> String {
        let expr = crate::sql::allowlist::parse_index_expression_text(text).expect("parse");
        crate::sql::index_expr::bind_index_expression(&expr, &schema())
            .expect("bind")
            .sql
    }

    /// 保存テキストの漂流（手書き改変・列の消失）は fail-closed の `CorruptSchema`。
    #[test]
    fn drifted_stored_text_is_rejected_fail_closed() {
        for sql in ["nope = 'x'", "n >", "n > 3 -- x", "n  >  3"] {
            let decl = DeclaredPartialIndex {
                index_name: "p".to_string(),
                predicate_sql: sql.to_string(),
                elements: vec![],
            };
            let err = CompiledPartialIndexes::compile(&schema(), &[decl])
                .err()
                .unwrap_or_else(|| panic!("{sql} must be rejected"));
            assert!(matches!(
                err,
                TenantWriteError::Catalog(crate::catalog::CatalogError::CorruptSchema(_))
            ));
        }
    }

    #[test]
    fn rename_rewrites_identifiers_only_and_handles_swaps() {
        let p =
            rebind_stored_index_predicate("n > 3 AND code = 'n'", &schema(), None).expect("bind");
        let renamed_schema = TableSchema::new(
            "t",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), true),
                ColumnDef::new("m", ColumnType::Integer, true),
                ColumnDef::new("code", ColumnType::Text, true),
                ColumnDef::new("flag", ColumnType::Boolean, true),
                ColumnDef::new("at_tz", ColumnType::TimestampTz, true),
            ],
        );
        let r = rebind_stored_index_predicate(&p.sql, &renamed_schema, Some(("n", "m")))
            .expect("rename");
        assert_eq!(
            r.sql, "m > 3 AND code = 'n'",
            "string literal 'n' is untouched"
        );
        assert_eq!(
            r.referenced_columns,
            vec!["m".to_string(), "code".to_string()]
        );
    }

    #[test]
    fn predicate_text_length_is_bounded() {
        let long = (0..400)
            .map(|i| format!("code = 'value-number-{i}'"))
            .collect::<Vec<_>>()
            .join(" AND ");
        assert!(bind(&long).is_err());
    }
}
