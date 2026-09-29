//! `[INNER|LEFT|RIGHT|FULL [OUTER]] JOIN`（2 テーブル等価結合）の束縛・実行本体
//! （SQL-28・RLS-10、TASK-212、Issue #925・#926）。
//!
//! 責務境界: `sql::allowlist::validate_sql_tokens` が構造検証した
//! [`crate::sql::allowlist::ValidatedJoin`] を受け取り、`core.rs::EngineCore`
//! の SQL 実行経路（session-less `execute_sql`・セッション経由
//! `execute_validated_in_session`）から呼ばれる。`sql::set_op`
//! （TASK-213・集合演算）と同じ設計判断を踏襲する:
//!
//! - 両辺（左右のテーブル参照）は既存の広域取得経路
//!   （[`crate::sql::scan::execute_scan_with_budget`]）でそれぞれ独立に評価する。
//!   これにより RLS 暗黙適用・fail-closed のエラー契約を第 2 の実行器を作らずに
//!   継承する（呼び出したセッション自身の [`crate::policy::PolicyContext`] で
//!   両辺を独立に評価するため、他テナント行が中間結果・結合キー・カーディナリ
//!   ティ判定・NULL 補完のいずれにも現れない）。
//! - 文全体（両辺の走査・ハッシュ表・出力行の生成）で 1 つの累計バイト予算
//!   （[`JoinBudget`]）を共有する（`sql::set_op::SetOpBudget` と同じ理由。
//!   security.md「不安全な設計｜無制限リソース確保（DoS）」対応）。
//!
//! アルゴリズム（ハッシュ結合。ビルド側・型クラス・上限・順序保証の詳細は
//! `docs/design/inner-join.md`・外部結合の NULL 補完・WHERE 簡約規則は
//! `docs/design/outer-join.md` 参照）: 行数の少ない側をビルド側にし、結合キーの
//! どれかが NULL の行はビルド・プローブいずれからも除外する。`LEFT`／`RIGHT`／
//! `FULL` JOIN では該当側（保存側）の未一致行を NULL 補完して出力に含める
//! （[`JoinPlan`] の `preserve_left`／`preserve_right`）。出力順序は
//! `(左側走査位置, 右側走査位置)`（NULL 補完行は「無し」を「有り」より後に
//! 並べる）の安定ソートで固定し、どちらの側をビルドに選んでも同じ結果順に
//! なるようにする（`scripts/check_sort_determinism.sh` ゲート対応。
//! `sort_unstable*` は使わない）。
//!
//! スコープ外（Issue #926 対象外事項。詳細は `docs/design/outer-join.md` 参照）:
//! - `CROSS`／`NATURAL` JOIN、`JOIN ... USING (...)`
//! - 3 テーブル以上の連鎖 JOIN
//! - `RelationSnapshotCache`（TASK-212 基盤）による結合入力のキャッシュ

use std::collections::HashMap;

use crate::catalog::{ColumnType, TableSchema};
use crate::policy::PolicyContext;
use crate::sql::allowlist::{
    JoinKind, JoinProjection, JoinWherePredicate, Projection, SqlSurfaceError, ValidatedJoin,
    WherePredicate,
};
use crate::sql::exec::{Cell, ColumnMeta, QueryResult, ResultRow};
use crate::sql::relation::{BindingScope, ColumnRef, ColumnSlot, TableRef};
use crate::sql::udf_call::UdfRegistry;

/// JOIN 1 辺（可視かつ WHERE に一致する行）の行数上限（実装既定値。Issue #925
/// §2.4）。無制限 `Vec` 確保を避ける（security.md「不安全な設計」対応）。
/// 超過検出のため走査自体は `MAX_JOIN_INPUT_ROWS + 1` を上限に行い、超えたら
/// `54000`。
pub(crate) const MAX_JOIN_INPUT_ROWS: usize = 100_000;

/// `LIMIT` 適用前の結合カーディナリティ（一致ペア数）の上限（実装既定値。
/// Issue #925 §2.4）。`LIMIT`／`OFFSET` の値によらず判定する（PR #1105 の
/// 教訓に沿った単純な規則）。超過は `54000`。
pub(crate) const MAX_JOIN_OUTPUT_ROWS: usize = 100_000;

/// テスト専用に上限を差し替えられるようにした構造体（`sql::scan::execute_scan`／
/// `sql::set_op::execute_with_budget` と同じ設計判断）。
pub(crate) struct JoinLimits {
    pub(crate) max_input_rows: usize,
    pub(crate) max_output_rows: usize,
    pub(crate) budget_cap: usize,
}

impl Default for JoinLimits {
    fn default() -> Self {
        Self {
            max_input_rows: MAX_JOIN_INPUT_ROWS,
            max_output_rows: MAX_JOIN_OUTPUT_ROWS,
            budget_cap: crate::arena::MAX_ARENA_TOTAL_BYTES,
        }
    }
}

/// 文全体（両辺の走査・ハッシュ表・出力行の生成）で共有する累計バイト予算
/// （`sql::set_op::SetOpBudget` と同じ設計。モジュールドキュメント参照）。
struct JoinBudget {
    used: usize,
    cap: usize,
}

impl JoinBudget {
    fn new(cap: usize) -> Self {
        Self { used: 0, cap }
    }

    fn remaining(&self) -> usize {
        self.cap.saturating_sub(self.used)
    }

    fn charge(&mut self, bytes: usize) -> Result<(), SqlSurfaceError> {
        self.used = self.used.saturating_add(bytes);
        if self.used > self.cap {
            return Err(SqlSurfaceError::payload_too_large(
                "JOIN exceeds the shared result byte budget",
            ));
        }
        Ok(())
    }
}

/// 行集合の推定バイト量（`sql::set_op::result_bytes` と同じ考え方。JOIN と
/// 集合演算は目的が異なる独立モジュールのため、予算計上のためだけに
/// モジュール境界を跨いで private 関数を共有しない——両モジュールが将来
/// 独立に変更されても互いに壊れない設計判断）。
fn result_bytes(column_count: usize, rows: &[ResultRow]) -> usize {
    let cell_struct_bytes = column_count.saturating_mul(std::mem::size_of::<Cell>());
    let result_row_struct_bytes = std::mem::size_of::<ResultRow>();
    let per_row_struct_bytes = cell_struct_bytes.saturating_add(result_row_struct_bytes);
    let mut total = per_row_struct_bytes.saturating_mul(rows.len());
    for row in rows {
        for cell in &row.cells {
            total = total.saturating_add(cell_payload_bytes(cell));
        }
    }
    total
}

fn cell_payload_bytes(cell: &Cell) -> usize {
    match cell {
        Cell::Text(s) => s.len(),
        Cell::Bytes(b) => b.len(),
        Cell::Json(s) => s.len(),
        Cell::Numeric(d) => d.to_string().len(),
        Cell::Vector(v) => v.len().saturating_mul(std::mem::size_of::<f32>()),
        Cell::Array(arr) => match arr {
            crate::row_codec::ArrayValue::Text(items) => items.iter().map(|s| s.len()).sum(),
            crate::row_codec::ArrayValue::Bool(items) => items.len(),
        },
        Cell::Null
        | Cell::Integer(_)
        | Cell::Float(_)
        | Cell::Bool(_)
        | Cell::SignedInteger(_)
        | Cell::Date(_)
        | Cell::Timestamp(_)
        | Cell::Uuid(_) => 0,
    }
}

/// [`crate::sql::allowlist::ValidatedJoin`] が参照するテーブル名（重複除去済み。
/// 自己結合では 1 個になる）。`core.rs::EngineCore` が単一の `read_txn` 上で
/// 両辺のスキーマを解決するために使う（`read_txn_with_schemas`）。
pub(crate) fn collect_join_tables(validated: &ValidatedJoin) -> Vec<String> {
    let mut out = Vec::with_capacity(validated.relations.len());
    for r in &validated.relations {
        if !out.iter().any(|t| t == r.table()) {
            out.push(r.table().to_string());
        }
    }
    out
}

