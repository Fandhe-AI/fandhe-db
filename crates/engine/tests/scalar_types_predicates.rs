//! `DATE`／`TIMESTAMP`／`NUMERIC`／`UUID`／`BYTEA` 列の `WHERE` 等価・範囲比較
//! 述語（TABLE-13・TASK-199、Issue #891）の結合テスト。ポインタ:
//! `docs/spec/05-tasks.md` TASK-199・`docs/spec/04-behavior/data-model.md`
//! TABLE-13。
//!
//! 算術を持たない非数値型（DATE/TIMESTAMP/NUMERIC/UUID/BYTEA）を宣言的経路
//! （`declarative_filter::FilterOp::TypedCompare`）へ束縛し、`=`/`<`/`<=`/`>`/
//! `>=` を文字列リテラル形（`col <op> '<literal>'`）で受理することを、検索
//! `SELECT`（SCALAR 先行）・広域取得 `scan`・集計 `COUNT(*) WHERE`・
//! `GROUP BY ... WHERE`・述語つき `UPDATE`/`DELETE` の各経路を横断して固定する。
//!
//! 算術を持つ数値型（INTEGER/BIGINT/REAL/DOUBLE。レーン A）を式・`WHERE` の
//! 算術中で参照する経路は本 Issue の対象外のまま（別 Issue 申し送り。詳細は
//! `docs/design/scalar-types-predicates.md` 参照）であることも固定する。
//!
//! `tests/numeric_column.rs`・`tests/datetime_column.rs`・`tests/uuid_column.rs`・
//! `tests/bytea_column.rs` と同じ流儀（`unique_db_path`／`CleanupGuard`、実
//! `Storage`＋`CpuScalarProvider`、`EngineCore::execute_sql`／
//! `execute_sql_in_session` を production 経路として検証）。

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
            ColumnDef::new("day", ColumnType::Date, true),
            ColumnDef::new("at", ColumnType::Timestamp, true),
            ColumnDef::new(
                "price",
                ColumnType::Numeric {
                    precision: 10,
                    scale: 2,
                },
                true,
            ),
            ColumnDef::new("ext_id", ColumnType::Uuid, true),
            ColumnDef::new("blob", ColumnType::Bytea, true),
            ColumnDef::new("qty", ColumnType::Integer, true),
        ],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("scalar-types-predicates");
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
    day: Option<&str>,
    at: Option<&str>,
    price: Option<&str>,
    ext_id: Option<&str>,
    blob: Option<&str>,
    qty: Option<i32>,
    op: &str,
) {
    // 明示 `NULL` リテラルは INSERT VALUES の文法として受理されないため
    // （`bind_set_assignments`〔UPDATE SET〕専用。Issue #889 レビュー指摘・
    // PR #1014）、`None` の列は列リストごと省略する（`datetime_column.rs`
    // の `insert_sql_without_day_at` と同じ方式。未指定の nullable 列は
    // 既定で NULL になる）。
    let mut columns = vec!["id", "embedding", "lang"];
    let mut values = vec![
        id.to_string(),
        "'[0.1,0.2]'".to_string(),
        format!("'{lang}'"),
    ];
    if let Some(v) = day {
        columns.push("day");
        values.push(format!("'{v}'"));
    }
    if let Some(v) = at {
        columns.push("at");
        values.push(format!("'{v}'"));
    }
    if let Some(v) = price {
        columns.push("price");
        values.push(format!("'{v}'"));
    }
    if let Some(v) = ext_id {
        columns.push("ext_id");
        values.push(format!("'{v}'"));
    }
    if let Some(v) = blob {
        columns.push("blob");
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

fn seed_two_rows(core: &EngineCore, ctx: &PolicyContext) {
    insert_row(
        core,
        ctx,
        1,
        "ja",
        Some("2024-01-01"),
        Some("2024-01-01 00:00:00"),
        Some("1.00"),
        Some("00000000-0000-0000-0000-000000000000"),
        Some("\\xdead"),
        Some(1),
        "op-1",
    );
    insert_row(
        core,
        ctx,
        2,
        "en",
        Some("2024-06-01"),
        Some("2024-06-01 12:00:00"),
        Some("2.50"),
        Some("ffffffff-ffff-ffff-ffff-ffffffffffff"),
        Some("\\xff"),
        Some(2),
        "op-2",
    );
}

// --- 型ごとの肯定ケース（等価・境界の範囲比較） -------------------------------

#[test]
fn date_equality_and_range_predicates_select_expected_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_two_rows(&core, &alice);

    assert_eq!(select_ids(&core, &alice, "day = '2024-01-01'"), vec![1]);
    assert_eq!(select_ids(&core, &alice, "day > '2024-01-01'"), vec![2]);
    assert_eq!(select_ids(&core, &alice, "day >= '2024-01-01'"), vec![1, 2]);
    assert_eq!(select_ids(&core, &alice, "day < '2024-06-01'"), vec![1]);
    assert_eq!(select_ids(&core, &alice, "day <= '2024-01-01'"), vec![1]);
}

#[test]
fn timestamp_equality_and_range_predicates_select_expected_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_two_rows(&core, &alice);

    assert_eq!(
        select_ids(&core, &alice, "at = '2024-01-01 00:00:00'"),
        vec![1]
    );
    assert_eq!(
        select_ids(&core, &alice, "at > '2024-01-01 00:00:00'"),
        vec![2]
    );
    assert_eq!(
        select_ids(&core, &alice, "at <= '2024-01-01 00:00:00'"),
        vec![1]
    );
}

