//! ウィンドウ集計の式引数の結合テスト（SQL-64・Issue #1895。関連: SQL-57・RLS-10）。
//!
//! 期待値は PostgreSQL 17 の意味論から手で導いた固定値で、PostgreSQL 実機での照合は未実施。
//! 式引数の結果型は集計 SELECT と同じ既存契約（式は倍精度）に従う。
//! `unique_db_path`＋`CleanupGuard`、`EngineCore::execute_sql` の production 経路で検証する。
//!
//! フィクスチャ（`sql64_window_filter.rs` と同じ）: id 1..8 の `k = [3, 1, 2, 5, NULL, 1, 4, 9]`、
//! `g = [a, a, a, b, b, c, b, a]`、`s = [x, NULL, y, z, w, NULL, x, x]`。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::Cell;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "t";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("g", ColumnType::Text, true),
            ColumnDef::new("s", ColumnType::Text, true),
            ColumnDef::new("k", ColumnType::Integer, true),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// `(id, g, s, k)`。
type Seed = (u64, &'static str, Option<&'static str>, Option<i32>);

const SEEDS: [Seed; 8] = [
    (1, "a", Some("x"), Some(3)),
    (2, "a", None, Some(1)),
    (3, "a", Some("y"), Some(2)),
    (4, "b", Some("z"), Some(5)),
    (5, "b", Some("w"), None),
    (6, "c", None, Some(1)),
    (7, "b", Some("x"), Some(4)),
    (8, "a", Some("x"), Some(9)),
];

fn insert_t(storage: &Storage, tenant: &str, seed: &Seed, vis: Visibility) {
    let (id, g, s, k) = *seed;
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx(tenant),
        id,
        vis,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(g.to_string()),
            s.map_or(Value::Null, |v| Value::Text(v.to_string())),
            k.map_or(Value::Null, Value::Integer),
        ],
        &OperationId::parse(&format!("seed-{tenant}-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

fn build_core(tag: &str) -> (EngineCore, CleanupGuard) {
    build_core_with(tag, &[])
}

/// `extra` は tenant-a の Public 行へ追加するシード（ゼロ除算の行を足すのに使う）。
fn build_core_with(tag: &str, extra: &[Seed]) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(tag);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    for seed in &SEEDS {
        insert_t(&storage, "tenant-a", seed, Visibility::Public);
    }
    for seed in extra {
        insert_t(&storage, "tenant-a", seed, Visibility::Public);
    }
    // 他テナントの Private 行（tenant-a からは見えてはならない）。
    insert_t(
        &storage,
        "tenant-b",
        &(100, "a", Some("SECRET"), Some(1000)),
        Visibility::Private,
    );
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn cell_str(c: &Cell) -> String {
    match c {
        Cell::Null => "NULL".to_string(),
        Cell::Integer(n) => n.to_string(),
        Cell::Float(v) => v.to_string(),
        Cell::SignedInteger(n) => n.to_string(),
        Cell::Text(s) => s.clone(),
        other => format!("{other:?}"),
    }
}

/// `id` 昇順に並べた、`id` を除く列の文字列表現（1 行 1 要素。複数列は `,` 区切り）。
fn by_id(core: &EngineCore, sql: &str) -> Vec<String> {
    let r = core
        .execute_sql(&ctx("tenant-a"), sql)
        .unwrap_or_else(|e| panic!("query failed: {sql}: {}", e.wire_code()));
    let mut rows: Vec<(u64, String)> = r
        .rows
        .iter()
        .map(|row| {
            let id = cell_str(&row.cells[0]).parse::<u64>().expect("numeric id");
            let rest: Vec<String> = row.cells.iter().skip(1).map(cell_str).collect();
            (id, rest.join(","))
        })
        .collect();
    rows.sort_by_key(|(id, _)| *id);
    rows.into_iter().map(|(_, v)| v).collect()
}

fn win(core: &EngineCore, expr: &str) -> Vec<String> {
    by_id(core, &format!("SELECT id, {expr} FROM t LIMIT 100"))
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}

fn code_of(core: &EngineCore, sql: &str) -> &'static str {
    core.execute_sql(&ctx("tenant-a"), sql)
        .expect_err(&format!("query must fail: {sql}"))
        .wire_code()
}

fn ddl(core: &EngineCore, sql: &str) {
    let mut session = engine::sql::mode::SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("tenant-a"), &mut session, sql)
        .unwrap_or_else(|e| panic!("ddl failed: {sql}: {}", e.wire_code()));
}