/// JOIN 結合キーの型クラス（Issue #925 §2.3）。整数クラス（疑似列 `id`・
/// `INTEGER`・`BIGINT`）は相互に結合できる（外部キー設計 `a.id = b.fk` 形を
/// 成立させるため）。それ以外は完全一致（`Enum` は型名まで一致）が必要。
#[derive(Debug, Clone, PartialEq, Eq)]
enum JoinKeyClass {
    Integer,
    Text,
    Bool,
    Date,
    Timestamp,
    Uuid,
    Bytea,
    Enum(String),
}

/// 結合キーとして許可する列型を判定する（Issue #925 §2.3）。`None` は疑似列
/// `id`（整数クラス）。`VECTOR`・`REAL`／`DOUBLE`／`NUMERIC`／`JSON`／`JSONB`／
/// `ARRAY` は許可しない（`42601`）。
fn key_class(ty: Option<&ColumnType>) -> Result<JoinKeyClass, SqlSurfaceError> {
    match ty {
        None => Ok(JoinKeyClass::Integer),
        Some(ColumnType::Integer) | Some(ColumnType::BigInt) => Ok(JoinKeyClass::Integer),
        Some(ColumnType::Text) => Ok(JoinKeyClass::Text),
        Some(ColumnType::Boolean) => Ok(JoinKeyClass::Bool),
        Some(ColumnType::Date) => Ok(JoinKeyClass::Date),
        Some(ColumnType::Timestamp) => Ok(JoinKeyClass::Timestamp),
        Some(ColumnType::Uuid) => Ok(JoinKeyClass::Uuid),
        Some(ColumnType::Bytea) => Ok(JoinKeyClass::Bytea),
        Some(ColumnType::Enum(def)) => Ok(JoinKeyClass::Enum(def.name().to_string())),
        Some(_) => Err(SqlSurfaceError::unsupported(
            "this column type cannot be used as a JOIN key",
        )),
    }
}

/// 解決済み列参照（[`ColumnSlot`]）を、そのテーブルの列名（疑似列 `id` を含む）
/// へ写像する。`scope` が同じ `schema` から構築済みであれば必ず成功する
/// （`BindingScope::resolve` の不変条件）。
fn slot_name(slot: ColumnSlot, schema: &TableSchema) -> Result<String, SqlSurfaceError> {
    match slot {
        ColumnSlot::Id => Ok("id".to_string()),
        ColumnSlot::Column(idx) => {
            schema
                .columns
                .get(idx)
                .map(|c| c.name.clone())
                .ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "JOIN column slot index out of range".to_string(),
                })
        }
    }
}

fn push_dedup(v: &mut Vec<String>, name: &str) {
    if !v.iter().any(|s| s == name) {
        v.push(name.to_string());
    }
}

/// 1 辺の走査投影（`sql::allowlist::Projection` へ変換する前の中間表現）。
/// `All` は疑似列 `id`（位置 0）＋スキーマ列順（位置 `1 + index`）という
/// [`crate::sql::parser::bind_projection`] の `Projection::All` 展開規則と
/// 一致させる（第 2 の展開規則を作らない）。
enum SideProjection {
    All,
    Columns(Vec<String>),
}

impl SideProjection {
    fn position(&self, name: &str, schema: &TableSchema) -> Option<usize> {
        match self {
            // `bind_projection`（`Projection::All`）・`BindingScope::
            // resolve_in_relation` はいずれも実カラムを疑似列 `id` より優先して
            // 照合する（スキーマが `id` という実カラムを持つ場合、その値を
            // 指す）。ここで疑似列を先に判定すると、その規則と矛盾する誤った
            // 位置（実カラム `id` の値ではなく行キー）を返してしまう
            // （advisor 指摘の回帰: `tests::star_projection_key_position_prefers_real_id_column_over_pseudo_column`）。
            SideProjection::All => schema
                .columns
                .iter()
                .position(|c| c.name == name)
                .map(|i| i + 1)
                .or(if name == "id" { Some(0) } else { None }),
            SideProjection::Columns(v) => v.iter().position(|s| s == name),
        }
    }

    fn to_projection(&self) -> Projection {
        match self {
            SideProjection::All => Projection::All,
            SideProjection::Columns(v) => Projection::Columns(v.clone()),
        }
    }
}

/// 出力列 1 個（どちら側の、走査結果の何番目のセルかという位置つき）。
enum OutputColumn {
    Left(usize, ColumnMeta),
    Right(usize, ColumnMeta),
}

/// 束縛済みの JOIN 実行計画（`build_plan` の戻り値）。[`execute_with_limits`]・
/// [`describe_columns`] が共有する（第 2 の束縛経路を作らない）。
struct JoinPlan {
    left_scan: crate::sql::allowlist::ValidatedScan,
    right_scan: crate::sql::allowlist::ValidatedScan,
    left_key_positions: Vec<usize>,
    right_key_positions: Vec<usize>,
    key_classes: Vec<JoinKeyClass>,
    output: Vec<OutputColumn>,
    /// 左側（保存側）の未一致行を NULL 補完して出力に含めるか（Issue #926
    /// §2.3）。`LEFT`／`FULL` JOIN かつ右側（欠損側）に WHERE 述語が無い場合の
    /// み真になる——欠損側に strict な述語（[`is_null_rejecting`]）があれば、
    /// NULL 補完行は必ずその述語で落ちるため、`INNER` と同じ扱いに簡約する
    /// （`docs/design/outer-join.md` の WHERE 簡約規則）。
    preserve_left: bool,
    /// 右側（保存側）の未一致行を NULL 補完して出力に含めるか。`RIGHT`／`FULL`
    /// JOIN かつ左側（欠損側）に WHERE 述語が無い場合のみ真になる（上記の
    /// 左右対称）。
    preserve_right: bool,
}

/// 述語が `NULL` に対して常に偽（non-matching）になるか（strict）を判定する
/// （Issue #926 §2.3。`docs/design/outer-join.md` の WHERE 簡約規則の前提）。
/// 現行で受理する全 variant（`=`・`LIKE`・比較・bool 等価・bool 列）は
/// いずれも strict なため常に `true` を返すが、ワイルドカード無しの網羅的
/// `match` にすることで、将来 `IS NULL` 等の非 strict な variant を追加した際に
/// コンパイルエラーで検出させる（簡約規則が黙って壊れるのを防ぐ）。
fn is_null_rejecting(pred: &JoinWherePredicate) -> bool {
    match pred {
        JoinWherePredicate::Equality { .. }
        | JoinWherePredicate::Prefix { .. }
        | JoinWherePredicate::Compare { .. }
        | JoinWherePredicate::BoolEquality { .. }
        | JoinWherePredicate::BoolColumn { .. } => true,
    }
}

fn join_where_column(pred: &JoinWherePredicate) -> &ColumnRef {
    match pred {
        JoinWherePredicate::Equality { column, .. }
        | JoinWherePredicate::Prefix { column, .. }
        | JoinWherePredicate::Compare { column, .. }
        | JoinWherePredicate::BoolEquality { column, .. }
        | JoinWherePredicate::BoolColumn { column } => column,
    }
}

/// [`JoinWherePredicate`]（修飾子つき列参照）を、束縛済みの相手側 relation へ
/// プッシュダウンする非修飾 [`WherePredicate`] へ変換する（Issue #925 §2.3）。
fn convert_join_where_predicate(pred: &JoinWherePredicate, name: String) -> WherePredicate {
    match pred {
        JoinWherePredicate::Equality { value, .. } => WherePredicate::Equality {
            column: name,
            value: value.clone(),
        },
        JoinWherePredicate::Prefix { pattern, .. } => WherePredicate::Prefix {
            column: name,
            pattern: pattern.clone(),
        },
        JoinWherePredicate::Compare { op, value, .. } => WherePredicate::Compare {
            column: name,
            op: *op,
            value: value.clone(),
        },
        JoinWherePredicate::BoolEquality { value, .. } => WherePredicate::BoolEquality {
            column: name,
            value: *value,
        },
        JoinWherePredicate::BoolColumn { .. } => WherePredicate::BoolColumn { column: name },
    }
}

