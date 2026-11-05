//! スカラー `ORDER BY` のキーセット述語（Issue #2093。ポインタ: NOSQL-23・SQL-25・TASK-253。
//! 設計は `docs/design/scalar-order-by-scan.md` の「キーセットモード」節）の結合テスト。
//!
//! 束縛 API（`BoundScan::new(..).with_order_by(..).with_after(..)`）と
//! `EngineCore::execute_bound_scan_page_in_session` を production 経路として使い、
//! (1) 全ページの連結が `ORDER BY <sort>, <PK>` の一括取得と一致すること、(2) 複合 PK の
//! タイブレーク・PK なし表の rowid 順、(3) 最終ページの `next = None`、(4) ページ間の挿入・削除、
//! (5) テナント境界、(6) 併用指定の拒否を固定する。fixture は `sql25_offset` と同じ流儀
//! （`unique_db_path`／`CleanupGuard`。固定パス・環境変数・時間計測に依存しない）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::allowlist::ScalarOrderKey;
use engine::sql::exec::{QueryResult, ResultRow};
use engine::sql::keyset::{KeysetPosition, KeysetSealKey};
use engine::sql::mode::SessionState;
use engine::sql::parser::{BoundScan, ProjectedColumn};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// Private のみ可視（他テナントの Public を可視としない）。`sql::scan` の経路 (A) に入る文脈。
fn private_only(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Private]).expect("valid tenant")
}

fn new_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        CleanupGuard(path),
    )
}

