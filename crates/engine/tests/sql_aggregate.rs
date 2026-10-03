//! 集計 SELECT（TASK-166・SQL-13）の結合テスト。ポインタ: `docs/spec/05-tasks.md`
//! TASK-166・`docs/spec/04-behavior/sql-surface.md` SQL-13・
//! `docs/spec/04-behavior/rls.md` RLS-7, RLS-8。
//!
//! `tests/rls_implicit.rs` と同じ流儀（`unique_db_path`＋`CleanupGuard`、決定的
//! 擬似乱数 xorshift64*、production の判定関数（`PolicyContext::is_visible`）を
//! 呼ばない独立オラクルで可視集合・集計値を手計算し、`EngineCore::execute_sql`
//! （SQL 経由）の結果と突き合わせる）で検証する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, QueryResult};
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

// ---------- 決定的擬似乱数（xorshift64*。`tests/rls_implicit.rs` と同一実装） ----------

struct Xorshift64 {
    state: u64,
}

impl Xorshift64 {
    fn new(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn next_f32_signed(&mut self) -> f32 {
        let unit = (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
        (unit * 2.0 - 1.0) as f32
    }
}

const DIM: usize = 4;
const TABLE: &str = "docs";
const TENANTS: [&str; 3] = ["tenant-a", "tenant-b", "tenant-c"];
const LANGS: [&str; 2] = ["ja", "en"];
const ROWS_PER_TENANT: u64 = 6;

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM as u32), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

#[derive(Clone)]
struct RowTruth {
    id: u64,
    tenant: &'static str,
    visibility: Visibility,
    lang: &'static str,
    embedding: Vec<f32>,
}

/// 独立オラクル（`PolicyContext::is_visible` を一切呼ばない）。
fn is_allowed(row: &RowTruth, viewer_tenant: &str, allow_private: bool) -> bool {
    match row.visibility {
        Visibility::Public => true,
        Visibility::Private => row.tenant == viewer_tenant && allow_private,
    }
}

fn seed_multi_tenant_corpus(storage: &Storage) -> Vec<RowTruth> {
    storage.create_table(&schema()).expect("create table");
    let mut rng = Xorshift64::new(0x0C0F_FEE0_1137);
    let mut truths = Vec::new();
    let mut id = 1u64;
    for &tenant in TENANTS.iter() {
        for i in 0..ROWS_PER_TENANT {
            let visibility = if i % 2 == 0 {
                Visibility::Public
            } else {
                Visibility::Private
            };
            let lang = LANGS[(i as usize) % LANGS.len()];
            let mut emb = vec![0.0f32; DIM];
            for slot in emb.iter_mut() {
                *slot = rng.next_f32_signed();
            }
            let ctx =
                PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                    .expect("valid tenant");
            // TASK-101（RECOVER-10）: 台帳は (tenant, table, operation_id) 単位で内容
            // ハッシュを持つため、同一テナント・同一テーブル内で内容の異なる複数行へ
            // 同一 operation_id を使い回すと 2 件目以降が OperationIdContentMismatch
            // で拒否される。行ごとに一意の operation_id を使う。
            let op_id =
                engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
                    .expect("valid operation_id");
            engine::tenant::insert_typed_row(
                storage,
                TABLE,
                &ctx,
                id,
                visibility,
                &[Value::Vector(emb.clone()), Value::Text(lang.to_string())],
                &op_id,
            )
            .expect("insert row");
            truths.push(RowTruth {
                id,
                tenant,
                visibility,
                lang,
                embedding: emb,
            });
            id += 1;
        }
    }
    truths
}

fn open_storage(path: &std::path::Path) -> Storage {
    Storage::open(path).expect("open storage")
}

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn visible_rows<'a>(
    truths: &'a [RowTruth],
    viewer_tenant: &str,
    allow_private: bool,
) -> Vec<&'a RowTruth> {
    truths
        .iter()
        .filter(|t| is_allowed(t, viewer_tenant, allow_private))
        .collect()
}

fn ctx_for(tenant: &str, allow_private: bool) -> PolicyContext {
    if allow_private {
        PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
            .expect("valid tenant")
    } else {
        PolicyContext::new(tenant).expect("valid tenant")
    }
}

fn single_row(result: &QueryResult) -> &[Cell] {
    assert_eq!(
        result.rows.len(),
        1,
        "aggregate result must be a single row"
    );
    &result.rows[0].cells
}

