//! 集合演算（`UNION`／`UNION ALL`／`INTERSECT`／`EXCEPT`）の束縛・実行本体
//! （SQL-29 (c)・RLS-10 (b)・TASK-213）。
//!
//! 責務境界: `sql::allowlist::validate_sql_tokens` が構造検証した
//! [`crate::sql::allowlist::ValidatedSetOperation`]（構文木は
//! [`crate::sql::allowlist::SetTree`]。葉は単一テーブルの広域取得
//! [`crate::sql::allowlist::ValidatedScan`] に限定する——TASK-212（`JOIN`・複数
//! テーブル実行計画基盤）を前提にしない設計判断。TASK-213 は TASK-212 に後続
//! するが、本実装は各枝を独立した単一テーブル走査として扱うことで基盤の完成を
//! 待たずに導入する）を受け取り、`core.rs::EngineCore` の SQL 実行経路
//! （session-less `execute_sql`・セッション経由 `execute_validated_in_session`）
//! から呼ばれる。
//!
//! 各枝は既存の広域取得経路（[`crate::sql::scan::execute_scan_with_budget`]）を
//! そのまま通す。これにより RLS 暗黙適用（RLS-7 相当）・fail-closed のエラー契約を
//! 第 2 の実行器を作らずに継承する（各枝は呼び出しセッション自身の
//! [`crate::policy::PolicyContext`] で独立に評価されるため、他テナント行が
//! 中間結果・重複除去・`INTERSECT`／`EXCEPT` の判定・件数のいずれにも現れない。
//! RLS-10 (b)）。
//!
//! 文全体（全枝の走査・合成・行キー生成）で 1 つの累計バイト予算
//! （[`SetOpBudget`]。単一クエリの結果予算と同じ上限
//! [`crate::arena::MAX_ARENA_TOTAL_BYTES`]）を共有する（PR #1105 レビュー指摘
//! 対応: 各枝が独立に同予算を使い、最大枝数上限（実装既定値 16）分の結果を
//! `eval_tree` が同時に保持し得ると、単一クエリの結果予算をはるかに超える
//! 合計メモリを SQL 入力だけで確保できてしまう〔DoS〕ため）。
//!
//! スコープ外（Issue #929 の対象外事項。詳細は PR 本文参照）:
//! - 明示トランザクション内での実行

use std::collections::{HashMap, HashSet};

use crate::catalog::{ColumnType, TableSchema};
use crate::policy::PolicyContext;
use crate::sql::allowlist::{SetOperator, SetTree, SqlSurfaceError, ValidatedScan};
use crate::sql::exec::{Cell, ColumnMeta, QueryResult, ResultRow};
use crate::sql::udf_call::UdfRegistry;

/// 重複除去を伴う演算（`UNION`／`INTERSECT`／`EXCEPT`）の行キー集合の基数上限
/// （実装既定値。`core::MAX_SEARCH_K` と同値）。超過は `54000`。合成結果の行数
/// 上限も同じ定数を共有する（重複除去後の行数は必ずこの値以下になる）。
const MAX_SET_OP_ROWS: usize = crate::core::MAX_SEARCH_K;

/// 集合演算文全体（全枝の走査・合成・行キー生成）で共有する累計バイト予算
/// （PR #1105 レビュー指摘対応）。各枝を独立に
/// [`crate::sql::scan::execute_scan`] の既定予算（[`crate::arena::MAX_ARENA_TOTAL_BYTES`]）
/// で評価すると、最大 16 枝分の結果を `eval_tree` が同時に保持できてしまい、
/// 単一クエリの結果予算をはるかに超える合計メモリを SQL 入力だけで確保できる
/// （security.md「不安全な設計｜無制限リソース確保（DoS）」対応）。本予算は
/// 単一クエリの結果予算と同じ上限を文全体で 1 つだけ持ち、枝の評価
/// （[`eval_branch`]）・ノード合成・行キー生成（[`eval_tree`]）のいずれでも
/// 消費し、超過は `54000` で打ち切る。
struct SetOpBudget {
    used: usize,
    cap: usize,
}

impl SetOpBudget {
    fn new(cap: usize) -> Self {
        Self { used: 0, cap }
    }

    /// まだ消費していない残り予算（次に評価する枝の [`crate::sql::scan::
    /// execute_scan_with_budget`] へそのまま渡す上限として使う。枝単体でも
    /// 残り予算を超えられないようにする）。
    fn remaining(&self) -> usize {
        self.cap.saturating_sub(self.used)
    }

    /// `bytes` を消費として計上する。累計が上限を超えたら `54000` で拒否する
    /// （fail-closed。超過分をアロケーションした後の判定ではなく、既に確保済み
    /// の行データのバイト量を根拠にするため、確保自体は上限内に収まった分のみ）。
    fn charge(&mut self, bytes: usize) -> Result<(), SqlSurfaceError> {
        self.used = self.used.saturating_add(bytes);
        if self.used > self.cap {
            return Err(SqlSurfaceError::payload_too_large(
                "set operation exceeds the shared result byte budget",
            ));
        }
        Ok(())
    }
}

