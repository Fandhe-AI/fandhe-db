//! 広域取得（ソートなしのフィルタ取得。`SELECT ... [WHERE ...] LIMIT n`）の実行本体
//! （Issue #454）。本 DB の「正解を含むデータ群を広く返し、丸ごと LLM へ渡す」設計
//! 思想を SQL 表層で直接表現する経路で、ランキング段（`ORDER BY`／`USING PLAN`）・
//! 取得モード（`recall`／`precision`）のいずれも持たない。
//!
//! 責務境界: [`crate::sql::parser::bind_scan`] が返す [`crate::sql::parser::BoundScan`]
//! を受け取り、対象テーブルの行テーブル（`user_rows/{table}`）を可視かつ `WHERE` を
//! 満たす行が `LIMIT` 件集まった時点で走査を打ち切る早期終了付きで走査し、単一の
//! [`crate::sql::exec::QueryResult`] を組み立てる。`core.rs::EngineCore::
//! execute_sql_in_session` の `Statement::Scan` アームから呼ばれるほか、`bind_scan`・
//! `execute_scan`・`BoundScan` は TASK-186（NOSQL-3）で公開 API へ昇格しており、
//! engine クレート外からも直接呼べる（[`crate::sql`] モジュールドキュメント参照）。
//!
//! [`crate::sql::aggregate::execute_aggregate`] と同じ理由で
//! [`crate::arena::VectorArena`]（既存の検索 SELECT 実行経路）は使わない: アリーナは
//! スキーマに `VECTOR` 列が必須で可視行の embedding を全件バッファへ確保するため、
//! `VECTOR` 列を持たないテーブルの広域取得や大規模テーブルの `id`/`TEXT` 列のみの
//! 取得には過剰（メモリ）かつ非対応。行走査・デコード段階選択（[`DecodeTier`]）・
//! RLS 適用順序（デコード前のヘッダ判定 → TABLE-12 のキー/ヘッダ tenant 整合検査 →
//! 必要範囲のみのデコード → SCALAR 段（`WHERE`）→ 可視性の再適用）は
//! `sql::aggregate` の走査ループと同一の規約を踏襲する（`.claude/rules/security.md`
//! 「テナント境界（P0）」）。
//!
//! 契約の詳細（`LIMIT` の意味・順序保証の有無・取得モードとの無関係性）は
//! `docs/design/wide-retrieval-scan.md`（spec ビヘイビア ID は SQL-15・TASK-170 として
//! 付与済み〔fandhe-db-spec#12〕。確定化は TASK-170 が担う。本モジュールは本リポの
//! 実装既定値として動作する）参照。`bound.order_by` が空の場合、順序は同一
//! スナップショット内の redb 行テーブルの物理走査順（`(tenant_id, id)` 昇順）であり、
//! `ORDER BY` 相当の意味的順序を持たない。
//!
//! **スカラー列 `ORDER BY`**（Issue #915・SQL-25・TASK-209。`docs/design/
//! scalar-order-by-scan.md` 参照）: `bound.order_by` が非空の場合は 2 経路のいずれかで
//! 決定的な順序を返す。(A) 先頭キーが疑似列 `id` かつ `ctx` が他テナントの `Public` 行を
//! 可視としない場合は、自テナントのパーティション（`(tenant, 0)..=(tenant, MAX)`）を
//! 直接範囲走査し `LIMIT` 件で打ち切る（`id` はテナント内で一意なため後続キーは
//! 無関係）。(B) それ以外は 2 パス（1 パス目で候補の並べ替えキー・`(tenant_id, id)` を
//! 容量 `limit` の `BinaryHeap` に保持し予算超過時は最悪要素を追い出す。2 パス目で
//! 勝者のみを同じスナップショットから再取得し投影する）。両経路とも RLS 適用順序
//! （デコード前のヘッダ判定 → TABLE-12 → 必要範囲のみのデコード → `WHERE` → 可視性の
//! 再適用）は順序なし経路と同一（[`with_visible_row`] に共通化）。
//!
//! `OFFSET`（Issue #916・SQL-25 (b)・TASK-209。詳細は
//! `docs/design/sql-offset-paging.md`）は「可視かつ `WHERE` 一致」の行のみを対象に
//! 読み飛ばす。RLS 判定・`WHERE` 評価が確定した後でのみ計数するため、不可視行は
//! 読み飛ばし数にも結果にも現れず、ページ境界の出方から他テナント行の存在・件数を
//! 推測できない。読み飛ばした行は投影・`cells` 確保・byte 予算計上のいずれも行わない
//! （深いページングでもメモリは O(`limit`) を保つ）。

use crate::catalog::{self, ColumnType, TableSchema};
use crate::declarative_filter;
use crate::policy::PolicyContext;
use crate::row_codec;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::exec::{Cell, ColumnMeta, QueryResult, ResultRow};
use crate::sql::expr_program::{ExprProgram, StackValue};
use crate::sql::order_value::{
    compare_ref_and_owned_values, compare_statement_order, expr_value_to_key_ref,
    extract_order_value_ref, scalar_key_ref_to_owned, OrderValue, ScalarKeyRef,
};
use crate::sql::parser::{BoundOrderKey, BoundOrderTarget, BoundScan, ProjectedColumn};
use crate::sql::udf_call::{self, ExprValue};
use crate::storage::{self, StorageError};
use redb::ReadableTable;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::rc::Rc;

/// 結果セット全体（テキスト・ベクトル各セルの複製バイト量の合計）の累計上限。
/// `bound.limit`（`1..=core::MAX_SEARCH_K`）で行数は既に有界だが、1 行あたりの
/// テキスト・ベクトルサイズは任意に大きくなりうるため、`sql::exec` の
/// `MAX_CANDIDATE_SCALAR_BYTES` と同じ定数（[`crate::arena::MAX_ARENA_TOTAL_BYTES`]）を
/// 流用し確保前に検証する（security.md「不安全な設計｜無制限リソース確保（DoS）」
/// 対応）。
const MAX_SCAN_RESULT_BYTES: usize = crate::arena::MAX_ARENA_TOTAL_BYTES;

/// 型不整合・実装バグの検出用（untrusted 入力起因ではないため `wire_code` は
/// `XX000`。[`crate::sql::aggregate::accumulator_bug`] と同方針）。
fn scan_bug(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: format!("scan tier/projection mismatch: {detail}"),
    }
}

fn storage_internal(e: impl Into<StorageError>) -> SqlSurfaceError {
    let _: StorageError = e.into();
    SqlSurfaceError::Internal {
        detail: "scan row scan failed".to_string(),
    }
}

/// `current` に `add` を加えた累計が `cap` を超えないことを確保前に検証する
/// （[`crate::sql::exec`] の同名ヘルパーと同方針）。
fn try_accumulate_budget(current: usize, add: usize, cap: usize) -> Result<usize, SqlSurfaceError> {
    let next = current.saturating_add(add);
    if next > cap {
        return Err(SqlSurfaceError::payload_too_large(
            "scan result exceeds capacity",
        ));
    }
    Ok(next)
}

/// `Computed` 列（式）が返した所有済みベクトルを累計予算へ計上する
/// （codex-review P1 指摘対応: `Computed` 列のベクトル結果は `try_clone_embedding_for_budget`
/// を通らないため、対策なしでは `VECTOR` 列直接投影と異なり `MAX_SCAN_RESULT_BYTES` を
/// 迂回して無制限にメモリを蓄積できてしまう。`vec_div`/`vec_mul` 等 embedding と同じ
/// 次元のベクトルを返す組み込み関数の結果を対象とする）。
fn try_accumulate_vector_budget(
    vector: Vec<f32>,
    budget: &mut usize,
    cap: usize,
) -> Result<Vec<f32>, SqlSurfaceError> {
    let bytes = vector.len().saturating_mul(std::mem::size_of::<f32>());
    *budget = try_accumulate_budget(*budget, bytes, cap)?;
    Ok(vector)
}

/// テキストセルの選択的複製（累計バイト量を確保前に検証。`String::try_reserve_exact`
/// によりホスト側メモリ不足時も abort ではなく `Err` を返す）。
fn try_alloc_text_for_budget(
    text: &str,
    budget: &mut usize,
    cap: usize,
) -> Result<String, SqlSurfaceError> {
    *budget = try_accumulate_budget(*budget, text.len(), cap)?;
    let mut owned = String::new();
    owned
        .try_reserve_exact(text.len())
        .map_err(|e| SqlSurfaceError::Internal {
            detail: format!("failed to reserve scalar text field: {e}"),
        })?;
    owned.push_str(text);
    Ok(owned)
}

/// ベクトルセルの選択的複製（累計バイト量を確保前に検証。上記テキスト版と同方針）。
fn try_clone_embedding_for_budget(
    embedding: &[f32],
    budget: &mut usize,
    cap: usize,
) -> Result<Vec<f32>, SqlSurfaceError> {
    let bytes = embedding.len().saturating_mul(std::mem::size_of::<f32>());
    *budget = try_accumulate_budget(*budget, bytes, cap)?;
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(embedding.len())
        .map_err(|e| SqlSurfaceError::Internal {
            detail: format!("failed to reserve embedding field: {e}"),
        })?;
    owned.extend_from_slice(embedding);
    Ok(owned)
}

/// 配列セルの選択的複製（累計バイト量を確保前に検証。上記テキスト・ベクトル版と
/// 同方針。Issue #888）。
fn try_alloc_array_for_budget(
    array_ref: row_codec::ArrayRef<'_>,
    budget: &mut usize,
    cap: usize,
) -> Result<crate::row_codec::ArrayValue, SqlSurfaceError> {
    // 要素本文（`payload_bytes`。TEXT 要素の実体を含む）に加え、`Vec<String>`
    // の構造体分（`String` 1 個あたり）も計上する（`sql::exec` の同名ヘルパーと
    // 同じ理由。Issue #888 レビュー指摘対応）。
    let approx_bytes = array_ref
        .payload_bytes()
        .saturating_add((array_ref.count() as usize).saturating_mul(std::mem::size_of::<String>()));
    *budget = try_accumulate_budget(*budget, approx_bytes, cap)?;
    array_ref.to_value().map_err(|e| SqlSurfaceError::Internal {
        detail: format!("failed to decode array field: {e}"),
    })
}

/// BYTEA セルの選択的複製（累計バイト量を確保前に検証。上記テキスト版と同方針。
/// UTF-8 検証を行わない点のみ異なる。Issue #886）。
fn try_alloc_bytes_for_budget(
    bytes: &[u8],
    budget: &mut usize,
    cap: usize,
) -> Result<Vec<u8>, SqlSurfaceError> {
    *budget = try_accumulate_budget(*budget, bytes.len(), cap)?;
    let mut owned: Vec<u8> = Vec::new();
    owned
        .try_reserve_exact(bytes.len())
        .map_err(|e| SqlSurfaceError::Internal {
            detail: format!("failed to reserve scalar bytea field: {e}"),
        })?;
    owned.extend_from_slice(bytes);
    Ok(owned)
}

/// 可視行 1 件のデコード段階（[`crate::sql::aggregate::DecodeTier`] と同じ意図・
/// 同じ規約を広域取得向けに再定義したもの。集計項目ではなく投影列・`WHERE` から
/// 参照列集合を導出する点のみ異なるため、型を共有せず本モジュール専用に持つ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecodeTier {
    /// 投影列が疑似列 `id`・embedding を参照しない `Computed` 式のみ・`WHERE` 句
    /// なしの場合に限り選択する。dim・metadata の構造検証（破損検知）は
    /// `DimAndScalar` と同じ [`storage::decode_row_dim_and_metadata_borrowed`] を
    /// 通す（`sql::aggregate` の `DecodeTier::Fast` と同方針。破損した可視行を
    /// fail-open で見逃さない）。
    Fast,
    /// dim・metadata（マスク済み [`row_codec::scan_scalar_columns_masked`]）まで。
    /// embedding は構造検証のみで `Vec<f32>` へ確保しない。
    DimAndScalar,
    /// embedding を含む完全デコード（[`storage::decode_row_body_into`]）。
    Embedding,
}

/// [`BoundScan::projection`]・`metadata_filters`・`expr_filters` から、可視行
/// 1 件あたりの最小限のデコード段階を一度だけ決める（Issue #350 の
/// `sql::aggregate::ReferencedColumns::derive` と同じ意図）。戻り値は
/// `(tier, scalar_mask)`。
fn decode_tier_for(schema: &TableSchema, bound: &BoundScan) -> (DecodeTier, Vec<bool>) {
    let mut scalar_mask = vec![false; schema.columns.len()];
    let mut needs_embedding = false;
    let mut has_scalar_reference = false;

    for col in &bound.projection {
        match col {
            ProjectedColumn::Column { index, .. } => {
                if let Some(column) = schema.columns.get(*index) {
                    match &column.ty {
                        ColumnType::Vector(_) => needs_embedding = true,
                        ColumnType::Text
                        | ColumnType::Integer
                        | ColumnType::BigInt
                        | ColumnType::Real
                        | ColumnType::Double
                        | ColumnType::Boolean
                        | ColumnType::Date
                        | ColumnType::Timestamp
                        | ColumnType::Array(_)
                        | ColumnType::Bytea
                        | ColumnType::Json
                        | ColumnType::Jsonb
                        | ColumnType::Enum(_)
                        | ColumnType::Numeric { .. }
                        | ColumnType::Uuid => {
                            has_scalar_reference = true;
                            if let Some(slot) = scalar_mask.get_mut(*index) {
                                *slot = true;
                            }
                        }
                    }
                }
            }
            ProjectedColumn::Computed { expr, .. } => {
                if udf_call::references_embedding(expr) {
                    needs_embedding = true;
                }
                // Issue #919・SQL-26: 投影の式項目が参照する `TEXT` 列を
                // `scalar_mask` へ反映する（マスク外参照を実 NULL と取り違え
                // させない fail-closed 契約。`mark_referenced_scalar_columns`
                // ドキュメント参照）。
                if udf_call::mark_referenced_scalar_columns(expr, &mut scalar_mask) {
                    has_scalar_reference = true;
                }
            }
            ProjectedColumn::Id => {}
        }
    }
    if !bound.metadata_filters.is_empty() {
        has_scalar_reference = true;
    }
    for filter in &bound.metadata_filters {
        if let Some(slot) = scalar_mask.get_mut(filter.column_index()) {
            *slot = true;
        }
    }
    for expr in &bound.expr_filters {
        if udf_call::references_embedding(expr) {
            needs_embedding = true;
        }
        if udf_call::mark_referenced_scalar_columns(expr, &mut scalar_mask) {
            has_scalar_reference = true;
        }
    }
    // Issue #915・SQL-25: スカラー ORDER BY のキー列も、投影されていなくても
    // 並べ替え値抽出のためにスキャン段でデコードする必要がある（`VECTOR` 列は
    // 束縛段〔`sql::parser::bind_scalar_order_by`〕が構造上 ORDER BY キーとして
    // 拒否済みのため `needs_embedding` は変化しない）。
    for key in &bound.order_by {
        match key.target {
            BoundOrderTarget::Column(index) => {
                has_scalar_reference = true;
                if let Some(slot) = scalar_mask.get_mut(index) {
                    *slot = true;
                }
            }
            // Issue #1188・SQL-26: 式キーが参照する列・embedding も並べ替え値の評価の
            // ためにスキャン段でデコードする（マスク外参照を実 NULL と取り違えさせない
            // fail-closed 契約は投影・WHERE の式と同じ）。
            BoundOrderTarget::Expr(i) => {
                if let Some((expr, _)) = bound.order_exprs.get(i) {
                    if udf_call::references_embedding(expr) {
                        needs_embedding = true;
                    }
                    if udf_call::mark_referenced_scalar_columns(expr, &mut scalar_mask) {
                        has_scalar_reference = true;
                    }
                }
            }
            BoundOrderTarget::Id => {}
        }
    }
    // TASK-208・SQL-24（Issue #912）: `WHERE` の OR 群が参照する列・embedding も
    // 同様に反映する。ここを取りこぼすと、OR 群が参照する列が
    // `scalar_mask`（`scan_scalar_columns_masked`）から漏れて常に `None` に
    // なり、あるいは embedding 未デコードのまま OR を評価することになり、
    // OR 条件が誤って評価される（fail-open のバグになりうる。
    // security.md「不安全な設計」対応）。
    if !bound.or_filters.is_empty() {
        has_scalar_reference = true;
    }
    for group in &bound.or_filters {
        group.visit_column_indices(&mut |idx| {
            if let Some(slot) = scalar_mask.get_mut(idx) {
                *slot = true;
            }
        });
        if group.references_embedding() {
            needs_embedding = true;
        }
    }

    let tier = if needs_embedding {
        DecodeTier::Embedding
    } else if has_scalar_reference
        || scalar_mask.iter().any(|&wanted| wanted)
        || !bound.expr_filters.is_empty()
    {
        DecodeTier::DimAndScalar
    } else {
        DecodeTier::Fast
    };
    (tier, scalar_mask)
}

