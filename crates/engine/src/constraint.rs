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
//! # 参照アクション（`CASCADE`・`SET NULL`・`SET DEFAULT`。Issue #1076）
//!
//! [`enforce_referencing_rows_in_txn`] は参照先側の検査を 2 段構えで行う:
//! (1) [`propagate_referential_actions`] が宣言済みの参照アクションを子テーブルへ
//! 再帰的に適用し、(2) 元の対象テーブルと連鎖で変更した各テーブルについて、
//! それを参照する全 `FOREIGN KEY`（アクションの有無を問わない）の事後状態検証
//! （[`verify_no_action_backstop`]。既存の `NO ACTION` 検査）を行う。(1) の実装に
//! 不具合があっても (2) が最終状態の参照整合性を fail-closed に保証する。連鎖の
//! 深さ・1 文あたりの対象行数には実装既定の上限
//! （[`MAX_REFERENTIAL_ACTION_DEPTH`]・[`MAX_REFERENTIAL_ACTION_ROWS`]。
//! spec 由来ではない）があり、超過は適用前に
//! `TenantWriteError::ReferentialActionLimitExceeded`（副作用ゼロ）で拒否する。
//! `TRUNCATE`（[`ReferencedRowsChange::Truncated`]）は連鎖を発火させない（(2) の
//! 事後検証のみ行う）。詳細な設計判断は `docs/design/foreign-key.md`「D13〜D19」
//! 節参照。
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
//! # 実装方式（既知の制約）
//!
//! 永続一意索引は導入せず、書き込み対象行を除いたテナント全行
//! （`(tenant_id, 0)..=(tenant_id, u64::MAX)` の物理キー範囲。可視性・
//! テーブル世代キャッシュのいずれも経由しない生の redb 走査）を 1 文あたり
//! 1 回だけ線形走査し、全キーの衝突をまとめて判定する。計算量はテナントの
//! 保有行数に比例するため、一意キーを宣言したテーブルへの書き込みは行数の
//! 多いテナントほど遅くなる（`docs/design/sql-primary-key.md`・
//! `docs/design/unique-constraint.md` 参照。永続索引化は将来の別課題）。
//! `tenant::enumerate_dml_candidates` が持つ [`crate::tenant`] 内部の総走査上限
//! （`MAX_SCANNED_ROWS`）は意図的に継承しない——継承すると、その上限を超える
//! 行数を既に保有するテナントが一意キー宣言テーブルへ一切書き込めなくなる
//! fail-closed 過ぎる制約になってしまうため。
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

use crate::catalog::{
    CatalogError, ColumnType, ForeignKeyDef, ForeignKeyMatch, ReferentialAction, TableSchema,
};
use crate::row_codec::ScalarRef;
use crate::tenant::TenantWriteError;
use redb::ReadableTable;
use std::collections::{BTreeSet, HashMap, HashSet};

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
/// 判定手順（全キーをまとめて扱い、テナント範囲の走査は 1 回だけ行う）:
/// 1. `written_ids` 各行について各キーの正準バイト列を計算し、`written_ids`
///    同士の重複（同一文内で 2 行が同じキー値を持つ）を検出する。
/// 2. 対象テナントの全行（`written_ids` を除く）を走査し、1. のいずれかと
///    一致するキー値を持つ行がないか調べる（自己更新の除外は `written_ids`
///    からの除外そのもので実現される。新旧値の比較は行わない）。
/// 3. いずれかで衝突が見つかった時点で [`TenantWriteError::UniqueViolation`]
///    を返す（最初の 1 件で打ち切り。副作用は呼び出し元が `write_txn` を
///    commit しないことで防ぐ）。
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

    // 1. 今回書き込んだ行同士のキー値衝突を検出する（キーごとに独立した表）。
    let written_id_set: HashSet<u64> = written_ids.iter().copied().collect();
    let mut written_keys: Vec<HashMap<Vec<u8>, u64>> =
        specs.iter().map(|_| HashMap::new()).collect();
    for &id in &written_id_set {
        let Some(guard) = row_table
            .get((tenant_id, id))
            .map_err(crate::catalog::CatalogError::from)?
        else {
            // 呼び出し元は必ず同一 txn 内で先に書き込み済みのはずだが、内部
            // 不変条件の欠落があっても黙って読み飛ばさず、判定対象から
            // 除外するに留める（この行が存在しない以上、一意性判定の対象には
            // なり得ない）。
            continue;
        };
        let buf = guard.value();
        let values = decode_key_columns(schema, &mask, buf)?;
        for (spec, keys) in specs.iter().zip(written_keys.iter_mut()) {
            let Some(key) = key_bytes(spec, &values).map_err(internal)? else {
                continue;
            };
            if let Some(existing_id) = keys.insert(key, id) {
                if existing_id != id {
                    return Err(TenantWriteError::UniqueViolation);
                }
            }
        }
    }
    if written_keys.iter().all(HashMap::is_empty) {
        return Ok(());
    }

    // 2. 対象テナントの残り全行（今回書き込んだ id を除く）を 1 回だけ走査する。
    // 物理キーは `(tenant_id, id)` の辞書順であり、`(tenant, 0)..=(tenant,
    // u64::MAX)` の閉区間が対象テナントの物理キー空間の全域を過不足なく覆う
    // （`tenant::enumerate_dml_candidates` と同じ範囲構築。RLS-9・TABLE-12）。
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
            // （`enumerate_dml_candidates` と同じ判断）。
            break;
        }
        if written_id_set.contains(&id) {
            // 自己更新・自己挿入の除外は id 一致そのもので行う（新旧値の
            // 比較はしない。§モジュールドキュメント参照）。
            continue;
        }
        let buf = v.value();
        let values = decode_key_columns(schema, &mask, buf)?;
        for (spec, keys) in specs.iter().zip(written_keys.iter()) {
            if keys.is_empty() {
                continue;
            }
            let Some(key) = key_bytes(spec, &values).map_err(internal)? else {
                continue;
            };
            if keys.contains_key(&key) {
                return Err(TenantWriteError::UniqueViolation);
            }
        }
    }

    Ok(())
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
    /// 行の削除（単一行・述語つき `DELETE`・ファイル形 `INSERT` の旧行置換）。
    /// `id` を含むすべての参照先キーが失われ得る。参照アクション（Issue #1076）を
    /// 発火させる（[`Self::Truncated`] とは異なる）。
    Removed,
    /// 既存行の指定列（論理インデックス）のみの更新（`UPDATE ... SET`・UPSERT の
    /// `DO UPDATE SET`）。`id` は予約列で `SET` できないため失われない。
    ColumnsUpdated(&'a [usize]),
    /// 既存行の全列置換（Rust API の `update_row`）。`id` は不変。
    AllColumnsReplaced,
    /// `TRUNCATE`（Issue #1076 A4）。全行削除だが、PostgreSQL の `TRUNCATE` と
    /// 同様に参照アクションを発火させない（`TRUNCATE ... CASCADE` は別構文で
    /// 未実装のまま `42601`）。事後検証（NO ACTION の背後保証）のみ行う。
    Truncated,
}

/// ON DELETE／ON UPDATE の連鎖（Issue #1076）が 1 文あたりに辿ってよい深さ
/// （元の文を 0 段目とする）。実装既定値であり spec 由来ではない（MySQL の
/// 15 段と同じ桁を採用）。
pub(crate) const MAX_REFERENTIAL_ACTION_DEPTH: u32 = 16;

/// ON DELETE／ON UPDATE の連鎖（Issue #1076）が 1 文あたりに削除・更新してよい
/// 自テナント所有の子行の総数（元の文が直接対象にした行は含まない）。実装既定値
/// であり spec 由来ではない。
pub(crate) const MAX_REFERENTIAL_ACTION_ROWS: u32 = 10_000;

/// ON UPDATE CASCADE／SET NULL／SET DEFAULT（Issue #1076）の連鎖起点となる、
/// 更新前の行の全列値（`row_codec::decode_scalar_columns` が返す論理列順）。
/// `tenant.rs` の各更新関数が、既存行を書き換える**前**に読み取った値を積む。
/// 参照先キー（主キー・UNIQUE 構成列）を含まない更新では、呼び出し元は捕捉を
/// 省いてよい（[`enforce_referencing_rows_in_txn`] は `None` を渡された場合、
/// ON UPDATE アクションを一切発火させない——事後検証〔NO ACTION 相当〕は
/// 引き続き行われる）。
#[derive(Default)]
pub(crate) struct UpdatedKeyPreImages {
    old_values: HashMap<u64, Vec<crate::row_codec::Value>>,
}

impl UpdatedKeyPreImages {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 更新前の id `id` の全列値 `values` を記録する（`tenant.rs` の書き込み
    /// 関数が既存行を上書きする直前に呼ぶ契約）。
    ///
    /// 同じ `id` を同一文内で複数回記録しようとした場合（Cursor Bugbot 指摘・
    /// PR #1138）は最初の呼び出しだけを残す。複数行 `UPSERT` の `UNIQUE` 対象で
    /// 複数の `VALUES` 行が同じ既存行に衝突すると、2 回目以降の呼び出しは
    /// 「1 回目の更新が反映された後」の中間状態を渡す——`collect_action_targets`
    /// の `ColumnsUpdated` 分岐はこの値を「文が始まる前」の旧キーとして使うため、
    /// 後勝ちで上書きすると本来の旧キーを失い、対象の子行を取り違える（見失う・
    /// 別のキーへ連鎖する）。最初の記録が唯一の真の文実行前スナップショットで
    /// あるため、以降の呼び出しは無視する。
    pub(crate) fn record(&mut self, id: u64, values: Vec<crate::row_codec::Value>) {
        self.old_values.entry(id).or_insert(values);
    }
}

