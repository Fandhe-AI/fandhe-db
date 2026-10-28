//! `bind_scan`／`execute_scan`／`BoundScan`（TASK-186・NOSQL-3。Issue #726）が
//! engine クレート外から到達可能な公開 API であることを固定する結合テスト。
//!
//! `tests/catalog.rs::read_raw_rows` と同じ流儀（`Storage` で投入 → drop →
//! 生 `redb::Database::open` で読み取り専用トランザクションを得る）で
//! `&redb::ReadTransaction` を用意する。`Storage::db()` は `pub(crate)` のまま
//! 変更しないため、`execute_scan` の呼び出しにはこの経路以外に手段がない
//! （`sql::exec::execute_statement` の既存テストと同じ制約）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::allowlist::{validate_sql, Statement};
use engine::sql::parser::{bind_scan, BoundScan, ProjectedColumn};
use engine::sql::scan::execute_scan;
use engine::sql::udf_call::UdfRegistry;
use engine::storage::{Storage, Visibility};
use redb::ReadableDatabase;

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(4), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

/// tenant-a に `lang = "ja"` の公開行、tenant-b に `lang = "ja"` の非公開
/// （`Visibility::Private`）行をそれぞれ投入する（RLS 境界確認用）。
/// `policy.rs::PolicyContext::is_visible` は `Public` 行を全テナント共通で
/// 可視とする契約のため、tenant-b 側は `Private` にしないと「tenant-a から
/// tenant-b の行が見えない」ことの検証にならない（tenant-b 行が `Private` の
/// ときのみ、tenant-a の `PolicyContext` からは不可視になる）。
fn seed_two_tenants(storage: &Storage) {
    storage.create_table(&schema()).expect("create table");
    let ctx_a = PolicyContext::with_visibilities("tenant-a", [Visibility::Public])
        .expect("valid tenant-a ctx");
    let ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
            .expect("valid tenant-b ctx");

    for id in 1..=5u64 {
        let op_id =
            engine::recovery::required_op_id::OperationId::parse(&format!("tenant-a-op-{id}"))
                .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            storage,
            TABLE,
            &ctx_a,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![id as f32, 0.0, 0.0, 0.0]),
                Value::Text("ja".to_string()),
            ],
            &op_id,
        )
        .expect("insert tenant-a row");
    }
    for id in 101..=103u64 {
        let op_id =
            engine::recovery::required_op_id::OperationId::parse(&format!("tenant-b-op-{id}"))
                .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            storage,
            TABLE,
            &ctx_b,
            id,
            Visibility::Private,
            &[
                Value::Vector(vec![id as f32, 0.0, 0.0, 0.0]),
                Value::Text("ja".to_string()),
            ],
            &op_id,
        )
        .expect("insert tenant-b row");
    }
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public]).expect("valid tenant ctx")
}

#[test]
fn bind_scan_and_execute_scan_are_reachable_from_outside_the_crate() {
    let path = unique_db_path("sql-scan-public-api-bind-execute");
    let _guard = CleanupGuard(path.clone());

    // `validate_sql`・`bind_scan` は `Storage`（`TableLookup`）を要求するため、
    // `execute_scan` に渡す `&redb::ReadTransaction` を生 `redb::Database` で
    // 得る前に、この単一の `Storage` セッション内で完結させる（redb は同一
    // ファイルへの `Storage`／生 `Database` の同時オープンを許さないため、
    // `tests/catalog.rs::read_raw_rows` と同じく drop 後に別ハンドルで開き直す）。
    let (schema, bound) = {
        let storage = Storage::open(&path).expect("open storage");
        seed_two_tenants(&storage);
        let validated = validate_sql(
            "SELECT id, lang FROM docs WHERE lang = 'ja' LIMIT 5",
            &storage,
        )
        .expect("validate_sql should accept scan form");
        let Statement::Scan(validated_scan) = validated else {
            panic!("expected Statement::Scan");
        };
        let schema = storage.get_table_schema(TABLE).expect("get_table_schema");
        let bound = bind_scan(&validated_scan, &schema, &UdfRegistry::default())
            .expect("bind_scan should succeed");
        assert_eq!(bound.table(), TABLE);
        assert_eq!(bound.limit(), 5);
        (schema, bound)
    };

    let db = redb::Database::open(&path).expect("reopen raw database");
    let read_txn = db.begin_read().expect("begin_read");
    let ctx_a = ctx_for("tenant-a");

    let result = execute_scan(&read_txn, &ctx_a, &schema, &bound)
        .expect("execute_scan should succeed (本 Issue で pub 化)");
    assert!(result.rows.len() <= 5);
    assert!(!result.rows.is_empty(), "tenant-a に ja 行が存在するはず");
    for row in &result.rows {
        assert!(
            (1..=5).contains(&row.id),
            "tenant-b の非公開行（id 101..=103）が漏れている: {}",
            row.id
        );
    }
}

