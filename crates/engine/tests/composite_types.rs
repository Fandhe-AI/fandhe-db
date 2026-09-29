//! `ARRAY` 列型（TABLE-14・TASK-198、Issue #888）の結合テスト。ポインタ:
//! `docs/spec/05-tasks.md` TASK-198・`docs/spec/04-behavior/data-model.md`
//! TABLE-14・`docs/spec/04-behavior/data-model.md` TABLE-1／TABLE-6／TABLE-7・
//! `docs/spec/04-behavior/wire-protocol.md` WIRE-13。
//!
//! `tests/boolean_column.rs` と同じ流儀（`unique_db_path`／`CleanupGuard`、実
//! `Storage`＋`CpuScalarProvider`、`EngineCore::execute_sql`／
//! `execute_sql_in_session` を production 経路として検証）。配列列の往復・
//! リテラル受理・要素数上限・RLS 境界・検索経路（KNN・`EXPLAIN`）への非影響・
//! 集計（`COUNT` のみ）を固定する。Issue #1193 で要素型の拡大（INTEGER／BIGINT／
//! REAL／DOUBLE／DATE／TIMESTAMP／UUID）・NULL 要素・配列列の等価述語（`=`／`IN`／
//! `IS [NOT] NULL`）を追加した（NOSQL-17・WIRE-13）。

use engine::catalog::{ArrayElemType, ArrayType, ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new(
                "tags",
                ColumnType::Array(ArrayType::new(ArrayElemType::Text, 4).expect("array ty")),
                true,
            ),
            ColumnDef::new(
                "flags",
                ColumnType::Array(ArrayType::new(ArrayElemType::Bool, 4).expect("array ty")),
                true,
            ),
        ],
    )
}

/// 配列列を持たない、それ以外は [`schema`] と同一のテーブル定義（検索経路の
/// 非影響検証用の対照）。
fn baseline_schema() -> TableSchema {
    TableSchema::new(
        "docs_baseline",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("array-column");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    storage
        .create_table(&baseline_schema())
        .expect("create baseline table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_sql(id: u64, lang: &str, tags_literal: &str, flags_literal: &str, seq: u64) -> String {
    format!(
        "INSERT INTO {TABLE} (id, embedding, lang, tags, flags) \
         VALUES ({id}, '[0.1,0.2]', '{lang}', '{tags_literal}', '{flags_literal}') \
         USING OPERATION_ID 'seed-{id}-{seq}'"
    )
}

// --- 往復（NULL・空配列・引用要素の区別を含む） -------------------------------

#[test]
fn array_column_roundtrips_through_storage_reopen() {
    let path = unique_db_path("array-column-roundtrip");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx_for("alice");

    {
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema()).expect("create table");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(1, "ja", "{a,\"b b\",\"\"}", "{t,f}", 1),
        )
        .expect("insert with array values should succeed");
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(2, "ja", "{}", "{}", 2),
        )
        .expect("insert with empty arrays should succeed");
        // tags/flags 未指定（nullable のため NULL 列として許容される）。
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang) VALUES (3, '[0.1,0.2]', 'ja') \
                 USING OPERATION_ID 'seed-3-3'"
            ),
        )
        .expect("insert without array columns should succeed");
    }

    // 再オープン後も値・型が一致する（NULL 列と空配列 `{}` が区別されること含む）。
    let storage = Storage::open(&path).expect("reopen storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let result = core
        .execute_sql(
            &alice,
            &format!("SELECT id, tags, flags FROM {TABLE} LIMIT 100"),
        )
        .expect("select after reopen should succeed");
    let mut by_id: std::collections::BTreeMap<u64, (Cell, Cell)> =
        std::collections::BTreeMap::new();
    for row in &result.rows {
        by_id.insert(row.id, (row.cells[1].clone(), row.cells[2].clone()));
    }
    match by_id.get(&1) {
        Some((Cell::Array(tags), Cell::Array(flags))) => {
            assert_eq!(
                *tags,
                engine::row_codec::ArrayValue::Text(vec![
                    Some("a".to_string()),
                    Some("b b".to_string()),
                    Some("".to_string())
                ])
            );
            assert_eq!(
                *flags,
                engine::row_codec::ArrayValue::Bool(vec![Some(true), Some(false)])
            );
        }
        other => panic!("expected Cell::Array pair for id=1, got {other:?}"),
    }
    match by_id.get(&2) {
        Some((Cell::Array(tags), Cell::Array(flags))) => {
            assert_eq!(*tags, engine::row_codec::ArrayValue::Text(vec![]));
            assert_eq!(*flags, engine::row_codec::ArrayValue::Bool(vec![]));
        }
        other => panic!("expected empty Cell::Array pair for id=2, got {other:?}"),
    }
    assert_eq!(by_id.get(&3), Some(&(Cell::Null, Cell::Null)));
}