/// [`crate::row_codec::Value`] を一意キー判定と同じ正準バイト列の入力
/// （[`ScalarRef`]）へ変換する。`FOREIGN KEY` 列は
/// [`ColumnType::is_primary_key_allowed`] が許可する型（`Text`・`Integer`・
/// `BigInt`・`Boolean`・`Date`・`Timestamp`・`Uuid`・`Bytea`・`Enum`）に限られる
/// （`validate_foreign_keys` が宣言時に強制する）ため、それ以外の `Value`
/// variant（`Vector`・`Real`・`Double`・`Numeric`・`Json`・`Array`）は FK 列の値
/// としては現れないはずの内部不変条件であり `None` を返す（fail-closed。
/// 呼び出し元は NULL と同様に「キー無し」として扱う）。
fn value_as_scalar_ref(value: &crate::row_codec::Value) -> Option<ScalarRef<'_>> {
    use crate::row_codec::Value;
    match value {
        Value::Text(s) => Some(ScalarRef::Text(s)),
        Value::Integer(i) => Some(ScalarRef::Integer(*i)),
        Value::BigInt(i) => Some(ScalarRef::BigInt(*i)),
        Value::Bool(b) => Some(ScalarRef::Bool(*b)),
        Value::Date(d) => Some(ScalarRef::Date(*d)),
        Value::Timestamp(t) => Some(ScalarRef::Timestamp(*t)),
        Value::Uuid(u) => Some(ScalarRef::Uuid(*u)),
        Value::Bytes(b) => Some(ScalarRef::Bytes(b)),
        Value::Enum(s) => Some(ScalarRef::Enum(s)),
        Value::Null
        | Value::Vector(_)
        | Value::Real(_)
        | Value::Double(_)
        | Value::Array(_)
        | Value::Json(_)
        | Value::Numeric(_) => None,
    }
}

/// 子テーブルの走査で集めた FK 参照元列の値ごとのキー（[`scan_child_fk_rows`]）。
/// `id` 参照（[`ForeignKeyDef::references_parent_id`]）は物理キーの `id` 値、
/// それ以外は一意性検査と同じ正準キーバイト列で持つ。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ChildKey {
    Id(u64),
    Bytes(Vec<u8>),
}

/// `fk` の参照元列（`child_schema` 側）の値から [`ChildKey`] を組み立てる。
/// いずれかの構成列が NULL の組は `None`（MATCH SIMPLE。参照アクションの対象外）。
fn child_key_from_values(
    fk: &ForeignKeyDef,
    indices: &[usize],
    values: &[Option<ScalarRef<'_>>],
) -> Result<Option<ChildKey>, TenantWriteError> {
    if fk.references_parent_id() {
        let &idx = indices
            .first()
            .ok_or_else(|| internal("id-referencing foreign key must have one column"))?;
        let id = match values.get(idx).and_then(|v| v.as_ref()) {
            None => return Ok(None),
            Some(ScalarRef::Integer(v)) => u64::try_from(*v).ok(),
            Some(ScalarRef::BigInt(v)) => u64::try_from(*v).ok(),
            Some(_) => {
                return Err(internal(
                    "id-referencing foreign key column has a non-integer value",
                ))
            }
        };
        Ok(id.map(ChildKey::Id))
    } else {
        let key_spec = KeySpec {
            indices: indices.to_vec(),
            null_policy: NullPolicy::Skip,
        };
        Ok(key_bytes(&key_spec, values)
            .map_err(internal)?
            .map(ChildKey::Bytes))
    }
}

/// 子テーブル `child_schema` の同一テナントの全行（可視性を問わない。RLS-10 (c)）を
/// 走査し、`fk` の参照元列の値（非 NULL）ごとに一致する子行 id を集める
/// （Issue #1076。ON DELETE／ON UPDATE の対象特定・NO ACTION 事後検証のいずれの
/// 母集合も同一テナント全行である契約〔A9〕に従う）。
///
/// `wanted_keys` が `Some` の場合はその集合に含まれるキーの行だけを保持し、
/// それ以外の行は一致判定の直後に捨てる（キーには追加しない）。`None` の場合は
/// 全 distinct キーを保持する（`pre_images` を渡さないフォールバック専用。
/// どのキーが目的か呼び出し元が特定できない場合にのみ使う）。
///
/// いずれの場合も、走査中に一致件数が `remaining_budget`（呼び出し元が算出する
/// `MAX_REFERENTIAL_ACTION_ROWS` の残り枠）を超えた時点で走査を打ち切り
/// `TenantWriteError::ReferentialActionLimitExceeded`（`54000`）を返す
/// （codex-review 指摘・PR #1138: 収集完了を待たずに打ち切ることで、削除前
/// キーと無関係に大きい子テーブルでも上限を超える分のメモリを確保しない）。
fn scan_child_fk_rows_for_keys(
    write_txn: &redb::WriteTransaction,
    child_schema: &TableSchema,
    fk: &ForeignKeyDef,
    tenant_id: &str,
    wanted_keys: Option<&HashSet<ChildKey>>,
    remaining_budget: u32,
) -> Result<HashMap<ChildKey, Vec<u64>>, TenantWriteError> {
    let mut out: HashMap<ChildKey, Vec<u64>> = HashMap::new();
    if wanted_keys.is_some_and(|w| w.is_empty()) {
        return Ok(out);
    }
    let mut indices = Vec::with_capacity(fk.columns().len());
    let mut mask = vec![false; child_schema.columns.len()];
    for name in fk.columns() {
        let idx = child_schema
            .columns
            .iter()
            .position(|c| &c.name == name)
            .ok_or_else(|| internal("foreign key column not found in live schema columns"))?;
        if let Some(slot) = mask.get_mut(idx) {
            *slot = true;
        }
        indices.push(idx);
    }
    let row_table_name = crate::catalog::user_rows_table_name(&child_schema.name);
    let row_table = match write_txn.open_table(crate::catalog::user_rows_table_def(&row_table_name))
    {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(out),
        Err(e) => {
            return Err(TenantWriteError::from(crate::catalog::map_row_table_error(
                e,
            )))
        }
    };
    let mut matched: u32 = 0;
    let range_start = std::ops::Bound::Included((tenant_id, 0u64));
    let range_end = std::ops::Bound::Included((tenant_id, u64::MAX));
    for entry in row_table
        .range::<(&str, u64)>((range_start, range_end))
        .map_err(CatalogError::from)?
    {
        let (k, v) = entry.map_err(CatalogError::from)?;
        let (key_tenant, id) = k.value();
        if key_tenant != tenant_id {
            break;
        }
        let values = decode_key_columns(child_schema, &mask, v.value())?;
        let Some(key) = child_key_from_values(fk, &indices, &values)? else {
            continue;
        };
        if let Some(wanted) = wanted_keys {
            if !wanted.contains(&key) {
                continue;
            }
        }
        matched = matched
            .checked_add(1)
            .ok_or_else(|| internal("referential action row count overflow"))?;
        if matched > remaining_budget {
            return Err(TenantWriteError::ReferentialActionLimitExceeded);
        }
        out.entry(key).or_default().push(id);
    }
    Ok(out)
}

/// `ids` に該当する子テーブル `child_schema` の行の**現在**の全列値を読み取る
/// （Cursor Bugbot 指摘・PR #1138）。`propagate_referential_actions` の Pass 1
/// （対象特定。このテーブルへの Pass 2 の書き込みをまだ一切行っていない時点）
/// から呼ぶ契約。
///
/// 同じ子行を複数の `FOREIGN KEY` が対象にする場合（例: 一方が `ON DELETE
/// SET NULL`・他方が `ON DELETE CASCADE`）、Pass 2 で先に適用された FK の
/// 書き込みが後の FK の `apply_referential_action` が読む「現在の行」を
/// 変えてしまう。`apply_referential_action` は実際の書き込み（read-merge-write）
/// にはこの「現在の行」を正しく使う必要がある一方、孫段の連鎖対象特定
/// （`collect_action_targets` の `wanted_keys`）が使う pre-image は「今回の文が
/// 始まる前」の値でなければならない（先に適用された SET NULL／SET DEFAULT の
/// 結果を pre-image に取り込むと、孫が参照する本来の旧キーが失われ、連鎖されず
/// 残った孫行が事後検証の `NO ACTION` バックストップで `23503` になる）。この
/// 関数を Pass 1 で 1 回呼んで結果を保持し、Pass 2 の各 `apply_referential_action`
/// 呼び出しへ渡すことで、この 2 つの用途を安全に分離する。
///
/// 読み戻せない id（Pass 1 の時点で既に存在しない）は結果に含めない。
fn snapshot_child_rows_before_pass2(
    write_txn: &redb::WriteTransaction,
    child_schema: &TableSchema,
    tenant_id: &str,
    ids: impl Iterator<Item = u64>,
) -> Result<HashMap<u64, Vec<crate::row_codec::Value>>, TenantWriteError> {
    let mut out = HashMap::new();
    let row_table_name = crate::catalog::user_rows_table_name(&child_schema.name);
    let row_table = match write_txn.open_table(crate::catalog::user_rows_table_def(&row_table_name))
    {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(out),
        Err(e) => {
            return Err(TenantWriteError::from(crate::catalog::map_row_table_error(
                e,
            )))
        }
    };
    for id in ids {
        let Some(guard) = row_table.get((tenant_id, id)).map_err(CatalogError::from)? else {
            continue;
        };
        let existing = crate::storage::decode_row_for_key(tenant_id, id, guard.value())
            .map_err(TenantWriteError::Storage)?;
        // `guard`（`row_table` の不変借用）は `existing` がデータを複製済みの
        // ため、明示的な drop は不要（このスコープ内で `row_table` への可変
        // 借用を行わないため、借用チェッカ上も問題ない）。
        let values = crate::row_codec::decode_scalar_columns(child_schema, &existing.metadata)
            .map_err(|e| CatalogError::Invalid(e.to_string()))?;
        out.insert(id, values);
    }
    Ok(out)
}

/// 再帰の内部で親から子へ渡す、所有版の変更表現（[`ReferencedRowsChange`] は
/// 借用スライスを持つため再帰境界をまたげない。Issue #1076）。
enum PropagatedChange {
    Removed,
    ColumnsUpdated(Vec<usize>),
}

impl PropagatedChange {
    fn from_public(change: ReferencedRowsChange<'_>, schema: &TableSchema) -> Option<Self> {
        match change {
            ReferencedRowsChange::Removed => Some(PropagatedChange::Removed),
            ReferencedRowsChange::ColumnsUpdated(idx) => {
                Some(PropagatedChange::ColumnsUpdated(idx.to_vec()))
            }
            ReferencedRowsChange::AllColumnsReplaced => Some(PropagatedChange::ColumnsUpdated(
                (0..schema.columns.len()).collect(),
            )),
            // TRUNCATE は参照アクションを発火させない（A4）。
            ReferencedRowsChange::Truncated => None,
        }
    }
}

/// 連鎖の適用中に積み上げる状態（Issue #1076）。深さ・行数の上限判定と、
/// 事後検証（[`verify_no_action_backstop`]）を後で行う対象テーブルの記録を兼ねる。
struct ActionState {
    rows_budget_used: u32,
    /// 連鎖で変更した (テーブル名, スキーマ) の一覧（事後検証対象。重複しうる）。
    touched: Vec<(String, TableSchema)>,
}

/// `FOREIGN KEY` の参照先側の検査（TABLE-17・TASK-205、Issue #907）。テーブル
/// `table_name`（スキーマ `schema`）の行を削除・更新した write トランザクション内で、
/// 行の変更・台帳記録の**後**・commit の**前**に呼ぶ（参照元側と同じ検査点・同じ
/// 順序。`operation_id` の再送判定〔`23505`／`22023`〕が本検査より優先される）。
///
/// 実行順序（Issue #1076 A3）: (1) 参照アクション（`CASCADE`・`SET NULL`・
/// `SET DEFAULT`）を連鎖的にすべて適用する → (2) 元のテーブルおよび連鎖で
/// 変更した各テーブルについて、それを参照する**全 FK**（アクションを問わない）の
/// 事後状態検証（[`verify_no_action_backstop`]）を行う。アクション適用にバグが
/// あっても、最終状態で参照整合性が成り立たなければ `23503` で拒否される
/// （fail-closed の最終不変条件）。
///
/// `pre_images` は ON UPDATE アクションの起点（更新前のキー値）。`None`（または
/// `change` が `Removed`／`Truncated`）の場合は ON UPDATE アクションを発火させない
/// （呼び出し元が主キー・UNIQUE 構成列を含まない更新と判定した場合等）。
///
/// 連鎖の走査・変更対象はすべてサーバー側導出テナント `tenant_id` の物理キー範囲
/// （`(tenant_id, 0)..=(tenant_id, u64::MAX)`。TABLE-12）に閉じ、他テナントの行は
/// 一切読み書きしない（RLS-9・RLS-10 (c)。テナント所有の不可視行も対象に含む）。
/// 連鎖の深さ・総行数が [`MAX_REFERENTIAL_ACTION_DEPTH`]・
/// [`MAX_REFERENTIAL_ACTION_ROWS`] を超えた場合は
/// [`TenantWriteError::ReferentialActionLimitExceeded`]（副作用ゼロ。適用前に判定）。
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
    pre_images: Option<&UpdatedKeyPreImages>,
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
        ReferencedRowsChange::Removed | ReferencedRowsChange::Truncated => None,
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

    let mut state = ActionState {
        rows_budget_used: 0,
        touched: Vec::new(),
    };
    if let Some(propagated) = PropagatedChange::from_public(change, schema) {
        propagate_referential_actions(
            write_txn, table_name, schema, tenant_id, propagated, pre_images, fk_mode, 0,
            &mut state,
        )?;
    }

    verify_no_action_backstop(
        write_txn,
        table_name,
        schema,
        tenant_id,
        updated_names.as_deref(),
        fk_mode,
    )?;
    // 連鎖で変更した各テーブルも全 FK について事後検証する（重複するテーブルへの
    // 再検証は無駄だが安全側であり、連鎖の総行数は上限で有界なため許容する）。
    for (touched_table, touched_schema) in &state.touched {
        verify_no_action_backstop(
            write_txn,
            touched_table,
            touched_schema,
            tenant_id,
            None,
            fk_mode,
        )?;
    }
    Ok(())
}

