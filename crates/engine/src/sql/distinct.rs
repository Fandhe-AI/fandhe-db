//! `SELECT DISTINCT`・`COUNT(DISTINCT <expr>)`（SQL-25 (c)・TASK-209）が共有する
//! 正準キー化・予算管理。呼び出し元は 2 経路:
//!
//! - `sql::allowlist::validate_sql_tokens` が `SELECT DISTINCT` を
//!   `sql::group_by`（既存の `GROUP BY` 実行器）へ脱糖する経路（`GROUP BY` と
//!   同じ [`crate::sql::group_by::ResultBudget`] を継承するため、本モジュールの
//!   予算は対象外）。
//! - `sql::aggregate`／`sql::group_by` の `Accumulator::CountDistinct` が
//!   `COUNT(DISTINCT <expr>)` の中間状態（クエリ全体で 1 つ）として使う
//!   [`DistinctBudget`]。
//!
//! キーの数値正準化（`-0.0` を `+0.0` へ、NaN を単一表現へ正規化）は
//! [`crate::constraint::push_canonical_component`]（UNIQUE／FOREIGN KEY の
//! 正準キー）と役割は異なる。1 つの `CountDistinct` アキュムレータは束縛時に
//! 決まった単一の入力型（`AggregateInput`）しか観測しないため、
//! `push_canonical_component` のような「複数コンポーネントを連結する際の境界
//! 曖昧性」は生じず（`sql::aggregate::Accumulator::observe_distinct` は型タグ・
//! 長さ接頭辞を付けない生の正準バイト列をそのままキーにする）、本モジュールは
//! その正準化プリミティブ（`canon_f64`）と予算管理（[`DistinctBudget`]）のみを
//! 提供する。
//!
//! `SELECT DISTINCT ON (...)`（Issue #3260。SQL-25 のポインタ）の構文判定
//! （[`is_distinct_on_head`]）と `ORDER BY` 先頭一致の検証
//! （[`check_distinct_on_order_prefix`]）もここに置く。呼び出し元は
//! `sql::allowlist::validate_select_statement` の DISTINCT ON gate だけで、行を返す実行段は
//! 別 Issue（#3261）の責務。

use crate::sql::allowlist::{
    Projection, ScalarOrderKey, ScanOrderKey, SelectItem, SqlSurfaceError,
};
use crate::sql::lexer::Token;
use crate::sql::udf_call::Expr;

/// `COUNT(DISTINCT)` の中間状態がクエリ全体で保持してよい異なり値の件数上限
/// （実装既定値）。`SELECT DISTINCT` の結果行数そのものは、既存の `GROUP BY`
/// 予算（`sql::group_by::MAX_GROUPS`）をそのまま継承するため対象外
/// （出力行数と中間状態のエントリ数は別の量であるため上限値も異なる）。
pub(crate) const MAX_DISTINCT_VALUES: usize = 100_000;

/// `COUNT(DISTINCT)` の中間状態が保持するキーの累計バイト数上限
/// （エントリごとの固定オーバーヘッド見積り込み）。`TEXT` 列は 1 値あたり
/// 最大 [`crate::row_codec::MAX_TEXT_FIELD_LEN`] に達しうるため、件数上限
/// だけでは有界にならない。
pub(crate) const MAX_DISTINCT_TOTAL_BYTES: usize = 16 * 1024 * 1024;

/// 1 エントリあたりの固定オーバーヘッド見積り（`HashSet<Vec<u8>>` のバケット・
/// `Vec` 自体のヒープ管理コストの概算）。
const DISTINCT_ENTRY_OVERHEAD_BYTES: usize = 32;

/// [`DistinctBudget::charge`] の件数超過 detail 文言。`sql::aggregate::
/// is_aggregate_text_budget_error`／`sql::group_by::is_text_accumulator_budget_error`
/// が照合する文言とは重ならない別文字列にする（processing 順序に依存しない
/// 単調増加の集合であり、索引経路と全走査で成否が変わってはならないため。
/// 全走査へのフォールバック対象にしない）。
const DISTINCT_CARDINALITY_EXCEEDED_DETAIL: &str =
    "COUNT(DISTINCT) cardinality exceeds the allowed limit";
