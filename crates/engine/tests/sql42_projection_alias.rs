//! 疑似列 `id`・列・式への `AS` 別名と `ORDER BY` からの別名参照
//! （SQL-42。`docs/spec/04-behavior/` のポインタのみ。Issue #1540、親 #1537）の結合テスト。
//! 実 `Storage`＋`CpuScalarProvider` を `EngineCore::execute_sql_in_session` 経由で使い、
//! 別名付き投影の値・型の不変、`ORDER BY` の別名解決（`42702` を含む）、非対応文脈の
//! fail-closed、テナント境界を固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run_opt(
    core: &EngineCore,
    ctx: &PolicyContext,
    sql: &str,
) -> Result<SqlOutcome, engine::sql::allowlist::SqlSurfaceError> {
    core.execute_sql_in_session(ctx, &mut SessionState::default(), sql)
}

fn run(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> QueryResult {
    match run_opt(core, ctx, sql).unwrap_or_else(|e| panic!("{sql:?} should succeed: {e:?}")) {
        SqlOutcome::Query(r) => r,
        other => panic!("expected Query for {sql:?}, got {other:?}"),
    }
}

fn code(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> String {
    match run_opt(core, ctx, sql) {
        Ok(o) => panic!("{sql:?} should be rejected, got {o:?}"),
        Err(e) => e.wire_code().to_string(),
    }
}

/// 結果行の `id` を返却順で集める。
fn ids(r: &QueryResult) -> Vec<u64> {
    r.rows.iter().map(|row| row.id).collect()
}

fn col_names(r: &QueryResult) -> Vec<String> {
    r.columns
        .iter()
        .map(|c| match c {
            ColumnMeta::Id => "id".to_string(),
            ColumnMeta::Scalar { name, .. }
            | ColumnMeta::Computed { name, .. }
            | ColumnMeta::AliasedId { name } => name.clone(),
        })
        .collect()
}

/// 行 id → n の対応（n は id 1,2,3 に対して 3,1,2。id 順と n 順が一致しない）。
fn setup() -> (EngineCore, PolicyContext, CleanupGuard) {
    let path = unique_db_path("sql42-projection-alias");
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("n", ColumnType::Integer, false),
                ColumnDef::new("m2", ColumnType::Integer, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("t1");
    for (id, n, m2, body) in [(1u64, 3i64, 10i64, "c"), (2, 1, 30, "a"), (3, 2, 20, "b")] {
        run_opt(
            &core,
            &ctx,
            &format!(
                "INSERT INTO t (id, embedding, n, m2, body) VALUES ({id}, '[0.{id},0.1]', {n}, {m2}, '{body}') \
                 USING OPERATION_ID 'seed-{id}'"
            ),
        )
        .unwrap_or_else(|e| panic!("insert should succeed: {e:?}"));
    }
    // 他テナントの行（別名付き投影・ORDER BY の結果に現れてはならない）。
    run_opt(
        &core,
        &ctx_for("t2"),
        "INSERT INTO t (id, embedding, n, m2, body) VALUES (9, '[0.9,0.1]', 0, 0, 'z') \
         USING OPERATION_ID 'seed-other'",
    )
    .unwrap_or_else(|e| panic!("insert should succeed: {e:?}"));
    (core, ctx, CleanupGuard(path))
}

#[test]
fn id_alias_and_expr_alias_mix() {
    let (core, ctx, _g) = setup();
    let r = run(&core, &ctx, "SELECT id AS i, n+1 AS m FROM t LIMIT 10");
    assert_eq!(col_names(&r), vec!["i", "m"]);
    assert!(matches!(r.columns[0], ColumnMeta::AliasedId { .. }));
    let mut rows: Vec<_> = r.rows.iter().collect();
    rows.sort_by_key(|row| row.id);
    let n = [3i64, 1, 2];
    for (k, row) in rows.iter().enumerate() {
        assert_eq!(row.cells[0], Cell::Integer(row.id));
        assert_eq!(row.cells[1], Cell::SignedInteger(n[k] + 1));
    }
}

#[test]
fn column_alias_keeps_type_and_cells() {
    let (core, ctx, _g) = setup();
    let plain = run(&core, &ctx, "SELECT n, body FROM t ORDER BY id LIMIT 10");
    let aliased = run(
        &core,
        &ctx,
        "SELECT n AS x, body AS b FROM t ORDER BY id LIMIT 10",
    );
    assert_eq!(col_names(&aliased), vec!["x", "b"]);
    for (a, p) in aliased.columns.iter().zip(&plain.columns) {
        match (a, p) {
            (ColumnMeta::Scalar { ty: ta, .. }, ColumnMeta::Scalar { ty: tp, .. }) => {
                assert_eq!(ta, tp)
            }
            other => panic!("expected Scalar metas, got {other:?}"),
        }
    }
    let cells =
        |r: &QueryResult| -> Vec<Vec<Cell>> { r.rows.iter().map(|x| x.cells.clone()).collect() };
    assert_eq!(cells(&aliased), cells(&plain));
    // 括弧付き列にも付けられる。
    let paren = run(&core, &ctx, "SELECT (n) AS x FROM t ORDER BY id LIMIT 10");
    assert_eq!(col_names(&paren), vec!["x"]);
}

#[test]
fn qualified_id_alias_matches_plain() {
    let (core, ctx, _g) = setup();
    let a = run(&core, &ctx, "SELECT t.id AS i FROM t ORDER BY id LIMIT 10");
    let b = run(&core, &ctx, "SELECT id AS i FROM t ORDER BY id LIMIT 10");
    assert_eq!(format!("{a:?}"), format!("{b:?}"));
}

#[test]
fn duplicate_aliases_are_projectable() {
    let (core, ctx, _g) = setup();
    let r = run(&core, &ctx, "SELECT n AS a, n+1 AS a FROM t LIMIT 10");
    assert_eq!(col_names(&r), vec!["a", "a"]);
}

#[test]
fn order_by_resolves_aliases() {
    let (core, ctx, _g) = setup();
    // 列別名: n は id 1,2,3 に対して 3,1,2 → 昇順は id 2,3,1。
    assert_eq!(
        ids(&run(
            &core,
            &ctx,
            "SELECT n AS x FROM t ORDER BY x LIMIT 10"
        )),
        vec![2, 3, 1]
    );
    assert_eq!(
        ids(&run(
            &core,
            &ctx,
            "SELECT n AS x FROM t ORDER BY x DESC LIMIT 10"
        )),
        vec![1, 3, 2]
    );
    // 疑似列 id の別名。
    assert_eq!(
        ids(&run(
            &core,
            &ctx,
            "SELECT id AS i FROM t ORDER BY i DESC LIMIT 10"
        )),
        vec![3, 2, 1]
    );
    // 式別名（n+1 ≡ n の順）と LIMIT／OFFSET。
    assert_eq!(
        ids(&run(
            &core,
            &ctx,
            "SELECT id AS i, n+1 AS m FROM t ORDER BY m LIMIT 10"
        )),
        vec![2, 3, 1]
    );
    assert_eq!(
        ids(&run(
            &core,
            &ctx,
            "SELECT id AS i, n+1 AS m FROM t ORDER BY m DESC LIMIT 1 OFFSET 1"
        )),
        vec![3]
    );
    // 式別名の逆順（-n）は n の降順と一致する。
    assert_eq!(
        ids(&run(
            &core,
            &ctx,
            "SELECT 0-n AS neg FROM t ORDER BY neg LIMIT 10"
        )),
        vec![1, 3, 2]
    );
}

#[test]
fn alias_takes_precedence_over_real_column() {
    let (core, ctx, _g) = setup();
    // `n` という出力名は式 `m2 - n`（id1:7, id2:29, id3:18）を指す。n 順なら 2,3,1。
    assert_eq!(
        ids(&run(
            &core,
            &ctx,
            "SELECT m2-n AS n FROM t ORDER BY n LIMIT 10"
        )),
        vec![1, 3, 2]
    );
    // `n AS id`: ORDER BY id は n の順（実在列 id は無いので疑似列より別名が優先）。
    assert_eq!(
        ids(&run(
            &core,
            &ctx,
            "SELECT n AS id FROM t ORDER BY id LIMIT 10"
        )),
        vec![2, 3, 1]
    );
}

#[test]
fn ambiguous_alias_is_rejected_only_for_different_targets() {
    let (core, ctx, _g) = setup();
    assert_eq!(
        code(
            &core,
            &ctx,
            "SELECT n AS a, m2 AS a FROM t ORDER BY a LIMIT 10"
        ),
        "42702"
    );
    assert_eq!(
        code(
            &core,
            &ctx,
            "SELECT n AS a, n+1 AS a FROM t ORDER BY a LIMIT 10"
        ),
        "42702"
    );
    // 同じ対象を指す重複は曖昧ではない。
    assert_eq!(
        ids(&run(
            &core,
            &ctx,
            "SELECT n AS a, n AS a FROM t ORDER BY a LIMIT 10"
        )),
        vec![2, 3, 1]
    );
    // 参照しなければ重複しても投影は許す。
    run(&core, &ctx, "SELECT n AS a, m2 AS a FROM t LIMIT 10");
}

#[test]
fn unknown_and_non_alias_order_by_unchanged() {
    let (core, ctx, _g) = setup();
    assert_eq!(
        code(&core, &ctx, "SELECT n AS x FROM t ORDER BY nope LIMIT 10"),
        "22000"
    );
    // 別名のない文は従来どおり入力列で解決する。
    assert_eq!(
        ids(&run(&core, &ctx, "SELECT body FROM t ORDER BY n LIMIT 10")),
        vec![2, 3, 1]
    );
    // 別名を付けても元の列名で並べ替えられる。
    assert_eq!(
        ids(&run(
            &core,
            &ctx,
            "SELECT n AS x FROM t ORDER BY n LIMIT 10"
        )),
        vec![2, 3, 1]
    );
}

#[test]
fn unsupported_forms_stay_fail_closed() {
    let (core, ctx, _g) = setup();
    for sql in [
        // 暗黙の別名は受理しない。
        "SELECT n m FROM t LIMIT 10",
        "SELECT n AS FROM t LIMIT 10",
        // 集合演算の枝・ビュー本体。
        "SELECT n AS x FROM t UNION SELECT n FROM t",
        "CREATE VIEW v AS SELECT n AS x FROM t",
    ] {
        assert_eq!(code(&core, &ctx, sql), "42601", "{sql}");
    }
}

#[test]
fn explain_matches_plain_error_class() {
    let (core, ctx, _g) = setup();
    assert_eq!(
        code(
            &core,
            &ctx,
            "EXPLAIN SELECT n AS a, m2 AS a FROM t ORDER BY a LIMIT 10"
        ),
        "42702"
    );
}

#[test]
fn tenant_boundary_holds() {
    let (core, ctx, _g) = setup();
    let r = run(
        &core,
        &ctx,
        "SELECT id AS i, n AS x FROM t ORDER BY x LIMIT 10",
    );
    assert_eq!(r.rows.len(), 3);
    assert!(!ids(&r).contains(&9));
    let other = run(
        &core,
        &ctx_for("t2"),
        "SELECT id AS i, n AS x FROM t ORDER BY x LIMIT 10",
    );
    assert_eq!(ids(&other), vec![9]);
    // エラー文言に他テナントの情報が入らない。
    let err = run_opt(
        &core,
        &ctx,
        "SELECT n AS a, m2 AS a FROM t ORDER BY a LIMIT 10",
    )
    .expect_err("ambiguous");
    assert!(!format!("{err:?}").contains('9'));
}

#[test]
fn aliased_columns_inside_subqueries() {
    let (core, ctx, _g) = setup();
    // 内側の別名付き id／列は別名なしと同じ結果になる（値族の食い違いで誤結果にしない）。
    for (aliased, plain) in [
        (
            "SELECT id FROM t WHERE id IN (SELECT id AS i FROM t WHERE n = 1) LIMIT 10",
            "SELECT id FROM t WHERE id IN (SELECT id FROM t WHERE n = 1) LIMIT 10",
        ),
        (
            "SELECT id FROM t WHERE n IN (SELECT n AS x FROM t WHERE n > 1) LIMIT 10",
            "SELECT id FROM t WHERE n IN (SELECT n FROM t WHERE n > 1) LIMIT 10",
        ),
        (
            "SELECT id FROM t WHERE n = (SELECT n AS x FROM t WHERE id = 2 LIMIT 1) LIMIT 10",
            "SELECT id FROM t WHERE n = (SELECT n FROM t WHERE id = 2 LIMIT 1) LIMIT 10",
        ),
    ] {
        let a = run_opt(&core, &ctx, aliased);
        let p = run_opt(&core, &ctx, plain);
        match (a, p) {
            (Ok(SqlOutcome::Query(a)), Ok(SqlOutcome::Query(p))) => {
                assert_eq!(ids(&a), ids(&p), "{aliased}")
            }
            (Err(a), Err(p)) => assert_eq!(a.wire_code(), p.wire_code(), "{aliased}"),
            other => panic!("{aliased}: divergent outcomes {other:?}"),
        }
    }
}

/// 別名付き疑似列 `id` を内側投影にしても、別名なしと同じ整数族として `IN`・スカラー比較が
/// 通る（codex-review P1 指摘対応。別名で値や型の扱いを変えない）。
#[test]
fn aliased_id_keeps_integer_family_in_subqueries() {
    let (core, ctx, _g) = setup();
    let plain_in = run(
        &core,
        &ctx,
        "SELECT id FROM t WHERE id IN (SELECT id FROM t WHERE n = 1 LIMIT 10) LIMIT 10",
    );
    let aliased_in = run(
        &core,
        &ctx,
        "SELECT id FROM t WHERE id IN (SELECT id AS i FROM t WHERE n = 1 LIMIT 10) LIMIT 10",
    );
    assert_eq!(ids(&aliased_in), ids(&plain_in));
    assert_eq!(ids(&aliased_in), vec![2]);

    let plain_eq = run(
        &core,
        &ctx,
        "SELECT id FROM t WHERE id = (SELECT id FROM t WHERE n = 1 LIMIT 10) LIMIT 10",
    );
    let aliased_eq = run(
        &core,
        &ctx,
        "SELECT id FROM t WHERE id = (SELECT id AS i FROM t WHERE n = 1 LIMIT 10) LIMIT 10",
    );
    assert_eq!(ids(&aliased_eq), ids(&plain_eq));
    assert_eq!(ids(&aliased_eq), vec![2]);
}