/// [`ValidatedJoin`] を束縛する（Issue #925 §2.3）。列解決（`BindingScope`）・
/// 結合キー型検証・投影展開・WHERE プッシュダウンをすべて行走査より前に確定
/// させ、両辺の [`crate::sql::allowlist::ValidatedScan`]（`sql::parser::
/// bind_scan_with_dummy_flags` へそのまま渡せる）を組み立てる。
fn build_plan(
    schemas: &HashMap<String, TableSchema>,
    validated: &ValidatedJoin,
) -> Result<JoinPlan, SqlSurfaceError> {
    let (left_ref, right_ref): (&TableRef, &TableRef) = match validated.relations.as_slice() {
        [l, r] => (l, r),
        _ => {
            return Err(SqlSurfaceError::Internal {
                detail: "JOIN must have exactly two relations".to_string(),
            })
        }
    };
    let left_schema = schemas
        .get(left_ref.table())
        .ok_or_else(|| SqlSurfaceError::Internal {
            detail: "schema missing for JOIN left relation".to_string(),
        })?;
    let right_schema = schemas
        .get(right_ref.table())
        .ok_or_else(|| SqlSurfaceError::Internal {
            detail: "schema missing for JOIN right relation".to_string(),
        })?;

    let scope = BindingScope::new(vec![
        (left_ref.clone(), left_schema),
        (right_ref.clone(), right_schema),
    ])?;

    let mut on_keys: Vec<(String, String, JoinKeyClass)> = Vec::with_capacity(validated.on.len());
    for (lhs, rhs) in &validated.on {
        let lhs_resolved = scope.resolve(lhs)?;
        let rhs_resolved = scope.resolve(rhs)?;
        if lhs_resolved.relation() == rhs_resolved.relation() {
            return Err(SqlSurfaceError::unsupported(
                "JOIN ON condition must reference both sides of the join",
            ));
        }
        let (left_resolved, right_resolved) = if lhs_resolved.relation() == 0 {
            (lhs_resolved, rhs_resolved)
        } else {
            (rhs_resolved, lhs_resolved)
        };
        let left_class = key_class(left_resolved.column_type())?;
        let right_class = key_class(right_resolved.column_type())?;
        if left_class != right_class {
            return Err(SqlSurfaceError::datatype_mismatch(
                "JOIN key type mismatch between the two sides",
            ));
        }
        let left_name = slot_name(left_resolved.slot(), left_schema)?;
        let right_name = slot_name(right_resolved.slot(), right_schema)?;
        on_keys.push((left_name, right_name, left_class));
    }

    let (left_side_proj, right_side_proj, output) = match &validated.projection {
        JoinProjection::All => {
            let mut output =
                Vec::with_capacity(2 + left_schema.columns.len() + right_schema.columns.len());
            output.push(OutputColumn::Left(0, ColumnMeta::Id));
            for (idx, col) in left_schema.columns.iter().enumerate() {
                output.push(OutputColumn::Left(
                    idx + 1,
                    ColumnMeta::Scalar {
                        name: col.name.clone(),
                        ty: col.ty.clone(),
                    },
                ));
            }
            output.push(OutputColumn::Right(0, ColumnMeta::Id));
            for (idx, col) in right_schema.columns.iter().enumerate() {
                output.push(OutputColumn::Right(
                    idx + 1,
                    ColumnMeta::Scalar {
                        name: col.name.clone(),
                        ty: col.ty.clone(),
                    },
                ));
            }
            (SideProjection::All, SideProjection::All, output)
        }
        JoinProjection::Columns(colrefs) => {
            let mut left_needed: Vec<String> = Vec::new();
            let mut right_needed: Vec<String> = Vec::new();
            for (l, _, _) in &on_keys {
                push_dedup(&mut left_needed, l);
            }
            for (_, r, _) in &on_keys {
                push_dedup(&mut right_needed, r);
            }
            let mut output = Vec::with_capacity(colrefs.len());
            for colref in colrefs {
                let resolved = scope.resolve(colref)?;
                let (is_left, schema) = if resolved.relation() == 0 {
                    (true, left_schema)
                } else {
                    (false, right_schema)
                };
                let name = slot_name(resolved.slot(), schema)?;
                let meta = match resolved.slot() {
                    ColumnSlot::Id => ColumnMeta::Id,
                    ColumnSlot::Column(idx) => ColumnMeta::Scalar {
                        name: name.clone(),
                        ty: schema
                            .columns
                            .get(idx)
                            .ok_or_else(|| SqlSurfaceError::Internal {
                                detail: "JOIN projection column index out of range".to_string(),
                            })?
                            .ty
                            .clone(),
                    },
                };
                if is_left {
                    push_dedup(&mut left_needed, &name);
                    let pos = left_needed.iter().position(|s| s == &name).ok_or_else(|| {
                        SqlSurfaceError::Internal {
                            detail: "JOIN left projection column missing after insertion"
                                .to_string(),
                        }
                    })?;
                    output.push(OutputColumn::Left(pos, meta));
                } else {
                    push_dedup(&mut right_needed, &name);
                    let pos = right_needed
                        .iter()
                        .position(|s| s == &name)
                        .ok_or_else(|| SqlSurfaceError::Internal {
                            detail: "JOIN right projection column missing after insertion"
                                .to_string(),
                        })?;
                    output.push(OutputColumn::Right(pos, meta));
                }
            }
            (
                SideProjection::Columns(left_needed),
                SideProjection::Columns(right_needed),
                output,
            )
        }
    };

    let mut left_where: Vec<WherePredicate> = Vec::new();
    let mut right_where: Vec<WherePredicate> = Vec::new();
    for pred in &validated.where_conjuncts {
        // Issue #926 §2.3: 外部結合の NULL 補完行は WHERE 述語より後に評価される
        // PostgreSQL 意味論を、実装上は「欠損側に述語があれば NULL 補完しない
        // （INNER に簡約する）」規則で再現する。この簡約が正しいのは述語が
        // strict（NULL に対して必ず偽）な場合に限るため、fail-closed に
        // 拒否する（現行の全 variant は strict なため到達しないが、将来
        // `IS NULL` 等の非 strict な variant が増えた際の防御）。この防御は
        // NULL 補完（LEFT/RIGHT/FULL）を行う場合にのみ必要であり、INNER JOIN
        // は非 strict な述語をプッシュダウンしても意味論上問題ないため対象外
        // とする（レビュー指摘対応: INNER JOIN で不要な拒否をしない）。
        if !matches!(validated.kind, JoinKind::Inner) && !is_null_rejecting(pred) {
            return Err(SqlSurfaceError::unsupported(
                "JOIN WHERE predicate is not supported for LEFT/RIGHT/FULL OUTER JOIN reduction",
            ));
        }
        let column_ref = join_where_column(pred);
        let resolved = scope.resolve(column_ref)?;
        let (is_left, schema) = if resolved.relation() == 0 {
            (true, left_schema)
        } else {
            (false, right_schema)
        };
        let name = slot_name(resolved.slot(), schema)?;
        let converted = convert_join_where_predicate(pred, name);
        if is_left {
            left_where.push(converted);
        } else {
            right_where.push(converted);
        }
    }

    // Issue #926 §2.3: 保存側の判定（上記コメント参照）。`Inner` は両方偽の
    // ままで、既存の INNER JOIN 挙動と完全に一致する。
    let preserve_left =
        matches!(validated.kind, JoinKind::Left | JoinKind::Full) && right_where.is_empty();
    let preserve_right =
        matches!(validated.kind, JoinKind::Right | JoinKind::Full) && left_where.is_empty();

    let mut left_key_positions = Vec::with_capacity(on_keys.len());
    let mut right_key_positions = Vec::with_capacity(on_keys.len());
    let mut key_classes = Vec::with_capacity(on_keys.len());
    for (l, r, class) in &on_keys {
        let lp =
            left_side_proj
                .position(l, left_schema)
                .ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "JOIN left key column missing from side projection".to_string(),
                })?;
        let rp =
            right_side_proj
                .position(r, right_schema)
                .ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "JOIN right key column missing from side projection".to_string(),
                })?;
        left_key_positions.push(lp);
        right_key_positions.push(rp);
        key_classes.push(class.clone());
    }

    let left_scan = crate::sql::allowlist::ValidatedScan {
        table_name: left_ref.table().to_string(),
        projection: left_side_proj.to_projection(),
        where_predicates: left_where,
        limit: 1,
        order_by: Vec::new(),
        offset: 0,
        window_items: Vec::new(),
    };
    let right_scan = crate::sql::allowlist::ValidatedScan {
        table_name: right_ref.table().to_string(),
        projection: right_side_proj.to_projection(),
        where_predicates: right_where,
        limit: 1,
        order_by: Vec::new(),
        offset: 0,
        window_items: Vec::new(),
    };

    Ok(JoinPlan {
        left_scan,
        right_scan,
        left_key_positions,
        right_key_positions,
        key_classes,
        output,
        preserve_left,
        preserve_right,
    })
}