// --- 要素数上限・リテラル形式違反・NULL 要素非対応 ---------------------------

#[test]
fn insert_rejects_element_count_exceeding_column_max_len() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    // tags は max_len=4。5 要素は超過。
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(1, "ja", "{a,b,c,d,e}", "{}", 1),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "54000");

    // 拒否された INSERT は台帳・テーブル世代を進めない（write txn 開始前に拒否）。
    let count = core
        .execute_sql(&alice, &format!("SELECT COUNT(*) FROM {TABLE}"))
        .expect("count should succeed");
    match &count.rows[0].cells[0] {
        Cell::Integer(0) => {}
        other => panic!("expected 0 rows after rejected insert, got {other:?}"),
    }
}

#[test]
fn insert_accepts_null_element_and_rejects_malformed_literal() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    // 引用なしの NULL は NULL 要素として受理される（Issue #1193）。
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "{a,null,b}", "{t,NULL}", 1),
    )
    .expect("NULL element must be accepted");
    let result = core
        .execute_sql(
            &alice,
            &format!("SELECT tags, flags FROM {TABLE} WHERE id = 1 LIMIT 1"),
        )
        .expect("select");
    match (&result.rows[0].cells[0], &result.rows[0].cells[1]) {
        (Cell::Array(tags), Cell::Array(flags)) => {
            assert_eq!(
                *tags,
                engine::row_codec::ArrayValue::Text(vec![
                    Some("a".to_string()),
                    None,
                    Some("b".to_string())
                ])
            );
            assert_eq!(
                *flags,
                engine::row_codec::ArrayValue::Bool(vec![Some(true), None])
            );
        }
        other => panic!("expected Cell::Array pair, got {other:?}"),
    }

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(2, "ja", "a,b,c", "{}", 2),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22P02");
}

#[test]
fn insert_rejects_invalid_bool_word_for_bool_array_column() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(1, "ja", "{}", "{t,maybe}", 1),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22P02");
}

// --- UPDATE SET / UPSERT ------------------------------------------------------

#[test]
fn update_set_replaces_array_column_value() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "{a}", "{t}", 1),
    )
    .expect("seed insert");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &format!("UPDATE {TABLE} SET tags = '{{x,y}}' WHERE id = 1 USING OPERATION_ID 'upd-1'"),
    )
    .expect("update should succeed");
    let result = core
        .execute_sql(
            &alice,
            &format!("SELECT tags FROM {TABLE} WHERE id = 1 LIMIT 1"),
        )
        .expect("select should succeed");
    match &result.rows[0].cells[0] {
        Cell::Array(v) => assert_eq!(
            *v,
            engine::row_codec::ArrayValue::Text(vec![Some("x".to_string()), Some("y".to_string())])
        ),
        other => panic!("expected Cell::Array, got {other:?}"),
    }
}

// --- RLS: 他テナントの配列行が漏えいしないこと --------------------------------

#[test]
fn array_rows_are_isolated_by_tenant() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let bob = ctx_for("bob");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "{secret}", "{t}", 1),
    )
    .expect("alice insert");

    let result = core
        .execute_sql(&bob, &format!("SELECT id, tags FROM {TABLE} LIMIT 100"))
        .expect("bob select should succeed");
    assert!(result.rows.is_empty(), "bob must not see alice's array row");

    let count = core
        .execute_sql(&bob, &format!("SELECT COUNT(tags) FROM {TABLE}"))
        .expect("bob count should succeed");
    match &count.rows[0].cells[0] {
        Cell::Integer(0) => {}
        other => panic!("expected 0 for bob's COUNT(tags), got {other:?}"),
    }
}

