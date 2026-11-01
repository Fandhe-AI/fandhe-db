//! `sql::row_policy` の単体テスト（RLS-13・Issue #2276）。コマンド別の層の選択、解決の入口（未有効化は
//! `None`・有効化のみは全行拒否・DISABLE で `None`・他テナントの状態は引けない）、固定文言の拒否を確認する。
//! 行ごとの評価（合成規則の全行列）は `tests/table_rls/rls13_policy_using.rs` が独立オラクルで固定する。

use super::*;
use crate::catalog::policy::{PolicyDef, PolicyExprs};
use crate::catalog::row_security::RowSecurityAction;
use crate::catalog::{ColumnDef, ColumnType};
use crate::sql::allowlist::SqlSurfaceError;
use crate::storage::Storage;
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
use redb::ReadableDatabase;

fn schema() -> TableSchema {
    TableSchema::new(
        "t",
        vec![
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("n", ColumnType::Integer, false),
        ],
    )
}

fn open(label: &str) -> (Storage, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    (storage, guard)
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::new(tenant).expect("valid tenant")
}

fn add_policy(storage: &Storage, tenant: &str, name: &str, permissive: bool, using: &str) {
    let def = PolicyDef {
        id: 0,
        name: name.to_string(),
        permissive,
        command: PolicyCommand::All,
        roles: Vec::new(),
        using_sql: None,
        check_sql: None,
        referenced_columns: Vec::new(),
    };
    storage
        .create_policy::<crate::catalog::CatalogError, _>(tenant, "t", &def, |_| {
            Ok(PolicyExprs {
                using_sql: Some(using.to_string()),
                check_sql: None,
                referenced_columns: vec!["lang".to_string()],
            })
        })
        .expect("create policy");
}

#[test]
fn layers_add_select_only_when_the_statement_reads_existing_rows() {
    assert_eq!(PolicyCmd::Select.layers(), vec![PolicyCommand::Select]);
    assert_eq!(
        PolicyCmd::Update {
            reads_existing: false
        }
        .layers(),
        vec![PolicyCommand::Update]
    );
    assert_eq!(
        PolicyCmd::Update {
            reads_existing: true
        }
        .layers(),
        vec![PolicyCommand::Update, PolicyCommand::Select]
    );
    assert_eq!(
        PolicyCmd::Delete {
            reads_existing: true
        }
        .layers(),
        vec![PolicyCommand::Delete, PolicyCommand::Select]
    );
}

#[test]
fn unenabled_table_resolves_to_none_and_is_not_rejected() {
    let (storage, _g) = open("row-policy-unit-none");
    let txn = storage.db().begin_read().expect("begin_read");
    let c = ctx("alice");
    assert!(
        resolve_row_policy(&txn, &c, &schema(), "t", PolicyCmd::Select)
            .expect("resolve")
            .is_none()
    );
    reject_if_policies_apply(&txn, &c, "t").expect("not applied");
}

#[test]
fn enabled_without_policies_denies_all_and_rejects_unsupported_paths() {
    let (storage, _g) = open("row-policy-unit-deny");
    storage
        .alter_table_row_security("alice", "t", RowSecurityAction::Enable)
        .expect("enable");
    let txn = storage.db().begin_read().expect("begin_read");
    let c = ctx("alice");
    let policy = resolve_row_policy(&txn, &c, &schema(), "t", PolicyCmd::Select)
        .expect("resolve")
        .expect("applies");
    assert!(policy.is_deny_all());
    match reject_if_policies_apply(&txn, &c, "t") {
        Err(SqlSurfaceError::FeatureNotSupported { detail }) => {
            assert!(detail.contains("row-level security"));
            assert!(!detail.contains("alice"));
        }
        other => panic!("expected FeatureNotSupported, got {other:?}"),
    }
    // 他テナントの状態は引けない（alice の有効化は bob に効かない）。
    let bob = ctx("bob");
    assert!(
        resolve_row_policy(&txn, &bob, &schema(), "t", PolicyCmd::Select)
            .expect("resolve")
            .is_none()
    );
}

#[test]
fn permissive_and_restrictive_compose_and_disable_returns_none() {
    let (storage, _g) = open("row-policy-unit-compose");
    storage
        .alter_table_row_security("alice", "t", RowSecurityAction::Enable)
        .expect("enable");
    add_policy(&storage, "alice", "p1", true, "lang = 'ja'");
    add_policy(&storage, "alice", "r1", false, "n >= 3");
    {
        let txn = storage.db().begin_read().expect("begin_read");
        let policy = resolve_row_policy(&txn, &ctx("alice"), &schema(), "t", PolicyCmd::Select)
            .expect("resolve")
            .expect("applies");
        assert!(!policy.is_deny_all());
        let mut mask = vec![false; 2];
        let needs = policy.mark_needs(&mut mask);
        assert!(needs.scalar);
        assert!(!needs.embedding);
        assert_eq!(mask, vec![true, true]);
    }
    storage
        .alter_table_row_security("alice", "t", RowSecurityAction::Disable)
        .expect("disable");
    let txn = storage.db().begin_read().expect("begin_read");
    assert!(
        resolve_row_policy(&txn, &ctx("alice"), &schema(), "t", PolicyCmd::Select)
            .expect("resolve")
            .is_none()
    );
}

#[test]
fn corrupt_stored_expression_fails_closed_with_a_fixed_message() {
    let (storage, _g) = open("row-policy-unit-corrupt");
    storage
        .alter_table_row_security("alice", "t", RowSecurityAction::Enable)
        .expect("enable");
    // 束縛できない保存テキスト（存在しない列）。DDL を経由せず保存して再束縛の失敗を作る。
    add_policy(&storage, "alice", "bad", true, "nosuchcol = 'x'");
    let txn = storage.db().begin_read().expect("begin_read");
    match resolve_row_policy(&txn, &ctx("alice"), &schema(), "t", PolicyCmd::Select) {
        Err(SqlSurfaceError::Internal { detail }) => {
            assert_eq!(detail, MSG_UNRESOLVABLE);
            assert!(!detail.contains("nosuchcol"));
        }
        other => panic!("expected a fixed Internal error, got {other:?}"),
    }
}
