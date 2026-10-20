//! 移行 `id` の IDENTITY 割り当て（Issue #1698）のテスト。契約は TABLE-23 (8)・TABLE-24 (1)〜(4)・
//! RECOVER-1 のポインタを正とする。テナント別の初期値・冪等性・クラッシュ耐性（commit 前の破棄と
//! 開き直し）・前提不一致の副作用ゼロ・DROP／RENAME 連動を固定する。

use super::*;
use crate::catalog::sequence::{AllocError, SEQUENCES_TABLE, SEQUENCE_STATE_TABLE};
use crate::catalog::{ColumnDef, RowIdentity};
use crate::row_codec::{encode_scalar_columns, Value};
use crate::storage::{encode_row, RowInput, Visibility};
use redb::ReadableDatabase;

fn tmp(label: &str) -> (Storage, crate::test_util::temp_db::CleanupGuard) {
    let path = crate::test_util::temp_db::unique_db_path(label);
    let guard = crate::test_util::temp_db::CleanupGuard(path.clone());
    (Storage::open(&path).expect("open"), guard)
}

/// `id BIGINT PRIMARY KEY` を宣言した採番方式の表（移行後の形の一例）。
fn migrated_shape(name: &str) -> TableSchema {
    TableSchema::new(name, vec![ColumnDef::new("id", ColumnType::BigInt, false)])
        .with_primary_key(vec!["id".to_string()])
        .with_row_identity(RowIdentity::Allocated)
}

/// 採番方式の表へ `(tenant, rowid, id 値)` の行を直接書く。
fn put_row(storage: &Storage, schema: &TableSchema, tenant: &str, rowid: u64, id: i64) {
    let metadata = encode_scalar_columns(schema, &[Value::BigInt(id)]).expect("scalars");
    let raw = encode_row(&RowInput {
        tenant_id: tenant,
        visibility: Visibility::Public,
        embedding: &[],
        metadata: &metadata,
    })
    .expect("row");
    let txn = storage.begin_write_txn().expect("txn");
    {
        let name = user_rows_table_name(&schema.name);
        let mut t = txn.open_table(user_rows_table_def(&name)).expect("open");
        t.insert((tenant, rowid), raw.as_slice()).expect("insert");
    }
    txn.commit_raw_for_test().expect("commit");
}

/// 次の払い出し値（新しい write txn で払い出して commit する）。
fn next(storage: &Storage, sequence: &str, tenant: &str) -> std::result::Result<i64, AllocError> {
    let def = storage
        .sequence_definition_full(sequence)
        .expect("def")
        .expect("exists");
    let txn = storage.begin_write_txn().expect("txn");
    let v = storage.sequence_allocator().next_value_in_txn(
        &txn,
        tenant,
        def.id,
        &def.params,
        def.restart_value,
    )?;
    txn.commit_raw_for_test().expect("commit");
    Ok(v)
}

fn state_rows(storage: &Storage) -> Vec<(String, u64, Vec<u8>)> {
    let rt = storage.db().begin_read().expect("read");
    let Ok(t) = rt.open_table(SEQUENCE_STATE_TABLE) else {
        return Vec::new();
    };
    t.iter()
        .expect("iter")
        .map(|e| {
            let (k, v) = e.expect("entry");
            let (tenant, id) = k.value();
            (tenant.to_string(), id, v.value().to_vec())
        })
        .collect()
}

fn definition_bytes(storage: &Storage) -> Vec<(String, Vec<u8>)> {
    let rt = storage.db().begin_read().expect("read");
    let Ok(t) = rt.open_table(SEQUENCES_TABLE) else {
        return Vec::new();
    };
    t.iter()
        .expect("iter")
        .map(|e| {
            let (k, v) = e.expect("entry");
            (k.value().1.to_string(), v.value().to_vec())
        })
        .collect()
}