/// 行集合（1 枝分の実行結果、または合成後の中間結果）の推定バイト量。
/// [`crate::sql::scan::execute_scan_with_budget`] が単一枝内で行う予算計上と
/// 同じ考え方（セル構造体オーバーヘッド＋可変長ペイロード実体）を、複数枝を
/// 横断する累計予算（[`SetOpBudget`]）へ計上するために使う。
fn result_bytes(columns: &[ColumnMeta], rows: &[ResultRow]) -> usize {
    let cell_struct_bytes = columns.len().saturating_mul(std::mem::size_of::<Cell>());
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

/// セル 1 個が持つ可変長ペイロードの推定バイト量（固定長セルは
/// [`result_bytes`] 側の構造体オーバーヘッドで既に計上済みのため `0`）。
fn cell_payload_bytes(cell: &Cell) -> usize {
    match cell {
        Cell::Text(s) => s.len(),
        Cell::Bytes(b) => b.len(),
        Cell::Json(s) => s.len(),
        // 正確な内部表現サイズではなく十進文字列表現による近似（予算計上の
        // 目的では十分。実際の確保量を下回らない側に倒す）。
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

/// 枝が参照するテーブル名を構文木から集める（重複は 1 回だけ収集する）。
/// `core.rs::EngineCore` が単一の `read_txn` 上で全テーブルのスキーマを解決する
/// ために使う（`read_txn_with_schemas`）。
pub(crate) fn collect_branch_tables(tree: &SetTree, out: &mut Vec<String>) {
    match tree {
        SetTree::Branch(scan) => {
            if !out.iter().any(|t| t == &scan.table_name) {
                out.push(scan.table_name.clone());
            }
        }
        SetTree::Op { left, right, .. } => {
            collect_branch_tables(left, out);
            collect_branch_tables(right, out);
        }
    }
}

/// 1 個の評価結果（枝またはノード合成後）。`has_vector` は結果列に `VECTOR` 列を
/// 含むかどうか（重複除去を伴う演算の可否判定に使う。§2.2 参照）。
struct EvalOutcome {
    columns: Vec<ColumnMeta>,
    rows: Vec<ResultRow>,
    has_vector: bool,
}

/// [`ColumnMeta`] 同士の型整合判定（SQL-29 (c) §2.2）。枝の投影は構文検証段
/// （`sql::allowlist::parse_set_branch`）で `Computed` 項目を拒否済みのため、
/// ここで到達するのは `Id`／`Scalar` のみ（`Computed` が現れた場合は防御的に
/// 不一致として扱う）。
fn columns_compatible(a: &ColumnMeta, b: &ColumnMeta) -> bool {
    match (a, b) {
        (ColumnMeta::Id, ColumnMeta::Id) => true,
        (ColumnMeta::Scalar { ty: ta, .. }, ColumnMeta::Scalar { ty: tb, .. }) => ta == tb,
        _ => false,
    }
}

/// 2 枝の列数・列型整合を検証する（SQL-29 (c) §2.2）。列数不一致・列型不一致は
/// `DatatypeMismatch`（`42804`）、重複除去を伴う演算での `VECTOR` 列混在は
/// `InvalidInput`（`22000`）で拒否する。[`validate_tree_types`]（実行前の事前
/// 検証）・[`describe_tree`]（Describe 経路）で同じ関数を共有し、判定基準を
/// 単一化する（PR #1105 レビュー指摘対応: 従来はこの判定ロジックを `eval_tree`
/// と `describe_tree` の 2 箇所に重複実装しており、判定基準が乖離しうる構造
/// だった）。
fn check_branch_type_compatibility(
    op: SetOperator,
    l_columns: &[ColumnMeta],
    r_columns: &[ColumnMeta],
    l_has_vector: bool,
    r_has_vector: bool,
) -> Result<(), SqlSurfaceError> {
    if l_columns.len() != r_columns.len() {
        return Err(SqlSurfaceError::datatype_mismatch(format!(
            "column count mismatch: {} vs {}",
            l_columns.len(),
            r_columns.len()
        )));
    }
    for (i, (a, b)) in l_columns.iter().zip(r_columns.iter()).enumerate() {
        if !columns_compatible(a, b) {
            return Err(SqlSurfaceError::datatype_mismatch(format!(
                "column {i} type mismatch between set operation branches"
            )));
        }
    }
    let dedup_needed = !matches!(op, SetOperator::UnionAll);
    if dedup_needed && (l_has_vector || r_has_vector) {
        return Err(SqlSurfaceError::invalid_input(
            "VECTOR columns cannot be combined with UNION/INTERSECT/EXCEPT (use UNION ALL)",
        ));
    }
    Ok(())
}

fn columns_have_vector(columns: &[ColumnMeta]) -> bool {
    columns.iter().any(|c| {
        matches!(
            c,
            ColumnMeta::Scalar {
                ty: ColumnType::Vector(_),
                ..
            }
        )
    })
}

/// `f64` の正準化（`-0.0` を `+0.0` へ、NaN を単一表現へ）。SQL-25 (c) の同値判定
/// 規約と同一（#1098 が `sql::distinct` へ `canon_f64` を追加した場合はそちらへ
/// 統合する。現時点では未マージのためローカル定義とする）。
fn canon_f64_bits(f: f64) -> u64 {
    let normalized = if f == 0.0 { 0.0 } else { f };
    if normalized.is_nan() {
        f64::NAN.to_bits()
    } else {
        normalized.to_bits()
    }
}

fn push_len_prefixed(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), SqlSurfaceError> {
    let len = u32::try_from(bytes.len()).map_err(|_| {
        SqlSurfaceError::payload_too_large("set operation row key exceeds length limit")
    })?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

/// 行の正準バイト列表現（重複除去・`INTERSECT`／`EXCEPT` の同値判定に使う行
/// キー）。セルごとに「型タグ＋長さ接頭辞（`u32` BE）＋ペイロード」を連結する
/// （SQL-29 (c) §2.3）。`VECTOR` 列は §2.2 の型検証で重複除去を伴う演算からは
/// 排除済みのため、到達した場合は防御的に拒否する（fail-closed）。
fn row_key(row: &ResultRow) -> Result<Vec<u8>, SqlSurfaceError> {
    let mut out = Vec::new();
    for cell in &row.cells {
        match cell {
            Cell::Null => out.push(0),
            Cell::Integer(v) => {
                out.push(1);
                out.extend_from_slice(&v.to_be_bytes());
            }
            Cell::Text(s) => {
                out.push(2);
                push_len_prefixed(&mut out, s.as_bytes())?;
            }
            Cell::Float(f) => {
                out.push(3);
                out.extend_from_slice(&canon_f64_bits(*f).to_be_bytes());
            }
            Cell::Bool(b) => {
                out.push(4);
                out.push(u8::from(*b));
            }
            Cell::SignedInteger(v) => {
                out.push(5);
                out.extend_from_slice(&v.to_be_bytes());
            }
            Cell::Date(d) => {
                out.push(6);
                out.extend_from_slice(&d.to_be_bytes());
            }
            Cell::Timestamp(t) => {
                out.push(7);
                out.extend_from_slice(&t.to_be_bytes());
            }
            Cell::Bytes(b) => {
                out.push(8);
                push_len_prefixed(&mut out, b)?;
            }
            Cell::Json(s) => {
                out.push(9);
                push_len_prefixed(&mut out, s.as_bytes())?;
            }
            Cell::Numeric(d) => {
                out.push(10);
                push_len_prefixed(&mut out, d.to_string().as_bytes())?;
            }
            Cell::Uuid(u) => {
                out.push(11);
                out.extend_from_slice(u.as_bytes());
            }
            Cell::Array(arr) => {
                out.push(12);
                match arr {
                    crate::row_codec::ArrayValue::Text(items) => {
                        out.push(0);
                        let n = u32::try_from(items.len()).map_err(|_| {
                            SqlSurfaceError::payload_too_large(
                                "set operation row key exceeds length limit",
                            )
                        })?;
                        out.extend_from_slice(&n.to_be_bytes());
                        for item in items {
                            push_len_prefixed(&mut out, item.as_bytes())?;
                        }
                    }
                    crate::row_codec::ArrayValue::Bool(items) => {
                        out.push(1);
                        let n = u32::try_from(items.len()).map_err(|_| {
                            SqlSurfaceError::payload_too_large(
                                "set operation row key exceeds length limit",
                            )
                        })?;
                        out.extend_from_slice(&n.to_be_bytes());
                        for b in items {
                            out.push(u8::from(*b));
                        }
                    }
                }
            }
            // §2.2 の型検証（重複除去を伴う演算からの `VECTOR` 列排除）で本来
            // 到達しない。破損状態を防御的に拒否する（fail-closed。XX000）。
            Cell::Vector(_) => {
                return Err(SqlSurfaceError::Internal {
                    detail: "VECTOR column reached set operation row key encoding".to_string(),
                });
            }
        }
    }
    Ok(out)
}

/// `payload_len` バイトのペイロードに対する [`push_len_prefixed`] の出力長
/// （長さ接頭辞 `u32` BE の 4 byte ＋ペイロード本体）。[`row_key_len`] が
/// `push_len_prefixed` を呼ばずに長さだけを算出するために使う。
fn len_prefixed_total_len(payload_len: usize) -> Result<usize, SqlSurfaceError> {
    u32::try_from(payload_len).map_err(|_| {
        SqlSurfaceError::payload_too_large("set operation row key exceeds length limit")
    })?;
    Ok(4usize.saturating_add(payload_len))
}

/// [`row_key`] が実際に生成するバイト列の長さを、`Vec<u8>` を確保せずに算出
/// する（PR #1105 レビュー指摘対応: 従来は `row_key` が可変長セルを複製した
/// `Vec<u8>` を確保した後で `SetOpBudget::charge` を呼んでいたため、予算上限
/// 付近で同程度の大きさの行キーを、予算検証より前に確保できてしまっていた
/// 〔security.md「長さフィールドは上限検証してからアロケーションに使う」〕。
/// 呼び出し元は本関数の戻り値を `budget.charge` へ渡してから `row_key` を
/// 呼ぶこと。`row_key` と同じ型タグ・長さ接頭辞規約に従う必要があるため、
/// 両者は変更時に必ず同時に保守する（`tests::row_key_len_matches_row_key_
/// output_length` が全 variant を横断して不一致を検出する）。
fn row_key_len(row: &ResultRow) -> Result<usize, SqlSurfaceError> {
    let mut total: usize = 0;
    for cell in &row.cells {
        let cell_len = match cell {
            // 型タグ 1 byte のみ（ペイロードなし）。
            Cell::Null => 1,
            // 型タグ 1 byte ＋ `u64`/`i64`/`f64` の 8 byte。
            Cell::Integer(_) | Cell::Float(_) | Cell::SignedInteger(_) | Cell::Timestamp(_) => {
                1 + 8
            }
            // 型タグ 1 byte ＋ `bool` の 1 byte。
            Cell::Bool(_) => 1 + 1,
            // 型タグ 1 byte ＋ `i32` の 4 byte。
            Cell::Date(_) => 1 + 4,
            // 型タグ 1 byte ＋長さ接頭辞つきペイロード。
            Cell::Text(s) => 1 + len_prefixed_total_len(s.len())?,
            Cell::Bytes(b) => 1 + len_prefixed_total_len(b.len())?,
            Cell::Json(s) => 1 + len_prefixed_total_len(s.len())?,
            Cell::Numeric(d) => 1 + len_prefixed_total_len(d.to_string().len())?,
            // 型タグ 1 byte ＋ UUID 本体（[`crate::uuid::Uuid::as_bytes`] は
            // 常に 16 byte）。
            Cell::Uuid(_) => 1 + 16,
            // 型タグ 1 byte（12）＋要素種別タグ 1 byte（0／1）＋要素数 `u32`
            // （4 byte）＋各要素のペイロード。
            Cell::Array(arr) => {
                let mut n = 1usize + 1usize + 4usize;
                match arr {
                    crate::row_codec::ArrayValue::Text(items) => {
                        for item in items {
                            n = n.saturating_add(len_prefixed_total_len(item.len())?);
                        }
                    }
                    crate::row_codec::ArrayValue::Bool(items) => {
                        n = n.saturating_add(items.len());
                    }
                }
                n
            }
            // §2.2 の型検証で本来到達しない（`row_key` と同じ防御的拒否）。
            Cell::Vector(_) => {
                return Err(SqlSurfaceError::Internal {
                    detail: "VECTOR column reached set operation row key encoding".to_string(),
                });
            }
        };
        total = total.saturating_add(cell_len);
    }
    Ok(total)
}

/// 単一の枝（`SELECT ... FROM <table> [WHERE ...]`）を束縛・実行する。可視かつ
/// `WHERE` 一致行が `core::MAX_SEARCH_K` を超える場合は `54000`（超過検出のため
/// `MAX_SEARCH_K + 1` を上限として走査する）。文全体で共有する `budget`
/// （[`SetOpBudget`]）の残り予算を枝の実行に渡し（枝単体でも残り予算を超えられ
/// ない）、実行結果のバイト量を消費として計上する。
fn eval_branch(
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    schema: &TableSchema,
    validated: &ValidatedScan,
    udfs: &UdfRegistry,
    budget: &mut SetOpBudget,
) -> Result<EvalOutcome, SqlSurfaceError> {
    let mut bound = crate::sql::parser::bind_scan(validated, schema, udfs)?;
    // §2.3: 可視行数の上限判定は「可視行数だけに依存」させる（全体 `LIMIT` の
    // 有無で成否が変わらない単純な規則）。`bound.limit`（`pub(crate)`）を
    // `MAX_SEARCH_K + 1` へ差し替えて超過を検出できるようにする。
    bound.limit = MAX_SET_OP_ROWS + 1;
    let result = crate::sql::scan::execute_scan_with_budget(
        read_txn,
        ctx,
        schema,
        &bound,
        budget.remaining(),
    )?;
    if result.rows.len() > MAX_SET_OP_ROWS {
        return Err(SqlSurfaceError::payload_too_large(
            "set operation branch exceeds the visible row limit",
        ));
    }
    budget.charge(result_bytes(&result.columns, &result.rows))?;
    let has_vector = columns_have_vector(&result.columns);
    Ok(EvalOutcome {
        columns: result.columns,
        rows: result.rows,
        has_vector,
    })
}

/// 集合演算木全体の列数・列型整合を、行走査（[`eval_tree`]・
/// [`crate::sql::scan::execute_scan_with_budget`]）より前に確定させる
/// （PR #1105 レビュー指摘対応: 従来は `eval_tree` が左右の枝を実際に走査して
/// から `check_branch_type_compatibility` を呼んでいたため、型不一致の文でも
/// 全行走査・結果確保が先行し、行数・バイト上限エラー（`54000`）が本来返す
/// べき `DatatypeMismatch`〔`42804`〕より先に成立し得た。`describe_tree` と
/// 同じ判定基準を、行を一切走査しない `bind_scan`〔`eval_branch` が実際の
/// 走査で使うのと同じ束縛〕で走査前に確定させる。戻り値は上位の呼び出し元
/// では使わず、検証のみを目的とする）。
fn validate_tree_types(
    schemas: &HashMap<String, TableSchema>,
    tree: &SetTree,
    udfs: &UdfRegistry,
) -> Result<(Vec<ColumnMeta>, bool), SqlSurfaceError> {
    match tree {
        SetTree::Branch(validated) => {
            let schema =
                schemas
                    .get(&validated.table_name)
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "schema missing for set operation branch table".to_string(),
                    })?;
            let bound = crate::sql::parser::bind_scan(validated, schema, udfs)?;
            let columns = crate::sql::describe::projected_columns(bound.projection(), schema);
            let has_vector = columns_have_vector(&columns);
            Ok((columns, has_vector))
        }
        SetTree::Op { op, left, right } => {
            let (l_columns, l_has_vector) = validate_tree_types(schemas, left, udfs)?;
            let (r_columns, r_has_vector) = validate_tree_types(schemas, right, udfs)?;
            check_branch_type_compatibility(
                *op,
                &l_columns,
                &r_columns,
                l_has_vector,
                r_has_vector,
            )?;
            Ok((l_columns, l_has_vector))
        }
    }
}

/// 構文木を後行順（postorder）で評価する。結果列名・メタデータは常に左の子
/// （左端の枝）に揃える。`budget` は文全体で共有する累計バイト予算
/// （[`SetOpBudget`]）で、枝の評価・行キー生成のいずれでも消費する。列数・
/// 列型整合の検証は本関数の呼び出し前に [`validate_tree_types`] が完結させて
/// いるため、ここでは繰り返さない（PR #1105 レビュー指摘対応。二重実装を避け、
/// 走査前検証の判定基準を単一化する）。
fn eval_tree(
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    schemas: &HashMap<String, TableSchema>,
    tree: &SetTree,
    udfs: &UdfRegistry,
    budget: &mut SetOpBudget,
) -> Result<EvalOutcome, SqlSurfaceError> {
    match tree {
        SetTree::Branch(validated) => {
            let schema =
                schemas
                    .get(&validated.table_name)
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "schema missing for set operation branch table".to_string(),
                    })?;
            eval_branch(read_txn, ctx, schema, validated, udfs, budget)
        }
        SetTree::Op { op, left, right } => {
            let l = eval_tree(read_txn, ctx, schemas, left, udfs, budget)?;
            let r = eval_tree(read_txn, ctx, schemas, right, udfs, budget)?;

            let rows = match op {
                SetOperator::UnionAll => {
                    let mut rows = l.rows;
                    rows.extend(r.rows);
                    if rows.len() > MAX_SET_OP_ROWS {
                        return Err(SqlSurfaceError::payload_too_large(
                            "set operation result exceeds the row limit",
                        ));
                    }
                    rows
                }
                SetOperator::Union => {
                    let mut seen: HashSet<Vec<u8>> = HashSet::new();
                    let mut rows = Vec::new();
                    for row in l.rows.into_iter().chain(r.rows) {
                        // 行キー生成も文全体で共有する累計バイト予算の対象
                        // （PR #1105 レビュー指摘対応。`seen` への格納で複製される
                        // 分の追加メモリを見逃さない）。長さを `row_key_len` で
                        // 先に算出して予算を確認してから `row_key` で実際に
                        // `Vec<u8>` を確保する（予算超過時に確保前に拒否する。
                        // 逆順だと上限直前で確保してから拒否することになる）。
                        budget.charge(row_key_len(&row)?)?;
                        let key = row_key(&row)?;
                        if seen.contains(&key) {
                            continue;
                        }
                        if rows.len() >= MAX_SET_OP_ROWS {
                            return Err(SqlSurfaceError::payload_too_large(
                                "set operation result exceeds the row limit",
                            ));
                        }
                        seen.insert(key);
                        rows.push(row);
                    }
                    rows
                }
                SetOperator::Intersect | SetOperator::Except => {
                    let mut right_keys: HashSet<Vec<u8>> = HashSet::new();
                    for row in &r.rows {
                        // 順序は上と同じ理由（長さ算出 → 予算確認 → 確保）。
                        budget.charge(row_key_len(row)?)?;
                        let key = row_key(row)?;
                        right_keys.insert(key);
                    }
                    let keep_if_present = matches!(op, SetOperator::Intersect);
                    let mut seen: HashSet<Vec<u8>> = HashSet::new();
                    let mut rows = Vec::new();
                    for row in l.rows.into_iter() {
                        // 順序は上と同じ理由（長さ算出 → 予算確認 → 確保）。
                        budget.charge(row_key_len(&row)?)?;
                        let key = row_key(&row)?;
                        let present = right_keys.contains(&key);
                        if present != keep_if_present || seen.contains(&key) {
                            continue;
                        }
                        if rows.len() >= MAX_SET_OP_ROWS {
                            return Err(SqlSurfaceError::payload_too_large(
                                "set operation result exceeds the row limit",
                            ));
                        }
                        seen.insert(key);
                        rows.push(row);
                    }
                    rows
                }
            };

            Ok(EvalOutcome {
                columns: l.columns,
                rows,
                has_vector: l.has_vector,
            })
        }
    }
}

