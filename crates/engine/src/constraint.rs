//! 行単位の宣言的制約——テナント内一意性制約（`PRIMARY KEY` 宣言〔Issue #903〕・
//! UNIQUE 制約〔Issue #905〕）と `CHECK` 制約〔Issue #906〕。いずれも TABLE-16・
//! TASK-204——と `FOREIGN KEY` 制約（TABLE-17・TASK-205、Issue #907）を検査する
//! 単一の検査点（書き込んだ行の検査の入口は [`enforce_row_constraints_in_txn`]、
//! 参照先側〔削除・更新された行を参照する行が残っていないか〕の入口は
//! [`enforce_referencing_rows_in_txn`]）。
//!
//! `CHECK` 制約は書き込んだ各行を同一 write トランザクション内で読み戻し、
//! `sql::check_constraint::CompiledChecks`（`WHERE` と同じ束縛・評価器を再利用）で
//! 評価する。一意性制約より**先**に評価する（両方に違反する行は `23514`。
//! PostgreSQL の評価順序に倣う）。`CHECK` を宣言しないテーブルはコンパイル自体を
//! 行わない（コストゼロ）。
//!
//! 呼び出し元は `tenant.rs` の各書き込み関数（`insert_*_unchecked`・
//! `upsert_typed_rows_unchecked`・`update_row_unchecked`・
//! `update_row_columns_unchecked`・`update_rows_where_unchecked`・
//! `replace_typed_rows_by_text_key`）で、いずれも「`operation_id` 台帳への記録 →
//! 行の書き込み」の**後**・テーブル世代 bump・commit の**前**に同一 write
//! トランザクション内から呼ぶ契約とする（RECOVER-12・TABLE-16。台帳照合を先に
//! 行うことで、`operation_id` の再送判定（`23505`／`22023`）が本検査より優先
//! されることを保証する）。明示トランザクション（SQL-31・TASK-221）中の書き込みも
//! `tenant::WriteTarget::InTxn` が共有 write トランザクションを渡すため同じ
//! 検査点を通り、redb の write トランザクションは自身が書いた未 commit の行を
//! 読めるため、同一トランザクション内の先行文が書いた行も母集合に含まれる。
//! `catalog.rs` の生書き込み API（`#[cfg(test)]` 限定・production では到達不能）は
//! この検査点を経由しない（既知のギャップ。`docs/design/sql-primary-key.md` 参照）。
//!
//! 主キーも UNIQUE 制約も宣言しないテーブル（大多数）は検査対象のキーが 0 個に
//! なり、呼び出しは即座に成功する（コストゼロ）。
//!
//! # キーの種類と NULL の扱い
//!
//! - 主キー（`schema.primary_key()`）: 構成列は `nullable == false`
//!   （`validate_schema` が強制）であり、NULL・列欠落は内部矛盾として
//!   fail-closed に拒否する。
//! - UNIQUE 制約（`schema.unique_constraints()`）: NULLS DISTINCT。構成列の
//!   いずれかが NULL の行はその制約の検査対象外とする（NULL 同士は衝突しない）。
//!
//! 等価判定は型タグ＋長さ前置の正準キーバイト列で行う。許可型は主キーが
//! `ColumnType::is_primary_key_allowed`、UNIQUE 制約はその上位集合
//! `ColumnType::is_unique_constraint_allowed`（Issue #1073 で REAL・
//! DOUBLE PRECISION・NUMERIC・JSON／JSONB・配列型を追加）を使う——2 つの
//! 許可リストを持つ理由は `ColumnType::is_primary_key_allowed` の doc 参照。
//!
//! # 実装方式（永続一意索引・Issue #1070）
//!
//! 旧実装（テナント全行の毎文線形走査）は永続一意索引
//! （`user_uniq/{table}`。[`unique_index`] モジュール）へ置き換え済み。
//! 判定は正引きサブキーの点照会（O(k・log n)。k は宣言済み一意キー数）で行い、
//! 検査コストはテナントの保有行数に比例しない（`docs/design/unique-index.md`
//! 参照。設計・不変条件・後方互換の詳細は [`unique_index`] モジュール
//! ドキュメント参照）。既存 DB（索引テーブル未作成）では各テナントの
//! 最初の書き込みで 1 回だけ自テナントの既存行をバックフィルする
//! （その回のみ O(n)）。
//!
//! `tenant::enumerate_dml_candidates` が持つ [`crate::tenant`] 内部の総走査上限
//! （`MAX_SCANNED_ROWS`）は意図的に継承しない——継承すると、その上限を超える
//! 行数を既に保有するテナントが一意キー宣言テーブルへ一切書き込めなくなる
//! fail-closed 過ぎる制約になってしまうため（バックフィルはテナントごとに
//! 高々 1 回であり、以降の書き込みは索引照会のみで完結する）。
//!
//! # テナント境界（RLS-9・RLS-10 (c)）
//!
//! 走査は常にサーバー側導出テナント（`ctx.tenant_id()` 由来の物理キー範囲）に
//! 閉じ、他テナントの行キー・値には一切触れない。判定母集合はテナントが所有する
//! **全行**（`Public`／`Private` を問わない。可視性フィルタで縮めない）とし、
//! 二次索引（`ScalarIndex`）・世代整合キャッシュのいずれも流用しない生の走査で
//! 判定する——キャッシュは「クエリ時点で可視だった行」を前提に構築されており、
//! 一意性制約はテナントが所有する不可視行との衝突も防がなければならないため
//! （TABLE-16・RLS-10 (c)）。違反時のエラー（`TenantWriteError::UniqueViolation`）
//! はキー値・列名・行 id・テナント名を含まない固定文言。

mod unique_index;

use crate::catalog::{CatalogError, ColumnType, ForeignKeyDef, ForeignKeyMatch, TableSchema};
use crate::row_codec::ScalarRef;
use crate::tenant::TenantWriteError;
use redb::ReadableTable;
use std::collections::{BTreeSet, HashSet};

/// 一意キー 1 個分の NULL の扱い。
#[derive(Clone, Copy, PartialEq, Eq)]
enum NullPolicy {
    /// 主キー: 構成列の NULL は内部矛盾（`nullable == false` が不変条件）。
    Reject,
    /// UNIQUE 制約: NULL を含む行はその制約の検査対象外（NULLS DISTINCT）。
    Skip,
}

/// 一意キー 1 個分の検査仕様（構成列の論理インデックスと NULL の扱い）。
struct KeySpec {
    indices: Vec<usize>,
    null_policy: NullPolicy,
}

/// `schema` が宣言する全一意キー（主キー → UNIQUE 制約の宣言順）の検査仕様と、
/// それらの構成列の和集合マスク（`row_codec::scan_scalar_columns_masked` へ
/// 渡す）を組み立てる。列名解決の失敗は `validate_schema`（`create_table`／
/// カタログ decode の両方が通す）が起こり得ないことを保証する不変条件だが、
/// 内部矛盾があっても panic せず fail-closed に拒否する。
fn key_specs(schema: &TableSchema) -> Result<(Vec<KeySpec>, Vec<bool>), CatalogError> {
    let resolve = |name: &String| -> Result<usize, CatalogError> {
        schema
            .columns
            .iter()
            .position(|c| &c.name == name)
            .ok_or_else(|| {
                CatalogError::Invalid(
                    "unique key column not found in live schema columns".to_string(),
                )
            })
    };
    let mut specs: Vec<KeySpec> = Vec::new();
    if let Some(pk_cols) = schema.primary_key() {
        specs.push(KeySpec {
            indices: pk_cols.iter().map(resolve).collect::<Result<_, _>>()?,
            null_policy: NullPolicy::Reject,
        });
    }
    for constraint in schema.unique_constraints() {
        specs.push(KeySpec {
            indices: constraint
                .columns()
                .iter()
                .map(resolve)
                .collect::<Result<_, _>>()?,
            null_policy: NullPolicy::Skip,
        });
    }
    let mut mask = vec![false; schema.columns.len()];
    for spec in &specs {
        for &idx in &spec.indices {
            if let Some(slot) = mask.get_mut(idx) {
                *slot = true;
            }
        }
    }
    Ok((specs, mask))
}

/// COMMIT 時に検査すべき遅延 `FOREIGN KEY` の範囲（TABLE-17・TASK-205、
/// Issue #1077）。autocommit（1 文＝1 トランザクション）は常に `All`——遅延指定
/// （`DEFERRABLE`／`INITIALLY DEFERRED`）でも「検査しない」ことにはならず、
/// 文単位で必ず検査する。明示トランザクション（SQL-31・TASK-221）中の
/// `tenant::WriteTarget::InTxn` 経路（`insert_row_unchecked`・
/// `insert_rows_unchecked`・`insert_typed_row_unchecked`・
/// `truncate_table_unchecked` の 4 経路のみ）だけが `ImmediateOnly` を渡し、
/// `INITIALLY DEFERRED` の FK を文単位検査から除外して COMMIT 時
/// （[`enforce_deferred_foreign_keys_in_txn`]）へ先送りする。`fail-closed`:
/// 先送りした FK は `sql::transaction::SessionTransaction::commit` が
/// 必ず検査してから commit する契約（省き忘れは fail-open のバグ）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FkCheckMode {
    All,
    ImmediateOnly,
}

impl FkCheckMode {
    /// `fk` をこのモードの下で文単位検査の対象に含めるか。
    fn includes(self, fk: &ForeignKeyDef) -> bool {
        match self {
            FkCheckMode::All => true,
            FkCheckMode::ImmediateOnly => !fk.is_initially_deferred(),
        }
    }
}

