//! 述語つき `UPDATE ... WHERE` の許可リスト・束縛（SQL-19、TASK-192・
//! Issue #869）が engine クレート外から到達可能な公開 API であることを固定する
//! 結合テスト。
//!
//! `sql_insert_explain_public_api.rs`（Issue #730）と同じ「クレート外の公開 API
//! だけを経由して production の挙動を固定する」流儀を踏襲する。加えて、本
//! Issue の設計判断の核である「述語つき `UPDATE` の `WHERE` が `SELECT` の
//! `WHERE`（[`bind_scan`]）と完全に同一の述語表現へ束縛される」ことを
//! 機械的に検証する（計画 §5.3）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::recovery::required_op_id::LedgerMode;
use engine::sql::allowlist::{validate_sql, validate_update_form};
use engine::sql::parser::{bind_scan, bind_update_form, BoundUpdateForm};
use engine::sql::udf_call::UdfRegistry;
use std::collections::HashSet;

struct FakeCatalog {
    tables: HashSet<&'static str>,
}

impl engine::sql::allowlist::TableLookup for FakeCatalog {
    fn table_exists(&self, name: &str) -> Result<bool, engine::sql::allowlist::SqlSurfaceError> {
        Ok(self.tables.contains(name))
    }
}

fn catalog() -> FakeCatalog {
    FakeCatalog {
        tables: ["documents"].into_iter().collect(),
    }
}

fn schema() -> TableSchema {
    TableSchema::new(
        "documents",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(3), false),
            ColumnDef::new("body", ColumnType::Text, false),
            ColumnDef::new("lang", ColumnType::Text, true),
            ColumnDef::new("path", ColumnType::Text, true),
        ],
    )
}

/// クレート外から `validate_update_form`・`bind_update_form` の両方に到達できる
/// ことの固定（Issue #869 の公開 API 到達性）。
#[test]
fn public_api_is_reachable_from_outside_the_engine_crate() {
    let lookup = catalog();
    let validated = validate_update_form(
        "UPDATE documents SET body = 'x' WHERE lang = 'ja' USING OPERATION_ID 'op-0001'",
        &lookup,
        LedgerMode::Ledgered,
    )
    .expect("predicate-form UPDATE must pass the allowlist");

    let bound = bind_update_form(&validated, &schema(), &UdfRegistry::default())
        .expect("predicate-form UPDATE must bind");

    match bound {
        BoundUpdateForm::Predicate(p) => assert_eq!(p.table(), "documents"),
        BoundUpdateForm::Single(_) => panic!("expected Predicate variant"),
    }
}

/// 同一 `WHERE` 述語（等価・前方一致・式比較の `AND` 結合）を (a) `SELECT`
/// 経由（`validate_sql` → `Statement::Scan` → `bind_scan`）と (b) 述語つき
/// `UPDATE` 経由（`validate_update_form` → `bind_update_form`）の両方で
/// 束縛し、`metadata_filters()`／`expr_filters()` が完全一致することを固定する
/// （計画 §2.1・§5.3: 二重実装を禁止し `bind_where_predicates` の単一実装へ
/// 集約したことの機械的裏付け）。
#[test]
fn predicate_update_where_binds_to_the_same_representation_as_select_where() {
    let lookup = catalog();
    let where_clause = "lang = 'ja' AND path LIKE 'src/%' AND id > 10";

    let select_sql = format!("SELECT id FROM documents WHERE {where_clause} LIMIT 1");
    let validated_select =
        validate_sql(&select_sql, &lookup).expect("SELECT scan form must pass the allowlist");
    let scan = match validated_select {
        engine::sql::allowlist::Statement::Scan(scan) => scan,
        other => panic!("expected Statement::Scan, got {other:?}"),
    };
    let bound_scan =
        bind_scan(&scan, &schema(), &UdfRegistry::default()).expect("SELECT scan must bind");

    let update_sql = format!(
        "UPDATE documents SET body = 'x' WHERE {where_clause} USING OPERATION_ID 'op-0001'"
    );
    let validated_update = validate_update_form(&update_sql, &lookup, LedgerMode::Ledgered)
        .expect("predicate-form UPDATE must pass the allowlist");
    let bound_update = bind_update_form(&validated_update, &schema(), &UdfRegistry::default())
        .expect("predicate-form UPDATE must bind");
    let update_predicate = match bound_update {
        BoundUpdateForm::Predicate(p) => p,
        BoundUpdateForm::Single(_) => panic!("expected Predicate variant"),
    };

    assert_eq!(
        bound_scan.metadata_filters(),
        update_predicate.metadata_filters()
    );
    assert_eq!(bound_scan.expr_filters(), update_predicate.expr_filters());
}

