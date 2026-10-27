//! `LAG`／`LEAD` の構文受理・値引数の束縛（SQL-64 (1)・Issue #1880）と評価（Issue #1881）の
//! 結合テスト。
//!
//! `tests/sql30_window.rs` と同じ流儀（`unique_db_path`＋`CleanupGuard`、公開 API の
//! `EngineCore::execute_sql` のみを使う）。期待値は PostgreSQL 17 の `lag`／`lead` の意味論
//! （オフセットは全行で評価・値は参照先の行の値で NULL は NULL のまま・既定値は参照先が
//! パーティション外のときだけ現在行で評価・負のオフセットは逆方向）から手で導く。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
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
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("score", ColumnType::Integer, false),
            ColumnDef::new("ratio", ColumnType::Double, false),
            ColumnDef::new("flag", ColumnType::Boolean, false),
        ],
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_row(storage: &Storage, ctx: &PolicyContext, id: u64, lang: &str, score: i32) {
    let op_id = engine::recovery::required_op_id::OperationId::parse(&format!(
        "test-op-{}-{id}",
        ctx.tenant_id()
    ))
    .expect("valid operation_id");
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        ctx,
        id,
        Visibility::Public,
        &[
            Value::Text(lang.to_string()),
            Value::Integer(score),
            Value::Double(0.5),
            Value::Bool(true),
        ],
        &op_id,
    )
    .expect("insert row");
}