/// [`crate::sql::allowlist::ValidatedSetOperation`] を実行する（`core.rs::
/// EngineCore::execute_validated_in_session` の `Statement::SetOperation`
/// アーム・session-less `execute_sql` の同アームから呼ばれる）。`schemas` は
/// 呼び出し元が単一の `read_txn`（同一スナップショット）上で解決済みのものを
/// 渡す（`EngineCore::read_txn_with_schemas`）。
pub(crate) fn execute(
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    schemas: &HashMap<String, TableSchema>,
    tree: &SetTree,
    udfs: &UdfRegistry,
    limit: Option<u32>,
) -> Result<QueryResult, SqlSurfaceError> {
    execute_with_budget(
        read_txn,
        ctx,
        schemas,
        tree,
        udfs,
        limit,
        crate::arena::MAX_ARENA_TOTAL_BYTES,
    )
}

/// [`execute`] の本体。`budget_cap` は文全体で共有する累計バイト予算
/// （[`SetOpBudget`]）の上限。[`execute`] は既定値
/// （[`crate::arena::MAX_ARENA_TOTAL_BYTES`]）を渡すだけの薄いラッパーで、
/// 小さい上限を注入できる回帰テスト（本モジュール内の `tests`）専用に分離して
/// いる（`sql::scan::execute_scan`／`execute_scan_with_budget` と同じ設計判断）。
pub(crate) fn execute_with_budget(
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    schemas: &HashMap<String, TableSchema>,
    tree: &SetTree,
    udfs: &UdfRegistry,
    limit: Option<u32>,
    budget_cap: usize,
) -> Result<QueryResult, SqlSurfaceError> {
    // PR #1105 レビュー指摘対応: 全体 `LIMIT` の範囲検証（`1..=MAX_SEARCH_K`）を
    // 全枝の走査・合成より前に行う（[`describe_columns`] と同じ検証を共有し、
    // Execute／Describe の受理・拒否を一致させる。従来は `eval_tree` 完了後の
    // 切り詰め直前でのみ検証しており、範囲外の `LIMIT` でも全枝を走査・合成し
    // 終えてから拒否していた）。
    let validated_limit = match limit {
        Some(n) => Some(crate::sql::parser::validate_search_limit(n)?),
        None => None,
    };
    // PR #1105 レビュー指摘対応: 列数・列型整合（`DatatypeMismatch`・`42804`）を
    // 全枝の走査・合成より前に確定させる（[`validate_tree_types`] のドキュメン
    // テーションコメント参照）。従来は `eval_tree` が左右の枝を実際に走査して
    // から検証していたため、型不一致の文でも全行走査・結果確保が先行し、
    // 行数・バイト上限エラー（`54000`）が `42804` に先行し得た。
    validate_tree_types(schemas, tree, udfs)?;
    let mut budget = SetOpBudget::new(budget_cap);
    let outcome = eval_tree(read_txn, ctx, schemas, tree, udfs, &mut budget)?;
    // §2.4: 上限判定（`MAX_SET_OP_ROWS`）は全体 `LIMIT` による切り詰めより前に
    // 完結している（`eval_tree` が既に検証済み）。
    let rows = match validated_limit {
        Some(n) => outcome.rows.into_iter().take(n).collect(),
        None => outcome.rows,
    };
    Ok(QueryResult {
        columns: outcome.columns,
        rows,
    })
}