#[test]
fn numeric_equality_and_range_predicates_do_not_round_to_column_scale() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_two_rows(&core, &alice);

    assert_eq!(select_ids(&core, &alice, "price = '1.00'"), vec![1]);
    assert_eq!(select_ids(&core, &alice, "price > '1.00'"), vec![2]);
    assert_eq!(select_ids(&core, &alice, "price >= '1.00'"), vec![1, 2]);
    assert_eq!(select_ids(&core, &alice, "price <= '1.00'"), vec![1]);

    // scale が異なるリテラルでも丸めずに正確な数値として比較する
    // （`numeric::cmp_exact`）。`1.000`（scale 3）は `1.00`（scale 2）と
    // 数値として等しい。
    assert_eq!(select_ids(&core, &alice, "price = '1.000'"), vec![1]);
    // `0.999999`（scale 6）は `1.00` 未満。丸めれば `1.00` に等しくなり
    // うるが、正確な比較では厳密に小さい。
    assert_eq!(select_ids(&core, &alice, "price > '0.999999'"), vec![1, 2]);
    assert!(select_ids(&core, &alice, "price = '0.999999'").is_empty());
}

#[test]
fn uuid_equality_and_range_predicates_select_expected_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_two_rows(&core, &alice);

    assert_eq!(
        select_ids(
            &core,
            &alice,
            "ext_id = '00000000-0000-0000-0000-000000000000'"
        ),
        vec![1]
    );
    assert_eq!(
        select_ids(
            &core,
            &alice,
            "ext_id > '00000000-0000-0000-0000-000000000000'"
        ),
        vec![2]
    );
}

#[test]
fn bytea_equality_and_range_predicates_select_expected_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_two_rows(&core, &alice);

    assert_eq!(select_ids(&core, &alice, "blob = '\\xdead'"), vec![1]);
    assert_eq!(select_ids(&core, &alice, "blob > '\\xdead'"), vec![2]);
    assert_eq!(select_ids(&core, &alice, "blob <= '\\xdead'"), vec![1]);
}

// --- NULL は常に不一致 -------------------------------------------------------

#[test]
fn null_rows_never_match_typed_compare_predicates() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_two_rows(&core, &alice);
    // day/at/price/ext_id/blob がすべて NULL の行。
    insert_row(
        &core, &alice, 3, "ja", None, None, None, None, None, None, "op-3",
    );

    for (col, op, lit) in [
        ("day", "=", "2024-01-01"),
        ("day", ">", "1970-01-01"),
        ("at", "=", "2024-01-01 00:00:00"),
        ("price", "=", "1.00"),
        ("ext_id", "=", "00000000-0000-0000-0000-000000000000"),
        ("blob", "=", "\\xdead"),
    ] {
        let rows = select_ids(&core, &alice, &format!("{col} {op} '{lit}'"));
        assert!(
            !rows.contains(&3),
            "NULL row must never match {col} {op} '{lit}'"
        );
    }
}

// --- 型不一致・リテラル形式違反は束縛時に 22000/22007/22P02/22P02 で拒否 -------------

