//! `WHERE` の `IN` / `BETWEEN` / `IS [NOT] NULL` / `NOT` 述語（SQL-24。
//! ポインタ: `docs/spec/05-tasks.md` TASK-208、`docs/spec/04-behavior/
//! sql-surface.md` SQL-24）の結合テスト。
//!
//! `tests/scalar_types_predicates.rs`（TABLE-13・TASK-199、Issue #891）と同じ
//! 流儀（`unique_db_path`／`CleanupGuard`、実 `Storage`＋`CpuScalarProvider`、
//! `EngineCore::execute_sql`／`execute_sql_in_session` を production 経路として
//! 検証）を踏襲し、検索 `SELECT`（SCALAR 先行）・広域取得 `scan`・集計
//! `COUNT(*) WHERE`・`GROUP BY ... WHERE`・述語つき `UPDATE`/`DELETE` の各経路を
//! 横断して固定する。テストファイル名は TASK-208 の共有名（`sql24_predicates.rs`）
//! を避け、並列実装中の兄弟 Issue（#912・#914）とのファイル追加衝突を防ぐ。
//!
//! 対象外（別 Issue へ申し送り。§8 参照）: `id`／INTEGER 系列の `IN`/`BETWEEN`、
//! `InTyped`／`IS NULL` の索引対応、`NOT (...)` の括弧グループ（#912）、`LIKE`
//! の中間一致等の拡張（#914）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
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
            ColumnDef::new("tag", ColumnType::Text, true),
            ColumnDef::new("day", ColumnType::Date, true),
            ColumnDef::new("qty", ColumnType::Integer, true),
        ],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql24-in-between-null-not");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

#[allow(clippy::too_many_arguments)]
fn insert_row(
    core: &EngineCore,
    ctx: &PolicyContext,
    id: u64,
    lang: &str,
    tag: Option<&str>,
    day: Option<&str>,
    qty: Option<i32>,
    visibility_op: &str,
    op: &str,
) {
    let _ = visibility_op;
    let mut columns = vec!["id", "embedding", "lang"];
    let mut values = vec![
        id.to_string(),
        "'[0.1,0.2]'".to_string(),
        format!("'{lang}'"),
    ];
    if let Some(v) = tag {
        columns.push("tag");
        values.push(format!("'{v}'"));
    }
    if let Some(v) = day {
        columns.push("day");
        values.push(format!("'{v}'"));
    }
    if let Some(v) = qty {
        columns.push("qty");
        values.push(v.to_string());
    }
    let sql = format!(
        "INSERT INTO {TABLE} ({}) VALUES ({}) USING OPERATION_ID '{op}'",
        columns.join(", "),
        values.join(", "),
    );
    core.execute_sql_in_session(ctx, &mut SessionState::default(), &sql)
        .expect("insert should succeed");
}

fn ids(cells: &[Vec<Cell>]) -> Vec<u64> {
    let mut out: Vec<u64> = cells
        .iter()
        .map(|row| match &row[0] {
            Cell::Integer(v) => *v,
            other => panic!("expected Cell::Integer for id, got {other:?}"),
        })
        .collect();
    out.sort_unstable();
    out
}

fn select_ids(core: &EngineCore, ctx: &PolicyContext, where_clause: &str) -> Vec<u64> {
    let result = core
        .execute_sql(
            ctx,
            &format!("SELECT id FROM {TABLE} WHERE {where_clause} LIMIT 100"),
        )
        .unwrap_or_else(|e| panic!("query {where_clause:?} should succeed, got {e:?}"));
    ids(&result
        .rows
        .iter()
        .map(|r| r.cells.clone())
        .collect::<Vec<_>>())
}

/// SEED: id 1..=5、`lang` は 1..3 が "ja"、4..5 が "en"。`tag` は id 1..3 のみ
/// 設定し 4..5 は NULL。`day` は id 1..4 のみ設定し 5 は NULL。
fn seed_five_rows(core: &EngineCore, ctx: &PolicyContext) {
    insert_row(
        core,
        ctx,
        1,
        "ja",
        Some("a"),
        Some("2024-01-01"),
        Some(1),
        "op",
        "op-1",
    );
    insert_row(
        core,
        ctx,
        2,
        "ja",
        Some("b"),
        Some("2024-03-01"),
        Some(2),
        "op",
        "op-2",
    );
    insert_row(
        core,
        ctx,
        3,
        "ja",
        Some("c"),
        Some("2024-06-01"),
        Some(3),
        "op",
        "op-3",
    );
    insert_row(
        core,
        ctx,
        4,
        "en",
        None,
        Some("2024-09-01"),
        Some(4),
        "op",
        "op-4",
    );
    insert_row(core, ctx, 5, "en", None, None, Some(5), "op", "op-5");
}