fn setup(label: &str, rows: u64) -> (EngineCore, PolicyContext, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let ctx = ctx_for("tenant-a");
    storage.create_table(&schema()).expect("create table");
    for id in 1..=rows {
        let lang = if id % 2 == 0 { "ja" } else { "en" };
        insert_row(&storage, &ctx, id, lang, id as i32 * 10);
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (core, ctx, guard)
}

fn code(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> &'static str {
    match core.execute_sql(ctx, sql) {
        Ok(_) => panic!("expected {sql:?} to be rejected"),
        Err(e) => e.wire_code(),
    }
}

/// 成功なら `"OK"`、失敗なら SQLSTATE。
fn outcome(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> &'static str {
    match core.execute_sql(ctx, sql) {
        Ok(_) => "OK",
        Err(e) => e.wire_code(),
    }
}

/// 結果を `id` 昇順に並べ、`id` 以外の列を返す（先頭列が `id`）。
fn rows_by_id(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<Vec<Cell>> {
    let result = core
        .execute_sql(ctx, sql)
        .unwrap_or_else(|e| panic!("sql={sql}: {}", e.wire_code()));
    let mut rows: Vec<(u64, Vec<Cell>)> = result
        .rows
        .iter()
        .map(|r| {
            let id = match &r.cells[0] {
                Cell::Integer(v) => *v,
                other => panic!("expected id Cell::Integer, got {other:?}"),
            };
            (id, r.cells[1..].to_vec())
        })
        .collect();
    rows.sort_by_key(|(id, _)| *id);
    rows.into_iter().map(|(_, c)| c).collect()
}

/// 2 列目（`id` の次）だけを `id` 昇順で返す。
fn col1(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<Cell> {
    rows_by_id(core, ctx, sql)
        .into_iter()
        .map(|mut c| c.remove(0))
        .collect()
}

fn si(v: Option<i64>) -> Cell {
    v.map_or(Cell::Null, Cell::SignedInteger)
}

fn sis(vs: &[Option<i64>]) -> Vec<Cell> {
    vs.iter().copied().map(si).collect()
}

fn txt(v: Option<&str>) -> Cell {
    v.map_or(Cell::Null, |t| Cell::Text(t.to_string()))
}

/// `vals` 表: NULL を含む値・BIGINT のオフセット・0 除算を起こす除数・TEXT を持つ。
/// (id, grp, v, n, d, t)
type ValsRow = (
    u64,
    &'static str,
    Option<i32>,
    Option<i64>,
    Option<i32>,
    Option<&'static str>,
);

const VALS: [ValsRow; 5] = [
    (1, "a", Some(10), Some(1), Some(0), Some("x")),
    (2, "a", None, Some(1), Some(5), Some("y")),
    (3, "a", Some(30), Some(2), Some(0), None),
    (4, "b", Some(40), None, Some(7), Some("z")),
    (5, "b", Some(50), Some(5_000_000_000), Some(1), Some("w")),
];

fn setup_vals(label: &str) -> (EngineCore, PolicyContext, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let ctx = ctx_for("tenant-a");
    storage
        .create_table(&TableSchema::new(
            "vals",
            vec![
                ColumnDef::new("grp", ColumnType::Text, false),
                ColumnDef::new("v", ColumnType::Integer, true),
                ColumnDef::new("n", ColumnType::BigInt, true),
                ColumnDef::new("d", ColumnType::Integer, true),
                ColumnDef::new("t", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    for (id, grp, v, n, d, t) in VALS {
        let op_id = engine::recovery::required_op_id::OperationId::parse(&format!("vals-op-{id}"))
            .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            &storage,
            "vals",
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Text(grp.to_string()),
                v.map_or(Value::Null, Value::Integer),
                n.map_or(Value::Null, Value::BigInt),
                d.map_or(Value::Null, Value::Integer),
                t.map_or(Value::Null, |t| Value::Text(t.to_string())),
            ],
            &op_id,
        )
        .expect("insert row");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (core, ctx, guard)
}

/// 解析・束縛・型検査を通過した文は評価まで進んで成功する。
#[test]
fn accepted_forms_execute() {
    let (core, ctx, _g) = setup("lag-accept", 4);
    let accepted = [
        "SELECT id, lag(score) OVER (ORDER BY score) FROM docs LIMIT 10",
        "SELECT id, LEAD(score, 2) OVER (PARTITION BY lang ORDER BY id) FROM docs LIMIT 10",
        "SELECT id, lag(score, 1, 0) OVER (ORDER BY id) FROM docs LIMIT 10",
        "SELECT id, lag(score * 2, 1 + 1, -1) OVER (ORDER BY id) FROM docs LIMIT 10",
        "SELECT id, lag(score, score) OVER (ORDER BY id) FROM docs LIMIT 10",
        "SELECT id, lag(score, NULL) OVER (ORDER BY id) FROM docs LIMIT 10",
        "SELECT id, lag(score, 1, NULL) OVER (ORDER BY id) FROM docs LIMIT 10",
        "SELECT id, lag(lang, 1, 'none') OVER (ORDER BY id) FROM docs LIMIT 10",
        "SELECT id, Lead(score) OVER (ORDER BY id) AS nxt FROM docs LIMIT 10",
        // 擬似列 `id` はオフセットに使える（bigint 扱い）。
        "SELECT id, lag(score, id) OVER (ORDER BY id) FROM docs LIMIT 10",
        // BOOLEAN の裸の列は既定値でも宣言型で統一され、ゲートへ到達する。
        "SELECT id, lag(flag, 1, flag) OVER (ORDER BY id) FROM docs LIMIT 10",
    ];
    for sql in accepted {
        assert_eq!(outcome(&core, &ctx, sql), "OK", "sql={sql}");
    }
}

#[test]
fn offset_and_default_type_mismatch_is_42804() {
    let (core, ctx, _g) = setup("lag-type", 2);
    let cases = [
        "SELECT id, lag(score, 1.5) OVER (ORDER BY id) FROM docs LIMIT 10",
        "SELECT id, lag(score, lang) OVER (ORDER BY id) FROM docs LIMIT 10",
        "SELECT id, lag(score, flag) OVER (ORDER BY id) FROM docs LIMIT 10",
        "SELECT id, lag(score, 1, 'x') OVER (ORDER BY id) FROM docs LIMIT 10",
        "SELECT id, lag(lang, 1, 0) OVER (ORDER BY id) FROM docs LIMIT 10",
        "SELECT id, lag(score, 1, flag) OVER (ORDER BY id) FROM docs LIMIT 10",
        "SELECT id, lag(flag, 1, score) OVER (ORDER BY id) FROM docs LIMIT 10",
    ];
    for sql in cases {
        assert_eq!(code(&core, &ctx, sql), "42804", "sql={sql}");
    }
}

#[test]
fn argument_count_and_shape_errors() {
    let (core, ctx, _g) = setup("lag-shape", 2);
    assert_eq!(
        code(
            &core,
            &ctx,
            "SELECT id, lag() OVER (ORDER BY id) FROM docs LIMIT 10"
        ),
        "42883"
    );
    assert_eq!(
        code(
            &core,
            &ctx,
            "SELECT id, lag(score, 1, 0, 0) OVER (ORDER BY id) FROM docs LIMIT 10"
        ),
        "42883"
    );
    for sql in [
        "SELECT id, lag(*) OVER (ORDER BY id) FROM docs LIMIT 10",
        "SELECT id, lag(DISTINCT score) OVER (ORDER BY id) FROM docs LIMIT 10",
        "SELECT id, lag($1) OVER (ORDER BY id) FROM docs LIMIT 10",
    ] {
        assert_eq!(code(&core, &ctx, sql), "42601", "sql={sql}");
    }
}

#[test]
fn nested_window_and_aggregate_in_arguments() {
    let (core, ctx, _g) = setup("lag-nested", 2);
    assert_eq!(
        code(
            &core,
            &ctx,
            "SELECT id, lag(row_number() OVER ()) OVER (ORDER BY id) FROM docs LIMIT 10"
        ),
        "42P20"
    );
    assert_eq!(
        code(
            &core,
            &ctx,
            "SELECT id, lag(count(*)) OVER (ORDER BY id) FROM docs LIMIT 10"
        ),
        "42803"
    );
}

/// `LAG`／`LEAD` を名前表へ加えた副作用: `FILTER` 内の `lag(x) OVER ()` は
/// PostgreSQL と同じ分類（`42P20`）になる。
#[test]
fn lag_inside_filter_is_window_misuse() {
    let (core, ctx, _g) = setup("lag-misuse", 2);
    assert_eq!(
        code(
            &core,
            &ctx,
            "SELECT count(*) FILTER (WHERE lag(score) OVER () > 1) FROM docs"
        ),
        "42P20"
    );
}

#[test]
fn unsupported_value_types_are_0a000() {
    let path = unique_db_path("lag-vector");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "vecs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("tenant-a");
    assert_eq!(
        code(
            &core,
            &ctx,
            "SELECT id, lag(embedding) OVER (ORDER BY id) FROM vecs LIMIT 10"
        ),
        "0A000"
    );
}

/// 0 行の表は 0 行、行がある表は値を返す（行数で挙動が分岐しない）。
#[test]
fn evaluation_works_for_empty_and_populated_tables() {
    let sql = "SELECT id, lag(score) OVER (ORDER BY id) FROM docs LIMIT 10";
    let (core0, ctx, _g0) = setup("lag-empty", 0);
    let (core5, ctx5, _g5) = setup("lag-five", 5);
    assert!(rows_by_id(&core0, &ctx, sql).is_empty());
    assert_eq!(
        col1(&core5, &ctx5, sql),
        sis(&[None, Some(10), Some(20), Some(30), Some(40)])
    );
}

/// ビュー越しでは、非公開列を値・オフセット・既定値で参照すると `42703` で拒否される。
#[test]
fn view_column_scope_covers_all_arguments() {
    let (core, ctx, _g) = setup("lag-view", 3);
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "CREATE VIEW v AS SELECT id, lang FROM docs",
    )
    .expect("create view");
    let run = |sql: &str| {
        let mut s = SessionState::default();
        core.execute_sql_in_session(&ctx, &mut s, sql)
            .expect_err("rejected")
            .wire_code()
    };
    for sql in [
        "SELECT id, lag(score) OVER (ORDER BY id) FROM v LIMIT 10",
        "SELECT id, lag(lang, score) OVER (ORDER BY id) FROM v LIMIT 10",
        "SELECT id, lag(lang, 1, score) OVER (ORDER BY id) FROM v LIMIT 10",
        "SELECT id, lag(lang, 1, 'x' || score) OVER (ORDER BY id) FROM v LIMIT 10",
    ] {
        assert_eq!(run(sql), "42703", "sql={sql}");
    }
    // 公開列だけを使う形は評価まで進む。
    let mut s = SessionState::default();
    let ok = core
        .execute_sql_in_session(
            &ctx,
            &mut s,
            "SELECT id, lag(lang, 1, 'none') OVER (ORDER BY id) FROM v LIMIT 10",
        )
        .expect("public columns only");
    let SqlOutcome::Query(ok) = ok else {
        panic!("expected a query result");
    };
    assert_eq!(ok.rows.len(), 3);
}

/// 式層が束縛できない型の裸の列（NUMERIC・UUID）も既定値で宣言型判定され、
/// 同型なら成功、異型なら `42804`、運べない型（BYTEA）は `0A000` になる。
#[test]
fn bare_column_defaults_use_declared_types() {
    let path = unique_db_path("lag-declared");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "typed",
            vec![
                ColumnDef::new(
                    "n1",
                    ColumnType::Numeric {
                        precision: 10,
                        scale: 2,
                    },
                    false,
                ),
                ColumnDef::new(
                    "n2",
                    ColumnType::Numeric {
                        precision: 10,
                        scale: 2,
                    },
                    false,
                ),
                ColumnDef::new(
                    "n3",
                    ColumnType::Numeric {
                        precision: 12,
                        scale: 4,
                    },
                    false,
                ),
                ColumnDef::new(
                    "n4",
                    ColumnType::Numeric {
                        precision: 38,
                        scale: 0,
                    },
                    false,
                ),
                ColumnDef::new(
                    "n5",
                    ColumnType::Numeric {
                        precision: 38,
                        scale: 38,
                    },
                    false,
                ),
                ColumnDef::new(
                    "n6",
                    ColumnType::Numeric {
                        precision: 20,
                        scale: 10,
                    },
                    false,
                ),
                ColumnDef::new(
                    "n7",
                    ColumnType::Numeric {
                        precision: 30,
                        scale: 5,
                    },
                    false,
                ),
                ColumnDef::new("u1", ColumnType::Uuid, false),
                ColumnDef::new("b1", ColumnType::Bytea, false),
                ColumnDef::new("i1", ColumnType::Integer, false),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("tenant-a");
    let cases = [
        ("lag(n1, 1, n2)", "OK"),
        // typmod（精度・スケール）違いの NUMERIC 同士も統一できる（`42804` にしない）。
        ("lag(n1, 1, n3)", "OK"),
        ("lag(n3, 1, n1)", "OK"),
        // 統一型が精度上限（38）を超える組は不正な型を作らず fail-closed（42804）。
        ("lag(n4, 1, n5)", "42804"),
        ("lag(n5, 1, n4)", "42804"),
        // 上限内（整数部 25＋小数部 10＝35）は受理。
        ("lag(n6, 1, n7)", "OK"),
        ("lag(u1, 1, u1)", "OK"),
        ("lag(n1, 1, u1)", "42804"),
        ("lag(i1, 1, n1)", "42804"),
        ("lag(b1, 1, b1)", "0A000"),
        ("lag(i1, 1, b1)", "0A000"),
        ("lag(b1, 1, i1)", "0A000"),
    ];
    for (call, want) in cases {
        let sql = format!("SELECT id, {call} OVER (ORDER BY id) FROM typed LIMIT 10");
        assert_eq!(outcome(&core, &ctx, &sql), want, "sql={sql}");
    }
}

// ---------- 評価（Issue #1881） ----------

fn docs_sql(call: &str) -> String {
    format!("SELECT id, {call} FROM docs LIMIT 100")
}

/// 既定のオフセット 1。パーティションなしの先頭（LAG）・末尾（LEAD）は NULL。
#[test]
fn default_offset_and_partition_boundaries() {
    let (core, ctx, _g) = setup("lag-eval-basic", 4);
    let q = |c: &str| col1(&core, &ctx, &docs_sql(c));
    assert_eq!(
        q("lag(score) OVER (ORDER BY id)"),
        sis(&[None, Some(10), Some(20), Some(30)])
    );
    assert_eq!(
        q("lead(score) OVER (ORDER BY id)"),
        sis(&[Some(20), Some(30), Some(40), None])
    );
    // en: id1,id3 / ja: id2,id4。各パーティションの先頭・末尾が NULL。
    assert_eq!(
        q("lag(score) OVER (PARTITION BY lang ORDER BY id)"),
        sis(&[None, None, Some(10), Some(20)])
    );
    assert_eq!(
        q("lead(score) OVER (PARTITION BY lang ORDER BY id)"),
        sis(&[Some(30), Some(40), None, None])
    );
    // 降順の ORDER BY では前後が入れ替わる。
    assert_eq!(
        q("lag(score) OVER (ORDER BY id DESC)"),
        sis(&[Some(20), Some(30), Some(40), None])
    );
}

/// オフセットが 0・1 より大きい・負・パーティション行数以上。
#[test]
fn explicit_zero_large_negative_and_oversized_offsets() {
    let (core, ctx, _g) = setup("lag-eval-offsets", 4);
    let q = |c: &str| col1(&core, &ctx, &docs_sql(c));
    assert_eq!(
        q("lag(score, 2) OVER (ORDER BY id)"),
        sis(&[None, None, Some(10), Some(20)])
    );
    assert_eq!(
        q("lag(score, 0) OVER (ORDER BY id)"),
        sis(&[Some(10), Some(20), Some(30), Some(40)])
    );
    assert_eq!(
        q("lead(score, 0) OVER (ORDER BY id)"),
        sis(&[Some(10), Some(20), Some(30), Some(40)])
    );
    // 負のオフセットは逆方向（lag(x, -k) = lead(x, k)）。
    assert_eq!(
        q("lag(score, -1) OVER (ORDER BY id)"),
        q("lead(score, 1) OVER (ORDER BY id)")
    );
    assert_eq!(
        q("lead(score, -2) OVER (ORDER BY id)"),
        q("lag(score, 2) OVER (ORDER BY id)")
    );
    assert_eq!(
        q("lag(score, 10) OVER (ORDER BY id)"),
        sis(&[None, None, None, None])
    );
    assert_eq!(
        q("lag(score, 10, 99) OVER (ORDER BY id)"),
        sis(&[Some(99); 4])
    );
    assert_eq!(
        q("lead(score, 4, 99) OVER (ORDER BY id)"),
        sis(&[Some(99); 4])
    );
}

/// 既定値: 定数・現在行を参照する式・裸の列・NULL。
#[test]
fn default_value_forms() {
    let (core, ctx, _g) = setup("lag-eval-default", 4);
    let q = |c: &str| col1(&core, &ctx, &docs_sql(c));
    assert_eq!(
        q("lag(score, 1, 0) OVER (ORDER BY id)"),
        sis(&[Some(0), Some(10), Some(20), Some(30)])
    );
    // 既定値は参照先ではなく現在行で評価する。
    assert_eq!(
        q("lag(score, 1, score * 2) OVER (ORDER BY id)"),
        sis(&[Some(20), Some(10), Some(20), Some(30)])
    );
    assert_eq!(
        q("lead(score, 1, score) OVER (ORDER BY id)"),
        sis(&[Some(20), Some(30), Some(40), Some(40)])
    );
    assert_eq!(
        q("lag(score, 1, NULL) OVER (ORDER BY id)"),
        sis(&[None, Some(10), Some(20), Some(30)])
    );
    // TEXT と BOOLEAN（宣言型の裸の列）。
    assert_eq!(
        q("lag(lang, 1, 'none') OVER (ORDER BY id)"),
        vec![
            txt(Some("none")),
            txt(Some("en")),
            txt(Some("ja")),
            txt(Some("en")),
        ]
    );
    assert_eq!(
        q("lag(flag, 1, flag) OVER (ORDER BY id)"),
        vec![Cell::Bool(true); 4]
    );
    // 疑似列 `id`（BIGINT）。
    assert_eq!(
        q("lag(id) OVER (ORDER BY id)"),
        sis(&[None, Some(1), Some(2), Some(3)])
    );
}

/// NULL を含む値・オフセット。参照先の値が NULL でも既定値へは落とさない。
#[test]
fn null_values_and_null_offsets() {
    let (core, ctx, _g) = setup_vals("lag-eval-null");
    let q = |c: &str, w: &str| {
        col1(
            &core,
            &ctx,
            &format!("SELECT id, {c} FROM vals {w} LIMIT 100"),
        )
    };
    // id3 の参照先 id2 の v は NULL。既定値 99 にはならず NULL のまま（COALESCE ではない）。
    assert_eq!(
        q("lag(v) OVER (ORDER BY id)", ""),
        sis(&[None, Some(10), None, Some(30), Some(40)])
    );
    assert_eq!(
        q("lag(v, 1, 99) OVER (ORDER BY id)", ""),
        sis(&[Some(99), Some(10), None, Some(30), Some(40)])
    );
    // リテラル NULL のオフセットは既定値があっても NULL。
    assert_eq!(
        q("lag(v, NULL, 99) OVER (ORDER BY id)", ""),
        sis(&[None; 5])
    );
    // 行ごとに異なるオフセット。id4 は n が NULL なので結果も NULL（既定値は使わない）。
    assert_eq!(
        q("lag(v, n) OVER (ORDER BY id)", "WHERE id <= 4"),
        sis(&[None, Some(10), Some(10), None])
    );
    assert_eq!(
        q("lag(v, n, -1) OVER (ORDER BY id)", "WHERE id <= 4"),
        sis(&[Some(-1), Some(10), Some(10), None])
    );
    // パーティション単位。
    assert_eq!(
        q("lag(v, 1, -1) OVER (PARTITION BY grp ORDER BY id)", ""),
        sis(&[Some(-1), Some(10), None, Some(-1), Some(40)])
    );
    // TEXT の NULL 参照。
    assert_eq!(
        q("lag(t, 1, 'none') OVER (ORDER BY id)", ""),
        vec![
            txt(Some("none")),
            txt(Some("x")),
            txt(Some("y")),
            txt(None),
            txt(Some("z")),
        ]
    );
}

/// int4 範囲外のオフセットは `22003`（全行で評価されるため、他行が範囲内でも拒否される）。
#[test]
fn offset_outside_int4_is_22003() {
    let (core, ctx, _g) = setup_vals("lag-eval-range");
    for sql in [
        "SELECT id, lag(v, n) OVER (ORDER BY id) FROM vals LIMIT 100",
        "SELECT id, lead(v, n * -1) OVER (ORDER BY id) FROM vals LIMIT 100",
        "SELECT id, lag(v, 2147483648) OVER (ORDER BY id) FROM vals LIMIT 100",
        "SELECT id, lag(v, -2147483649) OVER (ORDER BY id) FROM vals LIMIT 100",
    ] {
        assert_eq!(code(&core, &ctx, sql), "22003", "sql={sql}");
    }
    // 範囲の端は受理される。
    assert_eq!(
        outcome(
            &core,
            &ctx,
            "SELECT id, lag(v, 2147483647) OVER (ORDER BY id) FROM vals LIMIT 100"
        ),
        "OK"
    );
}

/// 値式のエラーは参照先になった行でだけ、既定値式のエラーは範囲外を使う行でだけ起きる。
#[test]
fn value_and_default_errors_are_lazy() {
    let (core, ctx, _g) = setup_vals("lag-eval-lazy");
    let run = |sql: &str| outcome(&core, &ctx, sql);
    // id3 は d = 0（0 除算）。id3 が先頭になる範囲では lead の参照先にならず成功する。
    let ok = "SELECT id, lead(100 / d) OVER (ORDER BY id) FROM vals WHERE id >= 3 LIMIT 100";
    assert_eq!(run(ok), "OK");
    assert_eq!(col1(&core, &ctx, ok), sis(&[Some(14), Some(100), None]));
    // id2..: id2 の参照先が id3（d = 0）になるためエラー。
    assert_eq!(
        run("SELECT id, lead(100 / d) OVER (ORDER BY id) FROM vals WHERE id >= 2 LIMIT 100"),
        "22012"
    );
    // 既定値式: id2 から始めれば先頭行（id2）の d は 5 で成功。id3（d = 0）は範囲内を参照する。
    let dflt = "SELECT id, lag(v, 1, 100 / d) OVER (ORDER BY id) FROM vals WHERE id >= 2 LIMIT 100";
    assert_eq!(
        col1(&core, &ctx, dflt),
        sis(&[Some(20), None, Some(30), Some(40)])
    );
    // id1（d = 0）が先頭で既定値を使うため 22012。
    assert_eq!(
        run("SELECT id, lag(v, 1, 100 / d) OVER (ORDER BY id) FROM vals LIMIT 100"),
        "22012"
    );
}

/// 結果行の切り出し（`LIMIT`／`OFFSET`・`WHERE`）の扱い。
#[test]
fn limit_offset_and_where_population() {
    let (core, ctx, _g) = setup("lag-eval-limit", 5);
    // LIMIT／OFFSET は全行で評価した後に切り出す（id3 の LAG は切り出し前の id2 を参照する）。
    assert_eq!(
        col1(
            &core,
            &ctx,
            "SELECT id, lag(score) OVER (ORDER BY id) FROM docs ORDER BY id LIMIT 2 OFFSET 2"
        ),
        sis(&[Some(20), Some(30)])
    );
    // WHERE で除外した行は母集合に現れない（参照先にならない）。
    assert_eq!(
        col1(
            &core,
            &ctx,
            "SELECT id, lag(score) OVER (ORDER BY id) FROM docs WHERE score <> 20 LIMIT 100"
        ),
        sis(&[None, Some(10), Some(30), Some(40)])
    );
}

/// `row_number()` と並べても列順・値が崩れない。
#[test]
fn mixed_with_other_window_functions() {
    let (core, ctx, _g) = setup("lag-eval-mixed", 3);
    let rows = rows_by_id(
        &core,
        &ctx,
        "SELECT id, row_number() OVER (ORDER BY id), lag(score) OVER (ORDER BY id), \
         lead(score, 1, -1) OVER (ORDER BY id) FROM docs LIMIT 100",
    );
    assert_eq!(rows.len(), 3);
    let expect = [(1u64, None, 20i64), (2, Some(10), 30), (3, Some(20), -1)];
    for (row, (rn, lag, lead)) in rows.iter().zip(expect) {
        assert_eq!(row[0], Cell::Integer(rn));
        assert_eq!(row[1], si(lag));
        assert_eq!(row[2], si(Some(lead)));
    }
}

fn insert_with(
    storage: &Storage,
    ctx: &PolicyContext,
    id: u64,
    visibility: Visibility,
    score: i32,
) {
    let op_id = engine::recovery::required_op_id::OperationId::parse(&format!(
        "iw-op-{}-{id}",
        ctx.tenant_id()
    ))
    .expect("valid operation_id");
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        ctx,
        id,
        visibility,
        &[
            Value::Text("en".to_string()),
            Value::Integer(score),
            Value::Double(0.5),
            Value::Bool(true),
        ],
        &op_id,
    )
    .expect("insert row");
}

/// テナント境界と可視性: 他テナントの Private 行・不可視の行は参照先にならない（RLS-10）。
#[test]
fn other_tenants_and_invisible_rows_never_become_targets() {
    let path = unique_db_path("lag-eval-rls");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let full = ctx_for("tenant-a");
    let public_only =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public]).expect("ctx");
    let other = ctx_for("tenant-b");
    insert_with(&storage, &full, 1, Visibility::Public, 10);
    insert_with(&storage, &full, 2, Visibility::Private, 20);
    insert_with(&storage, &full, 3, Visibility::Public, 30);
    // 他テナントの Private 行は tenant-a から見えない。
    insert_with(&storage, &other, 11, Visibility::Private, 1000);
    insert_with(&storage, &other, 12, Visibility::Private, 2000);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let sql = "SELECT id, lag(score, 1, -1) OVER (ORDER BY id), lead(score, 1, -1) \
               OVER (ORDER BY id) FROM docs LIMIT 100";
    // 全可視: tenant-a の 3 行のみ。
    assert_eq!(
        rows_by_id(&core, &full, sql),
        vec![
            vec![si(Some(-1)), si(Some(20))],
            vec![si(Some(10)), si(Some(30))],
            vec![si(Some(20)), si(Some(-1))],
        ]
    );
    // Public のみ: Private の id2 は参照先にならず、id3 の LAG は id1 を指す。
    assert_eq!(
        rows_by_id(&core, &public_only, sql),
        vec![
            vec![si(Some(-1)), si(Some(30))],
            vec![si(Some(10)), si(Some(-1))],
        ]
    );
    // tenant-b からは tenant-a の Public 行（id1・id3）と自分の Private 行だけが母集合になり、
    // tenant-a の Private 行（id2。値 20）は現れない。
    assert_eq!(
        rows_by_id(&core, &other, sql),
        vec![
            vec![si(Some(-1)), si(Some(30))],
            vec![si(Some(10)), si(Some(1000))],
            vec![si(Some(30)), si(Some(2000))],
            vec![si(Some(1000)), si(Some(-1))],
        ]
    );
}

/// 評価後射影形ビュー（`LIMIT` 本文）越しでも、引数にしか現れない列が NULL へ落ちない。
#[test]
fn lag_argument_column_survives_buffered_view() {
    let (core, ctx, _g) = setup("lag-eval-view", 4);
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "CREATE VIEW top3 AS SELECT id, score FROM docs ORDER BY id LIMIT 3",
    )
    .expect("create view");
    let mut s = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut s,
            "SELECT id, lag(score) OVER (ORDER BY id) FROM top3 LIMIT 100",
        )
        .expect("lag over a buffered view");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected a query result");
    };
    let mut rows: Vec<(u64, Cell)> = result
        .rows
        .iter()
        .map(|r| match &r.cells[0] {
            Cell::Integer(id) => (*id, r.cells[1].clone()),
            other => panic!("expected id, got {other:?}"),
        })
        .collect();
    rows.sort_by_key(|(id, _)| *id);
    let got: Vec<Cell> = rows.into_iter().map(|(_, c)| c).collect();
    assert_eq!(got, sis(&[None, Some(10), Some(20)]));
}