#[test]
fn type_mismatched_comparisons_are_rejected_at_bind_time() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_two_rows(&core, &alice);

    // DATE 列と数値リテラルの比較（型不一致）。TEXT 列の範囲比較は
    // Issue #1183 で受理されたため、型不一致の例には使わない。
    let err = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE day > 5 LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "42804");

    // DATE 列に UUID 形式のリテラルは形式違反として `22007`。
    let err = core
        .execute_sql(
            &alice,
            &format!(
                "SELECT id FROM {TABLE} WHERE day = '00000000-0000-0000-0000-000000000000' LIMIT 10"
            ),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22007");

    // UUID 列に形式違反のリテラルは `22P02`。
    let err = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE ext_id > 'not-a-uuid' LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22P02");

    // BYTEA 列に接頭辞なしのリテラルは `22P02`。
    let err = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE blob > 'deadbeef' LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22P02");
}

// --- INTEGER 列の比較・式参照（Issue #1183。SQL-24・SQL-26・TABLE-13 ポインタ） --

#[test]
fn integer_column_comparison_and_expression_reference_are_accepted() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_two_rows(&core, &alice);

    // `qty > '1'`（文字列リテラルとの比較）は PostgreSQL の暗黙型変換に当たるため
    // 引き続き対象外（`22000`）。
    let err = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE qty > '1' LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");

    assert_eq!(select_ids(&core, &alice, "qty > 1"), vec![2]);
    assert_eq!(select_ids(&core, &alice, "qty >= 1"), vec![1, 2]);
    assert_eq!(select_ids(&core, &alice, "qty < 2"), vec![1]);
    assert_eq!(select_ids(&core, &alice, "qty = 2"), vec![2]);
    assert_eq!(select_ids(&core, &alice, "qty + 1 > 2"), vec![2]);
    assert_eq!(
        select_ids(&core, &alice, "qty = 1 OR qty = 2 AND lang = 'en'"),
        vec![1, 2]
    );
}

#[test]
fn integer_column_expression_projection_and_aggregate_work() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_two_rows(&core, &alice);

    let result = core
        .execute_sql(
            &alice,
            &format!("SELECT id, COALESCE(qty * 2, 0) FROM {TABLE} WHERE qty >= 1 LIMIT 10"),
        )
        .expect("projection over an INTEGER column must succeed");
    let cells: Vec<_> = result.rows.iter().map(|r| r.cells.clone()).collect();
    assert_eq!(cells.len(), 2);
    assert!(cells.iter().any(|c| c[1] == Cell::Float(4.0)));

    let result = core
        .execute_sql(&alice, &format!("SELECT SUM(qty * 2) FROM {TABLE}"))
        .expect("SUM over an expression must succeed");
    assert_eq!(result.rows[0].cells[0], Cell::Float(6.0));
}

#[test]
fn integer_column_division_by_zero_is_22012() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_two_rows(&core, &alice);
    let err = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE qty / 0 > 1 LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22012");
}

// --- TEXT 列の範囲比較（Issue #1183。バイト順） ---------------------------------

#[test]
fn text_range_comparison_selects_expected_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_two_rows(&core, &alice);

    // lang: 1 = 'ja', 2 = 'en'
    assert_eq!(select_ids(&core, &alice, "lang < 'g'"), vec![2]);
    assert_eq!(select_ids(&core, &alice, "lang <= 'en'"), vec![2]);
    assert_eq!(select_ids(&core, &alice, "lang > 'en'"), vec![1]);
    assert_eq!(select_ids(&core, &alice, "lang >= 'en'"), vec![1, 2]);
    assert_eq!(select_ids(&core, &alice, "NOT lang < 'g'"), vec![1]);
    assert_eq!(select_ids(&core, &alice, "NOT lang >= 'g'"), vec![2]);
    assert_eq!(
        select_ids(&core, &alice, "lang < 'g' OR lang > 'j'"),
        vec![1, 2]
    );
    // 式レーン（`lower(lang)`）の比較と結果が一致する。
    assert_eq!(
        select_ids(&core, &alice, "lower(lang) < 'g'"),
        select_ids(&core, &alice, "lang < 'g'")
    );
}

// --- 広域取得 scan（`ORDER BY` を伴わない `SELECT ... WHERE ... LIMIT n`） -----

