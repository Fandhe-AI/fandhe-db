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

/// 投影に `id` を含まないビューの結果行（セル列の文字列表現。順不同比較用に整列）。
fn cells_of(core: &EngineCore, tenant: &str, sql: &str) -> Vec<String> {
    let mut v: Vec<String> = run(core, tenant, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
        .rows
        .iter()
        .map(|r| format!("{:?}", r.cells))
        .collect();
    v.sort();
    v
}

/// 述語中の識別子 `from`（語単位）を `to` へ置換する（直接実行のオラクル側の改名後 SQL を作る）。
fn rename_word(pred: &str, from: &str, to: &str) -> String {
    let mut out = String::new();
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        out.push_str(if word == from { to } else { word.as_str() });
        word.clear();
    };
    for ch in pred.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            word.push(ch);
        } else {
            flush(&mut word, &mut out);
            out.push(ch);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// 式述語つきビューが依存する表の列の `RENAME COLUMN` は本文を追随させ（Issue #3239）、
/// 改名後もビュー経由の結果が改名前・改名後の述語での直接実行と一致する。再オープン後も同じ。
/// 旧名での外側参照は未知列（42703）。
#[test]
fn rename_column_under_expression_view_follows_and_keeps_results() {
    let path = unique_db_path("view-where-expr-rename");
    let _guard = CleanupGuard(path.clone());
    let mut before = Vec::new();
    {
        let core = open_at(&path, false);
        for (i, (pred, _)) in cases().iter().enumerate() {
            ddl(
                &core,
                &format!("CREATE VIEW v{i} AS SELECT * FROM t WHERE {pred}"),
            )
            .unwrap_or_else(|e| panic!("{pred}: {e:?}"));
        }
        for tenant in TENANTS {
            for i in 0..cases().len() {
                before.push(ids(
                    &core,
                    tenant,
                    &format!("SELECT id FROM v{i} LIMIT 100"),
                ));
            }
        }
        ddl(&core, "ALTER TABLE t RENAME COLUMN n TO m").expect("rename follows");
        let mut k = 0;
        for tenant in TENANTS {
            for (i, (pred, _)) in cases().iter().enumerate() {
                let via = ids(&core, tenant, &format!("SELECT id FROM v{i} LIMIT 100"));
                assert_eq!(via, before[k], "tenant={tenant} pred={pred}");
                let renamed = rename_word(pred, "n", "m");
                let direct = ids(
                    &core,
                    tenant,
                    &format!("SELECT id FROM t WHERE {renamed} LIMIT 100"),
                );
                assert_eq!(via, direct, "direct tenant={tenant} pred={renamed}");
                k += 1;
            }
        }
        let err = run(&core, "alice", "SELECT id FROM t WHERE n > 3 LIMIT 100")
            .expect_err("old name is unknown");
        assert_eq!(err.wire_code(), "42703");
    }
    let storage = Storage::open(&path).expect("reopen");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut k = 0;
    for tenant in TENANTS {
        for i in 0..cases().len() {
            let via = ids(&core, tenant, &format!("SELECT id FROM v{i} LIMIT 100"));
            assert_eq!(via, before[k], "reopen tenant={tenant} v{i}");
            k += 1;
        }
    }
}

/// 改名列を参照しない式述語ビューは、その列を改名しても成功し結果が変わらない。
#[test]
fn rename_unreferenced_column_keeps_expression_view() {
    let (core, _g) = open_core("view-where-expr-rename-unref", false);
    ddl(
        &core,
        "CREATE VIEW vu AS SELECT * FROM t WHERE lower(s) = 'c'",
    )
    .expect("vu");
    let before = ids(&core, "alice", "SELECT id FROM vu LIMIT 100");
    ddl(&core, "ALTER TABLE t RENAME COLUMN n TO m").expect("rename");
    assert_eq!(ids(&core, "alice", "SELECT id FROM vu LIMIT 100"), before);
}

/// 連鎖（式述語つき単純形の上に式述語つき単純形）で両段が追随し、表 RENAME も追随する。
#[test]
fn rename_column_and_table_follow_chained_expression_views() {
    let (core, _g) = open_core("view-where-expr-rename-chain", false);
    ddl(
        &core,
        "CREATE VIEW c1 AS SELECT * FROM t WHERE n > 1 OR n IS NULL",
    )
    .expect("c1");
    ddl(
        &core,
        "CREATE VIEW c2 AS SELECT * FROM c1 WHERE n BETWEEN 2 AND 10 OR n IS NULL",
    )
    .expect("c2");
    let want: Vec<Vec<u64>> = TENANTS
        .iter()
        .map(|t| ids(&core, t, "SELECT id FROM c2 LIMIT 100"))
        .collect();
    ddl(&core, "ALTER TABLE t RENAME COLUMN n TO m").expect("rename column");
    ddl(&core, "ALTER TABLE t RENAME TO t2").expect("rename table");
    for (tenant, want) in TENANTS.iter().zip(&want) {
        assert_eq!(&ids(&core, tenant, "SELECT id FROM c2 LIMIT 100"), want);
    }
}

/// 更新可能ビューの本文述語は改名後も DML で保たれる（WITH CHECK OPTION の述語も追随する）。
#[test]
fn rename_column_keeps_updatable_view_check_option() {
    let (core, _g) = open_core("view-where-expr-rename-check", false);
    ddl(
        &core,
        "CREATE VIEW uc AS SELECT * FROM t WHERE n IS NULL OR n > 3 WITH CHECK OPTION",
    )
    .expect("uc");
    ddl(&core, "ALTER TABLE t RENAME COLUMN n TO m").expect("rename");
    let touched = run_outcome(
        &core,
        "alice",
        "UPDATE uc SET s = 'upd' WHERE id >= 1 USING OPERATION_ID 'op-3239-a'",
    );
    assert!(touched.is_ok(), "{touched:?}");
    assert_eq!(
        ids(&core, "alice", "SELECT id FROM t WHERE s = 'upd' LIMIT 100"),
        vec![2, 3]
    );
}

// intentional: docs/design/create-view.md
/// 評価後射影形（`LIMIT` 付き）の式述語ビューが列を読む場合の `RENAME COLUMN` は従来どおり
/// `0A000` で、ビューも表も変わらない。
#[test]
fn rename_column_under_buffered_expression_view_is_refused() {
    let (core, _g) = open_core("view-where-expr-rename-buffered", false);
    ddl(
        &core,
        "CREATE VIEW vb AS SELECT * FROM t WHERE n > 3 OR n IS NULL LIMIT 5",
    )
    .expect("vb");
    let before = ids(&core, "alice", "SELECT id FROM vb LIMIT 100");
    let err = match ddl(&core, "ALTER TABLE t RENAME COLUMN n TO m") {
        Err(e) => e,
        Ok(o) => panic!("must be refused: {o:?}"),
    };
    assert_eq!(err.wire_code(), "0A000");
    assert_eq!(ids(&core, "alice", "SELECT id FROM vb LIMIT 100"), before);
    assert!(run(&core, "alice", "SELECT id FROM t WHERE n > 3 LIMIT 100").is_ok());
}

/// 再オープン後の参照: `LIMIT` 付き本文・評価後射影形の上の単純形の段・
/// 列別名つきビューの外側からの式述語が、再オープンの前後で一致する。
#[test]
fn buffered_and_aliased_expression_views_persist_across_reopen() {
    let path = unique_db_path("view-where-expr-reopen-buffered");
    let _guard = CleanupGuard(path.clone());
    let queries = [
        "SELECT id FROM vl LIMIT 100",
        "SELECT id FROM vs2 LIMIT 100",
        "SELECT x FROM va WHERE x > 3 OR y = 'a' LIMIT 100",
    ];
    let snapshot = |core: &EngineCore| -> Vec<Vec<String>> {
        let mut out = Vec::new();
        for tenant in TENANTS {
            for q in queries {
                let mut rows: Vec<String> = run(core, tenant, q)
                    .unwrap_or_else(|e| panic!("{q}: {e:?}"))
                    .rows
                    .iter()
                    .map(|r| format!("{}:{:?}", r.id, r.cells))
                    .collect();
                rows.sort();
                out.push(rows);
            }
        }
        out
    };
    let expected = {
        let core = open_at(&path, false);
        for sql in [
            "CREATE VIEW vl AS SELECT * FROM t WHERE n > 3 OR n IS NULL LIMIT 50",
            "CREATE VIEW vs2 AS SELECT * FROM vl WHERE n IN (1, 5, 10) OR s = 'b'",
            "CREATE VIEW va (x, y) AS SELECT n, s FROM t WHERE n > 3 OR n IS NULL",
        ] {
            if let Err(e) = ddl(&core, sql) {
                panic!("{sql}: {e:?}");
            }
        }
        snapshot(&core)
    };
    let storage = Storage::open(&path).expect("reopen");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    assert_eq!(snapshot(&core), expected);
}

// intentional: docs/design/create-view.md
/// 式・`OR` の枝・`NOT` の内側・`IN`／`BETWEEN`／`IS NULL` だけで列を参照するビューがあるとき、
/// その列の `DROP COLUMN` は依存エラーで拒否され、表もビューも変わらない。
#[test]
fn drop_column_referenced_only_inside_expression_predicates_is_rejected() {
    for (i, pred) in [
        "n + 1 > 3",
        "s = 'a' OR n > 3",
        "NOT (s = 'a' OR n > 3)",
        "n IN (1, 2)",
        "n BETWEEN 1 AND 5",
        "n IS NULL",
        "COALESCE(n, 0) > 1",
    ]
    .iter()
    .enumerate()
    {
        let (core, _g) = open_core(&format!("view-where-expr-drop-{i}"), false);
        ddl(
            &core,
            &format!("CREATE VIEW vd AS SELECT s FROM t WHERE {pred}"),
        )
        .expect("vd");
        let before = cells_of(&core, "alice", "SELECT s FROM vd LIMIT 100");
        let err = match ddl(&core, "ALTER TABLE t DROP COLUMN n") {
            Err(e) => e,
            Ok(o) => panic!("{pred}: must be refused: {o:?}"),
        };
        assert_eq!(err.wire_code(), "2BP01", "{pred}");
        assert_eq!(
            cells_of(&core, "alice", "SELECT s FROM vd LIMIT 100"),
            before
        );
        assert!(
            run(&core, "alice", "SELECT n FROM t LIMIT 1").is_ok(),
            "{pred}"
        );
    }
}

// intentional: docs/design/create-view.md
/// 式述語だけで列を参照するビューがあるとき、`USING` を伴う一般変換の `ALTER COLUMN TYPE` は
/// `0A000` で拒否され何も変わらない。値を変えない拡大の後もビューは同じ結果を返す。
#[test]
fn alter_type_with_expression_view_dependency_is_guarded() {
    let (core, _g) = open_core("view-where-expr-alter-type", false);
    ddl(
        &core,
        "CREATE VIEW vt2 AS SELECT s FROM t WHERE n + 1 > 3 OR n IS NULL",
    )
    .expect("vt2");
    let before = cells_of(&core, "alice", "SELECT s FROM vt2 LIMIT 100");
    let err = match ddl(
        &core,
        "ALTER TABLE t ALTER COLUMN n TYPE BIGINT USING n * 2",
    ) {
        Err(e) => e,
        Ok(o) => panic!("must be refused: {o:?}"),
    };
    assert_eq!(err.wire_code(), "0A000");
    assert_eq!(
        cells_of(&core, "alice", "SELECT s FROM vt2 LIMIT 100"),
        before
    );
    ddl(&core, "ALTER TABLE t ALTER COLUMN n TYPE BIGINT").expect("widen");
    assert_eq!(
        cells_of(&core, "alice", "SELECT s FROM vt2 LIMIT 100"),
        before
    );
}

/// 改名後も本文の `OR` は可視性を広げない（他テナントの private 行の有無で結果が変わらない）。
#[test]
fn rename_keeps_tenant_boundary_for_or_body() {
    let (plain, _g1) = open_core("view-where-expr-rename-tenant-a", false);
    let (extra, _g2) = open_core("view-where-expr-rename-tenant-b", true);
    for core in [&plain, &extra] {
        ddl(
            core,
            "CREATE VIEW vo AS SELECT * FROM t WHERE s = 'a' OR n IS NULL",
        )
        .expect("vo");
        ddl(core, "ALTER TABLE t RENAME COLUMN n TO m").expect("rename");
    }
    for tenant in ["alice", "carol"] {
        assert_eq!(
            ids(&plain, tenant, "SELECT id FROM vo LIMIT 100"),
            ids(&extra, tenant, "SELECT id FROM vo LIMIT 100"),
            "tenant={tenant}"
        );
    }
}
/// 本文の `OR TRUE`（空の AND 枝）が保存時往復検証を通り、全行を返すビューとして作成できる
/// （Issue #3238 のレビュー指摘。描画が `(n = 1 OR )` にならない）。
#[test]
fn body_or_true_creates_and_returns_all_rows() {
    let (core, _g) = open_core("view-where-expr-or-true", false);
    ddl(
        &core,
        "CREATE VIEW vt AS SELECT * FROM t WHERE n = 1 OR TRUE",
    )
    .expect("or true");
    let all = ids(&core, "alice", "SELECT id FROM t LIMIT 100");
    let via_view = ids(&core, "alice", "SELECT id FROM vt LIMIT 100");
    assert_eq!(via_view, all);
}

// intentional: docs/design/create-view.md
/// ビュー定義の拒否は原因ごとの固定文言と SQLSTATE を返し（Issue #3240）、何も永続化されない。
/// 受理範囲は広げない。
#[test]
fn view_definition_rejections_carry_cause_specific_messages() {
    let (core, _g) = open_core("view-reject-causes", false);
    for pre in [
        "CREATE VIEW lim AS SELECT * FROM t LIMIT 5",
        "CREATE VIEW agg AS SELECT n, COUNT(*) AS c FROM t GROUP BY n",
        "CREATE VIEW ali (a, b) AS SELECT n, s FROM t",
    ] {
        ddl(&core, pre).unwrap_or_else(|e| panic!("{pre}: {e:?}"));
    }
    // (名前, SQL, 期待 SQLSTATE, 期待する詳細文言)。文言は固定で、識別子・内部キーを含まない。
    let cases: [(&str, &str, &str, &str); 6] = [
        (
            "x_join",
            "CREATE VIEW x_join AS SELECT * FROM t JOIN lim ON t.n = lim.n LIMIT 5",
            "42601",
            "unsupported SQL syntax: JOIN in a view body cannot reference a view",
        ),
        (
            "x_cte",
            "CREATE VIEW x_cte AS WITH w AS (SELECT * FROM lim) SELECT * FROM w",
            "42601",
            "unsupported SQL syntax: view body form is not supported",
        ),
        (
            "x_union",
            "CREATE VIEW x_union AS SELECT * FROM t UNION ALL SELECT * FROM lim",
            "42601",
            "unsupported SQL syntax: a view cannot reference a view with an aggregate/LIMIT body in this position",
        ),
        (
            "x_ilike",
            "CREATE VIEW x_ilike AS SELECT * FROM lim WHERE s ILIKE 'a%'",
            "0A000",
            "feature not supported: view body uses a feature that is not supported in this position",
        ),
        (
            "x_proj",
            "CREATE VIEW x_proj AS SELECT n + 1 FROM lim",
            "42601",
            "unsupported SQL syntax: view body form is not supported",
        ),
        (
            "x_replace",
            "CREATE OR REPLACE VIEW lim AS SELECT * FROM lim",
            "42P17",
            "view definition must not reference the view itself",
        ),
    ];
    for (name, sql, code, msg) in cases {
        let e = ddl(&core, sql).expect_err(sql);
        assert_eq!(e.wire_code(), code, "{sql}");
        assert_eq!(e.to_string(), msg, "{sql}");
        assert!(!e.to_string().contains('#'), "{sql}");
        // 拒否された名前は永続化されない（参照すると `42P01`）。
        if name != "x_replace" {
            let r = run(&core, "alice", &format!("SELECT id FROM {name} LIMIT 1"));
            assert_eq!(r.expect_err(name).wire_code(), "42P01", "{name}");
        }
    }
}