/// 列と定数の算術式を引数にとる集計（PARTITION BY・全体・累積）。NULL は集計対象外。
#[test]
fn arithmetic_expression_arguments() {
    let (core, _g) = build_core("sql64-we-arith");
    assert_eq!(
        win(&core, "sum(k + 1) OVER (PARTITION BY g)"),
        strs(&["19", "19", "19", "11", "11", "2", "11", "19"])
    );
    assert_eq!(win(&core, "sum(k * 2) OVER ()"), strs(&["50"; 8]));
    assert_eq!(
        win(&core, "sum(-k) OVER (ORDER BY id)"),
        strs(&["-3", "-4", "-6", "-11", "-11", "-12", "-16", "-25"])
    );
    assert_eq!(win(&core, "count(k + 1) OVER ()"), strs(&["7"; 8]));
    assert_eq!(win(&core, "sum(abs(k - 3)) OVER ()"), strs(&["14"; 8]));
}

/// TEXT 式（TypedExpr）の COUNT／MIN／MAX。
#[test]
fn text_expression_arguments() {
    let (core, _g) = build_core("sql64-we-text");
    assert_eq!(win(&core, "count(upper(s)) OVER ()"), strs(&["6"; 8]));
    assert_eq!(
        win(&core, "max(upper(s)) OVER (PARTITION BY g)"),
        strs(&["Y", "Y", "Y", "Z", "Z", "NULL", "Z", "Y"])
    );
    assert_eq!(win(&core, "min(upper(s)) OVER ()"), strs(&["W"; 8]));
}

/// 疑似列 `id` を含む式。
#[test]
fn pseudo_id_expression() {
    let (core, _g) = build_core("sql64-we-id");
    assert_eq!(win(&core, "sum(id * 2) OVER ()"), strs(&["72"; 8]));
}

/// embedding を参照する式（デコード対象の導出を検証）。`embedding = [id, 0]` なのでノルムは id。
#[test]
fn embedding_expression() {
    let (core, _g) = build_core("sql64-we-emb");
    assert_eq!(
        win(&core, "sum(vec_norm(embedding)) OVER ()"),
        strs(&["36"; 8])
    );
}

/// 文の WHERE が参照しない列だけを式が読む（`scalar_mask` の反映漏れの回帰検出）。
#[test]
fn expression_columns_outside_where() {
    let (core, _g) = build_core("sql64-we-mask");
    assert_eq!(
        by_id(
            &core,
            "SELECT id, sum(k + 1) OVER () FROM t WHERE g = 'a' LIMIT 100"
        ),
        strs(&["19"; 4])
    );
}

/// フレームとの組み合わせ（ROWS・GROUPS・EXCLUDE・RANGE）。
#[test]
fn frames_with_expression_arguments() {
    let (core, _g) = build_core("sql64-we-frames");
    assert_eq!(
        win(
            &core,
            "sum(k + 1) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW)"
        ),
        strs(&["4", "6", "5", "9", "6", "2", "7", "15"])
    );
    assert_eq!(
        win(
            &core,
            "sum(k + 1) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING \
             EXCLUDE CURRENT ROW)"
        ),
        strs(&["2", "7", "8", "3", "8", "5", "12", "5"])
    );
    assert_eq!(
        win(
            &core,
            "sum(k + 1) OVER (ORDER BY id RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)"
        ),
        strs(&["4", "6", "9", "15", "15", "17", "22", "32"])
    );
    assert_eq!(
        win(
            &core,
            "count(k + 1) OVER (ORDER BY id GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW)"
        ),
        strs(&["1", "2", "2", "2", "1", "1", "2", "2"])
    );
}