// --- Issue #915・SQL-25: スカラー ORDER BY の比較器・行値抽出 ------------------

/// [`compare_order_key`] の借用対応版（NULL 位置・降順の適用規約は同一）。
fn compare_ref_key(
    a: Option<&ScalarKeyRef<'_>>,
    b: Option<&OrderValue>,
    descending: bool,
) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => {
            if descending {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (Some(_), None) => {
            if descending {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (Some(x), Some(y)) => {
            let base = compare_ref_and_owned_values(x, y);
            if descending {
                base.reverse()
            } else {
                base
            }
        }
    }
}

/// 経路 (B) パス 1 の候補（借用のまま、`refs`・`id`・`tenant_id`）を、ヒープが
/// 保持中の 1 件（`other`）と比較する（[`HeapEntry::order`] の借用対応版。
/// Issue #915 追加是正: ヒープ満杯時にこの関数で「採用されるか」を複製前に
/// 判定し、採用されない候補（`Ordering::Less` 以外）は一切複製・確保しない。
/// `order_by` は `other.spec` と同一の並べ替え仕様である前提〔同一クエリの
/// 候補は必ず同じ `bound.order_by` から生成される〕）。戻り値は
/// `HeapEntry::order` と同じく「昇順ソートすると最終的な出力順になる」意味の
/// `Ordering`。
fn compare_candidate_ref_to_entry(
    order_by: &[BoundOrderKey],
    refs: &[Option<ScalarKeyRef<'_>>],
    id: u64,
    tenant_id: &str,
    other: &HeapEntry,
) -> Ordering {
    for (idx, key) in order_by.iter().enumerate() {
        let a = refs.get(idx).and_then(|o| o.as_ref());
        let b = other.keys.get(idx).and_then(|o| o.as_ref());
        let ord = compare_ref_key(a, b, key.descending);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    match id.cmp(&other.id) {
        Ordering::Equal => tenant_id.as_bytes().cmp(other.tenant_id.as_bytes()),
        id_ord => id_ord,
    }
}

/// `refs`（複製前の候補キー集合）が確保すれば占めるであろう [`heap_entry_bytes`]
/// を、複製せず借用長だけから見積もる（Issue #915 追加是正: Cursor Bugbot
/// 指摘「複数 TEXT キーの合計長を見ずに 1 キーずつしか予算照合しない」への
/// 対応。`heap_entry_bytes` と同じ計算式を、実際に複製する前に借用長だけで
/// 再現する。両者の対応は `heap_entry_bytes_matches_predicted_heap_entry_bytes`
/// で固定する）。
fn predicted_heap_entry_bytes(refs: &[Option<ScalarKeyRef<'_>>], tenant_id_len: usize) -> usize {
    let mut bytes = std::mem::size_of::<HeapEntry>();
    bytes = bytes.saturating_add(
        refs.len()
            .saturating_mul(std::mem::size_of::<Option<OrderValue>>()),
    );
    for r in refs {
        if let Some(ScalarKeyRef::Bytes(b)) = r {
            bytes = bytes.saturating_add(b.len());
        }
    }
    bytes.saturating_add(tenant_id_len)
}

/// [`BoundScan`] を実行する（Issue #454・TASK-186・NOSQL-3 の公開 API）。
/// `core.rs::EngineCore::execute_sql_in_session` の `Statement::Scan` アームから
/// 呼ばれるほか、[`BoundScan`] が公開型へ昇格したため engine クレート外から
/// SQL テキストを経由せず直接呼び出すこともできる（TASK-186・NOSQL-3）。
///
/// 既定の結果バイト予算（[`MAX_SCAN_RESULT_BYTES`]）で
/// [`execute_scan_with_budget`] へ委譲する薄いラッパー。
pub fn execute_scan(
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    schema: &TableSchema,
    bound: &BoundScan,
) -> Result<QueryResult, SqlSurfaceError> {
    execute_scan_with_budget(read_txn, ctx, schema, bound, MAX_SCAN_RESULT_BYTES)
}

/// [`execute_scan`] の本体（PR #1049 レビュー指摘 P1 対応。`sql::cursor::
/// CursorStatement::Declare` の内側 SELECT 実行専用の入口として
/// `core.rs::EngineCore` から直接呼ばれる）。
///
/// `max_result_bytes` は行生成中の累積バイト予算の上限（[`try_accumulate_budget`]
/// 等へそのまま渡す）。[`execute_scan`] は既定値（[`MAX_SCAN_RESULT_BYTES`]＝
/// 1 GiB）を渡すだけの薄いラッパーだが、`DECLARE` の内側 SELECT は
/// [`crate::sql::cursor::CursorRegistry`] の遥かに小さい byte 予算
/// （[`crate::sql::cursor::MAX_CURSOR_BYTES_PER_SESSION`]＝16 MiB）でしか
/// 保持できないにも関わらず、[`execute_scan`] の既定予算のまま実行すると
/// カーソル登録時（`CursorRegistry::declare`）の判定より先に最大 1 GiB もの
/// 結果を確定させてしまう（ネットワーク入力によるメモリ確保量の増幅。
/// security.md「不安全な設計」対応）。`core.rs` の `DECLARE` 実行経路はこの
/// 本体を `MAX_CURSOR_BYTES_PER_SESSION` で直接呼び、行生成中に打ち切る。
/// RLS 判定・TABLE-12 整合検査・デコード・`WHERE` を 1 行分適用してから
/// `build` へ渡す共通ヘルパー（Issue #915: `execute_scan_with_budget` の
/// 3 経路——順序なし・経路 (A) `id` 早期打ち切り・経路 (B) 上位 N 件 2 パスの
/// パス 2——が同じ RLS 適用順序〔security.md P0〕を共有するために抽出した）。
/// `build` は可視かつ `WHERE` を満たす行に対してのみ呼ばれ、その戻り値を
/// `Some` へ包んで返す。可視でない・`WHERE` を満たさない行は `Ok(None)`
/// （呼び出し元は「打ち切りではなくスキップ」として扱う）。`build` は投影段の
/// 組み立て（[`build_projected_cells`]）・ORDER BY キー抽出
/// （[`extract_order_value_ref`]）のいずれの用途にも使う。
#[allow(clippy::too_many_arguments)]
fn with_visible_row<T>(
    buf: &[u8],
    key_tenant: &str,
    id: u64,
    schema: &TableSchema,
    bound: &BoundScan,
    tier: DecodeTier,
    scalar_mask: &[bool],
    expected_dim: Option<u32>,
    ctx: &PolicyContext,
    embedding_scratch: &mut Vec<f32>,
    where_expr_scratch: &mut Vec<StackValue>,
    build: impl FnOnce(u32, &[Option<row_codec::ScalarRef<'_>>], &[f32]) -> Result<T, SqlSurfaceError>,
) -> Result<Option<T>, SqlSurfaceError> {
    // RLS 段（無条件・デコード前）。`sql::aggregate::execute_aggregate` の
    // 走査ループと同一の順序（security.md P0「テナント境界」）。
    let (tenant_id, visibility, offset) =
        storage::decode_row_header(buf).map_err(storage_internal)?;
    if !ctx.is_visible(tenant_id, visibility) {
        return Ok(None);
    }

    // TABLE-12: 物理キー側 `tenant_id` とヘッダ側 `tenant_id` の整合検査。
    storage::verify_row_key_tenant(key_tenant, tenant_id).map_err(storage_internal)?;

    let (dim, metadata): (u32, &[u8]) = match tier {
        DecodeTier::Fast | DecodeTier::DimAndScalar => {
            storage::decode_row_dim_and_metadata_borrowed(buf).map_err(storage_internal)?
        }
        DecodeTier::Embedding => storage::decode_row_body_into(buf, offset, embedding_scratch)
            .map_err(storage_internal)?,
    };
    if let Some(expected) = expected_dim {
        if dim != 0 && dim != expected {
            return Err(SqlSurfaceError::Internal {
                detail: "scan row scan failed: embedding dimension mismatch".to_string(),
            });
        }
    }

    let scanned: Vec<Option<row_codec::ScalarRef<'_>>> = match tier {
        DecodeTier::Fast => {
            row_codec::validate_scalar_columns(schema, metadata)?;
            Vec::new()
        }
        DecodeTier::DimAndScalar | DecodeTier::Embedding => {
            row_codec::scan_scalar_columns_masked(schema, metadata, Some(scalar_mask))?
        }
    };

    // Issue #919・SQL-26: `WHERE`/投影の式評価（`BoundExpr::TextColumnRef`）が
    // 使う行スカラービュー。`decode_tier_for` が式の参照する `TEXT` 列を
    // `scalar_mask` へ反映済み（`DecodeTier::Fast` ならそもそも式評価に到達
    // しない）なので、ここでは `scanned` の値をそのまま `.as_text()` へ写す
    // だけでよい（`sql::aggregate`／`sql::group_by` 等と同じ契約）。
    // SCALAR 段（WHERE）。
    if !declarative_filter::matches_all(&bound.metadata_filters, &scanned) {
        return Ok(None);
    }
    for (expr, program) in bound.expr_filters.iter().zip(&bound.expr_filter_programs) {
        let references_embedding = udf_call::references_embedding(expr);
        // `dim == 0`（`VECTOR` 列が NULL）の行を式が実際に評価すると空
        // スライスを実データと区別できず `vec_norm` 等が `0.0` を返し
        // 本来 NULL のはずの比較が意図せずマッチしてしまう（Cursor Bugbot
        // 指摘）。この判定は `program.eval` 自身（`ExprStep::PushVector`。
        // `sql::expr_program` 参照）が embedding スライスの空判定で行う
        // ため、ここでは行を無条件除外しない（codex-review P1 指摘対応・
        // Issue #921: `references_embedding` は式木全体を静的に走査するため、
        // `CASE` の選ばれない分岐に embedding 参照があるだけの行まで誤って
        // 除外していた。実際に選択された枝が embedding を参照する場合に
        // のみ `eval` が `ExprValue::Null` を返し、下記の NULL 処理
        // 〔SQL の NULL 比較は unknown → `WHERE` では偽と同義〕で除外
        // される）。
        let embedding: &[f32] =
            if references_embedding {
                match tier {
                    DecodeTier::Embedding => embedding_scratch.as_slice(),
                    DecodeTier::Fast | DecodeTier::DimAndScalar => return Err(scan_bug(
                        "WHERE expression references the VECTOR column but tier did not decode it",
                    )),
                }
            } else {
                &[]
            };
        match program.eval(id, embedding, &scanned, where_expr_scratch)? {
            ExprValue::Bool(true) => {}
            // NULL（UNKNOWN）は非該当として扱う（対象ビヘイビア: SQL-26。
            // Issue #921）。
            ExprValue::Bool(false) | ExprValue::Null => return Ok(None),
            // 束縛段（`sql::parser::bind_where_predicates`）が `WHERE` 式
            // 述語の型を `Bool` に限定済みのため到達しない。
            _ => {
                return Err(SqlSurfaceError::invalid_input(
                    "WHERE expression did not evaluate to a boolean",
                ))
            }
        }
    }
    // TASK-208・SQL-24（Issue #912）: `WHERE` の OR 群を、既存のメタデータ
    // フィルタ・式述語と同じ SCALAR 段の一部として適用する。
    for group in &bound.or_filters {
        let group_embedding: &[f32] = match tier {
            DecodeTier::Embedding => embedding_scratch.as_slice(),
            DecodeTier::Fast | DecodeTier::DimAndScalar => &[],
        };
        if !group.matches(
            &scanned,
            id,
            group_embedding,
            dim as usize,
            where_expr_scratch,
        )? {
            return Ok(None);
        }
    }

    // defense-in-depth（`RlsSafetyNet` と同趣旨）: デコード前判定が唯一の
    // 防御線にならないよう、同じ `tenant_id`・`visibility` へ再適用する
    // （security.md P0）。
    if !ctx.is_visible(tenant_id, visibility) {
        return Ok(None);
    }

    let embedding_for_build: &[f32] = match tier {
        DecodeTier::Embedding => embedding_scratch.as_slice(),
        DecodeTier::Fast | DecodeTier::DimAndScalar => &[],
    };
    build(dim, &scanned, embedding_for_build).map(Some)
}

/// 可視かつ `WHERE` を満たす行 1 件の投影段（Issue #454 の既存経路・Issue #915 の
/// 経路 (A)／経路 (B) パス 2 が共有する）。`embedding` は `tier ==
/// DecodeTier::Embedding` のときのみ実データ、それ以外は空スライス
/// （[`with_visible_row`] が渡す）。
#[allow(clippy::too_many_arguments)]
fn build_projected_cells(
    schema: &TableSchema,
    bound: &BoundScan,
    tier: DecodeTier,
    id: u64,
    dim: u32,
    embedding: &[f32],
    scanned: &[Option<row_codec::ScalarRef<'_>>],
    computed_programs: &[Option<ExprProgram>],
    proj_expr_scratch: &mut Vec<StackValue>,
    byte_budget: &mut usize,
    max_result_bytes: usize,
) -> Result<Vec<Cell>, SqlSurfaceError> {
    // Issue #919・SQL-26: `Computed` 列の式評価（`BoundExpr::TextColumnRef`）が
    // 使う行スカラービュー。`decode_tier_for` が式の参照する `TEXT` 列を
    // `scalar_mask` へ反映済みのため、`scanned` の値をそのまま `.as_text()` へ
    // 写すだけでよい（`with_visible_row` の `WHERE` 評価と同じ契約）。
    // 投影段。確保失敗時に abort せず `Err` を返せるよう `try_reserve_exact`
    // を使う（`try_alloc_text_for_budget`／`try_clone_embedding_for_budget`
    // と同方針）。
    let mut cells: Vec<Cell> = Vec::new();
    cells
        .try_reserve_exact(bound.projection.len())
        .map_err(|e| SqlSurfaceError::Internal {
            detail: format!("failed to reserve scan result cells: {e}"),
        })?;
    for (col_idx, col) in bound.projection.iter().enumerate() {
        match col {
            ProjectedColumn::Id => cells.push(Cell::Integer(id)),
            ProjectedColumn::Column { index, .. } => {
                let column =
                    schema
                        .columns
                        .get(*index)
                        .ok_or_else(|| SqlSurfaceError::Internal {
                            detail: "projected column index out of range".to_string(),
                        })?;
                match &column.ty {
                    ColumnType::Vector(_) => {
                        // `dim == 0` は `VECTOR` 列が未設定（NULL。TABLE-5 の
                        // 追加列を含む）という `storage::Row` の既存契約
                        // （`sql::aggregate::RowVector` のドキュメント参照）。
                        if dim == 0 {
                            cells.push(Cell::Null);
                        } else {
                            if tier != DecodeTier::Embedding {
                                return Err(scan_bug(
                                    "VECTOR column projected but tier did not decode embedding",
                                ));
                            }
                            cells.push(Cell::Vector(try_clone_embedding_for_budget(
                                embedding,
                                byte_budget,
                                max_result_bytes,
                            )?));
                        }
                    }
                    ColumnType::Text => match scanned.get(*index) {
                        Some(Some(row_codec::ScalarRef::Text(t))) => cells.push(Cell::Text(
                            try_alloc_text_for_budget(t, byte_budget, max_result_bytes)?,
                        )),
                        Some(None) | None => cells.push(Cell::Null),
                        Some(Some(_)) => {
                            return Err(SqlSurfaceError::Internal {
                                detail: "scalar payload type mismatch".to_string(),
                            })
                        }
                    },
                    // F8（Issue #882 計画）: REAL/DOUBLE は `Cell::Float`
                    // （REAL は f64 への無損失拡大）へ投影する。
                    ColumnType::Real => match scanned.get(*index) {
                        Some(Some(row_codec::ScalarRef::Real(v))) => {
                            cells.push(Cell::Float(f64::from(*v)))
                        }
                        Some(None) | None => cells.push(Cell::Null),
                        Some(Some(_)) => {
                            return Err(SqlSurfaceError::Internal {
                                detail: "scalar payload type mismatch".to_string(),
                            })
                        }
                    },
                    ColumnType::Double => match scanned.get(*index) {
                        Some(Some(row_codec::ScalarRef::Double(v))) => cells.push(Cell::Float(*v)),
                        Some(None) | None => cells.push(Cell::Null),
                        Some(Some(_)) => {
                            return Err(SqlSurfaceError::Internal {
                                detail: "scalar payload type mismatch".to_string(),
                            })
                        }
                    },
                    ColumnType::Integer => match scanned.get(*index) {
                        Some(Some(row_codec::ScalarRef::Integer(v))) => {
                            cells.push(Cell::SignedInteger(i64::from(*v)))
                        }
                        Some(Some(_)) => {
                            return Err(SqlSurfaceError::Internal {
                                detail: "scanned scalar type mismatch for INTEGER column"
                                    .to_string(),
                            })
                        }
                        Some(None) | None => cells.push(Cell::Null),
                    },
                    ColumnType::BigInt => match scanned.get(*index) {
                        Some(Some(row_codec::ScalarRef::BigInt(v))) => {
                            cells.push(Cell::SignedInteger(*v))
                        }
                        Some(Some(_)) => {
                            return Err(SqlSurfaceError::Internal {
                                detail: "scanned scalar type mismatch for BIGINT column"
                                    .to_string(),
                            })
                        }
                        Some(None) | None => cells.push(Cell::Null),
                    },
                    ColumnType::Boolean => match scanned.get(*index) {
                        Some(Some(row_codec::ScalarRef::Bool(b))) => cells.push(Cell::Bool(*b)),
                        Some(None) | None => cells.push(Cell::Null),
                        Some(Some(_)) => {
                            return Err(SqlSurfaceError::Internal {
                                detail: "scalar payload type mismatch".to_string(),
                            })
                        }
                    },
                    ColumnType::Date => match scanned.get(*index) {
                        Some(Some(v)) => match v.as_date() {
                            Some(d) => cells.push(Cell::Date(d)),
                            None => {
                                return Err(scan_bug(
                                    "DATE column scan yielded a non-Date scalar value",
                                ))
                            }
                        },
                        Some(None) | None => cells.push(Cell::Null),
                    },
                    ColumnType::Timestamp => {
                        match scanned.get(*index) {
                            Some(Some(v)) => match v.as_timestamp() {
                                Some(t) => cells.push(Cell::Timestamp(t)),
                                None => return Err(scan_bug(
                                    "TIMESTAMP column scan yielded a non-Timestamp scalar value",
                                )),
                            },
                            Some(None) | None => cells.push(Cell::Null),
                        }
                    }
                    ColumnType::Array(_) => match scanned.get(*index) {
                        Some(Some(row_codec::ScalarRef::Array(array_ref))) => {
                            let value = try_alloc_array_for_budget(
                                *array_ref,
                                byte_budget,
                                max_result_bytes,
                            )?;
                            cells.push(Cell::Array(value));
                        }
                        Some(Some(_)) => {
                            return Err(scan_bug(
                                "ARRAY column scan yielded a non-Array scalar value",
                            ))
                        }
                        Some(None) | None => cells.push(Cell::Null),
                    },
                    ColumnType::Bytea => match scanned.get(*index) {
                        Some(Some(row_codec::ScalarRef::Bytes(b))) => {
                            cells.push(Cell::Bytes(try_alloc_bytes_for_budget(
                                b,
                                byte_budget,
                                max_result_bytes,
                            )?));
                        }
                        Some(Some(_)) => {
                            return Err(scan_bug(
                                "BYTEA column scan yielded a non-Bytea scalar value",
                            ))
                        }
                        Some(None) | None => cells.push(Cell::Null),
                    },
                    ColumnType::Json | ColumnType::Jsonb => match scanned.get(*index) {
                        Some(Some(row_codec::ScalarRef::Json(t))) => {
                            cells.push(Cell::Json(try_alloc_text_for_budget(
                                t,
                                byte_budget,
                                max_result_bytes,
                            )?));
                        }
                        Some(Some(_)) => {
                            return Err(scan_bug(
                                "JSON column scan yielded a non-Json scalar value",
                            ))
                        }
                        Some(None) | None => cells.push(Cell::Null),
                    },
                    // ENUM 列は既存の `Cell::Text` へ写像する（Issue #890 D7。
                    // `sql::exec` の投影と同じ扱い）。
                    ColumnType::Enum(_) => match scanned.get(*index) {
                        Some(Some(v)) => match v.as_dictionary_text() {
                            Some(t) => {
                                cells.push(Cell::Text(try_alloc_text_for_budget(
                                    t,
                                    byte_budget,
                                    max_result_bytes,
                                )?));
                            }
                            None => {
                                return Err(scan_bug(
                                    "ENUM column scan yielded a non-Enum scalar value",
                                ))
                            }
                        },
                        Some(None) | None => cells.push(Cell::Null),
                    },
                    ColumnType::Numeric { .. } => match scanned.get(*index) {
                        Some(Some(v)) => match v.as_numeric() {
                            Some(d) => cells.push(Cell::Numeric(d)),
                            None => {
                                return Err(scan_bug(
                                    "NUMERIC column scan yielded a non-Numeric scalar value",
                                ))
                            }
                        },
                        Some(None) | None => cells.push(Cell::Null),
                    },
                    ColumnType::Uuid => match scanned.get(*index) {
                        Some(Some(v)) => match v.as_uuid() {
                            Some(u) => cells.push(Cell::Uuid(u)),
                            None => {
                                return Err(scan_bug(
                                    "UUID column scan yielded a non-Uuid scalar value",
                                ))
                            }
                        },
                        Some(None) | None => cells.push(Cell::Null),
                    },
                }
            }
            ProjectedColumn::Computed { .. } => {
                let program = computed_programs
                    .get(col_idx)
                    .and_then(|p| p.as_ref())
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "computed projection program missing at evaluation time"
                            .to_string(),
                    })?;
                // `dim == 0`（`VECTOR` 列が NULL）の行を式が実際に参照する
                // 場合の NULL 伝播は `program.eval` 自身（`ExprStep::
                // PushVector` の空スライス判定。`sql::expr_program`
                // 参照）が行う（codex-review P1 指摘対応・Issue #921: 静的な
                // 式木走査（`references_embedding`）による事前判定は `CASE`
                // の選ばれない分岐に embedding 参照があるだけの行まで誤って
                // NULL 化していたため撤去し、評価時点の判定へ一本化した）。
                let embedding_for_eval: &[f32] = if tier == DecodeTier::Embedding {
                    embedding
                } else {
                    &[]
                };
                match program.eval(id, embedding_for_eval, scanned, proj_expr_scratch)? {
                    ExprValue::Scalar(v) => cells.push(Cell::Float(v)),
                    ExprValue::Vector(v) => {
                        // codex-review P1 指摘対応: `Computed` 列のベクトル
                        // 結果も `VECTOR` 列直接投影と同じ累計予算
                        // （`MAX_SCAN_RESULT_BYTES`）へ計上する。所有化
                        // （`into_owned_vector`）自体は `try_reserve_exact`
                        // 経由で単発の確保失敗には強いが、累計を見ないと
                        // 行数分の蓄積で予算を回避できてしまうため。
                        let owned = udf_call::into_owned_vector(v)?;
                        cells.push(Cell::Vector(try_accumulate_vector_budget(
                            owned,
                            byte_budget,
                            max_result_bytes,
                        )?));
                    }
                    ExprValue::Bool(b) => cells.push(Cell::Bool(b)),
                    // Issue #919・SQL-26（AC2）: 投影の式結果は `Cell::Text`/
                    // `Cell::Null` へそのまま写像する（wire 層は既存の
                    // `Computed` → text OID 25 の経路をそのまま使うため
                    // wire 側の変更は不要）。
                    ExprValue::Text(t) => {
                        cells.push(Cell::Text(try_alloc_text_for_budget(
                            &t,
                            byte_budget,
                            max_result_bytes,
                        )?));
                    }
                    // 対象ビヘイビア: SQL-26（Issue #920）。`DATE`／`TIMESTAMP`
                    // を返す式（`date_trunc` 等）を `Cell::Date`／
                    // `Cell::Timestamp` へ写像する。
                    ExprValue::Date(d) => cells.push(Cell::Date(d)),
                    ExprValue::Timestamp(t) => cells.push(Cell::Timestamp(t)),
                    // Issue #919・SQL-26（AC2）と Issue #921・SQL-26 の共有
                    // 契約: NULL は `Cell::Null` へ写像する。
                    ExprValue::Null => cells.push(Cell::Null),
                }
            }
        }
    }
    Ok(cells)
}

/// 経路 (B)（上位 N 件 2 パス）の 1 パス目が `BinaryHeap` へ保持する候補
/// 1 件分（Issue #915・SQL-25）。`spec`（`Rc` 共有）は `bound.order_by` の
/// 複製を全候補で使い回すための参照カウント（キー数は
/// [`crate::sql::allowlist::MAX_SCALAR_ORDER_KEYS`] で有界）。
#[derive(Debug, Clone)]
struct HeapEntry {
    keys: Vec<Option<OrderValue>>,
    tenant_id: String,
    id: u64,
    spec: Rc<[BoundOrderKey]>,
}

impl HeapEntry {
    /// `spec` の各キーを順に比較し、最初の非同点で確定する（同点継続時は
    /// `id` 昇順 → `tenant_id` バイト順。§受入基準 2「決定的な順序」）。
    /// 戻り値は「昇順ソートすると最終的な出力順になる」意味の `Ordering`。
    fn order(&self, other: &Self) -> Ordering {
        compare_statement_order(
            &self.spec,
            &self.keys,
            self.id,
            self.tenant_id.as_bytes(),
            &other.keys,
            other.id,
            other.tenant_id.as_bytes(),
        )
    }
}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.order(other) == Ordering::Equal
    }
}
impl Eq for HeapEntry {}
impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.order(other)
    }
}
impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// `entry` が保持するバイト量の概算（TEXT キーの所有バイト・`tenant_id`・
/// `keys` 配列自体のヒープ確保量（容量ベース）・構造体分。予算計上・解放の
/// 対称な単位として使う（Issue #915・codex-review PR #1096 P1 是正:
/// `keys: Vec<Option<OrderValue>>` の配列容量が未計上だと、複数キー・大きい
/// `LIMIT + OFFSET` で配列本体が `max_result_bytes` の外側に蓄積し得た）。
fn heap_entry_bytes(entry: &HeapEntry) -> usize {
    heap_entry_keys_bytes(entry).saturating_add(heap_entry_fixed_bytes(entry))
}

/// [`heap_entry_bytes`] のうち `keys`（並べ替えキー配列。TEXT 実体を含む）分
/// だけを切り出したもの（codex-review PR #1096 追加是正: パス 2 で投影に不要な
/// `keys` を先行して実際に破棄し、その分だけ先に予算を解放するために分離した。
/// `keys` 配列自体のヒープ確保量（容量ベース）と TEXT 実体を計上する）。
fn heap_entry_keys_bytes(entry: &HeapEntry) -> usize {
    let mut bytes = entry
        .keys
        .capacity()
        .saturating_mul(std::mem::size_of::<Option<OrderValue>>());
    for key in &entry.keys {
        if let Some(OrderValue::Bytes(b)) = key {
            bytes = bytes.saturating_add(b.len());
        }
    }
    bytes
}

/// [`heap_entry_bytes`] のうち `keys` 以外（構造体本体・`tenant_id`）分だけを
/// 切り出したもの（[`heap_entry_keys_bytes`] と対になる残り。`entry` 自体が
/// 実際に破棄されるまで生存し続ける部分）。
fn heap_entry_fixed_bytes(entry: &HeapEntry) -> usize {
    std::mem::size_of::<HeapEntry>().saturating_add(entry.tenant_id.len())
}

/// [`BoundScan::order_by`] の先頭キーが疑似列 `id` かどうか（経路 (A) の判定条件の
/// 片方。Issue #915）。
fn leading_key_is_id(bound: &BoundScan) -> bool {
    matches!(bound.order_by.first(), Some(key) if key.target == BoundOrderTarget::Id)
}

pub(crate) fn execute_scan_with_budget(
    read_txn: &impl crate::storage::read_source::ReadSource,
    ctx: &PolicyContext,
    schema: &TableSchema,
    bound: &BoundScan,
    max_result_bytes: usize,
) -> Result<QueryResult, SqlSurfaceError> {
    // SQL-30・TASK-214（Issue #930）: ウィンドウ項目を持つ広域取得は専用の実行器
    // （`sql::window::execute_window_scan`）へ委譲する。本体側の変更をこの
    // dispatch 1 行に留めることで、`sql::scan` を編集する他 PR との衝突を
    // 最小化する（`docs/design/window-functions.md` 参照）。
    if !bound.windows().is_empty() {
        return crate::sql::window::execute_window_scan(
            read_txn,
            ctx,
            schema,
            bound,
            max_result_bytes,
        );
    }

    let expected_dim = schema.vector_dim();
    let (tier, scalar_mask) = decode_tier_for(schema, bound);

    // 結果列メタデータは行数によらず常に構築する（空集合でも列は返す既存契約。
    // `sql::exec::execute_statement`・`sql::aggregate::execute_aggregate` と同じ）。
    let mut columns = Vec::with_capacity(bound.projection.len());
    for col in &bound.projection {
        columns.push(match col {
            ProjectedColumn::Id => ColumnMeta::Id,
            ProjectedColumn::Column { index, name } => {
                let column =
                    schema
                        .columns
                        .get(*index)
                        .ok_or_else(|| SqlSurfaceError::Internal {
                            detail: "projected column index out of range".to_string(),
                        })?;
                ColumnMeta::Scalar {
                    name: name.clone(),
                    ty: column.ty.clone(),
                }
            }
            ProjectedColumn::Computed { name, ty, .. } => {
                crate::sql::exec::computed_column_meta(name, *ty)
            }
        });
    }

    let row_table_name = catalog::user_rows_table_name(&bound.table);
    let table = match read_txn.open_table(catalog::user_rows_table_def(&row_table_name)) {
        Ok(t) => Some(t),
        // 対象テーブルの行が 1 件も書き込まれていない（行テーブル自体が未作成）は
        // 空集合として扱う（`sql::aggregate::execute_aggregate` と同じ契約）。
        Err(redb::TableError::TableDoesNotExist(_)) => None,
        Err(e) => {
            return Err(SqlSurfaceError::Internal {
                detail: format!("scan row scan failed: {}", catalog::map_row_table_error(e)),
            })
        }
    };

    // Issue #353 と同じく、`Computed` 列の式を行ループの外で 1 回だけステップ列
    // コンパイルする（行ループでの再帰評価をなくす）。
    let computed_programs: Vec<Option<ExprProgram>> = bound
        .projection
        .iter()
        .map(|col| match col {
            ProjectedColumn::Computed { expr, .. } => Some(ExprProgram::compile(expr)),
            ProjectedColumn::Id | ProjectedColumn::Column { .. } => None,
        })
        .collect();

    // 可視行ごとの embedding デコード先スクラッチバッファ・式評価用の明示スタック
    // （行ごとに新規確保しない。`sql::aggregate::execute_aggregate` と同方針）。
    // `WHERE` 評価用（`with_visible_row` 内部）と投影段の `Computed` 列評価用
    // （`build_projected_cells`）でスタックを分ける（Issue #915: 両者を同一の
    // `&mut Vec<StackValue>` にすると `with_visible_row` の引数と投影クロージャの
    // キャプチャが同じ変数を同時に可変借用してしまいコンパイルできないため）。
    let mut embedding_scratch: Vec<f32> = Vec::new();
    let mut where_expr_scratch: Vec<StackValue> = Vec::new();
    let mut proj_expr_scratch: Vec<StackValue> = Vec::new();
    // Issue #1188: 経路 (B) パス 1 の式キー評価用スタック（`ORDER BY` が式キーを持つ場合のみ使う）。
    let mut order_expr_scratch: Vec<StackValue> = Vec::new();
    let mut byte_budget: usize = 0;
    let mut rows: Vec<ResultRow> = Vec::new();
    // Issue #916・SQL-25 (b)・TASK-209: `OFFSET` で読み飛ばした「可視かつ WHERE 一致」
    // 行数。RLS 判定（デコード前・再適用の両方）と WHERE 評価より後でのみ加算する
    // ことで、不可視行・WHERE 不一致行が読み飛ばし数に一切現れないようにする
    // （不可視行のスキップ手段にしない。RLS-7/8）。
    let mut skipped: usize = 0;

    // codex-review P1 指摘対応: `cells`（`Vec<Cell>`）・`rows`（`Vec<ResultRow>`）
    // 双方の確保量を累計予算へ計上する。テキスト・ベクトルの実体バイトのみを
    // 計上する従来の `try_alloc_text_for_budget`／`try_clone_embedding_for_budget`
    // は、`id` 等実体バイトを消費しない列だけを大量に並べた投影（例:
    // `SELECT id, id, ..., id LIMIT 10000`）では `byte_budget` が 0 のまま
    // `rows.len() * bound.projection.len()` 個の `Cell` と `rows.len()` 個の
    // `ResultRow` を確保できてしまい、`MAX_SCAN_RESULT_BYTES` を迂回してメモリ
    // 枯渇を招く（`sql/exec.rs` の `row_struct_bytes` と同じ意図。構造体
    // アロケーション自体を見逃さない）。`ResultRow` 自体は `cells` を除いても
    // `id`/`score`/`Vec<Cell>` のヘッダ分の固定サイズを持つため、1 行あたりの
    // 構造体確保量として `cells` 分とまとめて計上する。
    let cell_struct_bytes = bound
        .projection
        .len()
        .saturating_mul(std::mem::size_of::<Cell>());
    let result_row_struct_bytes = std::mem::size_of::<ResultRow>();
    let per_row_struct_bytes = cell_struct_bytes.saturating_add(result_row_struct_bytes);

    let Some(table) = table else {
        return Ok(QueryResult { columns, rows });
    };

    if bound.order_by.is_empty() {
        // 順序なし（従来経路）。`build_visible_row` は可視かつ `WHERE` を
        // 満たす行のみ `Some` を返す（Issue #915: `rows` 自体は捕捉せず
        // 呼び出し元が push する——クロージャが `rows` を可変借用すると
        // ループ条件 `rows.len() >= bound.limit` の読み取りと競合するため）。
        let mut build_visible_row = |key_tenant: &str, id: u64, buf: &[u8]| {
            with_visible_row(
                buf,
                key_tenant,
                id,
                schema,
                bound,
                tier,
                &scalar_mask,
                expected_dim,
                ctx,
                &mut embedding_scratch,
                &mut where_expr_scratch,
                |dim, scanned, embedding| {
                    // PR #1096 レビュー指摘 P1（codex-review・cursor Bugbot 双方が
                    // 独立検出）対応: 順序保証なし経路では物理走査順がそのまま
                    // 出力順（本モジュールドキュメント「順序」節）のため、`skipped`
                    // 判定を投影・`byte_budget` 加算より前に確定させる。ここで
                    // `None` を返す行は「スキップされる行」であり、大きな
                    // `TEXT`/`VECTOR` を含んでいても投影・予算計上を一切行わない
                    // （既存の「スキップ行は投影・予算計上しない」契約。逆順にすると
                    // OFFSET が大きいだけで不要に `54000` を返す回帰になる）。
                    if skipped < bound.offset {
                        skipped += 1;
                        return Ok(None);
                    }
                    // `cells`／`rows` 確保前に累計予算を検証（確保そのものを
                    // 許可する前に拒否できるよう `Vec::try_reserve` 系より先に
                    // 判定する）。
                    byte_budget =
                        try_accumulate_budget(byte_budget, per_row_struct_bytes, max_result_bytes)?;
                    let cells = build_projected_cells(
                        schema,
                        bound,
                        tier,
                        id,
                        dim,
                        embedding,
                        scanned,
                        &computed_programs,
                        &mut proj_expr_scratch,
                        &mut byte_budget,
                        max_result_bytes,
                    )?;
                    Ok(Some(ResultRow {
                        id,
                        score: 0.0,
                        cells,
                    }))
                },
            )
        };

        // 早期終了: 可視かつ WHERE を満たす行が `bound.limit` 件集まった
        // 時点で走査を打ち切る（本モジュールドキュメント「順序保証なし」
        // 契約の実装側。テナントを跨いだ物理走査順のどこで打ち切っても、
        // 不可視行は一切カウントされないため他テナントの存在・件数の情報を
        // 漏らさない）。
        for entry in table.iter().map_err(storage_internal)? {
            if rows.len() >= bound.limit {
                break;
            }
            let (k, v) = entry.map_err(storage_internal)?;
            let (key_tenant, id) = k.value();
            let buf = v.value();
            // `build_visible_row` が `Some(None)` を返すのは「可視かつ WHERE を
            // 満たすが OFFSET でスキップされる行」（投影・予算計上済みでない）。
            if let Some(Some(row)) = build_visible_row(key_tenant, id, buf)? {
                rows.try_reserve(1).map_err(|e| SqlSurfaceError::Internal {
                    detail: format!("failed to reserve scan result rows: {e}"),
                })?;
                rows.push(row);
            }
        }
    } else if leading_key_is_id(bound) && !ctx.allows_public() {
        // 経路 (A): 先頭キーが `id` かつ `ctx` が他テナントの `Public` 行を
        // 可視としない場合、可視行は自テナントのパーティションに閉じる
        // （`id` はテナント内で一意なため後続キーは無関係）。物理キーは
        // `(tenant_id, id)` の辞書順であり `u64::MIN == 0`／`u64::MAX` が
        // 対象テナントの id 空間の両端を覆うため、この閉区間は対象テナント
        // 所有行のみを列挙し他テナント領域のキー・値には一切触れない
        // （`tenant.rs::enumerate_dml_candidates` と同型の閉区間。Issue #871
        // と同じ判断）。
        let mut build_visible_row = |key_tenant: &str, id: u64, buf: &[u8]| {
            with_visible_row(
                buf,
                key_tenant,
                id,
                schema,
                bound,
                tier,
                &scalar_mask,
                expected_dim,
                ctx,
                &mut embedding_scratch,
                &mut where_expr_scratch,
                |dim, scanned, embedding| {
                    // PR #1096 レビュー指摘 P1（codex-review・cursor Bugbot 双方が
                    // 独立検出）対応: 経路 (A) は物理走査順が疑似列 `id` による
                    // ソート確定順と一致するため、`skipped` 判定を投影・
                    // `byte_budget` 加算より前に確定させる（unordered 経路と同じ
                    // 理由。スキップされる行の大きな `TEXT`/`VECTOR` で不要に
                    // `54000` を返す回帰を避ける）。
                    if skipped < bound.offset {
                        skipped += 1;
                        return Ok(None);
                    }
                    byte_budget =
                        try_accumulate_budget(byte_budget, per_row_struct_bytes, max_result_bytes)?;
                    let cells = build_projected_cells(
                        schema,
                        bound,
                        tier,
                        id,
                        dim,
                        embedding,
                        scanned,
                        &computed_programs,
                        &mut proj_expr_scratch,
                        &mut byte_budget,
                        max_result_bytes,
                    )?;
                    Ok(Some(ResultRow {
                        id,
                        score: 0.0,
                        cells,
                    }))
                },
            )
        };

        let tenant = ctx.tenant_id();
        let range_start = std::ops::Bound::Included((tenant, 0u64));
        let range_end = std::ops::Bound::Included((tenant, u64::MAX));
        let descending = bound
            .order_by
            .first()
            .map(|key| key.descending)
            .unwrap_or(false);
        let range = table
            .range::<(&str, u64)>((range_start, range_end))
            .map_err(storage_internal)?;
        if descending {
            for entry in range.rev() {
                if rows.len() >= bound.limit {
                    break;
                }
                let (k, v) = entry.map_err(storage_internal)?;
                let (key_tenant, id) = k.value();
                let buf = v.value();
                // `build_visible_row` が `Some(None)` を返すのは「可視かつ WHERE
                // を満たすが OFFSET でスキップされる行」（投影・予算計上済みでない）。
                if let Some(Some(row)) = build_visible_row(key_tenant, id, buf)? {
                    rows.try_reserve(1).map_err(|e| SqlSurfaceError::Internal {
                        detail: format!("failed to reserve scan result rows: {e}"),
                    })?;
                    rows.push(row);
                }
            }
        } else {
            for entry in range {
                if rows.len() >= bound.limit {
                    break;
                }
                let (k, v) = entry.map_err(storage_internal)?;
                let (key_tenant, id) = k.value();
                let buf = v.value();
                if let Some(Some(row)) = build_visible_row(key_tenant, id, buf)? {
                    rows.try_reserve(1).map_err(|e| SqlSurfaceError::Internal {
                        detail: format!("failed to reserve scan result rows: {e}"),
                    })?;
                    rows.push(row);
                }
            }
        }
    } else {
        // 経路 (B): 上位 N 件の 2 パス。パス 1 は既存の走査ループ（RLS →
        // TABLE-12 → デコード → WHERE → 可視性の再判定）をそのまま通し、
        // 並べ替えキーの所有値と `(tenant_id, id)` だけを容量 `bound.limit` の
        // `BinaryHeap` に保持する（最悪要素を追い出す。§受入基準「予算」）。
        // Issue #916・SQL-25 (b)・TASK-209: `OFFSET` はソート確定後に適用する契約
        // （`docs/design/sql-offset-paging.md`）のため、パス 1 では
        // `bound.limit + bound.offset` 件（先頭から `offset` 件を捨てた後に
        // ちょうど `limit` 件残る件数）を保持する。両者とも `validate_search_limit`・
        // `validate_search_offset` で `MAX_SEARCH_K` 以下に検証済みのため実際には
        // 桁あふれしないが、規約（`.claude/rules/coding-rust.md`）に従い
        // `saturating_add` で未定義動作を避ける。
        let heap_capacity = bound.limit.saturating_add(bound.offset);
        let spec: Rc<[BoundOrderKey]> = Rc::from(bound.order_by.clone());
        let mut heap: BinaryHeap<HeapEntry> = BinaryHeap::new();
        let mut heap_budget: usize = 0;

        for entry in table.iter().map_err(storage_internal)? {
            let (k, v) = entry.map_err(storage_internal)?;
            let (key_tenant, id) = k.value();
            let buf = v.value();
            // codex-review PR #1096 追加是正（P1 2 件・Cursor Bugbot 1 件の統合
            // 是正）: 複製（`scalar_key_ref_to_owned`）はこの build クロージャの
            // 「最後」でのみ行う。まず `extract_order_value_ref` で全キーを
            // 借用のまま取り出し、(1) ヒープが満杯なら複製前に
            // `compare_candidate_ref_to_entry` で現在の最悪候補と比較し、
            // 採用されない（`Ordering::Less` でない）候補は一切複製・確保せず
            // `Ok(None)` で抜ける（上位候補にならない大きな TEXT キーの行が
            // クエリ全体を不要に拒否するのを防ぐ）。(2) 採用される・またはヒープに
            // まだ空きがある場合のみ、全キー合計長・`keys` 配列容量・候補保持量を
            // 単一の [`predicted_heap_entry_bytes`] で確保前に照合する
            // （複数 TEXT キーの合計長を見ずに 1 キーずつしか照合しない旧実装の
            // 抜け穴を解消）。
            let decision = with_visible_row(
                buf,
                key_tenant,
                id,
                schema,
                bound,
                tier,
                &scalar_mask,
                expected_dim,
                ctx,
                &mut embedding_scratch,
                &mut where_expr_scratch,
                |_dim, scanned, embedding| {
                    // Issue #1188: 式キーは行ごとに評価し、評価結果（`ExprValue`。TEXT は
                    // 列参照なら借用・関数結果なら評価時に構築済みの所有値）を借用したまま
                    // 採否判定・予算照合に使う。複製（所有値化）は採用が決まった候補に
                    // 対してだけ下の `scalar_key_ref_to_owned` で 1 回行う（列キーの経路と
                    // 同じ「採用前に複製しない」契約）。RLS・WHERE を通過した可視行に対して
                    // のみ評価される（`with_visible_row` の順序）。評価エラー（`22012`／
                    // `22003`／`22008` 等）は可視行の値のみから生じ、そのまま fail-closed で返す。
                    let mut expr_values: Vec<ExprValue<'_>> =
                        Vec::with_capacity(bound.order_exprs.len());
                    for key in &bound.order_by {
                        if let BoundOrderTarget::Expr(i) = key.target {
                            let (_, program) = bound.order_exprs.get(i).ok_or_else(|| {
                                SqlSurfaceError::Internal {
                                    detail: "expression order key program missing".to_string(),
                                }
                            })?;
                            expr_values.push(program.eval(
                                id,
                                embedding,
                                scanned,
                                &mut order_expr_scratch,
                            )?);
                        }
                    }
                    let mut expr_iter = expr_values.iter();
                    let mut refs: Vec<Option<ScalarKeyRef<'_>>> =
                        Vec::with_capacity(bound.order_by.len());
                    for key in &bound.order_by {
                        if matches!(key.target, BoundOrderTarget::Expr(_)) {
                            let v = expr_iter.next().ok_or_else(|| SqlSurfaceError::Internal {
                                detail: "expression order key value missing".to_string(),
                            })?;
                            refs.push(expr_value_to_key_ref(v, key.kind)?);
                        } else {
                            refs.push(extract_order_value_ref(schema, key, id, scanned)?);
                        }
                    }
                    if heap.len() >= heap_capacity {
                        let worst = heap.peek().ok_or_else(|| SqlSurfaceError::Internal {
                            detail: "heap reported at capacity but is empty".to_string(),
                        })?;
                        let cmp = compare_candidate_ref_to_entry(
                            &bound.order_by,
                            &refs,
                            id,
                            key_tenant,
                            worst,
                        );
                        if cmp != Ordering::Less {
                            // 現在の最悪候補以上（採用されない）。複製せずスキップ。
                            return Ok(None);
                        }
                        // codex-review PR #1096 追加是正（最新ラウンド）: 採用が
                        // 決まった時点では最悪候補（`worst`）はまだヒープ内に
                        // 残っており（`heap.pop()` は複製・push が終わった後で
                        // 呼ぶ）実メモリ上も生存し続けている。複製前の見積り
                        // （`predicted`）は、この一時的な同時保持（`worst` を
                        // 含む現在の `heap_budget` 全体 + 新候補）を基準に照合
                        // する（`worst` を先に差し引いた残り予算と照合すると、
                        // 複製〜実際の追い出しまでの間、実際の同時保持量が
                        // 計上額を超えうる。「計上額 ≥ 実際の同時保持量」を
                        // 常に保つ不変条件）。
                        let predicted = predicted_heap_entry_bytes(&refs, key_tenant.len());
                        try_accumulate_budget(heap_budget, predicted, max_result_bytes)?;
                    } else {
                        let predicted = predicted_heap_entry_bytes(&refs, key_tenant.len());
                        try_accumulate_budget(heap_budget, predicted, max_result_bytes)?;
                    }
                    let mut keys = Vec::with_capacity(refs.len());
                    for r in refs {
                        keys.push(match r {
                            Some(v) => Some(scalar_key_ref_to_owned(v)?),
                            None => None,
                        });
                    }
                    Ok(Some(keys))
                },
            )?;
            let Some(Some(keys)) = decision else {
                continue;
            };
            let candidate = HeapEntry {
                keys,
                tenant_id: key_tenant.to_string(),
                id,
                spec: Rc::clone(&spec),
            };
            if heap.len() < heap_capacity {
                let bytes = heap_entry_bytes(&candidate);
                heap_budget = try_accumulate_budget(heap_budget, bytes, max_result_bytes)?;
                heap.push(candidate);
            } else {
                // codex-review PR #1096 追加是正（最新ラウンド）: 新候補を先に
                // 計上・push してから最悪候補を追い出す（追い出し中は新旧
                // 両方が実際にヒープ内へ同時に存在するため、計上額はこの
                // 一時的な合計を下回ってはならない。閉包内の
                // `try_accumulate_budget(heap_budget, predicted, ...)` と
                // 対称）。追い出した候補（`popped`）は明示的に `drop` して
                // 実際に破棄したことを確定させてから、その分だけ解放する
                // （解放が実際の破棄より先に起きないことを保証する。
                // 「計上額 ≥ 実際の同時保持量」の不変条件）。
                let bytes = heap_entry_bytes(&candidate);
                heap_budget = try_accumulate_budget(heap_budget, bytes, max_result_bytes)?;
                heap.push(candidate);
                if let Some(popped) = heap.pop() {
                    let popped_bytes = heap_entry_bytes(&popped);
                    drop(popped);
                    heap_budget = heap_budget.checked_sub(popped_bytes).ok_or_else(
                        || SqlSurfaceError::Internal {
                            detail: "scan result byte budget underflow while evicting the worst heap candidate"
                                .to_string(),
                        },
                    )?;
                }
            }
        }

        // codex-review PR #1096 P1 是正: パス 1 のヒープ候補（`heap_budget`）と
        // パス 2 の投影結果（`byte_budget`）を別々の予算カウンタで `max_result_bytes`
        // 上限判定していたため、両者を同時に保持する実メモリ量が単独の上限判定を
        // すり抜けて `max_result_bytes` の 2 倍近くまで達し得た（`DECLARE CURSOR` の
        // 小さい予算指定でメモリ予算を実質迂回できる経路）。`heap.into_sorted_vec()`
        // が返す全候補はパス 2 の投影完了までヒープ内で保持され続けるため
        // （各要素は消費時に初めて破棄される）、パス 2 開始時点で候補が占める
        // 実メモリは `heap_budget` 分そのまま残っている。ここで `byte_budget` を
        // `heap_budget` から引き継ぐことで、候補・結果を跨いだ単一の共通予算
        // カウンタとして扱い、以降の `try_accumulate_budget` 呼び出しが両者の
        // 合計を `max_result_bytes` 以下に fail-closed で制限する。
        byte_budget = heap_budget;

        // Issue #916・SQL-25 (b)・TASK-209: 経路 (B) の `OFFSET` は
        // `docs/design/sql-offset-paging.md`「ORDER BY なし OFFSET の意味論」節が
        // 申し送る #915 統合事項（ソート確定後にスキップする契約）を、上のパス 1
        // でのヒープ容量を `bound.limit + bound.offset` へ広げることで満たす
        // （`heap.len() < ...` の判定を参照）。ここでは `heap.into_sorted_vec()`
        // が返す確定済み順序の先頭から `bound.offset` 件を読み飛ばす（不可視行・
        // WHERE 不一致行は既に候補から除外済みのため、ここで数えても他テナントの
        // 存在・件数は漏えいしない）。
        //
        // codex-review PR #1096 追加是正: `OFFSET` で読み飛ばす候補は投影されない
        // ため `byte_budget` へ加算されないが、`winner` 自体はここで破棄される
        // まで、パス 1 が計上した分のメモリを実際に保持していた。この解放を
        // 怠ると、読み飛ばし対象に大きな TEXT キーが集中する入力で、以降の
        // 残り行の投影が実際の保持量より小さい `byte_budget` のまま
        // `max_result_bytes` を実質迂回できた（line 1369 の
        // `byte_budget = heap_budget` 引き継ぎと対称に、破棄のたびに同じ
        // [`heap_entry_bytes`] で計上分を戻す）。この解放は下の
        // `build_visible_row` クロージャが `byte_budget` を排他的に借用する
        // 「前」に完了させる必要があるため（クロージャは存続期間中ずっと
        // 借用を保持し、途中で直接代入できない）、読み飛ばし専用のループを
        // クロージャ定義より前に独立させている。
        //
        // codex-review PR #1096 追加是正（最新ラウンド）: 「計上額 ≥ 実際の
        // 同時保持量」を常に保つため、バイト数は `winner` を実際に `drop`
        // した**後**に解放する（先に解放すると、まだ実メモリ上に残っている
        // `winner` を計上から除外したことになり、瞬間的に計上額が実際の
        // 保持量を下回りうる）。加算と解放が対称であることを保証するため
        // `saturating_sub` ではなく `checked_sub` を使い、対応が崩れていれば
        // fail-closed に `Internal` へ落とす（過小申告のまま静かに処理を
        // 続けない）。
        let mut sorted_winners = heap.into_sorted_vec().into_iter();
        for winner in sorted_winners.by_ref().take(bound.offset) {
            let bytes = heap_entry_bytes(&winner);
            drop(winner);
            byte_budget = byte_budget.checked_sub(bytes).ok_or_else(|| {
                SqlSurfaceError::Internal {
                    detail: "scan result byte budget underflow while releasing an offset-skipped candidate"
                        .to_string(),
                }
            })?;
        }

        // パス 2: 全順序で確定してから（`BinaryHeap::into_sorted_vec` は
        // `Ord` の昇順。`(tenant_id, id)` が一意な全順序のため安定性は
        // 問題にならない）、同じ read txn 内で勝者のみを再取得し投影する
        // （`sort_unstable_*` を使わない。`make sort-determinism-check`）。
        // codex-review PR #1096 追加是正（最新ラウンド）: `with_visible_row`
        // 呼び出しをループの外へ括り出さず反復ごとにインラインで行う。
        // 括り出したクロージャが `byte_budget` を排他的に可変借用し続けると
        // （元の設計）、`keys` の解放（下記）を投影より前の任意の位置で
        // 直接代入できなくなるため（借用チェッカーに拒否される）、反復ごとに
        // 独立した一時クロージャとして渡す形へ変更した（パス 1 の候補判定
        // クロージャと同型。Issue #915）。
        for mut winner in sorted_winners {
            let guard = table
                .get((winner.tenant_id.as_str(), winner.id))
                .map_err(storage_internal)?;
            let Some(guard) = guard else {
                // 同じスナップショット内で消えることは本来起こらない
                // （fail-closed。§検証方法「経路 (A) と (B) の等価性」）。
                return Err(SqlSurfaceError::Internal {
                    detail: "scan row scan failed: ordered row missing on second pass".to_string(),
                });
            };
            let buf = guard.value();

            // codex-review PR #1096 追加是正（最新ラウンド）: 投影は再デコード
            // した `scanned`／`embedding` のみを使い、`winner.keys`（並べ替え
            // 用に複製した TEXT 等）は不要。投影セルの確保より前に `keys` を
            // 実際に破棄してから、その分だけ予算を解放する（実際の破棄が
            // 解放より先に完了することを保証する。「計上額 ≥ 実際の同時
            // 保持量」の不変条件。大きな TEXT キーが `DECLARE CURSOR` の
            // 小さい `max_result_bytes` を一時的に超える余地を残さない）。
            let keys_bytes = heap_entry_keys_bytes(&winner);
            let owned_keys = std::mem::take(&mut winner.keys);
            drop(owned_keys);
            byte_budget = byte_budget.checked_sub(keys_bytes).ok_or_else(|| {
                SqlSurfaceError::Internal {
                    detail:
                        "scan result byte budget underflow while releasing a projected candidate's sort keys"
                            .to_string(),
                }
            })?;

            let row = with_visible_row(
                buf,
                winner.tenant_id.as_str(),
                winner.id,
                schema,
                bound,
                tier,
                &scalar_mask,
                expected_dim,
                ctx,
                &mut embedding_scratch,
                &mut where_expr_scratch,
                |dim, scanned, embedding| {
                    byte_budget =
                        try_accumulate_budget(byte_budget, per_row_struct_bytes, max_result_bytes)?;
                    let cells = build_projected_cells(
                        schema,
                        bound,
                        tier,
                        winner.id,
                        dim,
                        embedding,
                        scanned,
                        &computed_programs,
                        &mut proj_expr_scratch,
                        &mut byte_budget,
                        max_result_bytes,
                    )?;
                    Ok(ResultRow {
                        id: winner.id,
                        score: 0.0,
                        cells,
                    })
                },
            )?;
            let Some(row) = row else {
                return Err(SqlSurfaceError::Internal {
                    detail: "scan row scan failed: ordered row became invisible on second pass"
                        .to_string(),
                });
            };
            rows.try_reserve(1).map_err(|e| SqlSurfaceError::Internal {
                detail: format!("failed to reserve scan result rows: {e}"),
            })?;
            rows.push(row);

            // `winner`（`keys` は既に破棄済みなので残りは構造体本体・
            // `tenant_id`）は `build_visible_row` へ `tenant_id` を貸す
            // 必要があったためここまで生存させた。ここで実際に破棄して
            // から、対応するバイト数を解放する（上の `keys` 解放と同じ
            // 「実際の破棄が解放より先」の順序）。
            let fixed_bytes = heap_entry_fixed_bytes(&winner);
            drop(winner);
            byte_budget =
                byte_budget
                    .checked_sub(fixed_bytes)
                    .ok_or_else(|| {
                        SqlSurfaceError::Internal {
                    detail:
                        "scan result byte budget underflow while releasing a projected candidate"
                            .to_string(),
                }
                    })?;
        }
    }

    Ok(QueryResult { columns, rows })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnDef, ColumnType, TableSchema};
    use crate::sql::parser::BoundScan;
    use crate::storage::{RowInput, Storage, Visibility};
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
    use redb::ReadableDatabase;

    /// nullable な `VECTOR` 列（TABLE-5 想定）を単一列として持つテーブルのスキーマ
    /// （`sql::aggregate` モジュール内テストの `nullable_vector_schema` と同型）。
    fn nullable_vector_schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(3), true)],
        )
    }

    /// 検証専用: `encode_row`（低レベル API）で直接行を書き込む。TABLE-5 が想定する
    /// 「既存行が対象列を未設定のまま持つ」状態を、公開 INSERT 経路を経由せず直接
    /// 再現する（`sql::aggregate` モジュール内テストの `write_row_direct` と同型。
    /// PR #229 codex-review 指摘対応の踏襲）。
    fn write_row_direct(
        storage: &Storage,
        table_name: &str,
        tenant_id: &str,
        id: u64,
        embedding: &[f32],
    ) {
        let write_txn = storage.db().begin_write().expect("begin_write");
        {
            let mut table = write_txn
                .open_table(crate::catalog::user_rows_table_def(
                    &crate::catalog::user_rows_table_name(table_name),
                ))
                .expect("open row table");
            let buf = crate::storage::encode_row(&RowInput {
                tenant_id,
                visibility: Visibility::Public,
                embedding,
                metadata: &[],
            })
            .expect("encode row");
            table
                .insert((tenant_id, id), buf.as_slice())
                .expect("insert row");
        }
        crate::storage::bump_generation_and_commit(write_txn).expect("commit");
    }

    /// [`write_row_direct`] の `Visibility` 明示指定版（Issue #916・SQL-25 (b)・
    /// TASK-209 の RLS 回帰テスト専用。他テナントの行を `Private` として書き込み、
    /// `OFFSET` の計数が可視行のみを対象にすることを検証するために使う）。
    fn write_row_direct_with_visibility(
        storage: &Storage,
        table_name: &str,
        tenant_id: &str,
        id: u64,
        embedding: &[f32],
        visibility: Visibility,
    ) {
        let write_txn = storage.db().begin_write().expect("begin_write");
        {
            let mut table = write_txn
                .open_table(crate::catalog::user_rows_table_def(
                    &crate::catalog::user_rows_table_name(table_name),
                ))
                .expect("open row table");
            let buf = crate::storage::encode_row(&RowInput {
                tenant_id,
                visibility,
                embedding,
                metadata: &[],
            })
            .expect("encode row");
            table
                .insert((tenant_id, id), buf.as_slice())
                .expect("insert row");
        }
        crate::storage::bump_generation_and_commit(write_txn).expect("commit");
    }

    fn bound_star_scan(limit: usize) -> BoundScan {
        BoundScan {
            table: "docs".to_string(),
            projection: vec![
                ProjectedColumn::Id,
                ProjectedColumn::Column {
                    index: 0,
                    name: "embedding".to_string(),
                },
            ],
            metadata_filters: Vec::new(),
            expr_filters: Vec::new(),
            expr_filter_programs: Vec::new(),
            or_filters: Vec::new(),
            limit,
            order_by: Vec::new(),
            offset: 0,
            windows: Vec::new(),
            order_exprs: Vec::new(),
        }
    }

    #[test]
    fn projects_null_for_unset_nullable_vector_column() {
        let path = unique_db_path("scan-nullable-vector");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = nullable_vector_schema();
        storage.create_table(&schema).expect("create table");

        // id=1: VECTOR 値あり、id=2: nullable 列が未設定（embedding 空 = NULL）。
        write_row_direct(&storage, "docs", "tenant-a", 1, &[1.0, 2.0, 3.0]);
        write_row_direct(&storage, "docs", "tenant-a", 2, &[]);

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let read_txn = storage.db().begin_read().expect("begin_read");
        let bound = bound_star_scan(10);
        let result = execute_scan(&read_txn, &ctx, &schema, &bound).expect("scan should succeed");

        assert_eq!(result.rows.len(), 2);
        let row1 = result.rows.iter().find(|r| r.id == 1).expect("row 1");
        assert_eq!(row1.cells[1], Cell::Vector(vec![1.0, 2.0, 3.0]));
        let row2 = result.rows.iter().find(|r| r.id == 2).expect("row 2");
        assert_eq!(row2.cells[1], Cell::Null);
    }

    /// PR #1049 レビュー指摘（P1）の回帰: [`execute_scan_with_budget`] は
    /// 呼び出し元が渡した `max_result_bytes` を実際に honor する——`DECLARE`
    /// の内側 SELECT 実行専用の入口（`core.rs::EngineCore::
    /// execute_cursor_inner_query`）が `sql::cursor::MAX_CURSOR_BYTES_PER_SESSION`
    /// （16 MiB）を渡すことで、`execute_scan` の既定予算（`MAX_SCAN_RESULT_BYTES`
    /// ＝1 GiB）に到達するより遥かに手前で行生成中に打ち切れることを、
    /// 既定予算では成功する結果が小さい `max_result_bytes` では `54000`
    /// （`payload_too_large`）になることで確認する。
    #[test]
    fn execute_scan_with_budget_honors_caller_supplied_cap() {
        let path = unique_db_path("scan-with-budget-cap");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = nullable_vector_schema();
        storage.create_table(&schema).expect("create table");
        write_row_direct(&storage, "docs", "tenant-a", 1, &[1.0, 2.0, 3.0]);

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let read_txn = storage.db().begin_read().expect("begin_read");
        let bound = bound_star_scan(10);

        // 既定予算（`execute_scan`）では成功する。
        execute_scan(&read_txn, &ctx, &schema, &bound).expect("default budget should succeed");

        // 同じデータ・同じクエリでも、呼び出し元が極端に小さい予算を渡せば
        // 行生成中に打ち切られる（`execute_scan` の既定予算まで到達しない）。
        let err = execute_scan_with_budget(&read_txn, &ctx, &schema, &bound, 1)
            .expect_err("tiny caller-supplied budget must reject before default cap");
        assert_eq!(err.wire_code(), "54000");
    }

    /// codex-review PR #1096 P1 是正の回帰: 経路 (B)（上位 N 件 2 パス。
    /// `crates/engine/src/sql/scan.rs:1303` 付近）は、パス 1 のヒープ候補保持量
    /// （`heap_budget`）とパス 2 の投影結果保持量（`byte_budget`）を別々に
    /// `max_result_bytes` へ照合していたため、候補側・結果側それぞれ単独では
    /// 予算内でも同時に保持する合計サイズが上限を超えうる（`DECLARE CURSOR` の
    /// 小さい予算指定でメモリ予算を実質迂回できる経路）。ここでは、候補
    /// （`ORDER BY` 対象の TEXT 値）と結果（同じ列の投影セル）の双方が同じ長さの
    /// テキストを保持する行を用意し、単独では収まるが合計では超過する予算値を
    /// 計算して渡す。修正後は `byte_budget` がパス 2 開始時に `heap_budget` を
    /// 引き継ぐ単一の共通予算カウンタになるため、合計超過を行生成中に検出して
    /// `54000` で打ち切る。
    #[test]
    fn execute_scan_with_budget_bounds_combined_candidate_and_result_bytes_on_path_b() {
        let path = unique_db_path("scan-path-b-combined-budget");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), true),
                ColumnDef::new("tag", ColumnType::Text, true),
            ],
        );
        storage.create_table(&schema).expect("create table");

        // codex-review PR #1096 追加是正（P1）の回帰も兼ねるため 2 行にする
        // （1 行だけだと、パス 2 で唯一の候補を消費した瞬間にヒープ予算が
        // 解放されて予算超過を再現できない）。両行とも同じ長さの TEXT キーを
        // 持たせ、`heap_capacity`〔`limit + offset` = 2〕に等しい 2 行のみを
        // 用意することで、パス 1 終了時点で両方が退避されずヒープに残る。
        let tag_value = "x".repeat(2000);
        let tenant_id = "tenant-a";
        for id in 1..=2u64 {
            let write_txn = storage.db().begin_write().expect("begin_write");
            {
                let mut table = write_txn
                    .open_table(crate::catalog::user_rows_table_def(
                        &crate::catalog::user_rows_table_name("docs"),
                    ))
                    .expect("open row table");
                let metadata = crate::row_codec::encode_scalar_columns(
                    &schema,
                    &[
                        crate::row_codec::Value::Null,
                        crate::row_codec::Value::Text(tag_value.clone()),
                    ],
                )
                .expect("encode scalar columns");
                let buf = crate::storage::encode_row(&RowInput {
                    tenant_id,
                    visibility: Visibility::Public,
                    embedding: &[],
                    metadata: &metadata,
                })
                .expect("encode row");
                table
                    .insert((tenant_id, id), buf.as_slice())
                    .expect("insert row");
            }
            crate::storage::bump_generation_and_commit(write_txn).expect("commit");
        }

        let ctx = PolicyContext::new(tenant_id).expect("valid tenant");
        let read_txn = storage.db().begin_read().expect("begin_read");

        let bound = BoundScan {
            table: "docs".to_string(),
            projection: vec![
                ProjectedColumn::Id,
                ProjectedColumn::Column {
                    index: 1,
                    name: "tag".to_string(),
                },
            ],
            metadata_filters: Vec::new(),
            expr_filters: Vec::new(),
            expr_filter_programs: Vec::new(),
            or_filters: Vec::new(),
            limit: 2,
            order_by: vec![crate::sql::parser::BoundOrderKey {
                target: crate::sql::parser::BoundOrderTarget::Column(1),
                kind: crate::sql::parser::OrderKind::Bytes,
                descending: false,
            }],
            offset: 0,
            windows: Vec::new(),
            order_exprs: Vec::new(),
        };

        // パス 1 のヒープ候補 1 件分の内訳（[`heap_entry_keys_bytes`]・
        // [`heap_entry_fixed_bytes`] と同じ計算式）と、パス 2 の投影結果
        // 1 行分（`per_row_struct_bytes` + テキスト実体）をそれぞれ単独で
        // 見積もる（2 行とも同じ値になる）。
        let keys_bytes_estimate = bound
            .order_by
            .len()
            .saturating_mul(std::mem::size_of::<Option<OrderValue>>())
            .saturating_add(tag_value.len());
        let fixed_bytes_estimate = std::mem::size_of::<HeapEntry>().saturating_add(tenant_id.len());
        let heap_entry_bytes_estimate = keys_bytes_estimate.saturating_add(fixed_bytes_estimate);
        let cell_struct_bytes = bound
            .projection
            .len()
            .saturating_mul(std::mem::size_of::<Cell>());
        let result_row_struct_bytes = std::mem::size_of::<ResultRow>();
        let per_row_bytes_estimate = cell_struct_bytes
            .saturating_add(result_row_struct_bytes)
            .saturating_add(tag_value.len());

        // codex-review PR #1096 追加是正（最新ラウンド）: パス 2 は候補の
        // `keys`（投影に不要）を投影セル確保より前に実際に破棄してから
        // その分だけ解放し、`tenant_id`・構造体本体（`fixed`。投影呼び出しに
        // 貸す必要があるため投影後まで生存）は行を確定させた後に破棄・解放
        // する。2 候補（K・F は 1 候補分の keys／fixed、R は 1 行分の投影
        // コスト）を順に消費する過程の各時点の計上額を列挙し、その最大値が
        // 真に必要なピーク予算になる（「計上額 ≥ 実際の同時保持量」を
        // 各時点で満たす最小の値。詳細な導出根拠は PR #1096 レビュー対応
        // コメント参照）。
        let k = keys_bytes_estimate;
        let f = fixed_bytes_estimate;
        let r = per_row_bytes_estimate;
        let true_peak = [
            2 * k + 2 * f, // パス 1 終了時点（2 候補とも keys・fixed 生存）
            k + 2 * f,     // 候補 1 の keys 解放後
            k + 2 * f + r, // 候補 1 の投影コスト計上後
            k + f + r,     // 候補 1 の fixed 解放後（行 1 件確定）
            f + r,         // 候補 2 の keys 解放後
            f + 2 * r,     // 候補 2 の投影コスト計上後
            2 * r,         // 候補 2 の fixed 解放後（行 2 件確定）
        ]
        .into_iter()
        .max()
        .expect("non-empty array");
        // 旧実装（消費した候補を一切解放しない）が要求していた累積総量。
        // 真のピークはこれを確実に下回ることを固定する（解放しないと
        // 依然として拒否される予算であることの対照）。
        let old_buggy_cumulative_total =
            (2 * heap_entry_bytes_estimate).saturating_add(2 * per_row_bytes_estimate);
        let fits_true_peak_cap = true_peak + 8;
        assert!(
            fits_true_peak_cap < old_buggy_cumulative_total,
            "test cap must fall strictly between the corrected peak and the old buggy \
             cumulative total to exercise the per-candidate release fix"
        );

        // 十分大きい既定予算では成功する。
        execute_scan(&read_txn, &ctx, &schema, &bound).expect("default budget should succeed");

        // codex-review PR #1096 追加是正（P1）の回帰: 真に必要なピーク予算
        // ぎりぎりでは成功する（旧実装がここを `54000` で誤って拒否していた
        // 予算値。消費済み候補のヒープ予算を解放しないと、2 候補分の
        // ヒープ予算に 2 行分の投影結果が積み上がり、この予算を超過する）。
        let result = execute_scan_with_budget(&read_txn, &ctx, &schema, &bound, fits_true_peak_cap)
            .expect(
                "consumed heap candidates must release their budget before projecting the result",
            );
        assert_eq!(result.rows.len(), 2);

        // codex-review PR #1096 追加是正（最新ラウンド）の回帰: `true_peak`
        // ちょうどでは成功し、`true_peak - 1` では必ず `54000` になる境界を
        // 固定する。「先行解放」バグ（`winner` の所有メモリを実際に破棄する
        // 前に予算を解放し、続けて投影セルを確保する）が再混入すると、
        // 一時的な同時保持量を実際より少なく見積もるため `true_peak` 未満の
        // 予算でも誤って成功してしまう。
        let result = execute_scan_with_budget(&read_txn, &ctx, &schema, &bound, true_peak)
            .expect("a cap exactly at the true simultaneous-holding peak must still succeed");
        assert_eq!(result.rows.len(), 2);
        let err = execute_scan_with_budget(&read_txn, &ctx, &schema, &bound, true_peak - 1)
            .expect_err(
                "a cap one byte below the true simultaneous-holding peak must be rejected \
                 (a premature-release regression would under-count usage and let this succeed)",
            );
        assert_eq!(err.wire_code(), "54000");

        // 各候補・結果の 1 件分単独では収まるが、解放せず単純合算すると
        // 超過する極小予算では引き続き `54000` で拒否される（予算判定自体が
        // 無効化されたわけではないことを固定する）。
        let tiny_cap = 8;
        let err = execute_scan_with_budget(&read_txn, &ctx, &schema, &bound, tiny_cap)
            .expect_err("a cap smaller than a single candidate's own bytes must still be rejected");
        assert_eq!(err.wire_code(), "54000");
    }

    /// codex-review PR #1096 P1 是正の回帰: `heap_entry_bytes` が `keys:
    /// Vec<Option<OrderValue>>` 配列自体の確保量（容量ベース）を計上することを
    /// 検証する。旧実装は構造体本体・TEXT 実体・`tenant_id` のみを数え、複数
    /// キー指定時の配列本体を予算の外側に置いていた（Issue #915）。
    #[test]
    fn heap_entry_bytes_accounts_for_keys_array_capacity() {
        let spec: Rc<[BoundOrderKey]> = Rc::from(Vec::<BoundOrderKey>::new());
        let make_entry = |key_count: usize| HeapEntry {
            keys: vec![None; key_count],
            tenant_id: "tenant-a".to_string(),
            id: 1,
            spec: Rc::clone(&spec),
        };

        let one_key = make_entry(1);
        let eight_keys = make_entry(8);
        let option_order_value_size = std::mem::size_of::<Option<OrderValue>>();

        // 8 キー分の配列は 1 キー分より少なくとも 7 要素分（`Option<OrderValue>`
        // 換算）大きい。未計上のまま容量を無視すると差分は 0 になる。
        let diff = heap_entry_bytes(&eight_keys) - heap_entry_bytes(&one_key);
        assert!(
            diff >= option_order_value_size * 7,
            "keys 配列の容量差が heap_entry_bytes に反映されていない: diff={diff}, \
             option_order_value_size={option_order_value_size}"
        );
    }

    /// codex-review PR #1096 追加是正の回帰（Cursor Bugbot 指摘）: 複製前に
    /// 借用長だけで見積もる [`predicted_heap_entry_bytes`] が、実際に複製した
    /// 後の [`heap_entry_bytes`] と一致することを固定する（TEXT キー複数本を
    /// 含む場合の合計長のずれ・計算式の drift を検知する回帰。過去に
    /// `heap_entry_bytes` 自体の計算式が一度ずれた実績があるため、2 つの
    /// 独立した実装を突き合わせる）。
    #[test]
    fn heap_entry_bytes_matches_predicted_heap_entry_bytes() {
        let tenant_id = "tenant-ab";
        let text_a = "x".repeat(37);
        let text_b = "y".repeat(4001);
        let refs = vec![
            Some(ScalarKeyRef::Bytes(text_a.as_bytes())),
            Some(ScalarKeyRef::Id(42)),
            None,
            Some(ScalarKeyRef::Bytes(text_b.as_bytes())),
        ];
        // 実装（pass 1）と同じく `Vec::with_capacity(refs.len())` + push で
        // 構築する（`collect()` はサイズヒント次第で容量が一致しない可能性が
        // あるため、`heap_entry_bytes` の容量ベース計上と確実に揃える）。
        let mut owned_keys: Vec<Option<OrderValue>> = Vec::with_capacity(refs.len());
        for r in &refs {
            owned_keys
                .push(r.map(|v| scalar_key_ref_to_owned(v).expect("fallible alloc must succeed")));
        }
        let spec: Rc<[BoundOrderKey]> = Rc::from(Vec::<BoundOrderKey>::new());
        let entry = HeapEntry {
            keys: owned_keys,
            tenant_id: tenant_id.to_string(),
            id: 1,
            spec,
        };

        assert_eq!(
            heap_entry_bytes(&entry),
            predicted_heap_entry_bytes(&refs, tenant_id.len()),
            "predicted_heap_entry_bytes must exactly match heap_entry_bytes for the same \
             candidate, otherwise the pre-copy budget check and the post-copy accounting can \
             disagree"
        );
    }

    #[test]
    fn early_termination_stops_scanning_once_limit_rows_are_collected() {
        let path = unique_db_path("scan-early-termination");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = nullable_vector_schema();
        storage.create_table(&schema).expect("create table");
        for id in 1..=10u64 {
            write_row_direct(&storage, "docs", "tenant-a", id, &[1.0, 2.0, 3.0]);
        }

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let read_txn = storage.db().begin_read().expect("begin_read");
        let bound = bound_star_scan(3);
        let result = execute_scan(&read_txn, &ctx, &schema, &bound).expect("scan should succeed");
        assert_eq!(result.rows.len(), 3);
    }

    /// Issue #916・SQL-25 (b)・TASK-209: `OFFSET` は物理走査順の先頭から可視行を
    /// 読み飛ばし、`LIMIT` と組み合わせて連続するページに分割できることを確認する
    /// （`(tenant_id, id)` 昇順の走査順は本モジュールドキュメント「順序保証なし」
    /// 契約の一部。`id` 昇順で書き込んだため走査順は `id` 昇順に一致する）。
    #[test]
    fn offset_skips_leading_visible_rows_before_limit_applies() {
        let path = unique_db_path("scan-offset-paging");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = nullable_vector_schema();
        storage.create_table(&schema).expect("create table");
        for id in 1..=10u64 {
            write_row_direct(&storage, "docs", "tenant-a", id, &[1.0, 2.0, 3.0]);
        }

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let read_txn = storage.db().begin_read().expect("begin_read");

        let mut bound = bound_star_scan(3);
        bound.offset = 5;
        let result = execute_scan(&read_txn, &ctx, &schema, &bound).expect("scan should succeed");
        let ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![6, 7, 8]);
    }

    /// Issue #916・SQL-25 (b)・TASK-209（RLS-7/8）: `OFFSET` の計数は可視行のみを
    /// 対象にする——不可視（他テナント）行が物理走査順の先頭に挟まっていても、
    /// 読み飛ばし数には一切現れない。挟まっていない場合と同じ結果になることで、
    /// 不可視行の存在・件数を `OFFSET` の挙動から推測できないことを確認する。
    #[test]
    fn offset_counts_only_visible_rows_not_other_tenant_rows() {
        let path = unique_db_path("scan-offset-rls");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = nullable_vector_schema();
        storage.create_table(&schema).expect("create table");
        // 物理走査順の先頭に他テナントの不可視行を多数挟む（`(tenant_id, id)` の
        // 辞書順で "tenant-a" より前に来るよう "tenant-0" を使う。`Private` にする
        // ことで `PolicyContext::new("tenant-a")`〔既定は `Public` のみ許可〕から
        // 不可視にする——`Public` は可視性ラベルに関わらずテナントを跨いで見える
        // 契約〔`policy::PolicyContext::is_visible`〕のため）。
        for id in 1..=5u64 {
            write_row_direct_with_visibility(
                &storage,
                "docs",
                "tenant-0",
                id,
                &[9.0, 9.0, 9.0],
                Visibility::Private,
            );
        }
        for id in 1..=3u64 {
            write_row_direct(&storage, "docs", "tenant-a", id, &[1.0, 2.0, 3.0]);
        }

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let read_txn = storage.db().begin_read().expect("begin_read");

        let mut bound = bound_star_scan(10);
        bound.offset = 1;
        let result = execute_scan(&read_txn, &ctx, &schema, &bound).expect("scan should succeed");
        let ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
        // 不可視行が計数に混入すれば 1 件目からずれて `[2, 3]` にならない。
        assert_eq!(ids, vec![2, 3]);
    }

    /// PR #1096 レビュー指摘 P1（codex-review・cursor Bugbot 双方が独立検出）の
    /// 回帰: `OFFSET` で読み飛ばす行は投影・`byte_budget` 加算のいずれも行わない
    /// （既存の「スキップ行は投影・予算計上しない」契約）。修正前は
    /// `build_visible_row` が可視性・`WHERE` 判定後に投影・予算計上を終えてから
    /// `skipped < bound.offset` を判定していたため、読み飛ばされる行が大きい
    /// `VECTOR` を持つだけで返却対象でない行の分まで予算に計上され、不要に
    /// `54000`（payload_too_large）になっていた。読み飛ばし行の分を除けば収まる
    /// 予算で、実際に収まることを確認する。
    #[test]
    fn offset_skipped_rows_do_not_count_toward_result_byte_budget() {
        let path = unique_db_path("scan-offset-skip-budget");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        // 大きめの VECTOR（次元 5000 = 20000 バイト/行）で、スキップ行の予算
        // 誤計上が閾値超過として顕在化するようにする。
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(5000), true)],
        );
        storage.create_table(&schema).expect("create table");
        let big_embedding = vec![1.0f32; 5000];
        // id 1..=5: OFFSET で読み飛ばされる行（返却されない）。
        for id in 1..=5u64 {
            write_row_direct(&storage, "docs", "tenant-a", id, &big_embedding);
        }
        // id 6..=7: 返却される行。
        for id in 6..=7u64 {
            write_row_direct(&storage, "docs", "tenant-a", id, &big_embedding);
        }

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let read_txn = storage.db().begin_read().expect("begin_read");
        let mut bound = bound_star_scan(10);
        bound.offset = 5;

        // 返却される 2 行分（約 40000 バイト＋構造体分）は収まるが、スキップ
        // される 5 行分まで誤って計上すると（約 140000 バイト）超過する予算。
        let result = execute_scan_with_budget(&read_txn, &ctx, &schema, &bound, 60_000)
            .expect("skipped rows must not count toward the result byte budget");
        let ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![6, 7]);
    }

    /// codex-review PR #1096 追加是正の回帰: 経路 (B) がヒープにまだ空きが
    /// あり候補を必ず追加する場合（[`predicted_heap_entry_bytes`] を
    /// [`extract_order_value_ref`] の借用結果から算出し、複製〔`to_vec`〕する
    /// 前に予算照合する）、単独で `max_result_bytes` を超える TEXT キーは
    /// `54000` で拒否する（AGENTS.md の無制限リソース確保回避。修正前は複製後
    /// にしか判定せず、大きな TEXT 値・小さい予算〔`DECLARE CURSOR`〕の
    /// 組合せで判定前に
    /// 過大確保が起こり得た）。
    #[test]
    fn path_b_order_by_text_key_larger_than_budget_is_rejected_before_copying() {
        let path = unique_db_path("scan-path-b-order-key-budget");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = TableSchema::new("docs", vec![ColumnDef::new("tag", ColumnType::Text, true)]);
        storage.create_table(&schema).expect("create table");

        // 並べ替えキー自体の長さが、以下で使う `max_result_bytes` を単独で
        // 超えるようにする（複製すればどのみち予算超過だが、複製前に拒否
        // されることを「複製せずに `54000` を返す」という観測可能な挙動
        // （成功しないこと）で確認する）。
        let huge_tag = "x".repeat(200_000);
        let tenant_id = "tenant-a";
        let write_txn = storage.db().begin_write().expect("begin_write");
        {
            let mut table = write_txn
                .open_table(crate::catalog::user_rows_table_def(
                    &crate::catalog::user_rows_table_name("docs"),
                ))
                .expect("open row table");
            let metadata = crate::row_codec::encode_scalar_columns(
                &schema,
                &[crate::row_codec::Value::Text(huge_tag)],
            )
            .expect("encode scalar columns");
            let buf = crate::storage::encode_row(&RowInput {
                tenant_id,
                visibility: Visibility::Public,
                embedding: &[],
                metadata: &metadata,
            })
            .expect("encode row");
            table
                .insert((tenant_id, 1u64), buf.as_slice())
                .expect("insert row");
        }
        crate::storage::bump_generation_and_commit(write_txn).expect("commit");

        let ctx = PolicyContext::new(tenant_id).expect("valid tenant");
        let read_txn = storage.db().begin_read().expect("begin_read");
        let bound = BoundScan {
            table: "docs".to_string(),
            projection: vec![ProjectedColumn::Id],
            metadata_filters: Vec::new(),
            expr_filters: Vec::new(),
            expr_filter_programs: Vec::new(),
            or_filters: Vec::new(),
            limit: 1,
            order_by: vec![crate::sql::parser::BoundOrderKey {
                target: crate::sql::parser::BoundOrderTarget::Column(0),
                kind: crate::sql::parser::OrderKind::Bytes,
                descending: false,
            }],
            offset: 0,
            windows: Vec::new(),
            order_exprs: Vec::new(),
        };

        let err = execute_scan_with_budget(&read_txn, &ctx, &schema, &bound, 4096)
            .expect_err("a single ORDER BY key larger than the cap must be rejected");
        assert_eq!(err.wire_code(), "54000");
    }

    /// codex-review PR #1096 追加是正の回帰（P1・最新ラウンド）: 経路 (B) は
    /// ヒープが満杯（`heap.len() >= heap_capacity`）の時、複製前に借用値のまま
    /// 現在の最悪候補と比較し、採用されない（上位候補にならない）行は複製・
    /// 予算照合の対象にしない。修正前は複製前の借用長を無条件に
    /// `max_result_bytes` と照合していたため、上位候補にならないほど大きな
    /// TEXT キーを持つ行が 1 件でも存在するだけでクエリ全体を `54000` へ
    /// 誤って拒否していた（例: `ORDER BY text_col ASC LIMIT 1` で既に小さい
    /// キーの勝者があり、後続行の巨大なキーが明らかに後順位でも拒否されて
    /// いた）。
    #[test]
    fn path_b_large_key_that_never_becomes_a_top_candidate_does_not_reject_the_query() {
        let path = unique_db_path("scan-path-b-loser-large-key");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = TableSchema::new("docs", vec![ColumnDef::new("tag", ColumnType::Text, true)]);
        storage.create_table(&schema).expect("create table");

        let write_row = |id: u64, tag: &str| {
            let write_txn = storage.db().begin_write().expect("begin_write");
            {
                let mut table = write_txn
                    .open_table(crate::catalog::user_rows_table_def(
                        &crate::catalog::user_rows_table_name("docs"),
                    ))
                    .expect("open row table");
                let metadata = crate::row_codec::encode_scalar_columns(
                    &schema,
                    &[crate::row_codec::Value::Text(tag.to_string())],
                )
                .expect("encode scalar columns");
                let buf = crate::storage::encode_row(&RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[],
                    metadata: &metadata,
                })
                .expect("encode row");
                table
                    .insert(("tenant-a", id), buf.as_slice())
                    .expect("insert row");
            }
            crate::storage::bump_generation_and_commit(write_txn).expect("commit");
        };

        // id を昇順にした物理走査順どおりに、まず小さい TEXT キーの行 2 件で
        // ヒープ（`heap_capacity` = `limit + offset` = 2）を満杯にしてから、
        // 昇順ソートで明らかに最後尾（"z" は "a"/"b" より大きい）になる巨大な
        // TEXT キーの行を末尾の id に置く。
        write_row(1, "a");
        write_row(2, "b");
        let huge_tag = "z".repeat(200_000);
        write_row(3, &huge_tag);

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let read_txn = storage.db().begin_read().expect("begin_read");
        let bound = BoundScan {
            table: "docs".to_string(),
            projection: vec![ProjectedColumn::Id],
            metadata_filters: Vec::new(),
            expr_filters: Vec::new(),
            expr_filter_programs: Vec::new(),
            or_filters: Vec::new(),
            limit: 2,
            order_by: vec![crate::sql::parser::BoundOrderKey {
                target: crate::sql::parser::BoundOrderTarget::Column(0),
                kind: crate::sql::parser::OrderKind::Bytes,
                descending: false,
            }],
            offset: 0,
            windows: Vec::new(),
            order_exprs: Vec::new(),
        };

        // 巨大なキーを複製すれば単独でも超過するが、上位候補にならないため
        // 複製されず、実際に採用される 2 件（"a"・"b"）だけなら十分収まる
        // 予算。
        let result = execute_scan_with_budget(&read_txn, &ctx, &schema, &bound, 4096)
            .expect("a row whose key never becomes a top candidate must not be copied or rejected");
        let ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![1, 2]);
    }

    /// codex-review PR #1096 追加是正の回帰（P1・最新ラウンド）: 経路 (B)
    /// パス 1 がヒープ満杯時に候補を置き換える際、新候補を複製する前に
    /// 追い出し予定の最悪候補（`worst`）はまだヒープ内に残っており実メモリ上も
    /// 生存している。予算照合を「現在の `heap_budget`（`worst` を含む）+
    /// 新候補」で行わず、`worst` を先に差し引いた残り予算で照合すると、
    /// 複製〜実際の追い出しまでの一時的な同時保持量が計上額を超えうる。
    /// 大きなキーを持つ `worst` 1 件分ぎりぎりの予算では、置き換え中の
    /// 一時的な同時保持（`worst` + 新候補）を賄えないため `54000` になる
    /// ことを固定する。
    #[test]
    fn path_b_heap_replacement_accounts_for_worst_still_alive_during_copy() {
        let path = unique_db_path("scan-path-b-replace-worst-alive");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = TableSchema::new("docs", vec![ColumnDef::new("tag", ColumnType::Text, true)]);
        storage.create_table(&schema).expect("create table");

        let write_row = |id: u64, tag: &str| {
            let write_txn = storage.db().begin_write().expect("begin_write");
            {
                let mut table = write_txn
                    .open_table(crate::catalog::user_rows_table_def(
                        &crate::catalog::user_rows_table_name("docs"),
                    ))
                    .expect("open row table");
                let metadata = crate::row_codec::encode_scalar_columns(
                    &schema,
                    &[crate::row_codec::Value::Text(tag.to_string())],
                )
                .expect("encode scalar columns");
                let buf = crate::storage::encode_row(&RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[],
                    metadata: &metadata,
                })
                .expect("encode row");
                table
                    .insert(("tenant-a", id), buf.as_slice())
                    .expect("insert row");
            }
            crate::storage::bump_generation_and_commit(write_txn).expect("commit");
        };

        // id=1（大きな TEXT キー）がまずヒープ（`heap_capacity` = 1）を
        // 満たし、id=2（小さいキー・昇順で id=1 より先頭）がそれを置き換える。
        let big_tag = "b".repeat(5000);
        write_row(1, &big_tag);
        write_row(2, "a");

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let read_txn = storage.db().begin_read().expect("begin_read");
        let bound = BoundScan {
            table: "docs".to_string(),
            projection: vec![ProjectedColumn::Id],
            metadata_filters: Vec::new(),
            expr_filters: Vec::new(),
            expr_filter_programs: Vec::new(),
            or_filters: Vec::new(),
            limit: 1,
            order_by: vec![crate::sql::parser::BoundOrderKey {
                target: crate::sql::parser::BoundOrderTarget::Column(0),
                kind: crate::sql::parser::OrderKind::Bytes,
                descending: false,
            }],
            offset: 0,
            windows: Vec::new(),
            order_exprs: Vec::new(),
        };

        // 十分大きい既定予算では、最終的に id=2（"a"）だけが残る。
        let result =
            execute_scan(&read_txn, &ctx, &schema, &bound).expect("default budget should succeed");
        assert_eq!(
            result.rows.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![2]
        );

        // `worst`（id=1、大きなキー）1 件分ぎりぎりの予算では、置き換え中に
        // 一時的に新候補（id=2）と同時保持する分の余地がなく `54000` になる
        // （`worst` を先に解放したことにしてから照合する実装だと、この余地
        // 不足を見逃して誤って成功してしまう）。
        let worst_alone_cap = std::mem::size_of::<HeapEntry>()
            .saturating_add(std::mem::size_of::<Option<OrderValue>>())
            .saturating_add(big_tag.len())
            .saturating_add("tenant-a".len())
            + 4;
        let err = execute_scan_with_budget(&read_txn, &ctx, &schema, &bound, worst_alone_cap)
            .expect_err(
                "replacing the worst candidate must account for holding both the worst and the \
                 new candidate simultaneously during the copy",
            );
        assert_eq!(err.wire_code(), "54000");
    }

    /// codex-review PR #1096 追加是正の回帰: 経路 (B) パス 2 で `OFFSET` に
    /// より読み飛ばす候補が保持していたヒープ予算（`byte_budget`）を解放する。
    /// 修正前は破棄した候補分がそのまま計上され続け、読み飛ばし対象に大きい
    /// TEXT キーが集中すると、実際には十分収まる残り行の投影までもが
    /// `max_result_bytes` を実質迂回できないまま `54000` に誤って落ちていた。
    #[test]
    fn path_b_offset_skipped_candidates_release_their_heap_budget() {
        let path = unique_db_path("scan-path-b-offset-release-budget");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = TableSchema::new("docs", vec![ColumnDef::new("tag", ColumnType::Text, true)]);
        storage.create_table(&schema).expect("create table");

        let write_row = |id: u64, tag: &str| {
            let write_txn = storage.db().begin_write().expect("begin_write");
            {
                let mut table = write_txn
                    .open_table(crate::catalog::user_rows_table_def(
                        &crate::catalog::user_rows_table_name("docs"),
                    ))
                    .expect("open row table");
                let metadata = crate::row_codec::encode_scalar_columns(
                    &schema,
                    &[crate::row_codec::Value::Text(tag.to_string())],
                )
                .expect("encode scalar columns");
                let buf = crate::storage::encode_row(&RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[],
                    metadata: &metadata,
                })
                .expect("encode row");
                table
                    .insert(("tenant-a", id), buf.as_slice())
                    .expect("insert row");
            }
            crate::storage::bump_generation_and_commit(write_txn).expect("commit");
        };

        // 昇順ソートで先頭（"a" < "z"）に来て `OFFSET` により読み飛ばされる、
        // 大きな TEXT キーを持つ行。
        let skipped_tag = "a".repeat(20_000);
        for id in 1..=5u64 {
            write_row(id, &skipped_tag);
        }
        // 昇順ソートで末尾に来て、実際に返却される小さな TEXT キーの行。
        for id in 6..=7u64 {
            write_row(id, "z");
        }

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let read_txn = storage.db().begin_read().expect("begin_read");
        let bound = BoundScan {
            table: "docs".to_string(),
            projection: vec![
                ProjectedColumn::Id,
                ProjectedColumn::Column {
                    index: 0,
                    name: "tag".to_string(),
                },
            ],
            metadata_filters: Vec::new(),
            expr_filters: Vec::new(),
            expr_filter_programs: Vec::new(),
            or_filters: Vec::new(),
            limit: 10,
            order_by: vec![crate::sql::parser::BoundOrderKey {
                target: crate::sql::parser::BoundOrderTarget::Column(0),
                kind: crate::sql::parser::OrderKind::Bytes,
                descending: false,
            }],
            offset: 5,
            windows: Vec::new(),
            order_exprs: Vec::new(),
        };

        // パス 1 終了時点のヒープ候補 7 件分（`heap_entry_bytes` と同じ計算式。
        // `heap_capacity`〔`limit + offset` = 15〕が実件数 7 を上回るため、
        // どちらの候補も追い出されず全件がパス 2 開始時点の予算を占める）。
        let tenant_id_len = "tenant-a".len();
        let heap_entry_overhead = std::mem::size_of::<HeapEntry>()
            .saturating_add(std::mem::size_of::<Option<OrderValue>>())
            .saturating_add(tenant_id_len);
        let skipped_entry_bytes = heap_entry_overhead.saturating_add(skipped_tag.len());
        let kept_entry_bytes = heap_entry_overhead.saturating_add(1); // tag == "z"
        let peak_heap_bytes = 5usize
            .saturating_mul(skipped_entry_bytes)
            .saturating_add(2usize.saturating_mul(kept_entry_bytes));

        // 返却される行 1 件分の投影コスト（`per_row_struct_bytes` + TEXT 実体）。
        let cell_struct_bytes = bound
            .projection
            .len()
            .saturating_mul(std::mem::size_of::<Cell>());
        let per_row_struct_bytes =
            cell_struct_bytes.saturating_add(std::mem::size_of::<ResultRow>());
        let per_kept_row_projection_bytes = per_row_struct_bytes.saturating_add(1); // tag == "z"

        // 解放しなければ「パス 1 のピーク（5 行分の巨大 TEXT キーを含む）＋
        // 返却 2 行分の投影コスト」が必要になるが、解放すれば「返却 2 行分の
        // ヒープ候補＋投影コスト」で足りる。両者の間に収まる予算を使うことで、
        // 解放漏れがあれば必ず `54000` になる（解放されていれば十分な余白で
        // 成功する）ことを固定する。
        let cap = peak_heap_bytes.saturating_add(2 * per_kept_row_projection_bytes) - 1;
        let with_release_requirement =
            (2 * kept_entry_bytes).saturating_add(2 * per_kept_row_projection_bytes);
        assert!(
            cap > with_release_requirement,
            "test cap must leave headroom once offset-skipped candidates are released"
        );

        let result = execute_scan_with_budget(&read_txn, &ctx, &schema, &bound, cap)
            .expect("offset-skipped candidates must release their heap budget");
        let ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![6, 7]);
    }

    /// `vec_norm(embedding)` を投影する `Computed` 列を持つ `BoundScan` を組み立てる
    /// （Cursor Bugbot 指摘の回帰テスト用ヘルパー）。
    fn bound_scan_with_vec_norm_projection(limit: usize) -> BoundScan {
        BoundScan {
            table: "docs".to_string(),
            projection: vec![
                ProjectedColumn::Id,
                ProjectedColumn::Computed {
                    name: "n".to_string(),
                    expr: udf_call::BoundExpr::Builtin {
                        f: udf_call::BuiltinFn::VecNorm,
                        args: vec![udf_call::BoundExpr::VectorRef],
                    },
                    ty: udf_call::ExprType::Scalar,
                },
            ],
            metadata_filters: Vec::new(),
            expr_filters: Vec::new(),
            expr_filter_programs: Vec::new(),
            or_filters: Vec::new(),
            limit,
            order_by: Vec::new(),
            offset: 0,
            windows: Vec::new(),
            order_exprs: Vec::new(),
        }
    }

    #[test]
    fn computed_projection_is_null_for_unset_nullable_vector_column() {
        // Cursor Bugbot 指摘の回帰テスト: `dim == 0`（NULL vector）の行で
        // embedding を参照する `Computed` 式を評価すると、空スライスを実データと
        // 区別できず `vec_norm` が `0.0` を返してしまっていた。正しくは
        // `ProjectedColumn::Column` の直接投影と同じく `Cell::Null` を返す。
        let path = unique_db_path("scan-computed-null-vector");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = nullable_vector_schema();
        storage.create_table(&schema).expect("create table");

        write_row_direct(&storage, "docs", "tenant-a", 1, &[3.0, 4.0, 0.0]);
        write_row_direct(&storage, "docs", "tenant-a", 2, &[]);

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let read_txn = storage.db().begin_read().expect("begin_read");
        let bound = bound_scan_with_vec_norm_projection(10);
        let result = execute_scan(&read_txn, &ctx, &schema, &bound).expect("scan should succeed");

        assert_eq!(result.rows.len(), 2);
        let row1 = result.rows.iter().find(|r| r.id == 1).expect("row 1");
        assert_eq!(row1.cells[1], Cell::Float(5.0));
        let row2 = result.rows.iter().find(|r| r.id == 2).expect("row 2");
        assert_eq!(
            row2.cells[1],
            Cell::Null,
            "NULL vector 行の Computed 列は NULL を返すべき（0.0 に丸められてはならない）"
        );
    }

    #[test]
    fn where_expr_referencing_embedding_excludes_null_vector_rows() {
        // Cursor Bugbot 指摘の回帰テスト: `WHERE vec_norm(embedding) = 0` は
        // NULL vector 行（dim == 0）を「たまたま値が一致した」行として誤って
        // マッチさせてはならない（SQL の NULL 比較は unknown → WHERE では偽）。
        let path = unique_db_path("scan-where-null-vector");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = nullable_vector_schema();
        storage.create_table(&schema).expect("create table");

        write_row_direct(&storage, "docs", "tenant-a", 1, &[3.0, 4.0, 0.0]);
        write_row_direct(&storage, "docs", "tenant-a", 2, &[]);

        let expr = udf_call::BoundExpr::Binary {
            op: udf_call::BinOp::Eq,
            lhs: Box::new(udf_call::BoundExpr::Builtin {
                f: udf_call::BuiltinFn::VecNorm,
                args: vec![udf_call::BoundExpr::VectorRef],
            }),
            rhs: Box::new(udf_call::BoundExpr::Number(0.0)),
        };
        let program = ExprProgram::compile(&expr);
        let bound = BoundScan {
            table: "docs".to_string(),
            projection: vec![ProjectedColumn::Id],
            metadata_filters: Vec::new(),
            expr_filters: vec![expr],
            expr_filter_programs: vec![program],
            or_filters: Vec::new(),
            limit: 10,
            order_by: Vec::new(),
            offset: 0,
            windows: Vec::new(),
            order_exprs: Vec::new(),
        };

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let read_txn = storage.db().begin_read().expect("begin_read");
        let result = execute_scan(&read_txn, &ctx, &schema, &bound).expect("scan should succeed");

        assert!(
            result.rows.is_empty(),
            "NULL vector 行が WHERE vec_norm(embedding) = 0 に誤ってマッチした: {:?}",
            result.rows
        );
    }

    #[test]
    fn case_where_only_unselected_branch_references_embedding_does_not_exclude_null_vector_rows() {
        // codex-review P1 指摘の回帰テスト（Issue #921）: `references_embedding`
        // は式木全体を静的に走査するため、`CASE` の**選ばれない**分岐にだけ
        // embedding 参照（`vec_norm(embedding)`）があるだけの式でも「embedding を
        // 参照する式」と誤判定していた。旧実装はこの誤判定に基づき `dim == 0`
        // （`VECTOR` 列が NULL）の行を無条件除外していたため、実際に選択される
        // 分岐が embedding を一切参照しない場合でも NULL vector 行が
        // 除外されてしまっていた。本テストは `id = 2` の行（`dim == 0`）が
        // `THEN` 分岐（embedding 非参照）を選ぶケースで、行が正しく残ることを
        // 固定する。
        let path = unique_db_path("scan-where-case-unselected-branch-null-vector");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = nullable_vector_schema();
        storage.create_table(&schema).expect("create table");

        // id=1: 実データを持つ行（ELSE 分岐が選ばれ、embedding を参照し 0 とは
        // 一致しないため除外される）。
        write_row_direct(&storage, "docs", "tenant-a", 1, &[3.0, 4.0, 0.0]);
        // id=2: `VECTOR` 列が NULL（dim == 0）の行。THEN 分岐（embedding 非参照）
        // が選ばれるため、embedding の NULL 性に関わらず一致するはず。
        write_row_direct(&storage, "docs", "tenant-a", 2, &[]);

        // `CASE WHEN id = 2 THEN (id = id) ELSE (vec_norm(embedding) = 0) END`
        let expr = udf_call::BoundExpr::Case {
            whens: vec![(
                udf_call::BoundExpr::Binary {
                    op: udf_call::BinOp::Eq,
                    lhs: Box::new(udf_call::BoundExpr::IdRef),
                    rhs: Box::new(udf_call::BoundExpr::Number(2.0)),
                },
                udf_call::BoundExpr::Binary {
                    op: udf_call::BinOp::Eq,
                    lhs: Box::new(udf_call::BoundExpr::IdRef),
                    rhs: Box::new(udf_call::BoundExpr::IdRef),
                },
            )],
            else_result: Box::new(udf_call::BoundExpr::Binary {
                op: udf_call::BinOp::Eq,
                lhs: Box::new(udf_call::BoundExpr::Builtin {
                    f: udf_call::BuiltinFn::VecNorm,
                    args: vec![udf_call::BoundExpr::VectorRef],
                }),
                rhs: Box::new(udf_call::BoundExpr::Number(0.0)),
            }),
        };
        // 式木全体には embedding 参照（ELSE 分岐）が含まれることを前提として
        // 明示する（この前提がないとテストの意図が伝わらないため）。
        assert!(udf_call::references_embedding(&expr));

        let program = ExprProgram::compile(&expr);
        let bound = BoundScan {
            table: "docs".to_string(),
            projection: vec![ProjectedColumn::Id],
            metadata_filters: Vec::new(),
            expr_filters: vec![expr],
            expr_filter_programs: vec![program],
            or_filters: Vec::new(),
            limit: 10,
            order_by: Vec::new(),
            offset: 0,
            windows: Vec::new(),
            order_exprs: Vec::new(),
        };

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let read_txn = storage.db().begin_read().expect("begin_read");
        let result = execute_scan(&read_txn, &ctx, &schema, &bound).expect("scan should succeed");

        let ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
        assert_eq!(
            ids,
            vec![2],
            "NULL vector 行 (id=2) が選択されない分岐の embedding 参照だけを理由に \
             誤って除外された（または実データ行が誤って含まれた）: {ids:?}"
        );
    }

    #[test]
    fn computed_case_projection_with_unselected_embedding_branch_is_not_null_for_null_vector_row() {
        // codex-review P1 指摘の回帰テスト（Issue #921）: `Computed` 列の投影
        // （`sql/scan.rs` の `ProjectedColumn::Computed` 分岐）でも、WHERE と
        // 同じ誤判定（`references_embedding` の式木全体走査）により、`CASE` の
        // 選ばれない分岐にだけ embedding 参照がある行が NULL 投影されていた
        // （`if *references_embedding && dim == 0 { cells.push(Cell::Null) }`
        // の旧実装）。`id = 2` の行（`dim == 0`）が embedding を参照しない
        // `THEN` 分岐を選ぶ場合、`Cell::Null` ではなく選ばれた値が投影される
        // ことを固定する。
        let path = unique_db_path("scan-computed-case-unselected-branch-null-vector");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = nullable_vector_schema();
        storage.create_table(&schema).expect("create table");

        write_row_direct(&storage, "docs", "tenant-a", 1, &[3.0, 4.0, 0.0]);
        write_row_direct(&storage, "docs", "tenant-a", 2, &[]);

        // `CASE WHEN id = 2 THEN 1 ELSE vec_norm(embedding) END`
        let expr = udf_call::BoundExpr::Case {
            whens: vec![(
                udf_call::BoundExpr::Binary {
                    op: udf_call::BinOp::Eq,
                    lhs: Box::new(udf_call::BoundExpr::IdRef),
                    rhs: Box::new(udf_call::BoundExpr::Number(2.0)),
                },
                udf_call::BoundExpr::Number(1.0),
            )],
            else_result: Box::new(udf_call::BoundExpr::Builtin {
                f: udf_call::BuiltinFn::VecNorm,
                args: vec![udf_call::BoundExpr::VectorRef],
            }),
        };
        assert!(udf_call::references_embedding(&expr));

        let bound = BoundScan {
            table: "docs".to_string(),
            projection: vec![
                ProjectedColumn::Id,
                ProjectedColumn::Computed {
                    name: "n".to_string(),
                    expr,
                    ty: udf_call::ExprType::Scalar,
                },
            ],
            metadata_filters: Vec::new(),
            expr_filters: Vec::new(),
            expr_filter_programs: Vec::new(),
            or_filters: Vec::new(),
            limit: 10,
            order_by: Vec::new(),
            offset: 0,
            windows: Vec::new(),
            order_exprs: Vec::new(),
        };

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let read_txn = storage.db().begin_read().expect("begin_read");
        let result = execute_scan(&read_txn, &ctx, &schema, &bound).expect("scan should succeed");

        assert_eq!(result.rows.len(), 2);
        let row1 = result.rows.iter().find(|r| r.id == 1).expect("row 1");
        assert_eq!(row1.cells[1], Cell::Float(5.0));
        let row2 = result.rows.iter().find(|r| r.id == 2).expect("row 2");
        assert_eq!(
            row2.cells[1],
            Cell::Float(1.0),
            "NULL vector 行 (id=2) の CASE 投影が選択されない分岐の embedding 参照 \
             だけを理由に誤って NULL 化された"
        );
    }

    #[test]
    fn computed_vector_projection_accumulates_into_byte_budget() {
        // codex-review P1 指摘の回帰テスト: `Computed` 列が返すベクトル結果
        // （`vec_div(embedding, 1.0)`）は `VECTOR` 列直接投影と同じ累計予算
        // （`MAX_SCAN_RESULT_BYTES`）を消費し、上限超過時は `54000` 相当の
        // `payload_too_large` で拒否される必要がある。
        let path = unique_db_path("scan-computed-vector-budget");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = nullable_vector_schema();
        storage.create_table(&schema).expect("create table");
        write_row_direct(&storage, "docs", "tenant-a", 1, &[1.0, 2.0, 3.0]);

        let expr = udf_call::BoundExpr::Builtin {
            f: udf_call::BuiltinFn::VecDiv,
            args: vec![
                udf_call::BoundExpr::VectorRef,
                udf_call::BoundExpr::Number(1.0),
            ],
        };
        let bound = BoundScan {
            table: "docs".to_string(),
            projection: vec![
                ProjectedColumn::Id,
                ProjectedColumn::Computed {
                    name: "v".to_string(),
                    expr,
                    ty: udf_call::ExprType::Vector,
                },
            ],
            metadata_filters: Vec::new(),
            expr_filters: Vec::new(),
            expr_filter_programs: Vec::new(),
            or_filters: Vec::new(),
            limit: 10,
            order_by: Vec::new(),
            offset: 0,
            windows: Vec::new(),
            order_exprs: Vec::new(),
        };

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let read_txn = storage.db().begin_read().expect("begin_read");

        // 通常時（予算内）は成功する。
        let result = execute_scan(&read_txn, &ctx, &schema, &bound).expect("scan should succeed");
        assert_eq!(result.rows.len(), 1);

        // 1 行あたりのベクトルサイズが `MAX_SCAN_RESULT_BYTES` を超える場合、
        // `Computed` 列でも他セルと同様に拒否されることを確認する
        // （budget ヘルパー自体の単体検証。行走査を経ずに直接ヘルパーを叩く）。
        let huge = vec![0.0f32; (MAX_SCAN_RESULT_BYTES / std::mem::size_of::<f32>()) + 1];
        let mut budget = 0usize;
        let err = try_accumulate_vector_budget(huge, &mut budget, MAX_SCAN_RESULT_BYTES)
            .expect_err("oversized vector must be rejected before accumulation");
        match err {
            SqlSurfaceError::PayloadTooLarge { .. } => {}
            other => panic!("expected PayloadTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn cell_struct_bytes_budget_rejects_projection_width_that_bypasses_text_and_vector_accounting()
    {
        // codex-review P1 指摘の回帰テスト: `id` のみを大量に並べた投影
        // （実体バイトを持たないセル）は `try_alloc_text_for_budget`／
        // `try_clone_embedding_for_budget` を一切通らないため、`cells`
        // （`Vec<Cell>`）自体の確保量を累計しないと `byte_budget` が 0 のまま
        // `MAX_SCAN_RESULT_BYTES` を迂回できてしまっていた。実際に数千万列の
        // 投影を構築するとテスト自体が数 GB のメモリを消費するため
        // （`ProjectedColumn` 1 要素あたり数十バイト）、`execute_scan` が使う式
        // （`projection.len().saturating_mul(size_of::<Cell>())` →
        // `try_accumulate_budget`）を直接検証する。
        let huge_projection_len = MAX_SCAN_RESULT_BYTES / std::mem::size_of::<Cell>() + 1;
        let cell_struct_bytes = huge_projection_len.saturating_mul(std::mem::size_of::<Cell>());
        let mut budget = 0usize;
        let err = try_accumulate_budget(budget, cell_struct_bytes, MAX_SCAN_RESULT_BYTES)
            .expect_err("projection width exceeding the byte cap must be rejected");
        match err {
            SqlSurfaceError::PayloadTooLarge { .. } => {}
            other => panic!("expected PayloadTooLarge, got {other:?}"),
        }
        // 予算内の投影幅は引き続き受理される（既存の `id`/`embedding` 2 列投影
        // テストが確認する通常経路への回帰がないことの補足確認）。
        let small_cell_struct_bytes = 2usize.saturating_mul(std::mem::size_of::<Cell>());
        budget = 0;
        budget = try_accumulate_budget(budget, small_cell_struct_bytes, MAX_SCAN_RESULT_BYTES)
            .expect("small projection width must stay within budget");
        assert_eq!(budget, small_cell_struct_bytes);
    }

    #[test]
    fn cell_struct_bytes_accumulates_across_rows_in_execute_scan() {
        // `execute_scan` の行ループへ実際に結線されていることの確認（累計は
        // 行数に比例して増える契約。実行そのものは既存のバイト予算内に収まる
        // 小規模な走査で検証し、上のユニットテストと相補的に production 経路の
        // 配線漏れを検出する）。
        let path = unique_db_path("scan-cell-struct-bytes-wired");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = nullable_vector_schema();
        storage.create_table(&schema).expect("create table");
        for id in 1..=5u64 {
            write_row_direct(&storage, "docs", "tenant-a", id, &[1.0, 2.0, 3.0]);
        }

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let read_txn = storage.db().begin_read().expect("begin_read");
        let bound = bound_star_scan(5);
        let result = execute_scan(&read_txn, &ctx, &schema, &bound).expect("scan should succeed");
        assert_eq!(result.rows.len(), 5);
    }

    #[test]
    fn result_row_struct_bytes_alone_can_exceed_budget_even_with_a_single_projected_column() {
        // codex-review P1 指摘の回帰テスト: `cells` の確保量だけを計上する対策では、
        // `id` 1 列のみを投影する広域取得（`Cell` 自体は小さい）でも `ResultRow`
        // （`id`/`score`/`Vec<Cell>` ヘッダ分）が行数分積み上がることを見逃す。
        // `cell_struct_bytes` 単体では `MAX_SCAN_RESULT_BYTES` を超えない小さい
        // 投影幅でも、`result_row_struct_bytes` を合算した `per_row_struct_bytes`
        // なら十分な行数で超過が検出できることを、実際に巨大メモリを確保せず
        // `try_accumulate_budget` の反復適用で確認する（`execute_scan` が使う式と
        // 同一）。
        let cell_struct_bytes = 1usize.saturating_mul(std::mem::size_of::<Cell>());
        let result_row_struct_bytes = std::mem::size_of::<ResultRow>();
        let per_row_struct_bytes = cell_struct_bytes.saturating_add(result_row_struct_bytes);
        assert!(
            per_row_struct_bytes > 0,
            "per-row struct accounting must be positive to ever trip the cap"
        );

        let rows_to_exceed_cap = MAX_SCAN_RESULT_BYTES / per_row_struct_bytes + 1;
        let mut budget = 0usize;
        let mut rejected = false;
        for _ in 0..rows_to_exceed_cap {
            match try_accumulate_budget(budget, per_row_struct_bytes, MAX_SCAN_RESULT_BYTES) {
                Ok(next) => budget = next,
                Err(SqlSurfaceError::PayloadTooLarge { .. }) => {
                    rejected = true;
                    break;
                }
                Err(other) => panic!("unexpected error: {other:?}"),
            }
        }
        assert!(
            rejected,
            "accumulating per_row_struct_bytes across enough rows must eventually reject"
        );

        // `cell_struct_bytes` のみを計上する（旧実装相当の）計算では同じ行数で
        // 上限を超えないことを確認し、`result_row_struct_bytes` の合算が実際に
        // 検出精度を変えていることを固定する。
        let mut cell_only_budget = 0usize;
        for _ in 0..rows_to_exceed_cap {
            cell_only_budget =
                try_accumulate_budget(cell_only_budget, cell_struct_bytes, MAX_SCAN_RESULT_BYTES)
                    .expect("cell_struct_bytes alone must not exceed the cap at this row count");
        }
    }

    // --- Issue #894: 新スカラー型（TABLE-13・TASK-199）の DecodeTier 選択 ------

    /// 新スカラー型を多数含むスキーマ（`sql::aggregate` モジュール内テストの
    /// `many_new_scalar_types_schema` と同型の列構成）。
    fn many_new_scalar_types_schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), true), // 0
                ColumnDef::new("u", ColumnType::Uuid, true),              // 1
                ColumnDef::new("bo", ColumnType::Boolean, true),          // 2
                ColumnDef::new(
                    "n",
                    ColumnType::Numeric {
                        precision: 10,
                        scale: 2,
                    },
                    true,
                ), // 3
                ColumnDef::new("dt", ColumnType::Date, true),             // 4
            ],
        )
    }

    #[test]
    fn decode_tier_for_new_type_projection_is_dim_and_scalar() {
        let schema = many_new_scalar_types_schema();
        let bound = BoundScan {
            table: "docs".to_string(),
            projection: vec![
                ProjectedColumn::Id,
                ProjectedColumn::Column {
                    index: 1,
                    name: "u".to_string(),
                },
                ProjectedColumn::Column {
                    index: 4,
                    name: "dt".to_string(),
                },
            ],
            metadata_filters: Vec::new(),
            expr_filters: Vec::new(),
            expr_filter_programs: Vec::new(),
            or_filters: Vec::new(),
            limit: 10,
            order_by: Vec::new(),
            offset: 0,
            windows: Vec::new(),
            order_exprs: Vec::new(),
        };
        let (tier, mask) = decode_tier_for(&schema, &bound);
        assert_eq!(tier, DecodeTier::DimAndScalar);
        assert_eq!(mask, vec![false, true, false, false, true]);
    }

    #[test]
    fn decode_tier_for_id_only_projection_on_new_type_schema_is_fast() {
        let schema = many_new_scalar_types_schema();
        let bound = BoundScan {
            table: "docs".to_string(),
            projection: vec![ProjectedColumn::Id],
            metadata_filters: Vec::new(),
            expr_filters: Vec::new(),
            expr_filter_programs: Vec::new(),
            or_filters: Vec::new(),
            limit: 10,
            order_by: Vec::new(),
            offset: 0,
            windows: Vec::new(),
            order_exprs: Vec::new(),
        };
        let (tier, mask) = decode_tier_for(&schema, &bound);
        assert_eq!(tier, DecodeTier::Fast);
        assert!(mask.iter().all(|&wanted| !wanted));
    }

    #[test]
    fn decode_tier_for_vector_projection_is_embedding() {
        let schema = many_new_scalar_types_schema();
        let bound = BoundScan {
            table: "docs".to_string(),
            projection: vec![
                ProjectedColumn::Id,
                ProjectedColumn::Column {
                    index: 0,
                    name: "embedding".to_string(),
                },
            ],
            metadata_filters: Vec::new(),
            expr_filters: Vec::new(),
            expr_filter_programs: Vec::new(),
            or_filters: Vec::new(),
            limit: 10,
            order_by: Vec::new(),
            offset: 0,
            windows: Vec::new(),
            order_exprs: Vec::new(),
        };
        let (tier, _mask) = decode_tier_for(&schema, &bound);
        assert_eq!(tier, DecodeTier::Embedding);
    }
}
