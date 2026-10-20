//! 宣言 PRIMARY KEY の等価述語を永続一意索引の点検索として計画・実行する（Issue #2198）。
//!
//! ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-23・TABLE-16、`rls.md` RLS-9・RLS-10、
//! `sql-surface.md` SQL-27（EXPLAIN の語彙）。設計記録は `docs/design/primary-key-point-lookup.md`。
//!
//! # 責務境界
//!
//! - **静的分類**（[`classify_pk_point`]）: 束縛済みの [`BoundScan`] が「PK 構成列すべてへの
//!   リテラル等価」を AND 連言に含むかを、スキーマと束縛結果だけから判定する。`PolicyContext`・
//!   テナントごとの索引の状態・データには依存しない。`sql::scan` の実行経路の選択と、
//!   `core.rs::scan_explain_result` の `EXPLAIN` 表示が**同じ関数**を呼ぶ（2 行が矛盾しない。
//!   SQL-27 の「矛盾出力の防止」）。
//! - **候補探査**（[`for_each_candidate`]）: 分類が [`PkPointPlan::Point`] のときに限り、
//!   `sql::scan` が全行走査の代わりに呼ぶ。テナントごとに永続一意索引 `user_uniq/{table}` の正引きを
//!   点照会し、候補行（高々 1 行／テナント）を呼び出し元のクロージャへ渡す。マーカーが無効な
//!   テナントはそのテナントの物理キー範囲だけを走査して縮退する。
//!
//! # 正しさの不変条件
//!
//! 1. **偽陰性なし**: 行 R が `WHERE` の PK 等価述語を満たすなら、R の PK 値の正準キーは変換した
//!    リテラルの正準キーと等しい。正準キーは書き込み側と同じ
//!    [`crate::constraint::push_canonical_component`] で作る（第 2 の符号化器を持たない）。リテラルを
//!    列の値へ完全に変換できない・判断に迷う場合は必ず [`PkPointPlan::NotApplicable`]（全走査）に倒す。
//! 2. **偽陽性は除かれる**: 探査は候補を絞るだけで、全候補が全走査と同じ `with_visible_row`
//!    （デコード前の `is_visible`、TABLE-12 の tenant 整合、PK 述語を含む `WHERE` 全体の再評価、
//!    可視性の再適用）を通る。stale な正引き（`unique-index.md` D4）はここで自然に除かれる。
//! 3. **出力の同一性**: テナントを物理キーの昇順に処理し、各テナントの候補を昇順に渡すため、
//!    行・順序・`OFFSET` の数え方は順序なしの全走査と同一になる。
//!
//! # テナント境界（P0）
//!
//! 読み取り側の候補探査は、他テナントの索引パーティション（マーカーと正引き）を**読み取り専用**で
//! 参照する。書き込み側の一意性判定（自テナントのパーティションだけを見る）は変えない。他テナントの
//! `Public` 行は全走査でも読まれて `is_visible` で選別されており、同じ前例に従う。可視性の判定は
//! `PolicyContext::is_visible` だけが行い、ここでは新しいテナント比較の経路を作らない。`ctx` が
//! 他テナントの `Public` 行を可視としない場合は、可視行が自テナントに閉じるため自テナントだけを探査する
//! （`sql::scan` の経路 (A) と同じ判断）。索引が壊れている場合は固定文言の内部エラー（値・id・
//! テナントを含めない）で fail-closed に拒否する。
//!
//! # コスト
//!
//! O(T・log n)（T は探査対象テナント数）。メモリはテナント数に比例せず定数（反復器でストリーム処理）。

use crate::catalog::{self, ColumnType, TableSchema};
use crate::constraint::{push_canonical_component, PkIndexProbe};
use crate::declarative_filter::{CompareOp, FilterOp, MetadataFilter, TypedLiteral};
use crate::policy::PolicyContext;
use crate::row_codec::ScalarRef;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::parser::BoundScan;
use crate::sql::udf_call::{BinOp, BoundExpr};
use crate::storage::read_source::ReadSource;
use redb::ReadableTable;
use std::ops::Bound;