/// Describe（拡張クエリプロトコル）向けに、検索本体を実行せず結果列メタデータ
/// だけを導出する。`bind_scan_with_dummy_flags` で枝を束縛し、[`execute`] と
/// 同じ型整合検証（§2.2）を行う（行走査は一切行わない）。`limit` は全体
/// `LIMIT`（任意）で、[`execute`] と同じ範囲検証（`1..=MAX_SEARCH_K`）を行う
/// （PR #1105 レビュー指摘対応: 従来は Describe が全体 `LIMIT` を一切検証せず、
/// Execute では拒否される範囲外の値が Describe だけ受理されてしまっていた）。
pub(crate) fn describe_columns(
    schemas: &HashMap<String, TableSchema>,
    tree: &SetTree,
    udfs: &UdfRegistry,
    dummy_equality_flags: &[bool],
    limit: Option<u32>,
) -> Result<Vec<ColumnMeta>, SqlSurfaceError> {
    if let Some(n) = limit {
        crate::sql::parser::validate_search_limit(n)?;
    }
    Ok(describe_tree(schemas, tree, udfs, dummy_equality_flags)?.0)
}

fn describe_tree(
    schemas: &HashMap<String, TableSchema>,
    tree: &SetTree,
    udfs: &UdfRegistry,
    dummy_equality_flags: &[bool],
) -> Result<(Vec<ColumnMeta>, bool), SqlSurfaceError> {
    match tree {
        SetTree::Branch(validated) => {
            let schema =
                schemas
                    .get(&validated.table_name)
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "schema missing for set operation branch table".to_string(),
                    })?;
            let bound = crate::sql::parser::bind_scan_with_dummy_flags(
                validated,
                schema,
                udfs,
                dummy_equality_flags,
            )?;
            let columns = crate::sql::describe::projected_columns(bound.projection(), schema);
            let has_vector = columns_have_vector(&columns);
            Ok((columns, has_vector))
        }
        SetTree::Op { op, left, right } => {
            let (l_columns, l_has_vector) =
                describe_tree(schemas, left, udfs, dummy_equality_flags)?;
            let (r_columns, r_has_vector) =
                describe_tree(schemas, right, udfs, dummy_equality_flags)?;
            check_branch_type_compatibility(
                *op,
                &l_columns,
                &r_columns,
                l_has_vector,
                r_has_vector,
            )?;
            Ok((l_columns, l_has_vector))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnDef, ColumnType, TableSchema};
    use crate::recovery::required_op_id::OperationId;
    use crate::row_codec::Value;
    use crate::sql::allowlist::{Projection, SetOperator, ValidatedScan};
    use crate::sql::udf_call::UdfRegistry;
    use crate::storage::{Storage, Visibility};
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
    use redb::ReadableDatabase;

    fn text_schema(name: &str) -> TableSchema {
        TableSchema::new(
            name,
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        )
    }

    fn insert_text_row(storage: &Storage, table: &str, tenant_ctx: &PolicyContext, body: &str) {
        let op_id = OperationId::parse(&format!("seed-{table}")).expect("valid operation_id");
        crate::tenant::insert_typed_row(
            storage,
            table,
            tenant_ctx,
            1,
            Visibility::Public,
            &[Value::Vector(vec![0.0, 0.0]), Value::Text(body.to_string())],
            &op_id,
        )
        .expect("insert row");
    }

    fn text_branch(table: &str) -> SetTree {
        SetTree::Branch(Box::new(ValidatedScan {
            table_name: table.to_string(),
            projection: Projection::Columns(vec!["body".to_string()]),
            where_predicates: Vec::new(),
            limit: crate::core::MAX_SEARCH_K as u32,
            order_by: Vec::new(),
            offset: 0,
            window_items: Vec::new(),
        }))
    }

    /// PR #1105 レビュー指摘の回帰: 各枝が独立に既定予算（1 GiB）を使うのではなく、
    /// 文全体で 1 つの累計バイト予算を共有すること。1 枝だけなら収まるが、2 枝を
    /// 合成すると超える上限を注入し、`UNION ALL`（重複除去なし。行キー生成の影響を
    /// 排除して枝単体の計上だけを確認する）が `54000` になることを確認する。
    #[test]
    fn shared_byte_budget_rejects_when_combined_branches_exceed_the_injected_cap() {
        let path = unique_db_path("set-op-shared-budget-reject");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        storage.create_table(&text_schema("a")).expect("create a");
        storage.create_table(&text_schema("b")).expect("create b");
        let tenant_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let body = "x".repeat(4096);
        insert_text_row(&storage, "a", &tenant_ctx, &body);
        insert_text_row(&storage, "b", &tenant_ctx, &body);

        let tree = SetTree::Op {
            op: SetOperator::UnionAll,
            left: Box::new(text_branch("a")),
            right: Box::new(text_branch("b")),
        };
        let mut table_names = Vec::new();
        collect_branch_tables(&tree, &mut table_names);

        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut schemas = HashMap::new();
        schemas.insert("a".to_string(), text_schema("a"));
        schemas.insert("b".to_string(), text_schema("b"));
        let udfs = UdfRegistry::default();

        // 1 枝分（約 4096 バイト＋構造体オーバーヘッド）は収まるが、2 枝合計は
        // 超える上限を注入する。
        let small_cap = 4096 + 512;
        let err = execute_with_budget(
            &read_txn,
            &tenant_ctx,
            &schemas,
            &tree,
            &udfs,
            None,
            small_cap,
        )
        .expect_err("combined branch bytes must exceed the injected shared budget");
        assert_eq!(err.wire_code(), "54000");
    }

    /// 上のちょうど対照: 2 枝合計が収まる上限を注入すれば成功することを確認する
    /// （回帰テストが常に失敗するだけの壊れた検証になっていないことの確認）。
    #[test]
    fn shared_byte_budget_succeeds_when_combined_branches_fit_the_injected_cap() {
        let path = unique_db_path("set-op-shared-budget-accept");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        storage.create_table(&text_schema("a")).expect("create a");
        storage.create_table(&text_schema("b")).expect("create b");
        let tenant_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let body = "x".repeat(4096);
        insert_text_row(&storage, "a", &tenant_ctx, &body);
        insert_text_row(&storage, "b", &tenant_ctx, &body);

        let tree = SetTree::Op {
            op: SetOperator::UnionAll,
            left: Box::new(text_branch("a")),
            right: Box::new(text_branch("b")),
        };
        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut schemas = HashMap::new();
        schemas.insert("a".to_string(), text_schema("a"));
        schemas.insert("b".to_string(), text_schema("b"));
        let udfs = UdfRegistry::default();

        let generous_cap = 1024 * 1024;
        let result = execute_with_budget(
            &read_txn,
            &tenant_ctx,
            &schemas,
            &tree,
            &udfs,
            None,
            generous_cap,
        )
        .expect("combined branch bytes must fit the generous injected budget");
        assert_eq!(result.rows.len(), 2);
    }

    /// PR #1105 レビュー指摘の回帰: 行キー生成（`UNION` の重複除去）も共有予算の
    /// 対象であること。2 枝分の行データ自体は収まるが、行キーの複製分を追加すると
    /// 超える上限では `UNION`（重複除去あり）が `54000` になることを確認する
    /// （`UNION ALL` は行キーを生成しないため同じ上限でも成功するはずだが、本
    /// テストは行キー計上の有無に絞って確認するため `UNION` 側のみを検証する）。
    #[test]
    fn shared_byte_budget_also_charges_row_key_generation() {
        let path = unique_db_path("set-op-shared-budget-row-key");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        storage.create_table(&text_schema("a")).expect("create a");
        storage.create_table(&text_schema("b")).expect("create b");
        let tenant_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        // UNION の重複除去では左右が別内容でないと両方が結果に残らないが、行キー
        // 生成自体は一致・不一致に関わらず両方の行に対して行うため、内容は異なる
        // ものにしておく。
        insert_text_row(&storage, "a", &tenant_ctx, &"x".repeat(4096));
        insert_text_row(&storage, "b", &tenant_ctx, &"y".repeat(4096));

        let tree = SetTree::Op {
            op: SetOperator::Union,
            left: Box::new(text_branch("a")),
            right: Box::new(text_branch("b")),
        };
        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut schemas = HashMap::new();
        schemas.insert("a".to_string(), text_schema("a"));
        schemas.insert("b".to_string(), text_schema("b"));
        let udfs = UdfRegistry::default();

        // 2 枝分の行データ（約 8192 バイト＋構造体オーバーヘッド）はちょうど収まるが、
        // 行キー（各行の複製で約 8192 バイト追加）までは収まらない上限。
        let cap_without_row_key_room = 4096 * 2 + 1024;
        let err = execute_with_budget(
            &read_txn,
            &tenant_ctx,
            &schemas,
            &tree,
            &udfs,
            None,
            cap_without_row_key_room,
        )
        .expect_err("row key generation must be charged against the shared budget");
        assert_eq!(err.wire_code(), "54000");
    }

    /// PR #1105 レビュー指摘の回帰: 全体 `LIMIT` の範囲外検証（`22000`）は全枝の
    /// 走査より前に行う。存在しないテーブルを枝に含めても（走査すれば `XX000`
    /// 相当の内部エラーになるはずの構成）、範囲外 `LIMIT` が先に `22000` で拒否
    /// することを確認する。
    #[test]
    fn out_of_range_limit_is_rejected_before_branches_are_scanned() {
        let path = unique_db_path("set-op-limit-before-scan");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        let tenant_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let tree = text_branch("missing_table");
        let read_txn = storage.db().begin_read().expect("begin_read");
        // 意図的にスキーマを解決しない（走査されれば `schemas.get` が `None` を
        // 返し `Internal` エラーになる構成）。
        let schemas: HashMap<String, TableSchema> = HashMap::new();
        let udfs = UdfRegistry::default();

        let err = execute(&read_txn, &tenant_ctx, &schemas, &tree, &udfs, Some(0))
            .expect_err("LIMIT 0 must be rejected before any branch is scanned");
        assert_eq!(err.wire_code(), "22000");

        let err = execute(
            &read_txn,
            &tenant_ctx,
            &schemas,
            &tree,
            &udfs,
            Some(crate::core::MAX_SEARCH_K as u32 + 1),
        )
        .expect_err("LIMIT over MAX_SEARCH_K must be rejected before any branch is scanned");
        assert_eq!(err.wire_code(), "22000");
    }

    /// PR #1105 レビュー指摘の回帰: Describe 経路（`describe_columns`）も Execute
    /// と同じ全体 `LIMIT` 範囲検証を行う（従来は検証していなかった）。
    #[test]
    fn describe_columns_rejects_out_of_range_limit_like_execute() {
        let tree = text_branch("a");
        let schemas: HashMap<String, TableSchema> = HashMap::new();
        let udfs = UdfRegistry::default();

        let err = describe_columns(&schemas, &tree, &udfs, &[], Some(0))
            .expect_err("LIMIT 0 must be rejected by describe_columns");
        assert_eq!(err.wire_code(), "22000");

        let err = describe_columns(
            &schemas,
            &tree,
            &udfs,
            &[],
            Some(crate::core::MAX_SEARCH_K as u32 + 1),
        )
        .expect_err("LIMIT over MAX_SEARCH_K must be rejected by describe_columns");
        assert_eq!(err.wire_code(), "22000");
    }

    fn int_schema(name: &str) -> TableSchema {
        TableSchema::new(
            name,
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("score", ColumnType::Integer, false),
            ],
        )
    }

    fn int_branch(table: &str) -> SetTree {
        SetTree::Branch(Box::new(ValidatedScan {
            table_name: table.to_string(),
            projection: Projection::Columns(vec!["score".to_string()]),
            where_predicates: Vec::new(),
            limit: crate::core::MAX_SEARCH_K as u32,
            order_by: Vec::new(),
            offset: 0,
            window_items: Vec::new(),
        }))
    }

    /// PR #1105 レビュー指摘の回帰（set_op.rs:353）: 列型不一致
    /// （`DatatypeMismatch`・`42804`）の判定は、`eval_tree` が左右の枝を実際に
    /// 走査するより前に確定していること。枝 `a`（TEXT 列）には実データを
    /// 投入し、枝 `b`（INTEGER 列）とは列型が一致しない構成にした上で、
    /// 予算上限を `0` にする（`a` を少しでも走査すれば必ず `54000` になる
    /// 構成）。従来（列型検証が `eval_tree` の走査後）は `54000` が先に成立して
    /// いたが、修正後は走査前の [`validate_tree_types`] が `42804` を確定させる
    /// ため、`54000` に先行されない。
    #[test]
    fn type_mismatch_is_rejected_before_any_branch_is_scanned_even_with_a_zero_budget() {
        let path = unique_db_path("set-op-type-mismatch-before-scan");
        let storage = Storage::open(&path).expect("open storage");
        let _guard = CleanupGuard(path);
        storage.create_table(&text_schema("a")).expect("create a");
        let tenant_ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        insert_text_row(&storage, "a", &tenant_ctx, "some text");

        let tree = SetTree::Op {
            op: SetOperator::UnionAll,
            left: Box::new(text_branch("a")),
            right: Box::new(int_branch("b")),
        };
        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut schemas = HashMap::new();
        schemas.insert("a".to_string(), text_schema("a"));
        schemas.insert("b".to_string(), int_schema("b"));
        let udfs = UdfRegistry::default();

        let err = execute_with_budget(&read_txn, &tenant_ctx, &schemas, &tree, &udfs, None, 0)
            .expect_err("type mismatch must be rejected even with a zero-byte budget");
        assert_eq!(err.wire_code(), "42804");
    }

    /// PR #1105 レビュー指摘の回帰（set_op.rs:393・415・423）: [`row_key_len`]
    /// が算出する長さは、[`row_key`] が実際に確保する `Vec<u8>` の長さと
    /// 全 variant で一致すること（両者は変更時に必ず同時に保守する必要が
    /// あるため、この一致検証が乖離を検出する）。
    #[test]
    fn row_key_len_matches_row_key_output_length() {
        let uuid = crate::uuid::parse_uuid_text("550e8400-e29b-41d4-a716-446655440000")
            .expect("valid uuid literal");
        let decimal = crate::numeric::parse_literal_exact("123.45").expect("valid decimal literal");
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![
                Cell::Null,
                Cell::Integer(42),
                Cell::Text("hello world".to_string()),
                Cell::Float(3.5),
                Cell::Bool(true),
                Cell::SignedInteger(-7),
                Cell::Date(19_000),
                Cell::Timestamp(1_700_000_000_000_000),
                Cell::Bytes(vec![1, 2, 3, 4, 5]),
                Cell::Json("{\"a\":1}".to_string()),
                Cell::Numeric(decimal),
                Cell::Uuid(uuid),
                Cell::Array(crate::row_codec::ArrayValue::Text(vec![
                    "x".to_string(),
                    "yz".to_string(),
                ])),
                Cell::Array(crate::row_codec::ArrayValue::Bool(vec![true, false, true])),
            ],
        };

        let computed_len = row_key_len(&row).expect("row_key_len should succeed");
        let actual_len = row_key(&row).expect("row_key should succeed").len();
        assert_eq!(
            computed_len, actual_len,
            "row_key_len must predict the exact byte length row_key allocates"
        );
    }
}
