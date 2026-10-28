//! 相関 `IN`／`NOT IN` の半結合・NULL を考慮した反結合の結合テスト（SQL-61 (2)。
//! `docs/spec/04-behavior/` のポインタのみ。Issue #1844、親 #1840。テナント境界は RLS-10 ポインタ）。
//!
//! 実 `Storage`＋`CpuScalarProvider` を `EngineCore::execute_sql` 経由で使い、期待値は
//! PostgreSQL 17 の三値論理から手計算した固定値。外側 1 行について S を「相関条件を満たす内側可視行の
//! 投影値 y の多重集合」、x を外側の対象値とすると（WHERE 文脈では UNKNOWN は偽として除外）:
//! - `x IN (S)`: x が S の非 NULL 要素と等しければ真、それ以外（x が NULL・NULL を含むだけ・S が空）は偽。
//! - `x NOT IN (S)`: S が空なら真（x が NULL でも真）／x が NULL なら偽／x ∈ S なら偽／S に NULL を
//!   含めば偽（UNKNOWN）／それ以外は真。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::error_format::ClassifiedError;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn o_schema() -> TableSchema {
    TableSchema::new(
        "o",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("g", ColumnType::BigInt, true),
            ColumnDef::new("x", ColumnType::Integer, true),
            ColumnDef::new("w", ColumnType::Integer, true),
            ColumnDef::new("d", ColumnType::Double, true),
        ],
    )
}

fn n_schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("g", ColumnType::BigInt, true),
            ColumnDef::new("y", ColumnType::Integer, true),
            ColumnDef::new("w", ColumnType::Integer, true),
            ColumnDef::new("t", ColumnType::Text, true),
            ColumnDef::new("d", ColumnType::Double, true),
        ],
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ins(core: &EngineCore, ctx: &PolicyContext, table: &str, id: u64, cols: &[(&str, &str)]) {
    let mut names = vec!["id".to_string(), "embedding".to_string()];
    let mut values = vec![id.to_string(), "'[0.1,0.2]'".to_string()];
    for (c, v) in cols {
        names.push((*c).to_string());
        values.push((*v).to_string());
    }
    let sql = format!(
        "INSERT INTO {table} ({}) VALUES ({}) USING OPERATION_ID '{table}-{id}'",
        names.join(", "),
        values.join(", ")
    );
    core.execute_sql_in_session(ctx, &mut SessionState::default(), &sql)
        .unwrap_or_else(|e| panic!("sql={sql:?} should succeed: {e:?}"));
}

