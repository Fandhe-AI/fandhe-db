//! `bind_aggregate`／`execute_aggregate`／`BoundAggregate`（TASK-186・NOSQL-4・
//! NOSQL-5。Issue #727）が engine クレート外から到達可能な公開 API であることを
//! 固定する結合テスト。
//!
//! `tests/sql_scan_public_api.rs` と同じ流儀（`Storage` で投入 → drop →
//! 生 `redb::Database::open` で読み取り専用トランザクションを得る）で
//! `&redb::ReadTransaction` を用意する。`Storage::db()` は `pub(crate)` のまま
//! 変更しないため、`execute_aggregate` の呼び出しにはこの経路以外に手段がない。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::aggregate::execute_aggregate;
use engine::sql::allowlist::{validate_sql, AggregateFunc, Statement};
use engine::sql::exec::Cell;
use engine::sql::parser::{
    bind_aggregate, AggregateTarget, BoundAggregate, BoundAggregateItem, HavingOp, HavingSpec,
};
use engine::sql::udf_call::UdfRegistry;
use engine::storage::{Storage, Visibility};
use redb::ReadableDatabase;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
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

/// tenant-a に `Public` 行 id 1..=5（`lang` = `"ja"` 3 件・`"en"` 2 件）、
/// tenant-b に `Private` 行 id 101..=103（`lang` = `"xx"`。tenant-b にしか
/// 存在しないグループ値）を投入する（RLS 境界確認用。
/// `tests/sql_scan_public_api.rs::seed_two_tenants` と同じ判断で tenant-b 側を
/// `Private` にし、tenant-a の `PolicyContext` からは不可視にする）。
fn seed_two_tenants(storage: &Storage) {
    storage.create_table(&schema()).expect("create table");
    let ctx_a = PolicyContext::with_visibilities("tenant-a", [Visibility::Public])
        .expect("valid tenant-a ctx");
    let ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
            .expect("valid tenant-b ctx");

    let langs = ["ja", "ja", "ja", "en", "en"];
    for (idx, lang) in langs.iter().enumerate() {
        let id = idx as u64 + 1;
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
                Value::Text((*lang).to_string()),
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
                Value::Text("xx".to_string()),
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
fn bind_aggregate_and_execute_aggregate_are_reachable_from_outside_the_crate() {
    let path = unique_db_path("sql-aggregate-public-api-bind-execute");
    let _guard = CleanupGuard(path.clone());

    // `validate_sql`・`bind_aggregate` は `Storage`（`TableLookup`）を要求する
    // ため、`execute_aggregate` に渡す `&redb::ReadTransaction` を生
    // `redb::Database` で得る前に、この単一の `Storage` セッション内で完結させる
    // （`tests/sql_scan_public_api.rs` と同じ制約）。
    let (schema, bound) = {
        let storage = Storage::open(&path).expect("open storage");
        seed_two_tenants(&storage);
        let validated = validate_sql("SELECT COUNT(*), SUM(id), MIN(lang) FROM docs", &storage)
            .expect("validate_sql should accept aggregate form");
        let Statement::Aggregate(validated_aggregate) = validated else {
            panic!("expected Statement::Aggregate");
        };
        let schema = storage.get_table_schema(TABLE).expect("get_table_schema");
        let bound = bind_aggregate(&validated_aggregate, &schema, &UdfRegistry::default())
            .expect("bind_aggregate should succeed");
        assert_eq!(bound.table(), TABLE);
        assert_eq!(bound.items().len(), 3);
        assert_eq!(bound.items()[0].func(), AggregateFunc::Count);
        assert_eq!(bound.items()[0].name(), "count");
        assert!(!bound.has_group_by());
        (schema, bound)
    };

    let db = redb::Database::open(&path).expect("reopen raw database");
    let read_txn = db.begin_read().expect("begin_read");
    let ctx_a = ctx_for("tenant-a");

    let result = execute_aggregate(&read_txn, &ctx_a, &schema, &bound)
        .expect("execute_aggregate should succeed (本 Issue で pub 化)");
    assert_eq!(result.rows.len(), 1);
    let row = &result.rows[0];
    // tenant-a の可視行は 5 件（id 1..=5）。tenant-b の Private 行
    // （id 101..=103・`lang="xx"`）が混入しないことは MIN(lang) の値で確認する
    // （RLS-7・RLS-8。混入していれば辞書順最小の "en" ではなく別値になる）。
    assert_eq!(row.cells[0], Cell::Integer(5));
    assert_eq!(row.cells[1], Cell::Integer(15));
    assert_eq!(row.cells[2], Cell::Text("en".to_string()));
}

#[test]
fn execute_aggregate_dispatches_group_by_without_caches() {
    let path = unique_db_path("sql-aggregate-public-api-group-by");
    let _guard = CleanupGuard(path.clone());

    let (schema, bound) = {
        let storage = Storage::open(&path).expect("open storage");
        seed_two_tenants(&storage);
        let validated = validate_sql(
            "SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang ORDER BY n DESC",
            &storage,
        )
        .expect("validate_sql should accept group by form");
        let Statement::Aggregate(validated_aggregate) = validated else {
            panic!("expected Statement::Aggregate");
        };
        let schema = storage.get_table_schema(TABLE).expect("get_table_schema");
        let bound = bind_aggregate(&validated_aggregate, &schema, &UdfRegistry::default())
            .expect("bind_aggregate should succeed");
        assert!(bound.has_group_by());
        (schema, bound)
    };

    let db = redb::Database::open(&path).expect("reopen raw database");
    let read_txn = db.begin_read().expect("begin_read");
    let ctx_a = ctx_for("tenant-a");

    // `execute_aggregate`（3 キャッシュとも `None`）越しに
    // `execute_grouped_aggregate` の列挙形フォールバック経路が動くことを固定
    // する（Issue #475）。tenant-b 専有のグループ値 `"xx"` は現れない。
    let result = execute_aggregate(&read_txn, &ctx_a, &schema, &bound)
        .expect("execute_aggregate should dispatch to group by execution");
    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.rows[0].cells[0], Cell::Text("ja".to_string()));
    assert_eq!(result.rows[0].cells[1], Cell::Integer(3));
    assert_eq!(result.rows[1].cells[0], Cell::Text("en".to_string()));
    assert_eq!(result.rows[1].cells[1], Cell::Integer(2));
}

#[test]
fn execute_aggregate_applies_index_eligible_where_without_caches() {
    let path = unique_db_path("sql-aggregate-public-api-where");
    let _guard = CleanupGuard(path.clone());

    let (schema, bound) = {
        let storage = Storage::open(&path).expect("open storage");
        seed_two_tenants(&storage);
        let validated = validate_sql("SELECT COUNT(*) FROM docs WHERE lang = 'ja'", &storage)
            .expect("validate_sql should accept aggregate form with WHERE");
        let Statement::Aggregate(validated_aggregate) = validated else {
            panic!("expected Statement::Aggregate");
        };
        let schema = storage.get_table_schema(TABLE).expect("get_table_schema");
        let bound = bind_aggregate(&validated_aggregate, &schema, &UdfRegistry::default())
            .expect("bind_aggregate should succeed");
        assert_eq!(bound.metadata_filters().len(), 1);
        assert!(bound.expr_filters().is_empty());
        (schema, bound)
    };

    let db = redb::Database::open(&path).expect("reopen raw database");
    let read_txn = db.begin_read().expect("begin_read");
    let ctx_a = ctx_for("tenant-a");

    // 索引対応述語（`TEXT` 等価）だが `ScalarIndex` 非提供（`None`）のため
    // 従来の全行走査へ縮退し、正しい値を返すことを固定する。
    let result = execute_aggregate(&read_txn, &ctx_a, &schema, &bound)
        .expect("execute_aggregate should succeed with WHERE and no caches");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].cells[0], Cell::Integer(3));
}