/// `f64` が整数として正確に表せる上限（2^53）。`BoundExpr::ColumnRef` は行の整数値を `f64` へ写して
/// 比較するため、リテラルがこの未満でなければ「`f64` 上で等しい行」と「整数として等しい行」が
/// 食い違いうる。食い違いうる値は点検索へ回さない。
const EXACT_F64_INT_LIMIT: f64 = 9_007_199_254_740_992.0;

/// 静的分類の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PkPointPlan {
    /// PK 構成列すべてにリテラル等価があり、PK の宣言順に連結した正準キーが確定した。
    Point { canonical_key: Vec<u8> },
    /// 点検索にできない（従来どおり全走査）。
    NotApplicable,
}

impl PkPointPlan {
    /// 点検索として計画されたか（`EXPLAIN` の語彙選択に使う）。
    pub(crate) fn is_point(&self) -> bool {
        matches!(self, PkPointPlan::Point { .. })
    }
}

/// `bound` が宣言 PK の点検索になるかを静的に分類する。条件はすべて満たす場合だけ `Point`:
///
/// 1. `schema` が宣言 PK を持つ。
/// 2. ウィンドウ項目も `ORDER BY` も無い（順序なしの経路だけを対象にする）。
/// 3. PK の各構成列に、トップレベルの AND 連言（`metadata_filters`・`expr_filters`）としてその列と
///    リテラルの等価述語がある。`or_filters` は分類に使わない（取り出した行で再評価される）。
/// 4. 各リテラルを列の値へ完全に変換できる。
///
/// `sql::scan::execute_scan_with_budget` の dispatch と `core.rs::scan_explain_result` が共有する。
pub(crate) fn classify_pk_point(schema: &TableSchema, bound: &BoundScan) -> PkPointPlan {
    classify_inner(schema, bound).unwrap_or(PkPointPlan::NotApplicable)
}

fn classify_inner(schema: &TableSchema, bound: &BoundScan) -> Option<PkPointPlan> {
    let pk_cols = schema.primary_key()?;
    if pk_cols.is_empty() || !bound.windows.is_empty() || !bound.order_by.is_empty() {
        return None;
    }
    let mut canonical_key: Vec<u8> = Vec::new();
    for name in pk_cols {
        let index = schema.columns.iter().position(|c| &c.name == name)?;
        let column = schema.columns.get(index)?;
        // 許可型以外（拡張型など）は点検索にしない（fail-closed）。
        if !column.ty.is_primary_key_allowed() {
            return None;
        }
        push_literal_component(&mut canonical_key, index, &column.ty, bound)?;
    }
    Some(PkPointPlan::Point { canonical_key })
}

/// 列 `index`（型 `ty`）の等価リテラルを探し、正準キーの 1 要素として `out` へ足す。
/// 変換できる等価述語が無ければ `None`。
fn push_literal_component(
    out: &mut Vec<u8>,
    index: usize,
    ty: &ColumnType,
    bound: &BoundScan,
) -> Option<()> {
    for filter in &bound.metadata_filters {
        if filter.column_index() != index {
            continue;
        }
        if let Some(component) = component_from_filter(filter, ty) {
            out.extend_from_slice(&component);
            return Some(());
        }
    }
    for expr in &bound.expr_filters {
        if let Some(component) = component_from_numeric_expr(expr, index, ty) {
            out.extend_from_slice(&component);
            return Some(());
        }
    }
    None
}

/// 正準キー 1 要素を、書き込み側と同じ符号化器で作る。
fn encode_component(value: ScalarRef<'_>) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    push_canonical_component(&mut out, value).ok()?;
    Some(out)
}