/// [`enforce_referencing_rows_in_txn`] の事後検証本体（Issue #907 の既存挙動）。
/// `table_name` を参照する各 `FOREIGN KEY` について、参照元の同一テナント全行の
/// 値の組が現在の `table_name` にすべて存在することを確かめる。`updated_names`
/// が `Some` の場合、参照先列がそれらに含まれない FK は検査を省く（連鎖で変更した
/// テーブルは `None` を渡し、常に全 FK を検査する）。`fk_mode` が `ImmediateOnly`
/// のとき `INITIALLY DEFERRED` の FK は検査対象から除く（TABLE-17・TASK-205、
/// Issue #1077）。
fn verify_no_action_backstop(
    write_txn: &redb::WriteTransaction,
    table_name: &str,
    schema: &TableSchema,
    tenant_id: &str,
    updated_names: Option<&[&str]>,
    fk_mode: FkCheckMode,
) -> Result<(), TenantWriteError> {
    let referencing = crate::catalog::referencing_foreign_keys_in_txn(write_txn, table_name)?;
    for (child_schema, fk) in &referencing {
        if let Some(names) = updated_names {
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

/// 連鎖の対象子行 1 件分: `(子行 id, ON UPDATE CASCADE の新しい参照先キー値。
/// それ以外のアクションでは `None`)`。[`collect_action_targets`] の戻り値・
/// [`apply_referential_action`] の入力で共有する（Issue #1076）。
type ActionTargets = Vec<(u64, Option<Vec<crate::row_codec::Value>>)>;

/// `propagate_referential_actions` の Pass 1（対象特定）が FK 1 個分について
/// 確定した情報: `(子スキーマ, FK 宣言, アクション, 対象子行, Pass 1 時点の
/// 行スナップショット)`。最後の要素は [`apply_referential_action`] が孫段への
/// pre-image に使う（`snapshot_child_rows_before_pass2` 参照。Cursor Bugbot
/// 指摘・PR #1138）。
type PendingReferentialAction<'a> = (
    &'a TableSchema,
    &'a ForeignKeyDef,
    ReferentialAction,
    ActionTargets,
    HashMap<u64, Vec<crate::row_codec::Value>>,
);

/// `table_name`（スキーマ `schema`）に加えた変更 `change` を起点に、参照アクション
/// （`CASCADE`・`SET NULL`・`SET DEFAULT`。Issue #1076）を再帰的に子テーブルへ
/// 適用する。`depth` は元の文を 0 段目とした連鎖の段数。適用した子テーブルは
/// `state.touched` へ積み、[`enforce_referencing_rows_in_txn`] が最後にまとめて
/// 事後検証する。引数 7 個超は連鎖 1 段分の呼び出しコンテキスト（トランザクション・
/// 対象テーブル・変更内容・再帰状態）を素直に渡した結果であり、構造体へまとめる
/// ほどの凝集性はない（呼び出しは本モジュール内の再帰 1 箇所のみ）。
#[allow(clippy::too_many_arguments)]
fn propagate_referential_actions(
    write_txn: &redb::WriteTransaction,
    table_name: &str,
    schema: &TableSchema,
    tenant_id: &str,
    change: PropagatedChange,
    pre_images: Option<&UpdatedKeyPreImages>,
    fk_mode: FkCheckMode,
    depth: u32,
    state: &mut ActionState,
) -> Result<(), TenantWriteError> {
    let referencing = crate::catalog::referencing_foreign_keys_in_txn(write_txn, table_name)?;

    // Pass 1（対象特定。codex-review 指摘・PR #1138）: このテーブルを参照する
    // 全 FK の連鎖対象を、いずれの FK のアクションもまだ適用していない子テーブル
    // の状態から確定する。同じ子列に複数の FK が作用する場合（例: 一方が
    // `ON DELETE SET NULL`・他方が `ON DELETE CASCADE`）に、先に適用した FK の
    // 書き込みが後続 FK の `collect_action_targets` の走査結果を変えてしまい、
    // 宣言順（`referencing` の並び）で最終結果が変わるのを防ぐ。行数上限
    // （`MAX_REFERENTIAL_ACTION_ROWS`）の判定もこの収集段階で行う——
    // `collect_action_targets` に残り枠を渡し、子テーブル走査中に上限超過を
    // 検出した時点で打ち切る（`scan_child_fk_rows_for_keys` ドキュメント参照）。
    // 同じ理由で、対象行の Pass 2 適用前スナップショット（`apply_referential_action`
    // が孫段への pre-image に使う値。`snapshot_child_rows_before_pass2` 参照）
    // もこの Pass 1 の時点で確定する（Cursor Bugbot 指摘・PR #1138: 同じ子行を
    // 複数の FK が対象にし、`SET NULL`／`SET DEFAULT` が `ON DELETE CASCADE` より
    // 先に適用される場合、`CASCADE` 側が「現在の行」を再読取りして pre-image を
    // 作ると `SET` 適用後の値を記録してしまい、孫段の連鎖対象特定が本来の旧キーを
    // 見失う）。
    let mut pending_actions: Vec<PendingReferentialAction<'_>> = Vec::new();
    for (child_schema, fk) in &referencing {
        let action = match &change {
            PropagatedChange::Removed => fk.on_delete(),
            PropagatedChange::ColumnsUpdated(indices) => {
                // `id` 参照は ON UPDATE で発火しない（`id` 疑似列は不変。A7）。
                if fk.references_parent_id() {
                    continue;
                }
                let names: Vec<&str> = indices
                    .iter()
                    .filter_map(|&i| schema.columns.get(i).map(|c| c.name.as_str()))
                    .collect();
                if !fk
                    .parent_columns()
                    .iter()
                    .any(|c| names.contains(&c.as_str()))
                {
                    continue;
                }
                fk.on_update()
            }
        };
        if matches!(action, ReferentialAction::NoAction) {
            continue;
        }

        let remaining_budget = MAX_REFERENTIAL_ACTION_ROWS.saturating_sub(state.rows_budget_used);
        let affected = collect_action_targets(
            write_txn,
            table_name,
            schema,
            child_schema,
            fk,
            tenant_id,
            &change,
            pre_images,
            action,
            remaining_budget,
        )?;
        if affected.is_empty() {
            continue;
        }

        let n = u32::try_from(affected.len())
            .map_err(|_| internal("referential action row count overflow"))?;
        let new_budget = state
            .rows_budget_used
            .checked_add(n)
            .ok_or_else(|| internal("referential action row budget overflow"))?;
        if new_budget > MAX_REFERENTIAL_ACTION_ROWS {
            return Err(TenantWriteError::ReferentialActionLimitExceeded);
        }
        state.rows_budget_used = new_budget;

        let original_snapshot = snapshot_child_rows_before_pass2(
            write_txn,
            child_schema,
            tenant_id,
            affected.iter().map(|(id, _)| *id),
        )?;

        pending_actions.push((child_schema, fk, action, affected, original_snapshot));
    }

    let new_depth = depth
        .checked_add(1)
        .ok_or_else(|| internal("referential action depth overflow"))?;
    if !pending_actions.is_empty() && new_depth > MAX_REFERENTIAL_ACTION_DEPTH {
        return Err(TenantWriteError::ReferentialActionLimitExceeded);
    }

    // Pass 2（適用。codex-review 指摘・PR #1138）: `referencing`（カタログ走査順＝
    // 概ね宣言順）ではなく、FK 自身の構造（参照元の子テーブル名・参照元列・
    // 参照先テーブル・参照先列）で決まる正準順に並べ替えてから適用し、
    // declaration 順に依存しない決定的な結果にする。`ForeignKeyDef::
    // shares_reference_shape` は同一テーブル内での重複宣言だけを禁止するため
    // （`referencing` は複数の異なる子テーブルにまたがりうる）、子テーブル名を
    // キーの先頭に含めて全体で一意にする（異なる子テーブルの行ストアは互いに
    // 独立に書き込むため、それら同士の適用順自体は結果に影響しないが、キーの
    // 一意性そのものは保つ）。`CASCADE`（削除）は行そのものを消すため、他
    // アクションとどちらの順で交差しても最終状態は削除に収束する
    // （`apply_referential_action` の `SET NULL`／`SET DEFAULT` 分岐は削除済み
    // 行を素通りし、`CASCADE` 側は `id` で読み直すため既に書き換えられた行も
    // 問題なく削除できる）。同じ列に `SET NULL` と `SET DEFAULT` が競合する
    // 退化ケースだけは適用順で最終値が変わり得るため、宣言順ではなく FK の
    // 構造キーで固定する（キーの大小関係が結果を決めるだけで、それ自体に
    // PostgreSQL 由来の意味はない）。
    pending_actions.sort_by(|a, b| {
        let key_of = |item: &PendingReferentialAction<'_>| {
            (
                item.0.name.clone(),
                item.1.columns().to_vec(),
                item.1.parent_table().to_string(),
                item.1.parent_columns().to_vec(),
            )
        };
        key_of(a).cmp(&key_of(b))
    });

    // 同一の子テーブルが同一の親テーブルを複数の `FOREIGN KEY` で参照する場合
    // （`referencing` に同じ `child_schema.name` が複数回現れる）の対応
    // （Issue #1076・codex-review 指摘）: `enforce_row_constraints_in_txn`
    // （対象行の**全** FK を検査する）を FK ごとに即座に呼ぶと、この時点で
    // まだアクションを適用していない同じ子テーブルの別 FK が旧値のまま検査され
    // `23503` に誤って失敗する。子テーブル名 → (スキーマ, 検証対象 id) へ
    // 蓄積し、このテーブルを参照する全 FK のアクション適用が終わってから
    // まとめて 1 回ずつ検証する（子孫段への再帰も同様に、全 FK 適用後へ
    // 遅延する）。
    let mut pending_validation: HashMap<String, (TableSchema, Vec<u64>)> = HashMap::new();
    let mut pending_recursions: Vec<(
        TableSchema,
        PropagatedChange,
        Option<UpdatedKeyPreImages>,
        u32,
    )> = Vec::new();
    for (child_schema, fk, action, affected, original_snapshot) in pending_actions {
        let is_delete = matches!(change, PropagatedChange::Removed);
        let (child_change, child_pre_images) = apply_referential_action(
            write_txn,
            child_schema,
            fk,
            action,
            &affected,
            &original_snapshot,
            tenant_id,
            is_delete,
        )?;

        crate::catalog::bump_table_generation_in_txn(write_txn, &child_schema.name)?;

        if let PropagatedChange::ColumnsUpdated(_) = &child_change {
            // 書いた値が壊れていないか（CHECK → UNIQUE → 子自身の FK 参照元側）の
            // 検証対象 id を蓄積する（`SET DEFAULT` の値が参照先に無い・UNIQUE
            // 衝突・CHECK 違反はここで検出される）。削除（`CASCADE` の ON DELETE
            // 側）は行が既に無いため対象外。実際の検証はこのテーブルを参照する
            // 全 FK のループを終えた後、子テーブルごとに 1 回だけ行う（上記
            // `pending_validation` ドキュメント参照）。
            let child_ids: Vec<u64> = affected.iter().map(|(id, _)| *id).collect();
            pending_validation
                .entry(child_schema.name.clone())
                .and_modify(|(_, ids)| ids.extend(child_ids.iter().copied()))
                .or_insert_with(|| ((*child_schema).clone(), child_ids));
        }
        state
            .touched
            .push((child_schema.name.clone(), child_schema.clone()));

        pending_recursions.push((
            (*child_schema).clone(),
            child_change,
            child_pre_images,
            new_depth,
        ));
    }

    // 蓄積した検証をここでまとめて行う（同じ子テーブルへ複数 FK が action を
    // 適用していても、全アクション適用後の最終値を 1 回だけ検査する）。
    for (child_table, (child_schema, child_ids)) in pending_validation {
        enforce_row_constraints_in_txn(
            write_txn,
            &child_table,
            &child_schema,
            tenant_id,
            &child_ids,
            fk_mode,
        )?;
    }

    // 検証後に子孫段の連鎖を辿る（親段の全 FK 適用・検証が確定した状態で
    // 再帰するため、孫段の `collect_action_targets` が中途半端な親状態を
    // 読むことはない）。
    for (child_schema, child_change, child_pre_images, new_depth) in pending_recursions {
        propagate_referential_actions(
            write_txn,
            &child_schema.name,
            &child_schema,
            tenant_id,
            child_change,
            child_pre_images.as_ref(),
            fk_mode,
            new_depth,
            state,
        )?;
    }
    Ok(())
}

/// [`propagate_referential_actions`] が 1 個の `FOREIGN KEY` について連鎖の
/// 対象となる子行を特定する（Issue #1076 A6）。戻り値は `(子行 id, 新しい
/// 参照先キー値)` の一覧: `CASCADE` の `ON UPDATE` のみ新しいキー値
/// （`fk.columns()` の位置に対応する `Value`）を積む。それ以外のアクション
/// （`ON DELETE` 全般・`SET NULL`／`SET DEFAULT`）は書き込み時に値を必要としない
/// ため `None`。
///
/// - `ON DELETE`（`change == Removed`）: 子行の参照元キー（非 NULL）が、事後状態
///   （既に削除済みの `parent_table`）に存在しなくなった行（孤立行）。
/// - `ON UPDATE`（`change == ColumnsUpdated`）: `pre_images` に記録された旧キー値
///   のうち、現在（書き込み後）の親行の新キー値と異なるものを持つ子行。
///   `pre_images` が無ければ空を返す（呼び出し元が発火不要と判定済み）。
///
/// 引数 8 個超は連鎖 1 段分の呼び出しコンテキスト（参照元・参照先スキーマ・
/// FK 宣言・変更内容・行数上限の残り枠）を素直に渡した結果であり、構造体へ
/// まとめるほどの凝集性はない（呼び出しは [`propagate_referential_actions`] の
/// 1 箇所のみ）。
#[allow(clippy::too_many_arguments)]
fn collect_action_targets(
    write_txn: &redb::WriteTransaction,
    parent_table: &str,
    parent_schema: &TableSchema,
    child_schema: &TableSchema,
    fk: &ForeignKeyDef,
    tenant_id: &str,
    change: &PropagatedChange,
    pre_images: Option<&UpdatedKeyPreImages>,
    action: ReferentialAction,
    remaining_budget: u32,
) -> Result<ActionTargets, TenantWriteError> {
    match change {
        PropagatedChange::Removed => {
            if let Some(pre) = pre_images {
                // 限定版（Issue #1076 A14・codex-review 指摘対応）: `pre_images` に
                // 積まれた「今回の文で実際に削除された行」の削除前キー値のみを
                // CASCADE 等の対象にする。下記のグローバルスキャン（フォールバック）は
                // 「現在の親に存在しないキーを持つ子行全体」を対象にしてしまうため、
                // `INITIALLY DEFERRED` の `FOREIGN KEY` が同一トランザクション内で
                // 許す一時的な合法孤立行（このステートメントより前の文が作り、以降の
                // 文で解消される予定の孤立行）まで誤って連鎖削除・連鎖更新の対象に
                // 含めてしまう。全削除呼び出し元（`tenant.rs` の各削除関数）は
                // `remove` の戻り値（削除前の物理行）から復元した旧値を積んで渡す
                // 契約（`UpdatedKeyPreImages` ドキュメント参照）。
                if pre.old_values.is_empty() {
                    return Ok(Vec::new());
                }
                // `id` 参照（`fk.references_parent_id()`）は `parent_columns()` が
                // 疑似列 `id`（`parent_schema.columns` には存在しない物理キー）を
                // 返すため、列参照 FK と同じ `position()` 検索を行うと常に失敗する
                // （codex-review・Cursor Bugbot 指摘・Issue #1076）。`parent_indices`
                // は下記の `old_key` 算出で列参照 FK（`else` 分岐）にのみ使うため、
                // `id` 参照では検索自体を行わず空のままにする。
                let parent_indices: Vec<usize> = if fk.references_parent_id() {
                    Vec::new()
                } else {
                    fk.parent_columns()
                        .iter()
                        .map(|name| {
                            parent_schema
                                .columns
                                .iter()
                                .position(|c| &c.name == name)
                                .ok_or_else(|| {
                                    internal("referenced column not found in parent schema")
                                })
                        })
                        .collect::<Result<_, _>>()?
                };
                // 列参照 FK は親テーブルを 1 回だけ走査し、存在するキーの集合を作る
                // （`ColumnsUpdated` 分岐の点照会 `read_parent_row_values` と異なり、
                // 削除された行はもう `id` で読み戻せないため、キーバイト列の集合との
                // 突合せに寄せる。`id` 参照は物理キーの点照会のままにする）。
                let present_keys: Option<HashSet<Vec<u8>>> = if fk.references_parent_id() {
                    None
                } else {
                    Some(scan_parent_key_bytes(
                        write_txn,
                        parent_table,
                        parent_schema,
                        fk,
                        tenant_id,
                    )?)
                };
                // 削除前キーのうち、現時点でもまだ失われたままのキー（`exists ==
                // false`）だけを「対象になり得るキー」として先に確定する
                // （codex-review 指摘・PR #1138: 子テーブルの走査〔次段〕を、
                // このキー集合に一致する行だけへ絞り込むための下ごしらえ。
                // 無関係な子行を一切 `scan_child_fk_rows_for_keys` の結果へ
                // 含めない）。
                let mut wanted_keys: HashSet<ChildKey> = HashSet::new();
                let mut seen_keys: HashSet<ChildKey> = HashSet::new();
                for (removed_id, old_values) in &pre.old_values {
                    let old_key = if fk.references_parent_id() {
                        Some(ChildKey::Id(*removed_id))
                    } else {
                        build_owned_key(old_values, &parent_indices, fk)?
                    };
                    let Some(old_key) = old_key else { continue };
                    // 複数行が同じキーを共有することは PK／UNIQUE 制約上あり得ないが、
                    // 同一テナント内の重複走査を避ける保険として重複キーは 1 回のみ扱う。
                    if !seen_keys.insert(old_key.clone()) {
                        continue;
                    }
                    let exists = match &old_key {
                        ChildKey::Id(id) => {
                            parent_row_exists(write_txn, parent_table, tenant_id, *id)?
                        }
                        ChildKey::Bytes(bytes) => present_keys
                            .as_ref()
                            .is_some_and(|present| present.contains(bytes)),
                    };
                    if exists {
                        // 同一トランザクション内の別の文が、削除されたのと同じキーを
                        // 持つ親行を既に再投入している。最終状態としてキーは失われて
                        // いないため対象外（`ColumnsUpdated` 分岐と同じ最終状態判定）。
                        continue;
                    }
                    wanted_keys.insert(old_key);
                }
                if wanted_keys.is_empty() {
                    return Ok(Vec::new());
                }
                let child_rows = scan_child_fk_rows_for_keys(
                    write_txn,
                    child_schema,
                    fk,
                    tenant_id,
                    Some(&wanted_keys),
                    remaining_budget,
                )?;
                let mut out = Vec::new();
                for ids in child_rows.values() {
                    for &id in ids {
                        out.push((id, None));
                    }
                }
                return Ok(out);
            }
            // フォールバック（`pre_images` を渡さない呼び出し元向け。現状の
            // `tenant.rs` 側呼び出し元はすべて上記の限定版を使うため通常は
            // 到達しない）: 現在の親に存在しないキーを持つ子行全体を対象にする、
            // より広い（保守的だが `INITIALLY DEFERRED` では過剰連鎖になり得る）走査。
            // どのキーが目的か事前に絞れないため `wanted_keys` は渡さないが、
            // 行数上限は走査中に判定する（`scan_child_fk_rows_for_keys` ドキュメント
            // 参照）。**注意**: `wanted_keys = None` の場合、上限は「孤立している
            // 子行の件数」ではなく「非 NULL キーを持つ子行の総数」で判定される
            // （孤立判定〔`present_keys` との突合せ〕は走査後に行うため）。この
            // 経路は現状到達しないため実害はないが、到達する呼び出し元を新設する
            // 場合は、非孤立行が `MAX_REFERENTIAL_ACTION_ROWS` を超えるだけで
            // 無関係に `54000` へ倒れうる点に注意すること。
            let child_rows = scan_child_fk_rows_for_keys(
                write_txn,
                child_schema,
                fk,
                tenant_id,
                None,
                remaining_budget,
            )?;
            if child_rows.is_empty() {
                return Ok(Vec::new());
            }
            // 列参照 FK は親テーブルを 1 回だけ走査し、存在するキーの集合を作る
            // （`child_rows` の distinct キーごとに親を再走査すると、自己参照の
            // 深い木で段あたり O(distinct キー数 × 親行数) に膨らむ。
            // `id` 参照は物理キーの点照会のままにする——`parent_row_exists` は
            // O(1) であり、事前に全 id を集めても走査コストを削減しない）。
            let present_keys: Option<HashSet<Vec<u8>>> =
                if child_rows.keys().any(|k| matches!(k, ChildKey::Bytes(_))) {
                    Some(scan_parent_key_bytes(
                        write_txn,
                        parent_table,
                        parent_schema,
                        fk,
                        tenant_id,
                    )?)
                } else {
                    None
                };
            let mut out = Vec::new();
            for (key, ids) in child_rows {
                let exists = match &key {
                    ChildKey::Id(id) => parent_row_exists(write_txn, parent_table, tenant_id, *id)?,
                    ChildKey::Bytes(bytes) => present_keys
                        .as_ref()
                        .is_some_and(|present| present.contains(bytes)),
                };
                if !exists {
                    for id in ids {
                        out.push((id, None));
                    }
                }
            }
            Ok(out)
        }
        PropagatedChange::ColumnsUpdated(_) => {
            let Some(pre) = pre_images else {
                return Ok(Vec::new());
            };
            if pre.old_values.is_empty() {
                return Ok(Vec::new());
            }
            let parent_indices: Vec<usize> = fk
                .parent_columns()
                .iter()
                .map(|name| {
                    parent_schema
                        .columns
                        .iter()
                        .position(|c| &c.name == name)
                        .ok_or_else(|| internal("referenced column not found in parent schema"))
                })
                .collect::<Result<_, _>>()?;
            // 親行ごとに「旧キーが新キーと異なる（＝失われた）」場合の旧キーだけを
            // 対象候補として先に確定する（codex-review 指摘・PR #1138）。親の
            // 現在値の点照会（`read_parent_row_values`）は子テーブルの走査より
            // 先に完結できるため、子スキャンをこの確定済みキー集合へ絞り込める。
            let mut wanted: HashMap<ChildKey, Option<Vec<crate::row_codec::Value>>> =
                HashMap::new();
            for (parent_id, old_values) in &pre.old_values {
                let old_key = build_owned_key(old_values, &parent_indices, fk)?;
                let Some(old_key) = old_key else { continue };
                let new_values = read_parent_row_values(
                    write_txn,
                    parent_table,
                    parent_schema,
                    tenant_id,
                    *parent_id,
                )?;
                let new_key = match &new_values {
                    Some(values) => build_owned_key(values, &parent_indices, fk)?,
                    None => None,
                };
                if new_key.as_ref() == Some(&old_key) {
                    continue;
                }
                let new_key_values: Option<Vec<crate::row_codec::Value>> =
                    if matches!(action, ReferentialAction::Cascade) {
                        match &new_values {
                            Some(values) => Some(
                                parent_indices
                                    .iter()
                                    .map(|&idx| {
                                        values.get(idx).cloned().ok_or_else(|| {
                                            internal("parent row value index out of range")
                                        })
                                    })
                                    .collect::<Result<_, _>>()?,
                            ),
                            // 親行自体が同一トランザクション内で削除された（先に
                            // ON DELETE 連鎖が走った等）。ON UPDATE CASCADE の
                            // 対象ではなくなっている（削除側の連鎖が別途処理する）。
                            None => continue,
                        }
                    } else {
                        None
                    };
                wanted.insert(old_key, new_key_values);
            }
            if wanted.is_empty() {
                return Ok(Vec::new());
            }
            let wanted_keys: HashSet<ChildKey> = wanted.keys().cloned().collect();
            let child_rows = scan_child_fk_rows_for_keys(
                write_txn,
                child_schema,
                fk,
                tenant_id,
                Some(&wanted_keys),
                remaining_budget,
            )?;
            let mut out = Vec::new();
            for (key, ids) in &child_rows {
                let new_key_values = wanted.get(key).cloned().flatten();
                for &id in ids {
                    out.push((id, new_key_values.clone()));
                }
            }
            Ok(out)
        }
    }
}

/// 親テーブル `parent_table` の `id` 疑似列参照の存在確認（`id` 参照 FK 用）。
fn parent_row_exists(
    write_txn: &redb::WriteTransaction,
    parent_table: &str,
    tenant_id: &str,
    id: u64,
) -> Result<bool, TenantWriteError> {
    let row_table_name = crate::catalog::user_rows_table_name(parent_table);
    match write_txn.open_table(crate::catalog::user_rows_table_def(&row_table_name)) {
        Ok(t) => Ok(t
            .get((tenant_id, id))
            .map_err(CatalogError::from)?
            .is_some()),
        Err(redb::TableError::TableDoesNotExist(_)) => Ok(false),
        Err(e) => Err(TenantWriteError::from(crate::catalog::map_row_table_error(
            e,
        ))),
    }
}

/// 親テーブル `parent_table` の同一テナント全行を 1 回だけ走査し、`fk` の
/// 参照先列（列参照 FK）の正準キーバイト列の集合を作る（Issue #1076 A6 の
/// ON DELETE 対象特定用）。子側の distinct キーごとに親を再走査すると、自己
/// 参照の深い木で連鎖の段あたり O(distinct キー数 × 親行数) に膨らむため
/// （codex-review P1・A04 不安全な設計〔DoS〕。`enforce_foreign_keys_in_txn` が
/// 参照元側の検査で採る「必要な組をまとめて 1 回の走査で照合する」設計と同じ
/// 考え方を、参照先側の対象特定にも適用する）、親走査は 1 回に集約する。
fn scan_parent_key_bytes(
    write_txn: &redb::WriteTransaction,
    parent_table: &str,
    parent_schema: &TableSchema,
    fk: &ForeignKeyDef,
    tenant_id: &str,
) -> Result<HashSet<Vec<u8>>, TenantWriteError> {
    let mut out: HashSet<Vec<u8>> = HashSet::new();
    let row_table_name = crate::catalog::user_rows_table_name(parent_table);
    let row_table = match write_txn.open_table(crate::catalog::user_rows_table_def(&row_table_name))
    {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(out),
        Err(e) => {
            return Err(TenantWriteError::from(crate::catalog::map_row_table_error(
                e,
            )))
        }
    };
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
            break;
        }
        let values = decode_key_columns(parent_schema, &mask, v.value())?;
        if let Some(key) = key_bytes(&key_spec, &values).map_err(internal)? {
            out.insert(key);
        }
    }
    Ok(out)
}