fn as_integer(cell: &Cell) -> u64 {
    match cell {
        Cell::Integer(v) => *v,
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
}

fn as_float_or_null(cell: &Cell) -> Option<f64> {
    match cell {
        Cell::Float(v) => Some(*v),
        Cell::Null => None,
        other => panic!("expected Cell::Float or Cell::Null, got {other:?}"),
    }
}

fn as_text_or_null(cell: &Cell) -> Option<String> {
    match cell {
        Cell::Text(v) => Some(v.clone()),
        Cell::Null => None,
        other => panic!("expected Cell::Text or Cell::Null, got {other:?}"),
    }
}

fn oracle_vec_norm(v: &[f32]) -> f64 {
    v.iter()
        .map(|&x| (x as f64) * (x as f64))
        .sum::<f64>()
        .sqrt()
}

// --- SQL-13 基本: 3 テナント × Public/Private の可視集合ごとに集計値がオラクルと一致 ---

#[test]
fn sql13_basic_aggregates_match_independent_oracle() {
    let path = unique_db_path("sql13-basic");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);

    for &tenant in TENANTS.iter() {
        for allow_private in [false, true] {
            let ctx = ctx_for(tenant, allow_private);
            let visible = visible_rows(&truths, tenant, allow_private);

            let result = core
                .execute_sql(
                    &ctx,
                    "SELECT COUNT(*), COUNT(lang), SUM(id), AVG(id), MIN(id), MAX(id), MIN(lang), MAX(lang), SUM(vec_norm(embedding)) FROM docs",
                )
                .expect("aggregate query should succeed");
            let cells = single_row(&result);

            let count = visible.len() as u64;
            assert_eq!(
                as_integer(&cells[0]),
                count,
                "COUNT(*) tenant={tenant} allow_private={allow_private}"
            );
            assert_eq!(
                as_integer(&cells[1]),
                count,
                "COUNT(lang) tenant={tenant} allow_private={allow_private}"
            );

            let id_sum: u64 = visible.iter().map(|r| r.id).sum();
            let expected_sum = if visible.is_empty() {
                None
            } else {
                Some(id_sum)
            };
            assert_eq!(
                cells[2],
                expected_sum.map(Cell::Integer).unwrap_or(Cell::Null),
                "SUM(id) tenant={tenant} allow_private={allow_private}"
            );

            let expected_avg = if visible.is_empty() {
                None
            } else {
                Some(id_sum as f64 / visible.len() as f64)
            };
            assert_eq!(
                as_float_or_null(&cells[3]),
                expected_avg,
                "AVG(id) tenant={tenant} allow_private={allow_private}"
            );

            let expected_min_id = visible.iter().map(|r| r.id).min();
            let expected_max_id = visible.iter().map(|r| r.id).max();
            assert_eq!(
                cells[4],
                expected_min_id.map(Cell::Integer).unwrap_or(Cell::Null),
                "MIN(id) tenant={tenant} allow_private={allow_private}"
            );
            assert_eq!(
                cells[5],
                expected_max_id.map(Cell::Integer).unwrap_or(Cell::Null),
                "MAX(id) tenant={tenant} allow_private={allow_private}"
            );

            let expected_min_lang = visible.iter().map(|r| r.lang).min();
            let expected_max_lang = visible.iter().map(|r| r.lang).max();
            assert_eq!(
                as_text_or_null(&cells[6]),
                expected_min_lang.map(|s| s.to_string()),
                "MIN(lang) tenant={tenant} allow_private={allow_private}"
            );
            assert_eq!(
                as_text_or_null(&cells[7]),
                expected_max_lang.map(|s| s.to_string()),
                "MAX(lang) tenant={tenant} allow_private={allow_private}"
            );

            let expected_norm_sum: f64 =
                visible.iter().map(|r| oracle_vec_norm(&r.embedding)).sum();
            match as_float_or_null(&cells[8]) {
                Some(actual) if !visible.is_empty() => {
                    assert!(
                        (actual - expected_norm_sum).abs() < 1e-6,
                        "SUM(vec_norm(embedding)) mismatch: actual={actual} expected={expected_norm_sum}"
                    );
                }
                None if visible.is_empty() => {}
                other => panic!("unexpected SUM(vec_norm(embedding)) result: {other:?}"),
            }
        }
    }
}