// --- 検索経路（KNN・EXPLAIN）への非影響 ---------------------------------------

#[test]
fn vector_search_is_bit_identical_with_and_without_array_column() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    for (id, x, y, tags) in [
        (1u64, "0.10", "0.20", "{a,b}"),
        (2, "0.30", "0.10", "{}"),
        (3, "0.05", "0.05", "{c}"),
    ] {
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, tags) \
                 VALUES ({id}, '[{x},{y}]', 'ja', '{tags}') USING OPERATION_ID 'v-{id}'"
            ),
        )
        .expect("insert into array-column table");
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO docs_baseline (id, embedding, lang) VALUES ({id}, '[{x},{y}]', 'ja') \
                 USING OPERATION_ID 'b-{id}'"
            ),
        )
        .expect("insert into baseline table");
    }

    let with_array = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} ORDER BY embedding <=> '[0.1,0.2]' LIMIT 3"),
        )
        .expect("knn on array-column table");
    let baseline = core
        .execute_sql(
            &alice,
            "SELECT id FROM docs_baseline ORDER BY embedding <=> '[0.1,0.2]' LIMIT 3",
        )
        .expect("knn on baseline table");

    let with_array_ids: Vec<u64> = with_array.rows.iter().map(|r| r.id).collect();
    let baseline_ids: Vec<u64> = baseline.rows.iter().map(|r| r.id).collect();
    assert_eq!(
        with_array_ids, baseline_ids,
        "presence of an ARRAY column must not change KNN ordering"
    );

    for (a, b) in with_array.rows.iter().zip(baseline.rows.iter()) {
        assert_eq!(a.score, b.score, "KNN scores must be bit-identical");
    }
}

// --- WHERE 述語・集計の対象範囲（D-A8） ---------------------------------------

#[test]
fn where_array_column_rejects_unsupported_predicates_and_malformed_literals() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    // 右辺が配列リテラルの形式でなければ書き込みと同じ分類（22P02）で拒否する。
    let err = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE tags = 'x' LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22P02");
    // 範囲比較・LIKE・BETWEEN は配列列に対して従来どおり 22000。
    for predicate in [
        "tags LIKE 'x%'",
        "tags > '{a}'",
        "tags BETWEEN '{a}' AND '{b}'",
    ] {
        let err = core
            .execute_sql(
                &alice,
                &format!("SELECT id FROM {TABLE} WHERE {predicate} LIMIT 10"),
            )
            .unwrap_err();
        assert_eq!(err.wire_code(), "22000", "predicate: {predicate}");
    }
}