// --- IN --------------------------------------------------------------------

#[test]
fn in_list_selects_matching_rows_across_select_scan_aggregate_group_by() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_five_rows(&core, &alice);

    // 検索 SELECT（SCALAR 先行）
    assert_eq!(
        select_ids(&core, &alice, "tag IN ('a', 'c', 'zzz')"),
        vec![1, 3]
    );

    // 広域取得 scan（ORDER BY を伴わない）
    let result = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE lang IN ('en') LIMIT 10"),
        )
        .expect("scan path should accept IN predicate");
    assert_eq!(
        ids(&result
            .rows
            .iter()
            .map(|r| r.cells.clone())
            .collect::<Vec<_>>()),
        vec![4, 5]
    );

    // 集計 COUNT(*) WHERE
    let result = core
        .execute_sql(
            &alice,
            &format!("SELECT COUNT(*) FROM {TABLE} WHERE lang IN ('ja', 'en')"),
        )
        .expect("aggregate COUNT(*) WHERE should accept IN predicate");
    match &result.rows[0].cells[0] {
        Cell::Integer(v) => assert_eq!(*v, 5),
        other => panic!("expected Cell::Integer, got {other:?}"),
    }

    // GROUP BY ... WHERE
    let result = core
        .execute_sql(
            &alice,
            &format!(
                "SELECT lang, COUNT(*) AS n FROM {TABLE} WHERE tag IN ('a', 'b', 'c') \
                 GROUP BY lang ORDER BY lang"
            ),
        )
        .expect("GROUP BY ... WHERE should accept IN predicate");
    assert_eq!(result.rows.len(), 1);
    match (&result.rows[0].cells[0], &result.rows[0].cells[1]) {
        (Cell::Text(lang), Cell::Integer(n)) => {
            assert_eq!(lang, "ja");
            assert_eq!(*n, 3);
        }
        other => panic!("unexpected row shape: {other:?}"),
    }
}

#[test]
fn not_in_excludes_matching_rows_and_excludes_null_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_five_rows(&core, &alice);

    // `tag NOT IN (...)` は NULL 行（id 4, 5）を含まない（三値論理: NULL は
    // UNKNOWN のまま。`NOT` で真に反転しない）。
    assert_eq!(select_ids(&core, &alice, "tag NOT IN ('a')"), vec![2, 3]);
}

#[test]
fn predicate_update_and_delete_accept_in_predicate() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_five_rows(&core, &alice);

    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "UPDATE {TABLE} SET lang = 'fr' WHERE tag IN ('a', 'b') \
                 USING OPERATION_ID 'op-upd-in'"
            ),
        )
        .expect("predicate UPDATE should accept IN predicate");
    match outcome {
        SqlOutcome::Update(u) => assert_eq!(u.rows_affected, 2),
        other => panic!("expected SqlOutcome::Update, got {other:?}"),
    }
    assert_eq!(select_ids(&core, &alice, "lang = 'fr'"), vec![1, 2]);

    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!("DELETE FROM {TABLE} WHERE lang IN ('en') USING OPERATION_ID 'op-del-in'"),
        )
        .expect("predicate DELETE should accept IN predicate");
    match outcome {
        SqlOutcome::Delete(d) => assert_eq!(d.rows_affected, 2),
        other => panic!("expected SqlOutcome::Delete, got {other:?}"),
    }
    assert_eq!(select_ids(&core, &alice, "lang = 'en'"), Vec::<u64>::new());
}

// --- BETWEEN -----------------------------------------------------------------