fn seeded(
    label: &str,
) -> (
    Storage,
    crate::test_util::temp_db::CleanupGuard,
    TableSchema,
) {
    let (s, g) = tmp(label);
    let schema = migrated_shape("items");
    s.create_table(&schema).expect("create");
    let schema = s.get_table_schema("items").expect("schema");
    // テナント A: 3・10・7、B: 5。C は行なし。
    put_row(&s, &schema, "tenant-a", 1, 3);
    put_row(&s, &schema, "tenant-a", 2, 10);
    put_row(&s, &schema, "tenant-a", 3, 7);
    put_row(&s, &schema, "tenant-b", 1, 5);
    (s, g, schema)
}

#[test]
fn assigns_sequence_owner_default_and_per_tenant_start() {
    let (s, _g, _) = seeded("mig-id-basic");
    let out = s.assign_migrated_id_identity("items").expect("assign");
    assert_eq!(
        out,
        MigratedIdIdentityOutcome::Assigned {
            sequence: "items_id_seq".to_string()
        }
    );
    let def = s
        .sequence_definition_full("items_id_seq")
        .expect("def")
        .expect("exists");
    assert_eq!(def.params, migrated_sequence_params());
    assert_eq!(
        s.sequence_owner("items_id_seq").expect("owner"),
        Some(SequenceOwner {
            table: "items".to_string(),
            column: "id".to_string()
        })
    );
    let schema = s.get_table_schema("items").expect("schema");
    let id_col = schema.columns.iter().find(|c| c.name == "id").expect("id");
    assert_eq!(
        id_col.default_expr,
        Some(ColumnDefault::Identity {
            sequence: "items_id_seq".to_string(),
            kind: IdentityKind::ByDefault
        })
    );
    // テナントごとに既存の最大 id の次から始まる。行のないテナントは 1。
    assert_eq!(next(&s, "items_id_seq", "tenant-a").expect("next"), 11);
    assert_eq!(next(&s, "items_id_seq", "tenant-b").expect("next"), 6);
    assert_eq!(next(&s, "items_id_seq", "tenant-c").expect("next"), 1);
    assert_eq!(next(&s, "items_id_seq", "tenant-a").expect("next"), 12);
}

#[test]
fn rerun_is_idempotent_and_writes_nothing() {
    let (s, _g, _) = seeded("mig-id-idem");
    s.assign_migrated_id_identity("items").expect("assign");
    let defs = definition_bytes(&s);
    let states = state_rows(&s);
    let out = s.assign_migrated_id_identity("items").expect("rerun");
    assert_eq!(
        out,
        MigratedIdIdentityOutcome::AlreadyAssigned {
            sequence: "items_id_seq".to_string()
        }
    );
    assert_eq!(definition_bytes(&s), defs);
    assert_eq!(state_rows(&s), states);
}

#[test]
fn uncommitted_assignment_leaves_nothing_and_rerun_converges() {
    let (s, _g, _) = seeded("mig-id-crash");
    {
        // commit 前の破棄（クラッシュの模擬）。
        let txn = s.begin_write_txn().expect("txn");
        assign_migrated_id_identity_in_txn(&txn, "items").expect("assign");
        txn.abort().expect("abort");
    }
    assert!(definition_bytes(&s).is_empty());
    assert!(state_rows(&s).is_empty());
    let schema = s.get_table_schema("items").expect("schema");
    assert!(schema
        .columns
        .iter()
        .find(|c| c.name == "id")
        .expect("id")
        .default_expr
        .is_none());
    let out = s.assign_migrated_id_identity("items").expect("rerun");
    assert!(matches!(out, MigratedIdIdentityOutcome::Assigned { .. }));
}