/// `metadata_filters` の 1 本が列の等価述語なら、その正準キー要素を返す。
fn component_from_filter(filter: &MetadataFilter, ty: &ColumnType) -> Option<Vec<u8>> {
    match (filter.op(), ty) {
        // TEXT／VARCHAR の等価はバイト列の等価（照合は deterministic。`collation.rs`）。
        (FilterOp::Equals(s), ColumnType::Text | ColumnType::Varchar(_)) => {
            encode_component(ScalarRef::Text(s))
        }
        // ENUM は束縛段が語彙検証済みのラベルで比較する。正準キーもラベルのバイト列。
        (FilterOp::Equals(s), ColumnType::Enum(_)) => encode_component(ScalarRef::Enum(s)),
        // CHAR(n) の格納値は n 文字まで半角空白で詰めてある。リテラル（右側の空白は束縛時に除去済み）を
        // 同じ長さへ詰めて格納形にそろえる。n 超過は点検索にしない（全走査なら一致行なし）。
        (FilterOp::Equals(s), ColumnType::Bpchar(n)) => {
            let width = usize::try_from(*n).ok()?;
            let chars = s.chars().count();
            let pad = width.checked_sub(chars)?;
            let mut padded = String::with_capacity(s.len().checked_add(pad)?);
            padded.push_str(s);
            padded.extend(std::iter::repeat_n(' ', pad));
            encode_component(ScalarRef::Text(&padded))
        }
        (FilterOp::BoolEquals(b), ColumnType::Boolean) => encode_component(ScalarRef::Bool(*b)),
        (
            FilterOp::TypedCompare {
                op: CompareOp::Eq,
                value,
            },
            _,
        ) => match (value, ty) {
            (TypedLiteral::Date(d), ColumnType::Date) => encode_component(ScalarRef::Date(*d)),
            (TypedLiteral::Timestamp(t), ColumnType::Timestamp) => {
                encode_component(ScalarRef::Timestamp(*t))
            }
            (TypedLiteral::Uuid(u), ColumnType::Uuid) => encode_component(ScalarRef::Uuid(*u)),
            (TypedLiteral::Bytes(b), ColumnType::Bytea) => {
                encode_component(ScalarRef::Bytes(b.as_slice()))
            }
            _ => None,
        },
        _ => None,
    }
}

/// `expr_filters` の 1 本が整数 PK 列 `index` と数値リテラルの等価（`<列> = <数値>`、左右どちらでも）
/// なら、その正準キー要素を返す。整数でない値・型の値域外・`|v| >= 2^53`（`f64` 比較と整数の
/// 等価が食い違いうる）は `None`。
fn component_from_numeric_expr(expr: &BoundExpr, index: usize, ty: &ColumnType) -> Option<Vec<u8>> {
    let BoundExpr::Binary {
        op: BinOp::Eq,
        lhs,
        rhs,
    } = expr
    else {
        return None;
    };
    let literal = match (lhs.as_ref(), rhs.as_ref()) {
        (BoundExpr::ColumnRef { index: i }, BoundExpr::Number(v)) if *i == index => *v,
        (BoundExpr::Number(v), BoundExpr::ColumnRef { index: i }) if *i == index => *v,
        _ => return None,
    };
    if !literal.is_finite() || literal.fract() != 0.0 || literal.abs() >= EXACT_F64_INT_LIMIT {
        return None;
    }
    // 上の検査で整数かつ |v| < 2^53 なので i64 へ正確に変換できる。
    let value = literal as i64;
    match ty {
        ColumnType::SmallInt => {
            let v = i16::try_from(value).ok()?;
            encode_component(ScalarRef::Integer(i32::from(v)))
        }
        ColumnType::Integer => encode_component(ScalarRef::Integer(i32::try_from(value).ok()?)),
        ColumnType::BigInt => encode_component(ScalarRef::BigInt(value)),
        _ => None,
    }
}

/// 候補行 1 件（`key_tenant`・行 `id`・行バイト列）を受け取り、探査を打ち切るなら `true` を返す訪問者。
type CandidateVisitor<'a> = dyn FnMut(&str, u64, &[u8]) -> Result<bool, SqlSurfaceError> + 'a;

/// 行テーブル／索引テーブルの内部エラーを固定文言へ写す（値・id・テナントを含めない）。
fn probe_failure() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "primary key lookup failed".to_string(),
    }
}