// --- WHERE の式述語（`sql::udf_call::eval` 経由の `expr_filters`）が正しく反行を除外する ---
//
// `WHERE lang = 'fr'`（`metadata_filters`）・`WHERE visible()`（フラグのみ）は
// `execute_aggregate` の `bound.expr_filters` ループ（`continue 'rows` を含む）を
// 通らないため、`<expr> <cmp> <expr>` 形の式述語（`sql::parser::bind_where_predicates`
// が `WherePredicate::Expression` から束縛する）で別途検証する。
#[test]
fn sql13_where_expression_predicate_filters_rows() {
    let path = unique_db_path("sql13-expr-filter");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a", true);
    let visible = visible_rows(&truths, "tenant-a", true);
    let mid = visible
        .iter()
        .map(|r| r.id)
        .max()
        .expect("corpus must be non-empty")
        / 2;

    let result = core
        .execute_sql(&ctx, &format!("SELECT COUNT(*) FROM docs WHERE id > {mid}"))
        .expect("aggregate with an expression predicate should succeed");
    let expected = visible.iter().filter(|r| r.id > mid).count() as u64;
    assert_eq!(as_integer(&single_row(&result)[0]), expected);

    // 複合形: `visible()`（フラグのみ）・等価条件（`metadata_filters`）・式述語
    // （`expr_filters`）を 1 文に共存させ、いずれか 1 つでも誤って適用されなければ
    // このアサーションが落ちる。
    let combined = core
        .execute_sql(
            &ctx,
            &format!("SELECT COUNT(*) FROM docs WHERE visible() AND id > {mid} AND lang = 'ja'"),
        )
        .expect("combined WHERE predicates should succeed");
    let expected_combined = visible
        .iter()
        .filter(|r| r.id > mid && r.lang == "ja")
        .count() as u64;
    assert_eq!(as_integer(&single_row(&combined)[0]), expected_combined);
}

// --- RLS 境界: COUNT の値から他テナント行の存在・件数を推測できない ------------------

#[test]
fn sql13_rls_count_is_invariant_to_other_tenants_private_rows() {
    let path = unique_db_path("sql13-rls-invariance");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let _truths = seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);

    let viewer_ctx = ctx_for("tenant-b", false);
    let before = core
        .execute_sql(&viewer_ctx, "SELECT COUNT(*) FROM docs")
        .expect("query should succeed");
    let before_count = as_integer(&single_row(&before)[0]);

    // テナント A に Private 行を大量追加しても、tenant-b（Public のみ可視）の
    // COUNT(*) は不変であること（他テナントの存在・件数を推測不能。RLS-7・RLS-8）。
    // `EngineCore` は `Storage` を非公開で保持するため、既に構築済みの `core` へ
    // 書き込むには `EngineCore::insert_row`（TASK-95 の公開 API）を使う。
    let writer_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let metadata = engine::row_codec::encode_scalar_columns(
        &schema(),
        &[Value::Null, Value::Text("ja".to_string())],
    )
    .expect("encode scalar columns");
    for i in 0..50u64 {
        let id = 10_000 + i;
        let op_id = engine::recovery::required_op_id::OperationId::parse(&format!("test-op-2-{i}"))
            .expect("valid operation_id");
        core.insert_row(
            &writer_ctx,
            TABLE,
            id,
            &engine::storage::RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Private,
                embedding: &[0.0f32; DIM],
                metadata: &metadata,
            },
            Some(&op_id),
        )
        .expect("insert row");
    }

    let after = core
        .execute_sql(&viewer_ctx, "SELECT COUNT(*) FROM docs")
        .expect("query should succeed");
    let after_count = as_integer(&single_row(&after)[0]);
    assert_eq!(
        before_count, after_count,
        "COUNT(*) must not leak other tenants' private row counts"
    );

    // `WHERE visible()` の有無でも結果が変わらないこと。
    let with_visible = core
        .execute_sql(&viewer_ctx, "SELECT COUNT(*) FROM docs WHERE visible()")
        .expect("query should succeed");
    assert_eq!(as_integer(&single_row(&with_visible)[0]), after_count);
}

// --- NULL 契約: TEXT 列 NULL は COUNT(col) から除外・MIN/MAX は NULL を無視 --------