#[test]
fn reopen_continues_after_existing_max_id_without_collision() {
    let path = crate::test_util::temp_db::unique_db_path("mig-id-reopen");
    let _guard = crate::test_util::temp_db::CleanupGuard(path.clone());
    {
        let s = Storage::open(&path).expect("open");
        let schema = migrated_shape("items");
        s.create_table(&schema).expect("create");
        let schema = s.get_table_schema("items").expect("schema");
        put_row(&s, &schema, "tenant-a", 1, 42);
        s.assign_migrated_id_identity("items").expect("assign");
    }
    // 開き直し（新しい SequenceAllocator）でも既存の最大 id の次から続く。
    let s = Storage::open(&path).expect("reopen");
    assert_eq!(next(&s, "items_id_seq", "tenant-a").expect("next"), 43);
    assert_eq!(next(&s, "items_id_seq", "tenant-a").expect("next"), 44);
}

#[test]
fn max_id_at_bigint_limit_stops_with_limit_reached() {
    let (s, _g) = tmp("mig-id-limit");
    let schema = migrated_shape("items");
    s.create_table(&schema).expect("create");
    let schema = s.get_table_schema("items").expect("schema");
    put_row(&s, &schema, "tenant-a", 1, i64::MAX);
    s.assign_migrated_id_identity("items").expect("assign");
    assert!(matches!(
        next(&s, "items_id_seq", "tenant-a"),
        Err(AllocError::LimitReached { ascending: true })
    ));
    // 他テナントの上限到達は自テナントに影響しない。
    assert_eq!(next(&s, "items_id_seq", "tenant-b").expect("next"), 1);
}

#[test]
fn non_positive_ids_start_from_one() {
    let (s, _g) = tmp("mig-id-neg");
    let schema = migrated_shape("items");
    s.create_table(&schema).expect("create");
    let schema = s.get_table_schema("items").expect("schema");
    put_row(&s, &schema, "tenant-a", 1, -5);
    put_row(&s, &schema, "tenant-a", 2, 0);
    s.assign_migrated_id_identity("items").expect("assign");
    assert_eq!(next(&s, "items_id_seq", "tenant-a").expect("next"), 1);
}

#[test]
fn legacy_physical_key_route_saturates_u64_above_i64_max() {
    let (s, _g) = tmp("mig-id-legacy");
    // 従来方式の表（物理キー = id）。行の中身は経路 (a) では読まない。
    s.create_table(&TableSchema::new(
        "legacy",
        vec![ColumnDef::new("v", ColumnType::Text, true)],
    ))
    .expect("create");
    let big = u64::try_from(i64::MAX).expect("fits") + 10;
    let txn = s.begin_write_txn().expect("txn");
    {
        let name = user_rows_table_name("legacy");
        let mut t = txn.open_table(user_rows_table_def(&name)).expect("open");
        for (tenant, id) in [("a", 3u64), ("a", 9), ("b", 4), ("c", big), ("c", 1)] {
            t.insert((tenant, id), [0u8].as_slice()).expect("insert");
        }
    }
    let schema = get_table_schema_in_txn_impl(&*txn, "legacy", true).expect("schema");
    let maxima = per_tenant_max_id_in_txn(&txn, &schema).expect("max");
    assert_eq!(
        maxima,
        vec![
            ("a".to_string(), 9),
            ("b".to_string(), 4),
            ("c".to_string(), i64::MAX)
        ]
    );
    txn.abort().expect("abort");
    // 従来方式の表（id 列なし）には割り当てられない。
    assert!(matches!(
        s.assign_migrated_id_identity("legacy"),
        Err(CatalogError::Invalid(_))
    ));
}

#[test]
fn name_collision_gets_numeric_suffix() {
    let (s, _g) = tmp("mig-id-name");
    s.create_table(&migrated_shape("items")).expect("create");
    s.create_sequence("items_id_seq", &migrated_sequence_params())
        .expect("preexisting");
    let out = s.assign_migrated_id_identity("items").expect("assign");
    assert_eq!(
        out,
        MigratedIdIdentityOutcome::Assigned {
            sequence: "items_id_seq1".to_string()
        }
    );
}