fn exec(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> SqlOutcome {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(ctx, &mut s, sql)
        .unwrap_or_else(|e| panic!("{sql}: {} {e:?}", e.wire_code()))
}

fn insert(core: &EngineCore, ctx: &PolicyContext, sql: &str) {
    core.execute_insert_sql(ctx, sql)
        .unwrap_or_else(|e| panic!("{sql}: {} {e:?}", e.wire_code()));
}

fn query(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> QueryResult {
    match exec(core, ctx, sql) {
        SqlOutcome::Query(r) => r,
        other => panic!("expected Query for {sql}, got {other:?}"),
    }
}

fn cells_of(rows: &[ResultRow]) -> Vec<String> {
    rows.iter().map(|r| format!("{:?}", r.cells)).collect()
}

fn keys(spec: &[(&str, bool)]) -> Vec<ScalarOrderKey> {
    spec.iter()
        .map(|(c, d)| ScalarOrderKey {
            column: (*c).to_string(),
            descending: *d,
        })
        .collect()
}

fn projection(schema_cols: &[&str]) -> Vec<ProjectedColumn> {
    schema_cols
        .iter()
        .enumerate()
        .map(|(index, name)| ProjectedColumn::Column {
            index,
            name: (*name).to_string(),
        })
        .collect()
}

/// 1 ページ分を取得する。
fn page(
    core: &EngineCore,
    ctx: &PolicyContext,
    table: &str,
    cols: &[&str],
    sort: &[ScalarOrderKey],
    limit: usize,
    after: Option<KeysetPosition>,
) -> (Vec<ResultRow>, Option<KeysetPosition>) {
    let session = SessionState::default();
    let page = core
        .execute_bound_scan_page_in_session(ctx, &session, table, |schema, _| {
            BoundScan::new(
                table.to_string(),
                projection(cols),
                Vec::new(),
                Vec::new(),
                limit,
            )
            .with_order_by(sort, schema)?
            .with_after(after, schema)
        })
        .unwrap_or_else(|e| panic!("page: {} {e:?}", e.wire_code()));
    let (result, next) = page.into_parts();
    (result.rows, next)
}

/// 全ページを巡回して連結する（各ページの件数は `limit` 以下、最終ページ以外は `limit` ちょうど）。
fn page_all(
    core: &EngineCore,
    ctx: &PolicyContext,
    table: &str,
    cols: &[&str],
    sort: &[ScalarOrderKey],
    limit: usize,
) -> (Vec<ResultRow>, usize) {
    let mut all = Vec::new();
    let mut after = None;
    let mut pages = 0usize;
    loop {
        let (rows, next) = page(core, ctx, table, cols, sort, limit, after.take());
        pages += 1;
        assert!(rows.len() <= limit);
        if next.is_some() {
            assert_eq!(rows.len(), limit, "only the last page may be short");
        }
        all.extend(rows);
        match next {
            Some(n) => after = Some(n),
            None => return (all, pages),
        }
        assert!(pages < 1000, "paging must terminate");
    }
}

fn seed_scalar_table(core: &EngineCore, c: &PolicyContext) {
    exec(
        core,
        c,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, k INTEGER, s TEXT)",
    );
    // 重複値・NULL を含む。
    insert(
        core,
        c,
        "INSERT INTO t (id, k, s) VALUES (1, 5, 'a'), (2, NULL, 'b'), (3, 5, NULL), (4, 1, 'a'), \
         (5, 3, 'c'), (6, NULL, 'a'), (7, 5, 'b'), (8, 1, 'c')",
    );
    insert(
        core,
        c,
        "INSERT INTO t (id, k, s) VALUES (9, 3, 'a'), (10, 2, 'b'), (11, NULL, NULL), (12, 5, 'c')",
    );
}

const COLS: &[&str] = &["id", "k", "s"];

/// 昇順・降順・複数 `sort` で、全ページ連結が同じ順序の一括取得および OFFSET ページングと一致する。
#[test]
fn keyset_pages_match_order_by_and_offset_paging() {
    let (core, _g) = new_core("keyset-pages-match");
    let c = ctx("alice");
    seed_scalar_table(&core, &c);
    let cases: &[(&[(&str, bool)], &str)] = &[
        (&[("k", false)], "k, id"),
        (&[("k", true)], "k DESC, id"),
        (&[("k", false), ("s", true)], "k, s DESC, id"),
        (&[("s", true), ("k", true)], "s DESC, k DESC, id"),
        (&[], "id"),
    ];
    for (sort, order_sql) in cases {
        let oracle = query(
            &core,
            &c,
            &format!("SELECT id, k, s FROM t ORDER BY {order_sql} LIMIT 100"),
        );
        for limit in [1usize, 3, 4, 12, 50] {
            let (rows, _) = page_all(&core, &c, "t", COLS, &keys(sort), limit);
            assert_eq!(
                cells_of(&rows),
                cells_of(&oracle.rows),
                "sort={sort:?} limit={limit}"
            );
        }
        // OFFSET ページング（同順序）の全ページ連結とも一致する。
        let mut offset_all = Vec::new();
        let mut offset = 0usize;
        loop {
            let r = query(
                &core,
                &c,
                &format!("SELECT id, k, s FROM t ORDER BY {order_sql} LIMIT 5 OFFSET {offset}"),
            );
            if r.rows.is_empty() {
                break;
            }
            offset += r.rows.len();
            offset_all.extend(r.rows);
        }
        let (rows, _) = page_all(&core, &c, "t", COLS, &keys(sort), 5);
        assert_eq!(cells_of(&rows), cells_of(&offset_all), "sort={sort:?}");
    }
}

/// 件数が `limit` の倍数ちょうどでも最終ページの `next` は `None`（空ページを返さない）。
#[test]
fn last_page_has_no_next_even_when_count_is_a_multiple_of_limit() {
    let (core, _g) = new_core("keyset-last-page");
    let c = ctx("alice");
    seed_scalar_table(&core, &c);
    let (rows, pages) = page_all(&core, &c, "t", COLS, &keys(&[("k", false)]), 6);
    assert_eq!(rows.len(), 12);
    assert_eq!(pages, 2);
    let (rows, next) = page(&core, &c, "t", COLS, &keys(&[]), 12, None);
    assert_eq!(rows.len(), 12);
    assert!(next.is_none());
    let (rows, next) = page(&core, &c, "t", COLS, &keys(&[]), 11, None);
    assert_eq!(rows.len(), 11);
    assert!(next.is_some());
}

/// 複合 PK 表: `sort` が全行同値なら PK 列の辞書式（宣言順）で決まる。
#[test]
fn composite_primary_key_breaks_ties_in_declaration_order() {
    let (core, _g) = new_core("keyset-composite-pk");
    let c = ctx("alice");
    exec(
        &core,
        &c,
        "CREATE TABLE pk (a TEXT, b INTEGER, c INTEGER, PRIMARY KEY (a, b))",
    );
    insert(
        &core,
        &c,
        "INSERT INTO pk (a, b, c) VALUES ('y', 2, 0), ('x', 9, 0), ('y', 1, 0), ('x', 3, 0), \
         ('z', 1, 0), ('x', 1, 0)",
    );
    let oracle = query(
        &core,
        &c,
        "SELECT a, b, c FROM pk ORDER BY c, a, b LIMIT 100",
    );
    for limit in [1usize, 2, 4] {
        let (rows, _) = page_all(
            &core,
            &c,
            "pk",
            &["a", "b", "c"],
            &keys(&[("c", false)]),
            limit,
        );
        assert_eq!(cells_of(&rows), cells_of(&oracle.rows), "limit={limit}");
    }
    // sort 無し: PK 昇順のみ。
    let oracle = query(&core, &c, "SELECT a, b, c FROM pk ORDER BY a, b LIMIT 100");
    let (rows, _) = page_all(&core, &c, "pk", &["a", "b", "c"], &keys(&[]), 2);
    assert_eq!(cells_of(&rows), cells_of(&oracle.rows));
}

/// PK なしの採番方式の表: 同値の `sort` は物理 rowid（挿入順）で決まり、全件を重複・欠落なく巡回する。
/// 応答の行 id は 0 のまま（rowid を露出しない）。経路 (A)（Private のみ）・(B)（Public 可視）の両方。
#[test]
fn primary_key_less_table_orders_by_rowid_without_exposing_it() {
    let (core, _g) = new_core("keyset-pk-less");
    let c = ctx("alice");
    exec(&core, &c, "CREATE TABLE np (k INTEGER, v TEXT)");
    insert(
        &core,
        &c,
        "INSERT INTO np (k, v) VALUES (1, 'a'), (1, 'b'), (0, 'c'), (1, 'd'), (0, 'e'), (1, 'f'), (2, 'g')",
    );
    let expected_by_k = ["c", "e", "a", "b", "d", "f", "g"];
    let expected_none = ["a", "b", "c", "d", "e", "f", "g"];
    for context in [ctx("alice"), private_only("alice")] {
        for limit in [1usize, 2, 3, 7, 10] {
            let (rows, _) = page_all(
                &core,
                &context,
                "np",
                &["k", "v"],
                &keys(&[("k", false)]),
                limit,
            );
            let vs: Vec<String> = rows
                .iter()
                .map(|r| format!("{:?}", r.cells.get(1)))
                .collect();
            let expect: Vec<String> = expected_by_k
                .iter()
                .map(|v| {
                    format!(
                        "{:?}",
                        Some(&engine::sql::exec::Cell::Text((*v).to_string()))
                    )
                })
                .collect();
            assert_eq!(vs, expect, "limit={limit}");
            assert!(rows.iter().all(|r| r.id == 0), "rowid must not be exposed");

            let (rows, _) = page_all(&core, &context, "np", &["k", "v"], &keys(&[]), limit);
            let vs: Vec<String> = rows
                .iter()
                .map(|r| format!("{:?}", r.cells.get(1)))
                .collect();
            let expect: Vec<String> = expected_none
                .iter()
                .map(|v| {
                    format!(
                        "{:?}",
                        Some(&engine::sql::exec::Cell::Text((*v).to_string()))
                    )
                })
                .collect();
            assert_eq!(vs, expect, "no-sort limit={limit}");
        }
    }
}

/// ページ間の挿入・削除: 位置より後ろの新規行は出て、前の行は出ない。重複しない。
#[test]
fn rows_changed_between_pages_follow_keyset_semantics() {
    let (core, _g) = new_core("keyset-concurrent-change");
    let c = ctx("alice");
    exec(
        &core,
        &c,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, k INTEGER)",
    );
    insert(
        &core,
        &c,
        "INSERT INTO t (id, k) VALUES (10, 1), (20, 2), (30, 3), (40, 4), (50, 5)",
    );
    let cols = ["id", "k"];
    let sort = keys(&[("k", false)]);
    let (p1, next) = page(&core, &c, "t", &cols, &sort, 2, None);
    assert_eq!(p1.len(), 2);
    // 位置（k=2）より前へ挿入・後ろへ挿入・位置の行と未取得の行を削除。
    insert(
        &core,
        &c,
        "INSERT INTO t (id, k) VALUES (5, 0), (35, 3), (60, 9)",
    );
    exec(&core, &c, "DELETE FROM t WHERE id = 20");
    exec(&core, &c, "DELETE FROM t WHERE id = 40");
    let (p2, next2) = page(&core, &c, "t", &cols, &sort, 10, next);
    assert!(next2.is_none());
    let ids: Vec<String> = p2
        .iter()
        .map(|r| format!("{:?}", r.cells.first()))
        .collect();
    let expect: Vec<String> = [30i64, 35, 50, 60]
        .iter()
        .map(|i| format!("{:?}", Some(&engine::sql::exec::Cell::SignedInteger(*i))))
        .collect();
    assert_eq!(ids, expect);
}