fn select_ids(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<u64> {
    let mut ids: Vec<u64> = core
        .execute_sql(ctx, sql)
        .unwrap_or_else(|e| panic!("query failed: {sql}: {e:?}"))
        .rows
        .iter()
        .map(|r| r.id)
        .collect();
    ids.sort_unstable();
    ids
}

fn seed_equality_rows(core: &EngineCore, ctx: &PolicyContext) {
    for (id, tags) in [
        (1u64, Some("{a,b}")),
        (2, Some("{}")),
        (3, None),
        (4, Some("{a,NULL}")),
        (5, Some("{a,NULL}")),
        (6, Some("{NULL,a}")),
        (7, Some("{\"NULL\"}")),
    ] {
        let sql = match tags {
            Some(t) => format!(
                "INSERT INTO {TABLE} (id, embedding, lang, tags) \
                 VALUES ({id}, '[0.1,0.2]', 'ja', '{t}') USING OPERATION_ID 'eq-{id}'"
            ),
            None => format!(
                "INSERT INTO {TABLE} (id, embedding, lang) \
                 VALUES ({id}, '[0.1,0.2]', 'ja') USING OPERATION_ID 'eq-{id}'"
            ),
        };
        core.execute_sql_in_session(ctx, &mut SessionState::default(), &sql)
            .expect("seed insert");
    }
}

#[test]
fn where_array_equality_in_and_is_null_are_three_valued() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_equality_rows(&core, &alice);
    let q = |predicate: &str| {
        select_ids(
            &core,
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE {predicate} LIMIT 100"),
        )
    };
    assert_eq!(q("tags = '{a,b}'"), vec![1]);
    assert_eq!(q("tags = '{ a , b }'"), vec![1]);
    assert_eq!(q("tags = '{}'"), vec![2]);
    // NULL 要素どうしは等しい（PostgreSQL の array_eq と同じ）。位置が違えば別値。
    assert_eq!(q("tags = '{a,NULL}'"), vec![4, 5]);
    assert_eq!(q("tags = '{NULL,a}'"), vec![6]);
    // 引用つきの "NULL" は文字列 NULL（NULL 要素とは別値）。
    assert_eq!(q("tags = '{\"NULL\"}'"), vec![7]);
    assert_eq!(q("tags IN ('{a,b}','{}','{x}')"), vec![1, 2]);
    assert_eq!(q("tags IS NULL"), vec![3]);
    assert_eq!(q("tags IS NOT NULL"), vec![1, 2, 4, 5, 6, 7]);
    // 三値論理: 列 NULL の行（id=3）は NOT でも一致しない。
    assert_eq!(q("NOT tags = '{a,b}'"), vec![2, 4, 5, 6, 7]);
    assert_eq!(q("tags NOT IN ('{a,b}','{}')"), vec![4, 5, 6, 7]);
    // 索引対応述語との複合でも再評価される（PlainScan へ倒れる）。
    assert_eq!(q("lang = 'ja' AND tags = '{a,b}'"), vec![1]);
    assert_eq!(q("lang = 'zz' AND tags = '{a,b}'"), Vec::<u64>::new());
    assert_eq!(q("tags = '{a,b}' OR tags IS NULL"), vec![1, 3]);
}

#[test]
fn array_equality_survives_reopen_and_update() {
    let path = unique_db_path("array-eq-reopen");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx_for("alice");
    {
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema()).expect("create table");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        seed_equality_rows(&core, &alice);
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "UPDATE {TABLE} SET tags = '{{z,NULL}}' WHERE id = 1 USING OPERATION_ID 'upd-z'"
            ),
        )
        .expect("update");
    }
    let storage = Storage::open(&path).expect("reopen");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    assert_eq!(
        select_ids(
            &core,
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE tags = '{{z,NULL}}' LIMIT 100")
        ),
        vec![1]
    );
    assert_eq!(
        select_ids(
            &core,
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE tags = '{{a,b}}' LIMIT 100")
        ),
        Vec::<u64>::new()
    );
}

#[test]
fn array_equality_does_not_cross_tenants() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let bob = ctx_for("bob");
    seed_equality_rows(&core, &alice);
    let sql = format!("SELECT id FROM {TABLE} WHERE tags = '{{a,b}}' LIMIT 100");
    assert_eq!(select_ids(&core, &alice, &sql), vec![1]);
    assert!(select_ids(&core, &bob, &sql).is_empty());
    let count = core
        .execute_sql(
            &bob,
            &format!("SELECT COUNT(*) FROM {TABLE} WHERE tags = '{{a,b}}'"),
        )
        .expect("bob count");
    match &count.rows[0].cells[0] {
        Cell::Integer(0) => {}
        other => panic!("expected 0, got {other:?}"),
    }
}

/// DISTANCE 先行（`HINT ORDER(DISTANCE, SCALAR, RLS)`）で OR 群に式述語と配列等価が
/// 混在する場合、遅延評価が本物の配列値で判定すること（Issue #1193。空プレースホルダで
/// 評価すると全行が誤って一致・不一致になる fail-open の回帰）。
#[test]
fn distance_first_or_group_with_expr_and_array_equality_uses_real_array_values() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_equality_rows(&core, &alice);
    let q = |predicate: &str| {
        select_ids(
            &core,
            &alice,
            &format!(
                "SELECT id FROM {TABLE} WHERE (1 / (id - 1000)) > 1000000 OR {predicate} \
                 ORDER BY embedding <=> '[0.1,0.2]' LIMIT 100 \
                 HINT ORDER(DISTANCE, SCALAR, RLS)"
            ),
        )
    };
    assert_eq!(q("tags = '{a,b}'"), vec![1]);
    assert_eq!(q("tags = '{a,NULL}'"), vec![4, 5]);
    assert_eq!(q("tags IS NULL"), vec![3]);
    assert_eq!(q("tags IN ('{}','{NULL,a}')"), vec![2, 6]);
}