#[test]
fn rejected_preconditions_leave_no_side_effects() {
    let (s, _g) = tmp("mig-id-reject");
    // id 列なし。
    s.create_table(&TableSchema::new(
        "no_id",
        vec![ColumnDef::new("v", ColumnType::Text, true)],
    ))
    .expect("create");
    // 型が BIGINT でない。
    s.create_table(
        &TableSchema::new(
            "int_id",
            vec![ColumnDef::new("id", ColumnType::Integer, false)],
        )
        .with_primary_key(vec!["id".to_string()])
        .with_row_identity(RowIdentity::Allocated),
    )
    .expect("create");
    // PK が id でない。
    s.create_table(
        &TableSchema::new(
            "other_pk",
            vec![
                ColumnDef::new("id", ColumnType::BigInt, false),
                ColumnDef::new("k", ColumnType::BigInt, false),
            ],
        )
        .with_primary_key(vec!["k".to_string()])
        .with_row_identity(RowIdentity::Allocated),
    )
    .expect("create");
    // 別の DEFAULT を持つ。
    let mut with_default = migrated_shape("has_default");
    for c in with_default.columns.iter_mut() {
        c.default_expr = Some(ColumnDefault::Number("7".to_string()));
        c.fill_value = c.default_expr.clone();
    }
    s.create_table(&with_default).expect("create");

    for table in ["no_id", "int_id", "other_pk", "has_default"] {
        assert!(
            matches!(
                s.assign_migrated_id_identity(table),
                Err(CatalogError::Invalid(_))
            ),
            "{table}"
        );
    }
    assert!(definition_bytes(&s).is_empty());
    assert!(state_rows(&s).is_empty());
    assert!(matches!(
        s.assign_migrated_id_identity("missing"),
        Err(CatalogError::TableNotFound(_))
    ));
}

#[test]
fn inconsistent_owner_is_rejected_not_repaired() {
    let (s, _g, _) = seeded("mig-id-owner");
    s.assign_migrated_id_identity("items").expect("assign");
    // 所有者を外して食い違いを作る。
    let txn = s.begin_write_txn().expect("txn");
    set_sequence_owner_in_txn(&txn, "items_id_seq", None).expect("unown");
    txn.commit_raw_for_test().expect("commit");
    assert!(matches!(
        s.assign_migrated_id_identity("items"),
        Err(CatalogError::Invalid(_))
    ));
}

#[test]
fn ddl_interplay_drop_sequence_blocked_rename_follows_drop_table_cleans() {
    let (s, _g, _) = seeded("mig-id-ddl");
    s.assign_migrated_id_identity("items").expect("assign");
    // 依存する列があるので単独の DROP SEQUENCE は拒否される。
    assert!(matches!(
        s.drop_sequence("items_id_seq"),
        Err(CatalogError::DependentObjectsStillExist(_))
    ));
    // RENAME TO で所有者が追従し、SEQUENCE 名は変わらない。
    s.alter_table_rename("items", "things").expect("rename");
    assert_eq!(
        s.sequence_owner("items_id_seq").expect("owner"),
        Some(SequenceOwner {
            table: "things".to_string(),
            column: "id".to_string()
        })
    );
    // DROP TABLE で所有 SEQUENCE の定義と状態行も消える。
    assert_eq!(next(&s, "items_id_seq", "tenant-a").expect("next"), 11);
    s.drop_table("things").expect("drop");
    assert!(definition_bytes(&s).is_empty());
    assert!(state_rows(&s).is_empty());
}

#[test]
fn state_rows_are_keyed_by_tenant() {
    let (s, _g, _) = seeded("mig-id-keys");
    s.assign_migrated_id_identity("items").expect("assign");
    let tenants: Vec<String> = state_rows(&s).into_iter().map(|r| r.0).collect();
    assert_eq!(
        tenants,
        vec!["tenant-a".to_string(), "tenant-b".to_string()]
    );
}

