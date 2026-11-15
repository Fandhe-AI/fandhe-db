//! `CREATE VIEW` 本文 `WHERE` の式述語（`IS NULL`・`OR`・数値比較・`IN`・`BETWEEN`・`NOT`・
//! `<>`・組み込み関数。Issue #3238）の結合テスト。ポインタ: `docs/spec/04-behavior/
//! table-behavior.md` TABLE-18・TABLE-42・TABLE-43・`docs/spec/04-behavior/rls.md` RLS-10 (b)。
//!
//! 方針は `docs/design/create-view.md`（Issue #3237・#3238 追記）。検証する契約:
//! 1. 受理した本文の SELECT 結果が、基表への直接実行および Rust 側の独立オラクルと一致する
//! 2. 非組み込み関数・UDF・現在時刻関数・`visible()`・サブクエリ・CTE 本文は `42601` で
//!    何も永続化されない（`OR` 分岐・`NOT` の内側でも同様）
//! 3. テナント境界（本文の `OR` が可視性を広げない）・再オープン後の参照・連鎖・更新可能ビュー・
//!    MATERIALIZED VIEW

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TENANTS: [&str; 3] = ["alice", "bob", "carol"];

fn schema() -> TableSchema {
    TableSchema::new(
        "t",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("n", ColumnType::Integer, true),
            ColumnDef::new("s", ColumnType::Text, false),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// `(id, テナント, n, s, 可視性)`。NULL 行・境界値・大文字小文字違いを含む。
fn fixture(extra_private: bool) -> Vec<(u64, &'static str, Option<i32>, &'static str, Visibility)> {
    let mut rows = vec![
        (1, "alice", Some(1), "a", Visibility::Public),
        (2, "alice", Some(5), "a", Visibility::Public),
        (3, "alice", None, "b", Visibility::Public),
        (4, "alice", Some(3), "C", Visibility::Private),
        (5, "bob", Some(10), "a", Visibility::Public),
        (6, "bob", None, "z", Visibility::Private),
        (7, "bob", Some(2), "b", Visibility::Public),
        (8, "carol", Some(4), "b", Visibility::Public),
    ];
    if extra_private {
        rows.push((9, "bob", None, "a", Visibility::Private));
        rows.push((10, "bob", Some(100), "b", Visibility::Private));
    }
    rows
}

fn open_at(path: &std::path::Path, extra_private: bool) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    for (id, tenant, n, s, vis) in fixture(extra_private) {
        engine::tenant::insert_typed_row(
            &storage,
            "t",
            &ctx(tenant),
            id,
            vis,
            &[
                Value::Vector(vec![id as f32, 0.0]),
                n.map_or(Value::Null, Value::Integer),
                Value::Text(s.to_string()),
            ],
            &OperationId::parse(&format!("seed-{id}")).expect("op id"),
        )
        .expect("insert row");
    }
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn open_core(label: &str, extra_private: bool) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    (open_at(&path, extra_private), guard)
}

fn run_outcome(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<QueryResult, SqlSurfaceError> {
    match run_outcome(core, tenant, sql)? {
        SqlOutcome::Query(q) => Ok(q),
        other => panic!("expected Query for {sql}, got {other:?}"),
    }
}

fn ddl(core: &EngineCore, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("alice"), &mut session, sql)
}

fn ids(core: &EngineCore, tenant: &str, sql: &str) -> Vec<u64> {
    let mut v: Vec<u64> = run(core, tenant, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
        .rows
        .iter()
        .map(|r| r.id)
        .collect();
    v.sort_unstable();
    v
}

/// 可視な行の `(id, n, s)`（独立オラクルの母集合。可視性は RLS 経由の直接実行に任せる）。
fn visible_rows(core: &EngineCore, tenant: &str) -> Vec<(u64, Option<i64>, String)> {
    run(core, tenant, "SELECT n, s FROM t LIMIT 100")
        .expect("visible rows")
        .rows
        .iter()
        .map(|r| {
            let n = match &r.cells[0] {
                Cell::Null => None,
                Cell::SignedInteger(v) => Some(*v),
                other => panic!("unexpected n cell {other:?}"),
            };
            let s = match &r.cells[1] {
                Cell::Text(s) => s.clone(),
                other => panic!("unexpected s cell {other:?}"),
            };
            (r.id, n, s)
        })
        .collect()
}

type Oracle = fn(Option<i64>, &str) -> bool;

/// (本文の述語, SQL の 3 値論理で「真」になる行だけを選ぶ Rust 側の独立オラクル)。
fn cases() -> Vec<(&'static str, Oracle)> {
    vec![
        ("n > 3", |n, _| n.is_some_and(|v| v > 3)),
        ("n IS NULL", |n, _| n.is_none()),
        ("n IS NOT NULL", |n, _| n.is_some()),
        ("s <> 'a'", |_, s| s != "a"),
        ("s = 'a' OR n > 3", |n, s| {
            s == "a" || n.is_some_and(|v| v > 3)
        }),
        ("n + 1 > 3", |n, _| n.is_some_and(|v| v + 1 > 3)),
        ("n IN (1, 2)", |n, _| n.is_some_and(|v| v == 1 || v == 2)),
        ("s IN ('a', 'b')", |_, s| s == "a" || s == "b"),
        ("s NOT IN ('a')", |_, s| s != "a"),
        ("n BETWEEN 1 AND 5", |n, _| {
            n.is_some_and(|v| (1..=5).contains(&v))
        }),
        ("NOT (s = 'a' OR n > 3)", |n, s| {
            s != "a" && n.is_some_and(|v| v <= 3)
        }),
        ("lower(s) = 'c'", |_, s| s.to_lowercase() == "c"),
        ("COALESCE(n, 0) > 1", |n, _| n.unwrap_or(0) > 1),
        ("n > 3 OR n IS NULL", |n, _| n.is_none_or(|v| v > 3)),
    ]
}

/// 受理した各本文の結果が、基表への直接実行・Rust 側のオラクルの双方と一致する（3 テナント）。
/// 外側の `WHERE`・`ORDER BY` と組み合わせた形も確認する。
#[test]
fn expression_predicate_bodies_match_direct_execution_and_oracle() {
    let (core, _g) = open_core("view-where-expr", false);
    for (i, (pred, _)) in cases().iter().enumerate() {
        ddl(
            &core,
            &format!("CREATE VIEW v{i} AS SELECT * FROM t WHERE {pred}"),
        )
        .unwrap_or_else(|e| panic!("{pred}: {e:?}"));
    }
    for tenant in TENANTS {
        let rows = visible_rows(&core, tenant);
        for (i, (pred, oracle)) in cases().iter().enumerate() {
            let direct = ids(
                &core,
                tenant,
                &format!("SELECT id FROM t WHERE {pred} LIMIT 100"),
            );
            let via = ids(&core, tenant, &format!("SELECT id FROM v{i} LIMIT 100"));
            assert_eq!(via, direct, "tenant={tenant} pred={pred}");
            let mut want: Vec<u64> = rows
                .iter()
                .filter(|(_, n, s)| oracle(*n, s))
                .map(|(id, _, _)| *id)
                .collect();
            want.sort_unstable();
            assert_eq!(via, want, "oracle tenant={tenant} pred={pred}");
        }
        // 外側の WHERE・ORDER BY との合成（本文述語は落ちない）。
        let outer = run(
            &core,
            tenant,
            "SELECT id FROM v0 WHERE s <> 'zz' ORDER BY id DESC LIMIT 100",
        )
        .expect("outer");
        let mut got: Vec<u64> = outer.rows.iter().map(|r| r.id).collect();
        let mut want: Vec<u64> = rows
            .iter()
            .filter(|(_, n, s)| n.is_some_and(|v| v > 3) && s != "zz")
            .map(|(id, _, _)| *id)
            .collect();
        want.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(got, want, "tenant={tenant} outer");
        got.sort_unstable();
    }
}

/// 末尾 `;` 付き（psql が送る通常の形）でも作成できる。
#[test]
fn trailing_semicolon_body_is_accepted() {
    let (core, _g) = open_core("view-where-expr-semi", false);
    ddl(&core, "CREATE VIEW vs AS SELECT * FROM t WHERE n > 3;").expect("create");
    // alice には自分の行 2 と他テナントの public 行 5・8 が見える。
    assert_eq!(
        ids(&core, "alice", "SELECT id FROM vs LIMIT 100"),
        vec![2, 5, 8]
    );
}

// intentional: docs/design/create-view.md
/// 非組み込み関数・UDF・現在時刻関数・`visible()`・式左辺 `LIKE`・サブクエリ・投影の式項目・
/// CTE 本文の述語は `42601` で、何も永続化されない（`OR` 分岐・`NOT` の内側でも同じ）。
#[test]
fn non_builtin_and_unsupported_forms_are_rejected_and_not_persisted() {
    let (core, _g) = open_core("view-where-expr-reject", false);
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("alice"), &mut session, "CREATE FUNCTION f(x) AS x + 1")
        .expect("create function");
    let bodies = [
        "SELECT * FROM t WHERE f(n) > 1",
        "SELECT * FROM t WHERE no_such_fn(n) > 1",
        "SELECT * FROM t WHERE s = 'a' OR no_such_fn(n) > 1",
        "SELECT * FROM t WHERE NOT (no_such_fn(n) > 1)",
        "SELECT * FROM t WHERE n > now()",
        "SELECT * FROM t WHERE n < current_date",
        "SELECT * FROM t WHERE visible()",
        "SELECT * FROM t WHERE s = 'a' OR visible()",
        "SELECT * FROM t WHERE lower(s) LIKE 'a%'",
        "SELECT n + 1 FROM t WHERE n > 3",
        "SELECT * FROM t WHERE n IN (SELECT n FROM t)",
        "WITH x AS (SELECT * FROM t WHERE n > 3) SELECT * FROM x",
    ];
    for (i, body) in bodies.iter().enumerate() {
        let err = core
            .execute_sql_in_session(
                &ctx("alice"),
                &mut session,
                &format!("CREATE VIEW r{i} AS {body}"),
            )
            .expect_err(body);
        assert_eq!(err.wire_code(), "42601", "body={body}");
        let err = run(&core, "alice", &format!("SELECT * FROM r{i} LIMIT 1")).expect_err("absent");
        assert_eq!(err.wire_code(), "42P01", "body={body}");
    }
}

/// 本文の `OR` が可視性を広げない: 自テナントの行だけが返り、他テナントの private 行の
/// 有無で結果が変わらない（RLS-10 (b)）。
#[test]
fn body_or_does_not_widen_tenant_visibility() {
    let mut results = Vec::new();
    for extra in [false, true] {
        let (core, _g) = open_core(&format!("view-where-expr-rls-{extra}"), extra);
        ddl(
            &core,
            "CREATE VIEW vor AS SELECT * FROM t WHERE n > 3 OR n IS NULL",
        )
        .expect("vor");
        let per: Vec<Vec<u64>> = TENANTS
            .iter()
            .map(|t| ids(&core, t, "SELECT id FROM vor LIMIT 100"))
            .collect();
        results.push(per);
    }
    // alice・carol には bob の private 行（9・10）の有無で結果が変わらない。
    assert_eq!(results[0][0], results[1][0]);
    assert_eq!(results[0][2], results[1][2]);
    // 可視なのは自分の行と他テナントの public 行だけ（bob の private 行 6 は alice・carol に出ない）。
    assert_eq!(results[1][0], vec![2, 3, 5, 8]);
    assert_eq!(results[1][2], vec![2, 3, 5, 8]);
    // bob 自身は自分の private 行が増えた分だけ増える。
    assert_eq!(results[0][1], vec![2, 3, 5, 6, 8]);
    assert_eq!(results[1][1], vec![2, 3, 5, 6, 8, 9, 10]);
}

/// 再オープン後も同じ結果で参照できる（保存した描画が参照時の再解析で成立する）。
#[test]
fn expression_views_persist_across_reopen() {
    let path = unique_db_path("view-where-expr-reopen");
    let _guard = CleanupGuard(path.clone());
    let mut expected = Vec::new();
    {
        let core = open_at(&path, false);
        for (i, (pred, _)) in cases().iter().enumerate() {
            ddl(
                &core,
                &format!("CREATE VIEW v{i} AS SELECT * FROM t WHERE {pred}"),
            )
            .unwrap_or_else(|e| panic!("{pred}: {e:?}"));
            expected.push(ids(&core, "bob", &format!("SELECT id FROM v{i} LIMIT 100")));
        }
    }
    let storage = Storage::open(&path).expect("reopen");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    for (i, want) in expected.iter().enumerate() {
        let got = ids(&core, "bob", &format!("SELECT id FROM v{i} LIMIT 100"));
        assert_eq!(&got, want, "view v{i}");
    }
}

/// 連鎖: 式述語つきの単純形ビューの上に重ねる・`LIMIT` 付きビューの上に式述語つきの
/// 単純形の段を重ねる。どちらも直接実行と一致する。
#[test]
fn chained_expression_views_match_direct_execution() {
    let (core, _g) = open_core("view-where-expr-chain", false);
    ddl(
        &core,
        "CREATE VIEW c1 AS SELECT * FROM t WHERE n > 1 OR n IS NULL",
    )
    .expect("c1");
    ddl(
        &core,
        "CREATE VIEW c2 AS SELECT * FROM c1 WHERE s <> 'a' AND n IS NOT NULL",
    )
    .expect("c2");
    ddl(
        &core,
        "CREATE VIEW lim AS SELECT id, n, s FROM t ORDER BY id LIMIT 6",
    )
    .expect("lim");
    ddl(
        &core,
        "CREATE VIEW c3 AS SELECT id, n FROM lim WHERE n > 1 OR n IS NULL",
    )
    .expect("c3");
    for tenant in TENANTS {
        let direct2 = ids(
            &core,
            tenant,
            "SELECT id FROM t WHERE (n > 1 OR n IS NULL) AND s <> 'a' AND n IS NOT NULL LIMIT 100",
        );
        assert_eq!(
            ids(&core, tenant, "SELECT id FROM c2 LIMIT 100"),
            direct2,
            "{tenant} c2"
        );
        let direct3 = ids(
            &core,
            tenant,
            "SELECT id FROM lim WHERE n > 1 OR n IS NULL LIMIT 100",
        );
        assert_eq!(
            ids(&core, tenant, "SELECT id FROM c3 LIMIT 100"),
            direct3,
            "{tenant} c3"
        );
    }
}

/// 更新可能ビュー（TABLE-43）: 本文述語に合う行だけが変わり、`WITH CHECK OPTION` は本文述語を
/// 満たさない `INSERT` を `44000` で拒否する。
#[test]
fn updatable_view_keeps_body_predicate_for_dml_and_check_option() {
    let (core, _g) = open_core("view-where-expr-dml", false);
    ddl(
        &core,
        "CREATE VIEW uv AS SELECT * FROM t WHERE n > 3 WITH CHECK OPTION",
    )
    .expect("uv");
    let upd = run_outcome(
        &core,
        "alice",
        "UPDATE uv SET s = 'upd' WHERE id >= 1 USING OPERATION_ID 'op-3238-u'",
    )
    .expect("update");
    drop(upd);
    // 本文述語に合う行 2（n=5）だけが変わる。n=1・NULL・3 の行は変わらない。
    let changed = ids(&core, "alice", "SELECT id FROM t WHERE s = 'upd' LIMIT 100");
    assert_eq!(changed, vec![2]);
    let err = run_outcome(
        &core,
        "alice",
        "INSERT INTO uv (id, embedding, n, s) VALUES (20, '[1,1]', 1, 'x') USING OPERATION_ID 'op-3238-i1'",
    )
    .expect_err("check option");
    assert_eq!(err.wire_code(), "44000");
    run_outcome(
        &core,
        "alice",
        "INSERT INTO uv (id, embedding, n, s) VALUES (21, '[1,1]', 5, 'x') USING OPERATION_ID 'op-3238-i2'",
    )
    .expect("insert n=5");
    run_outcome(
        &core,
        "alice",
        "DELETE FROM uv WHERE id >= 1 USING OPERATION_ID 'op-3238-d'",
    )
    .expect("delete");
    // alice の n > 3 の行（2・21）だけが消え、他の行（他テナントの public 行 5・7・8 を含む）は残る。
    assert_eq!(
        ids(&core, "alice", "SELECT id FROM t LIMIT 100"),
        vec![1, 3, 4, 5, 7, 8]
    );
}

/// MATERIALIZED VIEW（TABLE-42）も同じ許可形で、内容は直接実行と一致する。UDF を含む本文は `42601`。
// intentional: docs/design/create-view.md
#[test]
fn materialized_view_accepts_expression_body_and_rejects_unknown_function() {
    let (core, _g) = open_core("view-where-expr-matview", false);
    ddl(
        &core,
        "CREATE MATERIALIZED VIEW mv AS SELECT * FROM t WHERE n > 3 WITH DATA",
    )
    .expect("matview");
    // WITH DATA で実体化されるのは作成者（alice）の可視範囲。matview の行は疑似列 `id` を
    // 持たないため、`(n, s)` の値（ソート済み）で直接実行と比較する。
    let sorted = |sql: &str| {
        let mut v: Vec<String> = run(&core, "alice", sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
            .rows
            .iter()
            .map(|r| format!("{:?}", r.cells))
            .collect();
        v.sort();
        v
    };
    let direct = sorted("SELECT n, s FROM t WHERE n > 3 LIMIT 100");
    assert_eq!(direct.len(), 3);
    assert_eq!(sorted("SELECT n, s FROM mv LIMIT 100"), direct);
    let err = ddl(
        &core,
        "CREATE MATERIALIZED VIEW mv2 AS SELECT * FROM t WHERE no_such_fn(n) > 3 WITH DATA",
    )
    .expect_err("unknown function");
    assert_eq!(err.wire_code(), "42601");
}
/// 非公開列（下位ビューが公開しない `n`）を本文の式述語で参照する段は、`OR`／`NOT` の内側でも
/// 絞り込み結果を返さない（作成時または参照時に失敗する。filter oracle の防止。RLS-10 (b)）。
// intentional: docs/design/create-view.md
#[test]
fn hidden_column_in_expression_body_never_returns_rows() {
    let (core, _g) = open_core("view-where-expr-hidden", false);
    ddl(&core, "CREATE VIEW p AS SELECT s FROM t").expect("p");
    for (i, pred) in [
        "n IS NULL",
        "n IS NOT NULL",
        "n > 3",
        "n IN (1)",
        "n BETWEEN 1 AND 5",
        "s = 'a' OR n IS NULL",
        "s = 'a' OR n > 3",
        "NOT (n > 3)",
        "NOT (n IS NULL OR n > 3)",
        "n + 1 > 3",
        "NOT (n + 1 > 3)",
        "lower(n) = 'x'",
    ]
    .iter()
    .enumerate()
    {
        let created = ddl(
            &core,
            &format!("CREATE VIEW q{i} AS SELECT s FROM p WHERE {pred}"),
        );
        if created.is_ok() {
            let res = run(&core, "alice", &format!("SELECT * FROM q{i} LIMIT 10"));
            assert!(res.is_err(), "hidden column leaked rows: {pred}");
        }
    }
}

/// プレースホルダ `$1` を含む本文は定義できない（`42601`。永続化されない）。
// intentional: docs/design/create-view.md
#[test]
fn placeholder_in_body_is_rejected() {
    let (core, _g) = open_core("view-where-expr-param", false);
    let err = ddl(&core, "CREATE VIEW vp AS SELECT * FROM t WHERE n > $1").expect_err("param");
    assert_eq!(err.wire_code(), "42601");
    let err = run(&core, "alice", "SELECT * FROM vp LIMIT 1").expect_err("absent");
    assert_eq!(err.wire_code(), "42P01");
}

/// 列別名つきビューと、`OR`／`IS NULL`／`IN`／`NOT` 本文の更新可能ビューへの `UPDATE` は、
/// 本文述語の範囲を広げない。
#[test]
fn aliased_and_updatable_views_keep_or_body_predicates() {
    let (core, _g) = open_core("view-where-expr-alias-dml", false);
    ddl(
        &core,
        "CREATE VIEW va (x, y) AS SELECT n, s FROM t WHERE n > 3 OR n IS NULL",
    )
    .expect("va");
    // 別名つきビューは疑似列 `id` を公開しない。本文に合う行（alice の 2・3 と他テナントの public 行
    // 5・8）の `(x, y)` が、基表への直接実行と一致する。
    let rows = |sql: &str| {
        let mut v: Vec<Vec<Cell>> = run(&core, "alice", sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
            .rows
            .iter()
            .map(|r| r.cells.clone())
            .collect();
        v.sort_by_key(|c| format!("{c:?}"));
        v
    };
    assert_eq!(
        rows("SELECT x, y FROM va LIMIT 100"),
        rows("SELECT n, s FROM t WHERE n > 3 OR n IS NULL LIMIT 100")
    );
    assert_eq!(rows("SELECT x, y FROM va LIMIT 100").len(), 4);
    assert_eq!(rows("SELECT x, y FROM va WHERE y = 'b' LIMIT 100").len(), 2);
    for (i, body, touched) in [
        (0, "n IS NULL", vec![3u64]),
        (1, "n IN (1, 3)", vec![1, 4]),
        (2, "NOT (n = 5 OR n IS NULL)", vec![1, 4]),
        // 直前までの UPDATE で行 1 の `s` は `'a'` でなくなっているため、行 2（s='a'）と 3（n IS NULL）。
        (3, "s = 'a' OR n IS NULL", vec![2, 3]),
    ] {
        ddl(
            &core,
            &format!("CREATE VIEW uv{i} AS SELECT * FROM t WHERE {body}"),
        )
        .expect("uv");
        run_outcome(
            &core,
            "alice",
            &format!(
                "UPDATE uv{i} SET s = 'upd{i}' WHERE id >= 1 USING OPERATION_ID 'op-3238-a{i}'"
            ),
        )
        .expect("update");
        let changed = ids(
            &core,
            "alice",
            &format!("SELECT id FROM t WHERE s = 'upd{i}' LIMIT 100"),
        );
        assert_eq!(changed, touched, "body={body}");
    }
}

/// 式述語つきビューが依存する表の列の `RENAME COLUMN` は、現状 fail-closed で拒否する（意味を
/// 変えた本文を保存しない）。追随は #3239。
// tracked: #3239
#[test]
fn rename_column_under_expression_view_is_refused_fail_closed() {
    let (core, _g) = open_core("view-where-expr-rename", false);
    ddl(
        &core,
        "CREATE VIEW vr AS SELECT * FROM t WHERE n > 3 OR n IS NULL",
    )
    .expect("vr");
    let before = ids(&core, "alice", "SELECT id FROM vr LIMIT 100");
    let res = ddl(&core, "ALTER TABLE t RENAME COLUMN n TO m");
    assert!(
        res.is_err(),
        "rename must be refused while the view depends on it"
    );
    assert_eq!(ids(&core, "alice", "SELECT id FROM vr LIMIT 100"), before);
}
