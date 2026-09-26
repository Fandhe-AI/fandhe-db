//! 実行計画の複数テーブル対応基盤（SQL-28・RLS-10、TASK-212、Issue #924）その 3:
//! テーブル単位の RLS 可視スナップショットと、それを複数テーブル横断で束ねる
//! [`MultiRelationSnapshot`]。
//!
//! [`crate::sql::visible_cache::VisibleSnapshot`]（Issue #478）は行キーの `id`
//! だけを保持するが、物理キーは `(tenant_id, id)`（TABLE-12）であり、1 つの
//! `PolicyContext` が複数テナントの `Public` 行を見得るため `id` 単独では行を
//! 一意に識別できない。本モジュールの [`RelationSnapshot`] は `(tenant_id, id)`
//! を保持し、複数テーブルの結果を突き合わせる後続タスク（Issue #925 以降の
//! JOIN 実装）が行を取り違えないようにする。
//!
//! 構築は本モジュール自身が担う（呼び出し元の既存走査へ相乗りする
//! `VisibleSnapshotBuilder` とは異なり、複数テーブル対応の新規エントリ
//! ポイントのため）。走査順序は [`crate::sql::aggregate`] の走査規律
//! （ヘッダのみデコード → 可視性判定 → tenant 整合検査 → dim・metadata の
//! 構造検証 → スカラー列の構造検証）と同一にする。

use crate::catalog::{self, TableSchema};
use crate::policy::PolicyContext;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::generation_key::{ApproxHeapBytes, GenerationKeyedCache, TableGenerationKey};
use crate::sql::relation::{TableRef, MAX_TABLE_REFS};
use crate::storage::{self, StorageError};
use redb::ReadableTable;
use std::sync::Arc;

/// 走査・破損検出のエラーはストレージ詳細を含めない固定文言の `Internal`
/// （`XX000`）にする（`.claude/rules/security.md` P0）。
fn storage_internal(e: impl Into<StorageError>) -> SqlSurfaceError {
    let _ = e.into();
    SqlSurfaceError::Internal {
        detail: "relation snapshot scan failed".to_string(),
    }
}

/// [`catalog::CatalogError`]（`table_generation_in_txn`・
/// [`crate::sql::generation_key::TableGenerationKey::capture`] 由来）を
/// [`storage_internal`] と同じ固定文言の `Internal` へ写像する。
fn catalog_internal(e: impl Into<catalog::CatalogError>) -> SqlSurfaceError {
    let _ = e.into();
    SqlSurfaceError::Internal {
        detail: "relation snapshot scan failed".to_string(),
    }
}

/// 1 テーブル分の RLS 可視スナップショット。物理走査順に並んだ `(tenant_id, id)`
/// の列と、構築時の `ctx`・テーブル世代を保持する。
pub struct RelationSnapshot {
    visible_rows: Vec<(String, u64)>,
    built_table_generation: u64,
}

impl RelationSnapshot {
    /// 可視行の `(tenant_id, id)` を物理走査順に返す。
    pub fn visible_rows(&self) -> &[(String, u64)] {
        &self.visible_rows
    }

    pub fn built_table_generation(&self) -> u64 {
        self.built_table_generation
    }