#[test]
fn temp_scoped_storage_is_rejected() {
    // 一時表用のインメモリ Storage は永続 DB の SEQUENCE を解決できないため、移行の対象外。
    let s = Storage::open_with_backend(redb::backends::InMemoryBackend::new(), 1 << 20)
        .expect("open temp");
    assert!(s.is_temp_scoped());
    s.create_table(&migrated_shape("items")).expect("create");
    assert!(matches!(
        s.assign_migrated_id_identity("items"),
        Err(CatalogError::Invalid(_))
    ));
    assert!(definition_bytes(&s).is_empty());
}

/// #2200 の移行を実際に通した表（疑似列 `id`・列なし）を作り、各テナントへ物理キーで行を書く。
fn migrated_real(
    label: &str,
    tables: &[(&str, Option<&str>)],
) -> (Storage, crate::test_util::temp_db::CleanupGuard) {
    let (s, g) = tmp(label);
    for (name, pk) in tables {
        let schema = match pk {
            Some(pk) => {
                TableSchema::new(*name, vec![ColumnDef::new(*pk, ColumnType::BigInt, false)])
                    .with_primary_key(vec![(*pk).to_string()])
            }
            None => TableSchema::new(*name, vec![ColumnDef::new("k", ColumnType::BigInt, true)]),
        };
        s.create_table(&schema).expect("create");
        let txn = s.begin_write_txn().expect("txn");
        {
            let rows = user_rows_table_name(name);
            let mut t = txn.open_table(user_rows_table_def(&rows)).expect("open");
            for (tenant, id) in [("tenant-a", 3u64), ("tenant-a", 10), ("tenant-b", 5)] {
                t.insert((tenant, id), [0u8].as_slice()).expect("insert");
            }
        }
        txn.commit_raw_for_test().expect("commit");
    }
    let n = crate::catalog::legacy_id_migration::migrate_legacy_ids(&s).expect("migrate");
    assert_eq!(n, tables.len());
    (s, g)
}

#[test]
fn real_migrated_tables_get_owner_only_identity_for_both_roles() {
    let (s, _g) = migrated_real("mig-id-real", &[("plain", None), ("keyed", Some("code"))]);
    for table in ["plain", "keyed"] {
        let schema = s.get_table_schema(table).expect("schema");
        assert_eq!(schema.row_identity(), RowIdentity::MigratedId);
        let out = s.assign_migrated_id_identity(table).expect("assign");
        let seq = format!("{table}_id_seq");
        assert_eq!(
            out,
            MigratedIdIdentityOutcome::Assigned {
                sequence: seq.clone()
            }
        );
        // 列は増えず、所有者 `(table, id)` だけで結び付く。
        let after = s.get_table_schema(table).expect("schema");
        assert_eq!(after.columns.len(), 1);
        assert_eq!(
            s.sequence_owner(&seq).expect("owner"),
            Some(SequenceOwner {
                table: table.to_string(),
                column: "id".to_string()
            })
        );
        assert_eq!(next(&s, &seq, "tenant-a").expect("next"), 11);
        assert_eq!(next(&s, &seq, "tenant-b").expect("next"), 6);
        assert_eq!(next(&s, &seq, "tenant-c").expect("next"), 1);
        // 再実行は何も書かず収束する。
        let before = (definition_bytes(&s), state_rows(&s));
        assert_eq!(
            s.assign_migrated_id_identity(table).expect("rerun"),
            MigratedIdIdentityOutcome::AlreadyAssigned { sequence: seq }
        );
        assert_eq!((definition_bytes(&s), state_rows(&s)), before);
    }
    // DROP TABLE で所有 SEQUENCE も消える。
    s.drop_table("plain").expect("drop");
    s.drop_table("keyed").expect("drop");
    assert!(definition_bytes(&s).is_empty());
}
