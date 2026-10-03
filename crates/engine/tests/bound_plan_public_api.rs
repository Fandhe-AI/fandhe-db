//! 単一 `Storage` 上で SQL テキスト経由の実行（`EngineCore::execute_sql_in_session`）と
//! 束縛済み計画経由の実行（`EngineCore::execute_bound_scan_in_session`／
//! `execute_bound_aggregate_in_session`。TASK-186・NOSQL-3・NOSQL-4・NOSQL-5。
//! Issue #728）が混在しても結果が一致し、テナント境界（RLS-7・RLS-8）を
//! 破らないことを固定する結合テスト（Issue #729）。
//!
//! `tests/core_bound_plan_entry.rs` は束縛済みエントリ単体の契約（RLS 暗黙適用・
//! fail-closed・binder エラー伝播・テーブル不一致拒否）を固定する担当のままとし、
//! 本ファイルは「SQL 経路と束縛済み経路の結果一致」に専念する
//! （`docs/design/bound-plan-session-entry.md`「テスト」節参照）。
//!
//! `crates/wire-server/tests/wire_scan.rs`・`wire_aggregate.rs` はいずれも本 Issue の
//! production 変更対象外だが、束縛済みエントリが SQL 経路とキャッシュ配線
//! （`VisibleBitmapCache`／`SqlArenaCache`／`ScalarIndexCache`）を共有するようになった
//! ことが wire 経由の既存回帰を壊していないことは、本ファイルとは別に
//! `cargo test -p fandhe-vector-db-wire-server --test wire_scan --test wire_aggregate`
//! を実行して確認する（PR 本文に pass 件数を記録する）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::allowlist::{
    validate_sql, ScalarOrderKey, SqlSurfaceError, Statement, TableLookup, MAX_SCALAR_ORDER_KEYS,
};
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::parser::{bind_aggregate, bind_scan, BoundScan, ProjectedColumn};
use engine::sql::SqlOutcome;
use engine::storage::{RowInput, Storage, Visibility};

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

/// `core_bound_plan_entry.rs::seed_two_tenants` と同じ tenant-a Public 5 行
/// （id 1..=5・`lang` = `"ja"` 3 件・`"en"` 2 件）・tenant-b Private 3 行
/// （id 101..=103・`lang` = `"xx"`）を投入する。`Visibility::Public` はテナント
/// 非依存の全体公開（`PolicyContext::is_visible`）であるため、tenant-b 側に
/// 追加の `Public` 行は置かない（置くと「全テナントから見える」だけになり、
/// クロステナント交互実行の非漏えい検証対象にならない）。
/// `Visibility::Private` 行のみが `row_tenant == ctx.tenant_id()` の一致を要求する
/// ため、tenant-a からは tenant-b の Private 行（`lang` = `"xx"`）が一切見えない
/// 一方、tenant-b（`[Public, Private]` ctx）からは tenant-a の Public 行に加えて
/// 自分の Private 行も見える（RLS-7・RLS-8。
/// [`cross_tenant_interleaving_never_leaks_private_rows_through_shared_caches`]）。
fn seed_two_tenants(storage: &Storage) {
    storage.create_table(&schema()).expect("create table");
    let ctx_a = PolicyContext::with_visibilities("tenant-a", [Visibility::Public])
        .expect("valid tenant-a ctx");
    let ctx_b = ctx_b_full();

    let langs = ["ja", "ja", "ja", "en", "en"];
    for (idx, lang) in langs.iter().enumerate() {
        let id = idx as u64 + 1;
        let op_id =
            engine::recovery::required_op_id::OperationId::parse(&format!("bp-seed-a-{id}"))
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
            engine::recovery::required_op_id::OperationId::parse(&format!("bp-seed-b-priv-{id}"))
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
        .expect("insert tenant-b private row");
    }
}

fn ctx_public(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public]).expect("valid tenant ctx")
}

fn ctx_b_full() -> PolicyContext {
    PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
        .expect("valid tenant-b ctx")
}

/// tenant-a に対して `[Public, Private]` を許可した `PolicyContext`（tenant-b と
/// 同一の可視性権限集合）。tenant-a は自身の `Visibility::Private` 行を 1 件も
/// 持たないため、`row_tenant == ctx.tenant_id()` の tenant_id 照合が正しく効いて
/// いれば可視行数は `ctx_public("tenant-a")` と変わらない（5 行のまま）。この
/// 照合が欠落し可視性ラベルの集合一致だけで判定される退行が起きると、tenant-a
/// からも tenant-b の Private 行（id 101..=103・lang="xx"）が見えてしまう
/// （[`cross_tenant_interleaving_with_symmetric_visibility_never_leaks_private_rows`]。
/// レビュー指摘: RLS-7・RLS-8 のテナント境界検証では両コンテキストの可視性権限を
/// 揃え、可視性ラベルの一致だけで期待結果になってしまう構成を避ける）。
fn ctx_a_full() -> PolicyContext {
    PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
        .expect("valid tenant-a full ctx")
}