/// FILTER との併用: 除外行の引数は評価されない（ゼロ除算の行があっても `22012` にならない）。
#[test]
fn filter_skips_argument_evaluation() {
    let (core, _g) = build_core_with("sql64-we-filter", &[(9, "c", Some("q"), Some(0))]);
    assert_eq!(
        code_of(&core, "SELECT id, sum(10 / k) OVER () FROM t LIMIT 100"),
        "22012"
    );
    let got = win(&core, "count(10 / k) FILTER (WHERE k <> 0) OVER ()");
    assert_eq!(got, strs(&["7"; 9]));
    // どのフレームにも入らない行の評価エラーは表面化しない（id9 は誰の先行行にもならない）。
    let got = win(
        &core,
        "count(10 / k) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING)",
    );
    assert_eq!(got.len(), 9);
}

/// 型検査と先読み検査の SQLSTATE。
#[test]
fn error_contract() {
    let (core, _g) = build_core("sql64-we-err");
    let q = |expr: &str| format!("SELECT id, {expr} FROM t LIMIT 100");
    assert_eq!(code_of(&core, &q("sum(upper(s)) OVER ()")), "42883");
    assert_eq!(
        code_of(&core, &q("sum(row_number() OVER ()) OVER ()")),
        "42P20"
    );
    assert_eq!(code_of(&core, &q("sum(count(*)) OVER ()")), "42803");
    assert_eq!(code_of(&core, &q("sum($1) OVER ()")), "42601");
    assert_eq!(code_of(&core, &q("count(DISTINCT k + 1) OVER ()")), "42601");
    assert_eq!(code_of(&core, &q("sum(k + nosuch) OVER ()")), "42703");
    assert_eq!(code_of(&core, &q("sum(k, k) OVER ()")), "42601");
    assert_eq!(code_of(&core, &q("sum() OVER ()")), "42601");
}

/// RLS: 他テナントの Private 行（k = 1000）は式の評価にも集計の母集合にも現れない。
#[test]
fn other_tenant_rows_do_not_affect_results() {
    let (core, _g) = build_core("sql64-we-rls");
    assert_eq!(win(&core, "sum(k + 1) OVER ()"), strs(&["32"; 8]));
}

/// ビュー越し: 非公開列を式に含めると 42703、公開列だけの式は動く。
#[test]
fn view_exposes_only_public_columns() {
    let (core, _g) = build_core("sql64-we-view");
    ddl(&core, "CREATE VIEW pv AS SELECT id, g, k FROM t");
    assert_eq!(
        code_of(
            &core,
            "SELECT id, count(upper(s)) OVER () FROM pv LIMIT 100"
        ),
        "42703"
    );
    assert_eq!(
        by_id(
            &core,
            "SELECT id, sum(k + 1) OVER (PARTITION BY g) FROM pv LIMIT 100"
        ),
        strs(&["19", "19", "19", "11", "11", "2", "11", "19"])
    );
}

/// 評価後射影形（バッファ済み）ビュー越し: 式が通常のスカラー列だけを読むなら受理される。
#[test]
fn buffered_view_scalar_expression() {
    let (core, _g) = build_core("sql64-we-buffered");
    ddl(
        &core,
        "CREATE VIEW top4 AS SELECT id, k FROM t ORDER BY id LIMIT 4",
    );
    assert_eq!(
        by_id(
            &core,
            "SELECT id, sum(k + 1) OVER (ORDER BY id) FROM top4 LIMIT 100"
        ),
        strs(&["4", "6", "9", "15"])
    );
}