// --- Issue #1193: 要素型の拡大 -----------------------------------------------

const TYPED_TABLE: &str = "typed_docs";

fn typed_schema() -> TableSchema {
    let arr = |elem| ColumnType::Array(ArrayType::new(elem, 4).expect("array ty"));
    TableSchema::new(
        TYPED_TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("ints", arr(ArrayElemType::Integer), true),
            ColumnDef::new("bigs", arr(ArrayElemType::BigInt), true),
            ColumnDef::new("reals", arr(ArrayElemType::Real), true),
            ColumnDef::new("dbls", arr(ArrayElemType::Double), true),
            ColumnDef::new("days", arr(ArrayElemType::Date), true),
            ColumnDef::new("stamps", arr(ArrayElemType::Timestamp), true),
            ColumnDef::new("uuids", arr(ArrayElemType::Uuid), true),
        ],
    )
}

fn typed_core(name: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(name);
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&typed_schema()).expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

const TYPED_ROW: &str = "INSERT INTO typed_docs \
    (id, embedding, ints, bigs, reals, dbls, days, stamps, uuids) VALUES \
    (1, '[0.1,0.2]', '{1,NULL,-3}', '{9007199254740993,NULL}', '{1.5,-0,NULL}', '{2.25,NULL}', \
     '{1970-01-02,NULL}', '{\"1970-01-01 00:00:01\",NULL}', \
     '{00000000-0000-0000-0000-000000000001,NULL}') USING OPERATION_ID 'typed-1'";

#[test]
fn new_element_types_roundtrip_with_null_elements_and_match_by_equality() {
    use engine::row_codec::ArrayValue as V;
    let path = unique_db_path("array-typed-roundtrip");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx_for("alice");
    {
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&typed_schema()).expect("create table");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        core.execute_sql_in_session(&alice, &mut SessionState::default(), TYPED_ROW)
            .expect("typed insert");
    }
    let storage = Storage::open(&path).expect("reopen");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let result = core
        .execute_sql(
            &alice,
            "SELECT ints, bigs, reals, dbls, days, stamps, uuids FROM typed_docs WHERE id = 1 LIMIT 1",
        )
        .expect("select");
    let cells: Vec<&Cell> = result.rows[0].cells.iter().collect();
    let array = |i: usize| match cells[i] {
        Cell::Array(v) => v.clone(),
        other => panic!("expected array at {i}, got {other:?}"),
    };
    assert_eq!(array(0), V::Integer(vec![Some(1), None, Some(-3)]));
    assert_eq!(array(1), V::BigInt(vec![Some(9_007_199_254_740_993), None]));
    assert_eq!(array(2), V::Real(vec![Some(1.5), Some(0.0), None]));
    assert_eq!(array(3), V::Double(vec![Some(2.25), None]));
    assert_eq!(array(4), V::Date(vec![Some(1), None]));
    assert_eq!(array(5), V::Timestamp(vec![Some(1_000_000), None]));
    assert!(matches!(array(6), V::Uuid(items) if items.len() == 2 && items[1].is_none()));

    let q = |predicate: &str| {
        select_ids(
            &core,
            &alice,
            &format!("SELECT id FROM {TYPED_TABLE} WHERE {predicate} LIMIT 10"),
        )
    };
    assert_eq!(q("ints = '{1,NULL,-3}'"), vec![1]);
    assert_eq!(q("ints = '{1,-3}'"), Vec::<u64>::new());
    assert_eq!(q("bigs = '{9007199254740993,NULL}'"), vec![1]);
    // -0 は +0 へ正規化されるので等価。
    assert_eq!(q("reals = '{1.5,0,NULL}'"), vec![1]);
    assert_eq!(q("dbls = '{2.25,NULL}'"), vec![1]);
    assert_eq!(q("days = '{1970-01-02,NULL}'"), vec![1]);
    assert_eq!(q("stamps = '{\"1970-01-01 00:00:01\",NULL}'"), vec![1]);
    assert_eq!(
        q("uuids = '{00000000-0000-0000-0000-000000000001,NULL}'"),
        vec![1]
    );
    assert_eq!(q("ints IN ('{2}','{1,NULL,-3}')"), vec![1]);
    assert_eq!(q("ints IS NOT NULL AND stamps IS NOT NULL"), vec![1]);
}