#[test]
fn sql13_null_contract_for_text_column() {
    let path = unique_db_path("sql13-null-contract");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage.create_table(&schema()).expect("create table");

    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let op = |n: &str| {
        engine::recovery::required_op_id::OperationId::parse(n).expect("valid operation_id")
    };

    // `note` 列追加（TABLE-5: 追加列は暗黙 nullable）の**前**に 1 行挿入しておく
    // （id 0）。この行のバッファは `note` の presence タグを持たない末尾切り詰め形の
    // ため、`row_codec::scan_scalar_columns` の「バッファ末尾で打ち切られた nullable
    // 列は欠落として許容」経路（新設列追加後の既存行が持つ実際の形）を通す。
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &ctx,
        0,
        Visibility::Public,
        &[
            Value::Vector(vec![0.0f32; DIM]),
            Value::Text("ja".to_string()),
        ],
        &op("op-0"),
    )
    .expect("insert row 0 before ALTER TABLE");

    storage
        .alter_table_add_column(TABLE, ColumnDef::new("note", ColumnType::Text, true))
        .expect("alter table");

    // note あり 2 行・note NULL 1 行。
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &ctx,
        1,
        Visibility::Public,
        &[
            Value::Vector(vec![0.0f32; DIM]),
            Value::Text("ja".to_string()),
            Value::Text("hello".to_string()),
        ],
        &op("op-1"),
    )
    .expect("insert row 1");
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &ctx,
        2,
        Visibility::Public,
        &[
            Value::Vector(vec![0.0f32; DIM]),
            Value::Text("en".to_string()),
            Value::Text("world".to_string()),
        ],
        &op("op-2"),
    )
    .expect("insert row 2");
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &ctx,
        3,
        Visibility::Public,
        &[
            Value::Vector(vec![0.0f32; DIM]),
            Value::Text("ja".to_string()),
            Value::Null,
        ],
        &op("op-3"),
    )
    .expect("insert row 3");

    let core = new_core(storage);
    let result = core
        .execute_sql(
            &ctx,
            "SELECT COUNT(*), COUNT(note), MIN(note), MAX(note) FROM docs",
        )
        .expect("aggregate query should succeed");
    let cells = single_row(&result);
    // id 0（ALTER 前挿入・末尾切り詰めで note 欠落）+ id 1, 2（note あり）+ id 3
    // （note 明示 NULL）の計 4 行。
    assert_eq!(as_integer(&cells[0]), 4);
    assert_eq!(
        as_integer(&cells[1]),
        2,
        "COUNT(note) must exclude both the truncated-buffer row and the explicit NULL row"
    );
    assert_eq!(as_text_or_null(&cells[2]), Some("hello".to_string()));
    assert_eq!(as_text_or_null(&cells[3]), Some("world".to_string()));

    // WHERE が 1 行も一致しない場合: COUNT=0・他は NULL。
    let empty = core
        .execute_sql(
            &ctx,
            "SELECT COUNT(*), COUNT(note), MIN(note), MAX(note) FROM docs WHERE lang = 'fr'",
        )
        .expect("aggregate query should succeed");
    let empty_cells = single_row(&empty);
    assert_eq!(as_integer(&empty_cells[0]), 0);
    assert_eq!(as_integer(&empty_cells[1]), 0);
    assert_eq!(as_text_or_null(&empty_cells[2]), None);
    assert_eq!(as_text_or_null(&empty_cells[3]), None);
}

// --- 行なしテーブルでも空集合契約どおり応答する -------------------------------------

#[test]
fn sql13_empty_table_returns_empty_set_contract() {
    let path = unique_db_path("sql13-empty-table");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage.create_table(&schema()).expect("create table");
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let result = core
        .execute_sql(&ctx, "SELECT COUNT(*), SUM(id), MIN(lang) FROM docs")
        .expect("aggregate query should succeed");
    let cells = single_row(&result);
    assert_eq!(as_integer(&cells[0]), 0);
    assert_eq!(cells[1], Cell::Null);
    assert_eq!(as_text_or_null(&cells[2]), None);
}

// --- オーバーフロー: SUM(id) の u64 桁あふれは 22003 で fail-closed -----------------