fn ids(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<u64> {
    let mut v: Vec<u64> = core
        .execute_sql(ctx, sql)
        .unwrap_or_else(|e| panic!("sql={sql:?} should succeed: {e:?}"))
        .rows
        .iter()
        .map(|r| r.id)
        .collect();
    v.sort_unstable();
    v
}

fn err(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> (String, String) {
    match core.execute_sql(ctx, sql) {
        Ok(r) => panic!("sql={sql:?} should be rejected but returned {r:?}"),
        Err(e) => (e.error_class().wire_code().to_string(), e.to_string()),
    }
}

fn o_where(core: &EngineCore, ctx: &PolicyContext, pred: &str) -> Vec<u64> {
    ids(
        core,
        ctx,
        &format!("SELECT id FROM o WHERE {pred} LIMIT 100"),
    )
}

/// 標準フィクスチャ（テナント `t1`）。
///
/// n（内側）: n1 {g1, y1}、n2 {g1, y2}、n3 {g2, y NULL}、n4 {g2, y5}、n5 {g3, y7}、
///   n6 {g NULL, y9}（相関キーが NULL でどの外側行の S にも属さない）。w はすべて 10。
/// o（外側）: o1 {g1, x1, w5}、o2 {g1, x3, w5}、o3 {g2, x5, w20}、o4 {g2, x6, w11}、
///   o5 {g3, x NULL, w5}、o6 {g4, x NULL, w5}、o7 {g NULL, x9, w5}、o8 {g NULL, x NULL, w5}、
///   o9 {g3, x8, w5}、o10 {g2, x6, w5}。
fn setup() -> (EngineCore, PolicyContext, CleanupGuard) {
    let path = unique_db_path("sql61-correlated-in");
    let storage = Storage::open(&path).expect("open storage");
    for schema in [o_schema(), n_schema("n"), n_schema("m")] {
        storage.create_table(&schema).expect("create table");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let guard = CleanupGuard(path);
    let ctx = ctx_for("t1");
    for (id, cols) in [
        (1u64, vec![("g", "1"), ("y", "1")]),
        (2, vec![("g", "1"), ("y", "2")]),
        (3, vec![("g", "2")]),
        (4, vec![("g", "2"), ("y", "5")]),
        (5, vec![("g", "3"), ("y", "7")]),
        (6, vec![("y", "9")]),
    ] {
        let mut c = cols;
        c.push(("w", "10"));
        ins(&core, &ctx, "n", id, &c);
    }
    for (id, cols) in [
        (1u64, vec![("g", "1"), ("x", "1"), ("w", "5")]),
        (2, vec![("g", "1"), ("x", "3"), ("w", "5")]),
        (3, vec![("g", "2"), ("x", "5"), ("w", "20")]),
        (4, vec![("g", "2"), ("x", "6"), ("w", "11")]),
        (5, vec![("g", "3"), ("w", "5")]),
        (6, vec![("g", "4"), ("w", "5")]),
        (7, vec![("x", "9"), ("w", "5")]),
        (8, vec![("w", "5")]),
        (9, vec![("g", "3"), ("x", "8"), ("w", "5")]),
        (10, vec![("g", "2"), ("x", "6"), ("w", "5")]),
    ] {
        ins(&core, &ctx, "o", id, &cols);
    }
    (core, ctx, guard)
}

#[test]
fn sql61_correlated_in_is_a_semi_join_on_the_equality_key() {
    let (core, ctx, _g) = setup();
    // o1 (x1 ∈ {1,2}) と o3 (x5 ∈ {NULL,5}) だけが真。x が NULL の o5、NULL を含むだけの S の o4・o10、
    // S が空の o6〜o8、一致なしの o2・o9 は偽。
    assert_eq!(
        o_where(&core, &ctx, "x IN (SELECT y FROM n WHERE n.g = o.g)"),
        vec![1, 3]
    );
}

#[test]
fn sql61_correlated_not_in_follows_postgres_null_semantics_without_residual() {
    let (core, ctx, _g) = setup();
    // 真: o2（x3 ∉ {1,2}）、o6（g4 は S が空）、o7・o8（相関キー NULL は S が空。x が NULL の o8 も真）、
    // o9（x8 ∉ {7}）。偽: o1・o3（x ∈ S）、o4・o10（S={NULL,5} に x6 なし → UNKNOWN）、
    // o5（S={7} が非空で x が NULL → UNKNOWN）。
    assert_eq!(
        o_where(&core, &ctx, "x NOT IN (SELECT y FROM n WHERE n.g = o.g)"),
        vec![2, 6, 7, 8, 9]
    );
    // `NOT (x IN (...))` は `x NOT IN (...)` と同じ結果（否定は葉まで押し下げられる）。
    assert_eq!(
        o_where(&core, &ctx, "NOT (x IN (SELECT y FROM n WHERE n.g = o.g))"),
        vec![2, 6, 7, 8, 9]
    );
}

#[test]
fn sql61_correlated_in_and_not_in_with_a_residual_condition() {
    let (core, ctx, _g) = setup();
    // 残余 `n.w > o.w`（内側 w はすべて 10）。o3 (w20)・o4 (w11) は S が空になる:
    // IN は偽、NOT IN は真。他の外側行（w5）は残余なしと同じ。
    assert_eq!(
        o_where(
            &core,
            &ctx,
            "x IN (SELECT y FROM n WHERE n.g = o.g AND n.w > o.w)"
        ),
        vec![1]
    );
    assert_eq!(
        o_where(
            &core,
            &ctx,
            "x NOT IN (SELECT y FROM n WHERE n.g = o.g AND n.w > o.w)"
        ),
        vec![2, 3, 4, 6, 7, 8, 9]
    );
}

#[test]
fn sql61_correlated_in_and_not_in_without_an_equality_key() {
    let (core, ctx, _g) = setup();
    // キーなし: w5 の外側行の S は n 全行の y = {1,2,NULL,5,7,9}（NULL を含む）。
    // IN は x ∈ {1,2,5,7,9} の o1・o7。NOT IN は NULL を含むため w5 の行はすべて偽、
    // S が空の o3・o4 だけが真。
    assert_eq!(
        o_where(&core, &ctx, "x IN (SELECT y FROM n WHERE n.w > o.w)"),
        vec![1, 7]
    );
    assert_eq!(
        o_where(&core, &ctx, "x NOT IN (SELECT y FROM n WHERE n.w > o.w)"),
        vec![3, 4]
    );
    // 整数クラスの跨ぎ（BIGINT の対象 g × INTEGER の y）: g ∈ {1,2,NULL,5,7,9} は o1・o2・o10。
    assert_eq!(
        o_where(&core, &ctx, "g IN (SELECT y FROM n WHERE n.w > o.w)"),
        vec![1, 2, 10]
    );
}

#[test]
fn sql61_correlated_in_with_pseudo_id_columns() {
    let (core, ctx, _g) = setup();
    // 対象と投影がともに疑似列 id。S(g) = 同じ g の n の id。
    assert_eq!(
        o_where(&core, &ctx, "id IN (SELECT id FROM n WHERE n.g = o.g)"),
        vec![1, 2, 3, 4, 5]
    );
    assert_eq!(
        o_where(&core, &ctx, "id NOT IN (SELECT id FROM n WHERE n.g = o.g)"),
        vec![6, 7, 8, 9, 10]
    );
    // 別名付きの列参照の投影（Issue #1540）も受理する。
    assert_eq!(
        o_where(&core, &ctx, "x IN (SELECT y AS yy FROM n WHERE n.g = o.g)"),
        vec![1, 3]
    );
}

#[test]
fn sql61_correlated_in_combines_with_or_and_aggregates() {
    let (core, ctx, _g) = setup();
    assert_eq!(
        o_where(
            &core,
            &ctx,
            "x NOT IN (SELECT y FROM n WHERE n.g = o.g) OR id = 1"
        ),
        vec![1, 2, 6, 7, 8, 9]
    );
    assert_eq!(
        o_where(
            &core,
            &ctx,
            "w = 5 AND x IN (SELECT y FROM n WHERE n.g = o.g)"
        ),
        vec![1]
    );
    let r = core
        .execute_sql(
            &ctx,
            "SELECT COUNT(*) FROM o WHERE x NOT IN (SELECT y FROM n WHERE n.g = o.g)",
        )
        .expect("aggregate");
    assert_eq!(r.rows[0].cells[0], Cell::Integer(5));
}

#[test]
fn sql61_correlated_in_does_not_observe_other_tenants_rows() {
    // RLS-10: 他テナントの内側行（特に y が NULL の行）は自テナントの IN／NOT IN の結果にもエラーにも
    // 影響しない。t2 は自分の行だけで判定する。
    let (core, ctx, _g) = setup();
    let t2 = ctx_for("t2");
    ins(&core, &t2, "n", 100, &[("g", "1")]);
    ins(&core, &t2, "n", 101, &[("g", "3"), ("y", "8")]);
    ins(&core, &t2, "o", 200, &[("g", "1"), ("x", "1")]);
    ins(&core, &t2, "o", 201, &[("g", "3"), ("x", "8")]);
    ins(&core, &t2, "o", 202, &[("g", "9"), ("x", "1")]);
    let not_in = "SELECT id FROM o WHERE x NOT IN (SELECT y FROM n WHERE n.g = o.g) LIMIT 100";
    let in_ = "SELECT id FROM o WHERE x IN (SELECT y FROM n WHERE n.g = o.g) LIMIT 100";
    // t1 は標準フィクスチャのまま（t2 の NULL 行が g=1 の S を汚さない）。
    assert_eq!(ids(&core, &ctx, not_in), vec![2, 6, 7, 8, 9]);
    assert_eq!(ids(&core, &ctx, in_), vec![1, 3]);
    // t2: o200 は S(g1)={NULL} → NOT IN は UNKNOWN で偽、IN も偽。o201 は S(g3)={8} に x8 → NOT IN 偽・IN 真。
    // o202 は S(g9) が空 → NOT IN 真。
    assert_eq!(ids(&core, &t2, not_in), vec![202]);
    assert_eq!(ids(&core, &t2, in_), vec![201]);
    // エラーも他テナントの行に依存しない。
    let bad = "SELECT id FROM o WHERE x IN (SELECT t FROM n WHERE n.g = o.g) LIMIT 100";
    assert_eq!(err(&core, &ctx, bad), err(&core, &t2, bad));
}

#[test]
fn sql61_correlated_in_fail_closed_forms() {
    let (core, ctx, _g) = setup();
    // 明示 LIMIT／OFFSET は集合の帰属を変えるため 42601（`correlated` を含む静的文言）。
    for sql in [
        "SELECT id FROM o WHERE x IN (SELECT y FROM n WHERE n.g = o.g LIMIT 5) LIMIT 100",
        "SELECT id FROM o WHERE x NOT IN (SELECT y FROM n WHERE n.g = o.g LIMIT 5) LIMIT 100",
        "SELECT id FROM o WHERE x NOT IN (SELECT y FROM n WHERE n.g = o.g LIMIT ALL OFFSET 1) LIMIT 100",
        // 式投影は実体化できない。
        "SELECT id FROM o WHERE x IN (SELECT y + 1 FROM n WHERE n.g = o.g) LIMIT 100",
        // 相関の ANY／ALL（= ANY・<> ALL を含む）は対象外。
        "SELECT id FROM o WHERE x = ANY (SELECT y FROM n WHERE n.g = o.g) LIMIT 100",
        "SELECT id FROM o WHERE x <> ALL (SELECT y FROM n WHERE n.g = o.g) LIMIT 100",
        // 外側参照が投影・ORDER BY にある。
        "SELECT id FROM o WHERE x IN (SELECT o.g FROM n WHERE n.g = o.g) LIMIT 100",
        "SELECT id FROM o WHERE x IN (SELECT y FROM n WHERE n.g = o.g ORDER BY o.g) LIMIT 100",
    ] {
        let (code, msg) = err(&core, &ctx, sql);
        assert_eq!(code, "42601", "{sql}: {msg}");
        assert!(msg.contains("correlated"), "{sql}: {msg}");
        assert!(!msg.contains('\u{2}'), "{sql}: {msg:?}");
    }
    // LIMIT 0 は従来どおり 22000。
    assert_eq!(
        err(
            &core,
            &ctx,
            "SELECT id FROM o WHERE x IN (SELECT y FROM n WHERE n.g = o.g LIMIT 0) LIMIT 100"
        )
        .0,
        "22000"
    );
    // 結合キーに表せない型（DOUBLE）は 0A000。
    for sql in [
        "SELECT id FROM o WHERE d IN (SELECT d FROM n WHERE n.g = o.g) LIMIT 100",
        "SELECT id FROM o WHERE d NOT IN (SELECT d FROM n WHERE n.g = o.g) LIMIT 100",
    ] {
        let (code, msg) = err(&core, &ctx, sql);
        assert_eq!(code, "0A000", "{sql}: {msg}");
        assert!(!msg.contains('\u{2}'), "{sql}: {msg:?}");
    }
    // 値族の不一致は非相関 IN と同じコード（22000）。
    let (code, msg) = err(
        &core,
        &ctx,
        "SELECT id FROM o WHERE x IN (SELECT t FROM n WHERE n.g = o.g) LIMIT 100",
    );
    assert_eq!(code, "22000", "{msg}");
    // 複数列の投影・未定義の対象列。
    assert_eq!(
        err(
            &core,
            &ctx,
            "SELECT id FROM o WHERE x IN (SELECT y, w FROM n WHERE n.g = o.g) LIMIT 100"
        )
        .0,
        "42601"
    );
    assert_eq!(
        err(
            &core,
            &ctx,
            "SELECT id FROM o WHERE zz IN (SELECT y FROM n WHERE n.g = o.g) LIMIT 100"
        )
        .0,
        "42703"
    );
}

#[test]
fn sql61_correlated_in_is_not_executed_in_predicate_dml() {
    let (core, ctx, _g) = setup();
    for sql in [
        "DELETE FROM o WHERE g IN (SELECT y FROM n WHERE n.w = x) USING OPERATION_ID 'd1'",
        "DELETE FROM o WHERE g NOT IN (SELECT y FROM n WHERE n.w = x) USING OPERATION_ID 'd2'",
        "UPDATE o SET w = 1 WHERE g IN (SELECT y FROM n WHERE n.w = x) USING OPERATION_ID 'u1'",
        "SELECT COUNT(*) FILTER (WHERE g IN (SELECT y FROM n WHERE n.w = x)) FROM o",
    ] {
        let e = core
            .execute_sql_in_session(&ctx, &mut SessionState::default(), sql)
            .expect_err(sql);
        assert_eq!(e.error_class().wire_code(), "42601", "{sql}: {e}");
        assert!(e.to_string().contains("correlated"), "{sql}: {e}");
    }
}

#[test]
fn sql61_deferred_error_path_validates_correlated_in_statically() {
    // 先行する述語がスカラーサブクエリの基数違反（21000）で解決を打ち切ったとき、後続の相関 IN／NOT IN は
    // 実行せずに静的検証される。静的に問題が無ければ元の 21000、静的な誤りがあればそれを優先する。
    let (core, ctx, _g) = setup();
    for neg in ["IN", "NOT IN"] {
        let ok = format!(
            "SELECT id FROM o WHERE g IN (SELECT g FROM n WHERE w = (SELECT w FROM n LIMIT 2) \
             AND y {neg} (SELECT y FROM m WHERE m.g = n.g)) LIMIT 100"
        );
        assert_eq!(err(&core, &ctx, &ok).0, "21000", "{ok}");
        let missing_column = format!(
            "SELECT id FROM o WHERE g IN (SELECT g FROM n WHERE w = (SELECT w FROM n LIMIT 2) \
             AND y {neg} (SELECT y FROM m WHERE m.g = n.g AND m.zz = 1)) LIMIT 100"
        );
        assert_eq!(
            err(&core, &ctx, &missing_column).0,
            "42703",
            "{missing_column}"
        );
        let family = format!(
            "SELECT id FROM o WHERE g IN (SELECT g FROM n WHERE w = (SELECT w FROM n LIMIT 2) \
             AND y {neg} (SELECT t FROM m WHERE m.g = n.g)) LIMIT 100"
        );
        assert_eq!(err(&core, &ctx, &family).0, "22000", "{family}");
        let limited = format!(
            "SELECT id FROM o WHERE g IN (SELECT g FROM n WHERE w = (SELECT w FROM n LIMIT 2) \
             AND y {neg} (SELECT y FROM m WHERE m.g = n.g LIMIT 3)) LIMIT 100"
        );
        assert_eq!(err(&core, &ctx, &limited).0, "42601", "{limited}");
    }
}
