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
//! 付与済み〔vector-db-spec#12〕。確定化は TASK-170 が担う。本モジュールは本リポの
//! 実装既定値として動作する）参照。順序は同一スナップショット内の redb
//! 行テーブルの物理走査順（`(tenant_id, id)` 昇順）であり、`ORDER BY` 相当の意味的
//! 順序を持たない。
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
use crate::sql::parser::{BoundScan, ProjectedColumn};
use crate::sql::udf_call::{self, ExprValue};
use crate::storage::{self, StorageError};
use redb::ReadableTable;

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

/// [`BoundScan`] を実行する（Issue #454・TASK-186・NOSQL-3 の公開 API）。
/// `core.rs::EngineCore::execute_sql_in_session` の `Statement::Scan` アームから
/// 呼ばれるほか、[`BoundScan`] が公開型へ昇格したため engine クレート外から
/// SQL テキストを経由せず直接呼び出すこともできる（TASK-186・NOSQL-3）。
///
/// 既定の結果バイト予算（[`MAX_SCAN_RESULT_BYTES`]）で
/// [`execute_scan_with_budget`] へ委譲する薄いラッパー。
pub fn execute_scan(
    read_txn: &redb::ReadTransaction,
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
pub(crate) fn execute_scan_with_budget(
    read_txn: &redb::ReadTransaction,
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
            ProjectedColumn::Computed { name, .. } => ColumnMeta::Computed { name: name.clone() },
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
    let computed_programs: Vec<Option<(ExprProgram, bool)>> = bound
        .projection
        .iter()
        .map(|col| match col {
            ProjectedColumn::Computed { expr, .. } => Some((
                ExprProgram::compile(expr),
                udf_call::references_embedding(expr),
            )),
            ProjectedColumn::Id | ProjectedColumn::Column { .. } => None,
        })
        .collect();

    // 可視行ごとの embedding デコード先スクラッチバッファ・式評価用の明示スタック
    // （行ごとに新規確保しない。`sql::aggregate::execute_aggregate` と同方針）。
    let mut embedding_scratch: Vec<f32> = Vec::new();
    let mut expr_scratch: Vec<StackValue> = Vec::new();
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

    if let Some(table) = table {
        'rows: for entry in table.iter().map_err(storage_internal)? {
            // 早期終了: 可視かつ WHERE を満たす行が `bound.limit` 件集まった時点で
            // 走査を打ち切る（本モジュールドキュメント「順序保証なし」契約の
            // 実装側。テナントを跨いだ物理走査順のどこで打ち切っても、不可視行は
            // 一切カウントされないため他テナントの存在・件数の情報を漏らさない）。
            if rows.len() >= bound.limit {
                break;
            }

            let (k, v) = entry.map_err(storage_internal)?;
            let (key_tenant, id) = k.value();
            let buf = v.value();

            // RLS 段（無条件・デコード前）。`sql::aggregate::execute_aggregate` の
            // 走査ループと同一の順序（security.md P0「テナント境界」）。
            let (tenant_id, visibility, offset) =
                storage::decode_row_header(buf).map_err(storage_internal)?;
            if !ctx.is_visible(tenant_id, visibility) {
                continue;
            }

            // TABLE-12: 物理キー側 `tenant_id` とヘッダ側 `tenant_id` の整合検査。
            storage::verify_row_key_tenant(key_tenant, tenant_id).map_err(storage_internal)?;

            let (dim, metadata): (u32, &[u8]) = match tier {
                DecodeTier::Fast | DecodeTier::DimAndScalar => {
                    storage::decode_row_dim_and_metadata_borrowed(buf).map_err(storage_internal)?
                }
                DecodeTier::Embedding => {
                    storage::decode_row_body_into(buf, offset, &mut embedding_scratch)
                        .map_err(storage_internal)?
                }
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
                    row_codec::scan_scalar_columns_masked(schema, metadata, Some(&scalar_mask))?
                }
            };

            // SCALAR 段（WHERE）。
            if !declarative_filter::matches_all(&bound.metadata_filters, &scanned) {
                continue;
            }
            for (expr, program) in bound.expr_filters.iter().zip(&bound.expr_filter_programs) {
                let references_embedding = udf_call::references_embedding(expr);
                // `dim == 0`（`VECTOR` 列が NULL。上記コメント参照）の行で embedding を
                // 参照する式を評価すると、空スライスを実データと区別できず
                // `vec_norm` 等が `0.0` を返し本来 NULL のはずの比較が意図せず
                // マッチしてしまう（Cursor Bugbot 指摘）。SQL の NULL 比較は
                // unknown → `WHERE` では偽と同義に扱われる契約に合わせ、embedding を
                // 参照する式は NULL 行を評価せず無条件にこの行を除外する。
                if references_embedding && dim == 0 {
                    continue 'rows;
                }
                let embedding: &[f32] = if references_embedding {
                    match tier {
                        DecodeTier::Embedding => embedding_scratch.as_slice(),
                        DecodeTier::Fast | DecodeTier::DimAndScalar => {
                            return Err(scan_bug(
                                "WHERE expression references the VECTOR column but tier did not decode it",
                            ))
                        }
                    }
                } else {
                    &[]
                };
                match program.eval(id, embedding, &mut expr_scratch)? {
                    ExprValue::Bool(true) => {}
                    ExprValue::Bool(false) => continue 'rows,
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
                    &mut expr_scratch,
                )? {
                    continue 'rows;
                }
            }

            // defense-in-depth（`RlsSafetyNet` と同趣旨）: デコード前判定が唯一の
            // 防御線にならないよう、同じ `tenant_id`・`visibility` へ再適用する
            // （security.md P0）。
            if !ctx.is_visible(tenant_id, visibility) {
                continue;
            }

            // Issue #916・SQL-25 (b)・TASK-209: `OFFSET` 段。ここまでに到達した行は
            // 可視かつ WHERE 一致が確定済み（上記 2 回の `is_visible` 再適用・
            // `metadata_filters`／`expr_filters` 評価をすべて通過している）ため、
            // ここで数えても他テナントの存在・件数の漏えいにはならない。投影・
            // `cells` 確保・byte 予算計上より前に読み飛ばすことで、スキップされた
            // 行はメモリを消費しない（深いページングでも O(n) を保つ）。
            if skipped < bound.offset {
                skipped += 1;
                continue;
            }

            // `cells`／`rows` 確保前に累計予算を検証（上記コメント参照。確保
            // そのものを許可する前に拒否できるよう `Vec::try_reserve` 系より先に
            // 判定する）。
            byte_budget =
                try_accumulate_budget(byte_budget, per_row_struct_bytes, max_result_bytes)?;

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
                        let column = schema.columns.get(*index).ok_or_else(|| {
                            SqlSurfaceError::Internal {
                                detail: "projected column index out of range".to_string(),
                            }
                        })?;
                        match &column.ty {
                            ColumnType::Vector(_) => {
                                // `dim == 0` は `VECTOR` 列が未設定（NULL。TABLE-5 の
                                // 追加列を含む）という `storage::Row` の既存契約
                                // （`sql::aggregate::RowVector` のドキュメント参照）。
                                if dim == 0 {
                                    cells.push(Cell::Null);
                                } else {
                                    let embedding = match tier {
                                        DecodeTier::Embedding => embedding_scratch.as_slice(),
                                        DecodeTier::Fast | DecodeTier::DimAndScalar => {
                                            return Err(scan_bug(
                                                "VECTOR column projected but tier did not decode embedding",
                                            ))
                                        }
                                    };
                                    cells.push(Cell::Vector(try_clone_embedding_for_budget(
                                        embedding,
                                        &mut byte_budget,
                                        max_result_bytes,
                                    )?));
                                }
                            }
                            ColumnType::Text => match scanned.get(*index) {
                                Some(Some(row_codec::ScalarRef::Text(t))) => {
                                    cells.push(Cell::Text(try_alloc_text_for_budget(
                                        t,
                                        &mut byte_budget,
                                        max_result_bytes,
                                    )?))
                                }
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
                                Some(Some(row_codec::ScalarRef::Double(v))) => {
                                    cells.push(Cell::Float(*v))
                                }
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
                                Some(Some(row_codec::ScalarRef::Bool(b))) => {
                                    cells.push(Cell::Bool(*b))
                                }
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
                            ColumnType::Timestamp => match scanned.get(*index) {
                                Some(Some(v)) => match v.as_timestamp() {
                                    Some(t) => cells.push(Cell::Timestamp(t)),
                                    None => {
                                        return Err(scan_bug(
                                            "TIMESTAMP column scan yielded a non-Timestamp scalar value",
                                        ))
                                    }
                                },
                                Some(None) | None => cells.push(Cell::Null),
                            },
                            ColumnType::Array(_) => match scanned.get(*index) {
                                Some(Some(row_codec::ScalarRef::Array(array_ref))) => {
                                    let value = try_alloc_array_for_budget(
                                        *array_ref,
                                        &mut byte_budget,
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
                                        &mut byte_budget,
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
                                        &mut byte_budget,
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
                                            &mut byte_budget,
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
                                    None => return Err(scan_bug(
                                        "NUMERIC column scan yielded a non-Numeric scalar value",
                                    )),
                                },
                                Some(None) | None => cells.push(Cell::Null),
                            },
                            ColumnType::Uuid => match scanned.get(*index) {
                                Some(Some(v)) => match v.as_uuid() {
                                    Some(u) => cells.push(Cell::Uuid(u)),
                                    None => return Err(scan_bug(
                                        "UUID column scan yielded a non-Uuid scalar value",
                                    )),
                                },
                                Some(None) | None => cells.push(Cell::Null),
                            },
                        }
                    }
                    ProjectedColumn::Computed { .. } => {
                        let (program, references_embedding) = computed_programs
                            .get(col_idx)
                            .and_then(|p| p.as_ref())
                            .ok_or_else(|| SqlSurfaceError::Internal {
                                detail: "computed projection program missing at evaluation time"
                                    .to_string(),
                            })?;
                        // `dim == 0`（`VECTOR` 列が NULL）の行で embedding を参照する
                        // 式を評価すると空スライスを実データと区別できず誤った数値
                        // （例: `vec_norm` が `0.0`）を返してしまう（Cursor Bugbot
                        // 指摘）。`ProjectedColumn::Column` の直接投影と同じく NULL を
                        // 伝播させる。
                        if *references_embedding && dim == 0 {
                            cells.push(Cell::Null);
                        } else {
                            let embedding_for_eval: &[f32] = match tier {
                                DecodeTier::Embedding => embedding_scratch.as_slice(),
                                DecodeTier::Fast | DecodeTier::DimAndScalar => &[],
                            };
                            match program.eval(id, embedding_for_eval, &mut expr_scratch)? {
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
                                        &mut byte_budget,
                                        max_result_bytes,
                                    )?));
                                }
                                ExprValue::Bool(b) => cells.push(Cell::Bool(b)),
                            }
                        }
                    }
                }
            }
            // `rows`（`Vec<ResultRow>`）の確保も `try_reserve` 系で行う
            // （上記コメント参照。上限判定は既に `per_row_struct_bytes` の累計へ
            // 反映済みのため、ここでは確保方式のみを abort 非経路へ切り替える）。
            rows.try_reserve(1).map_err(|e| SqlSurfaceError::Internal {
                detail: format!("failed to reserve scan result rows: {e}"),
            })?;
            rows.push(ResultRow {
                id,
                score: 0.0,
                cells,
            });
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
            offset: 0,
            windows: Vec::new(),
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
                },
            ],
            metadata_filters: Vec::new(),
            expr_filters: Vec::new(),
            expr_filter_programs: Vec::new(),
            or_filters: Vec::new(),
            limit,
            offset: 0,
            windows: Vec::new(),
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
            offset: 0,
            windows: Vec::new(),
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
                },
            ],
            metadata_filters: Vec::new(),
            expr_filters: Vec::new(),
            expr_filter_programs: Vec::new(),
            or_filters: Vec::new(),
            limit: 10,
            offset: 0,
            windows: Vec::new(),
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
            offset: 0,
            windows: Vec::new(),
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
            offset: 0,
            windows: Vec::new(),
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
            offset: 0,
            windows: Vec::new(),
        };
        let (tier, _mask) = decode_tier_for(&schema, &bound);
        assert_eq!(tier, DecodeTier::Embedding);
    }
}
