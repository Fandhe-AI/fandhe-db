//! 集計 SELECT の結果行に窓関数をかける結合テスト（SQL-64・Issue #1898。関連: SQL-13・SQL-14・
//! SQL-58・RLS-10 (b)）。
//!
//! Issue #1899: 窓の引数・`PARTITION BY`・`ORDER BY` に集計式を書く形も扱う（本文へ巻き上げる）。
//!
//! 評価順は「WHERE → GROUP BY → HAVING → 窓 → ORDER BY → LIMIT/OFFSET」で、窓の母集合は集計後の
//! 結果行（1 グループ = 1 行）。期待値は PostgreSQL 17 の意味論から手で導いた固定値で、PostgreSQL
//! 実機での照合は未実施。`unique_db_path`＋`CleanupGuard`、`EngineCore::execute_sql_in_session` の
//! production 経路で検証する。
//!
//! フィクスチャ: tenant-a の公開行 id 1..8（`g = [a, a, a, b, b, c, b, a]`、
//! `k = [3, 1, 2, 5, NULL, 1, 4, 9]`）。tenant-b の非公開行（id 100 は `g = a`・id 101 は `g = z`）は
//! tenant-a から見えてはならない。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::result_limits::ResultLimits;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
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
            ColumnDef::new("k", ColumnType::Integer, true),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// `(id, g, k)`。