/// RLS: 他テナントの行・Private 行は述語・件数・位置のいずれにも現れない。
#[test]
fn other_tenants_rows_never_affect_pages_or_positions() {
    let (core, _g) = new_core("keyset-rls");
    let alice = ctx("alice");
    let bob = ctx("bob");
    exec(
        &core,
        &alice,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, k INTEGER)",
    );
    insert(
        &core,
        &alice,
        "INSERT INTO t (id, k) VALUES (1, 1), (2, 1), (3, 2), (4, 2), (5, 3)",
    );
    insert(
        &core,
        &bob,
        "INSERT INTO t (id, k) VALUES (100, 1), (101, 1), (102, 2), (103, 2), (104, 3), (105, 3)",
    );
    let cols = ["id", "k"];
    let sort = keys(&[("k", false)]);
    for context in [ctx("alice"), private_only("alice")] {
        let (rows, pages) = page_all(&core, &context, "t", &cols, &sort, 2);
        assert_eq!(rows.len(), 5);
        assert_eq!(pages, 3);
        // bob が同時に書いても alice のページ境界は変わらない（件数・位置とも）。
    }
    let (_, next_a) = page(&core, &alice, "t", &cols, &sort, 2, None);
    let (_, next_b) = page(&core, &bob, "t", &cols, &sort, 2, None);
    // 位置の内容は封緘しても同一テナント内でしか意味を持たない（形は合うが別テナントの位置は別物）。
    let key = KeysetSealKey::from_bytes([3; 32]);
    let sealed = next_a
        .expect("alice has next")
        .seal(&key, b"alice")
        .expect("seal");
    assert!(KeysetPosition::open(&key, b"bob", &sealed).is_err());
    let _ = next_b.expect("bob has next");
}