fn push_len_prefixed(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), SqlSurfaceError> {
    let len = u32::try_from(bytes.len())
        .map_err(|_| SqlSurfaceError::payload_too_large("JOIN key exceeds length limit"))?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

/// 結合キー 1 成分（セル＋型クラス）を正準バイト列へ追記する（Issue #925
/// §2.4）。型クラスは [`build_plan`] の型検証を通過済みのため、`class` と
/// `cell` の組み合わせは常に整合する（不整合はビルド段の内部バグとして
/// 防御的に拒否する。fail-closed）。
fn encode_key_component(
    cell: &Cell,
    class: &JoinKeyClass,
    out: &mut Vec<u8>,
) -> Result<(), SqlSurfaceError> {
    match (class, cell) {
        (JoinKeyClass::Integer, Cell::Integer(v)) => {
            out.extend_from_slice(&i128::from(*v).to_be_bytes());
            Ok(())
        }
        (JoinKeyClass::Integer, Cell::SignedInteger(v)) => {
            out.extend_from_slice(&i128::from(*v).to_be_bytes());
            Ok(())
        }
        (JoinKeyClass::Text, Cell::Text(s)) | (JoinKeyClass::Enum(_), Cell::Text(s)) => {
            push_len_prefixed(out, s.as_bytes())
        }
        (JoinKeyClass::Bool, Cell::Bool(b)) => {
            out.push(u8::from(*b));
            Ok(())
        }
        (JoinKeyClass::Date, Cell::Date(d)) => {
            out.extend_from_slice(&d.to_be_bytes());
            Ok(())
        }
        (JoinKeyClass::Timestamp, Cell::Timestamp(t)) => {
            out.extend_from_slice(&t.to_be_bytes());
            Ok(())
        }
        (JoinKeyClass::Uuid, Cell::Uuid(u)) => {
            out.extend_from_slice(u.as_bytes());
            Ok(())
        }
        (JoinKeyClass::Bytea, Cell::Bytes(b)) => push_len_prefixed(out, b),
        _ => Err(SqlSurfaceError::Internal {
            detail: "JOIN key cell/class mismatch".to_string(),
        }),
    }
}

/// 行から結合キー（複数列の連結）を抽出する。いずれかの成分が `NULL` の行は
/// 結合キーを持たない（`Ok(None)`）——ビルド・プローブいずれからも除外する
/// （Issue #925 §2.4。NULL は決して一致しない）。
fn encode_key(
    row: &ResultRow,
    positions: &[usize],
    classes: &[JoinKeyClass],
) -> Result<Option<Vec<u8>>, SqlSurfaceError> {
    let mut out = Vec::new();
    for (pos, class) in positions.iter().zip(classes.iter()) {
        let cell = row
            .cells
            .get(*pos)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "JOIN key position out of range in result row".to_string(),
            })?;
        if matches!(cell, Cell::Null) {
            return Ok(None);
        }
        encode_key_component(cell, class, &mut out)?;
    }
    Ok(Some(out))
}

/// [`ValidatedJoin`] を実行する（`core.rs::EngineCore::execute_validated_in_session`
/// の `Statement::Join` アーム・session-less `execute_sql` の同アームから
/// 呼ばれる）。`schemas` は呼び出し元が単一の `read_txn`（同一スナップショット）
/// 上で解決済みのものを渡す（`EngineCore::read_txn_with_schemas`）。
pub(crate) fn execute(
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    schemas: &HashMap<String, TableSchema>,
    validated: &ValidatedJoin,
    udfs: &UdfRegistry,
) -> Result<QueryResult, SqlSurfaceError> {
    execute_with_limits(
        read_txn,
        ctx,
        schemas,
        validated,
        udfs,
        &JoinLimits::default(),
    )
}