/// 親テーブル `parent_table` の id `id` の現在（write トランザクション内、
/// post-write）の全列値を読む（ON UPDATE CASCADE の新キー値取得用）。行が存在
/// しない（同一トランザクション内で先に削除された等）場合は `None`。
fn read_parent_row_values(
    write_txn: &redb::WriteTransaction,
    parent_table: &str,
    parent_schema: &TableSchema,
    tenant_id: &str,
    id: u64,
) -> Result<Option<Vec<crate::row_codec::Value>>, TenantWriteError> {
    let row_table_name = crate::catalog::user_rows_table_name(parent_table);
    let row_table = match write_txn.open_table(crate::catalog::user_rows_table_def(&row_table_name))
    {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => {
            return Err(TenantWriteError::from(crate::catalog::map_row_table_error(
                e,
            )))
        }
    };
    let Some(guard) = row_table.get((tenant_id, id)).map_err(CatalogError::from)? else {
        return Ok(None);
    };
    let (_dim, metadata) = crate::storage::decode_row_dim_and_metadata_borrowed(guard.value())?;
    let values = crate::row_codec::decode_scalar_columns(parent_schema, metadata)
        .map_err(|e| CatalogError::Invalid(e.to_string()))?;
    Ok(Some(values))
}

/// `values`（`parent_indices` の位置。全列 `Value` 配列からの部分抽出）から
/// 正準キーバイト列を組み立てる（[`ChildKey::Bytes`] と同じ表現。`id` 参照は
/// `fk.columns()` の対応列と同じ扱いだが `ON UPDATE` では発火しないため
/// 呼び出し元が到達させない）。いずれかの構成列が NULL なら `None`。
fn build_owned_key(
    values: &[crate::row_codec::Value],
    indices: &[usize],
    _fk: &ForeignKeyDef,
) -> Result<Option<ChildKey>, TenantWriteError> {
    let refs: Vec<Option<ScalarRef<'_>>> = indices
        .iter()
        .map(|&idx| values.get(idx).and_then(value_as_scalar_ref))
        .collect();
    let key_spec = KeySpec {
        indices: (0..indices.len()).collect(),
        null_policy: NullPolicy::Skip,
    };
    Ok(key_bytes(&key_spec, &refs)
        .map_err(internal)?
        .map(ChildKey::Bytes))
}