#[test]
fn execute_aggregate_returns_empty_set_contract_for_table_without_visible_rows() {
    // SQL-13 の空集合契約（`COUNT=0`・`SUM=NULL`）を確認するため、行を一切
    // 投入していないテーブルに対して集計を実行する。
    let path = unique_db_path("sql-aggregate-public-api-empty-table");
    let _guard = CleanupGuard(path.clone());

    let (schema, bound) = {
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema()).expect("create table");
        let validated = validate_sql("SELECT COUNT(*), SUM(id) FROM docs", &storage)
            .expect("validate_sql should accept aggregate form");
        let Statement::Aggregate(validated_aggregate) = validated else {
            panic!("expected Statement::Aggregate");
        };
        let schema = storage.get_table_schema(TABLE).expect("get_table_schema");
        let bound = bind_aggregate(&validated_aggregate, &schema, &UdfRegistry::default())
            .expect("bind_aggregate should succeed");
        (schema, bound)
    };

    let db = redb::Database::open(&path).expect("reopen raw database");
    let read_txn = db.begin_read().expect("begin_read");
    let ctx_a = ctx_for("tenant-a");

    let result = execute_aggregate(&read_txn, &ctx_a, &schema, &bound)
        .expect("execute_aggregate should succeed on a table without rows");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].cells[0], Cell::Integer(0));
    assert_eq!(result.rows[0].cells[1], Cell::Null);
}