/// [`execute`] の本体。`limits` はテスト専用に上限を差し替えられるようにした
/// もの（[`JoinLimits`]。`sql::scan::execute_scan_with_budget` と同じ設計）。
pub(crate) fn execute_with_limits(
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    schemas: &HashMap<String, TableSchema>,
    validated: &ValidatedJoin,
    udfs: &UdfRegistry,
    limits: &JoinLimits,
) -> Result<QueryResult, SqlSurfaceError> {
    let plan = build_plan(schemas, validated)?;

    let (left_ref, right_ref) = match validated.relations.as_slice() {
        [l, r] => (l, r),
        _ => {
            return Err(SqlSurfaceError::Internal {
                detail: "JOIN must have exactly two relations".to_string(),
            })
        }
    };
    let left_schema = schemas
        .get(left_ref.table())
        .ok_or_else(|| SqlSurfaceError::Internal {
            detail: "schema missing for JOIN left relation".to_string(),
        })?;
    let right_schema = schemas
        .get(right_ref.table())
        .ok_or_else(|| SqlSurfaceError::Internal {
            detail: "schema missing for JOIN right relation".to_string(),
        })?;

    // §2.4/§2.5: 両辺の束縛（列参照・WHERE 述語の型検証）を完了させてから
    // `LIMIT`／`OFFSET` の範囲検証（`22000`）を行う（束縛エラー優先。
    // docs/design/inner-join.md の順序契約。Describe（`describe_columns`）と
    // 同じ判断順序に揃える）。走査（`execute_scan_with_budget`）自体は
    // 束縛結果を使って後段で行う。
    let mut left_bound =
        crate::sql::parser::bind_scan_with_dummy_flags(&plan.left_scan, left_schema, udfs, &[])?;
    left_bound.limit = limits.max_input_rows.saturating_add(1);
    let mut right_bound =
        crate::sql::parser::bind_scan_with_dummy_flags(&plan.right_scan, right_schema, udfs, &[])?;
    right_bound.limit = limits.max_input_rows.saturating_add(1);

    let limit = crate::sql::parser::validate_search_limit(validated.limit)?;
    let offset = crate::sql::parser::validate_search_offset(validated.offset)?;

    let mut budget = JoinBudget::new(limits.budget_cap);

    // §2.4: 両辺の評価は既存の広域取得経路（`execute_scan_with_budget`）を通す
    // ——RLS 暗黙適用・fail-closed のエラー写像を第 2 の実行器なしに継承する。
    // 呼び出したセッションの `ctx`（`PolicyContext`）で両辺を独立に評価する
    // ため、他テナント行は中間結果・結合キー・カーディナリティ判定の
    // いずれにも現れない（RLS-10 相当）。
    let left_result = crate::sql::scan::execute_scan_with_budget(
        read_txn,
        ctx,
        left_schema,
        &left_bound,
        budget.remaining(),
    )?;
    if left_result.rows.len() > limits.max_input_rows {
        return Err(SqlSurfaceError::payload_too_large(
            "JOIN left input exceeds the visible row limit",
        ));
    }
    budget.charge(result_bytes(left_result.columns.len(), &left_result.rows))?;

    let right_result = crate::sql::scan::execute_scan_with_budget(
        read_txn,
        ctx,
        right_schema,
        &right_bound,
        budget.remaining(),
    )?;
    if right_result.rows.len() > limits.max_input_rows {
        return Err(SqlSurfaceError::payload_too_large(
            "JOIN right input exceeds the visible row limit",
        ));
    }
    budget.charge(result_bytes(right_result.columns.len(), &right_result.rows))?;

    // ビルド側は行数の少ない方（同数なら右）。出力順序は後段の安定ソートで
    // 固定するため、ビルド側の選択自体は結果の観測可能な順序に影響しない
    // （モジュールドキュメント参照）。
    let build_is_left = left_result.rows.len() < right_result.rows.len();
    let (build_rows, build_positions, probe_rows, probe_positions) = if build_is_left {
        (
            &left_result.rows,
            &plan.left_key_positions,
            &right_result.rows,
            &plan.right_key_positions,
        )
    } else {
        (
            &right_result.rows,
            &plan.right_key_positions,
            &left_result.rows,
            &plan.left_key_positions,
        )
    };

    let mut table: HashMap<Vec<u8>, Vec<u32>> = HashMap::new();
    for (idx, row) in build_rows.iter().enumerate() {
        let idx_u32 = u32::try_from(idx).map_err(|_| {
            SqlSurfaceError::payload_too_large("JOIN build side row count exceeds limit")
        })?;
        if let Some(key) = encode_key(row, build_positions, &plan.key_classes)? {
            budget.charge(key.len())?;
            table.entry(key).or_default().push(idx_u32);
        }
    }

    // Issue #926 §2.4: 保存側だけ未一致フラグ配列を確保する（`preserve_*` が
    // 偽の側は確保しない——INNER JOIN では両方偽のままで既存の挙動と完全に
    // 一致する）。確保前に予算へ計上する（security.md「不安全な設計」対応）。
    let mut matched_left: Vec<bool> = if plan.preserve_left {
        budget.charge(left_result.rows.len())?;
        vec![false; left_result.rows.len()]
    } else {
        Vec::new()
    };
    let mut matched_right: Vec<bool> = if plan.preserve_right {
        budget.charge(right_result.rows.len())?;
        vec![false; right_result.rows.len()]
    } else {
        Vec::new()
    };

    // §2.4: 一致ペア数の上限判定（ループ内の早期打ち切り）は出力を実体化する
    // 前に行う。判定に使うのは可視行（両辺とも RLS・WHERE 適用済み）だけなので、
    // 他テナントの行数は結果に影響しない。
    let mut pairs: Vec<(u32, u32)> = Vec::new();
    for (probe_idx, row) in probe_rows.iter().enumerate() {
        let probe_idx_u32 = u32::try_from(probe_idx).map_err(|_| {
            SqlSurfaceError::payload_too_large("JOIN probe side row count exceeds limit")
        })?;
        let key = match encode_key(row, probe_positions, &plan.key_classes)? {
            Some(k) => k,
            None => continue,
        };
        budget.charge(key.len())?;
        if let Some(matched) = table.get(&key) {
            for &build_idx in matched {
                let (left_idx, right_idx) = if build_is_left {
                    (build_idx, probe_idx_u32)
                } else {
                    (probe_idx_u32, build_idx)
                };
                pairs.push((left_idx, right_idx));
                if let Some(m) = matched_left.get_mut(left_idx as usize) {
                    *m = true;
                }
                if let Some(m) = matched_right.get_mut(right_idx as usize) {
                    *m = true;
                }
                if pairs.len() > limits.max_output_rows {
                    return Err(SqlSurfaceError::payload_too_large(
                        "JOIN result exceeds the row limit",
                    ));
                }
            }
        }
    }

    // Issue #926 §2.4: 保存側の未一致行（NULL 補完対象）を数える。`INNER` は
    // 両方 0 のままで、以降の合計カーディナリティ判定・出力生成が既存の
    // INNER JOIN 挙動と完全に一致する。
    let unmatched_left = matched_left.iter().filter(|&&m| !m).count();
    let unmatched_right = matched_right.iter().filter(|&&m| !m).count();

    // §2.4: 合計カーディナリティ（NULL 補完行を含む）を実体化する前に判定する。
    // `LIMIT`／`OFFSET` の値には依存させない（PR #1105 の教訓に沿った単純な
    // 規則）。判定に使うのは可視行だけなので、他テナントの行数は結果に影響
    // しない。
    let total_cardinality = pairs
        .len()
        .checked_add(unmatched_left)
        .and_then(|t| t.checked_add(unmatched_right));
    match total_cardinality {
        Some(total) if total <= limits.max_output_rows => {}
        _ => {
            return Err(SqlSurfaceError::payload_too_large(
                "JOIN result exceeds the row limit",
            ));
        }
    }

    // 出力前の中間表現（`Option` は NULL 補完を表す。`None` 側はどちらか一方
    // のみで、両方 `None` の要素は作らない）。`pairs`（一致ペア）と要素数が
    // 重複しうる `combined`（一致ペア＋ NULL 補完行）を別に確保するため、
    // 確保前にこの容量分を共有バイト予算へ計上する（Issue #926 レビュー
    // 指摘。`matched_left`／`matched_right` と同じく「確保前に charge」の
    // 契約——security.md「不安全な設計」対応）。
    let combined_capacity = pairs
        .len()
        .saturating_add(unmatched_left)
        .saturating_add(unmatched_right);
    budget.charge(
        combined_capacity.saturating_mul(std::mem::size_of::<(Option<u32>, Option<u32>)>()),
    )?;
    let mut combined: Vec<(Option<u32>, Option<u32>)> = Vec::with_capacity(combined_capacity);
    for &(l, r) in &pairs {
        combined.push((Some(l), Some(r)));
    }
    if plan.preserve_left {
        for (idx, &m) in matched_left.iter().enumerate() {
            if !m {
                let idx_u32 = u32::try_from(idx).map_err(|_| {
                    SqlSurfaceError::payload_too_large("JOIN left row count exceeds limit")
                })?;
                combined.push((Some(idx_u32), None));
            }
        }
    }
    if plan.preserve_right {
        for (idx, &m) in matched_right.iter().enumerate() {
            if !m {
                let idx_u32 = u32::try_from(idx).map_err(|_| {
                    SqlSurfaceError::payload_too_large("JOIN right row count exceeds limit")
                })?;
                combined.push((None, Some(idx_u32)));
            }
        }
    }

    // 順序（決定的）: 左側走査順 → 右側走査順。左側が無い行（右のみの NULL
    // 補完行）は「有り」より後ろへ、右側が無い行はその左位置の直後へ並ぶ
    // （Issue #926 §2.4）。`sort_by_key` は安定ソート
    // （`scripts/check_sort_determinism.sh` ゲート対応。`sort_unstable*` は
    // 使わない）。`Inner`（`Option` が常に `Some`）ではこのキーは
    // `(false, l, false, r)` に潰れ、既存の `(l, r)` ソートと同じ順序になる。
    combined.sort_by_key(|&(l, r)| (l.is_none(), l.unwrap_or(0), r.is_none(), r.unwrap_or(0)));

    let start = offset.min(combined.len());
    let end = start.saturating_add(limit).min(combined.len());
    let sliced = combined.get(start..end).unwrap_or(&[]);

    // 出力行 1 行あたりの構造体オーバーヘッド（`result_bytes` の
    // per_row_struct_bytes と同じ計算式）。行ごとの見積もりに使う。
    let per_row_struct_bytes = plan
        .output
        .len()
        .saturating_mul(std::mem::size_of::<Cell>())
        .saturating_add(std::mem::size_of::<ResultRow>());

    let mut rows = Vec::with_capacity(sliced.len());
    for &(l, r) in sliced {
        let left_row = l
            .map(|idx| {
                left_result
                    .rows
                    .get(idx as usize)
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "JOIN left row index out of range".to_string(),
                    })
            })
            .transpose()?;
        let right_row = r
            .map(|idx| {
                right_result
                    .rows
                    .get(idx as usize)
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "JOIN right row index out of range".to_string(),
                    })
            })
            .transpose()?;

        // codex-review 指摘（PR #1110）: 大きなセルを持つ入力行が多数の出力行に
        // 一致すると、`.cloned()` で全行を複製してから予算照合すると `LIMIT`
        // の範囲内でも同じペイロードを大量に複製し、共有バイト予算を超える
        // メモリを確保した後にしか `54000` を返せない（security.md「不安全な
        // 設計｜無制限リソース確保（DoS）」に抵触）。複製前に借用のまま
        // この 1 行の見積もりを算出し、残予算と照合してから複製する
        // （超過時は複製・確保そのものを行わない fail-closed）。NULL 補完側
        // （`left_row`／`right_row` が `None`）は 0 バイトとして扱う
        // （Issue #926 §2.4）。
        let mut row_payload_bytes = 0usize;
        for out_col in &plan.output {
            if let Some(cell) = resolve_output_cell(out_col, left_row, right_row)? {
                row_payload_bytes = row_payload_bytes.saturating_add(cell_payload_bytes(cell));
            }
        }
        budget.charge(per_row_struct_bytes.saturating_add(row_payload_bytes))?;

        let mut cells = Vec::with_capacity(plan.output.len());
        for out_col in &plan.output {
            let cell = resolve_output_cell(out_col, left_row, right_row)?
                .cloned()
                .unwrap_or(Cell::Null);
            cells.push(cell);
        }
        // `ResultRow.id`: 左行があれば左の `id`、無ければ右の `id`
        // （`docs/design/outer-join.md`。wire・HTTP のクエリ出力では `row.id`
        // を使っていないため観測上の影響は無い）。
        let id = match left_row {
            Some(row) => row.id,
            None => {
                right_row
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "JOIN output row has neither a left nor a right side".to_string(),
                    })?
                    .id
            }
        };
        rows.push(ResultRow {
            id,
            score: 0.0,
            cells,
        });
    }

    let columns = plan
        .output
        .iter()
        .map(|c| match c {
            OutputColumn::Left(_, m) | OutputColumn::Right(_, m) => m.clone(),
        })
        .collect();

    Ok(QueryResult { columns, rows })
}