#[test]
fn sql13_sum_id_overflow_is_rejected() {
    let path = unique_db_path("sql13-overflow");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage.create_table(&schema()).expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let op = |n: &str| {
        engine::recovery::required_op_id::OperationId::parse(n).expect("valid operation_id")
    };
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &ctx,
        u64::MAX,
        Visibility::Public,
        &[
            Value::Vector(vec![0.0f32; DIM]),
            Value::Text("ja".to_string()),
        ],
        &op("op-1"),
    )
    .expect("insert row with id = u64::MAX");
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &ctx,
        1,
        Visibility::Public,
        &[
            Value::Vector(vec![0.0f32; DIM]),
            Value::Text("ja".to_string()),
        ],
        &op("op-2"),
    )
    .expect("insert row with id = 1");

    let core = new_core(storage);
    let sum_err = core
        .execute_sql(&ctx, "SELECT SUM(id) FROM docs")
        .expect_err("SUM(id) overflow must be rejected");
    assert_eq!(sum_err.wire_code(), "22003");
    let avg_err = core
        .execute_sql(&ctx, "SELECT AVG(id) FROM docs")
        .expect_err("AVG(id) overflow must be rejected");
    assert_eq!(avg_err.wire_code(), "22003");

    // MAX(id) はオーバーフローしないため通常どおり成功する。
    let max_ok = core
        .execute_sql(&ctx, "SELECT MAX(id) FROM docs")
        .expect("MAX(id) should not overflow");
    assert_eq!(as_integer(&single_row(&max_ok)[0]), u64::MAX);
}

// --- 型不整合: VECTOR 列への SUM/AVG/MIN/MAX は 22000、COUNT は受理 ----------------

#[test]
fn sql13_vector_column_type_mismatch_and_count_acceptance() {
    let path = unique_db_path("sql13-vector-type");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let _truths = seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a", true);

    for func in ["SUM", "AVG", "MIN", "MAX"] {
        let sql = format!("SELECT {func}(embedding) FROM docs");
        let err = core
            .execute_sql(&ctx, &sql)
            .expect_err("aggregate over VECTOR column must be rejected");
        assert_eq!(err.wire_code(), "42883", "{func}(embedding)");
    }

    let ok = core
        .execute_sql(&ctx, "SELECT COUNT(embedding) FROM docs")
        .expect("COUNT(embedding) should be accepted");
    assert_eq!(
        as_integer(&single_row(&ok)[0]),
        visible_rows(&_truths, "tenant-a", true).len() as u64
    );
}

// --- UDF 経由の集計: 宣言的 UDF を SUM 引数として使える。別セッションでは未定義 ------

#[test]
fn sql13_aggregate_over_declared_udf_call() {
    let path = unique_db_path("sql13-udf");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let truths = seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a", true);

    let mut session = engine::sql::mode::SessionState::default();
    core.execute_sql_in_session(&ctx, &mut session, "CREATE FUNCTION n(v) AS vec_norm(v)")
        .expect("CREATE FUNCTION should succeed");
    let result = core
        .execute_sql_in_session(&ctx, &mut session, "SELECT SUM(n(embedding)) FROM docs")
        .expect("aggregate over a declared UDF call should succeed");
    let cells = match result {
        engine::sql::SqlOutcome::Query(q) => q.rows[0].cells.clone(),
        other => panic!("expected SqlOutcome::Query, got {other:?}"),
    };
    let visible = visible_rows(&truths, "tenant-a", true);
    let expected: f64 = visible.iter().map(|r| oracle_vec_norm(&r.embedding)).sum();
    match as_float_or_null(&cells[0]) {
        Some(actual) => assert!((actual - expected).abs() < 1e-6),
        None => panic!("expected a non-NULL SUM"),
    }

    // 別セッション（UDF `n` 未登録）では未知の関数として `22000` になる。
    let err = core
        .execute_sql(&ctx, "SELECT SUM(n(embedding)) FROM docs")
        .expect_err("undefined UDF must be rejected in a fresh session");
    assert_eq!(err.wire_code(), "42883");
}

// --- VECTOR 列を持たないテーブルでも集計できる ------------------------------------
//
// `sql::aggregate::execute_aggregate` 自体は `VECTOR` 列の有無を前提にしない設計
// （モジュールドキュメント参照）。Issue #995 で INSERT 系の全入口が `VECTOR` 列
// なしテーブルへの書き込みを受理するようになったため、空集合だけでなく非空
// コーパスでの実地検証も行う。
#[test]
fn sql13_works_on_empty_table_without_vector_column() {
    let path = unique_db_path("sql13-no-vector");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let text_only_schema = TableSchema::new(
        "notes",
        vec![ColumnDef::new("lang", ColumnType::Text, false)],
    );
    storage
        .create_table(&text_only_schema)
        .expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let core = new_core(storage);
    let result = core
        .execute_sql(&ctx, "SELECT COUNT(*), MIN(lang) FROM notes")
        .expect("aggregate on a table without a VECTOR column should succeed");
    let cells = single_row(&result);
    assert_eq!(as_integer(&cells[0]), 0);
    assert_eq!(as_text_or_null(&cells[1]), None);
}