#[test]
fn scan_path_accepts_typed_compare_predicate() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_two_rows(&core, &alice);

    let result = core
        .execute_sql(
            &alice,
            &format!("SELECT id, lang FROM {TABLE} WHERE day > '2024-01-01' LIMIT 10"),
        )
        .expect("scan path should accept a typed compare predicate");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].cells[0], Cell::Integer(2));
}

// --- 集計 COUNT(*) WHERE ------------------------------------------------------

#[test]
fn aggregate_count_star_accepts_typed_compare_predicate() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_two_rows(&core, &alice);

    let result = core
        .execute_sql(
            &alice,
            &format!("SELECT COUNT(*) FROM {TABLE} WHERE day >= '2024-01-01'"),
        )
        .expect("aggregate COUNT(*) WHERE should accept a typed compare predicate");
    match &result.rows[0].cells[0] {
        Cell::Integer(v) => assert_eq!(*v, 2),
        other => panic!("expected Cell::Integer, got {other:?}"),
    }

    let result = core
        .execute_sql(
            &alice,
            &format!("SELECT COUNT(*) FROM {TABLE} WHERE price = '1.00'"),
        )
        .expect("aggregate COUNT(*) WHERE should accept a NUMERIC equality predicate");
    match &result.rows[0].cells[0] {
        Cell::Integer(v) => assert_eq!(*v, 1),
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
}

// --- GROUP BY ... WHERE -------------------------------------------------------

#[test]
fn group_by_with_typed_compare_where_accepts_predicate() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_two_rows(&core, &alice);
    insert_row(
        &core,
        &alice,
        3,
        "en",
        Some("2024-07-01"),
        Some("2024-07-01 00:00:00"),
        Some("3.00"),
        None,
        None,
        Some(3),
        "op-3",
    );

    let result = core
        .execute_sql(
            &alice,
            &format!(
                "SELECT lang, COUNT(*) AS n FROM {TABLE} WHERE day > '2024-01-01' \
                 GROUP BY lang ORDER BY lang"
            ),
        )
        .expect("GROUP BY ... WHERE should accept a typed compare predicate");
    assert_eq!(result.rows.len(), 1);
    match (&result.rows[0].cells[0], &result.rows[0].cells[1]) {
        (Cell::Text(lang), Cell::Integer(n)) => {
            assert_eq!(lang, "en");
            assert_eq!(*n, 2);
        }
        other => panic!("unexpected row shape: {other:?}"),
    }
}

// --- 述語つき UPDATE/DELETE ---------------------------------------------------

#[test]
fn predicate_update_accepts_typed_compare_where() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_two_rows(&core, &alice);

    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "UPDATE {TABLE} SET lang = 'fr' WHERE day > '2024-01-01' \
                 USING OPERATION_ID 'op-upd-1'"
            ),
        )
        .expect("predicate UPDATE should accept a typed compare predicate");
    match outcome {
        SqlOutcome::Update(u) => assert_eq!(u.rows_affected, 1),
        other => panic!("expected SqlOutcome::Update, got {other:?}"),
    }
    assert_eq!(
        select_ids(&core, &alice, "lang = 'fr'"),
        vec![2],
        "only the row matching the typed compare predicate should be updated"
    );
}

#[test]
fn predicate_delete_accepts_typed_compare_where() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_two_rows(&core, &alice);

    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!("DELETE FROM {TABLE} WHERE price >= '2.00' USING OPERATION_ID 'op-del-1'"),
        )
        .expect("predicate DELETE should accept a typed compare predicate");
    match outcome {
        SqlOutcome::Delete(d) => assert_eq!(d.rows_affected, 1),
        other => panic!("expected SqlOutcome::Delete, got {other:?}"),
    }
    assert_eq!(select_ids(&core, &alice, "lang = 'en'"), Vec::<u64>::new());
    assert_eq!(select_ids(&core, &alice, "lang = 'ja'"), vec![1]);
}

// --- RLS: 他テナントの行は typed compare 述語でも一切見えない -----------------

#[test]
fn rls_isolates_typed_compare_predicate_results_across_tenants() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let bob = ctx_for("bob");
    seed_two_rows(&core, &alice);
    insert_row(
        &core,
        &bob,
        101,
        "en",
        Some("2024-01-02"),
        Some("2024-01-02 00:00:00"),
        Some("9.99"),
        Some("ffffffff-ffff-ffff-ffff-ffffffffffff"),
        Some("\\xff"),
        Some(9),
        "op-bob-1",
    );

    // alice の視点では自身の行のみが typed compare 述語の対象になる。
    assert_eq!(select_ids(&core, &alice, "day >= '2024-01-01'"), vec![1, 2]);
    // bob の視点でも同様（他テナント行は見えない）。
    assert_eq!(select_ids(&core, &bob, "day >= '2024-01-01'"), vec![101]);
}