fn storage_failure(e: impl std::fmt::Display) -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: format!("scan row scan failed: {e}"),
    }
}

// 単体テスト専用の計測フック（索引経由の点照会に入った回数・テナント範囲走査へ縮退した回数）。
// 「実際に索引経路を通ること」「縮退が起きたこと」を、行数に依存しない証拠として示す。本番ビルドには
// 含まれない。`cargo test` はテストごとに別スレッドのためスレッドローカルにする。
#[cfg(test)]
thread_local! {
    pub(crate) static INDEX_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static RANGE_FALLBACKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn count_index_probe() {
    INDEX_PROBES.with(|c| c.set(c.get() + 1));
}
#[cfg(not(test))]
#[inline(always)]
fn count_index_probe() {}

#[cfg(test)]
fn count_range_fallback() {
    RANGE_FALLBACKS.with(|c| c.set(c.get() + 1));
}
#[cfg(not(test))]
#[inline(always)]
fn count_range_fallback() {}

/// 正引きが指す locator から行テーブルの物理キーを作る。現状の locator は行 `id`（u64）そのもの。
/// 索引の行参照が rowid へ移る場合（`docs/design/rowid-primary-key-separation.md`）は、解釈を
/// この 1 か所で直す。
#[inline]
fn row_key_for_locator(tenant: &str, locator: u64) -> (&str, u64) {
    (tenant, locator)
}

/// 点検索の候補行を、テナントの昇順・各テナント内は物理キーの昇順で `visit` へ渡す。
///
/// `visit(key_tenant, id, buf)` は候補 1 行ごとに呼ばれ、`true` を返すと探査を打ち切る（`LIMIT` 到達）。
/// `rows` は呼び出し元が開いた `user_rows/{table}`。`source` から `user_uniq/{table}` と索引カタログを
/// **読み取り専用**で開く。索引テーブルが無い・マーカーが無効なテナントは、そのテナントの範囲走査に
/// 縮退する（結果は同じ）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn for_each_candidate<R, T>(
    source: &R,
    rows: &T,
    ctx: &PolicyContext,
    schema: &TableSchema,
    table_name: &str,
    canonical_key: &[u8],
    visit: &mut CandidateVisitor<'_>,
) -> Result<(), SqlSurfaceError>
where
    R: ReadSource,
    T: ReadableTable<(&'static str, u64), &'static [u8]>,
{
    let index_table_name = catalog::user_uniq_table_name(table_name);
    let index_table = match source.open_table(catalog::user_uniq_table_def(&index_table_name)) {
        Ok(t) => Some(t),
        // 索引テーブルが未作成（まだ一度も一意キー検査が走っていない）なら全テナントが縮退する。
        Err(redb::TableError::TableDoesNotExist(_)) => None,
        Err(_) => return Err(probe_failure()),
    };
    let probe = match index_table {
        Some(_) => {
            Some(PkIndexProbe::for_read(source, table_name, schema).map_err(|_| probe_failure())?)
        }
        None => None,
    };
    let index = match (&index_table, &probe) {
        (Some(t), Some(p)) => Some((t, p)),
        _ => None,
    };

    // `ctx` が他テナントの Public 行を可視としないなら、可視行は自テナントに閉じる。
    if !ctx.allows_public() {
        visit_tenant(rows, index, ctx.tenant_id(), canonical_key, visit)?;
        return Ok(());
    }

    // 全テナントを物理キーの昇順に列挙する。行テーブルから 1 ホップずつ次のテナントへ seek する
    // （索引テーブルから列挙すると、行はあるがマーカーが無いテナントを見落とす）。
    let mut after: Option<String> = None;
    loop {
        let start: Bound<(&str, u64)> = match &after {
            None => Bound::Unbounded,
            Some(t) => Bound::Excluded((t.as_str(), u64::MAX)),
        };
        let tenant = {
            let mut iter = rows
                .range::<(&str, u64)>((start, Bound::Unbounded))
                .map_err(storage_failure)?;
            match iter.next() {
                None => break,
                Some(entry) => {
                    let (k, _) = entry.map_err(storage_failure)?;
                    k.value().0.to_string()
                }
            }
        };
        if visit_tenant(rows, index, &tenant, canonical_key, visit)? {
            break;
        }
        after = Some(tenant);
    }
    Ok(())
}

/// 1 テナント分の候補探査。`visit` が打ち切りを要求したら `Ok(true)`。
fn visit_tenant<T, I>(
    rows: &T,
    index: Option<(&I, &PkIndexProbe)>,
    tenant: &str,
    canonical_key: &[u8],
    visit: &mut CandidateVisitor<'_>,
) -> Result<bool, SqlSurfaceError>
where
    T: ReadableTable<(&'static str, u64), &'static [u8]>,
    I: ReadableTable<(&'static str, &'static [u8]), &'static [u8]>,
{
    if let Some((index_table, probe)) = index {
        let valid = probe
            .marker_valid(index_table, tenant)
            .map_err(|_| probe_failure())?;
        if valid {
            count_index_probe();
            let locator = probe
                .lookup_primary_key(index_table, tenant, canonical_key)
                .map_err(|_| probe_failure())?;
            // 正引きのミスは「このテナントに一致する生存行は無い」（マーカーが有効な間は生存行の各キーが
            // 必ず正引きを持つ。`constraint/unique_index.rs` の「正しさの不変条件」）。
            let Some(locator) = locator else {
                return Ok(false);
            };
            let guard = rows
                .get(row_key_for_locator(tenant, locator))
                .map_err(storage_failure)?;
            // 行が無い正引きは stale（削除済み）。何も出さない。
            let Some(guard) = guard else {
                return Ok(false);
            };
            return visit(tenant, locator, guard.value());
        }
    }
    // 縮退: このテナントだけを範囲走査する。
    count_range_fallback();
    let start = Bound::Included((tenant, 0u64));
    let end = Bound::Included((tenant, u64::MAX));
    for entry in rows
        .range::<(&str, u64)>((start, end))
        .map_err(storage_failure)?
    {
        let (k, v) = entry.map_err(storage_failure)?;
        let (key_tenant, id) = k.value();
        if visit(key_tenant, id, v.value())? {
            return Ok(true);
        }
    }
    Ok(false)
}
#[cfg(test)]
mod tests {
    //! 計測フックによる構造的な証拠（行数に依存しない）: 実際に索引経路を通ったこと・縮退が
    //! 起きたこと・探査がテナント数に比例し行数には比例しないこと。
    use super::*;
    use crate::core::EngineCore;
    use crate::kernel::CpuScalarProvider;
    use crate::sql::mode::SessionState;
    use crate::storage::{Storage, Visibility};
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

    fn core(label: &str) -> (EngineCore, CleanupGuard) {
        let path = unique_db_path(label);
        let guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open");
        (
            EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
            guard,
        )
    }

    fn ctx(tenant: &str) -> PolicyContext {
        PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
            .expect("tenant")
    }

    fn run(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) -> usize {
        match core.execute_sql_in_session(&ctx(tenant), s, sql) {
            Ok(crate::sql::SqlOutcome::Query(r)) => r.rows.len(),
            Ok(_) => 0,
            Err(e) => panic!("{sql}: {} {e:?}", e.wire_code()),
        }
    }

    fn reset() {
        INDEX_PROBES.with(|c| c.set(0));
        RANGE_FALLBACKS.with(|c| c.set(0));
    }

    fn counts() -> (usize, usize) {
        (
            INDEX_PROBES.with(|c| c.get()),
            RANGE_FALLBACKS.with(|c| c.get()),
        )
    }

    /// 単一テナントでは行数によらず探査 1 回・範囲走査 0 回（検査コストが行数に比例しない）。
    #[test]
    fn single_tenant_probe_count_does_not_grow_with_row_count() {
        for rows in [10u32, 400] {
            let (core, _g) = core(&format!("pkpt-unit-single-{rows}"));
            let mut s = SessionState::default();
            s.allow_ddl();
            run(
                &core,
                "t1",
                &mut s,
                "CREATE TABLE pk (code INTEGER PRIMARY KEY)",
            );
            for i in 1..=rows {
                run(
                    &core,
                    "t1",
                    &mut s,
                    &format!("INSERT INTO pk (id, code) VALUES ({i}, {i})"),
                );
            }
            reset();
            let hits = run(
                &core,
                "t1",
                &mut s,
                "SELECT id FROM pk WHERE code = 7 LIMIT 5",
            );
            assert_eq!(hits, 1);
            assert_eq!(counts(), (1, 0), "rows={rows}");
        }
    }

    /// マーカーが無効なテナントだけが範囲走査へ縮退し、書き込み後は索引経路へ移る。
    #[test]
    fn tenant_without_marker_falls_back_until_its_next_write() {
        let (core, _g) = core("pkpt-unit-mixed");
        let mut s = SessionState::default();
        s.allow_ddl();
        run(
            &core,
            "alice",
            &mut s,
            "CREATE TABLE t (code TEXT NOT NULL)",
        );
        run(
            &core,
            "alice",
            &mut s,
            "INSERT INTO t (id, code) VALUES (1, 'a')",
        );
        run(
            &core,
            "bob",
            &mut s,
            "INSERT INTO t (id, code) VALUES (2, 'a')",
        );
        run(
            &core,
            "alice",
            &mut s,
            "ALTER TABLE t ADD PRIMARY KEY (code)",
        );

        // ADD 直後は全テナントのマーカーが無い: 両テナントが縮退する。
        reset();
        assert_eq!(
            run(
                &core,
                "alice",
                &mut s,
                "SELECT id FROM t WHERE code = 'a' LIMIT 5"
            ),
            1
        );
        assert_eq!(counts(), (0, 2));

        // alice が書き込むと alice は索引経路、bob は縮退のまま（結果は同じ）。
        run(
            &core,
            "alice",
            &mut s,
            "INSERT INTO t (id, code) VALUES (3, 'b')",
        );
        reset();
        assert_eq!(
            run(
                &core,
                "alice",
                &mut s,
                "SELECT id FROM t WHERE code = 'a' LIMIT 5"
            ),
            1
        );
        assert_eq!(counts(), (1, 1));

        run(
            &core,
            "bob",
            &mut s,
            "INSERT INTO t (id, code) VALUES (4, 'b')",
        );
        reset();
        assert_eq!(
            run(
                &core,
                "bob",
                &mut s,
                "SELECT id FROM t WHERE code = 'a' LIMIT 5"
            ),
            1
        );
        assert_eq!(counts(), (2, 0));
    }

    /// ctx が他テナントの Public 行を可視としないときは、自テナントだけを探査する。
    #[test]
    fn context_without_public_probes_only_its_own_tenant() {
        let (core, _g) = core("pkpt-unit-own-tenant");
        let mut s = SessionState::default();
        s.allow_ddl();
        run(
            &core,
            "alice",
            &mut s,
            "CREATE TABLE pk (code TEXT PRIMARY KEY)",
        );
        for (tenant, id) in [("alice", 1), ("bob", 2), ("carol", 3)] {
            run(
                &core,
                tenant,
                &mut s,
                &format!("INSERT INTO pk (id, code) VALUES ({id}, 'a')"),
            );
        }
        let private_only =
            PolicyContext::with_visibilities("bob", [Visibility::Private]).expect("tenant");
        reset();
        let out = core
            .execute_sql_in_session(
                &private_only,
                &mut s,
                "SELECT id FROM pk WHERE code = 'a' LIMIT 5",
            )
            .expect("select");
        let crate::sql::SqlOutcome::Query(r) = out else {
            panic!("expected Query");
        };
        assert_eq!(r.rows.iter().map(|x| x.id).collect::<Vec<_>>(), vec![2]);
        assert_eq!(counts(), (1, 0));
    }
}