/// 同上。累計バイト数超過。
const DISTINCT_BYTES_EXCEEDED_DETAIL: &str =
    "COUNT(DISTINCT) accumulated key size exceeds the allowed limit";
/// 同上。`checked_add` のオーバーフロー側。
const DISTINCT_BUDGET_OVERFLOW_DETAIL: &str = "COUNT(DISTINCT) budget accounting overflowed";

/// `COUNT(DISTINCT <expr>)` の中間状態（クエリ全体で 1 つ。`GROUP BY` を伴う
/// 場合は全グループ・全項目の合計を管理する）。新規キー 1 件の挿入が確定した
/// 時点でのみ [`Self::charge`] を呼ぶ（既存キーへの再ヒットでは呼ばない）。
#[derive(Debug)]
pub(crate) struct DistinctBudget {
    entries: usize,
    bytes: usize,
}

impl DistinctBudget {
    pub(crate) fn new() -> Self {
        Self {
            entries: 0,
            bytes: 0,
        }
    }

    /// 新規キー 1 件（バイト長 `key_len`）を計上し、上限超過なら `54000` で
    /// fail-closed に拒否する。`checked_*` を使い、加算のオーバーフローも
    /// `54000` として扱う（`.claude/rules/coding-rust.md`「整数演算は
    /// `checked_*`／`saturating_*` を使う」）。
    pub(crate) fn charge(&mut self, key_len: usize) -> Result<(), SqlSurfaceError> {
        let entries = self
            .entries
            .checked_add(1)
            .ok_or_else(|| SqlSurfaceError::payload_too_large(DISTINCT_BUDGET_OVERFLOW_DETAIL))?;
        let added = key_len
            .checked_add(DISTINCT_ENTRY_OVERHEAD_BYTES)
            .ok_or_else(|| SqlSurfaceError::payload_too_large(DISTINCT_BUDGET_OVERFLOW_DETAIL))?;
        let bytes = self
            .bytes
            .checked_add(added)
            .ok_or_else(|| SqlSurfaceError::payload_too_large(DISTINCT_BUDGET_OVERFLOW_DETAIL))?;
        if entries > MAX_DISTINCT_VALUES {
            return Err(SqlSurfaceError::payload_too_large(
                DISTINCT_CARDINALITY_EXCEEDED_DETAIL,
            ));
        }
        if bytes > MAX_DISTINCT_TOTAL_BYTES {
            return Err(SqlSurfaceError::payload_too_large(
                DISTINCT_BYTES_EXCEEDED_DETAIL,
            ));
        }
        self.entries = entries;
        self.bytes = bytes;
        Ok(())
    }
}

/// `f64` の正準化。`-0.0` を `+0.0` へ、NaN をすべて単一の正準表現へ正規化する
/// （PostgreSQL の `DISTINCT`／`GROUP BY` は NaN 同士・±0 同士をそれぞれ同一視
/// する契約に合わせる）。
pub(crate) fn canon_f64(v: f64) -> [u8; 8] {
    let canon = if v.is_nan() {
        f64::NAN
    } else if v == 0.0 {
        0.0_f64
    } else {
        v
    };
    canon.to_bits().to_be_bytes()
}

/// `tokens[pos..]` が `DISTINCT` `ON` `(` の並びか（`DISTINCT`・`ON` は字句上いずれも文脈識別子）。
/// 列名 `on` を `DISTINCT` 修飾子の直後に置く形（`SELECT DISTINCT on FROM t`）は 3 番目が `(` で
/// ないため偽になり、従来の `SELECT DISTINCT` 経路へ進む。
pub(crate) fn is_distinct_on_head(tokens: &[Token], pos: usize) -> bool {
    let word = |i: usize, w: &str| matches!(tokens.get(i), Some(Token::Ident(n)) if n.eq_ignore_ascii_case(w));
    word(pos, "DISTINCT")
        && word(pos.saturating_add(1), "ON")
        && matches!(tokens.get(pos.saturating_add(2)), Some(Token::Punct('(')))
}