#[test]
fn new_element_type_literal_errors_use_scalar_column_classes() {
    let (core, path) = typed_core("array-typed-errors");
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let attempt = |column: &str, literal: &str| {
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO {TYPED_TABLE} (id, embedding, {column}) \
                 VALUES (9, '[0.1,0.2]', '{literal}') USING OPERATION_ID 'err-{column}'"
            ),
        )
        .unwrap_err()
        .wire_code()
    };
    assert_eq!(attempt("ints", "{2147483648}"), "22003");
    assert_eq!(attempt("ints", "{1.5}"), "22P02");
    assert_eq!(attempt("days", "{2023-02-30}"), "22008");
    assert_eq!(attempt("days", "{nope}"), "22007");
    assert_eq!(attempt("uuids", "{zzz}"), "22P02");
    assert_eq!(attempt("ints", "{1,2,3,4,5}"), "54000");
    // WHERE の右辺も同じ分類。
    let err = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TYPED_TABLE} WHERE ints = '{{2147483648}}' LIMIT 1"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22003");
}

#[test]
fn new_element_type_rows_are_isolated_by_tenant() {
    let (core, path) = typed_core("array-typed-rls");
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let bob = ctx_for("bob");
    core.execute_sql_in_session(&alice, &mut SessionState::default(), TYPED_ROW)
        .expect("typed insert");
    let sql = format!("SELECT id FROM {TYPED_TABLE} WHERE ints = '{{1,NULL,-3}}' LIMIT 10");
    assert_eq!(select_ids(&core, &alice, &sql), vec![1]);
    assert!(select_ids(&core, &bob, &sql).is_empty());
}

#[test]
fn null_containing_array_unique_keys_do_not_collide() {
    let path = unique_db_path("array-unique-null");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx_for("alice");
    let storage = Storage::open(&path).expect("open storage");
    let schema = TableSchema::new(
        "uniq",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new(
                "tags",
                ColumnType::Array(ArrayType::new(ArrayElemType::Text, 4).expect("array ty")),
                true,
            ),
        ],
    );
    storage.create_table(&schema).expect("create table");
    storage
        .alter_table_add_unique_constraint("uniq", &["tags"])
        .expect("add UNIQUE constraint on tags");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let insert = |id: u64, tags: &str| {
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO uniq (id, embedding, tags) VALUES ({id}, '[0.1,0.2]', '{tags}') \
                 USING OPERATION_ID 'u-{id}'"
            ),
        )
    };
    insert(1, "{a,NULL}").expect("first");
    insert(2, "{NULL,a}").expect("NULL position differs");
    insert(3, "{a}").expect("no NULL element");
    insert(4, "{a,\"NULL\"}").expect("string NULL differs from NULL element");
    assert_eq!(insert(5, "{a,NULL}").unwrap_err().wire_code(), "23505");
}

#[test]
fn count_array_column_is_accepted_but_sum_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "{a}", "{t}", 1),
    )
    .expect("seed insert");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang) VALUES (2, '[0.1,0.2]', 'ja') \
             USING OPERATION_ID 'seed-2'"
        ),
    )
    .expect("seed insert without tags");

    let result = core
        .execute_sql(&alice, &format!("SELECT COUNT(tags) FROM {TABLE}"))
        .expect("COUNT(array column) should succeed");
    match &result.rows[0].cells[0] {
        Cell::Integer(1) => {}
        other => panic!("expected COUNT(tags) = 1, got {other:?}"),
    }

    let err = core
        .execute_sql(&alice, &format!("SELECT SUM(tags) FROM {TABLE}"))
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");
}