#[test]
fn between_and_not_between_select_expected_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_five_rows(&core, &alice);

    assert_eq!(
        select_ids(&core, &alice, "day BETWEEN '2024-01-01' AND '2024-06-01'"),
        vec![1, 2, 3]
    );
    // `NOT BETWEEN` は NULL 行（id 5）を含まない。
    assert_eq!(
        select_ids(
            &core,
            &alice,
            "day NOT BETWEEN '2024-01-01' AND '2024-06-01'"
        ),
        vec![4]
    );
}

// --- IS [NOT] NULL -----------------------------------------------------------

#[test]
fn is_null_and_is_not_null_partition_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_five_rows(&core, &alice);

    assert_eq!(select_ids(&core, &alice, "tag IS NULL"), vec![4, 5]);
    assert_eq!(select_ids(&core, &alice, "tag IS NOT NULL"), vec![1, 2, 3]);

    // 対象列（`tag`）を投影に含めないケース（マスク漏れの回帰。SQL-24
    // 実装計画 §4.3）: `IS NULL` は `tag` が projection に無くても正しく
    // 評価されなければならない。`select_ids` は常に `SELECT id` のみを
    // 投影するため、このテスト自体がその形状を固定する。
}

/// codex-review 実機再現（PR #913 review 対応）: `HINT ORDER(DISTANCE, SCALAR,
/// RLS)`（DISTANCE 先行）の下で `IS NULL`／`IS NOT NULL` を INTEGER 列（`qty`。
/// 全行が非 NULL）へ適用する。DISTANCE 段の後で `candidate_columns`
/// （`Value`）を `row_codec::ScalarRef` へ逆変換する経路が
/// `Value::Integer`/`BigInt`/`Array` を実 NULL と同じ `None` へ丸めていたため、
/// 非 NULL の INTEGER 列が `IS NULL` に fail-open で一致していた
/// （`sql::exec` の `postfilter_verdicts` 修正で解消）。
#[test]
fn distance_first_is_null_excludes_non_null_integer_column() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_five_rows(&core, &alice);

    let result = core
        .execute_sql(
            &alice,
            &format!(
                "SELECT id FROM {TABLE} WHERE qty IS NULL \
                 ORDER BY embedding <=> '[0.1,0.2]' LIMIT 100 \
                 HINT ORDER(DISTANCE, SCALAR, RLS)"
            ),
        )
        .expect("DISTANCE-first HINT ORDER should still apply IS NULL");
    assert!(
        ids(&result
            .rows
            .iter()
            .map(|r| r.cells.clone())
            .collect::<Vec<_>>())
        .is_empty(),
        "no row has a NULL qty; a non-empty result means IS NULL fail-opened \
         on a non-NULL INTEGER column under DISTANCE-first postfilter"
    );
}

/// origin/main マージ時の回帰テスト（Issue #912 の `OR` と #913 の `IS NULL` の
/// 統合）: `HINT ORDER(DISTANCE, SCALAR, RLS)`（DISTANCE 先行）の下で、`OR` 群の
/// 分岐に INTEGER 列（`qty`。全行が非 NULL）への `IS NULL` を含む `WHERE`。
/// `bound.or_filters` の判定は `on_visible_row` が保持する生の `scanned`
/// （`row_codec::ScalarRef`）に対して行う（`candidate_columns`〔`Value`〕への
/// 逆変換を経由しない）ことを固定する。逆変換経由だと `Value::Integer` が実
/// NULL と区別できず、上の単純 `IS NULL` テストと同じ理由で fail-open になる。
#[test]
fn distance_first_or_group_is_null_excludes_non_null_integer_column() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_five_rows(&core, &alice);

    let result = core
        .execute_sql(
            &alice,
            &format!(
                "SELECT id FROM {TABLE} WHERE lang = 'zz' OR qty IS NULL \
                 ORDER BY embedding <=> '[0.1,0.2]' LIMIT 100 \
                 HINT ORDER(DISTANCE, SCALAR, RLS)"
            ),
        )
        .expect("DISTANCE-first HINT ORDER should still apply OR-group IS NULL");
    assert!(
        ids(&result
            .rows
            .iter()
            .map(|r| r.cells.clone())
            .collect::<Vec<_>>())
        .is_empty(),
        "no row has lang='zz' and no row has a NULL qty; a non-empty result means \
         the OR group's IS NULL fail-opened on a non-NULL INTEGER column under \
         DISTANCE-first postfilter"
    );
}