type Seed = (u64, &'static str, Option<i32>);

const SEEDS: [Seed; 8] = [
    (1, "a", Some(3)),
    (2, "a", Some(1)),
    (3, "a", Some(2)),
    (4, "b", Some(5)),
    (5, "b", None),
    (6, "c", Some(1)),
    (7, "b", Some(4)),
    (8, "a", Some(9)),
];

fn insert_t(storage: &Storage, tenant: &str, seed: &Seed, vis: Visibility) {
    let (id, g, k) = *seed;
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx(tenant),
        id,
        vis,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(g.to_string()),
            k.map_or(Value::Null, Value::Integer),
        ],
        &OperationId::parse(&format!("seed-{tenant}-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

fn build(tag: &str, limits: Option<ResultLimits>) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(tag);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    for seed in &SEEDS {
        insert_t(&storage, "tenant-a", seed, Visibility::Public);
    }
    insert_t(
        &storage,
        "tenant-b",
        &(100, "a", Some(1000)),
        Visibility::Private,
    );
    insert_t(
        &storage,
        "tenant-b",
        &(101, "z", Some(7)),
        Visibility::Private,
    );
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let core = match limits {
        Some(l) => core.with_result_limits(l),
        None => core,
    };
    (core, guard)
}

fn cell_str(c: &Cell) -> String {
    match c {
        Cell::Null => "NULL".to_string(),
        Cell::Integer(n) => n.to_string(),
        Cell::SignedInteger(n) => n.to_string(),
        Cell::Text(s) => s.clone(),
        Cell::Numeric(d) => d.to_string(),
        other => format!("{other:?}"),
    }
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<QueryResult, String> {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(&ctx(tenant), &mut session, sql) {
        Ok(SqlOutcome::Query(r)) => Ok(r),
        Ok(other) => panic!("expected Query outcome for {sql}, got {other:?}"),
        Err(e) => Err(e.wire_code().to_string()),
    }
}

/// 結果を 1 行 1 要素（セルを `,` 区切り）の文字列にする。順序は本文の `ORDER BY` が決める。
fn rows(core: &EngineCore, tenant: &str, sql: &str) -> Vec<String> {
    let r = run(core, tenant, sql).unwrap_or_else(|c| panic!("{sql}: {c}"));
    r.rows
        .iter()
        .map(|row| row.cells.iter().map(cell_str).collect::<Vec<_>>().join(","))
        .collect()
}

/// RowDescription に載る列名。
fn names(core: &EngineCore, tenant: &str, sql: &str) -> Vec<String> {
    let r = run(core, tenant, sql).unwrap_or_else(|c| panic!("{sql}: {c}"));
    r.columns
        .iter()
        .map(|c| match c {
            ColumnMeta::Scalar { name, .. } | ColumnMeta::Computed { name, .. } => name.clone(),
            other => format!("{other:?}"),
        })
        .collect()
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    run(core, tenant, sql).expect_err(sql)
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}

const A: &str = "tenant-a";

/// 窓の母集合は集計後の結果行（1 グループ = 1 行）。列順・列名は書いたとおりに復元される。
#[test]
fn window_runs_over_grouped_rows_with_original_column_order() {
    let (core, _g) = build("sql64-gw-order", None);
    let q = "SELECT g, count(*), row_number() OVER (ORDER BY g) FROM t GROUP BY g ORDER BY g";
    assert_eq!(rows(&core, A, q), strs(&["a,4,1", "b,3,2", "c,1,3"]));
    assert_eq!(names(&core, A, q), strs(&["g", "count", "row_number"]));
    // 窓項目が先頭にある列順。
    let q = "SELECT row_number() OVER (ORDER BY g), g, count(*) FROM t GROUP BY g ORDER BY g";
    assert_eq!(rows(&core, A, q), strs(&["1,a,4", "2,b,3", "3,c,1"]));
    assert_eq!(names(&core, A, q), strs(&["row_number", "g", "count"]));
    // 窓の引数・PARTITION BY・ORDER BY が参照するだけのキー列は結果へ漏れない（2 列のまま）。
    let q = "SELECT count(*), rank() OVER (ORDER BY g) FROM t GROUP BY g ORDER BY g";
    assert_eq!(rows(&core, A, q), strs(&["4,1", "3,2", "1,3"]));
    assert_eq!(names(&core, A, q), strs(&["count", "rank"]));
    // 非窓項目がキーだけ（集計項目なし）でも受理される。
    let q = "SELECT g, rank() OVER (ORDER BY g) FROM t GROUP BY g ORDER BY g";
    assert_eq!(rows(&core, A, q), strs(&["a,1", "b,2", "c,3"]));
    let q = "SELECT rank() OVER (ORDER BY g) FROM t GROUP BY g ORDER BY g";
    assert_eq!(rows(&core, A, q), strs(&["1", "2", "3"]));
    assert_eq!(names(&core, A, q), strs(&["rank"]));
}

/// 窓集計・`lag`・`PARTITION BY`・`FILTER` はグループ化キーを引数／キーにできる。
#[test]
fn window_functions_over_group_keys() {
    let (core, _g) = build("sql64-gw-funcs", None);
    let q = "SELECT g, count(*), count(*) OVER () FROM t GROUP BY g ORDER BY g";
    assert_eq!(rows(&core, A, q), strs(&["a,4,3", "b,3,3", "c,1,3"]));
    let q = "SELECT g, k, count(*), sum(k) OVER (PARTITION BY g ORDER BY k) \
             FROM t GROUP BY g, k ORDER BY g, k";
    assert_eq!(
        rows(&core, A, q),
        strs(&[
            "a,1,1,1",
            "a,2,1,3",
            "a,3,1,6",
            "a,9,1,15",
            "b,4,1,4",
            "b,5,1,9",
            "b,NULL,1,9",
            "c,1,1,1"
        ])
    );
    let q = "SELECT g, count(*), lag(g) OVER (ORDER BY g) FROM t GROUP BY g ORDER BY g";
    assert_eq!(rows(&core, A, q), strs(&["a,4,NULL", "b,3,a", "c,1,b"]));
    let q = "SELECT g, count(*) FILTER (WHERE g = 'a') OVER (ORDER BY g) \
             FROM t GROUP BY g ORDER BY g";
    assert_eq!(rows(&core, A, q), strs(&["a,1", "b,1", "c,1"]));
    // 同名の集計列が複数あっても曖昧にならない。
    let q = "SELECT g, sum(k), sum(k) AS s2, rank() OVER (ORDER BY g) FROM t GROUP BY g ORDER BY g";
    assert_eq!(
        rows(&core, A, q),
        strs(&["a,15,15,1", "b,9,9,2", "c,1,1,3"])
    );
    assert_eq!(names(&core, A, q), strs(&["g", "sum", "s2", "rank"]));
    // 出力別名と窓の別名は利用者が書いたとおり。
    let q = "SELECT g AS x, count(*), rank() OVER (ORDER BY g DESC) AS r \
             FROM t GROUP BY g ORDER BY g";
    assert_eq!(rows(&core, A, q), strs(&["a,4,3", "b,3,2", "c,1,1"]));
    assert_eq!(names(&core, A, q), strs(&["x", "count", "r"]));
}

/// 窓の `ORDER BY` が書く名前は入力列（グループ化キー）であり、出力別名ではない。
#[test]
fn window_name_resolves_to_input_column_not_output_alias() {
    let (core, _g) = build("sql64-gw-alias", None);
    // 出力列 `k` は `g` の別名。窓の `ORDER BY k` は入力列 k（整数）で順位付けする。
    let q = "SELECT g AS k, count(*), rank() OVER (ORDER BY k) FROM t GROUP BY g, k ORDER BY 1";
    let mut got = rows(&core, A, q);
    got.sort();
    assert_eq!(
        got,
        strs(&["a,1,1", "a,1,3", "a,1,4", "a,1,7", "b,1,5", "b,1,6", "b,1,8", "c,1,1"])
    );
    // 出力別名だけを窓が参照する形は未定義列（PostgreSQL と同じ）。
    assert_eq!(
        code(
            &core,
            A,
            "SELECT g AS x, count(*), rank() OVER (ORDER BY x) FROM t GROUP BY g"
        ),
        "42703"
    );
}

/// `GROUP BY` なしの集計と `HAVING`、`WHERE`、文全体の `ORDER BY`・`LIMIT`・`OFFSET`。
#[test]
fn single_group_having_where_order_limit() {
    let (core, _g) = build("sql64-gw-clauses", None);
    assert_eq!(
        rows(&core, A, "SELECT count(*), count(*) OVER () FROM t"),
        strs(&["8,1"])
    );
    // HAVING が偽なら 0 行（窓の母集合も空）。
    assert!(rows(
        &core,
        A,
        "SELECT count(*), count(*) OVER () FROM t HAVING count(*) > 100"
    )
    .is_empty());
    assert_eq!(
        rows(
            &core,
            A,
            "SELECT count(*), count(*) OVER () FROM t HAVING count(*) > 1"
        ),
        strs(&["8,1"])
    );
    // HAVING で落ちたグループは窓の母集合に入らない。
    let q = "SELECT g, count(*), rank() OVER (ORDER BY g) FROM t GROUP BY g \
             HAVING count(*) > 1 ORDER BY g";
    assert_eq!(rows(&core, A, q), strs(&["a,4,1", "b,3,2"]));
    // WHERE は集計前に掛かる。
    let q = "SELECT g, count(*), count(*) OVER () AS total FROM t WHERE k > 1 \
             GROUP BY g ORDER BY 2 DESC";
    assert_eq!(rows(&core, A, q), strs(&["a,3,2", "b,2,2"]));
    // LIMIT／OFFSET は窓の後: `total` は LIMIT 前の全グループ（3）で計算される。
    let q = "SELECT g, count(*), count(*) OVER () AS total FROM t GROUP BY g \
             ORDER BY count(*) DESC LIMIT 1 OFFSET 1";
    assert_eq!(rows(&core, A, q), strs(&["b,3,3"]));
    // 序数キーは書いたリストの位置（窓項目より後ろ）を指す。
    let q = "SELECT rank() OVER (ORDER BY g), g, count(*) FROM t GROUP BY g ORDER BY 3 DESC, 2";
    assert_eq!(rows(&core, A, q), strs(&["1,a,4", "2,b,3", "3,c,1"]));
}

/// 窓の母集合は参照者自身の可視行から作ったグループだけ（RLS-10 (b)）。
#[test]
fn tenant_boundary_is_preserved() {
    let (core, _g) = build("sql64-gw-rls", None);
    let q = "SELECT g, count(*), count(*) OVER (), rank() OVER (ORDER BY g) \
             FROM t GROUP BY g ORDER BY g";
    // tenant-a と無関係なテナント（公開行だけ）には tenant-b の非公開グループ z も行 100 も見えない。
    let expected_public = strs(&["a,4,3,1", "b,3,3,2", "c,1,3,3"]);
    assert_eq!(rows(&core, A, q), expected_public);
    assert_eq!(rows(&core, "tenant-c", q), expected_public);
    // tenant-b は自分の非公開行（a に 1 行、z に 1 行）を含めて 4 グループになる。
    assert_eq!(
        rows(&core, "tenant-b", q),
        strs(&["a,5,4,1", "b,3,4,2", "c,1,4,3", "z,1,4,4"])
    );
}

/// グループ数が `--max-result-rows` を超えたら部分結果なしで `54000`（黙って切り詰めない）。
#[test]
fn group_count_over_cap_is_rejected() {
    let (core, _g) = build("sql64-gw-cap", Some(ResultLimits::new(2)));
    assert_eq!(
        code(
            &core,
            A,
            "SELECT g, count(*), rank() OVER (ORDER BY g) FROM t GROUP BY g"
        ),
        "54000"
    );
}

/// 窓の `ORDER BY`・`PARTITION BY`・引数に集計式を書ける（Issue #1899）。
#[test]
fn aggregate_inside_window() {
    let (core, _g) = build("sql64-gw-agg-in-window", None);
    // グループ a: sum 15 / count 4 / max 9、b: 9 / 3 / 5、c: 1 / 1 / 1。
    let q = "SELECT g, sum(k), rank() OVER (ORDER BY sum(k) DESC) FROM t GROUP BY g ORDER BY g";
    assert_eq!(rows(&core, A, q), strs(&["a,15,1", "b,9,2", "c,1,3"]));
    assert_eq!(names(&core, A, q), strs(&["g", "sum", "rank"]));
    let q = "SELECT g, rank() OVER (ORDER BY count(*)) FROM t GROUP BY g ORDER BY g";
    assert_eq!(rows(&core, A, q), strs(&["a,3", "b,2", "c,1"]));
    assert_eq!(names(&core, A, q), strs(&["g", "rank"]));
    // FILTER 付き集計と修飾子（max は a=9・b=5・c=NULL）。
    let f = "max(k) FILTER (WHERE k > 4)";
    let q = format!("SELECT g, rank() OVER (ORDER BY {f}) FROM t GROUP BY g ORDER BY g");
    assert_eq!(rows(&core, A, &q), strs(&["a,2", "b,1", "c,3"]));
    let q = format!("SELECT g, rank() OVER (ORDER BY {f} DESC) FROM t GROUP BY g ORDER BY g");
    assert_eq!(rows(&core, A, &q), strs(&["a,2", "b,3", "c,1"]));
    let q =
        format!("SELECT g, rank() OVER (ORDER BY {f} NULLS FIRST) FROM t GROUP BY g ORDER BY g");
    assert_eq!(rows(&core, A, &q), strs(&["a,3", "b,2", "c,1"]));
    // 式キー: sum(k) + 1 は sum(k) と同順位。
    let q = "SELECT g, rank() OVER (ORDER BY sum(k) + 1), rank() OVER (ORDER BY sum(k)) \
             FROM t GROUP BY g ORDER BY g";
    assert_eq!(rows(&core, A, q), strs(&["a,3,3", "b,2,2", "c,1,1"]));
    // PARTITION BY の集計式（min は a=1・b=4・c=1 なので a と c が同じ区画）。
    let q = "SELECT g, row_number() OVER (PARTITION BY min(k) ORDER BY g) \
             FROM t GROUP BY g ORDER BY g";
    assert_eq!(rows(&core, A, q), strs(&["a,1", "b,1", "c,2"]));
    // 引数の集計式。
    let q = "SELECT g, sum(sum(k)) OVER () FROM t GROUP BY g ORDER BY g";
    assert_eq!(rows(&core, A, q), strs(&["a,25", "b,25", "c,25"]));
    let q = "SELECT g, sum(sum(k)) OVER (ORDER BY g) FROM t GROUP BY g ORDER BY g";
    assert_eq!(rows(&core, A, q), strs(&["a,15", "b,24", "c,25"]));
    let q = "SELECT g, lag(sum(k)) OVER (ORDER BY g) FROM t GROUP BY g ORDER BY g";
    assert_eq!(rows(&core, A, q), strs(&["a,NULL", "b,15", "c,9"]));
    // 同じ集計式を非窓項目・ORDER BY・引数に書いても一致する（隠し列は漏れない）。
    let q = "SELECT sum(k), rank() OVER (ORDER BY sum(k)), sum(sum(k)) OVER () \
             FROM t GROUP BY g ORDER BY 1";
    assert_eq!(rows(&core, A, q), strs(&["1,1,25", "9,2,25", "15,3,25"]));
    assert_eq!(names(&core, A, q), strs(&["sum", "rank", "sum"]));
    // HAVING・LIMIT との併用（LIMIT は窓の後）。
    let q = "SELECT g, rank() OVER (ORDER BY sum(k) DESC) FROM t GROUP BY g \
             HAVING count(*) > 1 ORDER BY g";
    assert_eq!(rows(&core, A, q), strs(&["a,1", "b,2"]));
    let q = "SELECT g, rank() OVER (ORDER BY sum(k) DESC) FROM t GROUP BY g \
             ORDER BY g LIMIT 1 OFFSET 1";
    assert_eq!(rows(&core, A, q), strs(&["b,2"]));
}

/// GROUP BY なしでも窓の中だけにある集計は 1 グループ 1 行の集計 SELECT になる（Issue #1899）。
#[test]
fn aggregate_inside_window_implicit_single_group() {
    let (core, _g) = build("sql64-gw-agg-single", None);
    assert_eq!(
        rows(&core, A, "SELECT rank() OVER (ORDER BY sum(k)) FROM t"),
        strs(&["1"])
    );
    assert_eq!(
        rows(&core, A, "SELECT sum(k), sum(sum(k)) OVER () FROM t"),
        strs(&["25,25"])
    );
    // 列参照を持たない非窓項目は暗黙の単一グループに混ざっても受理される。
    assert_eq!(
        rows(&core, A, "SELECT 1, rank() OVER (ORDER BY sum(k)) FROM t"),
        strs(&["1,1"])
    );
}

/// 巻き上げた集計もテナント境界の内側で計算される（tenant-b の 1000 を含まない）。
#[test]
fn aggregate_inside_window_respects_tenant_boundary() {
    let (core, _g) = build("sql64-gw-agg-rls", None);
    let q = "SELECT g, sum(sum(k)) OVER (), rank() OVER (ORDER BY sum(k) DESC) \
             FROM t GROUP BY g ORDER BY g";
    assert_eq!(rows(&core, A, q), strs(&["a,25,1", "b,25,2", "c,25,3"]));
    // tenant-b は自分の非公開行を足す: a=1015・b=9・c=1・z=7。
    assert_eq!(
        rows(&core, "tenant-b", q),
        strs(&["a,1032,1", "b,1032,2", "c,1032,4", "z,1032,3"])
    );
}

/// LIMIT 付き暗黙グループの先行検査は演算子キーワード・型名を列参照とみなさない（42703 にならない）。
#[test]
fn limit_precheck_ignores_keywords_and_type_names() {
    let (core, _g) = build("sql64-gw-kw", None);
    for expr in [
        "1 OR 2",
        "NULL IS NULL",
        "1::int",
        "'a' LIKE 'a'",
        "1 IN (1)",
    ] {
        let sql = format!("SELECT {expr}, rank() OVER (ORDER BY sum(k)) FROM t LIMIT 1");
        let code = core
            .execute_sql(&ctx("tenant-a"), &sql)
            .err()
            .map(|e| e.wire_code());
        assert_ne!(code, Some("42703"), "keyword misread as column: {sql}");
    }
}

/// 未対応の形は fail-closed（コード固定）。
#[test]
fn unsupported_shapes_fail_closed() {
    let (core, _g) = build("sql64-gw-reject", None);
    let cases: [(&str, &str); 25] = [
        // グループ化されていない列を窓が参照する。
        (
            "SELECT g, count(*), rank() OVER (ORDER BY k) FROM t GROUP BY g",
            "42803",
        ),
        // 巻き上げた集計式が入れ子集計・窓呼び出し・非グループ化列を含む（Issue #1899）。
        (
            "SELECT g, rank() OVER (ORDER BY sum(sum(k))) FROM t GROUP BY g",
            "42803",
        ),
        (
            "SELECT g, rank() OVER (ORDER BY sum(rank() OVER ())) FROM t GROUP BY g",
            "42803",
        ),
        (
            "SELECT g, rank() OVER (ORDER BY sum(k) + k) FROM t GROUP BY g",
            "42803",
        ),
        // 暗黙の単一グループで非グループ化列を射影する。
        ("SELECT k, rank() OVER (ORDER BY sum(k)) FROM t", "42803"),
        ("SELECT k, lag(count(*)) OVER () FROM t", "42803"),
        ("SELECT k, lag(count(*)) OVER () FROM t LIMIT 1", "42803"),
        // 未定義列は束縛段の 42703（先行ゲートの 42803 に潰されない）。
        (
            "SELECT nosuch, rank() OVER (ORDER BY sum(k)) FROM t",
            "42703",
        ),
        // LIMIT 付きでも未定義列は 42703（型名・CASE キーワードは列扱いしない）。
        (
            "SELECT nosuch, rank() OVER (ORDER BY sum(k)) FROM t LIMIT 1",
            "42703",
        ),
        (
            "SELECT CASE WHEN nosuch > 1 THEN 1 ELSE 2 END, rank() OVER (ORDER BY sum(k)) \
             FROM t LIMIT 1",
            "42703",
        ),
        // 列名 desc を末尾に持つ式は ASC/DESC 修飾子として剥がされず、未定義列として束縛される。
        (
            "SELECT g, rank() OVER (ORDER BY sum(k) + desc) FROM t GROUP BY g",
            "42703",
        ),
        // 窓の FILTER 内・フレームオフセット内の集計。
        (
            "SELECT g, count(*) FILTER (WHERE sum(k) > 1) OVER () FROM t GROUP BY g",
            "42803",
        ),
        // 窓が GROUP BY・HAVING・WHERE・ORDER BY 式にある。
        (
            "SELECT g, count(*) FROM t GROUP BY g HAVING rank() OVER (ORDER BY g) > 0",
            "42601",
        ),
        (
            "SELECT count(*) FROM t GROUP BY row_number() OVER ()",
            "42601",
        ),
        // ROLLUP／CUBE／GROUPING SETS と窓。
        (
            "SELECT g, count(*), rank() OVER (ORDER BY g) FROM t GROUP BY ROLLUP (g)",
            "42601",
        ),
        // DISTINCT。
        (
            "SELECT DISTINCT g, count(*), rank() OVER (ORDER BY g) FROM t GROUP BY g",
            "42601",
        ),
        // 窓の別名・窓項目の序数・範囲外の序数。
        (
            "SELECT g, count(*), rank() OVER (ORDER BY g) FROM t GROUP BY g ORDER BY rank",
            "42703",
        ),
        (
            "SELECT g, count(*), rank() OVER (ORDER BY g) FROM t GROUP BY g ORDER BY 3",
            "42601",
        ),
        (
            "SELECT g, count(*), rank() OVER (ORDER BY g) FROM t GROUP BY g ORDER BY 9",
            "42P10",
        ),
        // 序数 0 は先頭が通常列でも窓項目でも 42P10。
        (
            "SELECT g, count(*), rank() OVER (ORDER BY g) FROM t GROUP BY g ORDER BY 0",
            "42P10",
        ),
        (
            "SELECT rank() OVER (ORDER BY g), g, count(*) FROM t GROUP BY g ORDER BY 0",
            "42P10",
        ),
        // 存在しない列。
        (
            "SELECT g, count(*), rank() OVER (ORDER BY nope) FROM t GROUP BY g",
            "42703",
        ),
        // GROUP BY の序数（位置がずれて別の列を束ねないよう未対応のまま）。
        (
            "SELECT rank() OVER (ORDER BY g), g, count(*) FROM t GROUP BY 2",
            "42601",
        ),
        // 式の中の窓。
        (
            "SELECT g, count(*), 1 + rank() OVER (ORDER BY g) FROM t GROUP BY g",
            "42601",
        ),
        // スカラーサブクエリ項目との併用。
        (
            "SELECT g, count(*), (SELECT 1), rank() OVER (ORDER BY g) FROM t GROUP BY g",
            "42601",
        ),
    ];
    for (sql, want) in cases {
        assert_eq!(code(&core, A, sql), want, "{sql}");
    }
}

/// Describe の列は Execute の結果列と一致する（拡張クエリプロトコルの RowDescription 契約）。
#[test]
fn describe_matches_execute() {
    let (core, _g) = build("sql64-gw-describe", None);
    for sql in [
        "SELECT g, count(*), rank() OVER (ORDER BY g) AS r FROM t GROUP BY g ORDER BY g",
        "SELECT rank() OVER (ORDER BY g), g, sum(k) FROM t GROUP BY g",
        "SELECT count(*), count(*) OVER () FROM t",
        "SELECT g, sum(k), rank() OVER (ORDER BY sum(k) DESC) FROM t GROUP BY g",
        "SELECT rank() OVER (ORDER BY sum(k)) FROM t",
    ] {
        let session = SessionState::default();
        let parsed = core.parse_sql(sql).expect("parse");
        let described = core
            .describe_parsed_in_session(&session, &parsed)
            .expect("describe")
            .expect("has columns");
        let executed = run(&core, A, sql).expect("execute");
        assert_eq!(described, executed.columns, "{sql}");
    }
}

/// 文全体の `FETCH ... WITH TIES` は `0A000`、`EXPLAIN` は `42601`。
#[test]
fn with_ties_and_explain_are_rejected() {
    let (core, _g) = build("sql64-gw-ties", None);
    assert_eq!(
        code(
            &core,
            A,
            "SELECT g, count(*), rank() OVER (ORDER BY g) FROM t GROUP BY g \
             ORDER BY g FETCH FIRST 1 ROWS WITH TIES"
        ),
        "0A000"
    );
    assert_eq!(
        code(
            &core,
            A,
            "EXPLAIN SELECT g, count(*), rank() OVER (ORDER BY g) FROM t GROUP BY g"
        ),
        "42601"
    );
}