/// SET 対象化した `tenant_id`／`visibility` はいずれも構造的に拒否される
/// （疑似列・RLS 内部列。`42601`）ことを公開 API 経由でも固定する。テナント・
/// 可視性を書き換える経路が存在しないことのテナント境界検証（security.md
/// 「アクセス制御の不備」対応）。
#[test]
fn bound_predicate_update_never_exposes_tenant_or_visibility_assignment() {
    let lookup = catalog();

    let err = validate_update_form(
        "UPDATE documents SET tenant_id = 'evil' WHERE lang = 'ja' USING OPERATION_ID 'op-0001'",
        &lookup,
        LedgerMode::Ledgered,
    )
    .expect("must pass the allowlist (semantic rejection happens at bind time)");
    let bind_err = bind_update_form(&err, &schema(), &UdfRegistry::default()).unwrap_err();
    assert_eq!(bind_err.wire_code(), "42601");

    let err = validate_update_form(
        "UPDATE documents SET visibility = 'private' WHERE lang = 'ja' USING OPERATION_ID 'op-0001'",
        &lookup,
        LedgerMode::Ledgered,
    )
    .expect("must pass the allowlist (semantic rejection happens at bind time)");
    let bind_err = bind_update_form(&err, &schema(), &UdfRegistry::default()).unwrap_err();
    assert_eq!(bind_err.wire_code(), "42601");
}

/// `WHERE` 省略は許可リスト層で `42601` に拒否され、`ValidatedUpdateForm` を
/// 経由しても全行更新に相当する形が構造上作れないことを固定する。
#[test]
fn where_clause_omission_is_rejected_before_reaching_bound_form() {
    let lookup = catalog();
    let err = validate_update_form(
        "UPDATE documents SET body = 'x' USING OPERATION_ID 'op-0001'",
        &lookup,
        LedgerMode::Ledgered,
    )
    .unwrap_err();
    assert_eq!(err.wire_code(), "42601");
}

/// [`engine::sql::parser::check_dml_affected_rows_with_limit`] が公開 API として
/// 到達可能であることを固定する（実行結線〔Issue #871〕が対象行集合確定後・
/// 変更開始前に呼ぶ契約。計画 §2.4）。Issue #997・オーナー判断の改訂
/// （2026-09-27）で唯一の上限判定 API となった：`limit` が `None`（既定・
/// 上限なし）なら常に成功、`Some(limit)`（CLI 明示指定時）なら超過を
/// `54000` で拒否する。
#[test]
fn check_dml_affected_rows_with_limit_is_reachable() {
    assert!(engine::sql::parser::check_dml_affected_rows_with_limit(1_000_000, None).is_ok());
    let limit = std::num::NonZeroUsize::new(1_000).expect("1_000 is nonzero");
    assert!(engine::sql::parser::check_dml_affected_rows_with_limit(1_000, Some(limit)).is_ok());
    let err =
        engine::sql::parser::check_dml_affected_rows_with_limit(1_001, Some(limit)).unwrap_err();
    assert_eq!(err.wire_code(), "54000");
}