/// 特定した子行 `affected`（`(id, ON UPDATE CASCADE の新キー値)`）へ参照アクション
/// `action` を適用する（Issue #1076）。戻り値は子テーブルへの変更内容（さらに
/// 再帰する [`propagate_referential_actions`] への入力）と、子自身が親となる
/// 孫段の連鎖向けの pre-image。
///
/// `original_snapshot` は Pass 1（`snapshot_child_rows_before_pass2`）が確定した
/// 「このテーブルへの Pass 2 の書き込みが始まる前」の各行の全列値。孫段への
/// pre-image は必ずこのスナップショットから作る（Cursor Bugbot 指摘・PR #1138）。
/// 同じ子行を複数の FK が対象にする場合、この関数の呼び出し時点で行を再読取り
/// （`row_table.get`）した値は、正準順で先に適用された別 FK（例: `SET NULL`／
/// `SET DEFAULT`）による書き換え後の中間状態でありうる。その中間状態を
/// pre-image に使うと、`SET` で書き換わった列が孫段の連鎖対象特定
/// （`collect_action_targets` の `wanted_keys`）の旧キーとして使えなくなり
/// （`NULL`／`DEFAULT` 値は正しい参照先キーではない）、本来連鎖されるべき孫行が
/// 対象から漏れて事後検証の `NO ACTION` バックストップで `23503` になる。
/// 再読取りした「現在の行」自体は read-merge-write の書き込み対象としては
/// 引き続き正しく使う（他 FK が既に書き換えた列を上書きで消さないため）。
///
/// 引数 7 個超は 1 回の適用に必要なコンテキスト（対象スキーマ・FK 宣言・
/// 対象行・pre-image 用スナップショット）を素直に渡した結果であり、構造体へ
/// まとめるほどの凝集性はない（呼び出しは [`propagate_referential_actions`] の
/// 1 箇所のみ）。
#[allow(clippy::too_many_arguments)]
fn apply_referential_action(
    write_txn: &redb::WriteTransaction,
    child_schema: &TableSchema,
    fk: &ForeignKeyDef,
    action: ReferentialAction,
    affected: &ActionTargets,
    original_snapshot: &HashMap<u64, Vec<crate::row_codec::Value>>,
    tenant_id: &str,
    is_delete: bool,
) -> Result<(PropagatedChange, Option<UpdatedKeyPreImages>), TenantWriteError> {
    if is_delete && matches!(action, ReferentialAction::Cascade) {
        // `ON DELETE CASCADE`: 子行そのものを削除する。孫段の再帰
        // （`propagate_referential_actions`）が「今回の文で実際に削除された行」
        // だけを対象にできるよう、削除前の全列値を pre-image として記録して
        // `Some` で返す（codex-review 指摘・Issue #1076: ここで `None` を返すと
        // 孫段の `collect_action_targets` は `Removed` 分岐の限定版
        // （`pre_images` 使用）を選べず、フォールバックの全体スキャンで
        // `INITIALLY DEFERRED` の一時的な合法孤立行まで連鎖対象にしてしまう
        // ——`tenant.rs` の `remove` 呼び出し元が積む `removed_pre_images` と
        // 同じ契約をこの CASCADE 経由の削除にも適用する）。
        let row_table_name = crate::catalog::user_rows_table_name(&child_schema.name);
        let mut row_table = write_txn
            .open_table(crate::catalog::user_rows_table_def(&row_table_name))
            .map_err(crate::catalog::map_row_table_error)?;
        let mut pre_images = UpdatedKeyPreImages::new();
        for (id, _) in affected {
            let key = (tenant_id, *id);
            if row_table.get(&key).map_err(CatalogError::from)?.is_none() {
                // 同一トランザクション内で既に削除された等（多段連鎖の交差）。
                continue;
            }
            // Pass 1 で確定したスナップショットを pre-image として使う（`関数
            // ドキュメント`参照。ここで「現在の行」を再読取りすると、正準順で
            // 先に適用された別 FK の `SET NULL`／`SET DEFAULT` による書き換え後の
            // 値を記録してしまい、孫段の連鎖対象特定が本来の旧キーを見失う。
            // Cursor Bugbot 指摘・PR #1138）。存在するのにスナップショットが
            // 無いのは Pass 1／Pass 2 の対象集合不一致という内部矛盾であり、
            // fail-closed に拒否する。
            let Some(original_values) = original_snapshot.get(id) else {
                return Err(internal(
                    "referential action pre-image snapshot missing for a cascaded row",
                ));
            };
            pre_images.record(*id, original_values.clone());
            row_table.remove(key).map_err(CatalogError::from)?;
        }
        return Ok((PropagatedChange::Removed, Some(pre_images)));
    }

    // `SET NULL`／`SET DEFAULT`／`ON UPDATE CASCADE`: 子行の FK 列を書き換える
    // （read-merge-write。`tenant::merge_row_for_update` を共有する）。
    let fk_indices: Vec<usize> = fk
        .columns()
        .iter()
        .map(|name| {
            child_schema
                .columns
                .iter()
                .position(|c| &c.name == name)
                .ok_or_else(|| internal("foreign key column not found in live schema columns"))
        })
        .collect::<Result<_, _>>()?;

    let mut pre_images = UpdatedKeyPreImages::new();
    {
        let row_table_name = crate::catalog::user_rows_table_name(&child_schema.name);
        let mut row_table = write_txn
            .open_table(crate::catalog::user_rows_table_def(&row_table_name))
            .map_err(crate::catalog::map_row_table_error)?;
        for (id, new_key_values) in affected {
            let key = (tenant_id, *id);
            let Some(guard) = row_table.get(&key).map_err(CatalogError::from)? else {
                // 同一トランザクション内で既に削除された等（多段連鎖の交差）。
                continue;
            };
            let existing = crate::storage::decode_row_for_key(tenant_id, *id, guard.value())
                .map_err(TenantWriteError::Storage)?;
            // `guard`（`row_table` の不変借用）を、直後の可変借用（`insert`）と
            // 衝突しないよう明示的に drop する（`existing` は既にデータを
            // 複製済みのため、以降 `guard`／借用元バッファへは触れない）。
            drop(guard);
            // 孫段の連鎖のために、Pass 1 で確定したスナップショットを pre-image
            // として記録する（`existing`＝「現在の行」ではない。関数ドキュメント
            // 参照。`existing` は直後の read-merge-write にのみ使い、他 FK が
            // 既にこの行へ適用した書き換えを正しく引き継ぐ）。
            let Some(original_values) = original_snapshot.get(id) else {
                return Err(internal(
                    "referential action pre-image snapshot missing for an updated row",
                ));
            };
            pre_images.record(*id, original_values.clone());

            let assignments: Vec<(usize, crate::row_codec::Value)> = match action {
                ReferentialAction::SetNull => fk_indices
                    .iter()
                    .map(|&idx| (idx, crate::row_codec::Value::Null))
                    .collect(),
                ReferentialAction::SetDefault => fk_indices
                    .iter()
                    .map(|&idx| {
                        let column = child_schema.columns.get(idx).ok_or_else(|| {
                            internal("foreign key column index out of range for live schema")
                        })?;
                        let value = match &column.default {
                            Some(default) => {
                                crate::sql::parser::bind_column_default(column, default).map_err(
                                    |_| {
                                        // 宣言時検査（A8 (b)）を通過した DEFAULT が束縛
                                        // できないのは内部矛盾であり、値を黙って NULL へ
                                        // 差し替えず fail-closed に拒否する。
                                        internal(
                                            "SET DEFAULT value failed to bind to its column type",
                                        )
                                    },
                                )?
                            }
                            None => crate::row_codec::Value::Null,
                        };
                        Ok::<_, TenantWriteError>((idx, value))
                    })
                    .collect::<Result<_, _>>()?,
                ReferentialAction::Cascade => {
                    let Some(new_values) = new_key_values else {
                        return Err(internal(
                            "ON UPDATE CASCADE target is missing its new key values",
                        ));
                    };
                    fk_indices
                        .iter()
                        .zip(new_values.iter())
                        .map(|(&idx, value)| (idx, value.clone()))
                        .collect()
                }
                ReferentialAction::NoAction => {
                    return Err(internal(
                        "NO ACTION must not reach apply_referential_action",
                    ))
                }
            };

            // `visibility` は既存行のものを維持する（SET で触れない列と同じ扱い。
            // `tenant::update_row_columns_unchecked` と同じ契約）。`existing` は
            // 直後の `merge_row_for_update` へ move するため先に控える。
            let visibility = existing.visibility;
            let (embedding, metadata) =
                crate::tenant::merge_row_for_update(child_schema, existing, &assignments)
                    .map_err(TenantWriteError::Catalog)?;
            let row_input = crate::storage::RowInput {
                tenant_id,
                visibility,
                embedding: &embedding,
                metadata: &metadata,
            };
            let encoded =
                crate::storage::encode_row(&row_input).map_err(TenantWriteError::Storage)?;
            row_table
                .insert(key, encoded.as_slice())
                .map_err(CatalogError::from)?;
        }
    }
    Ok((
        PropagatedChange::ColumnsUpdated(fk_indices),
        Some(pre_images),
    ))
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

    // --- `collect_action_targets` の `Removed` 分岐: 限定版とフォールバックの
    // 違い（codex-review 指摘・PR #1138）------------------------------------------
    //
    // `INITIALLY DEFERRED` な `FOREIGN KEY` は、あるトランザクション内の先行の文が
    // 参照先行を削除した直後（後続の文でまだ解消されていない間）は、子行が一時的に
    // 合法な孤立行になり得る。`pre_images` に「今回の呼び出しが対象にした削除」
    // だけを積んで [`collect_action_targets`] に渡す限定版は、この無関係な孤立行を
    // 連鎖対象に含めてはならない。`pre_images` を渡さないフォールバック（グローバル
    // スキャン）はこの区別ができず、無関係な孤立行まで含めてしまう——これが
    // 限定版を常に選べるようにする（`tenant.rs` の pre-image 記録）ことの理由。
    // 本テストは両分岐を直接呼び分け、この違いを固定する。
    #[test]
    fn collect_action_targets_removed_branch_excludes_unrelated_pre_existing_orphan() {
        let (storage, _guard) = tmp_storage("constraint-collect-targets-removed");
        let parent_schema = TableSchema::new(
            "parents",
            vec![ColumnDef::new(
                "name",
                crate::catalog::ColumnType::Text,
                true,
            )],
        );
        let child_schema = TableSchema::new(
            "children",
            vec![ColumnDef::new(
                "parent_id",
                crate::catalog::ColumnType::BigInt,
                true,
            )],
        );
        storage
            .create_table(&parent_schema)
            .expect("create parents");
        storage
            .create_table(&child_schema)
            .expect("create children");

        // `id` 参照 FK（`REFERENCES parents` の参照先列省略と同じ表現。D2）。
        let fk = ForeignKeyDef::new(
            vec!["parent_id".to_string()],
            "parents".to_string(),
            vec![crate::catalog::FOREIGN_KEY_PARENT_ID_COLUMN.to_string()],
            ReferentialAction::Cascade,
            ReferentialAction::NoAction,
        );

        let write_txn = storage.begin_write_txn().expect("begin write");
        // 現存する親行（id=1）。今回の削除とは無関係で、その子行は絶対に触れない。
        put_raw_row(
            &write_txn,
            &parent_schema,
            "tenant-a",
            1,
            &[Value::Text("keep".to_string())],
        );
        // id=9 の親行は既に存在しない——`INITIALLY DEFERRED` の下で先行の文が
        // 削除済みだが、この文より後の文で再投入される予定の一時的な合法孤立行を
        // 子に持つ、と仮定する。今回の呼び出しの `pre_images` には積まない
        // （＝この文が削除した行ではない）。
        put_raw_row(
            &write_txn,
            &child_schema,
            "tenant-a",
            100,
            &[Value::BigInt(9)],
        );
        // id=5 が「今回の文が実際に削除した親行」。その子行だけが連鎖対象になる。
        put_raw_row(
            &write_txn,
            &child_schema,
            "tenant-a",
            101,
            &[Value::BigInt(5)],
        );
        // 現存する親（id=1）を参照する子行。絶対に対象外。
        put_raw_row(
            &write_txn,
            &child_schema,
            "tenant-a",
            102,
            &[Value::BigInt(1)],
        );

        let mut pre_images = UpdatedKeyPreImages::new();
        pre_images.record(5, vec![Value::Text("removed".to_string())]);

        let limited = collect_action_targets(
            &write_txn,
            "parents",
            &parent_schema,
            &child_schema,
            &fk,
            "tenant-a",
            &PropagatedChange::Removed,
            Some(&pre_images),
            ReferentialAction::Cascade,
            MAX_REFERENTIAL_ACTION_ROWS,
        )
        .expect("limited scan must succeed");
        let limited_ids: HashSet<u64> = limited.into_iter().map(|(id, _)| id).collect();
        assert_eq!(
            limited_ids,
            HashSet::from([101]),
            "限定版は今回削除された親（id=5）の子行だけを対象にし、\
             無関係な既存孤立行（id=100・親 id=9）も現存する親の子行（id=102）も含めない"
        );

        // フォールバック（`pre_images` 無し）は「現在の親に存在しないキーを持つ
        // 子行全体」を対象にするため、無関係な孤立行（id=100）まで含んでしまう
        // ——これが限定版を優先しなければならない理由そのものを固定する。
        let fallback = collect_action_targets(
            &write_txn,
            "parents",
            &parent_schema,
            &child_schema,
            &fk,
            "tenant-a",
            &PropagatedChange::Removed,
            None,
            ReferentialAction::Cascade,
            MAX_REFERENTIAL_ACTION_ROWS,
        )
        .expect("fallback scan must succeed");
        let fallback_ids: HashSet<u64> = fallback.into_iter().map(|(id, _)| id).collect();
        assert_eq!(
            fallback_ids,
            HashSet::from([100, 101]),
            "フォールバックは無関係な既存孤立行（id=100）も連鎖対象に含めてしまう"
        );

        write_txn.abort().expect("abort");
    }

    /// [`collect_action_targets_removed_branch_excludes_unrelated_pre_existing_orphan`]
    /// は `collect_action_targets` を直接呼び分けて分岐の違いを固定するが、
    /// 実際の連鎖経路（`enforce_referencing_rows_in_txn` → `propagate_referential_actions`
    /// → `apply_referential_action` の CASCADE 子行削除 → 孫段の
    /// `collect_action_targets`）が実際に限定版を選べているかまでは検証しない。
    /// 本テストは親→子→孫の 2 段連鎖を実際の入口から発火させ、孫テーブルの
    /// 無関係な既存孤立行（`INITIALLY DEFERRED` が許す一時的な合法孤立行を想定。
    /// 孫の FK を `DeferrableInitiallyDeferred` で宣言し `FkCheckMode::ImmediateOnly`
    /// を渡すことで事後検証からも除外する）が、親削除の連鎖に巻き込まれず生き残る
    /// ことを固定する（codex-review 指摘・PR #1138。`apply_referential_action` の
    /// CASCADE 子行削除が pre-image を `None` で返していた退行では、孫段が
    /// フォールバックの全走査に必ず落ち、この無関係な孤立行まで削除してしまう）。
    #[test]
    fn cascade_through_two_levels_does_not_touch_unrelated_pre_existing_grandchild_orphan() {
        let (storage, _guard) = tmp_storage("constraint-cascade-chain-orphan");
        let parent_schema = TableSchema::new(
            "parents",
            vec![ColumnDef::new(
                "name",
                crate::catalog::ColumnType::Text,
                true,
            )],
        );
        let child_schema = TableSchema::new(
            "children",
            vec![ColumnDef::new(
                "parent_id",
                crate::catalog::ColumnType::BigInt,
                true,
            )],
        )
        .with_foreign_keys(vec![ForeignKeyDef::new(
            vec!["parent_id".to_string()],
            "parents".to_string(),
            vec![crate::catalog::FOREIGN_KEY_PARENT_ID_COLUMN.to_string()],
            ReferentialAction::Cascade,
            ReferentialAction::NoAction,
        )]);
        let grandchild_schema = TableSchema::new(
            "grandchildren",
            vec![ColumnDef::new(
                "child_id",
                crate::catalog::ColumnType::BigInt,
                true,
            )],
        )
        .with_foreign_keys(vec![ForeignKeyDef::new(
            vec!["child_id".to_string()],
            "children".to_string(),
            vec![crate::catalog::FOREIGN_KEY_PARENT_ID_COLUMN.to_string()],
            ReferentialAction::Cascade,
            ReferentialAction::NoAction,
        )
        .with_options(
            ForeignKeyMatch::Simple,
            crate::catalog::ForeignKeyDeferrability::DeferrableInitiallyDeferred,
        )]);
        storage
            .create_table(&parent_schema)
            .expect("create parents");
        storage
            .create_table(&child_schema)
            .expect("create children");
        storage
            .create_table(&grandchild_schema)
            .expect("create grandchildren");

        let write_txn = storage.begin_write_txn().expect("begin write");
        put_raw_row(
            &write_txn,
            &parent_schema,
            "tenant-a",
            1,
            &[Value::Text("p1".to_string())],
        );
        put_raw_row(
            &write_txn,
            &child_schema,
            "tenant-a",
            10,
            &[Value::BigInt(1)],
        );
        // 削除対象の子（id=10）にぶら下がる孫。連鎖で一緒に削除されるべき。
        put_raw_row(
            &write_txn,
            &grandchild_schema,
            "tenant-a",
            100,
            &[Value::BigInt(10)],
        );
        // 無関係な既存孤立行: 参照先の子（id=99）はそもそも存在しない
        // （`INITIALLY DEFERRED` が許す、別の文が作った一時的な合法孤立行を想定）。
        // 今回の親削除の連鎖には一切関係がなく、生き残らなければならない。
        put_raw_row(
            &write_txn,
            &grandchild_schema,
            "tenant-a",
            101,
            &[Value::BigInt(99)],
        );

        // `id=1` の親行を「今回の文が削除した」ことを模す（`tenant.rs` の削除
        // 経路と同じ契約: 削除前の全列値を `pre_images` に積んでから物理行を消す）。
        {
            let mut table = write_txn
                .open_table(crate::catalog::user_rows_table_def(
                    &crate::catalog::user_rows_table_name("parents"),
                ))
                .expect("open parents row table");
            table.remove(("tenant-a", 1u64)).expect("remove parent row");
        }
        let mut pre_images = UpdatedKeyPreImages::new();
        pre_images.record(1, vec![Value::Text("p1".to_string())]);

        enforce_referencing_rows_in_txn(
            &write_txn,
            "parents",
            &parent_schema,
            "tenant-a",
            ReferencedRowsChange::Removed,
            Some(&pre_images),
            FkCheckMode::ImmediateOnly,
        )
        .expect("cascade through two levels must succeed");

        let children_table = write_txn
            .open_table(crate::catalog::user_rows_table_def(
                &crate::catalog::user_rows_table_name("children"),
            ))
            .expect("open children row table");
        assert!(
            children_table
                .get(("tenant-a", 10u64))
                .expect("read children")
                .is_none(),
            "親削除に連動して子行（id=10）は連鎖削除される"
        );
        let grandchildren_table = write_txn
            .open_table(crate::catalog::user_rows_table_def(
                &crate::catalog::user_rows_table_name("grandchildren"),
            ))
            .expect("open grandchildren row table");
        assert!(
            grandchildren_table
                .get(("tenant-a", 100u64))
                .expect("read grandchildren")
                .is_none(),
            "削除された子行（id=10）にぶら下がる孫行（id=100）は連鎖削除される"
        );
        assert!(
            grandchildren_table
                .get(("tenant-a", 101u64))
                .expect("read grandchildren")
                .is_some(),
            "今回の連鎖と無関係な既存孤立行（id=101）はフォールバック走査に \
             巻き込まれず生き残らなければならない"
        );

        drop(children_table);
        drop(grandchildren_table);
        write_txn.abort().expect("abort");
    }

    // --- `scan_child_fk_rows_for_keys` の行数上限判定（codex-review 指摘・
    // PR #1138）------------------------------------------------------------------
    //
    // 修正前は子テーブルの全非 NULL 行を無条件に `HashMap` へ保持してから
    // 呼び出し元（`propagate_referential_actions`）が `affected.len()` で上限
    // （`MAX_REFERENTIAL_ACTION_ROWS`）を判定していた。この場合、削除前キーと
    // 無関係な行を含め、一致した行を丸ごとメモリに確保してから拒否するため、
    // 上限を大きく超える一致件数があるとメモリ確保コストが上限による抑制の
    // 意味を失う。本テストは (1) 一致件数が残り枠を超えると走査を打ち切って
    // `54000` 相当のエラーを返すこと、(2) 無関係なキーの行は一致件数にも
    // 結果にも一切現れないこと、を固定する。
    #[test]
    fn scan_child_fk_rows_for_keys_rejects_once_matches_exceed_remaining_budget() {
        let (storage, _guard) = tmp_storage("constraint-scan-child-fk-budget");
        let child_schema = TableSchema::new(
            "children",
            vec![ColumnDef::new(
                "parent_id",
                crate::catalog::ColumnType::BigInt,
                true,
            )],
        );
        storage
            .create_table(&child_schema)
            .expect("create children");
        let fk = ForeignKeyDef::new(
            vec!["parent_id".to_string()],
            "parents".to_string(),
            vec![crate::catalog::FOREIGN_KEY_PARENT_ID_COLUMN.to_string()],
            ReferentialAction::Cascade,
            ReferentialAction::NoAction,
        );

        let write_txn = storage.begin_write_txn().expect("begin write");
        // wanted_keys に一致する行（parent_id=5）を 3 件。
        for id in [10u64, 11, 12] {
            put_raw_row(
                &write_txn,
                &child_schema,
                "tenant-a",
                id,
                &[Value::BigInt(5)],
            );
        }
        // wanted_keys と無関係な行（parent_id=6）を多数（一致件数にもエラー
        // 判定にも影響しないことを確認する）。
        for id in [20u64, 21, 22, 23, 24] {
            put_raw_row(
                &write_txn,
                &child_schema,
                "tenant-a",
                id,
                &[Value::BigInt(6)],
            );
        }
        let wanted_keys: HashSet<ChildKey> = HashSet::from([ChildKey::Id(5)]);

        // 残り枠が一致件数（3）未満だと打ち切って `ReferentialActionLimitExceeded`。
        let err = scan_child_fk_rows_for_keys(
            &write_txn,
            &child_schema,
            &fk,
            "tenant-a",
            Some(&wanted_keys),
            2,
        )
        .expect_err("matches exceeding the remaining budget must be rejected");
        assert!(matches!(
            err,
            TenantWriteError::ReferentialActionLimitExceeded
        ));

        // 残り枠が一致件数以上なら成功し、無関係な行（parent_id=6）は結果に
        // 一切含まれない。
        let ok = scan_child_fk_rows_for_keys(
            &write_txn,
            &child_schema,
            &fk,
            "tenant-a",
            Some(&wanted_keys),
            3,
        )
        .expect("matches within the remaining budget must succeed");
        let mut matched_ids: Vec<u64> = ok
            .get(&ChildKey::Id(5))
            .expect("key 5 must be present")
            .clone();
        matched_ids.sort_unstable();
        assert_eq!(matched_ids, vec![10, 11, 12]);
        assert_eq!(
            ok.len(),
            1,
            "無関係なキー（parent_id=6）は一致件数にも結果にも現れない"
        );

        write_txn.abort().expect("abort");
    }

    /// `UpdatedKeyPreImages::record` は同じ `id` への 2 回目以降の呼び出しを
    /// 無視し、最初に記録した値（文実行前の真のスナップショット）を保持し続ける
    /// （Cursor Bugbot 指摘・PR #1138）。呼び出し元（`tenant.rs`）は現状いずれも
    /// 1 文につき同じ id を 1 回しか記録しない契約を守っている（複数行 `UPSERT`
    /// の `UNIQUE` 対象は「同一文内で同じ対象キーを持つ複数の `VALUES` 行」を
    /// `tenant::upsert_typed_rows` が事前に拒否するため、同じ既存行が同一文内で
    /// 2 回書き換わる経路は現状到達しない）が、本関数自体の契約として単体でも
    /// 固定しておく。
    #[test]
    fn updated_key_pre_images_record_keeps_the_first_value_for_the_same_id() {
        let mut pre_images = UpdatedKeyPreImages::new();
        pre_images.record(1, vec![Value::Text("original".to_string())]);
        // 2 回目の呼び出し（中間状態を模す）は無視される。
        pre_images.record(1, vec![Value::Text("intermediate".to_string())]);
        assert_eq!(
            pre_images.old_values.get(&1),
            Some(&vec![Value::Text("original".to_string())]),
            "2 回目以降の record は最初の値を上書きしてはならない"
        );
    }
}