fn open_engine_core(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    seed_two_tenants(&storage);
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

/// [`core_bound_plan_entry.rs::FixedTableLookup`] と同じ判断（`validate_sql` が
/// `bind_scan`／`bind_aggregate` 用の `Statement` を得るためだけに要求する
/// `TableLookup`。束縛・実行そのものは `EngineCore` 側のスキーマ・redb 走査で
/// 行うため、テーブル名一致のみを見る最小実装で足りる）。
struct FixedTableLookup;

impl TableLookup for FixedTableLookup {
    fn table_exists(&self, name: &str) -> Result<bool, SqlSurfaceError> {
        Ok(name == TABLE)
    }
}

/// SQL テキスト経由（`execute_sql_in_session`）で `sql` を実行し `QueryResult` を得る。
/// `SqlOutcome::Query` 以外（`INSERT` 応答等）が返った場合は本ファイルの対象外の
/// クエリ形が混入したことを意味するため panic させる。
fn run_sql(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> QueryResult {
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(ctx, &mut session, sql)
        .unwrap_or_else(|err| panic!("execute_sql_in_session({sql:?}) failed: {err:?}"));
    match outcome {
        SqlOutcome::Query(result) => result,
        other => panic!("expected SqlOutcome::Query for {sql:?}, got {other:?}"),
    }
}

/// 束縛済み scan 計画経由（`execute_bound_scan_in_session`）で `sql` を実行する。
/// `validate_sql` → `Statement::Scan` → `bind_scan` は closure 内で行い、SQL 経由と
/// 同じテキストから束縛することで「入口の違い」だけが結果差の要因になるようにする。
fn run_bound_scan(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> QueryResult {
    let session = SessionState::default();
    core.execute_bound_scan_in_session(ctx, &session, TABLE, |schema, udfs| {
        let validated = validate_sql(sql, &FixedTableLookup)?;
        let Statement::Scan(validated_scan) = validated else {
            panic!("expected Statement::Scan for {sql:?}");
        };
        bind_scan(&validated_scan, schema, udfs)
    })
    .unwrap_or_else(|err| panic!("execute_bound_scan_in_session({sql:?}) failed: {err:?}"))
}

/// 束縛済み aggregate 計画経由（`execute_bound_aggregate_in_session`）で `sql` を実行する。
/// `run_bound_scan` と同じ設計（`GROUP BY` の有無を問わず `bind_aggregate` に委ねる）。
fn run_bound_aggregate(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> QueryResult {
    let session = SessionState::default();
    core.execute_bound_aggregate_in_session(ctx, &session, TABLE, |schema, udfs| {
        let validated = validate_sql(sql, &FixedTableLookup)?;
        let Statement::Aggregate(validated_aggregate) = validated else {
            panic!("expected Statement::Aggregate for {sql:?}");
        };
        bind_aggregate(&validated_aggregate, schema, udfs)
    })
    .unwrap_or_else(|err| panic!("execute_bound_aggregate_in_session({sql:?}) failed: {err:?}"))
}

/// scan（SQL-15 相当）は順序保証がない契約（`docs/design/wide-retrieval-scan.md`）
/// のため、`id` 昇順に並べ替えてから比較する。`score` は scan 経路では常に `0.0`
/// （`sql/scan.rs`）なので `columns`／`rows` の完全一致がそのまま「同じ集合」の
/// 確認になる。
fn sorted_by_id(mut result: QueryResult) -> QueryResult {
    result.rows.sort_by_key(|row| row.id);
    result
}

fn row_id(cell: &Cell) -> u64 {
    match cell {
        Cell::Integer(id) => *id,
        other => panic!("expected Cell::Integer for id, got {other:?}"),
    }
}

fn insert_extra_tenant_a_row(core: &EngineCore, ctx: &PolicyContext, id: u64, lang: &str) {
    let metadata = engine::row_codec::encode_scalar_columns(
        &schema(),
        &[Value::Null, Value::Text(lang.to_string())],
    )
    .expect("encode scalar columns");
    let op_id =
        engine::recovery::required_op_id::OperationId::parse(&format!("bp-mixed-extra-{id}"))
            .expect("valid operation_id");
    core.insert_row(
        ctx,
        TABLE,
        id,
        &RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Public,
            embedding: &[id as f32, 0.0, 0.0, 0.0],
            metadata: &metadata,
        },
        Some(&op_id),
    )
    .expect("insert extra tenant-a row");
}

// --- T1: scan の SQL 経由・束縛済み経由が同一行集合を返す -----------------------

#[test]
fn scan_sql_and_bound_paths_return_identical_rows_sorted_by_id() {
    let path = unique_db_path("bound-plan-mixed-scan-basic");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_public("tenant-a");

    let cases = [
        "SELECT id, lang FROM docs LIMIT 10",
        "SELECT id, lang FROM docs WHERE lang = 'ja' LIMIT 10",
        "SELECT * FROM docs LIMIT 10",
    ];
    let expected_counts = [5usize, 3, 5];

    for (sql, &expected_count) in cases.iter().zip(expected_counts.iter()) {
        let sql_result = sorted_by_id(run_sql(&core, &ctx_a, sql));
        let bound_result = sorted_by_id(run_bound_scan(&core, &ctx_a, sql));

        assert_eq!(
            sql_result.columns, bound_result.columns,
            "columns mismatch for {sql:?}"
        );
        assert_eq!(
            sql_result.rows.len(),
            expected_count,
            "row count for {sql:?}"
        );
        assert_eq!(
            sql_result.rows, bound_result.rows,
            "rows mismatch for {sql:?}"
        );

        // tenant-b の Private 行（id 101..=103）は一切現れない。
        for row in &sql_result.rows {
            let id = row_id(&row.cells[0]);
            assert!(
                !(101..=103).contains(&id),
                "tenant-b row id {id} leaked into tenant-a scan result for {sql:?}"
            );
        }
    }
}

// --- T2: LIMIT が可視行数を下回る場合、順序保証なしでも件数・可視範囲は一致 --------

#[test]
fn scan_limit_below_visible_count_yields_same_count_and_subset_on_both_paths() {
    let path = unique_db_path("bound-plan-mixed-scan-limit");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_public("tenant-a");
    let sql = "SELECT id FROM docs LIMIT 2";

    let sql_result = run_sql(&core, &ctx_a, sql);
    let bound_result = run_bound_scan(&core, &ctx_a, sql);

    // scan は順序保証なし（SQL-15）のため行集合の一致までは要求しない。
    // 件数と、返る id が可視範囲（tenant-a Public: 1..=5）内であることのみ固定する。
    assert_eq!(sql_result.rows.len(), 2, "SQL path row count");
    assert_eq!(bound_result.rows.len(), 2, "bound path row count");
    for result in [&sql_result, &bound_result] {
        for row in &result.rows {
            let id = row_id(&row.cells[0]);
            assert!(
                (1..=5).contains(&id),
                "id {id} out of tenant-a visible range"
            );
        }
    }
}

// --- T3: 集計（GROUP BY なし）の SQL 経由・束縛済み経由が固定オラクルと一致 --------

#[test]
fn aggregate_sql_and_bound_paths_match_for_multi_function_with_where() {
    let path = unique_db_path("bound-plan-mixed-aggregate-basic");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_public("tenant-a");

    let sql_all =
        "SELECT COUNT(*) AS n, COUNT(lang) AS c, SUM(id) AS s, AVG(id) AS a, MIN(id) AS lo, \
         MAX(id) AS hi, MIN(lang) AS lmin, MAX(lang) AS lmax FROM docs";
    let sql_result = run_sql(&core, &ctx_a, sql_all);
    let bound_result = run_bound_aggregate(&core, &ctx_a, sql_all);
    assert_eq!(sql_result.columns, bound_result.columns);
    assert_eq!(sql_result.rows, bound_result.rows);

    // 固定オラクル（両経路が「同じ間違った値」で一致する vacuous な pass を防ぐ）。
    let cells = &sql_result.rows[0].cells;
    assert_eq!(cells[0], Cell::Integer(5), "n");
    assert_eq!(cells[1], Cell::Integer(5), "c");
    assert_eq!(cells[2], Cell::Integer(15), "s");
    assert_eq!(cells[3], Cell::Float(3.0), "a");
    assert_eq!(cells[4], Cell::Integer(1), "lo");
    assert_eq!(cells[5], Cell::Integer(5), "hi");
    assert_eq!(cells[6], Cell::Text("en".to_string()), "lmin");
    assert_eq!(cells[7], Cell::Text("ja".to_string()), "lmax");

    let sql_where = "SELECT COUNT(*) AS n, COUNT(lang) AS c, SUM(id) AS s, AVG(id) AS a, \
                      MIN(id) AS lo, MAX(id) AS hi, MIN(lang) AS lmin, MAX(lang) AS lmax \
                      FROM docs WHERE lang = 'ja'";
    let sql_result_where = run_sql(&core, &ctx_a, sql_where);
    let bound_result_where = run_bound_aggregate(&core, &ctx_a, sql_where);
    assert_eq!(sql_result_where.rows, bound_result_where.rows);
    let cells_where = &sql_result_where.rows[0].cells;
    assert_eq!(cells_where[0], Cell::Integer(3), "n (WHERE lang='ja')");
    assert_eq!(cells_where[2], Cell::Integer(6), "s (WHERE lang='ja')");

    // UDF（`vec_norm`）を経由する式項目も両経路で一致する。
    // tenant-a の embedding は [id, 0, 0, 0] のため vec_norm = id、SUM = 1+2+3+4+5 = 15。
    let sql_vec_norm = "SELECT SUM(vec_norm(embedding)) AS s FROM docs";
    let sql_result_vn = run_sql(&core, &ctx_a, sql_vec_norm);
    let bound_result_vn = run_bound_aggregate(&core, &ctx_a, sql_vec_norm);
    assert_eq!(sql_result_vn.rows, bound_result_vn.rows);
    assert_eq!(sql_result_vn.rows[0].cells[0], Cell::Float(15.0));
}

// --- T4: GROUP BY／HAVING／ORDER BY LIMIT が順序込みで一致 ------------------------

#[test]
fn group_by_having_order_by_limit_match_in_order_on_both_paths() {
    let path = unique_db_path("bound-plan-mixed-group-by");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_public("tenant-a");

    let sql_base = "SELECT lang, COUNT(*) AS n, SUM(id) AS s FROM docs GROUP BY lang";
    let sql_result = run_sql(&core, &ctx_a, sql_base);
    let bound_result = run_bound_aggregate(&core, &ctx_a, sql_base);
    assert_eq!(sql_result.rows, bound_result.rows);
    // 固定オラクル: 既定キー昇順（"en" < "ja"）。en:(n=2,s=9)、ja:(n=3,s=6)。
    assert_eq!(
        sql_result.rows,
        vec![
            engine::sql::exec::ResultRow {
                id: 0,
                score: 0.0,
                cells: vec![
                    Cell::Text("en".to_string()),
                    Cell::Integer(2),
                    Cell::Integer(9)
                ],
            },
            engine::sql::exec::ResultRow {
                id: 0,
                score: 0.0,
                cells: vec![
                    Cell::Text("ja".to_string()),
                    Cell::Integer(3),
                    Cell::Integer(6)
                ],
            },
        ],
        "GROUP BY base ordering/oracle"
    );

    let sql_having =
        "SELECT lang, COUNT(*) AS n, SUM(id) AS s FROM docs GROUP BY lang HAVING n >= 3";
    let sql_result_having = run_sql(&core, &ctx_a, sql_having);
    let bound_result_having = run_bound_aggregate(&core, &ctx_a, sql_having);
    assert_eq!(sql_result_having.rows, bound_result_having.rows);
    assert_eq!(
        sql_result_having.rows.len(),
        1,
        "HAVING n >= 3 keeps only ja"
    );
    assert_eq!(
        sql_result_having.rows[0].cells[0],
        Cell::Text("ja".to_string())
    );

    let sql_order_limit =
        "SELECT lang, COUNT(*) AS n, SUM(id) AS s FROM docs GROUP BY lang ORDER BY n DESC LIMIT 1";
    let sql_result_ol = run_sql(&core, &ctx_a, sql_order_limit);
    let bound_result_ol = run_bound_aggregate(&core, &ctx_a, sql_order_limit);
    assert_eq!(sql_result_ol.rows, bound_result_ol.rows);
    assert_eq!(sql_result_ol.rows.len(), 1);
    assert_eq!(sql_result_ol.rows[0].cells[0], Cell::Text("ja".to_string()));
}

// --- T5: 世代進行（INSERT）を挟んだ前後で SQL 経由・束縛済み経由が両方とも新しい値になる ---

#[test]
fn interleaved_paths_stay_consistent_across_generation_bump() {
    let path = unique_db_path("bound-plan-mixed-generation-bump");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_public("tenant-a");
    let count_sql = "SELECT COUNT(*) AS n FROM docs";
    let group_sql = "SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang";

    // 追加前: SQL → bound → SQL → bound の順に交互実行し、いずれも 5 行・
    // ja グループ 3 件であることを確認する（`VisibleBitmapCache` を 2 回目以降
    // ヒットさせるため、意図的に同一クエリを複数回発行する）。
    for _ in 0..2 {
        let sql_result = run_sql(&core, &ctx_a, count_sql);
        let bound_result = run_bound_aggregate(&core, &ctx_a, count_sql);
        assert_eq!(sql_result.rows, bound_result.rows);
        assert_eq!(sql_result.rows[0].cells[0], Cell::Integer(5));
    }
    let stats_before = core.visible_bitmap_cache_stats();
    assert!(
        stats_before.hits >= 1,
        "expected VisibleBitmapCache hit before the generation bump, got {stats_before:?}"
    );

    insert_extra_tenant_a_row(&core, &ctx_a, 6, "ja");

    // 追加後: 両経路とも新しい世代（6 行・ja グループ 4 件）を即座に反映する
    // （`VisibleBitmapCache`／`SqlArenaCache` 等のテーブル単位世代整合キャッシュが
    // SQL 経路・束縛済み経路の双方へ同時に適用される契約）。
    let sql_count_after = run_sql(&core, &ctx_a, count_sql);
    let bound_count_after = run_bound_aggregate(&core, &ctx_a, count_sql);
    assert_eq!(sql_count_after.rows, bound_count_after.rows);
    assert_eq!(sql_count_after.rows[0].cells[0], Cell::Integer(6));

    let sql_group_after = run_sql(&core, &ctx_a, group_sql);
    let bound_group_after = run_bound_aggregate(&core, &ctx_a, group_sql);
    assert_eq!(sql_group_after.rows, bound_group_after.rows);
    let ja_row = sql_group_after
        .rows
        .iter()
        .find(|row| row.cells[0] == Cell::Text("ja".to_string()))
        .expect("ja group present after insert");
    assert_eq!(ja_row.cells[1], Cell::Integer(4));

    let sql_scan_after = sorted_by_id(run_sql(&core, &ctx_a, "SELECT id FROM docs LIMIT 10"));
    let bound_scan_after = sorted_by_id(run_bound_scan(
        &core,
        &ctx_a,
        "SELECT id FROM docs LIMIT 10",
    ));
    assert_eq!(sql_scan_after.rows, bound_scan_after.rows);
    assert_eq!(sql_scan_after.rows.len(), 6);
}

// --- T6: テナント交互（一方は束縛経路・他方は SQL 経路、その逆も）で他テナント非漏えい ---

#[test]
fn cross_tenant_interleaving_never_leaks_private_rows_through_shared_caches() {
    let path = unique_db_path("bound-plan-mixed-cross-tenant");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_public("tenant-a");
    let ctx_b = ctx_b_full();
    let count_sql = "SELECT COUNT(*) AS n FROM docs";
    let group_sql = "SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang";
    let scan_sql = "SELECT id, lang FROM docs LIMIT 10";

    let assert_tenant_a_result = |result: &QueryResult, label: &str| {
        for row in &result.rows {
            // `group_sql` の形（`cells[0]` が `lang` の `Cell::Text`）はここで
            // 直接検査できるが、`scan_sql`（`SELECT id, lang FROM docs LIMIT 10`）
            // は `cells[0]` が `id` の `Cell::Integer` になるため、`Cell::Text`
            // ガードだけでは scan 形状の非漏えい検査が一度も実行されない
            // （vacuous）。scan 形状は tenant-b の Private 行 id（101..=103）が
            // 混入していないことを直接検査することで、形状によらず非漏えいを
            // 固定する（レビュー指摘）。
            assert!(
                !(101..=103).contains(&row.id),
                "tenant-a {label} leaked tenant-b-only row id {}",
                row.id
            );
            if let Some(Cell::Text(lang)) =
                row.cells.iter().find(|cell| matches!(cell, Cell::Text(_)))
            {
                assert_ne!(lang, "xx", "tenant-a {label} leaked tenant-b-only group");
            }
        }
    };
    let assert_tenant_b_result = |result: &QueryResult, label: &str| {
        // tenant-b（`[Public, Private]` ctx）は tenant-a の Public 行（グローバル
        // 公開）に加え、自分の Private 行（`lang` = `"xx"`）も見えてよい。
        // 非空確認だけでは、tenant-a の結果（Public 5 行のみ）を誤って再利用
        // しても成立してしまう（レビュー指摘）。scan・GROUP BY はそれぞれの
        // 形状で「自分の Private データが実際に含まれる」ことを直接固定する。
        assert!(
            !result.rows.is_empty(),
            "tenant-b {label} unexpectedly empty"
        );
        if label == scan_sql {
            // scan_sql（`SELECT id, lang FROM docs LIMIT 10`）は `cells[0]` が
            // `id`（`Cell::Integer`）。自分の Private 行 id（101..=103）が
            // 全件含まれることを確認する。
            let seen_ids: std::collections::BTreeSet<u64> =
                result.rows.iter().map(|row| row.id).collect();
            for id in 101..=103u64 {
                assert!(
                    seen_ids.contains(&id),
                    "tenant-b {label} missing own private row id {id}"
                );
            }
        } else if label == group_sql {
            // group_sql（`SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang`）は
            // `cells[0]` が `lang`。`xx` グループの件数が 3（自分の Private 行数）
            // であることを直接固定する。
            let xx_row = result
                .rows
                .iter()
                .find(|row| row.cells[0] == Cell::Text("xx".to_string()))
                .unwrap_or_else(|| panic!("tenant-b {label} missing xx group"));
            assert_eq!(
                xx_row.cells[1],
                Cell::Integer(3),
                "tenant-b {label} xx group count"
            );
        }
    };

    // ラウンド 1: tenant-a は束縛経路、tenant-b は SQL 経路。
    for sql in [count_sql, group_sql, scan_sql] {
        let a_result = run_bound_via(&core, &ctx_a, sql);
        let b_result = run_sql(&core, &ctx_b, sql);
        assert_tenant_a_result(&a_result, sql);
        assert_tenant_b_result(&b_result, sql);
    }

    // ラウンド 2: tenant-a は SQL 経路、tenant-b は束縛経路（経路を反転）。
    for sql in [count_sql, group_sql, scan_sql] {
        let a_result = run_sql(&core, &ctx_a, sql);
        let b_result = run_bound_via(&core, &ctx_b, sql);
        assert_tenant_a_result(&a_result, sql);
        assert_tenant_b_result(&b_result, sql);
    }

    // 両テナントの COUNT(*) は経路によらず一致する。tenant-a は自分の Public
    // 5 行のみ（tenant-b の Private 行は非漏えい）、tenant-b は tenant-a の
    // Public 5 行 + 自分の Private 3 行 = 8 行（Visibility::Public の
    // グローバル公開契約どおり。非 vacuous な「同じ間違った値」を防ぐため
    // 固定オラクルとして両方アサートする）。
    let a_count_sql = run_sql(&core, &ctx_a, count_sql);
    let a_count_bound = run_bound_via(&core, &ctx_a, count_sql);
    assert_eq!(a_count_sql.rows, a_count_bound.rows);
    assert_eq!(a_count_sql.rows[0].cells[0], Cell::Integer(5));

    let b_count_sql = run_sql(&core, &ctx_b, count_sql);
    let b_count_bound = run_bound_via(&core, &ctx_b, count_sql);
    assert_eq!(b_count_sql.rows, b_count_bound.rows);
    assert_eq!(b_count_sql.rows[0].cells[0], Cell::Integer(8));

    // キャッシュが実際にヒットした状態（非 vacuous）で非漏えいを確認したことを示す。
    let stats = core.visible_bitmap_cache_stats();
    assert!(
        stats.hits >= 1,
        "expected VisibleBitmapCache hit during cross-tenant interleaving, got {stats:?}"
    );
}

// --- T7: 同一の可視性権限（[Public, Private]）を両テナントに許可しても分離される ---

/// [`cross_tenant_interleaving_never_leaks_private_rows_through_shared_caches`]
/// は tenant-a が Public のみ・tenant-b が `[Public, Private]` という非対称な
/// 権限構成のため、テナント ID の照合が欠落し可視性ラベルの集合一致だけで
/// 判定される退行があっても検出できない（レビュー指摘）。本テストは両
/// コンテキストに `[Public, Private]` を許可し、tenant_id 照合が実際に効いて
/// いることを確認する（RLS-7・RLS-8）。
#[test]
fn cross_tenant_interleaving_with_symmetric_visibility_never_leaks_private_rows() {
    let path = unique_db_path("bound-plan-mixed-cross-tenant-symmetric");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_a_full();
    let ctx_b = ctx_b_full();
    let count_sql = "SELECT COUNT(*) AS n FROM docs";
    let group_sql = "SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang";
    let scan_sql = "SELECT id, lang FROM docs LIMIT 10";

    // tenant-a（`[Public, Private]` ctx。ただし自身の Private 行は 0 件）から
    // tenant-b の Private 行（id 101..=103・lang="xx"）が一切見えないことを、
    // scan（`row.id` 直接検査）・GROUP BY（`lang` セル検査）の両形状で固定する。
    let assert_no_tenant_b_private_leak = |result: &QueryResult, label: &str| {
        for row in &result.rows {
            assert!(
                !(101..=103).contains(&row.id),
                "tenant-a (symmetric visibility) {label} leaked tenant-b private row id {}",
                row.id
            );
            if let Some(Cell::Text(lang)) =
                row.cells.iter().find(|cell| matches!(cell, Cell::Text(_)))
            {
                assert_ne!(
                    lang, "xx",
                    "tenant-a (symmetric visibility) {label} leaked tenant-b private group"
                );
            }
        }
    };
    // tenant-b 側は自分の Private データが実際に見えることを T6 と同じ形で確認する。
    let assert_tenant_b_own_private_data = |result: &QueryResult, label: &str| {
        assert!(
            !result.rows.is_empty(),
            "tenant-b {label} unexpectedly empty"
        );
        if label == scan_sql {
            let seen_ids: std::collections::BTreeSet<u64> =
                result.rows.iter().map(|row| row.id).collect();
            for id in 101..=103u64 {
                assert!(
                    seen_ids.contains(&id),
                    "tenant-b {label} missing own private row id {id}"
                );
            }
        } else if label == group_sql {
            let xx_row = result
                .rows
                .iter()
                .find(|row| row.cells[0] == Cell::Text("xx".to_string()))
                .unwrap_or_else(|| panic!("tenant-b {label} missing xx group"));
            assert_eq!(
                xx_row.cells[1],
                Cell::Integer(3),
                "tenant-b {label} xx group count"
            );
        }
    };

    // ラウンド 1: tenant-a は束縛経路、tenant-b は SQL 経路。
    for sql in [count_sql, group_sql, scan_sql] {
        let a_result = run_bound_via(&core, &ctx_a, sql);
        let b_result = run_sql(&core, &ctx_b, sql);
        assert_no_tenant_b_private_leak(&a_result, sql);
        assert_tenant_b_own_private_data(&b_result, sql);
    }
    // ラウンド 2: tenant-a は SQL 経路、tenant-b は束縛経路（経路を反転）。
    for sql in [count_sql, group_sql, scan_sql] {
        let a_result = run_sql(&core, &ctx_a, sql);
        let b_result = run_bound_via(&core, &ctx_b, sql);
        assert_no_tenant_b_private_leak(&a_result, sql);
        assert_tenant_b_own_private_data(&b_result, sql);
    }

    // 固定オラクル: tenant-a は Private 権限を得ても自身の Private 行を
    // 持たないため可視行数は 5 のまま変わらない（tenant_id 照合が正しく
    // 効いている証拠。可視性ラベルの集合一致だけで判定される退行が起きると
    // ここが 8〔tenant-a Public 5 + tenant-b Private 3〕へ膨れる）。
    let a_count_sql = run_sql(&core, &ctx_a, count_sql);
    let a_count_bound = run_bound_via(&core, &ctx_a, count_sql);
    assert_eq!(a_count_sql.rows, a_count_bound.rows);
    assert_eq!(a_count_sql.rows[0].cells[0], Cell::Integer(5));
}

/// `SELECT lang, ...` 形と `SELECT id, ...` 形の両方を単一ヘルパで扱うため、
/// クエリ文字列の先頭トークン（`SELECT ... FROM docs GROUP BY` の有無ではなく
/// 具体的な SQL 種別）を見ず、`validate_sql` の分類結果（`Statement::Scan` /
/// `Statement::Aggregate`）で分岐する（[`cross_tenant_interleaving_never_leaks_private_rows_through_shared_caches`]
/// が scan・aggregate 両方の SQL 文字列を同一ループで扱うための橋渡し）。
fn run_bound_via(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> QueryResult {
    let validated =
        validate_sql(sql, &FixedTableLookup).unwrap_or_else(|err| panic!("validate_sql: {err:?}"));
    match validated {
        Statement::Scan(_) => run_bound_scan(core, ctx, sql),
        Statement::Aggregate(_) => run_bound_aggregate(core, ctx, sql),
        other => panic!("unexpected statement kind for {sql:?}: {other:?}"),
    }
}

// --- T8: 複数スレッドから `Arc<EngineCore>` を共有して SQL・束縛済み経路を交互実行 ---

#[test]
fn concurrent_mixed_paths_over_shared_engine_core_match_reference() {
    let path = unique_db_path("bound-plan-mixed-concurrent");
    let _guard = CleanupGuard(path.clone());
    let core = std::sync::Arc::new(open_engine_core(&path));
    let ctx_a = ctx_public("tenant-a");

    let scan_sql = "SELECT id FROM docs LIMIT 10";
    let count_sql = "SELECT COUNT(*) AS n FROM docs";
    let group_sql = "SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang";

    // 単一スレッドで先に取得した参照結果（scan は id ソート後）。
    let reference_scan = sorted_by_id(run_sql(&core, &ctx_a, scan_sql));
    let reference_count = run_sql(&core, &ctx_a, count_sql);
    let reference_group = run_sql(&core, &ctx_a, group_sql);

    const THREADS: usize = 4;
    const ITERATIONS: usize = 20;
    std::thread::scope(|scope| {
        for thread_idx in 0..THREADS {
            let core = &core;
            let ctx_a = &ctx_a;
            let reference_scan = &reference_scan;
            let reference_count = &reference_count;
            let reference_group = &reference_group;
            scope.spawn(move || {
                for iteration in 0..ITERATIONS {
                    // スレッド・反復ごとに SQL 経路／束縛済み経路を交互に選ぶ。
                    let use_bound = (thread_idx + iteration) % 2 == 0;
                    let scan_result = if use_bound {
                        sorted_by_id(run_bound_scan(core, ctx_a, scan_sql))
                    } else {
                        sorted_by_id(run_sql(core, ctx_a, scan_sql))
                    };
                    assert_eq!(
                        &scan_result, reference_scan,
                        "thread {thread_idx} iter {iteration} scan"
                    );

                    let count_result = if use_bound {
                        run_bound_aggregate(core, ctx_a, count_sql)
                    } else {
                        run_sql(core, ctx_a, count_sql)
                    };
                    assert_eq!(
                        &count_result, reference_count,
                        "thread {thread_idx} iter {iteration} count"
                    );

                    let group_result = if use_bound {
                        run_bound_aggregate(core, ctx_a, group_sql)
                    } else {
                        run_sql(core, ctx_a, group_sql)
                    };
                    assert_eq!(
                        &group_result, reference_group,
                        "thread {thread_idx} iter {iteration} group"
                    );
                }
            });
        }
    });
}

// --- T9: 同一の拒否入力に対して SQL 経路・束縛済み経路の wire_code 分類が一致する ---

#[test]
fn bound_and_sql_paths_share_error_classification_for_same_invalid_input() {
    let path = unique_db_path("bound-plan-mixed-error-classification");
    let _guard = CleanupGuard(path.clone());
    let core = open_engine_core(&path);
    let ctx_a = ctx_public("tenant-a");

    // VECTOR 列への直接集計は `42883`（undefined_function。Issue #1349）で拒否される
    // （`tests/sql_aggregate.rs` の既存契約と同一）。
    let sql_vector_sum = "SELECT SUM(embedding) FROM docs";
    let mut session = SessionState::default();
    let sql_err = core
        .execute_sql_in_session(&ctx_a, &mut session, sql_vector_sum)
        .expect_err("SUM(embedding) should be rejected on the SQL path");
    let bound_session = SessionState::default();
    let bound_err = core
        .execute_bound_aggregate_in_session(&ctx_a, &bound_session, TABLE, |schema, udfs| {
            let validated = validate_sql(sql_vector_sum, &FixedTableLookup)?;
            let Statement::Aggregate(validated_aggregate) = validated else {
                panic!("expected Statement::Aggregate");
            };
            bind_aggregate(&validated_aggregate, schema, udfs)
        })
        .expect_err("SUM(embedding) should be rejected on the bound path");
    // 両経路の `wire_code` が一致するだけでなく、その一致先が実際に契約どおりの
    // `42883`（undefined_function）であることを固定オラクルとして検査する。一致検査
    // のみでは両経路が「同じ間違ったコード」で一致する vacuous pass を防げない
    // （レビュー指摘）。
    assert_eq!(sql_err.wire_code(), "42883", "sql_err={sql_err:?}");
    assert_eq!(bound_err.wire_code(), "42883", "bound_err={bound_err:?}");
    assert_eq!(
        sql_err.wire_code(),
        bound_err.wire_code(),
        "SUM(embedding) wire_code mismatch: sql={sql_err:?} bound={bound_err:?}"
    );

    // `GROUP BY` キー列を HAVING で直接比較する形は `22000`（invalid input）
    // で拒否される（`tests/sql_group_by.rs` の既存契約と同一）。
    let sql_having_key = "SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang HAVING lang > 1";
    let mut session2 = SessionState::default();
    let sql_err2 = core
        .execute_sql_in_session(&ctx_a, &mut session2, sql_having_key)
        .expect_err(
            "HAVING referencing the GROUP BY key column should be rejected on the SQL path",
        );
    let bound_session2 = SessionState::default();
    let bound_err2 = core
        .execute_bound_aggregate_in_session(&ctx_a, &bound_session2, TABLE, |schema, udfs| {
            let validated = validate_sql(sql_having_key, &FixedTableLookup)?;
            let Statement::Aggregate(validated_aggregate) = validated else {
                panic!("expected Statement::Aggregate");
            };
            bind_aggregate(&validated_aggregate, schema, udfs)
        })
        .expect_err(
            "HAVING referencing the GROUP BY key column should be rejected on the bound path",
        );
    // 同様に `22000`（invalid input）を固定オラクルとして検査する。
    assert_eq!(sql_err2.wire_code(), "22000", "sql_err2={sql_err2:?}");
    assert_eq!(bound_err2.wire_code(), "22000", "bound_err2={bound_err2:?}");
    assert_eq!(
        sql_err2.wire_code(),
        bound_err2.wire_code(),
        "HAVING key-column comparison wire_code mismatch: sql={sql_err2:?} bound={bound_err2:?}"
    );
}

// --- T5: `BoundScan::with_order_by`（NoSQL `sort` 表層向け入口。Issue #946・
// NOSQL-15・SQL-25 (a)・TASK-224）が SQL テキスト経由の `ORDER BY` 束縛と
// 等価な `BoundScan` を組み立てることの固定 ------------------------------------

fn udf_registry() -> engine::sql::udf_call::UdfRegistry {
    engine::sql::udf_call::UdfRegistry::default()
}

/// `SELECT id, lang FROM docs ORDER BY <sql_order_by> LIMIT <limit>` を
/// `validate_sql` → `bind_scan` で束縛する（SQL テキスト経由のオラクル）。
fn bind_scan_via_sql_text(
    schema: &TableSchema,
    sql_order_by: &str,
    limit: usize,
) -> Result<BoundScan, SqlSurfaceError> {
    let sql = format!("SELECT id, lang FROM docs ORDER BY {sql_order_by} LIMIT {limit}");
    let validated = validate_sql(&sql, &FixedTableLookup)?;
    let Statement::Scan(validated_scan) = validated else {
        panic!("expected Statement::Scan for {sql:?}");
    };
    bind_scan(&validated_scan, schema, &udf_registry())
}

/// `BoundScan::new` + [`BoundScan::with_order_by`]（NoSQL 表層の直接構築経路）
/// で同じ `ORDER BY` を束縛する（Issue #946 で追加した builder のオラクル）。
fn bind_scan_via_with_order_by(
    schema: &TableSchema,
    keys: &[ScalarOrderKey],
    limit: usize,
) -> Result<BoundScan, SqlSurfaceError> {
    let projection = vec![
        ProjectedColumn::Id,
        ProjectedColumn::Column {
            index: 1,
            name: "lang".to_string(),
        },
    ];
    BoundScan::new(TABLE.to_string(), projection, Vec::new(), Vec::new(), limit)
        .with_order_by(keys, schema)
}

#[test]
fn with_order_by_matches_sql_text_order_by_for_multiple_keys() {
    let path = unique_db_path("bound-plan-with-order-by-equivalence");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    seed_two_tenants(&storage);
    let schema = storage.get_table_schema(TABLE).expect("get_table_schema");

    let keys = [
        ScalarOrderKey {
            column: "lang".to_string(),
            descending: true,
        },
        ScalarOrderKey {
            column: "id".to_string(),
            descending: false,
        },
    ];
    let via_sql =
        bind_scan_via_sql_text(&schema, "lang DESC, id ASC", 10).expect("bind_scan via SQL text");
    let via_builder =
        bind_scan_via_with_order_by(&schema, &keys, 10).expect("with_order_by should bind");

    assert_eq!(
        via_sql, via_builder,
        "BoundScan::with_order_by should produce a BoundScan identical to the SQL ORDER BY path"
    );
}

#[test]
fn with_order_by_empty_slice_is_a_no_op_equivalent_to_bound_scan_new() {
    let path = unique_db_path("bound-plan-with-order-by-empty");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    seed_two_tenants(&storage);
    let schema = storage.get_table_schema(TABLE).expect("get_table_schema");

    let projection = vec![
        ProjectedColumn::Id,
        ProjectedColumn::Column {
            index: 1,
            name: "lang".to_string(),
        },
    ];
    let plain = BoundScan::new(
        TABLE.to_string(),
        projection.clone(),
        Vec::new(),
        Vec::new(),
        5,
    );
    let with_empty_order_by =
        BoundScan::new(TABLE.to_string(), projection, Vec::new(), Vec::new(), 5)
            .with_order_by(&[], &schema)
            .expect("empty order_by slice should be accepted as a no-op");

    assert_eq!(plain, with_empty_order_by);
}

#[test]
fn with_order_by_rejects_more_than_max_scalar_order_keys_with_54000() {
    let path = unique_db_path("bound-plan-with-order-by-key-limit");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    seed_two_tenants(&storage);
    let schema = storage.get_table_schema(TABLE).expect("get_table_schema");

    // ちょうど上限（8 キー）は受理される。`lang` を繰り返し指定しても
    // 束縛段では列名の重複を拒否しないため、件数検査だけを固定できる。
    let at_limit: Vec<ScalarOrderKey> = (0..MAX_SCALAR_ORDER_KEYS)
        .map(|_| ScalarOrderKey {
            column: "lang".to_string(),
            descending: false,
        })
        .collect();
    assert!(
        bind_scan_via_with_order_by(&schema, &at_limit, 5).is_ok(),
        "exactly MAX_SCALAR_ORDER_KEYS keys should be accepted"
    );

    // 上限超過（9 キー）は `54000`（payload_too_large）で拒否される。
    let mut over_limit = at_limit;
    over_limit.push(ScalarOrderKey {
        column: "id".to_string(),
        descending: false,
    });
    let err = bind_scan_via_with_order_by(&schema, &over_limit, 5)
        .expect_err("MAX_SCALAR_ORDER_KEYS + 1 keys should be rejected");
    assert_eq!(err.wire_code(), "54000", "err={err:?}");
}

#[test]
fn with_order_by_rejects_unknown_column_and_vector_column_with_22000() {
    let path = unique_db_path("bound-plan-with-order-by-invalid-column");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    seed_two_tenants(&storage);
    let schema = storage.get_table_schema(TABLE).expect("get_table_schema");

    let unknown_column = [ScalarOrderKey {
        column: "does_not_exist".to_string(),
        descending: false,
    }];
    let err = bind_scan_via_with_order_by(&schema, &unknown_column, 5)
        .expect_err("unknown column should be rejected");
    assert_eq!(err.wire_code(), "22000", "err={err:?}");

    // `embedding` は VECTOR 列（並べ替え不能。SQL-25 (a) と同じ判断）。
    let vector_column = [ScalarOrderKey {
        column: "embedding".to_string(),
        descending: false,
    }];
    let err = bind_scan_via_with_order_by(&schema, &vector_column, 5)
        .expect_err("VECTOR column should be rejected as unsortable");
    assert_eq!(err.wire_code(), "22000", "err={err:?}");
}