/// 従来方式の表（疑似列 `id`）で他テナントの Public 行が可視な場合、`(sort, id)` がテナント間で
/// 同値になりうる。最終同点は `tenant_id` のバイト順で、重複・欠落なく巡回する。
#[test]
fn legacy_table_with_cross_tenant_public_rows_pages_without_gaps() {
    let path = unique_db_path("keyset-legacy-public");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let schema = TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    );
    storage.create_table(&schema).expect("create table");
    let mut n = 0u32;
    for (tenant, id, lang, vis) in [
        ("tenant-a", 1u64, "ja", Visibility::Public),
        ("tenant-b", 1, "ja", Visibility::Public),
        ("tenant-a", 2, "ja", Visibility::Public),
        ("tenant-b", 2, "ja", Visibility::Public),
        ("tenant-b", 3, "ja", Visibility::Private),
        ("tenant-a", 3, "en", Visibility::Public),
        ("tenant-b", 4, "en", Visibility::Public),
    ] {
        n += 1;
        let tctx = ctx(tenant);
        let op_id = engine::recovery::required_op_id::OperationId::parse(&format!("seed-{n}"))
            .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &tctx,
            id,
            vis,
            &[
                Value::Vector(vec![id as f32, 0.0]),
                Value::Text(lang.to_string()),
            ],
            &op_id,
        )
        .expect("insert row");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let c = ctx("tenant-a");
    let sort = keys(&[("lang", false)]);
    let cols = ["embedding", "lang"];
    // a の 3 行 ＋ b の Public 3 行 = 6 行（b の Private は不可視）。
    let (all, _) = page_all(&core, &c, "docs", &cols, &sort, 100);
    assert_eq!(all.len(), 6);
    for limit in [1usize, 2, 4] {
        let (paged, _) = page_all(&core, &c, "docs", &cols, &sort, limit);
        assert_eq!(cells_of(&paged), cells_of(&all), "limit={limit}");
    }
}