// Issue #995: 行を持つ `VECTOR` 列なしテーブルでの集計（`COUNT`／`MIN`）が
// 正しい値を返すことを確認する（上記テストの空集合契約に対する非空版）。
#[test]
fn sql13_works_on_non_empty_table_without_vector_column() {
    let path = unique_db_path("sql13-no-vector-nonempty");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let text_only_schema = TableSchema::new(
        "notes",
        vec![ColumnDef::new("lang", ColumnType::Text, false)],
    );
    storage
        .create_table(&text_only_schema)
        .expect("create table");
    // SQL `INSERT` は常に `Visibility::Private` で書き込む
    // （`sql::exec::execute_insert_with_schema` の固定挙動）ため、読み戻しには
    // `Private` を許可する ctx を使う（`Public` のみの `PolicyContext::new` では
    // 見えない）。
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let core = new_core(storage);
    for (id, lang, op_id) in [
        (1u64, "en", "op-1"),
        (2u64, "ja", "op-2"),
        (3u64, "ja", "op-3"),
    ] {
        core.execute_insert_sql(
            &ctx,
            &format!(
                "INSERT INTO notes (id, lang) VALUES ({id}, '{lang}') USING OPERATION_ID '{op_id}'"
            ),
        )
        .expect("insert into a table without a VECTOR column should succeed");
    }
    let result = core
        .execute_sql(&ctx, "SELECT COUNT(*), MIN(lang) FROM notes")
        .expect("aggregate on a non-empty table without a VECTOR column should succeed");
    let cells = single_row(&result);
    assert_eq!(as_integer(&cells[0]), 3);
    assert_eq!(as_text_or_null(&cells[1]), Some("en".to_string()));
}

// --- 拒否経路の決定性: 同一入力を 2 回実行して同じ wire_code -----------------------

#[test]
fn sql13_rejection_is_deterministic_across_repeated_calls() {
    let path = unique_db_path("sql13-determinism");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage.create_table(&schema()).expect("create table");
    let core = new_core(storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let sql = "SELECT SUM(embedding) FROM docs";
    let first = core
        .execute_sql(&ctx, sql)
        .expect_err("first call should fail");
    let second = core
        .execute_sql(&ctx, sql)
        .expect_err("second call should fail");
    assert_eq!(first.wire_code(), second.wire_code());
    assert_eq!(first.wire_code(), "42883");
}

/// Issue #1349（SQL-26・ERR-6）: 非数値型への集計は `42883`（undefined_function）。
/// 数値型・DATE/TIMESTAMP の既存挙動は変えない（正の対照）。拒否内容に他テナントの
/// 情報を含めない（RLS の対照）。
#[test]
fn non_numeric_aggregates_are_rejected_with_42883() {
    let path = unique_db_path("sql13-non-numeric-42883");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let _truths = seed_multi_tenant_corpus(&storage);
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a", true);

    for sql in [
        "SELECT SUM(lang) FROM docs",
        "SELECT AVG(lang) FROM docs",
        "SELECT SUM(embedding) FROM docs",
        "SELECT MIN(embedding) FROM docs",
        "SELECT SUM(lower(lang)) FROM docs",
        "SELECT AVG(vec_div(embedding, 2)) FROM docs",
        "SELECT MAX(vec_div(embedding, 2)) FROM docs",
    ] {
        let err = core.execute_sql(&ctx, sql).expect_err(sql);
        assert_eq!(err.wire_code(), "42883", "{sql}");
        let msg = format!("{err:?}");
        assert!(!msg.contains("tenant-b"), "{sql}: {msg}");
    }

    // 正の対照: TEXT の MIN/MAX・数値型・COUNT は受理のまま。
    for sql in [
        "SELECT MIN(lang) FROM docs",
        "SELECT MAX(lang) FROM docs",
        "SELECT COUNT(lang) FROM docs",
        "SELECT COUNT(embedding) FROM docs",
        "SELECT SUM(id) FROM docs",
        "SELECT AVG(vec_norm(embedding)) FROM docs",
    ] {
        core.execute_sql(&ctx, sql)
            .unwrap_or_else(|e| panic!("{sql} should succeed: {e:?}"));
    }
}