    /// テーブル本体を走査し、可視行だけを記録した [`RelationSnapshot`] を構築する
    /// （`sql::aggregate` の走査規律と同じ順序: ヘッダデコード →
    /// `PolicyContext::is_visible` → `verify_row_key_tenant` →
    /// `decode_row_dim_and_metadata_borrowed` →
    /// `row_codec::validate_scalar_columns`）。行数上限
    /// [`crate::arena::MAX_ARENA_ROWS`] を超過した場合はクエリ自体を
    /// `54000`（[`SqlSurfaceError::payload_too_large`]）で拒否する（キャッシュ
    /// 登録見送りではなく拒否側に倒す）。
    fn build(
        read_txn: &redb::ReadTransaction,
        table: &TableSchema,
        ctx: &PolicyContext,
        built_table_generation: u64,
    ) -> Result<Self, SqlSurfaceError> {
        let row_table_name = catalog::user_rows_table_name(&table.name);
        let opened = match read_txn.open_table(catalog::user_rows_table_def(&row_table_name)) {
            Ok(t) => Some(t),
            // 対象テーブルの行が 1 件も書き込まれていない（行テーブル自体が
            // 未作成）は空集合として扱う（既存の検索 SELECT 実行経路と同じ契約）。
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(e) => return Err(catalog_internal(catalog::map_row_table_error(e))),
        };

        let mut visible_rows: Vec<(String, u64)> = Vec::new();
        if let Some(opened) = opened {
            for entry in opened.iter().map_err(storage_internal)? {
                let (k, v) = entry.map_err(storage_internal)?;
                let (key_tenant, id) = k.value();
                let buf = v.value();

                // RLS 段（無条件・デコード前）: ヘッダのみ読み、不可視行は
                // embedding・metadata を一切デコードしない（security.md P0
                // 「テナント境界」・Issue #137 の既存契約踏襲）。
                let (tenant_id, visibility, _offset) =
                    storage::decode_row_header(buf).map_err(storage_internal)?;
                if !ctx.is_visible(tenant_id, visibility) {
                    continue;
                }

                // 可視行・常に: 複合キー側の `key_tenant` とヘッダ側 `tenant_id`
                // の整合を検査する（TABLE-12）。
                storage::verify_row_key_tenant(key_tenant, tenant_id).map_err(storage_internal)?;

                // PR #369 の契約: `Fast` 相当のスナップショットであっても dim・
                // metadata の構造検証（破損検知）は必ず通す。
                let (dim, metadata) =
                    storage::decode_row_dim_and_metadata_borrowed(buf).map_err(storage_internal)?;
                if dim != 0 {
                    if let Some(expected) = table.vector_dim() {
                        if dim != expected {
                            return Err(storage_internal(StorageError::Codec(format!(
                                "embedding dim mismatch for row {id}: expected {expected}, got {dim}"
                            ))));
                        }
                    }
                }

                // レビュー指摘対応（PR #1104 Cursor Bugbot Medium）: dim/metadata の
                // 枠検証（`decode_row_dim_and_metadata_borrowed`）は presence・長さ・
                // バッファ境界のみを見る。列単位のスカラーペイロード（`TEXT` の
                // UTF-8 妥当性・宣言長上限等）はここでは未検証のままで、既存の
                // `sql::aggregate` の `DecodeTier::Fast` 経路（PR #369）が必須の
                // fail-closed 破損検知として課している検証と揃える必要がある。
                // `crate::row_codec::validate_scalar_columns`（検証専用・`Vec`
                // 確保なし）で同じ構造検証を通す。
                crate::row_codec::validate_scalar_columns(table, metadata)?;

                if visible_rows.len() >= crate::arena::MAX_ARENA_ROWS {
                    return Err(SqlSurfaceError::payload_too_large(
                        "relation snapshot visible row count exceeds limit",
                    ));
                }
                visible_rows.push((tenant_id.to_string(), id));
            }
        }

        Ok(Self {
            visible_rows,
            built_table_generation,
        })
    }
}

impl ApproxHeapBytes for RelationSnapshot {
    /// `GenerationKeyedCache` の容量判定用の概算バイト量（レビュー指摘対応・
    /// PR #1104 codex P2）。旧実装は各要素のテナント ID 文字数と `u64` の 8 byte
    /// しか数えておらず、`String` 自体のスタック表現（ptr/len/cap、64bit で
    /// `size_of::<String>() == 24`）・`Vec` 側の確保済み容量（`len()` ではなく
    /// `capacity()`。amortized 成長で未使用の余剰容量を含む）を無視していた。
    /// 短いテナント ID の行が大量にあると実使用量が `total_bytes_limit` を
    /// 超えても保持され続けてしまう（`rls.rs::PrefilterSnapshot::approx_heap_bytes`
    /// と同じ「`capacity()` 基準・保守的に上振れ」方針に揃える）。
    ///
    /// - `visible_rows` バッキング配列: `capacity() * size_of::<(String, u64)>()`
    ///   （`(String, u64)` タプルのスタック部分。`String` の管理領域込み）
    /// - 各テナント ID 文字列のヒープ確保量: `String::capacity()`
    ///   （`len()` ではなく確保済みバイト数を使う。過小評価しない）
    fn approx_heap_bytes(&self) -> usize {
        let vec_backing_bytes = self
            .visible_rows
            .capacity()
            .saturating_mul(std::mem::size_of::<(String, u64)>());
        let string_heap_bytes = self
            .visible_rows
            .iter()
            .map(|(tenant, _)| tenant.capacity())
            .fold(0usize, |acc, n| acc.saturating_add(n));
        vec_backing_bytes.saturating_add(string_heap_bytes)
    }
}