/// 併用拒否（多層防御。`42601`）と位置の形・型不一致（`22000`）。
#[test]
fn incompatible_options_and_mismatched_positions_are_rejected() {
    let (core, _g) = new_core("keyset-rejects");
    let c = ctx("alice");
    seed_scalar_table(&core, &c);
    let session = SessionState::default();
    let run = |build: &dyn Fn(
        &TableSchema,
    )
        -> Result<BoundScan, engine::sql::allowlist::SqlSurfaceError>| {
        core.execute_bound_scan_page_in_session(&c, &session, "t", |schema, _| build(schema))
            .map(|_| ())
    };
    let new = || BoundScan::new("t".to_string(), projection(COLS), Vec::new(), Vec::new(), 3);
    // OFFSET は with_after の後から設定されても実行時に拒否される。
    let e = run(&|s| Ok(new().with_after(None, s)?.with_offset(1))).expect_err("offset");
    assert_eq!(e.wire_code(), "42601");
    // with_after の前に OFFSET があれば with_after が拒否する。
    let e = run(&|s| new().with_offset(1).with_after(None, s)).expect_err("offset");
    assert_eq!(e.wire_code(), "42601");
    // with_after 後の with_order_by・二重 with_after。
    let e = run(&|s| {
        new()
            .with_after(None, s)?
            .with_order_by(&keys(&[("k", false)]), s)
    })
    .expect_err("order after");
    assert_eq!(e.wire_code(), "42601");
    let e = run(&|s| new().with_after(None, s)?.with_after(None, s)).expect_err("twice");
    assert_eq!(e.wire_code(), "42601");

    // 位置の形不一致: sort の数が異なる位置は別クエリの位置として拒否する。
    let (_, next) = page(&core, &c, "t", COLS, &keys(&[("k", false)]), 3, None);
    let pos = next.expect("next");
    let e = run(&|s| {
        new()
            .with_order_by(&keys(&[("k", false), ("s", false)]), s)?
            .with_after(Some(pos.clone()), s)
    })
    .expect_err("shape");
    assert_eq!(e.wire_code(), "22000");
    // 型不一致（k: INTEGER の位置を s: TEXT の sort へ）。
    let e = run(&|s| {
        new()
            .with_order_by(&keys(&[("s", false)]), s)?
            .with_after(Some(pos.clone()), s)
    })
    .expect_err("type");
    assert_eq!(e.wire_code(), "22000");
}

/// 封緘した位置で巡回しても結果は変わらない（`seal`→`open` の往復）。
#[test]
fn sealed_positions_round_trip_through_paging() {
    let (core, _g) = new_core("keyset-sealed");
    let c = ctx("alice");
    seed_scalar_table(&core, &c);
    let key = KeysetSealKey::from_bytes([9; 32]);
    let sort = keys(&[("k", true)]);
    let mut all = Vec::new();
    let mut after: Option<KeysetPosition> = None;
    loop {
        let (rows, next) = page(&core, &c, "t", COLS, &sort, 5, after.take());
        all.extend(rows);
        match next {
            Some(n) => {
                let bytes = n.seal(&key, b"ctx").expect("seal");
                after = Some(KeysetPosition::open(&key, b"ctx", &bytes).expect("open"));
            }
            None => break,
        }
    }
    let oracle = query(
        &core,
        &c,
        "SELECT id, k, s FROM t ORDER BY k DESC, id LIMIT 100",
    );
    assert_eq!(cells_of(&all), cells_of(&oracle.rows));
}

/// キーセットを使わない文（SQL テキスト・`with_after` なしの束縛）の結果は不変で、`next` は常に `None`。
#[test]
fn non_keyset_plans_are_unchanged_and_have_no_next() {
    let (core, _g) = new_core("keyset-unchanged");
    let c = ctx("alice");
    seed_scalar_table(&core, &c);
    let session = SessionState::default();
    let sort = keys(&[("k", false)]);
    let page = core
        .execute_bound_scan_page_in_session(&c, &session, "t", |schema, _| {
            BoundScan::new("t".to_string(), projection(COLS), Vec::new(), Vec::new(), 4)
                .with_order_by(&sort, schema)
        })
        .expect("page");
    let (result, next) = page.into_parts();
    assert!(next.is_none());
    let oracle = query(&core, &c, "SELECT id, k, s FROM t ORDER BY k, id LIMIT 4");
    assert_eq!(cells_of(&result.rows), cells_of(&oracle.rows));
}

/// `EXPLAIN` 経路はキーセット付きの計画も受理する（静的分類だけで、位置の値・rowid は出さない）。
#[test]
fn explain_accepts_keyset_plans_without_leaking_position_values() {
    let (core, _g) = new_core("keyset-explain");
    let c = ctx("alice");
    seed_scalar_table(&core, &c);
    let (_, next) = page(&core, &c, "t", COLS, &keys(&[("k", false)]), 3, None);
    let session = SessionState::default();
    let result = core
        .explain_bound_scan_in_session(&c, &session, "t", |schema, _| {
            BoundScan::new("t".to_string(), projection(COLS), Vec::new(), Vec::new(), 3)
                .with_order_by(&keys(&[("k", false)]), schema)?
                .with_after(next.clone(), schema)
        })
        .expect("explain accepts keyset plans");
    assert!(!result.rows.is_empty());
}