/// PR #1108 codex-review 指摘対応（P1）: DISTANCE 先行時、`bound.or_filters` に
/// 式述語を含む OR 群があると、[`distance_first_or_group_is_null_excludes_non_null_integer_column`]
/// と異なりその式述語の評価を DISTANCE 段の後（実際に Top-k として選ばれた行
/// だけ）まで遅延させる必要がある（`sql::where_tree::BoundOrGroup::
/// matches_deferred`）。ここでは式述語を含む OR 群の中で `IS NULL`（宣言的・
/// エラーを返さない部分）を固定し、遅延評価経路（`OrGroupMetadataVerdict`）
/// でも INTEGER 列の非 NULL 値が誤って NULL 扱いされない（fail-open しない）
/// ことを確認する。式述語（`(1 / (id - 1000)) > 1000000`）はテストデータの
/// `id`（1..=5）に対して常に偽になるよう設計し、`OR` の真偽が `IS NULL` 側の
/// 正しさのみに依存するようにする。
#[test]
fn distance_first_or_group_with_expr_branch_is_null_excludes_non_null_integer_column() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_five_rows(&core, &alice);

    let result = core
        .execute_sql(
            &alice,
            &format!(
                "SELECT id FROM {TABLE} WHERE (1 / (id - 1000)) > 1000000 OR qty IS NULL \
                 ORDER BY embedding <=> '[0.1,0.2]' LIMIT 100 \
                 HINT ORDER(DISTANCE, SCALAR, RLS)"
            ),
        )
        .expect(
            "DISTANCE-first HINT ORDER should still apply OR-group IS NULL with an expr sibling",
        );
    assert!(
        ids(&result
            .rows
            .iter()
            .map(|r| r.cells.clone())
            .collect::<Vec<_>>())
        .is_empty(),
        "the expr branch is always false and no row has a NULL qty; a non-empty result \
         means the deferred OR-group evaluation fail-opened on a non-NULL INTEGER column"
    );
}

/// PR #1108 codex-review 指摘対応（P1）: DISTANCE 先行時に `bound.or_filters` の
/// 式述語（ゼロ除算等でエラーを返しうる）を全可視行へ前倒しで評価すると、
/// DISTANCE 段で Top-k に選ばれない行のエラーまでクエリ全体の失敗にしてしまう
/// （評価順序・エラー契約の回帰）。「危険な」行（id=4。式 `1 / (id - 4)` が
/// ゼロ除算になる）のクエリベクトルとの内積を他行より明確に小さくして
/// （`ORDER BY embedding <=> ...` のランキングは内積が大きいほど上位）
/// DISTANCE ランキングで確実に Top-k 外へ追いやり、`LIMIT` を Top-k 内の行数に
/// 絞ったクエリが成功することを固定する（遅延評価が効いていなければ
/// `on_visible_row` の時点でエラーになり、クエリ全体が失敗する）。
#[test]
fn distance_first_or_group_expr_branch_error_on_non_topk_row_does_not_fail_query() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    let insert = |id: u64, embedding: &str| {
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang) VALUES ({id}, '{embedding}', 'ja') \
                 USING OPERATION_ID 'op-expr-defer-{id}'"
            ),
        )
        .expect("insert should succeed");
    };
    // クエリベクトル `[0.1,0.2]` との内積が僅かに異なる行（Top-k に入る）。
    // 内積: id1=0.05・id2=0.051・id3=0.052（降順ランキングで id3, id2 が上位）。
    insert(1, "[0.1,0.2]");
    insert(2, "[0.11,0.2]");
    insert(3, "[0.12,0.2]");
    // クエリベクトルとの内積が明確に小さい（負の）「危険な」行。`1 / (id - 4)` は
    // id=4 でゼロ除算になるが、DISTANCE ランキングでは最下位
    // （`LIMIT 2` の Top-k 外）になる。
    insert(4, "[-10.0,-10.0]");

    let result = core
        .execute_sql(
            &alice,
            &format!(
                "SELECT id FROM {TABLE} WHERE tag = 'never-matches' OR (1 / (id - 4)) < 1000000 \
                 ORDER BY embedding <=> '[0.1,0.2]' LIMIT 2 \
                 HINT ORDER(DISTANCE, SCALAR, RLS)"
            ),
        )
        .expect(
            "a division-by-zero in an OR group's expr branch on a row outside the DISTANCE \
             Top-k must not fail the query (deferred evaluation regression)",
        );
    let result_ids = ids(&result
        .rows
        .iter()
        .map(|r| r.cells.clone())
        .collect::<Vec<_>>());
    assert_eq!(
        result_ids,
        vec![2, 3],
        "expected exactly the 2 highest-ranked rows (LIMIT 2, no under-fetch expected \
         here); id=4 (the division-by-zero row) must not appear"
    );
}