/// 参照スロット順の [`RelationSnapshot`] 束と、全体の [`TableGenerationKey`]。
/// 自己結合では同じ `Arc<RelationSnapshot>` を共有するが、スロット（`Vec` の
/// 添字）は参照ごとに別に保つ（`sql::relation::BindingScope` の `relation` 添字と
/// 対応する）。
pub struct MultiRelationSnapshot {
    snapshots: Vec<Arc<RelationSnapshot>>,
    key: TableGenerationKey,
}

impl MultiRelationSnapshot {
    /// スロット順のスナップショット参照。
    pub fn snapshots(&self) -> &[Arc<RelationSnapshot>] {
        &self.snapshots
    }

    pub fn generation_key(&self) -> &TableGenerationKey {
        &self.key
    }
}

/// [`RelationSnapshot`] のテーブル単位世代整合キャッシュ（[`GenerationKeyedCache`]
/// の具体化）。`EngineCore` への保持・結線は Issue #925 以降。
pub type RelationSnapshotCache = GenerationKeyedCache<RelationSnapshot>;

/// 複数テーブルの RLS 可視スナップショットを、同一 `read_txn`・同一 `ctx` から
/// **独立に**解決する（SQL-28・RLS-10）。
///
/// 1. 参照数の上限（[`MAX_TABLE_REFS`]）を検証する
/// 2. 行を走査する前に、`read_txn` から全テーブルの世代を
///    [`TableGenerationKey::capture`] で確定させる（キャッシュヒット判定・
///    整合性判定の唯一の世代源泉）
/// 3. 各 `relations` 要素について `table_ref.table()`（世代キーの元）と
///    `schema.name`（実走査対象。`catalog::user_rows_table_name` の解決元）の
///    一致を検証する（不一致は `Internal` で fail-closed。誤った組が渡されると
///    別テーブルの内容が誤ったテーブルの世代キーでキャッシュされ RLS 可視集合が
///    汚染されるため）
/// 4. テーブルごとにキャッシュ（[`RelationSnapshotCache`]）を単一テーブル鍵で
///    引く。ミスしたらそのテーブルだけ走査して構築し `insert` する
/// 5. 自己結合（同じテーブル名を複数回参照）は同じ `Arc` を共有する
///
/// `ctx` はサーバー側で導出済みのものだけを受け取り、クライアント指定の値を
/// 受け取る引数は設けない（呼び出し元の責務）。
///
/// `cache`: `Some((storage, cache))` のときだけキャッシュを利用する。
/// `storage` はキャッシュの `insert`/`lookup` が世代を再読取するためだけに
/// 使う（既存の [`crate::sql::arena_cache::SqlArenaCache`] 等と同じ理由）。
/// キャッシュを使わない呼び出し（`None`）は `read_txn` だけで完結し、
/// `execute_aggregate`（`sql::aggregate`。`Storage` を要求しない公開 API の
/// 既存流儀）と同じ形になる。
pub fn resolve_relation_snapshots(
    read_txn: &redb::ReadTransaction,
    ctx: &PolicyContext,
    relations: &[(TableRef, &TableSchema)],
    cache: Option<(&crate::storage::Storage, &RelationSnapshotCache)>,
) -> Result<MultiRelationSnapshot, SqlSurfaceError> {
    if relations.is_empty() {
        return Err(SqlSurfaceError::payload_too_large(
            "at least one table reference is required",
        ));
    }
    if relations.len() > MAX_TABLE_REFS {
        return Err(SqlSurfaceError::payload_too_large(format!(
            "too many table references: {} (max {MAX_TABLE_REFS})",
            relations.len()
        )));
    }

    let table_names: Vec<&str> = relations.iter().map(|(r, _)| r.table()).collect();
    let key = TableGenerationKey::capture(read_txn, ctx.clone(), &table_names)
        .map_err(catalog_internal)?;

    // テーブル名 → 解決済みスナップショットのキャッシュ（自己結合での Arc 共有用）。
    let mut resolved: Vec<(String, Arc<RelationSnapshot>)> = Vec::new();
    let mut snapshots = Vec::with_capacity(relations.len());
    for (table_ref, schema) in relations {
        // 呼び出し元が `(TableRef, &TableSchema)` を取り違えて渡すと、世代キー
        // （`table_ref.table()` 由来）と実走査対象（`schema.name` 由来の行テーブル）
        // が食い違ったまま構築・キャッシュ登録されてしまう（誤ったテーブルの
        // `RelationSnapshot` が別テーブルの世代キーでキャッシュされ、以降の書き込みが
        // 無効化しない stale キャッシュとなり RLS 可視集合を汚染する）。fail-closed に
        // プログラム的検証を行う（`.claude/rules/security.md` テナント境界・fail-closed）。
        // 直下の重複排除ショートカットより **前** に検証すること: 自己結合等で同一
        // テーブル名を複数回参照する要素があると、ショートカットが先にあった場合
        // 2 件目以降がこの検証を経ずに `Ok` になってしまう（レビュー指摘）。
        if table_ref.table() != schema.name {
            return Err(SqlSurfaceError::Internal {
                detail: "relation snapshot table reference mismatch".to_string(),
            });
        }

        if let Some((_, existing)) = resolved.iter().find(|(name, _)| name == table_ref.table()) {
            snapshots.push(Arc::clone(existing));
            continue;
        }

        let single_key = TableGenerationKey::capture(read_txn, ctx.clone(), &[table_ref.table()])
            .map_err(catalog_internal)?;
        let built_generation = crate::catalog::table_generation_in_txn(read_txn, table_ref.table())
            .map_err(catalog_internal)?;

        let snapshot = if let Some((storage, cache)) = cache {
            if let Some(hit) = cache.lookup(storage, read_txn, ctx, &single_key) {
                hit
            } else {
                let built = RelationSnapshot::build(read_txn, schema, ctx, built_generation)?;
                cache.insert(storage, single_key, built)
            }
        } else {
            Arc::new(RelationSnapshot::build(
                read_txn,
                schema,
                ctx,
                built_generation,
            )?)
        };

        resolved.push((table_ref.table().to_string(), Arc::clone(&snapshot)));
        snapshots.push(snapshot);
    }

    Ok(MultiRelationSnapshot { snapshots, key })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnDef, ColumnType, TableSchema};
    use crate::recovery::required_op_id::OperationId;
    use crate::storage::{RowInput, Storage, Visibility};
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
    use redb::ReadableDatabase;

    fn ctx_with(tenant: &str, visibilities: Vec<Visibility>) -> PolicyContext {
        PolicyContext::with_visibilities(tenant, visibilities).expect("valid ctx")
    }

    fn create_table(storage: &Storage, name: &str) -> TableSchema {
        let schema = TableSchema::new(name, vec![ColumnDef::new("path", ColumnType::Text, false)]);
        storage.create_table(&schema).expect("create table");
        schema
    }

    /// テスト専用の行挿入ヘルパー（`sql::hnsw_hybrid` の `seed_row` と同じ流儀。
    /// `tenant::insert_row` が要求する `operation_id` はテストごとに一意な文字列で
    /// 満たす）。`table` は `create_table` と同じ単一 `TEXT NOT NULL` 列（`path`）
    /// のスキーマを前提とし、`row_codec::encode_scalar_columns` でその列に整合する
    /// metadata を組み立てる（PR #1104 レビュー対応で `RelationSnapshot::build` が
    /// `validate_scalar_columns` を通すようになったため、空 metadata は非
    /// nullable 列を持つスキーマと整合しない）。
    fn seed_row(storage: &Storage, table: &str, id: u64, tenant: &str, visibility: Visibility) {
        let c = PolicyContext::new(tenant).expect("valid tenant");
        let op_id = OperationId::parse(&format!("relation-snapshot-test-{tenant}-{table}-{id}"))
            .expect("valid operation_id");
        let schema = TableSchema::new(table, vec![ColumnDef::new("path", ColumnType::Text, false)]);
        let metadata = crate::row_codec::encode_scalar_columns(
            &schema,
            &[crate::row_codec::Value::Text(String::new())],
        )
        .expect("encode scalar columns");
        crate::tenant::insert_row(
            storage,
            table,
            &c,
            id,
            &RowInput {
                tenant_id: tenant,
                visibility,
                embedding: &[],
                metadata: &metadata,
            },
            &op_id,
        )
        .expect("seed row");
    }

    #[test]
    fn independent_visibility_per_table() {
        let path = unique_db_path("relation-snapshot-independent");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema_a = create_table(&storage, "a");
        let schema_b = create_table(&storage, "b");

        seed_row(&storage, "a", 1, "tenant-a", Visibility::Public);
        seed_row(&storage, "a", 2, "tenant-b", Visibility::Private);
        seed_row(&storage, "b", 1, "tenant-a", Visibility::Public);

        let public_ctx = ctx_with("tenant-a", vec![Visibility::Public]);
        let read_txn = storage.db().begin_read().unwrap();
        let relations = vec![
            (TableRef::new("a"), &schema_a),
            (TableRef::new("b"), &schema_b),
        ];
        let result =
            resolve_relation_snapshots(&read_txn, &public_ctx, &relations, None).expect("resolve");
        assert_eq!(result.snapshots().len(), 2);
        assert_eq!(
            result.snapshots()[0].visible_rows(),
            &[("tenant-a".to_string(), 1)]
        );
        assert_eq!(
            result.snapshots()[1].visible_rows(),
            &[("tenant-a".to_string(), 1)]
        );
    }

    #[test]
    fn self_join_shares_arc_with_two_slots() {
        let path = unique_db_path("relation-snapshot-self-join");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = create_table(&storage, "a");
        let c = ctx_with("tenant-a", vec![Visibility::Public]);

        let cache = RelationSnapshotCache::new(crate::arena::MAX_ARENA_TOTAL_BYTES);
        let read_txn = storage.db().begin_read().unwrap();
        let relations = vec![
            (TableRef::with_alias("a", "x"), &schema),
            (TableRef::with_alias("a", "y"), &schema),
        ];
        let result =
            resolve_relation_snapshots(&read_txn, &c, &relations, Some((&storage, &cache)))
                .expect("resolve");
        assert_eq!(result.snapshots().len(), 2);
        assert!(Arc::ptr_eq(&result.snapshots()[0], &result.snapshots()[1]));
    }

    #[test]
    fn empty_row_table_is_empty_snapshot() {
        let path = unique_db_path("relation-snapshot-empty");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = create_table(&storage, "a");
        let c = ctx_with("tenant-a", vec![Visibility::Public]);
        let read_txn = storage.db().begin_read().unwrap();
        let relations = vec![(TableRef::new("a"), &schema)];
        let result = resolve_relation_snapshots(&read_txn, &c, &relations, None).expect("resolve");
        assert!(result.snapshots()[0].visible_rows().is_empty());
    }

    #[test]
    fn cache_hit_and_miss_yield_same_result() {
        let path = unique_db_path("relation-snapshot-cache");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = create_table(&storage, "a");
        seed_row(&storage, "a", 1, "tenant-a", Visibility::Public);
        let c = ctx_with("tenant-a", vec![Visibility::Public]);
        let cache = RelationSnapshotCache::new(crate::arena::MAX_ARENA_TOTAL_BYTES);
        let relations = vec![(TableRef::new("a"), &schema)];

        let read_txn1 = storage.db().begin_read().unwrap();
        let miss = resolve_relation_snapshots(&read_txn1, &c, &relations, Some((&storage, &cache)))
            .expect("resolve miss");

        let read_txn2 = storage.db().begin_read().unwrap();
        let hit = resolve_relation_snapshots(&read_txn2, &c, &relations, Some((&storage, &cache)))
            .expect("resolve hit");

        assert_eq!(
            miss.snapshots()[0].visible_rows(),
            hit.snapshots()[0].visible_rows()
        );
    }

    #[test]
    fn rejects_too_many_relations() {
        let path = unique_db_path("relation-snapshot-too-many");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = create_table(&storage, "a");
        let c = ctx_with("tenant-a", vec![Visibility::Public]);
        let read_txn = storage.db().begin_read().unwrap();
        let relations: Vec<(TableRef, &TableSchema)> = (0..(MAX_TABLE_REFS + 1))
            .map(|_| (TableRef::new("a"), &schema))
            .collect();
        assert!(resolve_relation_snapshots(&read_txn, &c, &relations, None).is_err());
    }

    #[test]
    fn rejects_empty_relations() {
        let path = unique_db_path("relation-snapshot-empty-relations");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let c = ctx_with("tenant-a", vec![Visibility::Public]);
        let read_txn = storage.db().begin_read().unwrap();
        let relations: Vec<(TableRef, &TableSchema)> = Vec::new();
        assert!(resolve_relation_snapshots(&read_txn, &c, &relations, None).is_err());
    }

    /// 取り違え検証は先頭要素でも検出する（レビュー指摘の回帰: 検証がショート
    /// カットより後に置かれていた旧実装でも先頭要素では偶然検出できていたが、
    /// 併せて固定する）。
    #[test]
    fn rejects_mismatched_schema_in_first_slot() {
        let path = unique_db_path("relation-snapshot-mismatch-first");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema_a = create_table(&storage, "a");
        let _schema_b = create_table(&storage, "b");
        let c = ctx_with("tenant-a", vec![Visibility::Public]);
        let read_txn = storage.db().begin_read().unwrap();
        // `TableRef::new("a")` に対して意図的に `b` のスキーマを渡す（呼び出し元の
        // 取り違えを模す）。
        let relations = vec![(TableRef::new("a"), &_schema_b)];
        let err = resolve_relation_snapshots(&read_txn, &c, &relations, None)
            .err()
            .expect("must reject");
        assert!(
            matches!(err, SqlSurfaceError::Internal { .. }),
            "expected Internal, got {err:?}"
        );
        let _ = schema_a;
    }

    /// レビュー指摘の再現ケース: 同一テーブル名を複数回参照する要素（自己結合の
    /// 別名違い）で、2 件目が重複排除ショートカットにより取り違え検証を素通り
    /// して `Ok` になっていた回帰を固定する。`relations` は
    /// `[(TableRef::with_alias("a","x"), &schema_a), (TableRef::with_alias("a","y"), &schema_b)]`
    /// で、2 件目の `table_ref.table() == "a"` に対して `schema_b`（`name == "b"`）が
    /// 渡されているため取り違えとして拒否されるべき。
    #[test]
    fn rejects_mismatched_schema_after_duplicate_table_name() {
        let path = unique_db_path("relation-snapshot-mismatch-dup");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema_a = create_table(&storage, "a");
        let schema_b = create_table(&storage, "b");
        let c = ctx_with("tenant-a", vec![Visibility::Public]);
        let read_txn = storage.db().begin_read().unwrap();
        let relations = vec![
            (TableRef::with_alias("a", "x"), &schema_a),
            (TableRef::with_alias("a", "y"), &schema_b),
        ];
        let err = resolve_relation_snapshots(&read_txn, &c, &relations, None)
            .err()
            .expect("must reject");
        assert!(
            matches!(err, SqlSurfaceError::Internal { .. }),
            "expected Internal, got {err:?}"
        );
    }

    /// `sql::aggregate` の `write_row_raw` と同じ流儀（`tenant::insert_row` の
    /// 検証を経由せず行テーブルへ直接バイト列を書き込む。破損行を作る検証専用
    /// ヘルパー）。
    fn write_row_raw(storage: &Storage, table_name: &str, tenant_id: &str, id: u64, buf: &[u8]) {
        let write_txn = storage.db().begin_write().expect("begin_write");
        {
            let mut table = write_txn
                .open_table(catalog::user_rows_table_def(
                    &catalog::user_rows_table_name(table_name),
                ))
                .expect("open row table");
            table.insert((tenant_id, id), buf).expect("insert row");
        }
        storage::bump_generation_and_commit(write_txn).expect("commit");
    }

    /// レビュー指摘の回帰（PR #1104 Cursor Bugbot Medium）: `RelationSnapshot::build`
    /// は dim・metadata の枠検証は通すが、列単位のスカラーペイロード検証
    /// （`row_codec::validate_scalar_columns`）を経ていなかったため、枠は正しいが
    /// `TEXT` 列に不正 UTF-8 を含む破損行が可視として記録され `resolve` が成功
    /// してしまっていた。`sql::aggregate` の同種回帰テスト
    /// （`count_star_still_fails_closed_on_corrupted_metadata_with_no_scalar_reference`）
    /// と同じ破損手順（妥当な `Text` 値の末尾バイトを不正 UTF-8 の単独継続バイトへ
    /// 書き換え）で固定する。
    #[test]
    fn build_fails_closed_on_row_with_corrupted_scalar_metadata() {
        let path = unique_db_path("relation-snapshot-corrupt-metadata");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = TableSchema::new(
            "docs",
            vec![crate::catalog::ColumnDef::new(
                "body",
                ColumnType::Text,
                false,
            )],
        );
        storage.create_table(&schema).expect("create table");

        let mut metadata = crate::row_codec::encode_scalar_columns(
            &schema,
            &[crate::row_codec::Value::Text("hello".to_string())],
        )
        .expect("encode scalar columns");
        // presence タグ・長さフィールドはそのまま、値バイト列の末尾のみ不正
        // UTF-8（単独継続バイト）へ書き換える。
        let corrupt_offset = metadata.len() - 1;
        metadata[corrupt_offset] = 0x80;

        let buf = storage::encode_row(&RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Public,
            embedding: &[],
            metadata: &metadata,
        })
        .expect("encode row");
        write_row_raw(&storage, "docs", "tenant-a", 1, &buf);

        let ctx = ctx_with("tenant-a", vec![Visibility::Public]);
        let read_txn = storage.db().begin_read().expect("begin_read");
        let err = RelationSnapshot::build(&read_txn, &schema, &ctx, 0)
            .err()
            .expect("must fail closed on corrupted scalar metadata");
        assert!(
            matches!(err, SqlSurfaceError::Internal { .. }),
            "expected Internal, got {err:?}"
        );
    }

    /// レビュー指摘の回帰（PR #1104 codex P2）: `approx_heap_bytes` がテナント ID の
    /// 文字数と `u64` の 8 byte しか数えず、`String`／`Vec` の管理領域・確保容量を
    /// 無視していたため、短いテナント ID の行が大量にあると `GenerationKeyedCache`
    /// の `total_bytes_limit` が実効しない問題を固定する。旧実装の見積り方式
    /// （`len()` 基準）より必ず大きい値になり、かつ `(String, u64)` タプル 1 つ分の
    /// サイズ以上という保守的な下限を満たすことを確認する。
    #[test]
    fn approx_heap_bytes_accounts_for_string_and_vec_capacity_overhead() {
        let path = unique_db_path("relation-snapshot-heap-bytes");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = create_table(&storage, "a");
        const ROW_COUNT: u64 = 50;
        for id in 0..ROW_COUNT {
            seed_row(&storage, "a", id, "t", Visibility::Public);
        }
        let ctx = ctx_with("t", vec![Visibility::Public]);
        let read_txn = storage.db().begin_read().expect("begin_read");
        let snapshot = RelationSnapshot::build(&read_txn, &schema, &ctx, 0).expect("build");
        assert_eq!(snapshot.visible_rows().len(), ROW_COUNT as usize);

        let naive_estimate: usize = snapshot
            .visible_rows()
            .iter()
            .map(|(tenant, _)| tenant.len().saturating_add(std::mem::size_of::<u64>()))
            .fold(0usize, |acc, n| acc.saturating_add(n));
        let actual = snapshot.approx_heap_bytes();

        assert!(
            actual > naive_estimate,
            "approx_heap_bytes must account for String/Vec management overhead \
             (management overhead), not just raw tenant id byte lengths: \
             naive={naive_estimate} actual={actual}"
        );
        // 保守的な下限: 少なくとも各行 1 つ分の `(String, u64)` タプルサイズは
        // 計上されている。
        assert!(
            actual >= (ROW_COUNT as usize).saturating_mul(std::mem::size_of::<(String, u64)>()),
            "approx_heap_bytes must be at least rows * size_of::<(String, u64)>(): actual={actual}"
        );
    }
}