/// `tenant.rs` の各書き込み関数が書き込み後・commit 前に呼ぶ唯一の入口
/// （TABLE-16・TASK-204）。`CHECK` 制約（Issue #906）→ 一意性制約（主キー・
/// UNIQUE）→ `FOREIGN KEY` の参照元側（TABLE-17・TASK-205、Issue #907。書いた行が
/// 参照する値の組が参照先に存在するか）の順に検査する。`written_ids` の契約は [`enforce_unique_keys_in_txn`]
/// と同じ。いずれの制約も宣言しないテーブルは即座に成功する。`fk_mode` は
/// `INITIALLY DEFERRED` の FK を文単位検査から除外するかどうか
/// （[`FkCheckMode`] 参照。`CHECK`・一意性制約には遅延の概念がなく常時検査する）。
pub(crate) fn enforce_row_constraints_in_txn(
    write_txn: &redb::WriteTransaction,
    table_name: &str,
    schema: &TableSchema,
    tenant_id: &str,
    written_ids: &[u64],
    fk_mode: FkCheckMode,
) -> Result<(), TenantWriteError> {
    enforce_check_constraints_in_txn(write_txn, table_name, schema, tenant_id, written_ids)?;
    enforce_unique_keys_in_txn(write_txn, table_name, schema, tenant_id, written_ids)?;
    enforce_foreign_keys_in_txn(
        write_txn,
        table_name,
        schema,
        tenant_id,
        written_ids,
        fk_mode,
    )
}

/// `CHECK` 制約（TABLE-16・TASK-204、Issue #906）の検査。`written_ids` の各行を
/// 同一 write トランザクション内で読み戻し（物理キーはサーバー側導出テナント
/// `tenant_id` で名前空間化済み。RLS-9・TABLE-12）、書き込まれた最終値
/// （UPSERT の `DO UPDATE`・`UPDATE` の SET 適用後の値を含む）に対して全 `CHECK`
/// を評価する。`CHECK` を宣言しないテーブルは即座に成功する。
///
/// 読み戻せない id（同一文内で後から削除された等）は検査対象外とする
/// （存在しない行は制約に違反し得ない。[`enforce_unique_keys_in_txn`] と同じ扱い）。
/// 違反時は [`TenantWriteError::CheckViolation`]（制約名のみ。行の値・id・
/// テナントは含まない）を返し、呼び出し元は `write_txn` を commit しない。
fn enforce_check_constraints_in_txn(
    write_txn: &redb::WriteTransaction,
    table_name: &str,
    schema: &TableSchema,
    tenant_id: &str,
    written_ids: &[u64],
) -> Result<(), TenantWriteError> {
    if written_ids.is_empty() {
        return Ok(());
    }
    let Some(compiled) = crate::sql::check_constraint::CompiledChecks::compile(schema)? else {
        return Ok(());
    };
    let row_table_name = crate::catalog::user_rows_table_name(table_name);
    let row_table = write_txn
        .open_table(crate::catalog::user_rows_table_def(&row_table_name))
        .map_err(crate::catalog::map_row_table_error)?;
    let mut embedding: Vec<f32> = Vec::new();
    for &id in written_ids {
        let Some(guard) = row_table
            .get((tenant_id, id))
            .map_err(crate::catalog::CatalogError::from)?
        else {
            continue;
        };
        let buf = guard.value();
        let (_dim, metadata) =
            crate::storage::decode_row_embedding_and_metadata_into(buf, &mut embedding)?;
        compiled.enforce(schema, id, &embedding, metadata)?;
    }
    Ok(())
}

/// 一意性制約（主キー・UNIQUE）の検査点（[`enforce_row_constraints_in_txn`] から
/// 呼ばれる）。主キーも UNIQUE 制約も宣言しないテーブルは即座に成功する。
///
/// `written_ids` は今回の書き込みトランザクションで `user_rows/{table}` へ
/// 書き込んだ（または上書きした）行の `id` 集合。呼び出し元がこの txn の中で
/// 既に書き込み済みであることが前提で、本関数はそれらを同一 txn 内で読み戻す
/// （redb の write トランザクションは自身が書いた値を同一 txn 内で読める）。
///
/// 判定は永続一意索引（`user_uniq/{table}`）への点照会に委譲する
/// （[`unique_index::ensure_tenant_index`] でテナントの索引が最新であることを
/// 保証してから、[`unique_index::check_and_update`] で `written_ids` を索引へ
/// 反映しつつ衝突を検出する。設計・不変条件は [`unique_index`] モジュール
/// ドキュメント参照。Issue #1070）。衝突を検出した時点で
/// [`TenantWriteError::UniqueViolation`] を返し、呼び出し元は `write_txn` を
/// commit しない（索引への書き込みも同じ txn に含まれるため副作用は残らない）。
pub(crate) fn enforce_unique_keys_in_txn(
    write_txn: &redb::WriteTransaction,
    table_name: &str,
    schema: &TableSchema,
    tenant_id: &str,
    written_ids: &[u64],
) -> Result<(), TenantWriteError> {
    if schema.primary_key().is_none() && schema.unique_constraints().is_empty() {
        return Ok(());
    }
    if written_ids.is_empty() {
        return Ok(());
    }
    let (specs, mask) = key_specs(schema)?;

    let row_table_name = crate::catalog::user_rows_table_name(table_name);
    let row_table = write_txn
        .open_table(crate::catalog::user_rows_table_def(&row_table_name))
        .map_err(crate::catalog::map_row_table_error)?;
    let index_table_name = crate::catalog::user_uniq_table_name(table_name);
    let mut index_table = write_txn
        .open_table(crate::catalog::user_uniq_table_def(&index_table_name))
        .map_err(|e| TenantWriteError::Catalog(CatalogError::from(e)))?;

    let written_id_set: HashSet<u64> = written_ids.iter().copied().collect();
    unique_index::ensure_tenant_index(
        &row_table,
        &mut index_table,
        schema,
        &specs,
        &mask,
        tenant_id,
        &written_id_set,
    )?;
    unique_index::check_and_update(
        &row_table,
        &mut index_table,
        schema,
        &specs,
        tenant_id,
        &written_id_set,
        &mask,
    )
}

/// 削除経路（`tenant.rs` の `delete_row_impl`・`delete_rows_where_unchecked`・
/// `replace_typed_rows_by_text_key`）が、行削除の直後・同一 write トランザクション
/// 内から呼ぶ永続一意索引の後片付け窓口（[`unique_index::forget_rows_in_txn`] の
/// 薄いラッパー。Issue #1070）。行ストアの可変ハンドルを保持したまま呼んでよい
/// （索引テーブルのみを開くため `TableAlreadyOpen` にならない）。後片付けは
/// 索引肥大化を防ぐ衛生措置であり、呼び出し漏れがあっても [`enforce_unique_keys_in_txn`]
/// の読み戻し判定（stale 検出）が正しさを保証する。
pub(crate) fn forget_unique_index_rows_in_txn(
    write_txn: &redb::WriteTransaction,
    table_name: &str,
    schema: &TableSchema,
    tenant_id: &str,
    ids: &[u64],
) -> Result<(), TenantWriteError> {
    unique_index::forget_rows_in_txn(write_txn, table_name, schema, tenant_id, ids)
}

/// TRUNCATE（`tenant.rs::truncate_table_unchecked`）が、行ストアの `retain_in`
/// 後・`row_table` を drop した後の同一 write トランザクション内から呼ぶ、
/// テナント範囲の永続一意索引クリア窓口（[`unique_index::clear_tenant_in_txn`]
/// の薄いラッパー。Issue #1070）。他テナントの索引エントリには一切触れない。
pub(crate) fn clear_unique_index_for_tenant_in_txn(
    write_txn: &redb::WriteTransaction,
    table_name: &str,
    schema: &TableSchema,
    tenant_id: &str,
) -> Result<(), TenantWriteError> {
    unique_index::clear_tenant_in_txn(write_txn, table_name, schema, tenant_id)
}