/// 出力列 1 個のセルを、NULL 補完（欠損側）を考慮して解決する（Issue #926
/// §2.4）。欠損側（`left_row`／`right_row` が `None`）を参照する出力列は
/// `Ok(None)`（呼び出し元が `Cell::Null` へ写像する）を返す。存在するはずの
/// 側の列位置が見つからない場合は `build_plan` の内部不整合として拒否する
/// （fail-closed）。
fn resolve_output_cell<'a>(
    out_col: &OutputColumn,
    left_row: Option<&'a ResultRow>,
    right_row: Option<&'a ResultRow>,
) -> Result<Option<&'a Cell>, SqlSurfaceError> {
    let (row, pos) = match out_col {
        OutputColumn::Left(pos, _) => (left_row, *pos),
        OutputColumn::Right(pos, _) => (right_row, *pos),
    };
    match row {
        Some(row) => row
            .cells
            .get(pos)
            .map(Some)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "JOIN output column position out of range".to_string(),
            }),
        None => Ok(None),
    }
}

/// Describe（拡張クエリプロトコル）向けに、両辺の走査を一切行わず結果列
/// メタデータだけを導出する（`sql::set_op::describe_columns` と同じ方針）。
/// JOIN は `$n` パラメータを受理しない（`sql::params::validate_param_positions`
/// が構文段で拒否する）ため、ダミー等価フラグは常に空でよい。
pub(crate) fn describe_columns(
    schemas: &HashMap<String, TableSchema>,
    validated: &ValidatedJoin,
    udfs: &UdfRegistry,
) -> Result<Vec<ColumnMeta>, SqlSurfaceError> {
    let plan = build_plan(schemas, validated)?;

    let (left_ref, right_ref) = match validated.relations.as_slice() {
        [l, r] => (l, r),
        _ => {
            return Err(SqlSurfaceError::Internal {
                detail: "JOIN must have exactly two relations".to_string(),
            })
        }
    };
    let left_schema = schemas
        .get(left_ref.table())
        .ok_or_else(|| SqlSurfaceError::Internal {
            detail: "schema missing for JOIN left relation".to_string(),
        })?;
    let right_schema = schemas
        .get(right_ref.table())
        .ok_or_else(|| SqlSurfaceError::Internal {
            detail: "schema missing for JOIN right relation".to_string(),
        })?;
    // 走査せずに束縛だけ行い、WHERE リテラルの型検証を Execute と同じ判定
    // 基準で確定させる（Describe は本体を実行しない契約）。§2.4/§2.5:
    // 両辺の束縛完了を `LIMIT`／`OFFSET` 検証より前に行う（束縛エラー優先。
    // docs/design/inner-join.md の順序契約。Execute と同じ判断順序）。
    crate::sql::parser::bind_scan_with_dummy_flags(&plan.left_scan, left_schema, udfs, &[])?;
    crate::sql::parser::bind_scan_with_dummy_flags(&plan.right_scan, right_schema, udfs, &[])?;

    crate::sql::parser::validate_search_limit(validated.limit)?;
    crate::sql::parser::validate_search_offset(validated.offset)?;

    Ok(plan
        .output
        .iter()
        .map(|c| match c {
            OutputColumn::Left(_, m) | OutputColumn::Right(_, m) => m.clone(),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnDef, TableSchema};
    use crate::recovery::required_op_id::OperationId;
    use crate::row_codec::Value;
    use crate::sql::allowlist::{JoinKind, Projection, ValidatedJoin};
    use crate::sql::udf_call::UdfRegistry;
    use crate::storage::{Storage, Visibility};
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
    use redb::ReadableDatabase;

    fn schema(name: &str) -> TableSchema {
        TableSchema::new(
            name,
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("tag", ColumnType::Text, false),
            ],
        )
    }

    fn insert_row(storage: &Storage, table: &str, ctx: &PolicyContext, id: u64, tag: &str) {
        let op_id = OperationId::parse(&format!("seed-{table}-{id}")).expect("valid operation_id");
        crate::tenant::insert_typed_row(
            storage,
            table,
            ctx,
            id,
            Visibility::Public,
            &[Value::Vector(vec![0.0, 0.0]), Value::Text(tag.to_string())],
            &op_id,
        )
        .expect("insert row");
    }

    /// `id = id` の結合。両テーブルとも `id` を疑似列のまま使う最小構成
    /// （`build_plan` の整数クラス互換〔疑似列同士〕を経由する）。
    fn id_join(left: &str, right: &str) -> ValidatedJoin {
        ValidatedJoin {
            relations: vec![TableRef::new(left), TableRef::new(right)],
            kind: JoinKind::Inner,
            on: vec![(
                ColumnRef::qualified(left, "id"),
                ColumnRef::qualified(right, "id"),
            )],
            projection: JoinProjection::Columns(vec![ColumnRef::qualified(left, "tag")]),
            where_conjuncts: Vec::new(),
            limit: 10,
            offset: 0,
        }
    }

    /// Issue #925 §2.4 の回帰: 側の可視行数が注入した上限を超えたら `54000`。
    #[test]
    fn input_row_overflow_is_rejected_with_injected_limits() {
        let path = unique_db_path("join-input-row-overflow");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        storage.create_table(&schema("l")).expect("create l");
        storage.create_table(&schema("r")).expect("create r");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        for i in 0..5u64 {
            insert_row(&storage, "l", &ctx, i, "x");
        }
        insert_row(&storage, "r", &ctx, 0, "y");

        let validated = id_join("l", "r");
        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut schemas = HashMap::new();
        schemas.insert("l".to_string(), schema("l"));
        schemas.insert("r".to_string(), schema("r"));
        let udfs = UdfRegistry::default();
        let limits = JoinLimits {
            max_input_rows: 2,
            ..JoinLimits::default()
        };

        let err = execute_with_limits(&read_txn, &ctx, &schemas, &validated, &udfs, &limits)
            .expect_err("left side row count must exceed the injected input row limit");
        assert_eq!(err.wire_code(), "54000");
    }

    /// Issue #925 §2.4 の回帰: `LIMIT`／`OFFSET` の値によらず、結合カーディナリ
    /// ティ（`LIMIT` 適用前の一致ペア数）が注入した上限を超えたら `54000`。
    #[test]
    fn output_cardinality_overflow_is_rejected_regardless_of_limit() {
        let path = unique_db_path("join-output-cardinality-overflow");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        storage.create_table(&schema("l")).expect("create l");
        storage.create_table(&schema("r")).expect("create r");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        // 同一キー（id=0）を複数持たせて直積を発生させる代わりに、`ON` を
        // 常に真になる比較にはできないため、複数行が同じ疑似列 `id` を持つ
        // ことはできない。代わりに複数の一致候補を作るため、右側に複数行を
        // 挿入して結合キー列（`tag` を通さず `id` 疑似列）を直接一致させる
        // ことはできないので、右側は 1 行のみとし、`max_output_rows` を 0 に
        // 注入して「1 件の一致でも上限超過」を確認する。
        insert_row(&storage, "l", &ctx, 0, "x");
        insert_row(&storage, "r", &ctx, 0, "y");

        let validated = ValidatedJoin {
            limit: 1,
            ..id_join("l", "r")
        };
        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut schemas = HashMap::new();
        schemas.insert("l".to_string(), schema("l"));
        schemas.insert("r".to_string(), schema("r"));
        let udfs = UdfRegistry::default();
        let limits = JoinLimits {
            max_output_rows: 0,
            ..JoinLimits::default()
        };

        let err = execute_with_limits(&read_txn, &ctx, &schemas, &validated, &udfs, &limits)
            .expect_err("a single matching pair must exceed a zero output row limit");
        assert_eq!(err.wire_code(), "54000");
    }

    /// PR #1105（`sql::set_op`）と同じ設計判断の回帰: 文全体（両辺の走査・
    /// ハッシュ表・出力行）で 1 つの累計バイト予算を共有すること。1 辺だけなら
    /// 収まるが両辺合計では超える上限を注入すると `54000` になることを確認する。
    #[test]
    fn shared_byte_budget_rejects_when_combined_scans_exceed_the_injected_cap() {
        let path = unique_db_path("join-shared-budget");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        storage.create_table(&schema("l")).expect("create l");
        storage.create_table(&schema("r")).expect("create r");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        insert_row(&storage, "l", &ctx, 0, &"x".repeat(4096));
        insert_row(&storage, "r", &ctx, 0, &"y".repeat(4096));

        let validated = id_join("l", "r");
        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut schemas = HashMap::new();
        schemas.insert("l".to_string(), schema("l"));
        schemas.insert("r".to_string(), schema("r"));
        let udfs = UdfRegistry::default();
        let limits = JoinLimits {
            budget_cap: 4096 + 512,
            ..JoinLimits::default()
        };

        let err = execute_with_limits(&read_txn, &ctx, &schemas, &validated, &udfs, &limits)
            .expect_err("combined side bytes must exceed the injected shared budget");
        assert_eq!(err.wire_code(), "54000");
    }

    /// codex-review 指摘（PR #1110・join.rs:786）の回帰: 1 件の大きなセルを
    /// 持つビルド側の行が、プローブ側の複数行と一致して出力側で多数回
    /// 複製される（fan-out）場合でも、走査段（両辺のスキャン・ハッシュキー）
    /// だけでは収まる予算を注入すると出力段の複製で `54000` に落ちること
    /// （出力行ごとの見積もりが複製前に残予算と照合されていること）を確認する。
    /// ビルド側（`tag` を結合キーに使うため走査段では小さい値のまま）1 行の
    /// 大きな `embedding`（VECTOR）だけがプローブ側の一致数だけ出力へ複製される
    /// ため、走査段の合計とは別に出力段だけが予算超過することを再現できる。
    #[test]
    fn output_row_fan_out_of_a_large_cell_is_rejected_before_exceeding_the_budget() {
        let path = unique_db_path("join-fan-out-large-cell");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        let big_schema = |name: &str| {
            TableSchema::new(
                name,
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(512), false),
                    ColumnDef::new("tag", ColumnType::Text, false),
                ],
            )
        };
        storage.create_table(&big_schema("l")).expect("create l");
        storage.create_table(&big_schema("r")).expect("create r");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

        let op_id = OperationId::parse("seed-l-0").expect("valid operation_id");
        crate::tenant::insert_typed_row(
            &storage,
            "l",
            &ctx,
            0,
            Visibility::Public,
            &[Value::Vector(vec![1.0; 512]), Value::Text("k".to_string())],
            &op_id,
        )
        .expect("insert build-side row");
        // プローブ側は結合キー（`tag` = "k"）だけを共有する多数の小さな行。
        // 走査段の合計バイト量は小さいまま、出力段の一致ペア数（fan-out）だけが
        // 増える構図にする。
        const PROBE_ROWS: u64 = 32;
        for i in 0..PROBE_ROWS {
            let op_id = OperationId::parse(&format!("seed-r-{i}")).expect("valid operation_id");
            crate::tenant::insert_typed_row(
                &storage,
                "r",
                &ctx,
                i,
                Visibility::Public,
                &[Value::Vector(vec![0.0; 512]), Value::Text("k".to_string())],
                &op_id,
            )
            .expect("insert probe-side row");
        }

        let validated = ValidatedJoin {
            relations: vec![TableRef::new("l"), TableRef::new("r")],
            kind: JoinKind::Inner,
            on: vec![(
                ColumnRef::qualified("l", "tag"),
                ColumnRef::qualified("r", "tag"),
            )],
            projection: JoinProjection::Columns(vec![ColumnRef::qualified("l", "embedding")]),
            where_conjuncts: Vec::new(),
            limit: PROBE_ROWS as u32,
            offset: 0,
        };
        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut schemas = HashMap::new();
        schemas.insert("l".to_string(), big_schema("l"));
        schemas.insert("r".to_string(), big_schema("r"));
        let udfs = UdfRegistry::default();
        // 走査段（両辺のスキャン結果・結合キー）の合計は大きな embedding が
        // 1 個分（512 次元 * 4 byte = 2048 byte）＋プローブ側の小さな行群で
        // 収まるが、出力段は同じ embedding を `PROBE_ROWS` 回複製するため
        // 大きく超過する上限を注入する。
        let limits = JoinLimits {
            budget_cap: 8192,
            ..JoinLimits::default()
        };

        let err = execute_with_limits(&read_txn, &ctx, &schemas, &validated, &udfs, &limits)
            .expect_err("fan-out duplication of the large cell must exceed the injected budget");
        assert_eq!(err.wire_code(), "54000");
    }

    /// 上のちょうど対照: 両辺合計が収まる上限を注入すれば成功することを確認する
    /// （回帰テストが常に失敗するだけの壊れた検証になっていないことの確認）。
    #[test]
    fn shared_byte_budget_succeeds_when_combined_scans_fit_the_injected_cap() {
        let path = unique_db_path("join-shared-budget-accept");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        storage.create_table(&schema("l")).expect("create l");
        storage.create_table(&schema("r")).expect("create r");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        insert_row(&storage, "l", &ctx, 0, "x");
        insert_row(&storage, "r", &ctx, 0, "y");

        let validated = id_join("l", "r");
        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut schemas = HashMap::new();
        schemas.insert("l".to_string(), schema("l"));
        schemas.insert("r".to_string(), schema("r"));
        let udfs = UdfRegistry::default();

        let result = execute(&read_txn, &ctx, &schemas, &validated, &udfs)
            .expect("small rows must fit the default budget");
        assert_eq!(result.rows.len(), 1);
    }

    #[test]
    fn collect_join_tables_dedups_self_join() {
        let validated = ValidatedJoin {
            relations: vec![
                TableRef::with_alias("a", "x"),
                TableRef::with_alias("a", "y"),
            ],
            kind: JoinKind::Inner,
            on: vec![(ColumnRef::unqualified("id"), ColumnRef::unqualified("id"))],
            projection: JoinProjection::All,
            where_conjuncts: Vec::new(),
            limit: 10,
            offset: 0,
        };
        assert_eq!(collect_join_tables(&validated), vec!["a".to_string()]);
    }

    /// `Projection` を直接構築しても [`SideProjection::to_projection`] が
    /// 同じ形へ写像すること（内部型の変換規則の回帰）。
    #[test]
    fn side_projection_to_projection_matches_variant() {
        let all = SideProjection::All;
        assert_eq!(all.to_projection(), Projection::All);
        let cols = SideProjection::Columns(vec!["a".to_string(), "b".to_string()]);
        assert_eq!(
            cols.to_projection(),
            Projection::Columns(vec!["a".to_string(), "b".to_string()])
        );
    }

    /// advisor 指摘の回帰: スキーマが `id` という実カラムを持つ場合、
    /// `SideProjection::All::position` は疑似列（位置 0）ではなく実カラムの
    /// 位置（`1 + index`）を返さなければならない（`bind_projection`・
    /// `BindingScope::resolve_in_relation` と同じ優先規則）。
    #[test]
    fn side_projection_all_position_prefers_real_id_column_over_pseudo_column() {
        let schema = TableSchema::new(
            "t",
            vec![
                ColumnDef::new("id", ColumnType::Text, false),
                ColumnDef::new("tag", ColumnType::Text, false),
            ],
        );
        assert_eq!(SideProjection::All.position("id", &schema), Some(1));
        assert_eq!(SideProjection::All.position("tag", &schema), Some(2));
    }

    /// 実カラム `id` を持たないスキーマでは疑似列（位置 0）を返す
    /// （上のテストの対照。回帰が常に失敗するだけの壊れた検証になっていない
    /// ことの確認）。
    #[test]
    fn side_projection_all_position_falls_back_to_pseudo_id_when_no_real_column() {
        let schema = TableSchema::new("t", vec![ColumnDef::new("tag", ColumnType::Text, false)]);
        assert_eq!(SideProjection::All.position("id", &schema), Some(0));
    }

    /// advisor 指摘の統合回帰: 実カラム `id` を持つテーブルを `*` 投影で
    /// JOIN すると、結合キーの抽出・出力の両方が実カラムの値を使うこと
    /// （行キーではなく）。
    #[test]
    fn star_projection_join_key_uses_real_id_column_not_pseudo_row_key() {
        let path = unique_db_path("join-star-real-id-column");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        let left_schema = TableSchema::new(
            "l",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("id", ColumnType::Text, false),
            ],
        );
        let right_schema = TableSchema::new(
            "r",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("xx", ColumnType::Text, false),
            ],
        );
        storage.create_table(&left_schema).expect("create l");
        storage.create_table(&right_schema).expect("create r");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        // 行キー（疑似列）は l=100・r=200（互いに一致しない）だが、実カラム
        // `id`／`xx` は "shared-key" で一致させる。誤って疑似列を結合キーに
        // 使うと 0 件、正しく実カラムを使うと 1 件になる。
        let op_id = OperationId::parse("seed-l-100").expect("valid operation_id");
        crate::tenant::insert_typed_row(
            &storage,
            "l",
            &ctx,
            100,
            Visibility::Public,
            &[
                Value::Vector(vec![0.0, 0.0]),
                Value::Text("shared-key".to_string()),
            ],
            &op_id,
        )
        .expect("insert l row");
        let op_id = OperationId::parse("seed-r-200").expect("valid operation_id");
        crate::tenant::insert_typed_row(
            &storage,
            "r",
            &ctx,
            200,
            Visibility::Public,
            &[
                Value::Vector(vec![0.0, 0.0]),
                Value::Text("shared-key".to_string()),
            ],
            &op_id,
        )
        .expect("insert r row");

        let validated = ValidatedJoin {
            relations: vec![TableRef::new("l"), TableRef::new("r")],
            kind: JoinKind::Inner,
            on: vec![(
                ColumnRef::qualified("l", "id"),
                ColumnRef::qualified("r", "xx"),
            )],
            projection: JoinProjection::All,
            where_conjuncts: Vec::new(),
            limit: 10,
            offset: 0,
        };
        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut schemas = HashMap::new();
        schemas.insert("l".to_string(), left_schema);
        schemas.insert("r".to_string(), right_schema);
        let udfs = UdfRegistry::default();

        let result = execute(&read_txn, &ctx, &schemas, &validated, &udfs)
            .expect("join on real id/xx columns should succeed");
        assert_eq!(
            result.rows.len(),
            1,
            "join must match on the real id/xx column values, not the pseudo row key"
        );
    }

    /// Issue #926 §2.3 の回帰: `is_null_rejecting` は現行の全 `JoinWherePredicate`
    /// variant で真を返す（WHERE 簡約規則の前提）。将来 variant が増えた際は
    /// 網羅的 `match` がコンパイルエラーで検出する（本テストの更新も必要）。
    #[test]
    fn is_null_rejecting_returns_true_for_all_current_variants() {
        use crate::sql::allowlist::CompareOp;
        let column = ColumnRef::unqualified("x");
        let preds = [
            JoinWherePredicate::Equality {
                column: column.clone(),
                value: "v".to_string(),
            },
            JoinWherePredicate::Prefix {
                column: column.clone(),
                pattern: "v".to_string(),
            },
            JoinWherePredicate::Compare {
                column: column.clone(),
                op: CompareOp::Gt,
                value: "v".to_string(),
            },
            JoinWherePredicate::BoolEquality {
                column: column.clone(),
                value: true,
            },
            JoinWherePredicate::BoolColumn { column },
        ];
        for pred in &preds {
            assert!(is_null_rejecting(pred), "pred={pred:?}");
        }
    }

    /// Issue #926 §2.4 の回帰: LEFT JOIN では未一致の左行が NULL 補完行として
    /// カーディナリティに加算されるため、一致ペア数だけなら収まる注入上限でも
    /// 全体では超過して `54000` になる。
    #[test]
    fn unmatched_left_row_pushes_cardinality_over_the_injected_limit_for_left_join() {
        let path = unique_db_path("outer-join-cardinality-overflow");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        storage.create_table(&schema("l")).expect("create l");
        storage.create_table(&schema("r")).expect("create r");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        // id=0 は両側一致（1 ペア）。id=1 は左側のみ（NULL 補完で 1 行追加）。
        insert_row(&storage, "l", &ctx, 0, "x");
        insert_row(&storage, "l", &ctx, 1, "orphan");
        insert_row(&storage, "r", &ctx, 0, "y");

        let validated = ValidatedJoin {
            kind: JoinKind::Left,
            ..id_join("l", "r")
        };
        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut schemas = HashMap::new();
        schemas.insert("l".to_string(), schema("l"));
        schemas.insert("r".to_string(), schema("r"));
        let udfs = UdfRegistry::default();
        let limits = JoinLimits {
            max_output_rows: 1,
            ..JoinLimits::default()
        };

        let err = execute_with_limits(&read_txn, &ctx, &schemas, &validated, &udfs, &limits)
            .expect_err("1 matched pair + 1 NULL-padded row must exceed a limit of 1");
        assert_eq!(err.wire_code(), "54000");
    }

    /// 上のちょうど対照: 同じデータを INNER JOIN で実行すると未一致の左行は
    /// 出力に含まれないため、一致ペア数（1 件）が注入上限（1 件）に収まって
    /// 成功する（回帰テストが常に失敗するだけの壊れた検証になっていない
    /// ことの確認）。
    #[test]
    fn same_data_fits_under_the_injected_limit_for_inner_join() {
        let path = unique_db_path("outer-join-cardinality-accept");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        storage.create_table(&schema("l")).expect("create l");
        storage.create_table(&schema("r")).expect("create r");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        insert_row(&storage, "l", &ctx, 0, "x");
        insert_row(&storage, "l", &ctx, 1, "orphan");
        insert_row(&storage, "r", &ctx, 0, "y");

        let validated = id_join("l", "r");
        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut schemas = HashMap::new();
        schemas.insert("l".to_string(), schema("l"));
        schemas.insert("r".to_string(), schema("r"));
        let udfs = UdfRegistry::default();
        let limits = JoinLimits {
            max_output_rows: 1,
            ..JoinLimits::default()
        };

        let result = execute_with_limits(&read_txn, &ctx, &schemas, &validated, &udfs, &limits)
            .expect("INNER JOIN cardinality (1 matched pair) must fit the limit of 1");
        assert_eq!(result.rows.len(), 1);
    }

    /// Issue #926 §2.3 の回帰: LEFT JOIN で欠損側（右）に WHERE 述語があると、
    /// NULL 補完行は必ずその述語で落ちるため簡約して INNER と同じ結果になる
    /// （`preserve_left` が偽になる）。
    #[test]
    fn left_join_with_predicate_on_missing_side_reduces_to_inner() {
        let path = unique_db_path("outer-join-where-reduction");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        storage.create_table(&schema("l")).expect("create l");
        storage.create_table(&schema("r")).expect("create r");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        insert_row(&storage, "l", &ctx, 0, "x");
        insert_row(&storage, "l", &ctx, 1, "orphan");
        insert_row(&storage, "r", &ctx, 0, "y");

        let validated = ValidatedJoin {
            kind: JoinKind::Left,
            where_conjuncts: vec![JoinWherePredicate::Equality {
                column: ColumnRef::qualified("r", "tag"),
                value: "y".to_string(),
            }],
            ..id_join("l", "r")
        };
        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut schemas = HashMap::new();
        schemas.insert("l".to_string(), schema("l"));
        schemas.insert("r".to_string(), schema("r"));
        let udfs = UdfRegistry::default();

        let result = execute(&read_txn, &ctx, &schemas, &validated, &udfs)
            .expect("LEFT JOIN with a predicate on the missing side should succeed");
        assert_eq!(
            result.rows.len(),
            1,
            "the unmatched left row must not be NULL-padded when the right side has a WHERE predicate"
        );
    }
}