// --- Issue #768・TASK-186・NOSQL-4: `BoundAggregate::new`／`BoundAggregateItem::bind`
// （SQL テキスト非経由の直接構築）の到達性・SQL 経路との同一結果契約 -------------

/// `BoundAggregate::new` で組んだ計画が、等価な SQL テキスト（`validate_sql` →
/// `bind_aggregate`）から得た `BoundAggregate` と完全一致することを固定する
/// （NOSQL-4 の「SQL 表層と同一結果」契約の中核）。5 関数 × `*`／`id`（疑似列）／
/// `TEXT` 列（`lang`）／`VECTOR` 列（`embedding`、`COUNT` のみ）を横断する。
#[test]
fn bound_aggregate_new_matches_sql_text_bind_for_all_functions() {
    let path = unique_db_path("sql-aggregate-public-api-new-matches-sql");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let schema_val = storage.get_table_schema(TABLE).expect("get_table_schema");

    let sql = "SELECT COUNT(*), COUNT(id), SUM(id), AVG(id), MIN(id), MAX(id), \
               MIN(lang), MAX(lang), COUNT(lang), COUNT(embedding) FROM docs WHERE lang = 'ja'";
    let validated = validate_sql(sql, &storage).expect("validate_sql should accept aggregate form");
    let Statement::Aggregate(validated_aggregate) = validated else {
        panic!("expected Statement::Aggregate");
    };
    let via_sql = bind_aggregate(&validated_aggregate, &schema_val, &UdfRegistry::default())
        .expect("bind_aggregate should succeed");

    let items = vec![
        BoundAggregateItem::bind(AggregateFunc::Count, AggregateTarget::Star, &schema_val)
            .expect("count(*) should bind"),
        BoundAggregateItem::bind(
            AggregateFunc::Count,
            AggregateTarget::Column("id".to_string()),
            &schema_val,
        )
        .expect("count(id) should bind"),
        BoundAggregateItem::bind(
            AggregateFunc::Sum,
            AggregateTarget::Column("id".to_string()),
            &schema_val,
        )
        .expect("sum(id) should bind"),
        BoundAggregateItem::bind(
            AggregateFunc::Avg,
            AggregateTarget::Column("id".to_string()),
            &schema_val,
        )
        .expect("avg(id) should bind"),
        BoundAggregateItem::bind(
            AggregateFunc::Min,
            AggregateTarget::Column("id".to_string()),
            &schema_val,
        )
        .expect("min(id) should bind"),
        BoundAggregateItem::bind(
            AggregateFunc::Max,
            AggregateTarget::Column("id".to_string()),
            &schema_val,
        )
        .expect("max(id) should bind"),
        BoundAggregateItem::bind(
            AggregateFunc::Min,
            AggregateTarget::Column("lang".to_string()),
            &schema_val,
        )
        .expect("min(lang) should bind"),
        BoundAggregateItem::bind(
            AggregateFunc::Max,
            AggregateTarget::Column("lang".to_string()),
            &schema_val,
        )
        .expect("max(lang) should bind"),
        BoundAggregateItem::bind(
            AggregateFunc::Count,
            AggregateTarget::Column("lang".to_string()),
            &schema_val,
        )
        .expect("count(lang) should bind"),
        BoundAggregateItem::bind(
            AggregateFunc::Count,
            AggregateTarget::Column("embedding".to_string()),
            &schema_val,
        )
        .expect("count(embedding) should bind"),
    ];
    let metadata_filters = via_sql.metadata_filters().to_vec();
    let expr_filters = via_sql.expr_filters().to_vec();
    let via_direct = BoundAggregate::new(TABLE.to_string(), items, metadata_filters, expr_filters)
        .expect("BoundAggregate::new should succeed");

    assert_eq!(via_direct, via_sql);
}