#[test]
fn distance_first_is_not_null_includes_non_null_integer_column() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_five_rows(&core, &alice);

    let result = core
        .execute_sql(
            &alice,
            &format!(
                "SELECT id FROM {TABLE} WHERE qty IS NOT NULL \
                 ORDER BY embedding <=> '[0.1,0.2]' LIMIT 100 \
                 HINT ORDER(DISTANCE, SCALAR, RLS)"
            ),
        )
        .expect("DISTANCE-first HINT ORDER should still apply IS NOT NULL");
    assert_eq!(
        ids(&result
            .rows
            .iter()
            .map(|r| r.cells.clone())
            .collect::<Vec<_>>()),
        vec![1, 2, 3, 4, 5]
    );
}

#[test]
fn not_equals_excludes_null_rows_via_three_valued_logic() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_five_rows(&core, &alice);

    // `NOT tag = 'a'` は NULL 行（id 4, 5）を含まない。
    assert_eq!(select_ids(&core, &alice, "NOT tag = 'a'"), vec![2, 3]);
}

// --- 索引経由と全走査の一致（IN・BETWEEN） -----------------------------------

#[test]
fn in_predicate_index_trusted_path_matches_plain_scan_result() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_five_rows(&core, &alice);

    let sql = format!(
        "SELECT id FROM {TABLE} WHERE tag IN ('a', 'c') \
         ORDER BY embedding <=> '[0.1,0.2]' LIMIT 10"
    );
    // 索引を温める（cold は piggyback 構築のみで信頼経路に乗らない）。
    let _ = core.execute_sql(&alice, &sql).expect("warm query");
    let before = core.scalar_index_cache_stats().index_trusted_mask_scans;
    let result = core.execute_sql(&alice, &sql).expect("hot query");
    let after = core.scalar_index_cache_stats().index_trusted_mask_scans;
    assert!(
        after > before,
        "IN predicate must take the index-trusted mask path once warmed"
    );
    assert_eq!(
        ids(&result
            .rows
            .iter()
            .map(|r| r.cells.clone())
            .collect::<Vec<_>>()),
        vec![1, 3]
    );
}

// --- RLS: NOT / IS NULL / NOT IN が他テナントの行を漏らさない -----------------

#[test]
fn rls_boundary_holds_across_not_is_null_and_not_in() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let bob = ctx_for("bob");

    // 両テナントに同じ値・NULL 行を混在させる（id はテナント内で一意）。
    seed_five_rows(&core, &alice);
    seed_five_rows(&core, &bob);

    for (clause, expected) in [
        ("tag NOT IN ('a')", vec![2, 3]),
        ("tag IS NULL", vec![4, 5]),
        ("tag IS NOT NULL", vec![1, 2, 3]),
        ("NOT tag = 'a'", vec![2, 3]),
        ("day NOT BETWEEN '2024-01-01' AND '2024-06-01'", vec![4]),
    ] {
        assert_eq!(
            select_ids(&core, &alice, clause),
            expected,
            "alice: {clause}"
        );
        assert_eq!(select_ids(&core, &bob, clause), expected, "bob: {clause}");
    }
}

// --- 対象外（fail-closed のまま拒否されることを固定） -------------------------

#[test]
fn id_in_and_integer_between_remain_rejected_as_out_of_scope() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_five_rows(&core, &alice);

    // `id IN (...)`（数値リテラル。レーン A 未実装）は式フォールバックへ回り、
    // `IN` を式構文として解釈できないため `42601`。
    let err = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE id IN (1, 2) LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "42601");

    // INTEGER 列の `BETWEEN`（数値リテラル）も同様に `42601`。
    let err = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE qty BETWEEN 1 AND 3 LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "42601");
}