/// `tenant::upsert_typed_rows_unchecked` の UNIQUE 対象 UPSERT（TABLE-16、
/// Issue #1074）が、書き込み**前**に 1 回だけ呼ぶ既存行スキャン。対象テナント
/// （`ctx.tenant_id()` 由来の物理キー範囲。[`enforce_unique_keys_in_txn`] と同じ
/// `(tenant_id, 0)..=(tenant_id, u64::MAX)` の閉区間）が所有する**全行**
/// （`Public`／`Private` を問わない。可視性フィルタ・二次索引・世代キャッシュの
/// いずれも経由しない生の走査。RLS-9・RLS-10 (c)）を走査し、`indices`
/// （UNIQUE 制約の構成列。解決した制約の宣言順）が示す一意キーの正準バイト列
/// から行 `id` への対応を返す。NULL を含む行はキーを持たない（NULLS DISTINCT。
/// [`key_bytes`] と同じ扱い）ため対象外。
///
/// キー構築は書き込み時の検査点 [`enforce_unique_keys_in_txn`] と同じ
/// [`decode_key_columns`]／[`key_bytes`] を再利用する（第 2 の正準化を作らない。
/// `unique_key_from_values`〔`Value` 行専用〕とバイト単位で一致する契約は
/// 両者が同じ [`push_canonical_component`] を経由することで保証される）。
///
/// 1 つのキーに一致する既存行が 2 件以上ある場合は一意性の不変条件が破れて
/// いる内部矛盾であり、黙って上書きせず [`TenantWriteError::Catalog`]
/// （`CatalogError::Invalid`）で fail-closed に拒否する。
pub(crate) fn scan_tenant_rows_by_unique_key<T>(
    row_table: &T,
    schema: &TableSchema,
    tenant_id: &str,
    indices: &[usize],
) -> Result<HashMap<Vec<u8>, u64>, TenantWriteError>
where
    T: ReadableTable<(&'static str, u64), &'static [u8]>,
{
    let spec = KeySpec {
        indices: indices.to_vec(),
        null_policy: NullPolicy::Skip,
    };
    let mut mask = vec![false; schema.columns.len()];
    for &idx in indices {
        if let Some(slot) = mask.get_mut(idx) {
            *slot = true;
        }
    }

    let mut existing: HashMap<Vec<u8>, u64> = HashMap::new();
    let range_start = std::ops::Bound::Included((tenant_id, 0u64));
    let range_end = std::ops::Bound::Included((tenant_id, u64::MAX));
    for entry in row_table
        .range::<(&str, u64)>((range_start, range_end))
        .map_err(crate::catalog::CatalogError::from)?
    {
        let (k, v) = entry.map_err(crate::catalog::CatalogError::from)?;
        let (key_tenant, id) = k.value();
        if key_tenant != tenant_id {
            // 閉区間により理論上到達しないが、defense-in-depth として維持する
            // （`enforce_unique_keys_in_txn` と同じ判断）。
            break;
        }
        let buf = v.value();
        let values = decode_key_columns(schema, &mask, buf)?;
        let Some(key) = key_bytes(&spec, &values).map_err(internal)? else {
            continue;
        };
        if let Some(existing_id) = existing.insert(key, id) {
            if existing_id != id {
                // 同一 UNIQUE キーを共有する既存行が 2 件以上見つかった場合は
                // 一意性の不変条件が破れている内部矛盾であり（呼び出し元の
                // incoming VALUES 同士の衝突ではない）、UniqueViolation
                // （呼び出し元の衝突用 wire code）を誤って返すと非衝突の
                // UPSERT まで巻き込んで失敗させてしまう。ドキュメント通り
                // `internal` で fail-closed に拒否する。
                return Err(internal(
                    "duplicate existing rows share a UNIQUE key: catalog invariant violated",
                ));
            }
        }
    }
    Ok(existing)
}

/// [`crate::catalog::Storage::alter_table_add_unique_constraint`]（Rust API。
/// TABLE-16・TASK-204、Issue #905）が制約追加前に呼ぶ既存行の重複判定。
/// `row_table`（対象テーブルの行ストア全体）を物理キー順に走査し、テナントごと
/// に独立して `columns`（`schema` の生存列名。`validate_schema` で検証済み）の
/// 値の組の重複を探す。物理キー `(tenant_id, id)`（TABLE-12）の辞書順により
/// 同一テナントの行は常に連続するため、テナントが変わるたびに検査用キー集合を
/// リセットする（テナントを跨いだ同値は重複としない）。NULL を含む行は対象外
/// （NULLS DISTINCT）。キー構築は書き込み時の検査点
/// [`enforce_unique_keys_in_txn`] と同じ正準バイト列を使う。
pub(crate) fn table_has_duplicate_unique_key<T>(
    row_table: &T,
    schema: &TableSchema,
    columns: &[String],
) -> Result<bool, CatalogError>
where
    T: ReadableTable<(&'static str, u64), &'static [u8]>,
{
    let indices: Vec<usize> = columns
        .iter()
        .map(|name| {
            schema
                .columns
                .iter()
                .position(|c| &c.name == name)
                .ok_or_else(|| {
                    CatalogError::Invalid(format!(
                        "unique constraint references unknown column: {name}"
                    ))
                })
        })
        .collect::<Result<_, _>>()?;
    let spec = KeySpec {
        indices,
        null_policy: NullPolicy::Skip,
    };
    let mut mask = vec![false; schema.columns.len()];
    for &idx in &spec.indices {
        if let Some(slot) = mask.get_mut(idx) {
            *slot = true;
        }
    }

    let mut current_tenant: Option<String> = None;
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    for entry in row_table.iter()? {
        let (k, v) = entry?;
        let (key_tenant, _id) = k.value();
        if current_tenant.as_deref() != Some(key_tenant) {
            current_tenant = Some(key_tenant.to_string());
            seen.clear();
        }
        let buf = v.value();
        // 行ヘッダのテナントと物理キーのテナントの整合（TABLE-12）を確認して
        // から値を読む（不整合な行を別テナントの行として数えない）。
        let (row_tenant, _visibility, _offset) = crate::storage::decode_row_header(buf)
            .map_err(|e| CatalogError::CorruptSchema(e.to_string()))?;
        crate::storage::verify_row_key_tenant(key_tenant, row_tenant)
            .map_err(|e| CatalogError::CorruptSchema(e.to_string()))?;
        let (_dim, metadata) = crate::storage::decode_row_dim_and_metadata_borrowed(buf)
            .map_err(|e| CatalogError::CorruptSchema(e.to_string()))?;
        let values = crate::row_codec::scan_scalar_columns_masked(schema, metadata, Some(&mask))
            .map_err(|e| CatalogError::CorruptSchema(e.to_string()))?;
        let Some(key) =
            key_bytes(&spec, &values).map_err(|m| CatalogError::Invalid(m.to_string()))?
        else {
            continue;
        };
        if !seen.insert(key) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 内部矛盾（`validate_schema` を通過したスキーマからは到達しないはずの状態）を
/// 表す固定文言を [`TenantWriteError`] へ包む。
fn internal(message: &'static str) -> TenantWriteError {
    TenantWriteError::Catalog(CatalogError::Invalid(message.to_string()))
}

/// 1 行分の物理バイト列 `buf` から一意キー構成列（`mask` で示された列のみ）を
/// 復元する。
fn decode_key_columns<'a>(
    schema: &TableSchema,
    mask: &[bool],
    buf: &'a [u8],
) -> Result<Vec<Option<ScalarRef<'a>>>, TenantWriteError> {
    let (_dim, metadata) = crate::storage::decode_row_dim_and_metadata_borrowed(buf)?;
    Ok(crate::row_codec::scan_scalar_columns_masked(
        schema,
        metadata,
        Some(mask),
    )?)
}

/// 復元済みの列値 `values`（論理列インデックスで添字付け）から、一意キー
/// `spec` の正準バイト列を組み立てる。各コンポーネントは `[type_tag: u8]
/// [len: u32 BE][payload]` の形で連結する（`len` を明示することで、可変長
/// コンポーネント（TEXT・BYTEA・ENUM）を並べたときの境界曖昧性——`("ab","c")`
/// と `("a","bc")` が同一バイト列になる事故——を構造的に排除する）。
///
/// 構成列のいずれかが NULL（または列欠落）の場合、UNIQUE 制約は `Ok(None)`
/// （検査対象外。NULLS DISTINCT）、主キーは内部矛盾として `Err`。
fn key_bytes(
    spec: &KeySpec,
    values: &[Option<ScalarRef<'_>>],
) -> Result<Option<Vec<u8>>, &'static str> {
    let mut out = Vec::new();
    for &idx in &spec.indices {
        let Some(value) = values.get(idx).and_then(|v| v.as_ref()) else {
            return match spec.null_policy {
                NullPolicy::Skip => Ok(None),
                // 主キー列は `nullable == false`（`validate_schema` が強制）で
                // あるべきため、NULL・列欠落はここへ到達しないはずの内部矛盾。
                // black-box に無視せず fail-closed に拒否する。
                NullPolicy::Reject => Err("primary key column value is missing or NULL"),
            };
        };
        push_canonical_component(&mut out, *value)?;
    }
    Ok(Some(out))
}

/// [`key_bytes`] が使う 1 コンポーネント分のエンコード。型タグは
/// [`ColumnType::is_unique_constraint_allowed`] が許可する型と 1 対 1 に対応する
/// （新しい許可型を追加する際はここも同時に拡張する契約。Issue #1073 で
/// REAL・DOUBLE PRECISION・NUMERIC・JSON／JSONB・配列型を追加）。
///
/// # 型ごとの正規化（Issue #1073 D7。`docs/design/unique-constraint.md` D7 参照）
///
/// - REAL／DOUBLE PRECISION: [`crate::scalar_float::canonicalize_real`]／
///   `canonicalize_double`（`-0.0` を `+0.0` へ正規化）した後のビットパターン。
///   非有限値（NaN・±∞）は `Err`（`row_codec` の encode 側が非有限値を拒否する
///   ため通常到達しないが、defense in depth として維持する）。
/// - NUMERIC: 末尾ゼロを除去した `(unscaled, scale)` の正準形。`1.50` と `1.5`
///   が同一キーになる（列内では `scale` が固定なので元々単射だが、複合キー・
///   将来の型跨ぎ比較でも表現の揺れを吸収する）。
/// - JSON／JSONB: [`crate::json::canonical_equality_text`]（値としての等価
///   正規化テキスト。キー順・空白だけでなく数値も値として正規化する）。
/// - 配列: `[要素タグ: u8][要素数: u32 BE][要素列の生ペイロード]`。
///   [`crate::row_codec::ArrayRef::payload`] のエンコーダ決定性（要素順保持・
///   flags 固定・代替表現なし）により、この組は値に対して単射になる。
fn push_canonical_component(out: &mut Vec<u8>, value: ScalarRef<'_>) -> Result<(), &'static str> {
    fn push_len_prefixed(out: &mut Vec<u8>, tag: u8, payload: &[u8]) {
        out.push(tag);
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(payload);
    }

    match value {
        ScalarRef::Text(s) => {
            push_len_prefixed(out, ColumnType::Text.unique_key_tag(), s.as_bytes())
        }
        ScalarRef::Integer(i) => {
            push_len_prefixed(out, ColumnType::Integer.unique_key_tag(), &i.to_be_bytes())
        }
        ScalarRef::BigInt(i) => {
            push_len_prefixed(out, ColumnType::BigInt.unique_key_tag(), &i.to_be_bytes())
        }
        ScalarRef::Bool(b) => push_len_prefixed(
            out,
            ColumnType::Boolean.unique_key_tag(),
            &[if b { 1u8 } else { 0u8 }],
        ),
        ScalarRef::Date(d) => {
            push_len_prefixed(out, ColumnType::Date.unique_key_tag(), &d.to_be_bytes())
        }
        ScalarRef::Timestamp(t) => push_len_prefixed(
            out,
            ColumnType::Timestamp.unique_key_tag(),
            &t.to_be_bytes(),
        ),
        ScalarRef::Bytes(b) => push_len_prefixed(out, ColumnType::Bytea.unique_key_tag(), b),
        ScalarRef::Uuid(u) => {
            push_len_prefixed(out, ColumnType::Uuid.unique_key_tag(), u.as_bytes())
        }
        ScalarRef::Enum(s) => push_len_prefixed(
            out,
            // ENUM の型タグは語彙に依存しない固定値（列自体の型が
            // `ColumnType::Enum(Arc<EnumTypeDef>)` を持つが、ここでは変種
            // 判定のためのタグだけが必要なため語彙は参照しない）。
            9,
            s.as_bytes(),
        ),
        ScalarRef::Real(r) => {
            let canonical = crate::scalar_float::canonicalize_real(r);
            if !canonical.is_finite() {
                // `row_codec` の encode 側が非有限値を既に拒否しているため
                // 通常到達しない内部矛盾。値を黙って無視せず fail-closed に
                // 拒否する（defense in depth）。
                return Err("unique key REAL column has a non-finite value");
            }
            push_len_prefixed(
                out,
                ColumnType::Real.unique_key_tag(),
                &canonical.to_bits().to_be_bytes(),
            )
        }
        ScalarRef::Double(d) => {
            let canonical = crate::scalar_float::canonicalize_double(d);
            if !canonical.is_finite() {
                return Err("unique key DOUBLE PRECISION column has a non-finite value");
            }
            push_len_prefixed(
                out,
                ColumnType::Double.unique_key_tag(),
                &canonical.to_bits().to_be_bytes(),
            )
        }
        ScalarRef::Numeric(d) => {
            let (unscaled, scale) = canonical_numeric_parts(d);
            let mut payload = Vec::with_capacity(17);
            payload.extend_from_slice(&unscaled.to_be_bytes());
            payload.push(scale);
            push_len_prefixed(out, ColumnType::NUMERIC_UNIQUE_KEY_TAG, &payload)
        }
        ScalarRef::Json(s) => {
            let canonical = crate::json::canonical_equality_text(s)
                .map_err(|_| "unique key JSON column has an invalid stored value")?;
            push_len_prefixed(out, ColumnType::Json.unique_key_tag(), canonical.as_bytes())
        }
        ScalarRef::Array(a) => {
            let mut payload = Vec::new();
            payload.push(array_elem_tag(a.elem()));
            payload.extend_from_slice(&a.count().to_be_bytes());
            payload.extend_from_slice(a.payload());
            push_len_prefixed(out, ColumnType::ARRAY_UNIQUE_KEY_TAG, &payload)
        }
    }
    Ok(())
}

/// 束縛済み `VALUES`（`crate::row_codec::Value`。UPSERT の新規挿入予定値・
/// UNIQUE 対象列の一致判定の両方で使う）から一意キーの正準バイト列を計算する
/// （TABLE-16・Issue #1074。`sql::parser::bind_upsert_form` のバッチ内対象キー
/// 重複検出、`tenant::upsert_typed_rows_unchecked` の UNIQUE 対象衝突判定が
/// 本関数を共有する。既存行側の [`key_bytes`]／[`decode_key_columns`] と同じ
/// 正準表現（型タグ＋長さ前置）を独立に再実装せず、[`push_canonical_component`]
/// を再利用することでバイト単位の一致を構造的に保証する）。
///
/// `indices` は UNIQUE 制約の構成列（`schema.columns` に対する論理インデックス。
/// `key_specs` が解決したものと同じ規約）。構成列のいずれかが `Value::Null`
/// または列欠落の場合は `Ok(None)`（NULLS DISTINCT。本関数は UNIQUE 制約専用の
/// 呼び出しを想定し、`NullPolicy::Reject`〔主キー〕は扱わない）。
///
/// `ColumnType::is_unique_constraint_allowed`（Issue #1073）が PK 許可型の
/// 上位集合として REAL／DOUBLE PRECISION／NUMERIC／JSON／JSONB／ARRAY を
/// UNIQUE 制約の構成列として許可するため、`resolve_conflict_target` が解決する
/// `indices` はこれらの型の列も指しうる。本関数はそれらも
/// [`push_canonical_component`] へ委譲することで、UNIQUE 制約が許可する型と
/// `ON CONFLICT` 対象列として扱える型を一致させる。
pub(crate) fn unique_key_from_values(
    indices: &[usize],
    values: &[crate::row_codec::Value],
) -> Result<Option<Vec<u8>>, &'static str> {
    use crate::row_codec::Value;
    let mut out = Vec::new();
    for &idx in indices {
        let value = match values.get(idx) {
            None | Some(Value::Null) => return Ok(None),
            Some(v) => v,
        };
        // ARRAY 列（`Value::Array`）は `ScalarRef::Array` が要素列の生バイト列
        // （`ArrayRef`）への借用を要求するため、一時バッファへ組み立ててから
        // 借用する（`array_payload` は Array 分岐でのみ初期化され、`scalar` が
        // それを借用している間だけこのループの 1 反復内で生存する）。
        let array_payload: Vec<u8>;
        let scalar = match value {
            Value::Text(s) => ScalarRef::Text(s.as_str()),
            Value::Integer(i) => ScalarRef::Integer(*i),
            Value::BigInt(i) => ScalarRef::BigInt(*i),
            Value::Bool(b) => ScalarRef::Bool(*b),
            Value::Date(d) => ScalarRef::Date(*d),
            Value::Timestamp(t) => ScalarRef::Timestamp(*t),
            Value::Bytes(b) => ScalarRef::Bytes(b.as_slice()),
            Value::Enum(s) => ScalarRef::Enum(s.as_str()),
            Value::Uuid(u) => ScalarRef::Uuid(*u),
            Value::Real(r) => ScalarRef::Real(*r),
            Value::Double(d) => ScalarRef::Double(*d),
            Value::Numeric(d) => ScalarRef::Numeric(*d),
            Value::Json(s) => ScalarRef::Json(s.as_str()),
            Value::Array(a) => {
                let mut payload = Vec::new();
                crate::row_codec::write_array_elements_payload(&mut payload, a)
                    .map_err(|_| "unique key column has an array value that cannot be encoded")?;
                array_payload = payload;
                let count = u32::try_from(a.len())
                    .map_err(|_| "unique key column has an array value that cannot be encoded")?;
                ScalarRef::Array(crate::row_codec::ArrayRef::from_owned(
                    a.elem(),
                    count,
                    &array_payload,
                ))
            }
            Value::Null | Value::Vector(_) => {
                // `ColumnType::is_unique_constraint_allowed` が事前に拒否する
                // 型であり、UNIQUE 制約の構成列としては `validate_schema` を
                // 通過したスキーマから到達しないはずの内部矛盾
                // （`push_canonical_component` と同じ判断）。
                return Err("unique key column has a type that is not allowed as a unique key");
            }
        };
        push_canonical_component(&mut out, scalar)
            .map_err(|_| "unique key column has a type that is not allowed as a unique key")?;
    }
    Ok(Some(out))
}

/// NUMERIC の正準 `(unscaled, scale)`: 末尾ゼロを除去した最簡表現（`1.50` と
/// `1.5` を同一キーへ正規化する。Issue #1073 D7）。`scale == 0` に達したら
/// それ以上は割らない。`checked_rem`／`checked_div` で整数演算を明示的に扱う
/// （coding-rust.md）。
fn canonical_numeric_parts(d: crate::numeric::Decimal) -> (i128, u8) {
    let mut unscaled = d.unscaled();
    let mut scale = d.scale();
    while scale > 0 {
        let Some(0) = unscaled.checked_rem(10) else {
            break;
        };
        let Some(next) = unscaled.checked_div(10) else {
            break;
        };
        unscaled = next;
        scale -= 1;
    }
    (unscaled, scale)
}

/// 配列要素型の一意キー用固定タグ（配列列内部だけで使うスクラッチ値。
/// [`crate::catalog::ArrayElemType`] のカタログ表現とは独立に採番してよい。
/// `ColumnType::unique_key_tag` と同じ「非永続化のスクラッチタグ」方針）。
fn array_elem_tag(elem: crate::catalog::ArrayElemType) -> u8 {
    match elem {
        crate::catalog::ArrayElemType::Text => 0,
        crate::catalog::ArrayElemType::Bool => 1,
    }
}

/// `FOREIGN KEY` 1 件分の参照元側の検査仕様（TABLE-17・TASK-205、Issue #907）。
struct ForeignKeySpec<'a> {
    fk: &'a ForeignKeyDef,
    /// 参照元スキーマにおける参照元列の論理インデックス（`fk.columns()` の順）。
    indices: Vec<usize>,
}

/// 参照元の行から集めた、参照先に存在しなければならない値の組の集合
/// （重複排除済み）。`id` 参照は物理キーの `id` 値、それ以外は一意性検査と同じ
/// 正準キーバイト列（[`key_bytes`]）で持つ。
enum RequiredParentKeys {
    Ids(BTreeSet<u64>),
    Keys(HashSet<Vec<u8>>),
}

impl RequiredParentKeys {
    fn new(fk: &ForeignKeyDef) -> Self {
        if fk.references_parent_id() {
            RequiredParentKeys::Ids(BTreeSet::new())
        } else {
            RequiredParentKeys::Keys(HashSet::new())
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            RequiredParentKeys::Ids(ids) => ids.is_empty(),
            RequiredParentKeys::Keys(keys) => keys.is_empty(),
        }
    }
}

/// `schema` の各 `FOREIGN KEY` 宣言について参照元列のインデックスを解決し、
/// それらの和集合マスク（[`crate::row_codec::scan_scalar_columns_masked`] へ渡す）を
/// 組み立てる。列名解決の失敗は `validate_schema` が起こり得ないことを保証する
/// 不変条件だが、panic せず fail-closed に拒否する。
fn foreign_key_specs(
    schema: &TableSchema,
) -> Result<(Vec<ForeignKeySpec<'_>>, Vec<bool>), CatalogError> {
    let mut specs = Vec::with_capacity(schema.foreign_keys().len());
    let mut mask = vec![false; schema.columns.len()];
    for fk in schema.foreign_keys() {
        let mut indices = Vec::with_capacity(fk.columns().len());
        for name in fk.columns() {
            let idx = schema
                .columns
                .iter()
                .position(|c| &c.name == name)
                .ok_or_else(|| {
                    CatalogError::Invalid(
                        "foreign key column not found in live schema columns".to_string(),
                    )
                })?;
            if let Some(slot) = mask.get_mut(idx) {
                *slot = true;
            }
            indices.push(idx);
        }
        specs.push(ForeignKeySpec { fk, indices });
    }
    Ok((specs, mask))
}

/// 復元済みの参照元列の値から、参照先に存在しなければならない値の組を
/// `required` へ積む。`id` 参照で負値（物理キー `id` は `u64`）は参照先が
/// 存在し得ないため即座に違反とする。
///
/// `MATCH SIMPLE`（既定）はいずれかの構成列が NULL の組を検査対象外とする
/// （PostgreSQL の既定）。`MATCH FULL`（TABLE-17・TASK-205、Issue #1077）は
/// **すべて** NULL の組のみ検査対象外とし、NULL と非 NULL が混在する組は
/// `Err(ForeignKeyViolation)` にする（単一列の FK は NULL が 0 個か全部＝1 個の
/// いずれかしかあり得ないため `MATCH SIMPLE` と同じ挙動になる）。
fn push_required_key(
    spec: &ForeignKeySpec<'_>,
    values: &[Option<ScalarRef<'_>>],
    required: &mut RequiredParentKeys,
) -> Result<(), TenantWriteError> {
    match required {
        RequiredParentKeys::Ids(ids) => {
            let [idx] = spec.indices.as_slice() else {
                return Err(internal("id-referencing foreign key must have one column"));
            };
            let id = match values.get(*idx).and_then(|v| v.as_ref()) {
                None => return Ok(()),
                Some(ScalarRef::Integer(v)) => u64::try_from(*v),
                Some(ScalarRef::BigInt(v)) => u64::try_from(*v),
                // `validate_foreign_keys` が `INTEGER`／`BIGINT` 以外を拒否するため
                // 到達しない内部矛盾。黙って通さず fail-closed に拒否する。
                Some(_) => {
                    return Err(internal(
                        "id-referencing foreign key column has a non-integer value",
                    ))
                }
            }
            .map_err(|_| TenantWriteError::ForeignKeyViolation)?;
            ids.insert(id);
        }
        RequiredParentKeys::Keys(keys) => {
            if spec.fk.match_type() == ForeignKeyMatch::Full {
                let null_count = spec
                    .indices
                    .iter()
                    .filter(|&&idx| values.get(idx).and_then(|v| v.as_ref()).is_none())
                    .count();
                if null_count > 0 && null_count < spec.indices.len() {
                    return Err(TenantWriteError::ForeignKeyViolation);
                }
            }
            let key_spec = KeySpec {
                indices: spec.indices.clone(),
                null_policy: NullPolicy::Skip,
            };
            if let Some(key) = key_bytes(&key_spec, values).map_err(internal)? {
                keys.insert(key);
            }
        }
    }
    Ok(())
}

/// 参照先テーブル `parent_table`（スキーマ `parent_schema`）の**同一テナントの
/// 全行**（可視性を問わない。RLS-10 (c)）に、`required` の値の組がすべて存在する
/// ことを確かめる（TABLE-17・TASK-205、Issue #907）。1 つでも欠ければ
/// [`TenantWriteError::ForeignKeyViolation`]。
///
/// 走査・照会のキーはサーバー側導出テナント `tenant_id` の物理キー空間
/// （`(tenant_id, 0)..=(tenant_id, u64::MAX)`。TABLE-12）に閉じ、他テナントの
/// 行には一切触れない——他テナントだけが持つ値は「不在」と同じ結果になり、
/// 応答・処理経路のいずれにも他テナントの行の有無が現れない（RLS-9）。
/// 呼び出し元は `parent_table` の行ストアのハンドルを保持していない状態で呼ぶこと
/// （自己参照では参照元と同じ行ストアを開き直すため。redb の `TableAlreadyOpen`）。
fn verify_required_parent_keys(
    write_txn: &redb::WriteTransaction,
    parent_table: &str,
    parent_schema: &TableSchema,
    fk: &ForeignKeyDef,
    tenant_id: &str,
    required: RequiredParentKeys,
) -> Result<(), TenantWriteError> {
    if required.is_empty() {
        return Ok(());
    }
    let row_table_name = crate::catalog::user_rows_table_name(parent_table);
    let row_table = match write_txn.open_table(crate::catalog::user_rows_table_def(&row_table_name))
    {
        Ok(t) => t,
        // 参照先へまだ 1 行も挿入されていない（行ストア未作成）。必要な値の組が
        // 1 つ以上あるため違反。
        Err(redb::TableError::TableDoesNotExist(_)) => {
            return Err(TenantWriteError::ForeignKeyViolation)
        }
        Err(e) => {
            return Err(TenantWriteError::from(crate::catalog::map_row_table_error(
                e,
            )))
        }
    };
    match required {
        RequiredParentKeys::Ids(ids) => {
            for id in ids {
                if row_table
                    .get((tenant_id, id))
                    .map_err(CatalogError::from)?
                    .is_none()
                {
                    return Err(TenantWriteError::ForeignKeyViolation);
                }
            }
            Ok(())
        }
        RequiredParentKeys::Keys(mut pending) => {
            let mut indices = Vec::with_capacity(fk.parent_columns().len());
            let mut mask = vec![false; parent_schema.columns.len()];
            for name in fk.parent_columns() {
                let idx = parent_schema
                    .columns
                    .iter()
                    .position(|c| &c.name == name)
                    .ok_or_else(|| internal("referenced column not found in parent schema"))?;
                if let Some(slot) = mask.get_mut(idx) {
                    *slot = true;
                }
                indices.push(idx);
            }
            let key_spec = KeySpec {
                indices,
                null_policy: NullPolicy::Skip,
            };
            let range_start = std::ops::Bound::Included((tenant_id, 0u64));
            let range_end = std::ops::Bound::Included((tenant_id, u64::MAX));
            for entry in row_table
                .range::<(&str, u64)>((range_start, range_end))
                .map_err(CatalogError::from)?
            {
                let (k, v) = entry.map_err(CatalogError::from)?;
                let (key_tenant, _id) = k.value();
                if key_tenant != tenant_id {
                    // 閉区間により理論上到達しない（`enforce_unique_keys_in_txn` と
                    // 同じ defense-in-depth）。
                    break;
                }
                let values = decode_key_columns(parent_schema, &mask, v.value())?;
                if let Some(key) = key_bytes(&key_spec, &values).map_err(internal)? {
                    pending.remove(&key);
                    if pending.is_empty() {
                        return Ok(());
                    }
                }
            }
            Err(TenantWriteError::ForeignKeyViolation)
        }
    }
}

/// 参照先スキーマを取得する（自己参照は `None` を返し、呼び出し元が参照元
/// スキーマをそのまま使う）。参照先が存在しないのは `DROP TABLE` の依存検査
/// （`2BP01`）が防ぐ内部矛盾のため、`TableNotFound` を含めて内部エラーとして
/// fail-closed に拒否する。
fn parent_schema_for(
    write_txn: &redb::WriteTransaction,
    child_table: &str,
    fk: &ForeignKeyDef,
) -> Result<Option<TableSchema>, TenantWriteError> {
    if fk.parent_table() == child_table {
        return Ok(None);
    }
    crate::catalog::require_table_schema_write(write_txn, fk.parent_table())
        .map(Some)
        .map_err(|e| match e {
            CatalogError::TableNotFound(_) => {
                internal("referenced table of a foreign key is missing")
            }
            other => TenantWriteError::from(other),
        })
}

/// `FOREIGN KEY` の参照元側の検査（[`enforce_row_constraints_in_txn`] から呼ばれる。
/// TABLE-17・TASK-205、Issue #907）。`written_ids` の各行を同一 write トランザクション
/// 内で読み戻し（書き込み後の最終値。UPSERT の `DO UPDATE`・`UPDATE` の SET 適用後・
/// SET で触れない既存値を含む）、各 `FOREIGN KEY` の値の組が参照先の同一テナント
/// 全行に存在することを確かめる。同一文・同一明示トランザクション内で先に書いた
/// 参照先の行（自己参照で同じ文が書いた行を含む）も redb の write トランザクションが
/// 自身の未 commit の書き込みを読めるため母集合に含まれる。`FOREIGN KEY` を宣言
/// しないテーブルは即座に成功する。`fk_mode` が `ImmediateOnly` のとき
/// `INITIALLY DEFERRED` の FK は検査対象から除く（COMMIT 時
/// [`enforce_deferred_foreign_keys_in_txn`] へ先送りする。TABLE-17・TASK-205、
/// Issue #1077）。
fn enforce_foreign_keys_in_txn(
    write_txn: &redb::WriteTransaction,
    table_name: &str,
    schema: &TableSchema,
    tenant_id: &str,
    written_ids: &[u64],
    fk_mode: FkCheckMode,
) -> Result<(), TenantWriteError> {
    if schema.foreign_keys().is_empty() || written_ids.is_empty() {
        return Ok(());
    }
    let (specs, mask) = foreign_key_specs(schema)?;
    let specs: Vec<ForeignKeySpec<'_>> = specs
        .into_iter()
        .filter(|spec| fk_mode.includes(spec.fk))
        .collect();
    if specs.is_empty() {
        return Ok(());
    }
    let mut required: Vec<RequiredParentKeys> = specs
        .iter()
        .map(|spec| RequiredParentKeys::new(spec.fk))
        .collect();
    {
        let row_table_name = crate::catalog::user_rows_table_name(table_name);
        let row_table = write_txn
            .open_table(crate::catalog::user_rows_table_def(&row_table_name))
            .map_err(crate::catalog::map_row_table_error)?;
        let written: BTreeSet<u64> = written_ids.iter().copied().collect();
        for id in written {
            let Some(guard) = row_table.get((tenant_id, id)).map_err(CatalogError::from)? else {
                // 同一文内で後から削除された等、存在しない行は制約に違反し得ない
                // （`enforce_unique_keys_in_txn` と同じ扱い）。
                continue;
            };
            let values = decode_key_columns(schema, &mask, guard.value())?;
            for (spec, req) in specs.iter().zip(required.iter_mut()) {
                push_required_key(spec, &values, req)?;
            }
        }
    }
    for (spec, req) in specs.iter().zip(required) {
        let parent = parent_schema_for(write_txn, table_name, spec.fk)?;
        verify_required_parent_keys(
            write_txn,
            spec.fk.parent_table(),
            parent.as_ref().unwrap_or(schema),
            spec.fk,
            tenant_id,
            req,
        )?;
    }
    Ok(())
}

/// 参照先テーブルの行に加えた変更の種類（[`enforce_referencing_rows_in_txn`] の
/// 引数。TABLE-17・TASK-205、Issue #907）。変更が参照先キーに触れ得ない場合に
/// 検査（参照元のテナント内全行走査）を省くための情報。
#[derive(Clone, Copy)]
pub(crate) enum ReferencedRowsChange<'a> {
    /// 行の削除（単一行・述語つき `DELETE`・`TRUNCATE`・ファイル形 `INSERT` の
    /// 旧行置換）。`id` を含むすべての参照先キーが失われ得る。
    Removed,
    /// 既存行の指定列（論理インデックス）のみの更新（`UPDATE ... SET`・UPSERT の
    /// `DO UPDATE SET`）。`id` は予約列で `SET` できないため失われない。
    ColumnsUpdated(&'a [usize]),
    /// 既存行の全列置換（Rust API の `update_row`）。`id` は不変。
    AllColumnsReplaced,
}

/// `FOREIGN KEY` の参照先側の検査（TABLE-17・TASK-205、Issue #907）。テーブル
/// `table_name`（スキーマ `schema`）の行を削除・更新した write トランザクション内で、
/// 行の変更・台帳記録の**後**・commit の**前**に呼ぶ（参照元側と同じ検査点・同じ
/// 順序。`operation_id` の再送判定〔`23505`／`22023`〕が本検査より優先される）。
///
/// このテーブルを参照先とする各 `FOREIGN KEY`（他テーブル・自己参照のいずれも）に
/// ついて、参照元の**同一テナントの全行**（可視性を問わない。RLS-10 (c)）の値の組が
/// 変更後の参照先にすべて存在することを確かめる（事後状態の検証。削除・更新前の
/// 値を保持する必要がなく、自己参照・複数行の同時削除・置換のいずれにも同一の
/// 実装で効く）。`ALTER TABLE ... ADD FOREIGN KEY` を持たないため、各文の開始時点で
/// 参照整合性は常に成立しており、この検証は既定の `NO ACTION`（非遅延のため
/// `RESTRICT` と同値）と等価になる。違反は [`TenantWriteError::ForeignKeyViolation`]
/// （呼び出し元は `write_txn` を commit しない）。
///
/// 更新（[`ReferencedRowsChange::ColumnsUpdated`]／`AllColumnsReplaced`）で主キー・
/// UNIQUE 制約の構成列に触れない場合は、参照先キーが変わり得ないためカタログの
/// 逆引きすら行わない（主キー・UNIQUE を宣言しないテーブルの `UPDATE` はコスト
/// ゼロ）。計算量は参照元のテナント保有行数に比例する（一意性検査と同じく永続
/// 索引は持たない。`docs/design/foreign-key.md` 参照）。
/// `fk_mode` が `ImmediateOnly` のとき `INITIALLY DEFERRED` の FK は検査対象から
/// 除く（COMMIT 時 [`enforce_deferred_foreign_keys_in_txn`] へ先送りする。
/// TABLE-17・TASK-205、Issue #1077）。
pub(crate) fn enforce_referencing_rows_in_txn(
    write_txn: &redb::WriteTransaction,
    table_name: &str,
    schema: &TableSchema,
    tenant_id: &str,
    change: ReferencedRowsChange<'_>,
    fk_mode: FkCheckMode,
) -> Result<(), TenantWriteError> {
    let is_key_column = |name: &str| -> bool {
        schema
            .primary_key()
            .is_some_and(|pk| pk.iter().any(|c| c == name))
            || schema
                .unique_constraints()
                .iter()
                .any(|u| u.columns().iter().any(|c| c == name))
    };
    let updated_names: Option<Vec<&str>> = match change {
        ReferencedRowsChange::Removed => None,
        ReferencedRowsChange::ColumnsUpdated(indices) => Some(
            indices
                .iter()
                .filter_map(|&i| schema.columns.get(i).map(|c| c.name.as_str()))
                .collect(),
        ),
        ReferencedRowsChange::AllColumnsReplaced => {
            Some(schema.columns.iter().map(|c| c.name.as_str()).collect())
        }
    };
    if let Some(names) = &updated_names {
        if !names.iter().any(|n| is_key_column(n)) {
            return Ok(());
        }
    }
    let referencing = crate::catalog::referencing_foreign_keys_in_txn(write_txn, table_name)?;
    for (child_schema, fk) in &referencing {
        if let Some(names) = &updated_names {
            // `id` 参照は更新で失われない。列参照は、参照先列のいずれかが今回
            // 更新された列に含まれる場合のみ検査する。
            if fk.references_parent_id()
                || !fk
                    .parent_columns()
                    .iter()
                    .any(|c| names.contains(&c.as_str()))
            {
                continue;
            }
        }
        if !fk_mode.includes(fk) {
            continue;
        }
        verify_child_rows_for_tenant(write_txn, child_schema, fk, table_name, schema, tenant_id)?;
    }
    Ok(())
}

/// 子テーブル `child_schema` の同一テナント全行から `fk` が要求する値の組を集め、
/// 参照先 `parent_table`（スキーマ `parent_schema`）にすべて存在するか確かめる
/// （TABLE-17・TASK-205、Issue #907／#1077）。[`enforce_referencing_rows_in_txn`]
/// （事後状態の検証）と [`enforce_deferred_foreign_keys_in_txn`]（COMMIT 時の
/// 遅延検査）が共有する唯一の実装（挙動を 2 か所で重複させない）。
fn verify_child_rows_for_tenant(
    write_txn: &redb::WriteTransaction,
    child_schema: &TableSchema,
    fk: &ForeignKeyDef,
    parent_table: &str,
    parent_schema: &TableSchema,
    tenant_id: &str,
) -> Result<(), TenantWriteError> {
    // 参照元の同一テナント全行から、参照先に存在すべき値の組を集める。
    let (specs, mask) = foreign_key_specs(child_schema)?;
    let Some(spec) = specs.iter().find(|s| s.fk == fk) else {
        return Err(internal(
            "referencing foreign key not found in child schema",
        ));
    };
    let mut required = RequiredParentKeys::new(fk);
    {
        let row_table_name = crate::catalog::user_rows_table_name(&child_schema.name);
        let row_table =
            match write_txn.open_table(crate::catalog::user_rows_table_def(&row_table_name)) {
                Ok(t) => t,
                // 参照元へまだ 1 行も挿入されていない（行ストア未作成）。
                Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
                Err(e) => {
                    return Err(TenantWriteError::from(crate::catalog::map_row_table_error(
                        e,
                    )))
                }
            };
        let range_start = std::ops::Bound::Included((tenant_id, 0u64));
        let range_end = std::ops::Bound::Included((tenant_id, u64::MAX));
        for entry in row_table
            .range::<(&str, u64)>((range_start, range_end))
            .map_err(CatalogError::from)?
        {
            let (k, v) = entry.map_err(CatalogError::from)?;
            let (key_tenant, _id) = k.value();
            if key_tenant != tenant_id {
                break;
            }
            let values = decode_key_columns(child_schema, &mask, v.value())?;
            push_required_key(spec, &values, &mut required)?;
        }
        // `row_table` はここで drop してから `verify_required_parent_keys` が
        // 参照先の行ストアを開く（自己参照で redb の `TableAlreadyOpen` を
        // 避けるための順序。`parent_schema_for` 呼び出し元と同じ契約）。
    }
    verify_required_parent_keys(
        write_txn,
        parent_table,
        parent_schema,
        fk,
        tenant_id,
        required,
    )
}

/// 明示トランザクション（SQL-31・TASK-221）の COMMIT 時に、文単位検査から
/// 先送りしていた `INITIALLY DEFERRED` の `FOREIGN KEY` をまとめて検査する
/// （TABLE-17・TASK-205、Issue #1077）。`sql::transaction::SessionTransaction::commit`
/// が `written_by_tenant`（このトランザクション内で書き込んだ `(tenant, table)` の
/// 集合）の各要素について呼ぶ——記録漏れは検査漏れ＝fail-open のバグになるため、
/// 呼び出し元は書き込みのたびに必ず記録する契約（`sql::transaction::ActiveTxn`
/// ドキュメント参照）。
///
/// 事後状態（COMMIT 直前の最終状態）の全件検証を行う: `table` 自身が親として
/// 持つ `INITIALLY DEFERRED` の子（[`crate::catalog::referencing_foreign_keys_in_txn`]。
/// v9 対応が前提——v8 のみの逆引きだと v9 の子が漏れて fail-open になる）と、
/// `table` 自身が子として持つ `INITIALLY DEFERRED` の宣言の両方を、
/// [`verify_child_rows_for_tenant`] で検証する（`enforce_referencing_rows_in_txn`
/// と同じ実装を共有）。同一トランザクション内で複数文が同じ `(child, fk)` の
/// 組に触れても検査は 1 回で済むよう、呼び出し元がテーブル単位で重複排除する。
pub(crate) fn enforce_deferred_foreign_keys_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant_id: &str,
    table: &str,
) -> Result<(), TenantWriteError> {
    let schema = crate::catalog::require_table_schema_write(write_txn, table)?;

    // `table` を子とする宣言（このスキーマの `foreign_keys()`）。
    for fk in schema.foreign_keys() {
        if !fk.is_initially_deferred() {
            continue;
        }
        let parent = parent_schema_for(write_txn, table, fk)?;
        verify_child_rows_for_tenant(
            write_txn,
            &schema,
            fk,
            fk.parent_table(),
            parent.as_ref().unwrap_or(&schema),
            tenant_id,
        )?;
    }

    // `table` を親とする他テーブル（自己参照は上のループで既に検査済み）の宣言。
    let referencing = crate::catalog::referencing_foreign_keys_in_txn(write_txn, table)?;
    for (child_schema, fk) in &referencing {
        if !fk.is_initially_deferred() || child_schema.name == table {
            continue;
        }
        verify_child_rows_for_tenant(write_txn, child_schema, fk, table, &schema, tenant_id)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnDef, TableSchema};
    use crate::policy::PolicyContext;
    use crate::row_codec::Value;
    use crate::storage::{Storage, Visibility};
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

    fn tmp_storage(label: &str) -> (Storage, CleanupGuard) {
        let path = unique_db_path(label);
        let guard = CleanupGuard(path.clone());
        (Storage::open(&path).expect("open storage"), guard)
    }

    fn ctx(tenant: &'static str) -> PolicyContext {
        PolicyContext::new(tenant).expect("valid tenant id")
    }

    #[test]
    fn no_primary_key_is_a_no_op() {
        let (storage, _guard) = tmp_storage("constraint-no-pk");
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new(
                "body",
                crate::catalog::ColumnType::Text,
                true,
            )],
        );
        storage.create_table(&schema).expect("create table");
        let write_txn = storage.begin_write_txn().expect("begin write");
        enforce_unique_keys_in_txn(&write_txn, "docs", &schema, "tenant-a", &[1, 2, 3])
            .expect("no primary key must be a no-op");
        write_txn.abort().expect("abort");
    }

    #[test]
    fn detects_duplicate_within_same_batch() {
        let (storage, _guard) = tmp_storage("constraint-batch-dup");
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new(
                "code",
                crate::catalog::ColumnType::Text,
                false,
            )],
        )
        .with_primary_key(vec!["code".to_string()]);
        storage.create_table(&schema).expect("create table");

        let write_txn = storage.begin_write_txn().expect("begin write");
        {
            let mut table = write_txn
                .open_table(crate::catalog::user_rows_table_def(
                    &crate::catalog::user_rows_table_name("docs"),
                ))
                .expect("open row table");
            for id in [1u64, 2u64] {
                let metadata = crate::row_codec::encode_scalar_columns(
                    &schema,
                    &[Value::Text("same-code".to_string())],
                )
                .expect("encode scalar columns");
                let row = crate::storage::RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[],
                    metadata: &metadata,
                };
                let encoded = crate::storage::encode_row(&row).expect("encode row");
                table
                    .insert(("tenant-a", id), encoded.as_slice())
                    .expect("insert row");
            }
        }
        let err = enforce_unique_keys_in_txn(&write_txn, "docs", &schema, "tenant-a", &[1, 2])
            .expect_err("duplicate primary key within the same batch must be rejected");
        assert!(matches!(err, TenantWriteError::UniqueViolation));
        write_txn.abort().expect("abort");
    }

    #[test]
    fn detects_conflict_against_existing_tenant_row() {
        let (storage, _guard) = tmp_storage("constraint-existing-row");
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new(
                "code",
                crate::catalog::ColumnType::Text,
                false,
            )],
        )
        .with_primary_key(vec!["code".to_string()]);
        storage.create_table(&schema).expect("create table");

        crate::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx("tenant-a"),
            1,
            Visibility::Public,
            &[Value::Text("dup-code".to_string())],
            &crate::recovery::required_op_id::OperationId::parse("op-1").expect("op id"),
        )
        .expect("first insert must succeed");

        let write_txn = storage.begin_write_txn().expect("begin write");
        {
            let mut table = write_txn
                .open_table(crate::catalog::user_rows_table_def(
                    &crate::catalog::user_rows_table_name("docs"),
                ))
                .expect("open row table");
            let metadata = crate::row_codec::encode_scalar_columns(
                &schema,
                &[Value::Text("dup-code".to_string())],
            )
            .expect("encode scalar columns");
            let row = crate::storage::RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[],
                metadata: &metadata,
            };
            let encoded = crate::storage::encode_row(&row).expect("encode row");
            table
                .insert(("tenant-a", 2u64), encoded.as_slice())
                .expect("insert row");
        }
        let err = enforce_unique_keys_in_txn(&write_txn, "docs", &schema, "tenant-a", &[2])
            .expect_err("conflict against an existing tenant row must be rejected");
        assert!(matches!(err, TenantWriteError::UniqueViolation));
        write_txn.abort().expect("abort");
    }

    #[test]
    fn different_tenants_may_share_the_same_primary_key_value() {
        let (storage, _guard) = tmp_storage("constraint-cross-tenant");
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new(
                "code",
                crate::catalog::ColumnType::Text,
                false,
            )],
        )
        .with_primary_key(vec!["code".to_string()]);
        storage.create_table(&schema).expect("create table");

        crate::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx("tenant-a"),
            1,
            Visibility::Public,
            &[Value::Text("shared-code".to_string())],
            &crate::recovery::required_op_id::OperationId::parse("op-a").expect("op id"),
        )
        .expect("tenant-a insert must succeed");

        // 別テナントは同じ主キー値を持つ行を問題なく挿入できる（RLS-9・TABLE-16）。
        crate::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx("tenant-b"),
            1,
            Visibility::Public,
            &[Value::Text("shared-code".to_string())],
            &crate::recovery::required_op_id::OperationId::parse("op-b").expect("op id"),
        )
        .expect("tenant-b insert must succeed even with the same primary key value");
    }

    #[test]
    fn self_update_does_not_conflict_with_its_own_previous_value() {
        let (storage, _guard) = tmp_storage("constraint-self-update");
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new(
                "code",
                crate::catalog::ColumnType::Text,
                false,
            )],
        )
        .with_primary_key(vec!["code".to_string()]);
        storage.create_table(&schema).expect("create table");

        crate::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx("tenant-a"),
            1,
            Visibility::Public,
            &[Value::Text("code-1".to_string())],
            &crate::recovery::required_op_id::OperationId::parse("op-1").expect("op id"),
        )
        .expect("insert must succeed");

        // 同じ id への UPDATE（値は変えない）は自己衝突しない
        // （id ベースの自己除外。§モジュールドキュメント参照）。
        let write_txn = storage.begin_write_txn().expect("begin write");
        let schema_reloaded =
            crate::catalog::require_table_schema_write(&write_txn, "docs").expect("schema");
        enforce_unique_keys_in_txn(&write_txn, "docs", &schema_reloaded, "tenant-a", &[1])
            .expect("self-update with unchanged primary key must not conflict");
        write_txn.abort().expect("abort");
    }

    /// 生の行（テナント・id・スカラー値）を write トランザクション内で直接
    /// 書き込むテスト用ヘルパー（検査点を経由しない）。
    fn put_raw_row(
        write_txn: &redb::WriteTransaction,
        schema: &TableSchema,
        tenant: &str,
        id: u64,
        values: &[Value],
    ) {
        let mut table = write_txn
            .open_table(crate::catalog::user_rows_table_def(
                &crate::catalog::user_rows_table_name(&schema.name),
            ))
            .expect("open row table");
        let metadata =
            crate::row_codec::encode_scalar_columns(schema, values).expect("encode scalar columns");
        let row = crate::storage::RowInput {
            tenant_id: tenant,
            visibility: Visibility::Public,
            embedding: &[],
            metadata: &metadata,
        };
        let encoded = crate::storage::encode_row(&row).expect("encode row");
        table
            .insert((tenant, id), encoded.as_slice())
            .expect("insert row");
    }

    fn unique_schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("a", crate::catalog::ColumnType::Text, true),
                ColumnDef::new("b", crate::catalog::ColumnType::Text, true),
            ],
        )
        .with_unique_constraints(vec![crate::catalog::UniqueConstraint::new(vec![
            "a".to_string(),
            "b".to_string(),
        ])])
    }

    /// UNIQUE 制約は NULLS DISTINCT: 構成列のいずれかが NULL の行同士は衝突
    /// しない（主キーの NULL 拒否とは異なる扱い。Issue #905）。
    #[test]
    fn unique_constraint_skips_rows_with_null_component() {
        let (storage, _guard) = tmp_storage("constraint-unique-null");
        let schema = unique_schema();
        storage.create_table(&schema).expect("create table");
        let write_txn = storage.begin_write_txn().expect("begin write");
        for id in [1u64, 2u64] {
            put_raw_row(
                &write_txn,
                &schema,
                "tenant-a",
                id,
                &[Value::Text("same".to_string()), Value::Null],
            );
        }
        enforce_unique_keys_in_txn(&write_txn, "docs", &schema, "tenant-a", &[1, 2])
            .expect("rows with a NULL component must not conflict");
        write_txn.abort().expect("abort");
    }

    /// 複合 UNIQUE 制約は全構成列が一致した場合のみ衝突し、既存行との衝突も
    /// 同一テナント内に限って検出する。
    #[test]
    fn composite_unique_constraint_detects_full_match_within_tenant_only() {
        let (storage, _guard) = tmp_storage("constraint-unique-composite");
        let schema = unique_schema();
        storage.create_table(&schema).expect("create table");
        let write_txn = storage.begin_write_txn().expect("begin write");
        let xy = [Value::Text("x".to_string()), Value::Text("y".to_string())];
        put_raw_row(&write_txn, &schema, "tenant-b", 1, &xy);
        put_raw_row(
            &write_txn,
            &schema,
            "tenant-a",
            1,
            &[Value::Text("x".to_string()), Value::Text("z".to_string())],
        );
        put_raw_row(&write_txn, &schema, "tenant-a", 2, &xy);
        // tenant-b の同値・tenant-a の部分一致とは衝突しない。
        enforce_unique_keys_in_txn(&write_txn, "docs", &schema, "tenant-a", &[2])
            .expect("partial match and other tenants' values must not conflict");
        put_raw_row(&write_txn, &schema, "tenant-a", 3, &xy);
        let err = enforce_unique_keys_in_txn(&write_txn, "docs", &schema, "tenant-a", &[3])
            .expect_err("full match within the tenant must conflict");
        assert!(matches!(err, TenantWriteError::UniqueViolation));
        write_txn.abort().expect("abort");
    }

    /// 制約追加前の既存行重複判定（`table_has_duplicate_unique_key`）は
    /// テナントごとに独立し、NULL を含む行を対象外とする。
    #[test]
    fn table_has_duplicate_unique_key_is_scoped_per_tenant_and_skips_nulls() {
        let (storage, _guard) = tmp_storage("constraint-unique-alter-scan");
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new("a", crate::catalog::ColumnType::Text, true)],
        );
        storage.create_table(&schema).expect("create table");
        let write_txn = storage.begin_write_txn().expect("begin write");
        let x = [Value::Text("x".to_string())];
        put_raw_row(&write_txn, &schema, "tenant-a", 1, &x);
        put_raw_row(&write_txn, &schema, "tenant-b", 1, &x);
        put_raw_row(&write_txn, &schema, "tenant-a", 2, &[Value::Null]);
        put_raw_row(&write_txn, &schema, "tenant-a", 3, &[Value::Null]);
        let columns = vec!["a".to_string()];
        {
            let table = write_txn
                .open_table(crate::catalog::user_rows_table_def(
                    &crate::catalog::user_rows_table_name("docs"),
                ))
                .expect("open row table");
            assert!(!table_has_duplicate_unique_key(&table, &schema, &columns)
                .expect("scan must succeed"));
        }
        put_raw_row(&write_txn, &schema, "tenant-b", 2, &x);
        {
            let table = write_txn
                .open_table(crate::catalog::user_rows_table_def(
                    &crate::catalog::user_rows_table_name("docs"),
                ))
                .expect("open row table");
            assert!(table_has_duplicate_unique_key(&table, &schema, &columns)
                .expect("scan must succeed"));
        }
        write_txn.abort().expect("abort");
    }

    // --- Issue #1073: UNIQUE 制約の対象型拡張（REAL・DOUBLE PRECISION・
    // NUMERIC・JSON／JSONB・配列型）の正準キー生成 -------------------------

    /// REAL／DOUBLE PRECISION の `-0.0` は `+0.0` と同一の正準キーへ正規化される
    /// （`push_canonical_component` が `scalar_float::canonicalize_*` を経由する）。
    /// 非有限値（NaN）は `row_codec` の encode 側が既に拒否するため通常到達
    /// しないが、defense in depth として本関数レベルでも `Err` を確認する。
    #[test]
    fn push_canonical_component_normalizes_real_and_double_negative_zero() {
        let mut neg = Vec::new();
        push_canonical_component(&mut neg, ScalarRef::Real(-0.0)).expect("finite");
        let mut pos = Vec::new();
        push_canonical_component(&mut pos, ScalarRef::Real(0.0)).expect("finite");
        assert_eq!(neg, pos);

        let mut neg_d = Vec::new();
        push_canonical_component(&mut neg_d, ScalarRef::Double(-0.0)).expect("finite");
        let mut pos_d = Vec::new();
        push_canonical_component(&mut pos_d, ScalarRef::Double(0.0)).expect("finite");
        assert_eq!(neg_d, pos_d);

        let mut nan_out = Vec::new();
        assert!(push_canonical_component(&mut nan_out, ScalarRef::Real(f32::NAN)).is_err());
        let mut nan_out_d = Vec::new();
        assert!(push_canonical_component(&mut nan_out_d, ScalarRef::Double(f64::NAN)).is_err());
    }

    /// NUMERIC は末尾ゼロを除去した最簡表現で同一キーになる（`1.50` と `1.5`）。
    #[test]
    fn push_canonical_component_normalizes_numeric_trailing_zeros() {
        let a = crate::numeric::Decimal::from_parts(150, 2).expect("valid decimal"); // 1.50
        let b = crate::numeric::Decimal::from_parts(15, 1).expect("valid decimal"); // 1.5
        let mut out_a = Vec::new();
        push_canonical_component(&mut out_a, ScalarRef::Numeric(a)).expect("ok");
        let mut out_b = Vec::new();
        push_canonical_component(&mut out_b, ScalarRef::Numeric(b)).expect("ok");
        assert_eq!(out_a, out_b);

        // 末尾ゼロを持たない異なる値は別キーになる。
        let c = crate::numeric::Decimal::from_parts(151, 2).expect("valid decimal"); // 1.51
        let mut out_c = Vec::new();
        push_canonical_component(&mut out_c, ScalarRef::Numeric(c)).expect("ok");
        assert_ne!(out_a, out_c);
    }

    fn extended_types_schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("r", crate::catalog::ColumnType::Real, true),
                ColumnDef::new(
                    "n",
                    crate::catalog::ColumnType::Numeric {
                        precision: 10,
                        scale: 2,
                    },
                    true,
                ),
                ColumnDef::new("j", crate::catalog::ColumnType::Json, true),
                ColumnDef::new(
                    "arr",
                    crate::catalog::ColumnType::Array(
                        crate::catalog::ArrayType::new(crate::catalog::ArrayElemType::Text, 8)
                            .expect("valid array type"),
                    ),
                    true,
                ),
            ],
        )
        .with_unique_constraints(vec![
            crate::catalog::UniqueConstraint::new(vec!["j".to_string()]),
            crate::catalog::UniqueConstraint::new(vec!["arr".to_string()]),
        ])
    }

    /// JSON 列は正準化前のテキスト表現（キー順・空白）が異なっても値として
    /// 等価なら UNIQUE 制約に違反する（`json::canonical_equality_text` 経由。
    /// Issue #1073）。
    #[test]
    fn unique_constraint_on_json_column_detects_value_equal_but_textually_different_rows() {
        let (storage, _guard) = tmp_storage("constraint-unique-json");
        let schema = extended_types_schema();
        storage.create_table(&schema).expect("create table");
        let write_txn = storage.begin_write_txn().expect("begin write");
        let row = |json: &str| {
            vec![
                Value::Null,
                Value::Null,
                Value::Json(json.to_string()),
                Value::Null,
            ]
        };
        put_raw_row(&write_txn, &schema, "tenant-a", 1, &row(r#"{"a":1,"b":2}"#));
        enforce_unique_keys_in_txn(&write_txn, "docs", &schema, "tenant-a", &[1])
            .expect("first row must succeed");
        // キー順・空白だけが異なるが値として等価な JSON テキスト。
        put_raw_row(
            &write_txn,
            &schema,
            "tenant-a",
            2,
            &row(r#"{ "b": 2, "a": 1 }"#),
        );
        let err = enforce_unique_keys_in_txn(&write_txn, "docs", &schema, "tenant-a", &[2])
            .expect_err("value-equal JSON must conflict despite differing text");
        assert!(matches!(err, TenantWriteError::UniqueViolation));
        write_txn.abort().expect("abort");
    }

    /// 配列列の UNIQUE 制約は要素順を区別する（`{a,b}` と `{b,a}` は衝突しない）。
    #[test]
    fn unique_constraint_on_array_column_distinguishes_element_order() {
        let (storage, _guard) = tmp_storage("constraint-unique-array");
        let schema = extended_types_schema();
        storage.create_table(&schema).expect("create table");
        let write_txn = storage.begin_write_txn().expect("begin write");
        let row = |items: &[&str]| {
            vec![
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Array(crate::row_codec::ArrayValue::Text(
                    items.iter().map(|s| s.to_string()).collect(),
                )),
            ]
        };
        put_raw_row(&write_txn, &schema, "tenant-a", 1, &row(&["a", "b"]));
        enforce_unique_keys_in_txn(&write_txn, "docs", &schema, "tenant-a", &[1])
            .expect("first row must succeed");
        // 要素順が異なる配列は衝突しない。
        put_raw_row(&write_txn, &schema, "tenant-a", 2, &row(&["b", "a"]));
        enforce_unique_keys_in_txn(&write_txn, "docs", &schema, "tenant-a", &[2])
            .expect("different element order must not conflict");
        // 完全一致は衝突する。
        put_raw_row(&write_txn, &schema, "tenant-a", 3, &row(&["a", "b"]));
        let err = enforce_unique_keys_in_txn(&write_txn, "docs", &schema, "tenant-a", &[3])
            .expect_err("identical array must conflict");
        assert!(matches!(err, TenantWriteError::UniqueViolation));
        write_txn.abort().expect("abort");
    }
}