#[test]
fn bound_scan_new_constructs_directly_without_sql_text() {
    let path = unique_db_path("sql-scan-public-api-bound-scan-new");
    let _guard = CleanupGuard(path.clone());
    let schema = {
        let storage = Storage::open(&path).expect("open storage");
        seed_two_tenants(&storage);
        storage.get_table_schema(TABLE).expect("get_table_schema")
    };

    // SQL テキストの構文解析・`validate_sql` を一切経由せず、`BoundScan::new`
    // （本 Issue で追加した直接構築用 constructor）だけで束縛済み実行計画を
    // 組み立てる（TASK-186・NOSQL-3。SQL テキスト非経由の構築経路）。
    let projection = vec![
        ProjectedColumn::Id,
        ProjectedColumn::Column {
            index: 1,
            name: "lang".to_string(),
        },
    ];
    let bound = BoundScan::new(
        TABLE.to_string(),
        projection.clone(),
        Vec::new(),
        Vec::new(),
        3,
    );

    assert_eq!(bound.table(), TABLE);
    assert_eq!(bound.projection(), projection.as_slice());
    assert!(bound.metadata_filters().is_empty());
    assert!(bound.expr_filters().is_empty());
    assert_eq!(bound.limit(), 3);

    let db = redb::Database::open(&path).expect("reopen raw database");
    let read_txn = db.begin_read().expect("begin_read");
    let ctx_a = ctx_for("tenant-a");

    let result =
        execute_scan(&read_txn, &ctx_a, &schema, &bound).expect("execute_scan should succeed");
    assert!(result.rows.len() <= 3);
    for row in &result.rows {
        assert!(
            (1..=5).contains(&row.id),
            "tenant-b の非公開行（id 101..=103）が漏れている: {}",
            row.id
        );
    }
}

#[test]
fn bound_scan_new_with_unvalidated_large_limit_stays_bounded() {
    // `BoundScan::new` は `BoundStatement::new` と同じく `limit` を検証しない
    // 設計判断（`validate_search_limit` は `pub(crate)` のまま）。ただし
    // `execute_scan` は `bound.limit` の値によらず結果セットの累計バイト予算で
    // 走査を打ち切るため、未検証の巨大な `limit` を渡しても無制限確保には
    // 至らない（fail-closed。OWASP「不安全な設計」観点の回帰テスト）。
    let path = unique_db_path("sql-scan-public-api-unvalidated-limit");
    let _guard = CleanupGuard(path.clone());
    let schema = {
        let storage = Storage::open(&path).expect("open storage");
        seed_two_tenants(&storage);
        storage.get_table_schema(TABLE).expect("get_table_schema")
    };

    let bound = BoundScan::new(
        TABLE.to_string(),
        vec![ProjectedColumn::Id],
        Vec::new(),
        Vec::new(),
        usize::MAX,
    );
    assert_eq!(bound.limit(), usize::MAX);

    let db = redb::Database::open(&path).expect("reopen raw database");
    let read_txn = db.begin_read().expect("begin_read");
    let ctx_a = ctx_for("tenant-a");

    // 対象データは少数（5 行）のため、`usize::MAX` の `limit` でも正常に完走し
    // 全可視行が返るはず（累計バイト予算に達する規模ではない）。無制限確保に
    // よる panic・OOM が起きないことそのものがこのテストの主眼。
    let result = execute_scan(&read_txn, &ctx_a, &schema, &bound)
        .expect("execute_scan should stay bounded even with usize::MAX limit");
    assert_eq!(result.rows.len(), 5);
    for row in &result.rows {
        assert!(
            (1..=5).contains(&row.id),
            "tenant-b の非公開行（id 101..=103）が漏れている: {}",
            row.id
        );
    }
}