/// `*` は `COUNT` 以外の関数では受理しない（SQL 表層の構文層が
/// `Parser::parse_aggregate_item` で `SUM(*)` 等を構造的に拒否するのと同じ
/// `42601` 分類を、直接構築経路でも再現する）。
#[test]
fn bound_aggregate_item_bind_rejects_star_with_non_count_function() {
    let schema_val = schema();
    let err = BoundAggregateItem::bind(AggregateFunc::Sum, AggregateTarget::Star, &schema_val)
        .expect_err("SUM(*) must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

/// 集計項目が 0 個の `BoundAggregate::new` は SQL 側の構文エラー（集計項目なしの
/// SELECT リスト）と同じ `42601` で拒否する。
#[test]
fn bound_aggregate_new_rejects_empty_items() {
    let err = BoundAggregate::new(TABLE.to_string(), Vec::new(), Vec::new(), Vec::new())
        .expect_err("empty items must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

/// [`engine::sql::allowlist::MAX_AGGREGATE_ITEMS`] 超過は `Vec` 確保後であっても
/// `54000`（`payload_too_large`）で拒否する。
#[test]
fn bound_aggregate_new_rejects_item_count_over_limit() {
    let schema_val = schema();
    let max = engine::sql::allowlist::MAX_AGGREGATE_ITEMS;
    let items: Vec<BoundAggregateItem> = (0..=max)
        .map(|_| {
            BoundAggregateItem::bind(AggregateFunc::Count, AggregateTarget::Star, &schema_val)
                .expect("count(*) should bind")
        })
        .collect();
    let err = BoundAggregate::new(TABLE.to_string(), items, Vec::new(), Vec::new())
        .expect_err("over-limit item count must be rejected");
    assert_eq!(err.wire_code(), "54000");
}

/// ちょうど上限件数は受理する（境界値）。
#[test]
fn bound_aggregate_new_accepts_item_count_at_limit() {
    let schema_val = schema();
    let max = engine::sql::allowlist::MAX_AGGREGATE_ITEMS;
    let items: Vec<BoundAggregateItem> = (0..max)
        .map(|_| {
            BoundAggregateItem::bind(AggregateFunc::Count, AggregateTarget::Star, &schema_val)
                .expect("count(*) should bind")
        })
        .collect();
    let bound = BoundAggregate::new(TABLE.to_string(), items, Vec::new(), Vec::new())
        .expect("at-limit item count must be accepted");
    assert_eq!(bound.items().len(), max);
}

/// `VECTOR` 列は `SUM`/`AVG`/`MIN`/`MAX` と組み合わせると型不整合（`22000`）。
/// `COUNT` のみ非 NULL 行数として受理する（SQL-13・PR #229 と同一の例外）。
#[test]
fn bound_aggregate_item_bind_rejects_vector_column_for_sum_avg_min_max_but_accepts_count() {
    let schema_val = schema();
    for func in [
        AggregateFunc::Sum,
        AggregateFunc::Avg,
        AggregateFunc::Min,
        AggregateFunc::Max,
    ] {
        let err = BoundAggregateItem::bind(
            func,
            AggregateTarget::Column("embedding".to_string()),
            &schema_val,
        )
        .expect_err("VECTOR column with SUM/AVG/MIN/MAX must be rejected");
        assert_eq!(err.wire_code(), "42883", "func={func:?}");
    }
    BoundAggregateItem::bind(
        AggregateFunc::Count,
        AggregateTarget::Column("embedding".to_string()),
        &schema_val,
    )
    .expect("COUNT(embedding) must be accepted");
}

/// `TEXT` 列は `SUM`/`AVG` と組み合わせると型不整合（`22000`）。
#[test]
fn bound_aggregate_item_bind_rejects_text_column_for_sum_and_avg() {
    let schema_val = schema();
    for func in [AggregateFunc::Sum, AggregateFunc::Avg] {
        let err = BoundAggregateItem::bind(
            func,
            AggregateTarget::Column("lang".to_string()),
            &schema_val,
        )
        .expect_err("TEXT column with SUM/AVG must be rejected");
        assert_eq!(err.wire_code(), "42883", "func={func:?}");
    }
}

/// 未知の列名は `22000`（`resolve_aggregate_input` の既存契約をそのまま透過）。
#[test]
fn bound_aggregate_item_bind_rejects_unknown_column() {
    let schema_val = schema();
    let err = BoundAggregateItem::bind(
        AggregateFunc::Sum,
        AggregateTarget::Column("nope".to_string()),
        &schema_val,
    )
    .expect_err("unknown column must be rejected");
    assert_eq!(err.wire_code(), "22000");
}

// --- Issue #769・TASK-186・NOSQL-5: `BoundAggregate::new_grouped`
// （SQL テキスト非経由の `GROUP BY`／`HAVING` 直接構築）の到達性・SQL 経路との
// 同一結果契約 ----------------------------------------------------------------

/// `BoundAggregate::new_grouped` で組んだ計画が、等価な SQL テキスト
/// （`validate_sql` → `bind_aggregate`）から得た `BoundAggregate` と完全一致
/// することを固定する（NOSQL-5 の「SQL 表層と同一結果」契約の中核）。
#[test]
fn bound_aggregate_new_grouped_matches_sql_text_bind() {
    let path = unique_db_path("sql-aggregate-public-api-new-grouped-matches-sql");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let schema_val = storage.get_table_schema(TABLE).expect("get_table_schema");

    let sql = "SELECT lang, COUNT(*), SUM(id) FROM docs GROUP BY lang HAVING count >= 2";
    let validated = validate_sql(sql, &storage).expect("validate_sql should accept group by form");
    let Statement::Aggregate(validated_aggregate) = validated else {
        panic!("expected Statement::Aggregate");
    };
    let via_sql = bind_aggregate(&validated_aggregate, &schema_val, &UdfRegistry::default())
        .expect("bind_aggregate should succeed");

    let items = vec![
        BoundAggregateItem::bind(AggregateFunc::Count, AggregateTarget::Star, &schema_val)
            .expect("count(*) should bind"),
        BoundAggregateItem::bind(
            AggregateFunc::Sum,
            AggregateTarget::Column("id".to_string()),
            &schema_val,
        )
        .expect("sum(id) should bind"),
    ];
    let having = vec![HavingSpec {
        item_index: 0,
        op: HavingOp::Ge,
        literal: 2.0,
    }];
    let via_direct = BoundAggregate::new_grouped(
        TABLE.to_string(),
        items,
        Vec::new(),
        Vec::new(),
        "lang",
        having,
        &schema_val,
    )
    .expect("new_grouped should succeed");

    assert_eq!(via_direct, via_sql);
}

/// `execute_aggregate` を通し、直接構築した `GROUP BY`／`HAVING` 計画が
/// SQL テキスト経由と同一の実行結果（RLS 境界を含む）になることを固定する。
#[test]
fn execute_aggregate_dispatches_new_grouped_plan_without_caches() {
    let path = unique_db_path("sql-aggregate-public-api-new-grouped-execute");
    let _guard = CleanupGuard(path.clone());

    let (schema_val, bound) = {
        let storage = Storage::open(&path).expect("open storage");
        seed_two_tenants(&storage);
        let schema_val = storage.get_table_schema(TABLE).expect("get_table_schema");
        let items = vec![BoundAggregateItem::bind(
            AggregateFunc::Count,
            AggregateTarget::Star,
            &schema_val,
        )
        .expect("count(*) should bind")];
        let bound = BoundAggregate::new_grouped(
            TABLE.to_string(),
            items,
            Vec::new(),
            Vec::new(),
            "lang",
            Vec::new(),
            &schema_val,
        )
        .expect("new_grouped should succeed");
        assert!(bound.has_group_by());
        (schema_val, bound)
    };

    let db = redb::Database::open(&path).expect("reopen raw database");
    let read_txn = db.begin_read().expect("begin_read");
    let ctx_a = ctx_for("tenant-a");

    // tenant-b 専有のグループ値 `"xx"` は現れない（RLS-7・RLS-8）。`ORDER BY`
    // 省略時の既定順はグループキーのバイト順昇順（"en" < "ja"）。
    let result = execute_aggregate(&read_txn, &ctx_a, &schema_val, &bound)
        .expect("execute_aggregate should dispatch to group by execution");
    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.rows[0].cells[0], Cell::Text("en".to_string()));
    assert_eq!(result.rows[0].cells[1], Cell::Integer(2));
    assert_eq!(result.rows[1].cells[0], Cell::Text("ja".to_string()));
    assert_eq!(result.rows[1].cells[1], Cell::Integer(3));
}

/// `GROUP BY` 列が並べ替え不能な型（`VECTOR`）・未知列の場合は `22000` で拒否する
/// （SQL テキスト経由の `bind_group_by_clause` と同じ分類。Issue #1185・SQL-25 (d)
/// で `TEXT` 限定を外したため、疑似列 `id` は受理側へ契約改訂した）。
#[test]
fn bound_aggregate_new_grouped_rejects_non_text_group_by_column() {
    let schema_val = schema();
    let items =
        vec![
            BoundAggregateItem::bind(AggregateFunc::Count, AggregateTarget::Star, &schema_val)
                .expect("count(*) should bind"),
        ];
    for column in ["embedding", "nope"] {
        let err = BoundAggregate::new_grouped(
            TABLE.to_string(),
            items.clone(),
            Vec::new(),
            Vec::new(),
            column,
            Vec::new(),
            &schema_val,
        )
        .expect_err("non-TEXT group by column must be rejected");
        assert_eq!(err.wire_code(), "22000", "column={column}");
    }
    // Issue #1185: 疑似列 `id` はグループキーとして受理される。
    BoundAggregate::new_grouped(
        TABLE.to_string(),
        items,
        Vec::new(),
        Vec::new(),
        "id",
        Vec::new(),
        &schema_val,
    )
    .expect("id pseudo-column group by key is accepted since Issue #1185");
}

/// `HAVING` が `MIN`/`MAX(<TEXT 列>)` を参照すると `22000`（`COUNT(<TEXT 列>)`
/// は許可。SQL テキスト経由の `check_having_target_is_numeric` を共有）。
#[test]
fn bound_aggregate_new_grouped_rejects_having_on_text_min_max() {
    let schema_val = schema();
    let items = vec![BoundAggregateItem::bind(
        AggregateFunc::Min,
        AggregateTarget::Column("lang".to_string()),
        &schema_val,
    )
    .expect("min(lang) should bind")];
    let having = vec![HavingSpec {
        item_index: 0,
        op: HavingOp::Ge,
        literal: 1.0,
    }];
    let err = BoundAggregate::new_grouped(
        TABLE.to_string(),
        items,
        Vec::new(),
        Vec::new(),
        "lang",
        having,
        &schema_val,
    )
    .expect_err("HAVING on MIN(TEXT) must be rejected");
    assert_eq!(err.wire_code(), "22000");
}

/// `HAVING` の `item_index` が `items` の範囲外なら `22000`。
#[test]
fn bound_aggregate_new_grouped_rejects_having_item_index_out_of_range() {
    let schema_val = schema();
    let items =
        vec![
            BoundAggregateItem::bind(AggregateFunc::Count, AggregateTarget::Star, &schema_val)
                .expect("count(*) should bind"),
        ];
    let having = vec![HavingSpec {
        item_index: 1,
        op: HavingOp::Ge,
        literal: 1.0,
    }];
    let err = BoundAggregate::new_grouped(
        TABLE.to_string(),
        items,
        Vec::new(),
        Vec::new(),
        "lang",
        having,
        &schema_val,
    )
    .expect_err("out-of-range item_index must be rejected");
    assert_eq!(err.wire_code(), "22000");
}

/// `HAVING` 述語数が [`engine::sql::allowlist::MAX_AGGREGATE_ITEMS`] を超過
/// すると `54000`（`Vec` 確保前の検査）。
#[test]
fn bound_aggregate_new_grouped_rejects_having_predicate_count_over_limit() {
    let schema_val = schema();
    let items =
        vec![
            BoundAggregateItem::bind(AggregateFunc::Count, AggregateTarget::Star, &schema_val)
                .expect("count(*) should bind"),
        ];
    let max = engine::sql::allowlist::MAX_AGGREGATE_ITEMS;
    let having: Vec<HavingSpec> = (0..=max)
        .map(|_| HavingSpec {
            item_index: 0,
            op: HavingOp::Ge,
            literal: 1.0,
        })
        .collect();
    let err = BoundAggregate::new_grouped(
        TABLE.to_string(),
        items,
        Vec::new(),
        Vec::new(),
        "lang",
        having,
        &schema_val,
    )
    .expect_err("over-limit HAVING predicate count must be rejected");
    assert_eq!(err.wire_code(), "54000");
}

/// `HAVING` の `literal` が非有限なら `42601`。
#[test]
fn bound_aggregate_new_grouped_rejects_non_finite_having_literal() {
    let schema_val = schema();
    let items =
        vec![
            BoundAggregateItem::bind(AggregateFunc::Count, AggregateTarget::Star, &schema_val)
                .expect("count(*) should bind"),
        ];
    let having = vec![HavingSpec {
        item_index: 0,
        op: HavingOp::Ge,
        literal: f64::INFINITY,
    }];
    let err = BoundAggregate::new_grouped(
        TABLE.to_string(),
        items,
        Vec::new(),
        Vec::new(),
        "lang",
        having,
        &schema_val,
    )
    .expect_err("non-finite HAVING literal must be rejected");
    assert_eq!(err.wire_code(), "42601");
}