/// 出力別名・序数を解決した比較用の正準キーへ写す。裸の識別子が SELECT リストの別名に一意に
/// 一致すればその項目の式へ、整数リテラル（序数）は該当項目の式へ置き換える。曖昧・位置が確定
/// しない・範囲外は推測せず `0A000`（fail-closed）。それ以外は式をそのまま使う。
fn canonical_key(expr: &Expr, projection: &Projection) -> Result<Expr, SqlSurfaceError> {
    match expr {
        Expr::Ident(name) => resolve_alias(name, projection),
        Expr::Number(n) => match n.parse::<i64>() {
            Ok(position) => resolve_ordinal(position, projection),
            Err(_) => Ok(expr.clone()),
        },
        other => Ok(other.clone()),
    }
}

fn resolve_alias(name: &str, projection: &Projection) -> Result<Expr, SqlSurfaceError> {
    let Projection::Items(items) = projection else {
        return Ok(Expr::Ident(name.to_string()));
    };
    let mut found: Option<Expr> = None;
    for item in items {
        // 明示別名がなければ PostgreSQL と同じく暗黙の出力名（通常列は列名・関数呼び出しは関数名）を使う。
        let (expr, matches) = match item {
            SelectItem::Expr {
                expr,
                alias: Some(alias),
            } => (expr.clone(), alias == name),
            SelectItem::Expr { expr, alias: None } => match expr {
                Expr::Call { name: f, .. } => (expr.clone(), f.eq_ignore_ascii_case(name)),
                _ => continue,
            },
            // 通常列の暗黙の出力名は列名自身。明示別名との同名衝突を曖昧として検出するため候補に含める。
            SelectItem::Column(c) if c != "*" => (Expr::Ident(c.clone()), c == name),
            _ => continue,
        };
        if !matches {
            continue;
        }
        match &found {
            Some(prev) if *prev != expr => {
                return Err(SqlSurfaceError::feature_not_supported(
                    "SELECT DISTINCT ON key is ambiguous among output column names",
                ));
            }
            _ => found = Some(expr),
        }
    }
    Ok(found.unwrap_or_else(|| Expr::Ident(name.to_string())))
}

fn resolve_ordinal(position: i64, projection: &Projection) -> Result<Expr, SqlSurfaceError> {
    let unresolved = || {
        SqlSurfaceError::feature_not_supported(
            "SELECT DISTINCT ON position cannot be resolved to a select list item",
        )
    };
    let idx = usize::try_from(position)
        .ok()
        .and_then(|p| p.checked_sub(1))
        .ok_or_else(unresolved)?;
    match projection {
        Projection::All => Err(unresolved()),
        Projection::Columns(cols) => cols
            .get(idx)
            .map(|c| Expr::Ident(c.clone()))
            .ok_or_else(unresolved),
        Projection::Items(items) => {
            // `*` を含む項目より後ろは展開後の位置が確定しないため解決しない。
            let upto = items.get(..=idx).ok_or_else(unresolved)?;
            if upto
                .iter()
                .any(|i| matches!(i, SelectItem::Column(c) if c == "*"))
            {
                return Err(unresolved());
            }
            match items.get(idx) {
                Some(SelectItem::Column(c)) => Ok(Expr::Ident(c.clone())),
                Some(SelectItem::Expr { expr, .. }) => Ok(expr.clone()),
                None => Err(unresolved()),
            }
        }
    }
}