// --- operation_id 内容照合ハッシュ（Compare 用新規タグ）の再送判定 -----------
//
// `WherePredicate::Compare`（typed compare。`recovery::content_hash` の新規
// タグ）を経由した述語つき UPDATE/DELETE でも、既存の Equality 経由の
// 台帳照合（TASK-101・RECOVER-10 の重複拒否契約を包含する RECOVER-11）と
// 同じく「同一内容の再送は 23505」「内容不一致は 22023」に収束することを
// 固定する（Issue #891 レビュー指摘。`sql_predicate_dml_exec.rs` の
// Equality 版テストと同じ流儀）。

#[test]
fn predicate_update_typed_compare_resend_same_content_is_duplicate() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_two_rows(&core, &alice);

    let sql = format!(
        "UPDATE {TABLE} SET lang = 'fr' WHERE day > '2024-01-01' \
         USING OPERATION_ID 'op-upd-compare-resend'"
    );
    core.execute_sql_in_session(&alice, &mut SessionState::default(), &sql)
        .expect("first predicate UPDATE should succeed");

    let err = core
        .execute_sql_in_session(&alice, &mut SessionState::default(), &sql)
        .expect_err("resend of the same operation_id and predicate must be rejected");
    assert_eq!(err.wire_code(), "23505");
    // 再送は台帳照合で拒否され、行は 1 回目の適用のまま変化しない。
    assert_eq!(select_ids(&core, &alice, "lang = 'fr'"), vec![2]);
}

#[test]
fn predicate_update_typed_compare_resend_different_literal_is_content_mismatch() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_two_rows(&core, &alice);

    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &format!(
            "UPDATE {TABLE} SET lang = 'fr' WHERE day > '2024-01-01' \
             USING OPERATION_ID 'op-upd-compare-mismatch'"
        ),
    )
    .expect("first predicate UPDATE should succeed");

    // 同一 operation_id・同一列だが比較演算子の相手リテラルが異なる
    // （境界値を跨いで一致行集合も変わる）ため内容不一致。
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "UPDATE {TABLE} SET lang = 'fr' WHERE day > '2024-06-01' \
                 USING OPERATION_ID 'op-upd-compare-mismatch'"
            ),
        )
        .expect_err("resend with a different compare literal must be a content mismatch");
    assert_eq!(err.wire_code(), "22023");
}

#[test]
fn predicate_delete_typed_compare_resend_same_content_is_duplicate() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_two_rows(&core, &alice);

    let sql = format!(
        "DELETE FROM {TABLE} WHERE price >= '2.00' USING OPERATION_ID 'op-del-compare-resend'"
    );
    core.execute_sql_in_session(&alice, &mut SessionState::default(), &sql)
        .expect("first predicate DELETE should succeed");

    let err = core
        .execute_sql_in_session(&alice, &mut SessionState::default(), &sql)
        .expect_err("resend of the same operation_id and predicate must be rejected");
    assert_eq!(err.wire_code(), "23505");
    // 台帳照合による拒否のため、対象行は既に削除済みのまま変化しない。
    assert_eq!(select_ids(&core, &alice, "lang = 'en'"), Vec::<u64>::new());
}

#[test]
fn predicate_delete_typed_compare_resend_different_operator_is_content_mismatch() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_two_rows(&core, &alice);

    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &format!(
            "DELETE FROM {TABLE} WHERE price >= '2.00' USING OPERATION_ID 'op-del-compare-op'"
        ),
    )
    .expect("first predicate DELETE should succeed");

    // 同一 operation_id・同一列・同一リテラルだが演算子（>= → >）が異なる
    // ため内容不一致（削除対象は既に無いが台帳照合が列挙より先に働く）。
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "DELETE FROM {TABLE} WHERE price > '2.00' USING OPERATION_ID 'op-del-compare-op'"
            ),
        )
        .expect_err("resend with a different compare operator must be a content mismatch");
    assert_eq!(err.wire_code(), "22023");
}