/// `DISTINCT ON` の式列が `ORDER BY` の先頭と一致するかを検証する（PostgreSQL の規則。ON 集合に
/// 含まれない `ORDER BY` キーを読み飛ばした後に ON 集合のキーが現れる、または ON 集合のキーが
/// 結果列に入らないまま読み飛ばしが起きていれば `42P10`）。`ASC`／`DESC`・`NULLS` は一致に影響
/// しない。`ORDER BY` が空なら整合（決定性の扱いは別 Issue #3262）。判定は文だけで決まり、
/// 行・テナントに依存しない。
pub(crate) fn check_distinct_on_order_prefix(
    on_exprs: &[Expr],
    projection: &Projection,
    order_by: &[ScalarOrderKey],
    order_keys: &[ScanOrderKey],
) -> Result<(), SqlSurfaceError> {
    let on_keys = on_exprs
        .iter()
        .map(|e| canonical_key(e, projection))
        .collect::<Result<Vec<_>, _>>()?;
    let mut sort_keys: Vec<Expr> = Vec::new();
    if order_keys.is_empty() {
        for k in order_by {
            sort_keys.push(resolve_alias(&k.column, projection)?);
        }
    } else {
        for k in order_keys {
            sort_keys.push(match k {
                ScanOrderKey::Column { key, .. } => resolve_alias(&key.column, projection)?,
                ScanOrderKey::Expr { expr, .. } => expr.clone(),
                ScanOrderKey::Ordinal { position, .. } => resolve_ordinal(*position, projection)?,
            });
        }
    }
    // 重複キーは PostgreSQL と同様に除く（既出キーを不一致判定へ再投入しない）。
    let mut unique_keys: Vec<Expr> = Vec::new();
    for k in sort_keys {
        if !unique_keys.contains(&k) {
            unique_keys.push(k);
        }
    }
    let sort_keys = unique_keys;
    let mismatch = SqlSurfaceError::DistinctOnOrderByMismatch;
    let mut covered: Vec<&Expr> = Vec::new();
    let mut skipped = false;
    for k in &sort_keys {
        if on_keys.contains(k) {
            if skipped {
                return Err(mismatch);
            }
            covered.push(k);
        } else {
            skipped = true;
        }
    }
    for k in &on_keys {
        if !covered.contains(&k) {
            if skipped {
                return Err(mismatch);
            }
            covered.push(k);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canon_f64_normalizes_negative_zero() {
        assert_eq!(canon_f64(0.0), canon_f64(-0.0));
    }

    #[test]
    fn canon_f64_normalizes_nan_payloads() {
        let a = f64::from_bits(0x7ff8000000000001);
        let b = f64::from_bits(0x7ff8000000000002);
        assert!(a.is_nan() && b.is_nan());
        assert_eq!(canon_f64(a), canon_f64(b));
    }

    #[test]
    fn canon_f64_keeps_distinct_finite_values_distinct() {
        assert_ne!(canon_f64(1.0), canon_f64(2.0));
    }

    #[test]
    fn distinct_budget_rejects_cardinality_overflow() {
        let mut budget = DistinctBudget::new();
        for _ in 0..MAX_DISTINCT_VALUES {
            budget.charge(4).expect("within limit");
        }
        let err = budget.charge(4).expect_err("must exceed cardinality limit");
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn distinct_budget_rejects_byte_overflow() {
        let mut budget = DistinctBudget::new();
        let big = MAX_DISTINCT_TOTAL_BYTES;
        let err = budget.charge(big).expect_err("must exceed byte limit");
        assert_eq!(err.wire_code(), "54000");
    }

    fn id(n: &str) -> Expr {
        Expr::Ident(n.to_string())
    }

    fn sk(n: &str) -> ScalarOrderKey {
        ScalarOrderKey {
            column: n.to_string(),
            descending: false,
        }
    }

    fn check(on: &[&str], order: &[&str]) -> Result<(), SqlSurfaceError> {
        let on: Vec<Expr> = on.iter().map(|n| id(n)).collect();
        let order: Vec<ScalarOrderKey> = order.iter().map(|n| sk(n)).collect();
        check_distinct_on_order_prefix(&on, &Projection::All, &order, &[])
    }

    #[test]
    fn distinct_on_prefix_accepts_matching_orders() {
        assert!(check(&["a", "b"], &["b", "a"]).is_ok());
        assert!(check(&["a", "b"], &["a"]).is_ok());
        assert!(check(&["a"], &["a", "b"]).is_ok());
        assert!(check(&["a"], &[]).is_ok());
    }

    #[test]
    fn distinct_on_prefix_rejects_mismatched_orders_with_42p10() {
        for (on, order) in [
            (vec!["a"], vec!["b", "a"]),
            (vec!["a", "b"], vec!["a", "c"]),
            (vec!["a"], vec!["b"]),
        ] {
            let err = check(&on, &order).expect_err("must mismatch");
            assert_eq!(err.wire_code(), "42P10");
        }
    }

    #[test]
    fn distinct_on_prefix_resolves_alias_and_ordinal() {
        let proj = Projection::Items(vec![SelectItem::Expr {
            expr: id("lang"),
            alias: Some("l".to_string()),
        }]);
        let on = [id("l")];
        assert!(
            check_distinct_on_order_prefix(&on, &proj, &[sk("lang")], &[]).is_ok(),
            "alias in ON and column in ORDER BY must match"
        );
        let on = [Expr::Number("1".to_string())];
        assert!(check_distinct_on_order_prefix(&on, &proj, &[sk("lang")], &[]).is_ok());
        let on = [Expr::Number("2".to_string())];
        let err = check_distinct_on_order_prefix(&on, &proj, &[sk("lang")], &[])
            .expect_err("out of range ordinal is not guessed");
        assert_eq!(err.wire_code(), "0A000");
    }

    #[test]
    fn distinct_on_prefix_ignores_duplicate_order_keys() {
        // ORDER BY lang, id, lang は重複キーを除くと lang, id で先頭一致（PostgreSQL 準拠）。
        assert!(check(&["lang"], &["lang", "id", "lang"]).is_ok());
    }

    #[test]
    fn distinct_on_prefix_rejects_column_and_alias_name_collision() {
        // SELECT DISTINCT ON (lang) lang, id AS lang ... ORDER BY body
        let proj = Projection::Items(vec![
            SelectItem::Column("lang".to_string()),
            SelectItem::Expr {
                expr: id("id"),
                alias: Some("lang".to_string()),
            },
        ]);
        let err = check_distinct_on_order_prefix(&[id("lang")], &proj, &[sk("body")], &[])
            .expect_err("same output name for different exprs is ambiguous");
        assert_eq!(err.wire_code(), "0A000");
        // 同一式の重複は曖昧にならない。
        let proj = Projection::Items(vec![
            SelectItem::Column("lang".to_string()),
            SelectItem::Expr {
                expr: id("lang"),
                alias: Some("lang".to_string()),
            },
        ]);
        assert!(check_distinct_on_order_prefix(&[id("lang")], &proj, &[sk("lang")], &[]).is_ok());
    }

    #[test]
    fn distinct_on_prefix_resolves_implicit_output_name() {
        let call = Expr::Call {
            name: "lower".to_string(),
            args: vec![id("lang")],
        };
        let proj = Projection::Items(vec![SelectItem::Expr {
            expr: call.clone(),
            alias: None,
        }]);
        // DISTINCT ON (lower) ... ORDER BY lower(lang)
        let on = [id("lower")];
        let order = [ScanOrderKey::Expr {
            expr: call.clone(),
            descending: false,
            nulls: None,
        }];
        assert!(check_distinct_on_order_prefix(&on, &proj, &[], &order).is_ok());
        // DISTINCT ON (lower(lang)) ... ORDER BY lower
        let on = [call];
        assert!(check_distinct_on_order_prefix(&on, &proj, &[sk("lower")], &[]).is_ok());
    }

    #[test]
    fn is_distinct_on_head_requires_paren() {
        let t = |sql: &str| crate::sql::lexer::tokenize(sql).expect("tokenize");
        assert!(is_distinct_on_head(
            &t("SELECT DISTINCT ON (a) a FROM t"),
            1
        ));
        assert!(!is_distinct_on_head(&t("SELECT DISTINCT on FROM t"), 1));
        assert!(!is_distinct_on_head(&t("SELECT DISTINCT on, a FROM t"), 1));
    }
}
